use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
};

use crate::{
    daemon::{
        protocol::{Request, RequestEnvelope, ServerMessage},
        service::DaemonService,
    },
    error::{AppError, Result},
};

use super::{is_concurrent_request, spawn_request_worker, wait_for_request_workers};

pub(super) fn handle_connection(
    stream: TcpStream,
    service: &DaemonService,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let (out_tx, out_rx) = mpsc::channel::<ServerMessage>();
    let active_requests = Arc::new(AtomicUsize::new(0));
    let connection_id = service.register_connection(out_tx.clone());
    let writer_handle = thread::spawn(move || -> Result<()> {
        let mut writer = BufWriter::new(stream);
        while let Ok(message) = out_rx.recv() {
            write_line_message(&mut writer, &message)?;
        }
        Ok(())
    });

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RequestEnvelope = serde_json::from_str(&line)
            .map_err(|error| AppError::Protocol(format!("invalid daemon request: {error}")))?;
        let is_shutdown = matches!(request.request, Request::Shutdown);
        if is_concurrent_request(&request) {
            spawn_request_worker(
                service.clone(),
                Some(connection_id),
                request,
                out_tx.clone(),
                active_requests.clone(),
            );
        } else {
            wait_for_request_workers(&active_requests);
            if let Some(response) = service.handle_for_connection(Some(connection_id), request) {
                out_tx
                    .send(ServerMessage::Response(response))
                    .map_err(|error| {
                        AppError::Protocol(format!("failed to send response: {error}"))
                    })?;
            }
        }
        if is_shutdown {
            running.store(false, Ordering::Relaxed);
            break;
        }
    }

    wait_for_request_workers(&active_requests);
    service.unregister_connection(connection_id);
    drop(out_tx);
    writer_handle
        .join()
        .map_err(|_| AppError::Protocol("daemon writer thread panicked".into()))??;
    Ok(())
}

fn write_line_message(writer: &mut impl Write, message: &ServerMessage) -> Result<()> {
    serde_json::to_writer(&mut *writer, message)
        .map_err(|error| AppError::Protocol(format!("failed to encode response: {error}")))?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
