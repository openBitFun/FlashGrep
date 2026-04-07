use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use codgrep::daemon::{
    protocol::{ConsistencyMode, EnsureRepoParams, PathScope, QuerySpec, RepoConfig, SearchParams},
    daemon_state_file_path, DaemonClient, ManagedDaemonClient,
};
use tempfile::tempdir;

#[test]
fn managed_client_spawns_ensures_and_searches() {
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let managed = ManagedDaemonClient::new().with_daemon_program(env!("CARGO_BIN_EXE_cg"));
    let ensure = managed
        .ensure_repo(EnsureRepoParams {
            repo_path: repo_path.clone(),
            index_path: None,
            config: RepoConfig::default(),
            refresh: Default::default(),
        })
        .expect("ensure should succeed");
    assert_eq!(ensure.indexed_docs, Some(1));
    assert!(matches!(
        ensure.status.phase,
        codgrep::daemon::protocol::RepoPhase::ReadyClean
    ));

    let search = managed
        .search(
            EnsureRepoParams {
                repo_path,
                index_path: None,
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
                    before_context: 0,
                    after_context: 0,
                    top_k_tokens: 6,
                    max_count: None,
                    search_mode: codgrep::daemon::protocol::SearchModeConfig::CountOnly,
                },
                scope: PathScope::default(),
                consistency: ConsistencyMode::WorkspaceEventual,
                allow_scan_fallback: false,
            },
        )
        .expect("search should succeed");

    match search {
        codgrep::daemon::protocol::Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(matches!(
                backend,
                codgrep::daemon::protocol::SearchBackend::IndexedClean
            ));
            assert_eq!(results.matched_lines, 1);
            assert!(matches!(
                status.phase,
                codgrep::daemon::protocol::RepoPhase::ReadyClean
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let client = DaemonClient::new(ensure.addr);
    client
        .send(codgrep::daemon::protocol::Request::Shutdown)
        .expect("shutdown should succeed");
}

#[test]
fn managed_client_reuses_single_daemon_under_concurrent_ensure() {
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let ensure_params = EnsureRepoParams {
        repo_path: repo_path.clone(),
        index_path: None,
        config: RepoConfig::default(),
        refresh: Default::default(),
    };
    let state_file = daemon_state_file_path(&ensure_params).expect("state file path should resolve");
    let managed = Arc::new(ManagedDaemonClient::new().with_daemon_program(env!("CARGO_BIN_EXE_cg")));
    let barrier = Arc::new(Barrier::new(4));

    let handles = (0..4)
        .map(|_| {
            let managed = Arc::clone(&managed);
            let barrier = Arc::clone(&barrier);
            let ensure_params = ensure_params.clone();
            thread::spawn(move || {
                barrier.wait();
                managed.ensure_repo(ensure_params).expect("ensure should succeed")
            })
        })
        .collect::<Vec<_>>();

    let ensured = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread should join"))
        .collect::<Vec<_>>();

    let first = ensured.first().expect("ensured repos should exist");
    assert!(
        ensured
            .iter()
            .all(|repo| repo.addr == first.addr && repo.repo_id == first.repo_id)
    );
    assert!(!state_file.with_extension("lock").exists());

    let client = DaemonClient::new(first.addr.clone());
    client
        .send(codgrep::daemon::protocol::Request::Shutdown)
        .expect("shutdown should succeed");
}
