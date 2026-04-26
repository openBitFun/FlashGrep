use super::*;

#[test]
fn refresh_workspace_rebuilds_workspace_overlay_engine() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"DIRTY_OVERLAY_ENGINE\";\n")
        .expect("rewrite should succeed");

    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    wait_for_workspace_overlay_match_count(&repo_path, "DIRTY_OVERLAY_ENGINE", 1);
}

#[test]
fn probe_marks_dirty_paths_for_eventual_workspace_search() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"DIRTY\";\n").expect("rewrite should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 1, 0, 0);

    let search = search_repo(
        &service,
        &repo_id,
        "DIRTY",
        ConsistencyMode::WorkspaceEventual,
    );

    match search {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
            assert_eq!(results.matched_lines, 1);
            assert_eq!(status.dirty_files.modified, 1);
            assert_eq!(status.dirty_files.deleted, 0);
            assert_eq!(status.dirty_files.new, 0);
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn eventual_search_uses_transition_fallback_while_overlay_engine_recovers() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"DIRTY_FALLBACK\";\n")
        .expect("rewrite should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 1, 0, 0);

    clear_workspace_overlay_engine(&service, &repo_id);

    match search_repo(
        &service,
        &repo_id,
        "DIRTY_FALLBACK",
        ConsistencyMode::WorkspaceEventual,
    ) {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(is_transition_or_indexed_workspace_backend(backend));
            assert_eq!(results.matched_lines, 1);
            assert_eq!(status.dirty_files.modified, 1);
            assert_eq!(status.dirty_files.deleted, 0);
            assert_eq!(status.dirty_files.new, 0);
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY_FALLBACK",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_exact_dirty_stats(&status, 1, 0, 0);
}

#[test]
fn eventual_search_rebuilds_full_overlay_after_overlay_engine_reset() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let first = repo_path.join("first.rs");
    let second = repo_path.join("second.rs");
    fs::write(&first, "const NAME: &str = \"BASE_FIRST\";\n").expect("write should succeed");
    fs::write(&second, "const NAME: &str = \"BASE_SECOND\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("rewrite should succeed");
    fs::write(&second, "const NAME: &str = \"DIRTY_SECOND\";\n").expect("rewrite should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 2
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 2, 0, 0);

    clear_workspace_overlay_engine(&service, &repo_id);

    fs::write(&first, "const NAME: &str = \"DIRTY_FIRST_AGAIN\";\n")
        .expect("rewrite should succeed");

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY_SECOND",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY_FIRST_AGAIN",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    assert!(status.dirty_files.modified >= 1);
}

#[test]
fn eventual_search_honors_configured_overlay_auto_checkpoint_threshold() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let (repo_id, _) = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 256,
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    ..crate::daemon::protocol::RefreshPolicyConfig::default()
                },
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, status } => {
            let status = if matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening) {
                wait_for_status(&service, &repo_id, |status| {
                    !matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening)
                })
            } else {
                status
            };
            (repo_id, status)
        }
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    let first = repo_path.join("first.rs");
    fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("write should succeed");

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY_FIRST",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    wait_for_workspace_overlay_match_count(&repo_path, "DIRTY_FIRST", 1);
    wait_for_workspace_overlay_committed_match_count(&repo_path, "DIRTY_FIRST", 1);
}

#[test]
fn repo_status_reports_overlay_runtime_stats() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 256,
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    ..crate::daemon::protocol::RefreshPolicyConfig::default()
                },
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    let first = repo_path.join("first.rs");
    fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("write should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        status
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.active_segments >= 1 && overlay.committed_seq_no >= 1)
    });
    let overlay = status.overlay.expect("overlay stats should be present");
    assert!(overlay.active_segments >= 1);
    assert_eq!(overlay.active_delete_segments, 0);
    assert_eq!(overlay.uncommitted_ops, 0);
    assert!(overlay.merge_attempts >= overlay.merge_completed);
    assert_eq!(overlay.merge_failed, 0);
    assert_eq!(overlay.last_merge_error, None);
}

