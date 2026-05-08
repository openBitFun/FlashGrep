use super::*;

#[test]
fn opening_git_repo_ignores_default_storage_root_in_gitignore() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    init_git_repo(&repo_path);

    let (_, status) = open_repo(&service, repo_path.clone());
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));

    let gitignore =
        fs::read_to_string(repo_path.join(".gitignore")).expect(".gitignore read should succeed");
    assert_eq!(gitignore, "/.flashgrep-index-engine/\n");
}

#[test]
fn opening_git_repo_does_not_duplicate_existing_storage_ignore_entry() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    init_git_repo(&repo_path);
    fs::write(repo_path.join(".gitignore"), ".flashgrep-index-engine/\n")
        .expect(".gitignore write should succeed");

    let _ = open_repo(&service, repo_path.clone());

    let gitignore =
        fs::read_to_string(repo_path.join(".gitignore")).expect(".gitignore read should succeed");
    assert_eq!(gitignore, ".flashgrep-index-engine/\n");
}

#[test]
fn opening_git_repo_ignores_custom_repo_local_storage_root_in_gitignore() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(repo_path.join("custom")).expect("custom dir should succeed");
    init_git_repo(&repo_path);

    let response = service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(repo_path.join("custom/index")),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed");
    match response {
        Response::RepoOpened { status, .. } => {
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::Opening
                    | crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let gitignore =
        fs::read_to_string(repo_path.join(".gitignore")).expect(".gitignore read should succeed");
    assert_eq!(gitignore, "/custom/index/\n");
}

#[test]
fn opening_non_git_repo_does_not_create_gitignore() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let _ = open_repo(&service, repo_path.clone());

    assert!(!repo_path.join(".gitignore").exists());
}

#[test]
fn daemon_reports_missing_index_then_builds_and_searches() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"WORKTREE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, status) = open_repo(&service, repo_path.clone());
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));

    let build = service
        .handle_request_only(Request::BuildBaseSnapshot {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.clone(),
            },
        })
        .expect("build should succeed");
    let status = match build {
        Response::RepoBaseSnapshotBuilt { status, .. } => status,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
    ));

    let search = search_repo(&service, &repo_id, "WORKTREE");

    match search {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(matches!(
                backend,
                SearchBackend::IndexedClean | SearchBackend::IndexedWorkspaceView
            ));
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn ready_status_and_search_recover_from_overlay_snapshot_when_published_overlay_paths_cache_is_empty(
) {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASELINE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const VALUE: &str = \"SNAPSHOT_DIRTY\";\n")
        .expect("rewrite should succeed");
    let _ = refresh_repo(&service, &repo_id, true);
    let refreshed = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_eq!(refreshed.dirty_files.modified, 1);
    wait_for_workspace_overlay_match_count(&repo_path, "SNAPSHOT_DIRTY", 1);

    clear_published_overlay_paths_cache(&service, &repo_id);

    let status = get_status(&service, &repo_id);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    assert_eq!(status.dirty_files.modified, 1);
    assert_eq!(status.dirty_files.deleted, 0);
    assert_eq!(status.dirty_files.new, 0);

    let (_, status) = wait_for_search_count(&service, &repo_id, "SNAPSHOT_DIRTY", 1);
    assert_eq!(status.dirty_files.modified, 1);
}

#[test]
fn probe_delta_uses_overlay_snapshot_truth_when_published_overlay_paths_cache_is_empty() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let first = repo_path.join("first.rs");
    let second = repo_path.join("second.rs");
    fs::write(&first, "const VALUE: &str = \"BASE_FIRST\";\n").expect("first write should succeed");
    fs::write(&second, "const VALUE: &str = \"BASE_SECOND\";\n")
        .expect("second write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(
        &first,
        "const VALUE: &str = \"FIRST_DIRTY_TOKEN_20260416\";\n",
    )
    .expect("first rewrite should succeed");
    let _ = refresh_repo(&service, &repo_id, true);
    let refreshed = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_eq!(refreshed.dirty_files.modified, 1);
    wait_for_workspace_overlay_match_count(&repo_path, "FIRST_DIRTY_TOKEN_20260416", 1);

    clear_published_overlay_paths_cache(&service, &repo_id);

    fs::write(
        &second,
        "const VALUE: &str = \"SECOND_DIRTY_TOKEN_20260416\";\n",
    )
    .expect("second rewrite should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified >= 2
    });
    assert_eq!(status.dirty_files.modified, 2);
    assert_eq!(status.dirty_files.deleted, 0);
    assert_eq!(status.dirty_files.new, 0);

    let _ = wait_for_search_count(&service, &repo_id, "FIRST_DIRTY_TOKEN_20260416", 1);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SECOND_DIRTY_TOKEN_20260416", 1);
    assert_eq!(status.dirty_files.modified, 2);
}

