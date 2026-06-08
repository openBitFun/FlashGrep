use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    net::TcpStream,
    sync::atomic::{AtomicU64, Ordering},
    sync::Mutex,
    time::Duration,
};

use crate::error::{AppError, Result};

use super::protocol::{
    EnsureRepoParams, GlobParams, OpenRepoParams, RepoRef, Request, RequestEnvelope, Response,
    ResponseEnvelope, SearchParams, TaskRef,
};

#[derive(Debug)]
pub struct DaemonClient {
    addr: String,
    io_timeout: Option<Duration>,
    next_id: AtomicU64,
    connection: Mutex<Option<DaemonConnection>>,
}

#[derive(Debug)]
struct DaemonConnection {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
}

impl DaemonClient {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            io_timeout: None,
            next_id: AtomicU64::new(1),
            connection: Mutex::new(None),
        }
    }

    pub fn with_io_timeout(mut self, timeout: Duration) -> Self {
        self.io_timeout = Some(timeout);
        self
    }

    pub fn send(&self, request: Request) -> Result<Response> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = RequestEnvelope {
            jsonrpc: "2.0".into(),
            id: Some(request_id),
            request,
        };

        let mut connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Protocol("daemon connection mutex poisoned".into()))?;
        let response = match self.send_with_connection(&mut connection, &envelope) {
            Ok(response) => response,
            Err(_) => {
                *connection = None;
                self.send_with_connection(&mut connection, &envelope)?
            }
        };

        if response.id != Some(request_id) {
            return Err(AppError::Protocol(format!(
                "daemon response id mismatch: expected {request_id:?}, got {:?}",
                response.id
            )));
        }

        let ResponseEnvelope {
            jsonrpc,
            result,
            error,
            ..
        } = response;

        if jsonrpc != "2.0" {
            return Err(AppError::Protocol(format!(
                "unsupported daemon jsonrpc version: {}",
                jsonrpc
            )));
        }

        if let Some(error) = error {
            return Err(AppError::Protocol(error.message));
        }

        result.ok_or_else(|| AppError::Protocol("daemon response missing result".into()))
    }

    pub fn send_raw_json_line(&self, request: Request) -> Result<String> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = RequestEnvelope {
            jsonrpc: "2.0".into(),
            id: Some(request_id),
            request,
        };

        let mut connection = self
            .connection
            .lock()
            .map_err(|_| AppError::Protocol("daemon connection mutex poisoned".into()))?;
        let line = match self.send_raw_with_connection(&mut connection, &envelope) {
            Ok(line) => line,
            Err(_) => {
                *connection = None;
                self.send_raw_with_connection(&mut connection, &envelope)?
            }
        };

        validate_response_id(&line, request_id)?;
        Ok(line)
    }

    fn send_with_connection(
        &self,
        connection: &mut Option<DaemonConnection>,
        envelope: &RequestEnvelope,
    ) -> Result<ResponseEnvelope> {
        let connection = match connection {
            Some(connection) => connection,
            None => {
                *connection = Some(self.connect()?);
                connection.as_mut().ok_or_else(|| {
                    AppError::Protocol("daemon connection missing after successful connect".into())
                })?
            }
        };

        serde_json::to_writer(&mut connection.writer, envelope)
            .map_err(|error| AppError::Protocol(format!("failed to encode request: {error}")))?;
        connection.writer.write_all(b"\n")?;
        connection.writer.flush()?;

        let mut line = String::new();
        let read = connection.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(AppError::Protocol(
                "daemon closed connection without a response".into(),
            ));
        }
        serde_json::from_str(&line)
            .map_err(|error| AppError::Protocol(format!("failed to decode response: {error}")))
    }

    fn send_raw_with_connection(
        &self,
        connection: &mut Option<DaemonConnection>,
        envelope: &RequestEnvelope,
    ) -> Result<String> {
        let connection = match connection {
            Some(connection) => connection,
            None => {
                *connection = Some(self.connect()?);
                connection.as_mut().ok_or_else(|| {
                    AppError::Protocol("daemon connection missing after successful connect".into())
                })?
            }
        };

        serde_json::to_writer(&mut connection.writer, envelope)
            .map_err(|error| AppError::Protocol(format!("failed to encode request: {error}")))?;
        connection.writer.write_all(b"\n")?;
        connection.writer.flush()?;

        let mut line = String::new();
        let read = connection.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(AppError::Protocol(
                "daemon closed connection without a response".into(),
            ));
        }
        Ok(line)
    }

    fn connect(&self) -> Result<DaemonConnection> {
        let stream = TcpStream::connect(&self.addr)?;
        if let Some(timeout) = self.io_timeout {
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
        }
        let reader = BufReader::new(stream.try_clone()?);
        let writer = BufWriter::new(stream);
        Ok(DaemonConnection { reader, writer })
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

    pub fn glob(&self, params: GlobParams) -> Result<Response> {
        self.send(Request::Glob { params })
    }

    pub fn base_snapshot_build(&self, repo_id: impl Into<String>) -> Result<Response> {
        self.send(Request::BaseSnapshotBuild {
            params: RepoRef {
                repo_id: repo_id.into(),
            },
        })
    }

    pub fn base_snapshot_rebuild(&self, repo_id: impl Into<String>) -> Result<Response> {
        self.send(Request::BaseSnapshotRebuild {
            params: RepoRef {
                repo_id: repo_id.into(),
            },
        })
    }

    pub fn task_status(&self, task_id: impl Into<String>) -> Result<Response> {
        self.send(Request::TaskStatus {
            params: TaskRef {
                task_id: task_id.into(),
            },
        })
    }

    pub fn task_cancel(&self, task_id: impl Into<String>) -> Result<Response> {
        self.send(Request::TaskCancel {
            params: TaskRef {
                task_id: task_id.into(),
            },
        })
    }
}

fn validate_response_id(line: &str, expected_id: u64) -> Result<()> {
    let id = scan_top_level_response_id(line);
    if id != Some(expected_id) {
        return Err(AppError::Protocol(format!(
            "daemon response id mismatch: expected {expected_id:?}, got {id:?}"
        )));
    }
    Ok(())
}

fn scan_top_level_response_id(line: &str) -> Option<u64> {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        if &bytes[index..index + 4] == b"\"id\"" {
            index += 4;
            while matches!(bytes.get(index), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                index += 1;
            }
            if bytes.get(index) != Some(&b':') {
                continue;
            }
            index += 1;
            while matches!(bytes.get(index), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                index += 1;
            }
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            return std::str::from_utf8(bytes.get(start..index)?)
                .ok()?
                .parse()
                .ok();
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::scan_top_level_response_id;

    #[test]
    fn scan_top_level_response_id_reads_id_without_decoding_large_body() {
        let line = r#"{"jsonrpc":"2.0","id":42,"result":{"r":{"f":[["path",[1,2,3]]]}}}"#;
        assert_eq!(scan_top_level_response_id(line), Some(42));
    }
}
