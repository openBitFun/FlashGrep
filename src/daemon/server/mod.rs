use std::{
    fs,
    io::{BufReader, BufWriter},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    daemon::{
        protocol::{Request, ServerMessage},
        service::DaemonService,
    },
    error::{AppError, Result},
};

mod codec;
mod connection;

use codec::{read_content_length_request, write_content_length_message};
use connection::handle_connection;

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub bind_addr: String,
    pub state_file: Option<PathBuf>,
    pub stdio: bool,
}

pub fn serve(options: ServerOptions) -> Result<()> {
    if options.stdio {
        return serve_stdio();
    }
    let listener = TcpListener::bind(&options.bind_addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    if let Some(path) = options.state_file.as_deref() {
        write_state_file(path, &local_addr.to_string())?;
    }
    let state_guard = StateFileGuard {
        path: options.state_file.clone(),
    };
    println!("flashgrep daemon listening on {local_addr}");

    let running = Arc::new(AtomicBool::new(true));
    let service = DaemonService::new();

    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let service = service.clone();
                let running = Arc::clone(&running);
                thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, &service, &running) {
                        eprintln!("daemon connection error: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }

    service.shutdown_all();
    drop(state_guard);
    Ok(())
}

pub fn serve_stdio() -> Result<()> {
    let service = DaemonService::new();
    let (out_tx, out_rx) = mpsc::channel::<ServerMessage>();
    let connection_id = service.register_connection(out_tx.clone());
    let writer_handle = thread::spawn(move || -> Result<()> {
        let stdout = std::io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        while let Ok(message) = out_rx.recv() {
            write_content_length_message(&mut writer, &message)?;
        }
        Ok(())
    });

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut explicit_shutdown = false;
    loop {
        let Some(request) = read_content_length_request(&mut reader)? else {
            break;
        };
        let is_shutdown = matches!(request.request, Request::Shutdown | Request::Exit);
        if let Some(response) = service.handle_for_connection(Some(connection_id), request) {
            out_tx
                .send(ServerMessage::Response(response))
                .map_err(|error| AppError::Protocol(format!("failed to send response: {error}")))?;
        }
        if is_shutdown {
            explicit_shutdown = true;
            break;
        }
    }

    service.unregister_connection(connection_id);
    drop(out_tx);
    writer_handle
        .join()
        .map_err(|_| AppError::Protocol("daemon stdio writer thread panicked".into()))??;
    if !explicit_shutdown {
        service.wait_for_tasks_idle();
    }
    service.shutdown_all();
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStateFile {
    pub version: u32,
    pub addr: String,
    pub pid: u32,
    pub updated_unix_secs: u64,
}

struct StateFileGuard {
    path: Option<PathBuf>,
}

impl Drop for StateFileGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let _ = fs::remove_file(path);
    }
}

pub fn read_state_file(path: &Path) -> Result<DaemonStateFile> {
    let contents = fs::read_to_string(path)?;
    serde_json::from_str(&contents)
        .map_err(|error| AppError::Protocol(format!("invalid daemon state file: {error}")))
}

fn write_state_file(path: &Path, addr: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let record = DaemonStateFile {
        version: 1,
        addr: addr.to_string(),
        pid: std::process::id(),
        updated_unix_secs: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|error| {
        AppError::Protocol(format!("failed to encode daemon state file: {error}"))
    })?;
    fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests;