#[test]
fn overlay_merge_is_deferred_and_runs_in_background() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 256,
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 500,
                    overlay_merge_retry_delay_ms: 100,
                },
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    let first = repo_path.join("first.rs");
    fs::write(&first, "const NAME: &str = \"DIRTY_MERGE_DELAY\";\n").expect("write should succeed");

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY_MERGE_DELAY",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert!(status
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.committed_seq_no >= 1));

    let requested = wait_for_status(&service, &repo_id, |status| {
        status.overlay.as_ref().is_some_and(|overlay| {
            overlay.active_segments >= 1
                && overlay.committed_seq_no >= 1
                && overlay.merge_requested
                && !overlay.merge_running
        })
    });
    assert!(requested
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.merge_requested));

    let settled = wait_for_status(&service, &repo_id, |status| {
        status.overlay.as_ref().is_some_and(|overlay| {
            overlay.active_segments >= 1
                && overlay.committed_seq_no >= 1
                && !overlay.merge_requested
                && !overlay.merge_running
        })
    });
    assert!(settled
        .overlay
        .as_ref()
        .is_some_and(|overlay| !overlay.merge_requested));
}

#[test]
fn workspace_eventual_search_stays_stable_while_overlay_merge_is_pending() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"MERGE_STABLE_OLD\";\n")
        .expect("write should succeed");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 256,
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 500,
                    overlay_merge_retry_delay_ms: 100,
                },
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"MERGE_STABLE_NEW\";\n")
        .expect("rewrite should succeed");

    let requested = wait_for_status(&service, &repo_id, |status| {
        status.overlay.as_ref().is_some_and(|overlay| {
            overlay.committed_seq_no >= 1 && overlay.merge_requested && !overlay.merge_running
        })
    });
    assert!(requested
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.merge_requested));

    let mut saw_requested = false;
    for _ in 0..12 {
        let (_, new_status) = assert_search_count_now(
            &service,
            &repo_id,
            "MERGE_STABLE_NEW",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        let _ = assert_search_count_now(
            &service,
            &repo_id,
            "MERGE_STABLE_OLD",
            ConsistencyMode::WorkspaceEventual,
            0,
        );
        if new_status
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.merge_requested || overlay.merge_running)
        {
            saw_requested = true;
        }
        if new_status
            .overlay
            .as_ref()
            .is_some_and(|overlay| !overlay.merge_requested && !overlay.merge_running)
        {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    assert!(saw_requested);

    let settled = wait_for_status(&service, &repo_id, |status| {
        status
            .overlay
            .as_ref()
            .is_some_and(|overlay| !overlay.merge_requested && !overlay.merge_running)
    });
    assert!(settled
        .overlay
        .as_ref()
        .is_some_and(|overlay| overlay.merge_failed == 0));
}

#[test]
fn workspace_eventual_search_stays_stable_during_forced_overlay_rebuild() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"REBUILD_STABLE_BASE\";\n")
        .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"REBUILD_STABLE_DIRTY\";\n")
        .expect("rewrite should succeed");
    let refreshed = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        refreshed.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    wait_for_workspace_overlay_match_count(&repo_path, "REBUILD_STABLE_DIRTY", 1);

    clear_workspace_overlay_engine(&service, &repo_id);
    clear_published_overlay_paths_cache(&service, &repo_id);
    mark_workspace_probe_pending(&service, &repo_id);

    for _ in 0..12 {
        let _ = assert_search_count_now(
            &service,
            &repo_id,
            "REBUILD_STABLE_DIRTY",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        let _ = assert_search_count_now(
            &service,
            &repo_id,
            "REBUILD_STABLE_BASE",
            ConsistencyMode::WorkspaceEventual,
            0,
        );
        thread::sleep(Duration::from_millis(30));
    }

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
    });
    assert!(status.probe_healthy);
    assert_eq!(status.dirty_files.deleted, 0);
    assert_eq!(status.dirty_files.new, 0);
}

