use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    net::{Shutdown, TcpStream},
    sync::mpsc,
    thread,
};

use crate::{
    daemon::protocol::{
        ErrorResponse, OpenRepoParams, Request, RequestEnvelope, Response, ResponseEnvelope,
        ServerMessage,
    },
    error::{AppError, Result},
};

use super::{
    managed::ManagedDaemonClient,
    server::{read_content_length_request, write_content_length_message},
    service::DaemonService,
};

pub fn serve_managed_stdio_bridge(repo: OpenRepoParams) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdin_reader = BufReader::new(stdin.lock());
    let Some(initial_request) = read_content_length_request(&mut stdin_reader)? else {
        return Ok(());
    };

    // Answer the handshake locally before opening the repo. Opening a large base
    // snapshot is synchronous and can take tens of seconds; while that ran ahead
    // of the handshake, clients timed out during `initialize` on indexes that
    // were perfectly healthy. The repo open now blocks the first real request
    // instead, where waiting is expected.
    answer_handshake_locally(&initial_request)?;
    let handshake_response_id = initial_request.id;

    let opened = match ManagedDaemonClient::new().open_repo(repo) {
        Ok(opened) => opened,
        Err(error) => return serve_failed_bridge(&mut stdin_reader, error),
    };
    let stream = TcpStream::connect(&opened.addr)?;
    let reader_stream = stream.try_clone()?;
    let shutdown_stream = stream.try_clone()?;
    let (out_tx, out_rx) = mpsc::channel::<ServerMessage>();

    let stdout_handle = thread::spawn(move || -> Result<()> {
        let stdout = std::io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        while let Ok(message) = out_rx.recv() {
            write_content_length_message(&mut writer, &message)?;
        }
        Ok(())
    });

    let daemon_out = out_tx.clone();
    let daemon_reader_handle = thread::spawn(move || -> Result<()> {
        let reader = BufReader::new(reader_stream);
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let message = serde_json::from_str::<ServerMessage>(&line).map_err(|error| {
                AppError::Protocol(format!("failed to decode managed daemon response: {error}"))
            })?;
            // The handshake was already answered locally; the daemon still gets
            // the request so it registers this connection, but its duplicate
            // response must not reach the client.
            if let ServerMessage::Response(response) = &message {
                if response.id.is_some() && response.id == handshake_response_id {
                    continue;
                }
            }
            if daemon_out.send(message).is_err() {
                break;
            }
        }
        Ok(())
    });

    let mut daemon_writer = BufWriter::new(stream);
    write_daemon_request(&mut daemon_writer, &initial_request)?;
    while let Some(request) = read_content_length_request(&mut stdin_reader)? {
        match request.request {
            Request::Shutdown => {
                out_tx
                    .send(ServerMessage::Response(ResponseEnvelope::success(
                        request.id,
                        Response::ShutdownAck,
                    )))
                    .map_err(|error| {
                        AppError::Protocol(format!(
                            "failed to send bridge shutdown response: {error}"
                        ))
                    })?;
                break;
            }
            Request::Exit => break,
            _ => write_daemon_request(&mut daemon_writer, &request)?,
        }
    }

    let _ = shutdown_stream.shutdown(Shutdown::Both);
    drop(daemon_writer);
    let daemon_reader_result = daemon_reader_handle
        .join()
        .map_err(|_| AppError::Protocol("managed daemon bridge reader panicked".into()))?;
    drop(out_tx);
    stdout_handle
        .join()
        .map_err(|_| AppError::Protocol("managed daemon bridge writer panicked".into()))??;
    daemon_reader_result
}

fn write_daemon_request(writer: &mut impl Write, request: &RequestEnvelope) -> Result<()> {
    serde_json::to_writer(&mut *writer, request).map_err(|error| {
        AppError::Protocol(format!("failed to encode managed daemon request: {error}"))
    })?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Answers the opening handshake from a throwaway local service so the client is
/// not left waiting on the managed daemon. The same request is replayed to the
/// daemon once the connection is up, so the daemon still sees the handshake.
fn answer_handshake_locally(initial_request: &RequestEnvelope) -> Result<()> {
    let service = DaemonService::new();
    let Some(response) = service.handle_for_connection(None, initial_request.clone()) else {
        return Ok(());
    };
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    write_content_length_message(&mut writer, &ServerMessage::Response(response))
}

fn serve_failed_bridge(stdin: &mut impl BufRead, error: AppError) -> Result<()> {
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let message = error.to_string();
    while let Some(request) = read_content_length_request(stdin)? {
        let response = match request.request {
            Request::Initialized => None,
            Request::Shutdown => Some(ResponseEnvelope::success(request.id, Response::ShutdownAck)),
            Request::Exit => break,
            _ => request.id.map(|id| {
                ResponseEnvelope::error(
                    Some(id),
                    ErrorResponse {
                        code: -32000,
                        message: message.clone(),
                        data: None,
                    },
                )
            }),
        };
        if let Some(response) = response {
            write_content_length_message(&mut writer, &ServerMessage::Response(response))?;
        }
    }
    Ok(())
}
