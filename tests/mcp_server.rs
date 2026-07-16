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
    assert_eq!(
        initialize["result"]["serverInfo"]["version"],
        Value::String(env!("CARGO_PKG_VERSION").into())
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
    let search_tool = tools["result"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "search_code"))
        .expect("search_code tool should be present");
    assert!(
        search_tool["inputSchema"]["properties"]
            .get("allow_scan_fallback")
            .is_none(),
        "transition fallback should not require a caller option"
    );

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

#[test]
fn mcp_default_repo_from_subdirectory_reuses_git_root_storage() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let src_path = repo_path.join("src").join("nested");
    std::fs::create_dir_all(&src_path).expect("repo dir should succeed");
    std::fs::write(repo_path.join("README.md"), "SHARED_TOKEN\n").expect("write should succeed");
    std::fs::write(
        src_path.join("main.rs"),
        "const NAME: &str = \"SHARED_TOKEN\";\n",
    )
    .expect("write should succeed");
    for index in 0..300 {
        std::fs::write(
            repo_path.join(format!("bulk-{index}.rs")),
            format!("const BULK_{index}: &str = \"bulk value {index}\";\n"),
        )
        .expect("bulk file write should succeed");
    }
    ensure_git_repo_with_head(&repo_path);

    let mut server = McpServerProcess::spawn_from_cwd(&src_path);
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

    let status = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "repo_status",
                "arguments": {}
            }
        }),
    );
    assert!(status.get("error").is_none(), "{status}");
    assert_ne!(status["result"]["isError"], Value::Bool(true), "{status}");

    let content = status["result"]["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["text"].as_str())
        .expect("repo_status should return text content");
    let payload: Value = serde_json::from_str(content).expect("repo_status content should be JSON");
    let repo_root = std::fs::canonicalize(&repo_path).expect("repo path should canonicalize");
    let expected_repo_id = repo_root.to_string_lossy().into_owned();
    let expected_storage_root = repo_root
        .join(".flashgrep-index-engine")
        .to_string_lossy()
        .into_owned();

    assert_eq!(payload["repo_id"], Value::String(expected_repo_id));
    assert_eq!(
        payload["response"]["status"]["storage_root"],
        Value::String(expected_storage_root)
    );

    let search_started = Instant::now();
    let search = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": {
                    "pattern": "SHARED_TOKEN",
                    "fixed_strings": true
                }
            }
        }),
    );
    assert!(search.get("error").is_none(), "{search}");
    assert_ne!(search["result"]["isError"], Value::Bool(true), "{search}");
    assert!(
        search_started.elapsed() < Duration::from_secs(5),
        "search should not wait for index construction"
    );
    let search_payload = &search["result"]["structuredContent"];
    assert_eq!(
        search_payload["requested_root"],
        Value::String(
            std::fs::canonicalize(&src_path)
                .expect("subdirectory should canonicalize")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert!(
        search_payload["index_task"]["task_id"].is_string(),
        "{search}"
    );
    assert!(
        matches!(
            search_payload["response"]["backend"].as_str(),
            Some("rg_fallback" | "scan_fallback")
        ),
        "search during initial build should use a transition backend: {search}"
    );
    let search_results = search_payload["response"]["results"].to_string();
    assert!(search_results.contains("main.rs"), "{search}");
    assert!(!search_results.contains("README.md"), "{search}");

    let second_search = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": {
                    "pattern": "SHARED_TOKEN",
                    "fixed_strings": true
                }
            }
        }),
    );
    assert!(second_search.get("error").is_none(), "{second_search}");
    assert_ne!(
        second_search["result"]["isError"],
        Value::Bool(true),
        "a repeated search must reuse the running build task: {second_search}"
    );

    let build = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "build_index",
                "arguments": {}
            }
        }),
    );
    assert!(build.get("error").is_none(), "{build}");
    assert_ne!(build["result"]["isError"], Value::Bool(true), "{build}");

    let glob = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "glob_files",
                "arguments": {}
            }
        }),
    );
    assert!(glob.get("error").is_none(), "{glob}");
    assert_ne!(glob["result"]["isError"], Value::Bool(true), "{glob}");
    let glob_paths = glob["result"]["structuredContent"]["response"]["paths"].to_string();
    assert!(glob_paths.contains("main.rs"), "{glob}");
    assert!(!glob_paths.contains("README.md"), "{glob}");
    assert!(
        !src_path.join(".flashgrep-index-engine").exists(),
        "subdirectory must not get its own index storage"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = send_mcp_request(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": "repo_status",
                    "arguments": {}
                }
            }),
        );
        let phase = status["result"]["structuredContent"]["response"]["status"]["phase"]
            .as_str()
            .unwrap_or_default();
        if matches!(phase, "ready_clean" | "ready_dirty") {
            break;
        }
        assert!(Instant::now() < deadline, "index build timed out: {status}");
        thread::sleep(Duration::from_millis(25));
    }

    let indexed_search = send_mcp_request(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": {
                    "pattern": "SHARED_TOKEN",
                    "fixed_strings": true
                }
            }
        }),
    );
    assert_ne!(
        indexed_search["result"]["isError"],
        Value::Bool(true),
        "{indexed_search}"
    );
    let indexed_payload = &indexed_search["result"]["structuredContent"];
    assert!(
        matches!(
            indexed_payload["response"]["backend"].as_str(),
            Some("indexed_clean" | "indexed_workspace_view")
        ),
        "search should switch to the root index after the background build: {indexed_search}"
    );
    let indexed_results = indexed_payload["response"]["results"].to_string();
    assert!(indexed_results.contains("main.rs"), "{indexed_search}");
    assert!(!indexed_results.contains("README.md"), "{indexed_search}");

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
        Self::spawn_inner(Some(repo_path), None)
    }

    fn spawn_from_cwd(cwd: &Path) -> Self {
        Self::spawn_inner(None, Some(cwd))
    }

    fn spawn_inner(repo_path: Option<&Path>, cwd: Option<&Path>) -> Self {
        let daemon_bin = flashgrep_test_binary();
        let server_js = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("mcp")
            .join("server.js");
        let mut command = Command::new("node");
        command.arg(server_js);
        if let Some(repo_path) = repo_path {
            command.arg("--repo").arg(repo_path);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command
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

fn ensure_git_repo_with_head(repo_path: &Path) {
    run_git(repo_path, ["init", "-q"]);
    run_git(repo_path, ["config", "user.name", "Test User"]);
    run_git(repo_path, ["config", "user.email", "test@example.com"]);
    run_git(repo_path, ["add", "."]);
    run_git(repo_path, ["commit", "-qm", "initial"]);
}

fn run_git<const N: usize>(repo_path: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
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