#[test]
fn probe_reconciles_batched_tracked_file_updates() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");

    let dirty_count = 96usize;
    for index in 0..dirty_count {
        fs::write(
            repo_path.join(format!("tracked_{index:03}.rs")),
            "const NAME: &str = \"BASE\";\n",
        )
        .expect("write should succeed");
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    for index in 0..dirty_count {
        fs::write(
            repo_path.join(format!("tracked_{index:03}.rs")),
            "const NAME: &str = \"DIRTY\";\n",
        )
        .expect("rewrite should succeed");
    }

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == dirty_count
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, dirty_count, 0, 0);

    let (backend, search_status) = wait_for_search_count(
        &service,
        &repo_id,
        "DIRTY",
        ConsistencyMode::WorkspaceEventual,
        dirty_count,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert_eq!(search_status.dirty_files.modified, dirty_count);
}

#[test]
fn probe_tracks_new_text_files() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(
        repo_path.join("added.rs"),
        "const NAME: &str = \"ADDED\";\n",
    )
    .expect("write should succeed");

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "ADDED",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert_exact_dirty_stats(&status, 0, 0, 1);
}

#[test]
fn probe_tracks_new_text_files_inside_new_directory_without_extra_probe() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let nested = repo_path.join("nested");
    fs::create_dir_all(&nested).expect("nested dir should succeed");
    fs::write(
        nested.join("added.rs"),
        "const NAME: &str = \"NESTED_ADDED\";\n",
    )
    .expect("nested write should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 1
    });
    assert_exact_dirty_stats(&status, 0, 0, 1);

    match search_repo(
        &service,
        &repo_id,
        "NESTED_ADDED",
        ConsistencyMode::WorkspaceEventual,
    ) {
        Response::SearchCompleted {
            backend, results, ..
        } => {
            assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
            assert_eq!(results.matched_lines, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn probe_tracks_deleted_files() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let deleted = repo_path.join("deleted.rs");
    fs::write(&deleted, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::remove_file(&deleted).expect("remove should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.deleted == 1
    });
    assert_eq!(status.dirty_files.modified, 0);
    assert_eq!(status.dirty_files.new, 0);

    match search_repo(
        &service,
        &repo_id,
        "BASE",
        ConsistencyMode::WorkspaceEventual,
    ) {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(matches!(
                backend,
                SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
            ));
            assert_eq!(results.matched_lines, 0);
            assert_eq!(status.dirty_files.deleted, 1);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn reopened_daemon_materializes_fixed_string_phrase_from_published_overlay() {
    let _serial = daemon_test_serial();
    let setup_service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let cpp = repo_path.join("tracked.cpp");
    let header = repo_path.join("tracked.h");
    fs::write(
        &cpp,
        "/// VersionTuple baseline line.\nconst char *Name = \"VersionTuple\";\n",
    )
    .expect("write should succeed");
    fs::write(
        &header,
        "class VersionTuple {\n  /// VersionTuple baseline line.\n};\n",
    )
    .expect("write should succeed");

    match ensure_repo(&setup_service, repo_path.clone()) {
        Response::RepoEnsured { status, .. } => {
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }
    setup_service.shutdown_all();
    drop(setup_service);

    let _serial = daemon_test_serial();
    let service = DaemonService::new();
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

    let phrase = "VersionTuple probe validation content gamma";
    fs::write(
        &cpp,
        "/// VersionTuple baseline line.\n/// VersionTuple probe edit baseline.\nconst char *Name = \"VersionTuple\";\n",
    )
    .expect("write should succeed");
    fs::write(
        &header,
        "class VersionTuple {\n  /// VersionTuple probe validation content gamma.\n  /// VersionTuple baseline line.\n};\n",
    )
    .expect("write should succeed");

    let status = match service
        .handle_request_only(Request::RefreshRepo {
            params: crate::daemon::protocol::RefreshRepoParams {
                repo_id: repo_id.clone(),
                force: true,
            },
        })
        .expect("refresh should succeed")
    {
        Response::RepoStatus { status } => status,
        other => panic!("unexpected response: {other:?}"),
    };
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    assert!(status.probe_healthy);

    let phrase_query = QuerySpec {
        pattern: regex::escape(phrase),
        patterns: vec![phrase.into()],
        fixed_strings: true,
        search_mode: SearchModeConfig::MaterializeMatches,
        ..query(phrase)
    };
    let (backend, results, _) = wait_for_search_with_params_count(
        &service,
        &repo_id,
        phrase_query,
        vec![header],
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(
        backend,
        SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
    ));
    assert!(!results.hits.is_empty());
    assert!(results
        .hits
        .iter()
        .any(|hit| hit.path.ends_with("tracked.h")));
    assert!(results
        .hits
        .iter()
        .flat_map(|hit| hit.matches.iter())
        .any(|matched| matched
            .snippet
            .contains("VersionTuple probe validation content gamma")));
}

#[test]
fn published_overlay_uses_latest_contents_after_multiple_edits() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const VALUE: &str = \"FIRST_EDIT\";\n")
        .expect("first rewrite should succeed");
    let _ = refresh_repo(&service, &repo_id, true);
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
    });
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let (backend, _) = wait_for_search_count(
        &service,
        &repo_id,
        "FIRST_EDIT",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));

    fs::write(&tracked, "const VALUE: &str = \"SECOND_EDIT\";\n")
        .expect("second rewrite should succeed");
    let _ = refresh_repo(&service, &repo_id, true);
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
    });
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "FIRST_EDIT",
        ConsistencyMode::WorkspaceEventual,
        0,
    );

    let (backend, _) = wait_for_search_count(
        &service,
        &repo_id,
        "SECOND_EDIT",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(is_transition_or_indexed_workspace_backend(backend));
}

