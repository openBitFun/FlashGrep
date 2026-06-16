use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::tempdir;

#[test]
fn mcp_initialize_and_tools_list_do_not_open_invalid_default_repo() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("not-git");
    std::fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let mut server = McpServerProcess::spawn(&repo_path);
    let mut stdin = server.child.stdin.take().expect("stdin should be piped");
    let stdout = server.child.stdout.take().expect("stdout should be piped");
    let mut stdout = BufReader::new(stdout);

    let initialize = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "flashgrep-test",
                    "version": "test"
                }
            }
        }),
    );
    assert!(initialize.get("error").is_none(), "{initialize}");
    assert_eq!(
        initialize["result"]["serverInfo"]["name"],
        Value::String("flashgrep-mcp".into())
    );

    let tools = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    );
    assert!(tools.get("error").is_none(), "{tools}");
    let tool_names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .expect("tools should be an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name should be a string"))
        .collect();
    assert!(tool_names.contains(&"repo_status"));

    let tool_call = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": {
                    "pattern": "anything"
                }
            }
        }),
    );
    assert!(tool_call.get("error").is_none(), "{tool_call}");
    assert_eq!(tool_call["result"]["isError"], Value::Bool(true));
    assert!(
        tool_call
            .to_string()
            .contains("indexed daemon workspace requires a Git worktree with a HEAD commit"),
        "{tool_call}"
    );

    drop(stdin);
    server.wait_or_kill();
}

fn node_is_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct McpServerProcess {
    child: Child,
}

impl McpServerProcess {
    fn spawn(repo_path: &Path) -> Self {
        let daemon_bin = flashgrep_test_binary();
        let server_js = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mcp")
            .join("server.js");
        let child = Command::new("node")
            .arg(server_js)
            .arg("--repo")
            .arg(repo_path)
            .env("FLASHGREP_DAEMON_BIN", daemon_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("MCP server should spawn");
        Self { child }
    }

    fn wait_or_kill(&mut self) {
        wait_or_kill(&mut self.child);
    }
}

impl Drop for McpServerProcess {
    fn drop(&mut self) {
        wait_or_kill(&mut self.child);
    }
}

fn flashgrep_test_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_flashgrep") {
        return PathBuf::from(path);
    }
    let binary = if cfg!(windows) {
        "flashgrep.exe"
    } else {
        "flashgrep"
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join(binary)
}

fn send_mcp_request(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    request: Value,
) -> Value {
    let body = serde_json::to_vec(&request).expect("request should encode");
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("header write should succeed");
    stdin.write_all(&body).expect("body write should succeed");
    stdin.flush().expect("flush should succeed");
    read_mcp_response(stdout)
}

fn read_mcp_response(stdout: &mut BufReader<ChildStdout>) -> Value {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .expect("header read should succeed");
        assert!(read > 0, "MCP server closed stdout before response");
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(length) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                length
                    .trim()
                    .parse::<usize>()
                    .expect("content length should parse"),
            );
        }
    }

    let content_length = content_length.expect("response should include Content-Length");
    let mut body = vec![0u8; content_length];
    stdout
        .read_exact(&mut body)
        .expect("response body read should succeed");
    serde_json::from_slice(&body).expect("response should decode")
}

fn wait_or_kill(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