#[test]
fn workspace_search_uses_transition_fallback_without_base_index() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"SCAN_ONLY\";\n",
    )
    .expect("write should succeed");

    let (repo_id, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));

    match service
        .handle_request_only(Request::Search {
            params: crate::daemon::protocol::SearchParams {
                repo_id,
                query: query("SCAN_ONLY"),
                scope: PathScope::default(),
                allow_scan_fallback: true,
            },
        })
        .expect("search should succeed")
    {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(matches!(
                backend,
                SearchBackend::RgFallback | SearchBackend::ScanFallback
            ));
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn reopening_repo_reuses_base_snapshot_shard_for_workspace_search() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"REOPEN_BASE_SHARD\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
    build_repo(&setup_service, &repo_id);
    wait_for_status(&setup_service, &repo_id, |status| {
        matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        )
    });
    setup_service.shutdown_all();

    let service = DaemonService::new();
    let (repo_id, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    assert_exact_dirty_stats(&status, 0, 0, 0);
    assert!(has_base_snapshot_engine(&service, &repo_id));

    match search_repo(&service, &repo_id, "REOPEN_BASE_SHARD") {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(matches!(
                backend,
                SearchBackend::IndexedClean | SearchBackend::IndexedWorkspaceView
            ));
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn reopening_repo_detects_preexisting_dirty_files_during_open() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"BASELINE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
    build_repo(&setup_service, &repo_id);
    wait_for_status(&setup_service, &repo_id, |status| {
        matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        )
    });
    setup_service.shutdown_all();

    fs::write(
        repo_path.join("added.rs"),
        "const NAME: &str = \"REOPEN_DIRTY_FILE\";\n",
    )
    .expect("write should succeed");

    let service = DaemonService::new();
    let (repo_id, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    assert!(has_base_snapshot_engine(&service, &repo_id));
    assert_exact_dirty_stats(&status, 0, 0, 1);

    let (backend, search_status) =
        wait_for_search_count(&service, &repo_id, "REOPEN_DIRTY_FILE", 1);
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert_eq!(search_status.dirty_files.new, 1);
}

#[test]
fn opening_legacy_snapshot_without_base_shard_is_missing_index() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"LEGACY_ONLY\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
    build_repo(&setup_service, &repo_id);
    wait_for_status(&setup_service, &repo_id, |status| {
        matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        )
    });

    setup_service.shutdown_all();
    fs::remove_dir_all(repo_path.join(".flashgrep-index-engine"))
        .expect("removing base shard should succeed");

    let service = DaemonService::new();
    let (_, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));
}