#[test]
fn repeated_incremental_edits_converge_to_expected_overlay_state() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"REPEAT_BASE\";\n").expect("write should succeed");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 256,
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 50,
                    overlay_merge_retry_delay_ms: 25,
                },
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    let mut latest = String::new();
    let mut prior_tokens = Vec::new();
    for index in 0..8 {
        latest = format!("REPEAT_EDIT_TOKEN_{index}");
        if index > 0 {
            prior_tokens.push(format!("REPEAT_EDIT_TOKEN_{}", index - 1));
        }
        fs::write(&tracked, format!("const VALUE: &str = \"{latest}\";\n"))
            .expect("rewrite should succeed");
    }

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        &latest,
        ConsistencyMode::WorkspaceEventual,
        1,
    );

    let settled = wait_for_status(&service, &repo_id, |status| {
        status.overlay.as_ref().is_some_and(|overlay| {
            !overlay.merge_requested
                && !overlay.merge_running
                && overlay.active_delete_segments == 0
                && overlay.active_segments == 1
        })
    });
    let overlay = settled.overlay.expect("overlay stats should be present");
    assert_eq!(overlay.active_segments, 1);
    assert_eq!(overlay.active_delete_segments, 0);
    assert_eq!(overlay.merge_failed, 0);

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "REPEAT_BASE",
        ConsistencyMode::WorkspaceEventual,
        0,
    );
    for token in prior_tokens {
        let _ = wait_for_search_count(
            &service,
            &repo_id,
            &token,
            ConsistencyMode::WorkspaceEventual,
            0,
        );
    }
}

#[test]
fn repeated_published_overlay_searches_remain_stable() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(
        &tracked,
        "const VALUE: &str = \"REPEATED_DIRTY_TOKEN_20260410\";\n",
    )
    .expect("rewrite should succeed");
    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    for _ in 0..3 {
        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "REPEATED_DIRTY_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(is_transition_or_indexed_workspace_backend(backend));
    }
}

