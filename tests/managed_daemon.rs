use std::{
    ffi::OsStr,
    fs,
    io::ErrorKind,
    net::TcpListener,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};

use flashgrep::daemon::{
    daemon_state_file_path,
    protocol::{
        EnsureRepoParams, GlobParams, OpenRepoParams, PathScope, QuerySpec, RepoConfig,
        SearchParams,
    },
    DaemonClient, ManagedDaemonClient,
};
use tempfile::tempdir;

fn supports_managed_daemon_tests() -> bool {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping managed daemon test in restricted environment: {error}");
            false
        }
        Err(error) => panic!("failed to probe managed daemon test support: {error}"),
    }
}

fn flashgrep_test_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_flashgrep") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return candidate;
        }
    }

    if let Ok(current) = std::env::current_exe() {
        let candidates = [
            current.parent().map(|parent| parent.join("flashgrep")),
            current
                .parent()
                .and_then(|parent| parent.parent())
                .map(|parent| parent.join("flashgrep")),
            Some(PathBuf::from("target/debug/flashgrep")),
        ];
        for candidate in candidates.into_iter().flatten() {
            if candidate.exists()
                && candidate
                    .file_stem()
                    .is_some_and(|stem| stem == OsStr::new("flashgrep"))
            {
                return candidate;
            }
        }
    }

    PathBuf::from("flashgrep")
}

#[test]
fn managed_client_spawns_ensures_and_searches() {
    if !supports_managed_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let managed = ManagedDaemonClient::new().with_daemon_program(flashgrep_test_binary());
    let ensure = managed
        .ensure_repo(EnsureRepoParams {
            repo_path: repo_path.clone(),
            storage_root: None,
            config: RepoConfig::default(),
            refresh: Default::default(),
        })
        .expect("ensure should succeed");
    assert_eq!(ensure.indexed_docs, Some(1));
    assert!(matches!(
        ensure.status.phase,
        flashgrep::daemon::protocol::RepoPhase::ReadyClean
            | flashgrep::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let search = managed
        .search(
            EnsureRepoParams {
                repo_path,
                storage_root: None,
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
            SearchParams {
                repo_id: String::new(),
                query: QuerySpec {
                    pattern: "HELLO".into(),
                    patterns: Vec::new(),
                    case_insensitive: false,
                    multiline: false,
                    dot_matches_new_line: false,
                    fixed_strings: false,
                    word_regexp: false,
                    line_regexp: false,
                    top_k_tokens: 6,
                    max_count: None,
                    global_max_results: None,
                    search_mode: flashgrep::daemon::protocol::SearchModeConfig::CountOnly,
                },
                scope: PathScope::default(),
                allow_scan_fallback: false,
            },
        )
        .expect("search should succeed");

    match search {
        flashgrep::daemon::protocol::Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(matches!(
                backend,
                flashgrep::daemon::protocol::SearchBackend::IndexedClean
            ));
            assert_eq!(results.matched_lines, 1);
            assert!(matches!(
                status.phase,
                flashgrep::daemon::protocol::RepoPhase::ReadyClean
                    | flashgrep::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let client = DaemonClient::new(ensure.addr);
    client
        .send(flashgrep::daemon::protocol::Request::Shutdown)
        .expect("shutdown should succeed");
}

#[test]
fn managed_client_reuses_single_daemon_under_concurrent_ensure() {
    if !supports_managed_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let ensure_params = EnsureRepoParams {
        repo_path: repo_path.clone(),
        storage_root: None,
        config: RepoConfig::default(),
        refresh: Default::default(),
    };
    let state_file =
        daemon_state_file_path(&ensure_params).expect("state file path should resolve");
    let managed = Arc::new(ManagedDaemonClient::new().with_daemon_program(flashgrep_test_binary()));
    let barrier = Arc::new(Barrier::new(4));

    let handles = (0..4)
        .map(|_| {
            let managed = Arc::clone(&managed);
            let barrier = Arc::clone(&barrier);
            let ensure_params = ensure_params.clone();
            thread::spawn(move || {
                barrier.wait();
                managed
                    .ensure_repo(ensure_params)
                    .expect("ensure should succeed")
            })
        })
        .collect::<Vec<_>>();

    let ensured = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread should join"))
        .collect::<Vec<_>>();

    let first = ensured.first().expect("ensured repos should exist");
    assert!(ensured
        .iter()
        .all(|repo| repo.addr == first.addr && repo.repo_id == first.repo_id));
    assert!(!state_file.with_extension("lock").exists());

    let client = DaemonClient::new(first.addr.clone());
    client
        .send(flashgrep::daemon::protocol::Request::Shutdown)
        .expect("shutdown should succeed");
}

#[test]
fn managed_client_glob_uses_daemon_protocol_scope() {
    if !supports_managed_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
    fs::create_dir_all(repo_path.join("tests")).expect("repo dir should succeed");
    fs::create_dir_all(repo_path.join(".git")).expect("git dir should succeed");
    fs::write(repo_path.join(".gitignore"), "ignored.rs\n").expect("write should succeed");
    let src = repo_path.join("src/lib.rs");
    fs::write(&src, "const NAME: &str = \"HELLO\";\n").expect("write should succeed");
    fs::write(
        repo_path.join("tests/lib.rs"),
        "const NAME: &str = \"TEST\";\n",
    )
    .expect("write should succeed");
    fs::write(
        repo_path.join("ignored.rs"),
        "const NAME: &str = \"IGNORED\";\n",
    )
    .expect("write should succeed");

    let managed = ManagedDaemonClient::new().with_daemon_program(flashgrep_test_binary());
    let response = managed
        .glob(
            EnsureRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
            GlobParams {
                repo_id: String::new(),
                scope: PathScope {
                    roots: vec![repo_path.join("src")],
                    globs: vec!["*.rs".into()],
                    ..PathScope::default()
                },
            },
        )
        .expect("glob should succeed");

    match response {
        flashgrep::daemon::protocol::Response::GlobCompleted {
            repo_id,
            paths,
            status,
        } => {
            assert_eq!(
                paths,
                vec![fs::canonicalize(src)
                    .expect("canonicalize should succeed")
                    .to_string_lossy()
                    .into_owned()]
            );
            assert_eq!(repo_id, status.repo_id);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn managed_client_can_open_without_ensuring_index() {
    if !supports_managed_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let managed = ManagedDaemonClient::new().with_daemon_program(flashgrep_test_binary());
    let opened = managed
        .open_repo(OpenRepoParams {
            repo_path,
            storage_root: None,
            config: RepoConfig::default(),
            refresh: Default::default(),
        })
        .expect("open should succeed");
    assert!(matches!(
        opened.status.phase,
        flashgrep::daemon::protocol::RepoPhase::Opening
            | flashgrep::daemon::protocol::RepoPhase::MissingBaseSnapshot
            | flashgrep::daemon::protocol::RepoPhase::ReadyClean
            | flashgrep::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let client = DaemonClient::new(opened.addr);
    client
        .send(flashgrep::daemon::protocol::Request::Shutdown)
        .expect("shutdown should succeed");
}