#[test]
fn daemon_ignores_corrupted_legacy_generation_when_base_snapshot_shard_exists() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"WORKTREE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
    build_repo(&setup_service, &repo_id);
    let built = wait_for_status(&setup_service, &repo_id, |status| {
        matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        )
    });

    let storage_root = PathBuf::from(&built.storage_root);
    let generation = "legacy-corrupted";
    let layout = storage_root.join("generations").join(generation);
    fs::create_dir_all(&layout).expect("legacy generation dir should exist");
    fs::write(storage_root.join("CURRENT"), format!("{generation}\n"))
        .expect("legacy current should exist");
    fs::write(layout.join("docs.bin"), b"corrupted").expect("corrupt docs should succeed");

    setup_service.shutdown_all();

    let service = DaemonService::new();
    let (repo_id, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    match search_repo(&service, &repo_id, "WORKTREE") {
        Response::SearchCompleted { results, .. } => {
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn glob_lists_paths_without_requiring_index_and_respects_scope_filters() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
    fs::create_dir_all(repo_path.join("tests")).expect("repo dir should succeed");
    fs::create_dir_all(repo_path.join(".git")).expect("git dir should succeed");
    fs::write(repo_path.join(".gitignore"), "ignored.rs\n").expect("write should succeed");
    let src = repo_path.join("src/lib.rs");
    let test = repo_path.join("tests/lib.rs");
    fs::write(&src, "pub const NAME: &str = \"SRC\";\n").expect("write should succeed");
    fs::write(&test, "pub const NAME: &str = \"TEST\";\n").expect("write should succeed");
    fs::write(
        repo_path.join("ignored.rs"),
        "pub const NAME: &str = \"IGNORED\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());

    let src = fs::canonicalize(src).expect("canonicalize should succeed");
    let test = fs::canonicalize(test).expect("canonicalize should succeed");

    match glob_repo(
        &service,
        &repo_id,
        PathScope {
            globs: vec!["*.rs".into()],
            types: vec!["rust".into()],
            ..PathScope::default()
        },
    ) {
        Response::GlobCompleted { paths, .. } => {
            assert_eq!(
                paths,
                vec![
                    src.to_string_lossy().into_owned(),
                    test.to_string_lossy().into_owned()
                ]
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    match glob_repo(
        &service,
        &repo_id,
        PathScope {
            roots: vec![repo_path.join("src")],
            globs: vec!["*.rs".into()],
            ..PathScope::default()
        },
    ) {
        Response::GlobCompleted { paths, .. } => {
            assert_eq!(paths, vec![src.to_string_lossy().into_owned()]);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn scope_roots_must_stay_within_repo_boundary() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
    fs::write(
        repo_path.join("src/lib.rs"),
        "pub const NAME: &str = \"SRC\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    let outside_root = repo_path
        .parent()
        .expect("repo should have parent")
        .to_path_buf();

    let search_error = service
        .handle_request_only(Request::Search {
            params: crate::daemon::protocol::SearchParams {
                repo_id: repo_id.clone(),
                query: query("SRC"),
                scope: PathScope {
                    roots: vec![outside_root.clone()],
                    ..PathScope::default()
                },
                allow_scan_fallback: false,
            },
        })
        .expect_err("search should reject roots outside repo");
    assert!(search_error
        .to_string()
        .contains("scope root escapes repo boundary"));

    let glob_error = service
        .handle_request_only(Request::Glob {
            params: GlobParams {
                repo_id,
                scope: PathScope {
                    roots: vec![outside_root],
                    ..PathScope::default()
                },
            },
        })
        .expect_err("glob should reject roots outside repo");
    assert!(glob_error
        .to_string()
        .contains("scope root escapes repo boundary"));
}

#[test]
fn async_base_snapshot_build_returns_task_and_completes() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let (repo_id, status) = open_repo(&service, repo_path);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));

    let started = service
        .handle_request_only(Request::BaseSnapshotBuild {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.clone(),
            },
        })
        .expect("async build should succeed");
    let task_id = match started {
        Response::TaskStarted { task } => {
            assert_eq!(task.workspace_id, repo_id);
            assert!(matches!(
                task.state,
                TaskState::Queued | TaskState::Running | TaskState::Completed
            ));
            task.task_id
        }
        other => panic!("unexpected response: {other:?}"),
    };

    let task = wait_for_task_status(&service, &task_id, |task| {
        matches!(task.state, TaskState::Completed)
    });
    assert!(matches!(
        task.kind,
        crate::daemon::protocol::TaskKind::BuildBaseSnapshot
    ));

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.active_task_id.is_none()
    });
    assert!(status.probe_healthy);

    match search_repo(&service, &repo_id, "HELLO") {
        Response::SearchCompleted { results, .. } => {
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn eventual_search_uses_fallback_while_repo_is_still_opening_or_missing_index() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
        .expect("write should succeed");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };

    match search_repo(&service, &repo_id, "HELLO") {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(matches!(
                backend,
                SearchBackend::RgFallback | SearchBackend::ScanFallback
            ));
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn eventual_search_prefers_current_snapshot_while_repo_is_still_opening() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    for index in 0..256 {
        fs::write(
            repo_path.join(format!("file-{index}.rs")),
            format!("const NAME_{index}: &str = \"HELLO\";\n"),
        )
        .expect("write should succeed");
    }

    match ensure_repo(&setup_service, repo_path.clone()) {
        Response::RepoEnsured { status, .. } => {
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
            assert!(status.probe_healthy);
        }
        other => panic!("unexpected response: {other:?}"),
    }
    setup_service.shutdown_all();

    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let (repo_id, status) = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, status } => (repo_id, status),
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    match search_repo(&service, &repo_id, "HELLO") {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(
                matches!(backend, SearchBackend::IndexedClean),
                "workspace eventual search should keep using the current snapshot, got {backend:?}"
            );
            assert_eq!(results.matched_lines, 256);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn eventual_search_keeps_indexed_reads_while_clean_repo_has_pending_reconcile() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE_PENDING\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path);
    build_repo(&service, &repo_id);

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });
    assert_exact_dirty_stats(&status, 0, 0, 0);

    mark_workspace_probe_pending(&service, &repo_id);

    match search_repo(&service, &repo_id, "BASE_PENDING") {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(matches!(backend, SearchBackend::IndexedClean));
            assert_eq!(results.matched_lines, 1);
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
            ));
            assert_exact_dirty_stats(&status, 0, 0, 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn background_reconcile_uses_git_incremental_recovery() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    init_git_repo(&repo_path);
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE_GIT_PENDING\";\n",
    )
    .expect("write should succeed");
    git_commit_all(&repo_path, "initial");

    let (repo_id, _) = open_repo(&service, repo_path);
    build_repo(&service, &repo_id);
    let ready = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });
    let baseline_probe = ready.last_probe_unix_secs;
    assert_eq!(ready.dirty_files.modified, 0);
    assert_eq!(ready.dirty_files.deleted, 0);
    assert_eq!(ready.dirty_files.new, 0);

    mark_workspace_probe_pending(&service, &repo_id);

    let settled = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.last_probe_unix_secs != baseline_probe
    });
    assert_eq!(settled.dirty_files.modified, 0);
    assert_eq!(settled.dirty_files.deleted, 0);
    assert_eq!(settled.dirty_files.new, 0);
}

