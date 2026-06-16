use std::{
    fs,
    io::ErrorKind,
    io::{BufRead, BufReader, BufWriter, Write},
    net::TcpListener,
    net::TcpStream,
    process::Command,
    thread,
    time::Duration,
};

use tempfile::tempdir;

use super::*;
use crate::daemon::{
    protocol::{
        ClientCapabilities, InitializeParams, Notification, OpenRepoParams,
        ProgressNotificationParams, RepoConfig, Request, RequestEnvelope, Response,
        ResponseEnvelope, ServerMessage, TaskPhase, TaskState, TaskStatus,
    },
    DaemonClient,
};

fn supports_tcp_server_tests() -> bool {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping TCP daemon server test in restricted environment: {error}");
            false
        }
        Err(error) => panic!("failed to probe TCP daemon server support: {error}"),
    }
}

#[test]
fn serve_writes_and_removes_state_file() {
    if !supports_tcp_server_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let state_file = temp.path().join("daemon-state.json");

    let handle = thread::spawn({
        let state_file = state_file.clone();
        move || {
            serve(ServerOptions {
                bind_addr: "127.0.0.1:0".into(),
                state_file: Some(state_file),
                stdio: false,
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

#[test]
fn initialized_connection_receives_task_notifications() {
    if !supports_tcp_server_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let state_file = temp.path().join("daemon-state.json");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    for index in 0..256 {
        fs::write(
            repo_path.join(format!("file-{index}.rs")),
            format!("pub const NAME_{index}: &str = \"HELLO_{index}\";\n"),
        )
        .expect("write should succeed");
    }
    ensure_git_repo_with_head(&repo_path);

    let handle = thread::spawn({
        let state_file = state_file.clone();
        move || {
            serve(ServerOptions {
                bind_addr: "127.0.0.1:0".into(),
                state_file: Some(state_file),
                stdio: false,
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

    let stream = TcpStream::connect(&state.addr).expect("connect should succeed");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set timeout should succeed");
    let mut reader = BufReader::new(stream.try_clone().expect("clone should succeed"));
    let mut writer = BufWriter::new(stream);

    send_request(
        &mut writer,
        Some(1),
        Request::Initialize {
            params: InitializeParams {
                client_info: None,
                capabilities: ClientCapabilities {
                    progress: true,
                    status_notifications: true,
                    task_notifications: true,
                },
            },
        },
    );
    match read_message(&mut reader) {
        ServerMessage::Response(response) => match response.result.expect("result should exist") {
            Response::InitializeResult { .. } => {}
            other => panic!("unexpected initialize response: {other:?}"),
        },
        other => panic!("unexpected message: {other:?}"),
    }

    send_request(&mut writer, None, Request::Initialized);
    send_request(
        &mut writer,
        Some(2),
        Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        },
    );
    loop {
        match read_message(&mut reader) {
            ServerMessage::Response(response) => {
                if let Some(error) = response.error {
                    panic!("unexpected error response: {error:?}");
                }
                match response.result.expect("result should exist") {
                    Response::RepoOpened { .. } => break,
                    other => panic!("unexpected open response: {other:?}"),
                }
            }
            ServerMessage::Notification(_) => {}
        }
    }

    let repo_id = fs::canonicalize(&repo_path)
        .expect("repo should canonicalize")
        .to_string_lossy()
        .into_owned();
    let mut saw_status_changed = false;
    let mut saw_task_finished = false;
    let mut task_id = None;
    let mut progress_events = Vec::<ProgressNotificationParams>::new();
    let mut finished_task = None::<TaskStatus>;

    send_request(
        &mut writer,
        Some(3),
        Request::BaseSnapshotBuild {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.clone(),
            },
        },
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match read_message(&mut reader) {
            ServerMessage::Response(response) => {
                if let Some(error) = response.error {
                    panic!("unexpected error response: {error:?}");
                }
                match response.result.expect("result should exist") {
                    Response::TaskStarted { task } => {
                        task_id = Some(task.task_id);
                    }
                    Response::ShutdownAck => break,
                    other => panic!("unexpected response: {other:?}"),
                }
            }
            ServerMessage::Notification(notification) => match notification.notification {
                Notification::Progress { params } => {
                    if params.workspace_id == repo_id {
                        if let Some(expected) = task_id.as_ref() {
                            assert_eq!(&params.task_id, expected);
                        }
                        assert!(matches!(
                            params.kind,
                            crate::daemon::protocol::TaskKind::BuildBaseSnapshot
                        ));
                        assert!(!params.message.trim().is_empty());
                        if let Some(total) = params.total {
                            assert!(total > 0);
                            assert!(params.processed <= total);
                        }
                        progress_events.push(params);
                    }
                }
                Notification::WorkspaceStatusChanged { params } => {
                    if params.workspace_id == repo_id {
                        saw_status_changed = true;
                    }
                }
                Notification::TaskFinished { params } => {
                    if params.task.workspace_id == repo_id {
                        saw_task_finished = true;
                        if let Some(expected) = task_id.as_ref() {
                            assert_eq!(&params.task.task_id, expected);
                        }
                        finished_task = Some(params.task);
                    }
                }
            },
        }
        if task_id.is_some()
            && !progress_events.is_empty()
            && saw_status_changed
            && saw_task_finished
        {
            break;
        }
    }

    assert!(task_id.is_some(), "expected task_started response");
    assert!(
        !progress_events.is_empty(),
        "expected progress notification"
    );
    assert!(
        saw_status_changed,
        "expected workspace/statusChanged notification"
    );
    assert!(saw_task_finished, "expected task/finished notification");
    let expected_task_id = task_id.as_ref().expect("task id should be present");
    assert!(progress_events
        .iter()
        .all(|event| &event.task_id == expected_task_id));

    let phases = progress_events
        .iter()
        .filter_map(|event| event.phase)
        .collect::<Vec<_>>();
    assert!(phases.contains(&TaskPhase::Scanning));
    assert!(phases.contains(&TaskPhase::Tokenizing));
    assert!(phases.contains(&TaskPhase::Writing));
    assert!(phases.contains(&TaskPhase::Finalizing));
    assert!(phases.contains(&TaskPhase::RefreshingOverlay));
    assert!(progress_events.iter().any(|event| {
        event.phase == Some(TaskPhase::Tokenizing)
            && event
                .total
                .is_some_and(|total| total > 1 && event.processed > 0)
            && event.total.is_some_and(|total| event.processed < total)
    }));

    let last_progress = progress_events
        .last()
        .expect("progress events should be present");
    assert_eq!(last_progress.phase, Some(TaskPhase::RefreshingOverlay));
    assert_eq!(last_progress.processed, 2);
    assert_eq!(last_progress.total, Some(2));

    let finished_task = finished_task.expect("finished task should be captured");
    assert!(matches!(finished_task.state, TaskState::Completed));
    assert_eq!(finished_task.phase, Some(TaskPhase::RefreshingOverlay));
    assert!(finished_task.message.contains("built base snapshot"));

    let client = DaemonClient::new(state.addr);
    client
        .send(Request::Shutdown)
        .expect("shutdown should succeed");
    handle.join().expect("server thread should join");
}

fn send_request(writer: &mut BufWriter<TcpStream>, id: Option<u64>, request: Request) {
    let envelope = RequestEnvelope {
        jsonrpc: "2.0".into(),
        id,
        request,
    };
    serde_json::to_writer(&mut *writer, &envelope).expect("encode should succeed");
    writer.write_all(b"\n").expect("newline should succeed");
    writer.flush().expect("flush should succeed");
}

fn read_message(reader: &mut BufReader<TcpStream>) -> ServerMessage {
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read should succeed");
        assert!(read > 0, "expected server message");
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("decode json should succeed: {error}; line={line:?}"));
        if value.get("method").is_some() {
            let envelope = serde_json::from_value(value).unwrap_or_else(|error| {
                panic!("decode notification should succeed: {error}; line={line:?}")
            });
            return ServerMessage::Notification(envelope);
        }
        let envelope: ResponseEnvelope = serde_json::from_value(value).unwrap_or_else(|error| {
            panic!("decode response should succeed: {error}; line={line:?}")
        });
        return ServerMessage::Response(envelope);
    }
}

fn ensure_git_repo_with_head(repo_path: &std::path::Path) {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["init", "-q"])
        .status()
        .expect("git init should succeed");
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "user.email", "test@example.com"])
        .status()
        .expect("git config should succeed");
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "user.name", "Test User"])
        .status()
        .expect("git config should succeed");
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["add", "."])
        .status()
        .expect("git add should succeed");
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["commit", "--allow-empty", "-qm", "initial"])
        .status()
        .expect("git commit should succeed");
}

#[test]
fn content_length_stdio_round_trip() {
    let message = ServerMessage::Response(ResponseEnvelope::success(
        Some(7),
        Response::Pong { now_unix_secs: 123 },
    ));
    let mut encoded = Vec::new();
    write_content_length_message(&mut encoded, &message).expect("encode should succeed");
    let mut reader = BufReader::new(std::io::Cursor::new(encoded));
    let request = RequestEnvelope {
        jsonrpc: "2.0".into(),
        id: Some(1),
        request: Request::Ping,
    };
    let mut raw = Vec::new();
    let body = serde_json::to_vec(&request).expect("request should encode");
    write!(&mut raw, "Content-Length: {}\r\n\r\n", body.len()).expect("write should succeed");
    raw.extend_from_slice(&body);
    let mut request_reader = BufReader::new(std::io::Cursor::new(raw));

    let parsed = read_content_length_request(&mut request_reader)
        .expect("request should parse")
        .expect("request should exist");
    assert_eq!(parsed.id, Some(1));
    assert!(matches!(parsed.request, Request::Ping));

    let mut header = String::new();
    reader.read_line(&mut header).expect("header should read");
    assert!(header.starts_with("Content-Length: "));
}