#[test]
fn mixed_incremental_sequence_remains_searchable() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(
        &tracked,
        "const VALUE: &str = \"MIXED_EXISTING_TOKEN_20260410\";\n",
    )
    .expect("rewrite should succeed");
    let status = refresh_repo(&service, &repo_id, true);
    assert!(status.dirty_files.modified >= 1);

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "MIXED_EXISTING_TOKEN_20260410",
        ConsistencyMode::WorkspaceEventual,
        1,
    );

    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("revert should succeed");
    refresh_repo(&service, &repo_id, true);
    wait_for_search_count(
        &service,
        &repo_id,
        "MIXED_EXISTING_TOKEN_20260410",
        ConsistencyMode::WorkspaceEventual,
        0,
    );

    let nested = repo_path.join("nested");
    fs::create_dir_all(&nested).expect("nested dir should succeed");
    fs::write(
        nested.join("live.rs"),
        "const VALUE: &str = \"MIXED_NEWDIR_TOKEN_20260410\";\n",
    )
    .expect("nested write should succeed");
    refresh_repo(&service, &repo_id, true);

    let (_, status) = wait_for_search_count(
        &service,
        &repo_id,
        "MIXED_NEWDIR_TOKEN_20260410",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    for _ in 0..3 {
        let (_, status) = wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_NEWDIR_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
    }
}

#[test]
fn workspace_eventual_search_does_not_run_dirty_diff_observation() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let observation_path = temp.path().join("dirty-diff.tsv");
    unsafe {
        std::env::set_var("CODGREP_OBSERVE_DIRTY_DIFF_FILE", &observation_path);
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let (backend, _) = wait_for_search_count(
        &service,
        &repo_id,
        "BASE",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedClean));
    assert!(!observation_path.exists());

    unsafe {
        std::env::remove_var("CODGREP_OBSERVE_DIRTY_DIFF_FILE");
    }
}

#[test]
fn path_scoped_search_limits_published_overlay_results() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let left = repo_path.join("left.rs");
    let right = repo_path.join("right.rs");
    fs::write(&left, "const VALUE: &str = \"BASE\";\n").expect("left write should succeed");
    fs::write(&right, "const VALUE: &str = \"BASE\";\n").expect("right write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&left, "const VALUE: &str = \"SCOPED_TOKEN\";\n")
        .expect("left rewrite should succeed");
    fs::write(&right, "const VALUE: &str = \"SCOPED_TOKEN\";\n")
        .expect("right rewrite should succeed");
    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let query = QuerySpec {
        search_mode: SearchModeConfig::MaterializeMatches,
        ..query("SCOPED_TOKEN")
    };
    match wait_for_search_with_params_count(
        &service,
        &repo_id,
        query,
        vec![left.clone()],
        ConsistencyMode::WorkspaceEventual,
        1,
    ) {
        (backend, results, _) => {
            assert!(matches!(
                backend,
                SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
            ));
            assert_eq!(results.hits.len(), 1);
            assert!(results.hits[0].path.ends_with("left.rs"));
            assert!(results.hits[0]
                .matches
                .iter()
                .any(|matched| matched.snippet.contains("SCOPED_TOKEN")));
        }
    }
}

