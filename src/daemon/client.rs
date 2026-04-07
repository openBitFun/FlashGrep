use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    net::TcpStream,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::error::{AppError, Result};

use super::protocol::{
    EnsureRepoParams, OpenRepoParams, RepoRef, Request, RequestEnvelope, Response,
    ResponseEnvelope, ResponsePayload, SearchParams,
};

#[derive(Debug)]
pub struct DaemonClient {
    addr: String,
    next_id: AtomicU64,
}

impl DaemonClient {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn send(&self, request: Request) -> Result<Response> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = RequestEnvelope {
            id: Some(request_id),
            request,
        };

        let stream = TcpStream::connect(&self.addr)?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut writer = BufWriter::new(stream);
        serde_json::to_writer(&mut writer, &envelope)
            .map_err(|error| AppError::Protocol(format!("failed to encode request: {error}")))?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        let mut lines = reader.lines();
        let line = lines.next().ok_or_else(|| {
            AppError::Protocol("daemon closed connection without a response".into())
        })??;
        let response: ResponseEnvelope = serde_json::from_str(&line)
            .map_err(|error| AppError::Protocol(format!("failed to decode response: {error}")))?;

        if response.id != Some(request_id) {
            return Err(AppError::Protocol(format!(
                "daemon response id mismatch: expected {request_id:?}, got {:?}",
                response.id
            )));
        }

        match response.payload {
            ResponsePayload::Ok { result } => Ok(result),
            ResponsePayload::Error { error } => Err(AppError::Protocol(error.message)),
        }
    }

    pub fn open_repo(&self, params: OpenRepoParams) -> Result<Response> {
        self.send(Request::OpenRepo { params })
    }

    pub fn ensure_repo(&self, params: EnsureRepoParams) -> Result<Response> {
        self.send(Request::EnsureRepo { params })
    }

    pub fn get_repo_status(&self, repo_id: impl Into<String>) -> Result<Response> {
        self.send(Request::GetRepoStatus {
            params: RepoRef {
                repo_id: repo_id.into(),
            },
        })
    }

    pub fn search(&self, params: SearchParams) -> Result<Response> {
        self.send(Request::Search { params })
    }
}
