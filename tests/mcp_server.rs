#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::tempdir;

use flashgrep::daemon::{protocol::Request, DaemonClient};

#[test]
fn mcp_initialize_and_tools_list_do_not_open_invalid_default_repo() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("not-a-directory");
    std::fs::write(&repo_path, "invalid workspace target").expect("fixture file should exist");

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
            .contains("workspace must be a directory"),
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
    shutdown_managed_daemon(&repo_path);
}

#[test]
fn mcp_processes_share_the_repo_managed_daemon() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    std::fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    std::fs::write(
        repo_path.join("main.rs"),
        "const TOKEN: &str = \"SHARED\";\n",
    )
    .expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);
    let mut first_server = McpServerProcess::spawn(&repo_path);
    let mut first_stdin = first_server
        .child
        .stdin
        .take()
        .expect("stdin should be piped");
    let first_stdout = first_server
        .child
        .stdout
        .take()
        .expect("stdout should be piped");
    let mut first_stdout = BufReader::new(first_stdout);
    let first_search = send_mcp_request(
        &mut first_stdin,
        &mut first_stdout,
        search_request(1, "SHARED"),
    );
    assert_ne!(first_search["result"]["isError"], Value::Bool(true));
    let first_state = managed_daemon_state(&repo_path);

    let mut second_server = McpServerProcess::spawn(&repo_path);
    let mut second_stdin = second_server
        .child
        .stdin
        .take()
        .expect("stdin should be piped");
    let second_stdout = second_server
        .child
        .stdout
        .take()
        .expect("stdout should be piped");
    let mut second_stdout = BufReader::new(second_stdout);
    let second_search = send_mcp_request(
        &mut second_stdin,
        &mut second_stdout,
        search_request(2, "SHARED"),
    );
    assert_ne!(second_search["result"]["isError"], Value::Bool(true));
    let second_state = managed_daemon_state(&repo_path);
    assert_eq!(first_state["pid"], second_state["pid"]);
    assert_eq!(first_state["addr"], second_state["addr"]);

    drop(first_stdin);
    drop(second_stdin);
    first_server.wait_or_kill();
    second_server.wait_or_kill();

    let addr = second_state["addr"]
        .as_str()
        .expect("managed daemon addr should be a string");
    let response = DaemonClient::new(addr)
        .send(Request::Ping)
        .expect("managed daemon should outlive MCP bridges");
    assert!(matches!(
        response,
        flashgrep::daemon::protocol::Response::Pong { .. }
    ));
    shutdown_managed_daemon(&repo_path);
}

#[test]
fn mcp_dispatches_ping_without_waiting_for_search() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    std::fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    std::fs::write(
        repo_path.join("main.rs"),
        "const TOKEN: &str = \"HELLO\";\n",
    )
    .expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);
    let mut server = McpServerProcess::spawn(&repo_path);
    let mut stdin = server.child.stdin.take().expect("stdin should be piped");
    let stdout = server.child.stdout.take().expect("stdout should be piped");
    let mut stdout = BufReader::new(stdout);

    write_mcp_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": { "pattern": "HELLO", "fixed_strings": true }
            }
        }),
    );
    write_mcp_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "ping",
            "params": {}
        }),
    );
    stdin.flush().expect("requests should flush");

    let first = read_mcp_response(&mut stdout);
    assert_eq!(
        first["id"],
        Value::from(2),
        "ping should bypass search: {first}"
    );
    let second = read_mcp_response(&mut stdout);
    assert_eq!(
        second["id"],
        Value::from(1),
        "search response should follow: {second}"
    );
    assert_ne!(second["result"]["isError"], Value::Bool(true), "{second}");

    drop(stdin);
    server.wait_or_kill();
    shutdown_managed_daemon(&repo_path);
}