#[test]
fn jsonrpc_initialize_and_ping_use_v2_envelope() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();

    let initialize = service.handle(RequestEnvelope {
        jsonrpc: "2.0".into(),
        id: Some(1),
        request: Request::Initialize {
            params: InitializeParams::default(),
        },
    });
    assert_eq!(initialize.jsonrpc, "2.0");
    assert_eq!(initialize.id, Some(1));
    assert!(initialize.error.is_none());
    match initialize.result.expect("initialize result should exist") {
        Response::InitializeResult {
            protocol_version,
            capabilities,
            ..
        } => {
            assert_eq!(protocol_version, 1);
            assert!(capabilities.workspace_open);
            assert!(capabilities.search_query);
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let ping = service.handle(RequestEnvelope {
        jsonrpc: "2.0".into(),
        id: Some(2),
        request: Request::Ping,
    });
    assert_eq!(ping.jsonrpc, "2.0");
    assert_eq!(ping.id, Some(2));
    match ping.result.expect("ping result should exist") {
        Response::Pong { now_unix_secs } => {
            assert!(now_unix_secs > 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn ensure_repo_builds_missing_index_once() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("main.rs"),
        "const NAME: &str = \"WORKTREE\";\n",
    )
    .expect("write should succeed");

    match ensure_repo(&service, repo_path.clone()) {
        Response::RepoEnsured {
            repo_id,
            indexed_docs,
            status,
        } => {
            assert_eq!(
                repo_id,
                fs::canonicalize(&repo_path)
                    .expect("repo should canonicalize")
                    .to_string_lossy()
            );
            assert_eq!(indexed_docs, Some(1));
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    match ensure_repo(&service, repo_path) {
        Response::RepoEnsured {
            indexed_docs,
            status,
            ..
        } => {
            assert_eq!(indexed_docs, None);
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}