#[test]
fn transient_new_file_does_not_leave_phantom_match_after_delete() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"BASE\";\n",
    )
    .expect("tracked write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let transient = repo_path.join("transient.rs");
    fs::write(&transient, "const VALUE: &str = \"EPHEMERAL_TOKEN\";\n")
        .expect("transient write should succeed");
    fs::remove_file(&transient).expect("transient delete should succeed");

    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    match search_repo(
        &service,
        &repo_id,
        "EPHEMERAL_TOKEN",
        ConsistencyMode::WorkspaceEventual,
    ) {
        Response::SearchCompleted { results, .. } => {
            assert_eq!(results.matched_lines, 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn probe_removes_overlay_only_new_file_without_refresh() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"BASE\";\n",
    )
    .expect("tracked write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let nested = repo_path.join("nested");
    fs::create_dir_all(&nested).expect("nested dir should succeed");
    let transient = nested.join("transient.rs");
    fs::write(
        &transient,
        "const VALUE: &str = \"PROBE_DELETE_TOKEN_20260410\";\n",
    )
    .expect("transient write should succeed");
    wait_for_workspace_overlay_match_count(&repo_path, "PROBE_DELETE_TOKEN_20260410", 1);

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "PROBE_DELETE_TOKEN_20260410",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    fs::remove_file(&transient).expect("transient delete should succeed");
    fs::remove_dir(&nested).expect("nested dir delete should succeed");
    wait_for_workspace_overlay_match_count(&repo_path, "PROBE_DELETE_TOKEN_20260410", 0);

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "PROBE_DELETE_TOKEN_20260410",
        ConsistencyMode::WorkspaceEventual,
        0,
    );
    assert!(matches!(
        backend,
        SearchBackend::IndexedClean
            | SearchBackend::IndexedWorkspaceView
            | SearchBackend::RgFallback
            | SearchBackend::ScanFallback
    ));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::Opening
            | crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let status = wait_for_status(&service, &repo_id, |status| {
        status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 0, 0, 0);

    for _ in 0..3 {
        match search_repo(
            &service,
            &repo_id,
            "PROBE_DELETE_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted { results, .. } => {
                assert_eq!(results.matched_lines, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

#[test]
fn probe_clears_restored_tracked_file_without_refresh() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("tracked write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(
        &tracked,
        "const VALUE: &str = \"PROBE_RESTORE_TOKEN_20260416\";\n",
    )
    .expect("tracked rewrite should succeed");
    wait_for_workspace_overlay_match_count(&repo_path, "PROBE_RESTORE_TOKEN_20260416", 1);

    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "PROBE_RESTORE_TOKEN_20260416",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("tracked restore should succeed");
    wait_for_workspace_overlay_match_count(&repo_path, "PROBE_RESTORE_TOKEN_20260416", 0);
    let repo_status = get_status(&service, &repo_id);
    let build_config = crate::BuildConfig {
        repo_path: repo_path.clone(),
        storage_root: PathBuf::from(&repo_status.storage_root),
        tokenizer: crate::TokenizerMode::SparseNgram,
        corpus_mode: crate::CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size: 512 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let metadata =
        crate::index_engine::WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&build_config)
            .expect("read base snapshot metadata should succeed")
            .expect("base snapshot metadata should exist");
    let base_snapshot = crate::index_engine::WorkspaceBaseSnapshotIndex::open_if_matching(
        &build_config,
        &metadata.snapshot_key,
    )
    .expect("open base snapshot index should succeed")
    .expect("base snapshot index should match")
    .open_committed()
    .expect("open committed base snapshot should succeed");
    let base_catalog = base_snapshot
        .path_catalog()
        .expect("path catalog should load");
    let tracked_entry = base_catalog
        .get("tracked.rs")
        .expect("tracked entry should exist");
    let tracked_bytes = fs::read(&tracked).expect("tracked bytes should load");
    let mut tracked_hash = [0u8; 32];
    let tracked_crc = crc32fast::hash(&tracked_bytes);
    tracked_hash[..4].copy_from_slice(&tracked_crc.to_le_bytes());
    assert_eq!(tracked_entry.content_hash, tracked_hash);
    let (backend, status) = wait_for_search_count(
        &service,
        &repo_id,
        "PROBE_RESTORE_TOKEN_20260416",
        ConsistencyMode::WorkspaceEventual,
        0,
    );
    assert!(matches!(
        backend,
        SearchBackend::IndexedClean
            | SearchBackend::IndexedWorkspaceView
            | SearchBackend::RgFallback
            | SearchBackend::ScanFallback
    ));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::Opening
            | crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let status = wait_for_status(&service, &repo_id, |status| {
        status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn probe_keeps_indexed_reads_while_directory_events_wait_for_reconcile() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::create_dir_all(repo_path.join("nested")).expect("mkdir should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_eq!(status.dirty_files.modified, 0);
    assert_eq!(status.dirty_files.deleted, 0);
    assert_eq!(status.dirty_files.new, 0);

    match search_repo(
        &service,
        &repo_id,
        "BASE",
        ConsistencyMode::WorkspaceEventual,
    ) {
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
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let refreshed = service
        .handle_request_only(Request::RefreshRepo {
            params: crate::daemon::protocol::RefreshRepoParams {
                repo_id,
                force: false,
            },
        })
        .expect("refresh should succeed");

    match refreshed {
        Response::RepoStatus { status } => {
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
            ));
            assert_eq!(status.dirty_files.modified, 0);
            assert_eq!(status.dirty_files.deleted, 0);
            assert_eq!(status.dirty_files.new, 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn probe_updates_invalidate_glob_cache() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
    let stable = repo_path.join("src/lib.rs");
    fs::write(&stable, "pub const NAME: &str = \"STABLE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    let stable = fs::canonicalize(&stable)
        .expect("canonicalize should succeed")
        .to_string_lossy()
        .into_owned();
    let scope = PathScope {
        globs: vec!["*.rs".into()],
        ..PathScope::default()
    };

    match glob_repo(&service, &repo_id, scope.clone()) {
        Response::GlobCompleted { paths, .. } => {
            assert_eq!(paths, vec![stable.clone()]);
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let fresh = repo_path.join("src/fresh.rs");
    fs::write(&fresh, "pub const NAME: &str = \"FRESH\";\n").expect("write should succeed");
    let fresh = fs::canonicalize(&fresh)
        .expect("canonicalize should succeed")
        .to_string_lossy()
        .into_owned();

    let status = wait_for_glob_paths(
        &service,
        &repo_id,
        scope.clone(),
        &[fresh.clone(), stable.clone()],
    );
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::Opening
            | crate::daemon::protocol::RepoPhase::ReadyDirty
            | crate::daemon::protocol::RepoPhase::ReadyClean
    ));

    fs::remove_file(repo_path.join("src/fresh.rs")).expect("delete should succeed");
    let status = wait_for_glob_paths(&service, &repo_id, scope, &[stable.clone()]);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::Opening
            | crate::daemon::protocol::RepoPhase::ReadyDirty
            | crate::daemon::protocol::RepoPhase::ReadyClean
    ));
}

#[test]
#[ignore = "known gap: tracked directory restore can temporarily lose stable query results without refresh"]
fn probe_reconciles_tracked_directory_delete_and_restore_without_refresh() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let nested = repo_path.join("nested");
    fs::create_dir_all(&nested).expect("nested dir should succeed");
    let first = nested.join("first.rs");
    let second = nested.join("second.rs");
    fs::write(&first, "const NAME: &str = \"DIR_DELETE_ALPHA\";\n")
        .expect("first write should succeed");
    fs::write(&second, "const NAME: &str = \"DIR_DELETE_BETA\";\n")
        .expect("second write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::remove_dir_all(&nested).expect("tracked dir delete should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.deleted == 2
    });
    assert_exact_dirty_stats(&status, 0, 2, 0);

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "DIR_DELETE_ALPHA",
        ConsistencyMode::WorkspaceEventual,
        0,
    );
    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "DIR_DELETE_BETA",
        ConsistencyMode::WorkspaceEventual,
        0,
    );

    fs::create_dir_all(&nested).expect("nested dir restore should succeed");
    fs::write(&first, "const NAME: &str = \"DIR_DELETE_ALPHA\";\n")
        .expect("first restore should succeed");
    fs::write(&second, "const NAME: &str = \"DIR_DELETE_BETA\";\n")
        .expect("second restore should succeed");

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 0, 0, 0);

    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "DIR_DELETE_ALPHA",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
    let _ = wait_for_search_count(
        &service,
        &repo_id,
        "DIR_DELETE_BETA",
        ConsistencyMode::WorkspaceEventual,
        1,
    );
}
