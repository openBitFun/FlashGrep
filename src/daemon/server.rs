use std::{
    fs,
    io::{BufRead, BufReader, BufWriter, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    daemon::{protocol::RequestEnvelope, service::DaemonService},
    error::{AppError, Result},
};

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub bind_addr: String,
    pub state_file: Option<PathBuf>,
}

pub fn serve(options: ServerOptions) -> Result<()> {
    let listener = TcpListener::bind(&options.bind_addr)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    if let Some(path) = options.state_file.as_deref() {
        write_state_file(path, &local_addr.to_string())?;
    }
    let state_guard = StateFileGuard {
        path: options.state_file.clone(),
    };
    println!("codgrep daemon listening on {local_addr}");

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
mod tests {
    use std::{thread, time::Duration};

    use tempfile::tempdir;

    use super::*;
    use crate::daemon::{protocol::Request, DaemonClient};

    #[test]
    fn serve_writes_and_removes_state_file() {
        let temp = tempdir().expect("temp dir should succeed");
        let state_file = temp.path().join("daemon-state.json");

        let handle = thread::spawn({
            let state_file = state_file.clone();
            move || {
                serve(ServerOptions {
                    bind_addr: "127.0.0.1:0".into(),
                    state_file: Some(state_file),
                })
                .expect("serve should succeed");
            }
        });

        let state = {
            let mut record = None;
            for _ in 0..100 {
                if let Ok(current) = read_state_file(&state_file) {
                    record = Some(current);
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            record.expect("state file should appear")
        };

        let client = DaemonClient::new(state.addr);
        client
            .send(Request::Shutdown)
            .expect("shutdown should succeed");
        handle.join().expect("server thread should join");
        assert!(!state_file.exists());
    }
}

fn handle_connection(
    stream: TcpStream,
    service: &DaemonService,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RequestEnvelope = serde_json::from_str(&line)
            .map_err(|error| AppError::Protocol(format!("invalid daemon request: {error}")))?;
        let is_shutdown = matches!(request.request, crate::daemon::protocol::Request::Shutdown);
        let response = service.handle(request);
        serde_json::to_writer(&mut writer, &response)
            .map_err(|error| AppError::Protocol(format!("failed to encode response: {error}")))?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        if is_shutdown {
            running.store(false, Ordering::Relaxed);
            break;
        }
    }

    Ok(())
}