#[test]
fn repeated_mcp_open_after_checkout_keeps_base_readable_and_advances_incrementally() {
    if !node_is_available() {
        return;
    }

    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    std::fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    std::fs::write(
        repo_path.join("main.rs"),
        "const TOKEN: &str = \"MCP_MAIN_BRANCH\";\n",
    )
    .expect("write should succeed");
    std::fs::write(repo_path.join(".gitignore"), "/.flashgrep-index-engine/\n")
        .expect("gitignore write should succeed");
    ensure_git_repo_with_head(&repo_path);
    let main_branch = Command::new("git")
        .current_dir(&repo_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git branch should run");
    let main_branch = String::from_utf8_lossy(&main_branch.stdout)
        .trim()
        .to_string();

    let mut server = McpServerProcess::spawn(&repo_path);
    let mut stdin = server.child.stdin.take().expect("stdin should be piped");
    let stdout = server.child.stdout.take().expect("stdout should be piped");
    let mut stdout = BufReader::new(stdout);

    let initial = send_mcp_request(
        &mut stdin,
        &mut stdout,
        search_request(1, "MCP_MAIN_BRANCH"),
    );
    assert_ne!(initial["result"]["isError"], Value::Bool(true), "{initial}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let initial_status = loop {
        let status = send_mcp_request(
            &mut stdin,
            &mut stdout,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "repo_status", "arguments": {} }
            }),
        );
        let payload = &status["result"]["structuredContent"]["response"]["status"];
        if matches!(
            payload["phase"].as_str(),
            Some("ready_clean" | "ready_dirty")
        ) && payload["base_manifest_id"].is_string()
        {
            break payload.clone();
        }
        assert!(
            Instant::now() < deadline,
            "initial index timed out: {status}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    let initial_manifest = initial_status["base_manifest_id"].clone();

    run_git(&repo_path, ["checkout", "-qb", "feature-mcp"]);
    std::fs::write(
        repo_path.join("main.rs"),
        "const TOKEN: &str = \"MCP_FEATURE_BRANCH\";\n",
    )
    .expect("feature write should succeed");
    run_git(&repo_path, ["add", "."]);
    run_git(&repo_path, ["commit", "-qm", "feature"]);
    let feature_head = Command::new("git")
        .current_dir(&repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git head should run");
    let feature_head = String::from_utf8_lossy(&feature_head.stdout)
        .trim()
        .to_string();

    let transition = send_mcp_request(
        &mut stdin,
        &mut stdout,
        search_request(3, "MCP_FEATURE_BRANCH"),
    );
    assert_ne!(
        transition["result"]["isError"],
        Value::Bool(true),
        "{transition}"
    );
    let response = &transition["result"]["structuredContent"]["response"];
    assert!(matches!(
        response["backend"].as_str(),
        Some("rg_fallback" | "scan_fallback" | "indexed_workspace_view")
    ));
    assert_ne!(
        response["status"]["phase"],
        Value::String("missing_base_snapshot".into())
    );
    assert!(
        response["results"].to_string().contains("main.rs"),
        "{transition}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let settled = loop {
        let search = send_mcp_request(
            &mut stdin,
            &mut stdout,
            search_request(4, "MCP_FEATURE_BRANCH"),
        );
        let response = &search["result"]["structuredContent"]["response"];
        let status = &response["status"];
        if status["base_head_commit"] == Value::String(feature_head.clone())
            && status["active_task_id"].is_null()
            && matches!(
                status["phase"].as_str(),
                Some("ready_clean" | "ready_dirty")
            )
        {
            break status.clone();
        }
        assert!(
            Instant::now() < deadline,
            "base advance timed out: {search}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(settled["base_delta_depth"], Value::from(1));
    assert_ne!(settled["base_manifest_id"], initial_manifest);
    let feature_manifest = settled["base_manifest_id"].clone();

    run_git(&repo_path, ["checkout", "-q", &main_branch]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let search = send_mcp_request(
            &mut stdin,
            &mut stdout,
            search_request(5, "MCP_MAIN_BRANCH"),
        );
        let response = &search["result"]["structuredContent"]["response"];
        assert_ne!(response["status"]["phase"], "missing_base_snapshot");
        if response["status"]["base_manifest_id"] == initial_manifest
            && response["status"]["active_task_id"].is_null()
        {
            assert!(response["results"].to_string().contains("main.rs"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cached main switch timed out: {search}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    run_git(&repo_path, ["checkout", "-q", "feature-mcp"]);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let search = send_mcp_request(
            &mut stdin,
            &mut stdout,
            search_request(6, "MCP_FEATURE_BRANCH"),
        );
        let response = &search["result"]["structuredContent"]["response"];
        assert_ne!(response["status"]["phase"], "missing_base_snapshot");
        if response["status"]["base_manifest_id"] == feature_manifest
            && response["status"]["active_task_id"].is_null()
        {
            assert!(response["results"].to_string().contains("main.rs"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cached feature switch timed out: {search}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    drop(stdin);
    server.wait_or_kill();
    shutdown_managed_daemon(&repo_path);
}

#[test]
fn mcp_filesystem_and_unborn_workspaces_reconcile_changes_and_offline_restart() {
    if !node_is_available() {
        return;
    }
    for unborn in [false, true] {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let main = repo.join("main.rs");
        std::fs::write(&main, "MCP_BASE_TOKEN\n").unwrap();
        if unborn {
            run_git(&repo, ["init", "-q"]);
        }
        for restarted in [false, true] {
            let mut server = McpServerProcess::spawn(&repo);
            let mut stdin = server.child.stdin.take().unwrap();
            let mut stdout = BufReader::new(server.child.stdout.take().unwrap());
            let initialized = send_mcp_request(
                &mut stdin,
                &mut stdout,
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "filesystem-lifecycle-test", "version": "test" } }
                }),
            );
            assert!(initialized.get("error").is_none(), "{initialized}");
            if !restarted {
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_BASE_TOKEN", 1);
                std::fs::write(&main, "MCP_DIRTY_TOKEN_LONGER\n").unwrap();
                let added = repo.join("added.rs");
                std::fs::write(&added, "MCP_DIRTY_TOKEN_LONGER\n").unwrap();
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_DIRTY_TOKEN_LONGER", 2);
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_BASE_TOKEN", 0);
                std::fs::rename(&added, repo.join("renamed.rs")).unwrap();
                // Equal counts alone cannot establish that rename discovery completed.
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    let response = mcp_expect_indexed_count(
                        &mut stdin,
                        &mut stdout,
                        "MCP_DIRTY_TOKEN_LONGER",
                        2,
                    );
                    let files = response["response"]["results"]["file_counts"].to_string();
                    if files.contains("renamed.rs") && !files.contains("added.rs") {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "rename did not converge: {response}"
                    );
                    thread::sleep(Duration::from_millis(25));
                }
                std::fs::remove_file(&main).unwrap();
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_DIRTY_TOKEN_LONGER", 1);
                std::fs::write(&main, "MCP_BASE_TOKEN\n").unwrap();
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_BASE_TOKEN", 1);
            } else {
                let response = mcp_expect_indexed_count(
                    &mut stdin,
                    &mut stdout,
                    "MCP_OFFLINE_RESTART_TOKEN",
                    1,
                );
                mcp_expect_indexed_count(&mut stdin, &mut stdout, "MCP_DIRTY_TOKEN_LONGER", 0);
                let status = &response["response"]["status"];
                assert!(status["base_head_commit"].is_null(), "{status}");
                assert!(status["workspace_head_commit"].is_null(), "{status}");
                let progress = &status["workspace_progress"];
                assert!(
                    progress["published_revision"].as_u64().unwrap()
                        <= progress["reconciled_revision"].as_u64().unwrap()
                );
                assert!(
                    progress["reconciled_revision"].as_u64().unwrap()
                        <= progress["observed_revision"].as_u64().unwrap()
                );
            }
            drop(stdin);
            server.wait_or_kill();
            shutdown_managed_daemon(&repo);
            let state = std::fs::canonicalize(&repo)
                .unwrap()
                .join(".flashgrep-index-engine/daemon-state.json");
            let deadline = Instant::now() + Duration::from_secs(10);
            while state.exists() {
                assert!(Instant::now() < deadline, "daemon did not finish shutdown");
                thread::sleep(Duration::from_millis(25));
            }
            if !restarted {
                std::fs::remove_file(repo.join("renamed.rs")).unwrap();
                std::fs::write(&main, "MCP_OFFLINE_RESTART_TOKEN\n").unwrap();
            }
        }
        if unborn {
            assert!(!Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", "--verify", "HEAD"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success());
        } else {
            assert!(!repo.join(".git").exists());
        }
    }
}

fn mcp_expect_indexed_count(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    pattern: &str,
    count: u64,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let response = send_mcp_request(
            stdin,
            stdout,
            json!({
                "jsonrpc": "2.0", "id": 10, "method": "tools/call",
                "params": { "name": "search_code", "arguments": {
                    "pattern": pattern, "fixed_strings": true, "search_mode": "count_only", "freshness": "published"
                } }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(
            response["result"]["isError"],
            Value::Bool(true),
            "{response}"
        );
        let payload = &response["result"]["structuredContent"];
        if payload["response"]["results"]["matched_lines"].as_u64() == Some(count)
            && matches!(
                payload["response"]["backend"].as_str(),
                Some("indexed_clean" | "indexed_workspace_view")
            )
        {
            return payload.clone();
        }
        assert!(
            Instant::now() < deadline,
            "indexed result did not converge: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    }
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
    write_mcp_request(stdin, request);
    stdin.flush().expect("request should flush");
    read_mcp_response(stdout)
}

fn write_mcp_request(stdin: &mut ChildStdin, request: Value) {
    let body = serde_json::to_vec(&request).expect("request should encode");
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("header write should succeed");
    stdin.write_all(&body).expect("body write should succeed");
}

fn search_request(id: u64, pattern: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "search_code",
            "arguments": { "pattern": pattern, "fixed_strings": true }
        }
    })
}

fn managed_daemon_state(repo_path: &Path) -> Value {
    let root = std::fs::canonicalize(repo_path).expect("repo should canonicalize");
    let state_path = root.join(".flashgrep-index-engine/daemon-state.json");
    let bytes = std::fs::read(state_path).expect("managed daemon state should exist");
    serde_json::from_slice(&bytes).expect("managed daemon state should decode")
}

fn shutdown_managed_daemon(repo_path: &Path) {
    let state = managed_daemon_state(repo_path);
    let addr = state["addr"]
        .as_str()
        .expect("managed daemon addr should be a string");
    let _ = DaemonClient::new(addr).send(Request::Shutdown);
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
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(Duration::from_millis(25)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
