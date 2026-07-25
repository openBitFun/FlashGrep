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
    ensure_git_repo_with_head(&repo_path);

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    fs::write(&tracked, "const NAME: &str = \"DIRTY_OVERLAY_ENGINE\";\n")
        .expect("rewrite should succeed");

    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_exact_dirty_stats(&status, 1, 0, 0);

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
    ensure_git_repo_with_head(&repo_path);

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

    let (backend, status) = wait_for_search_count(&service, &repo_id, "DIRTY", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_eq!(status.dirty_files.modified, 1);
    assert_eq!(status.dirty_files.deleted, 0);
    assert_eq!(status.dirty_files.new, 0);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
}

#[test]
fn eventual_search_stays_indexed_while_overlay_engine_recovers() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);

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

    match search_repo(&service, &repo_id, "DIRTY_FALLBACK") {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(is_indexed_workspace_backend(backend));
            assert!(results.matched_lines <= 1);
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

    match search_repo(&service, &repo_id, "BASE") {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert!(is_indexed_workspace_backend(backend));
            assert!(results.matched_lines <= 1);
            assert_eq!(status.dirty_files.modified, 1);
            assert_eq!(status.dirty_files.deleted, 0);
            assert_eq!(status.dirty_files.new, 0);
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let (backend, status) = wait_for_search_count(&service, &repo_id, "DIRTY_FALLBACK", 1);
    assert!(is_indexed_workspace_backend(backend));
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
    ensure_git_repo_with_head(&repo_path);

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

    let (backend, status) = wait_for_search_count(&service, &repo_id, "DIRTY_SECOND", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let (backend, status) = wait_for_search_count(&service, &repo_id, "DIRTY_FIRST_AGAIN", 1);
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
    ensure_git_repo_with_head(&repo_path);

    let (repo_id, _) = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
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

    let _ = wait_for_search_count(&service, &repo_id, "DIRTY_FIRST", 1);
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
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
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
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 500,
                    overlay_merge_retry_delay_ms: 100,
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
    fs::write(&first, "const NAME: &str = \"DIRTY_MERGE_DELAY\";\n").expect("write should succeed");

    let (backend, status) = wait_for_search_count(&service, &repo_id, "DIRTY_MERGE_DELAY", 1);
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
fn workspace_search_stays_stable_while_overlay_merge_is_pending() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const NAME: &str = \"MERGE_STABLE_OLD\";\n")
        .expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 500,
                    overlay_merge_retry_delay_ms: 100,
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
        let (new_backend, new_results, new_status) =
            match search_repo(&service, &repo_id, "MERGE_STABLE_NEW") {
                Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                } => (backend, results, status),
                other => panic!("unexpected response: {other:?}"),
            };
        assert!(is_indexed_workspace_backend(new_backend));

        let (old_backend, old_results, _) =
            match search_repo(&service, &repo_id, "MERGE_STABLE_OLD") {
                Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                } => (backend, results, status),
                other => panic!("unexpected response: {other:?}"),
            };
        assert!(is_indexed_workspace_backend(old_backend));
        let visible_matches = new_results.matched_lines + old_results.matched_lines;
        assert!(
            (1..=2).contains(&visible_matches),
            "expected pending merge to keep at least one visible revision, got new={} old={}",
            new_results.matched_lines,
            old_results.matched_lines
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
fn workspace_search_stays_stable_during_forced_overlay_rebuild() {
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
    let _ = refresh_repo(&service, &repo_id, true);
    let refreshed = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified == 1
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert!(matches!(
        refreshed.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    wait_for_workspace_overlay_match_count(&repo_path, "REBUILD_STABLE_DIRTY", 1);

    clear_workspace_overlay_engine(&service, &repo_id);
    clear_published_overlay_paths_cache(&service, &repo_id);
    mark_workspace_probe_pending(&service, &repo_id);

    for _ in 0..12 {
        let (backend, _) = wait_for_search_count(&service, &repo_id, "REBUILD_STABLE_DIRTY", 1);
        assert!(is_transition_or_indexed_workspace_backend(backend));
        let (backend, _) = wait_for_search_count(&service, &repo_id, "REBUILD_STABLE_BASE", 0);
        assert!(is_transition_or_indexed_workspace_backend(backend));
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

    let (backend, search_status) = wait_for_search_count(&service, &repo_id, "DIRTY", dirty_count);
    assert!(is_transition_or_indexed_workspace_backend(backend));
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

    let (backend, status) = wait_for_search_count(&service, &repo_id, "ADDED", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_exact_dirty_stats(&status, 0, 0, 1);
}

#[test]
fn probe_respect_ignore_skips_ignored_untracked_files() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join(".gitignore"), "ignored.rs\n").expect("write should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig {
                    corpus_mode: crate::daemon::protocol::CorpusModeConfig::RespectIgnore,
                    ..RepoConfig::default()
                },
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    fs::write(
        repo_path.join("ignored.rs"),
        "const NAME: &str = \"IGNORED_UNTRACKED\";\n",
    )
    .expect("write should succeed");

    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let (backend, _) = wait_for_search_count(&service, &repo_id, "IGNORED_UNTRACKED", 0);
    assert!(matches!(backend, SearchBackend::IndexedClean));
}

#[test]
fn probe_no_ignore_tracks_ignored_untracked_files() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(repo_path.join(".gitignore"), "ignored.rs\n").expect("write should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("write should succeed");
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig {
                    corpus_mode: crate::daemon::protocol::CorpusModeConfig::NoIgnore,
                    ..RepoConfig::default()
                },
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);

    fs::write(
        repo_path.join("ignored.rs"),
        "const NAME: &str = \"IGNORED_UNTRACKED\";\n",
    )
    .expect("write should succeed");

    let (backend, status) = wait_for_search_count(&service, &repo_id, "IGNORED_UNTRACKED", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
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

    let (backend, _) = wait_for_search_count(&service, &repo_id, "NESTED_ADDED", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
}

#[test]
fn watcher_file_events_use_git_pathspec_fast_probe() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let probe_mode_path = temp.path().join("probe-mode.log");
    unsafe {
        std::env::set_var("FLASHGREP_TEST_PROBE_MODE_FILE", &probe_mode_path);
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });
    wait_for_workspace_watcher_active(&service, &repo_id);
    let _ = fs::remove_file(&probe_mode_path);

    fs::write(&tracked, "const VALUE: &str = \"FAST_PATH_TOKEN\";\n")
        .expect("rewrite should succeed");
    let (backend, status) = wait_for_search_count(&service, &repo_id, "FAST_PATH_TOKEN", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_exact_dirty_stats(&status, 1, 0, 0);

    let probe_modes =
        fs::read_to_string(&probe_mode_path).expect("probe mode log should be written");
    unsafe {
        std::env::remove_var("FLASHGREP_TEST_PROBE_MODE_FILE");
    }
    assert!(
        probe_modes.lines().any(|line| line == "fast"),
        "expected fast probe in {probe_modes:?}"
    );
}

#[test]
fn watcher_gitignore_events_fall_back_to_full_git_probe() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"BASE\";\n",
    )
    .expect("write should succeed");

    let probe_mode_path = temp.path().join("probe-mode.log");
    unsafe {
        std::env::set_var("FLASHGREP_TEST_PROBE_MODE_FILE", &probe_mode_path);
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    let _ = fs::remove_file(&probe_mode_path);

    fs::write(repo_path.join(".gitignore"), "*.tmp\n").expect("gitignore write should succeed");
    fs::write(
        repo_path.join("added.rs"),
        "const VALUE: &str = \"FULL_PROBE_TOKEN\";\n",
    )
    .expect("added write should succeed");

    let (backend, status) = wait_for_search_count(&service, &repo_id, "FULL_PROBE_TOKEN", 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_exact_dirty_stats(&status, 0, 0, 1);

    let probe_modes =
        fs::read_to_string(&probe_mode_path).expect("probe mode log should be written");
    unsafe {
        std::env::remove_var("FLASHGREP_TEST_PROBE_MODE_FILE");
    }
    assert!(
        probe_modes.lines().any(|line| line == "full"),
        "expected full probe in {probe_modes:?}"
    );
}

#[test]
fn scoped_search_observes_watcher_updates_without_query_time_probe() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    fs::write(&tracked, "const VALUE: &str = \"SCOPED_SEARCH_TOKEN\";\n")
        .expect("rewrite should succeed");

    let query = query("SCOPED_SEARCH_TOKEN");
    let (backend, results, status) =
        wait_for_search_with_params_count(&service, &repo_id, query, vec![tracked], 1);
    assert!(is_transition_or_indexed_workspace_backend(backend));
    assert_eq!(results.matched_lines, 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);
}

#[test]
fn scoped_search_does_not_advance_workspace_probe_state() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"SCOPED_BASE_TOKEN\";\n")
        .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path);
    build_repo(&service, &repo_id);
    let before = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    let (_, results, after) = wait_for_search_with_params_count(
        &service,
        &repo_id,
        query("SCOPED_BASE_TOKEN"),
        vec![tracked],
        1,
    );

    assert_eq!(results.matched_lines, 1);
    assert_eq!(after.last_probe_unix_secs, before.last_probe_unix_secs);
}

#[test]
fn indexed_base_and_overlay_preserve_word_and_line_match_semantics() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.cc");
    fs::write(
        &tracked,
        "NOTREACHED();\nDUMP_WILL_BE_NOTREACHED();\n  NOTREACHED();\n  NOTREACHED(); // base\n",
    )
    .expect("write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path);
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    let mut word_query = query("NOTREACHED");
    word_query.patterns = vec!["NOTREACHED".into()];
    word_query.fixed_strings = true;
    word_query.word_regexp = true;
    word_query.search_mode = SearchModeConfig::LineMatches;

    let raw_line = "  NOTREACHED();";
    let mut line_query = query(&regex::escape(raw_line));
    line_query.patterns = vec![raw_line.into()];
    line_query.fixed_strings = true;
    line_query.line_regexp = true;
    line_query.search_mode = SearchModeConfig::LineMatches;

    let (_, word_results, _) =
        wait_for_search_with_params_count(&service, &repo_id, word_query.clone(), Vec::new(), 3);
    assert_eq!(
        word_results
            .line_matches
            .iter()
            .map(|matched| matched.line_number)
            .collect::<Vec<_>>(),
        vec![1, 3, 4]
    );
    let (_, line_results, _) =
        wait_for_search_with_params_count(&service, &repo_id, line_query.clone(), Vec::new(), 1);
    assert_eq!(line_results.line_matches[0].line_number, 3);

    fs::write(
        &tracked,
        "DUMP_WILL_BE_NOTREACHED();\nNOTREACHED();\n  NOTREACHED();\n  NOTREACHED(); // overlay\n",
    )
    .expect("rewrite should succeed");
    wait_for_dirty_stats(&service, &repo_id, 1, 0, 0);
    wait_for_search_count(&service, &repo_id, "overlay", 1);

    let (_, word_results, _) =
        wait_for_search_with_params_count(&service, &repo_id, word_query, Vec::new(), 3);
    assert_eq!(
        word_results
            .line_matches
            .iter()
            .map(|matched| matched.line_number)
            .collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    let (_, line_results, _) =
        wait_for_search_with_params_count(&service, &repo_id, line_query, Vec::new(), 1);
    assert_eq!(line_results.line_matches[0].line_number, 3);
}

fn write_sequence_token(path: &std::path::Path, token: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("sequence parent dir should be created");
    }
    fs::write(path, format!("const VALUE: &str = \"{token}\";\n"))
        .expect("sequence file write should succeed");
}

fn remove_sequence_file(path: &std::path::Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("sequence file remove should succeed: {error:?}"),
    }
}

fn git_sequence(repo_path: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .status()
        .expect("git sequence command should run");
    assert!(
        status.success(),
        "git sequence command should succeed: git -C {} {}",
        repo_path.display(),
        args.join(" ")
    );
}

fn wait_for_scoped_token_count(
    service: &DaemonService,
    repo_id: &str,
    path: &std::path::Path,
    token: &str,
    expected: usize,
) -> crate::daemon::protocol::RepoStatus {
    wait_for_scoped_token_count_for_roots(service, repo_id, &[path.to_path_buf()], token, expected)
}

fn wait_for_scoped_token_count_for_roots(
    service: &DaemonService,
    repo_id: &str,
    roots: &[PathBuf],
    token: &str,
    expected: usize,
) -> crate::daemon::protocol::RepoStatus {
    let roots = roots
        .iter()
        .map(|path| canonical_sequence_scope_path(path))
        .collect::<Vec<_>>();
    let (_, _, status) =
        wait_for_search_with_params_count(service, repo_id, query(token), roots, expected);
    status
}

fn canonical_sequence_scope_path(path: &std::path::Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    match fs::canonicalize(parent) {
        Ok(parent) => match path.file_name() {
            Some(file_name) => parent.join(file_name),
            None => parent,
        },
        Err(_) => path.to_path_buf(),
    }
}

#[derive(Clone, Copy)]
enum CommonSequenceAction {
    Write(&'static str),
    Remove,
    RestoreBase,
}

#[derive(Clone, Copy)]
struct CommonSequenceStep {
    action: CommonSequenceAction,
    expected_token: Option<&'static str>,
    absent_token: Option<&'static str>,
    expected_dirty_stats: (usize, usize, usize),
}

fn write_common_sequence_state(
    path: &std::path::Path,
    action: CommonSequenceAction,
    base_token: &'static str,
) {
    match action {
        CommonSequenceAction::Write(token) => write_sequence_token(path, token),
        CommonSequenceAction::Remove => remove_sequence_file(path),
        CommonSequenceAction::RestoreBase => write_sequence_token(path, base_token),
    }
}

fn run_common_sequence_steps(
    service: &DaemonService,
    repo_id: &str,
    path: &std::path::Path,
    base_token: &'static str,
    steps: &[CommonSequenceStep],
) {
    for step in steps {
        write_common_sequence_state(path, step.action, base_token);
        let probe_token = step
            .expected_token
            .or(step.absent_token)
            .unwrap_or(base_token);
        let mut status = wait_for_scoped_token_count(
            service,
            repo_id,
            path,
            probe_token,
            usize::from(step.expected_token == Some(probe_token)),
        );
        assert_exact_dirty_stats(
            &status,
            step.expected_dirty_stats.0,
            step.expected_dirty_stats.1,
            step.expected_dirty_stats.2,
        );
        if let Some(absent_token) = step.absent_token {
            status = wait_for_scoped_token_count(service, repo_id, path, absent_token, 0);
            assert_exact_dirty_stats(
                &status,
                step.expected_dirty_stats.0,
                step.expected_dirty_stats.1,
                step.expected_dirty_stats.2,
            );
        }
    }
}

#[test]
fn watcher_reconciles_common_tracked_operation_permutations() {
    let _serial = daemon_test_serial();
    let cases: &[(&str, &[CommonSequenceStep])] = &[
        (
            "modify_modify_delete_restore_modify",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_MMDRM_A"),
                    expected_token: Some("SEQ_PERM_T_MMDRM_A"),
                    absent_token: Some("SEQ_PERM_TRACKED_BASE_modify_modify_delete_restore_modify"),
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_MMDRM_B"),
                    expected_token: Some("SEQ_PERM_T_MMDRM_B"),
                    absent_token: Some("SEQ_PERM_T_MMDRM_A"),
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_T_MMDRM_B"),
                    expected_dirty_stats: (0, 1, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::RestoreBase,
                    expected_token: Some(
                        "SEQ_PERM_TRACKED_BASE_modify_modify_delete_restore_modify",
                    ),
                    absent_token: Some("SEQ_PERM_T_MMDRM_B"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_MMDRM_C"),
                    expected_token: Some("SEQ_PERM_T_MMDRM_C"),
                    absent_token: Some("SEQ_PERM_TRACKED_BASE_modify_modify_delete_restore_modify"),
                    expected_dirty_stats: (1, 0, 0),
                },
            ],
        ),
        (
            "delete_restore_modify_restore_delete",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_restore_modify_restore_delete",
                    ),
                    expected_dirty_stats: (0, 1, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::RestoreBase,
                    expected_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_restore_modify_restore_delete",
                    ),
                    absent_token: None,
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_DRMRA_A"),
                    expected_token: Some("SEQ_PERM_T_DRMRA_A"),
                    absent_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_restore_modify_restore_delete",
                    ),
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::RestoreBase,
                    expected_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_restore_modify_restore_delete",
                    ),
                    absent_token: Some("SEQ_PERM_T_DRMRA_A"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_restore_modify_restore_delete",
                    ),
                    expected_dirty_stats: (0, 1, 0),
                },
            ],
        ),
        (
            "delete_write_write_restore_write_delete",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_write_write_restore_write_delete",
                    ),
                    expected_dirty_stats: (0, 1, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_DWWRWD_A"),
                    expected_token: Some("SEQ_PERM_T_DWWRWD_A"),
                    absent_token: None,
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_DWWRWD_B"),
                    expected_token: Some("SEQ_PERM_T_DWWRWD_B"),
                    absent_token: Some("SEQ_PERM_T_DWWRWD_A"),
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::RestoreBase,
                    expected_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_write_write_restore_write_delete",
                    ),
                    absent_token: Some("SEQ_PERM_T_DWWRWD_B"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_T_DWWRWD_C"),
                    expected_token: Some("SEQ_PERM_T_DWWRWD_C"),
                    absent_token: Some(
                        "SEQ_PERM_TRACKED_BASE_delete_write_write_restore_write_delete",
                    ),
                    expected_dirty_stats: (1, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_T_DWWRWD_C"),
                    expected_dirty_stats: (0, 1, 0),
                },
            ],
        ),
    ];

    for (name, steps) in cases {
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = fs::canonicalize(temp.path())
            .expect("temp dir should canonicalize")
            .join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let path = repo_path.join(format!("tracked_{name}.rs"));
        let base_token = Box::leak(format!("SEQ_PERM_TRACKED_BASE_{name}").into_boxed_str());
        write_sequence_token(&path, base_token);

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);
        wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
        });

        run_common_sequence_steps(&service, &repo_id, &path, base_token, steps);
    }
}

#[test]
fn watcher_reconciles_common_untracked_operation_permutations() {
    let _serial = daemon_test_serial();
    let cases: &[(&str, &[CommonSequenceStep])] = &[
        (
            "add_modify_delete_add_modify",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_AMDAM_A"),
                    expected_token: Some("SEQ_PERM_U_AMDAM_A"),
                    absent_token: None,
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_AMDAM_B"),
                    expected_token: Some("SEQ_PERM_U_AMDAM_B"),
                    absent_token: Some("SEQ_PERM_U_AMDAM_A"),
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_AMDAM_B"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_AMDAM_C"),
                    expected_token: Some("SEQ_PERM_U_AMDAM_C"),
                    absent_token: Some("SEQ_PERM_U_AMDAM_B"),
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_AMDAM_D"),
                    expected_token: Some("SEQ_PERM_U_AMDAM_D"),
                    absent_token: Some("SEQ_PERM_U_AMDAM_C"),
                    expected_dirty_stats: (0, 0, 1),
                },
            ],
        ),
        (
            "delete_add_delete_add",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_DADA_A"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_DADA_A"),
                    expected_token: Some("SEQ_PERM_U_DADA_A"),
                    absent_token: None,
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_DADA_A"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_DADA_B"),
                    expected_token: Some("SEQ_PERM_U_DADA_B"),
                    absent_token: Some("SEQ_PERM_U_DADA_A"),
                    expected_dirty_stats: (0, 0, 1),
                },
            ],
        ),
        (
            "add_delete_delete_add_delete",
            &[
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_ADDAD_A"),
                    expected_token: Some("SEQ_PERM_U_ADDAD_A"),
                    absent_token: None,
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_ADDAD_A"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_ADDAD_A"),
                    expected_dirty_stats: (0, 0, 0),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Write("SEQ_PERM_U_ADDAD_B"),
                    expected_token: Some("SEQ_PERM_U_ADDAD_B"),
                    absent_token: Some("SEQ_PERM_U_ADDAD_A"),
                    expected_dirty_stats: (0, 0, 1),
                },
                CommonSequenceStep {
                    action: CommonSequenceAction::Remove,
                    expected_token: None,
                    absent_token: Some("SEQ_PERM_U_ADDAD_B"),
                    expected_dirty_stats: (0, 0, 0),
                },
            ],
        ),
    ];

    for (name, steps) in cases {
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = fs::canonicalize(temp.path())
            .expect("temp dir should canonicalize")
            .join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const VALUE: &str = \"SEQ_PERM_UNTRACKED_REPO_BASE\";\n",
        )
        .expect("tracked write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);
        wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
        });

        let path = repo_path.join("scratch").join(format!("{name}.rs"));
        run_common_sequence_steps(
            &service,
            &repo_id,
            &path,
            "SEQ_PERM_UNTRACKED_UNUSED_BASE",
            steps,
        );
    }
}

#[test]
fn watcher_reconciles_repeated_tracked_file_operation_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    write_sequence_token(&tracked, "SEQ_TRACKED_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });
    let status = wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_BASE", 1);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    remove_sequence_file(&tracked);
    let status = wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_BASE", 0);
    assert_exact_dirty_stats(&status, 0, 1, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_DELETE_ADD_1");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_DELETE_ADD_1", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_BASE", 0);
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_DELETE_ADD_2");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_DELETE_ADD_1", 0);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_DELETE_ADD_2", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);

    remove_sequence_file(&tracked);
    remove_sequence_file(&tracked);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_DELETE_ADD_2", 0);
    assert_exact_dirty_stats(&status, 0, 1, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_DELETE_DELETE_WRITE_1");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &tracked,
        "SEQ_TRACKED_DELETE_DELETE_WRITE_1",
        1,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_DELETE_DELETE_WRITE_2");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &tracked,
        "SEQ_TRACKED_DELETE_DELETE_WRITE_1",
        0,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &tracked,
        "SEQ_TRACKED_DELETE_DELETE_WRITE_2",
        1,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_BASE");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &tracked,
        "SEQ_TRACKED_DELETE_DELETE_WRITE_2",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_BASE", 1);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_AFTER_CLEAN_1");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_AFTER_CLEAN_1", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&tracked, "SEQ_TRACKED_AFTER_CLEAN_2");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_AFTER_CLEAN_1", 0);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_TRACKED_AFTER_CLEAN_2", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);
}

#[test]
fn watcher_reconciles_repeated_untracked_file_operation_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"SEQ_UNTRACKED_BASE\";\n",
    )
    .expect("tracked write should succeed");
    let untracked = repo_path.join("nested").join("scratch.rs");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    remove_sequence_file(&untracked);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_1", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    write_sequence_token(&untracked, "SEQ_UNTRACKED_ADD_1");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_1", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);

    write_sequence_token(&untracked, "SEQ_UNTRACKED_ADD_2");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_1", 0);
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_2", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);

    remove_sequence_file(&untracked);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_2", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    remove_sequence_file(&untracked);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &untracked, "SEQ_UNTRACKED_ADD_2", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    write_sequence_token(&untracked, "SEQ_UNTRACKED_DELETE_ADD_1");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_ADD_1",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);

    write_sequence_token(&untracked, "SEQ_UNTRACKED_DELETE_ADD_2");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_ADD_1",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_ADD_2",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);

    remove_sequence_file(&untracked);
    write_sequence_token(&untracked, "SEQ_UNTRACKED_DELETE_WRITE_WRITE_1");
    write_sequence_token(&untracked, "SEQ_UNTRACKED_DELETE_WRITE_WRITE_2");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_ADD_2",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_WRITE_WRITE_1",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_UNTRACKED_DELETE_WRITE_WRITE_2",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 0, 1);
}

#[test]
fn watcher_reconciles_mixed_directory_operation_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    let src = repo_path.join("src");
    fs::create_dir_all(&src).expect("src dir should succeed");
    let first = src.join("first.rs");
    let second = src.join("second.rs");
    let third = src.join("third.rs");
    let fourth = src.join("fourth.rs");
    write_sequence_token(&first, "SEQ_DIR_FIRST_BASE");
    write_sequence_token(&second, "SEQ_DIR_SECOND_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    remove_sequence_file(&first);
    write_sequence_token(&second, "SEQ_DIR_SECOND_MOD_1");
    write_sequence_token(&third, "SEQ_DIR_THIRD_NEW_1");
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_FIRST_BASE", 0);
    assert_exact_dirty_stats(&status, 1, 1, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_SECOND_MOD_1", 1);
    assert_exact_dirty_stats(&status, 1, 1, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_THIRD_NEW_1", 1);
    assert_exact_dirty_stats(&status, 1, 1, 1);

    write_sequence_token(&first, "SEQ_DIR_FIRST_BASE");
    write_sequence_token(&second, "SEQ_DIR_SECOND_MOD_2");
    remove_sequence_file(&third);
    write_sequence_token(&fourth, "SEQ_DIR_FOURTH_NEW_1");
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_FIRST_BASE", 1);
    assert_exact_dirty_stats(&status, 1, 0, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_SECOND_MOD_1", 0);
    assert_exact_dirty_stats(&status, 1, 0, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_SECOND_MOD_2", 1);
    assert_exact_dirty_stats(&status, 1, 0, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_THIRD_NEW_1", 0);
    assert_exact_dirty_stats(&status, 1, 0, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_FOURTH_NEW_1", 1);
    assert_exact_dirty_stats(&status, 1, 0, 1);

    write_sequence_token(&second, "SEQ_DIR_SECOND_BASE");
    remove_sequence_file(&fourth);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_SECOND_MOD_2", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_SECOND_BASE", 1);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_DIR_FOURTH_NEW_1", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn watcher_reconciles_tracked_move_and_recreate_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let original = repo_path.join("original.rs");
    let moved = repo_path.join("moved.rs");
    let moved_again = repo_path.join("moved_again.rs");
    write_sequence_token(&original, "SEQ_MOVE_ORIGINAL_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    fs::rename(&original, &moved).expect("rename should succeed");
    write_sequence_token(&moved, "SEQ_MOVE_NEW_1");
    let move_roots = vec![original.clone(), moved.clone()];
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &move_roots,
        "SEQ_MOVE_ORIGINAL_BASE",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 1, 1);
    let status =
        wait_for_scoped_token_count_for_roots(&service, &repo_id, &move_roots, "SEQ_MOVE_NEW_1", 1);
    assert_exact_dirty_stats(&status, 0, 1, 1);

    write_sequence_token(&moved, "SEQ_MOVE_NEW_2");
    let status =
        wait_for_scoped_token_count_for_roots(&service, &repo_id, &move_roots, "SEQ_MOVE_NEW_1", 0);
    assert_exact_dirty_stats(&status, 0, 1, 1);
    let status =
        wait_for_scoped_token_count_for_roots(&service, &repo_id, &move_roots, "SEQ_MOVE_NEW_2", 1);
    assert_exact_dirty_stats(&status, 0, 1, 1);

    remove_sequence_file(&moved);
    write_sequence_token(&moved_again, "SEQ_MOVE_NEW_3");
    let move_again_roots = vec![original.clone(), moved.clone(), moved_again.clone()];
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &move_again_roots,
        "SEQ_MOVE_NEW_2",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 1, 1);
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &move_again_roots,
        "SEQ_MOVE_NEW_3",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 1, 1);

    remove_sequence_file(&moved_again);
    write_sequence_token(&original, "SEQ_MOVE_ORIGINAL_BASE");
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &move_again_roots,
        "SEQ_MOVE_NEW_3",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &move_again_roots,
        "SEQ_MOVE_ORIGINAL_BASE",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn watcher_reconciles_untracked_directory_delete_and_recreate_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"SEQ_UNTRACKED_DIR_BASE\";\n",
    )
    .expect("tracked write should succeed");
    let generated = repo_path.join("generated");
    let first = generated.join("first.rs");
    let second = generated.join("second.rs");
    let third = generated.join("third.rs");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    write_sequence_token(&first, "SEQ_UNTRACKED_DIR_FIRST");
    write_sequence_token(&second, "SEQ_UNTRACKED_DIR_SECOND");
    wait_for_dirty_stats(&service, &repo_id, 0, 0, 2);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &generated, "SEQ_UNTRACKED_DIR_FIRST", 1);
    assert_exact_dirty_stats(&status, 0, 0, 2);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &generated,
        "SEQ_UNTRACKED_DIR_SECOND",
        1,
    );
    assert_exact_dirty_stats(&status, 0, 0, 2);

    fs::remove_dir_all(&generated).expect("generated dir delete should succeed");
    wait_for_dirty_stats(&service, &repo_id, 0, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &generated, "SEQ_UNTRACKED_DIR_FIRST", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &generated,
        "SEQ_UNTRACKED_DIR_SECOND",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);

    write_sequence_token(&third, "SEQ_UNTRACKED_DIR_THIRD");
    wait_for_dirty_stats(&service, &repo_id, 0, 0, 1);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &generated, "SEQ_UNTRACKED_DIR_THIRD", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);
}

#[test]
fn refresh_reconciles_ignore_rule_flip_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let gitignore = repo_path.join(".gitignore");
    let ignored = repo_path.join("scratch.tmp");
    fs::write(&gitignore, "*.tmp\n").expect("gitignore write should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"SEQ_IGNORE_BASE\";\n",
    )
    .expect("tracked write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    write_sequence_token(&ignored, "SEQ_IGNORE_VISIBLE_ALPHA");
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_IGNORE_VISIBLE_ALPHA", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    fs::write(&gitignore, "").expect("gitignore rewrite should succeed");
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_IGNORE_VISIBLE_ALPHA", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);

    write_sequence_token(&ignored, "SEQ_IGNORE_VISIBLE_BETA");
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_IGNORE_VISIBLE_ALPHA", 0);
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_IGNORE_VISIBLE_BETA", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);

    fs::write(&gitignore, "*.tmp\n").expect("gitignore restore should succeed");
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_IGNORE_VISIBLE_BETA", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn watcher_keeps_prefix_like_dirty_paths_distinct() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let src = repo_path.join("src");
    let src2 = repo_path.join("src2");
    fs::create_dir_all(&src).expect("src dir should succeed");
    fs::create_dir_all(&src2).expect("src2 dir should succeed");
    let scoped = src.join("file.rs");
    let unrelated = src2.join("file.rs");
    write_sequence_token(&scoped, "SEQ_PREFIX_SCOPED_BASE");
    write_sequence_token(&unrelated, "SEQ_PREFIX_UNRELATED_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    write_sequence_token(&scoped, "SEQ_PREFIX_SCOPED_DIRTY");
    write_sequence_token(&unrelated, "SEQ_PREFIX_UNRELATED_DIRTY");
    wait_for_dirty_stats(&service, &repo_id, 2, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &scoped, "SEQ_PREFIX_SCOPED_DIRTY", 1);
    assert_exact_dirty_stats(&status, 2, 0, 0);

    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &unrelated,
        "SEQ_PREFIX_UNRELATED_DIRTY",
        1,
    );
    assert_exact_dirty_stats(&status, 2, 0, 0);

    write_sequence_token(&scoped, "SEQ_PREFIX_SCOPED_BASE");
    wait_for_dirty_stats(&service, &repo_id, 1, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &src, "SEQ_PREFIX_SCOPED_DIRTY", 0);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &src2, "SEQ_PREFIX_UNRELATED_DIRTY", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);
}

#[test]
fn watcher_reconciles_file_directory_type_flip_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let flip = repo_path.join("flip.rs");
    write_sequence_token(&flip, "SEQ_TYPE_FLIP_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    remove_sequence_file(&flip);
    fs::create_dir_all(&flip).expect("directory replacement should succeed");
    write_sequence_token(&flip.join("child.rs"), "SEQ_TYPE_FLIP_CHILD");
    wait_for_dirty_stats(&service, &repo_id, 0, 1, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_BASE", 0);
    assert_exact_dirty_stats(&status, 0, 1, 1);
    let status = wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_CHILD", 1);
    assert_exact_dirty_stats(&status, 0, 1, 1);

    fs::remove_dir_all(&flip).expect("directory replacement remove should succeed");
    write_sequence_token(&flip, "SEQ_TYPE_FLIP_MODIFIED_FILE");
    wait_for_dirty_stats(&service, &repo_id, 1, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_CHILD", 0);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_MODIFIED_FILE", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&flip, "SEQ_TYPE_FLIP_BASE");
    wait_for_dirty_stats(&service, &repo_id, 0, 0, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_MODIFIED_FILE", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count(&service, &repo_id, &flip, "SEQ_TYPE_FLIP_BASE", 1);
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn watcher_reconciles_text_binary_text_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    let untracked = repo_path.join("scratch.rs");
    write_sequence_token(&tracked, "SEQ_BINARY_TRACKED_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    fs::write(&tracked, b"\0SEQ_BINARY_TRACKED_HIDDEN\n").expect("binary write should succeed");
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_BINARY_TRACKED_BASE", 0);
    assert_exact_dirty_stats(&status, 0, 1, 0);
    let status =
        wait_for_scoped_token_count(&service, &repo_id, &tracked, "SEQ_BINARY_TRACKED_HIDDEN", 0);
    assert_exact_dirty_stats(&status, 0, 1, 0);

    write_sequence_token(&tracked, "SEQ_BINARY_TRACKED_TEXT_AGAIN");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &tracked,
        "SEQ_BINARY_TRACKED_TEXT_AGAIN",
        1,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);

    fs::write(&untracked, b"\0SEQ_BINARY_UNTRACKED_HIDDEN\n").expect("binary write should succeed");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_BINARY_UNTRACKED_HIDDEN",
        0,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);

    write_sequence_token(&untracked, "SEQ_BINARY_UNTRACKED_TEXT");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_BINARY_UNTRACKED_TEXT",
        1,
    );
    assert_exact_dirty_stats(&status, 1, 0, 1);

    fs::write(&untracked, b"\0SEQ_BINARY_UNTRACKED_HIDDEN_AGAIN\n")
        .expect("binary write should succeed");
    let status = wait_for_scoped_token_count(
        &service,
        &repo_id,
        &untracked,
        "SEQ_BINARY_UNTRACKED_TEXT",
        0,
    );
    assert_exact_dirty_stats(&status, 1, 0, 0);
}

#[test]
fn refresh_reconciles_index_staged_operation_sequences() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    let staged_new = repo_path.join("staged_new.rs");
    write_sequence_token(&tracked, "SEQ_STAGE_BASE");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    write_sequence_token(&tracked, "SEQ_STAGE_STAGED_MOD");
    git_sequence(&repo_path, &["add", "tracked.rs"]);
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 1, 0, 0);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_STAGE_STAGED_MOD", 1);
    assert_exact_dirty_stats(&status, 1, 0, 0);

    git_sequence(&repo_path, &["reset", "-q", "--", "tracked.rs"]);
    write_sequence_token(&tracked, "SEQ_STAGE_BASE");
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_STAGE_STAGED_MOD", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);

    write_sequence_token(&staged_new, "SEQ_STAGE_NEW_FILE");
    git_sequence(&repo_path, &["add", "staged_new.rs"]);
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 1);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_STAGE_NEW_FILE", 1);
    assert_exact_dirty_stats(&status, 0, 0, 1);

    git_sequence(&repo_path, &["rm", "-q", "--cached", "staged_new.rs"]);
    remove_sequence_file(&staged_new);
    let status = refresh_repo(&service, &repo_id, true);
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let (_, status) = wait_for_search_count(&service, &repo_id, "SEQ_STAGE_NEW_FILE", 0);
    assert_exact_dirty_stats(&status, 0, 0, 0);
}

#[test]
fn watcher_handles_many_paths_across_directories() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = fs::canonicalize(temp.path())
        .expect("temp dir should canonicalize")
        .join("repo");
    let src = repo_path.join("src");
    let generated = repo_path.join("generated");
    fs::create_dir_all(&src).expect("src dir should succeed");
    fs::create_dir_all(&generated).expect("generated dir should succeed");
    let tracked_paths = (0..6)
        .map(|index| src.join(format!("tracked_{index}.rs")))
        .collect::<Vec<_>>();
    for (index, path) in tracked_paths.iter().enumerate() {
        write_sequence_token(path, &format!("SEQ_MANY_TRACKED_BASE_{index}"));
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);
    wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });

    for (index, path) in tracked_paths.iter().enumerate().take(3) {
        write_sequence_token(path, &format!("SEQ_MANY_TRACKED_MOD_{index}"));
    }
    for (index, path) in tracked_paths.iter().enumerate().skip(3) {
        remove_sequence_file(path);
        let replacement = generated.join(format!("new_{index}.rs"));
        write_sequence_token(&replacement, &format!("SEQ_MANY_UNTRACKED_NEW_{index}"));
    }

    let roots = vec![src.clone(), generated.clone()];
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &roots,
        "SEQ_MANY_TRACKED_MOD_0",
        1,
    );
    assert_exact_dirty_stats(&status, 3, 3, 3);
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &roots,
        "SEQ_MANY_UNTRACKED_NEW_5",
        1,
    );
    assert_exact_dirty_stats(&status, 3, 3, 3);

    for (index, path) in tracked_paths.iter().enumerate().take(3) {
        write_sequence_token(path, &format!("SEQ_MANY_TRACKED_BASE_{index}"));
    }
    for (index, path) in tracked_paths.iter().enumerate().skip(3) {
        write_sequence_token(path, &format!("SEQ_MANY_TRACKED_BASE_{index}"));
        remove_sequence_file(&generated.join(format!("new_{index}.rs")));
    }

    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &roots,
        "SEQ_MANY_TRACKED_MOD_0",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);
    let status = wait_for_scoped_token_count_for_roots(
        &service,
        &repo_id,
        &roots,
        "SEQ_MANY_UNTRACKED_NEW_5",
        0,
    );
    assert_exact_dirty_stats(&status, 0, 0, 0);
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

    match search_repo(&service, &repo_id, "BASE") {
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

    let _ = match service
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
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.probe_healthy
    });
    assert!(status.probe_healthy);

    let phrase_query = QuerySpec {
        pattern: regex::escape(phrase),
        patterns: vec![phrase.into()],
        fixed_strings: true,
        search_mode: SearchModeConfig::LineMatches,
        ..query(phrase)
    };
    let (backend, results, _) =
        wait_for_search_with_params_count(&service, &repo_id, phrase_query, vec![header], 1);
    assert!(matches!(
        backend,
        SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
    ));
    assert!(!results.line_matches.is_empty());
    assert!(results
        .line_matches
        .iter()
        .any(|matched| matched.path.ends_with("tracked.h")));
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

    let (backend, _) = wait_for_search_count(&service, &repo_id, "FIRST_EDIT", 1);
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

    let _ = wait_for_search_count(&service, &repo_id, "FIRST_EDIT", 0);

    let (backend, _) = wait_for_search_count(&service, &repo_id, "SECOND_EDIT", 1);
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
    ensure_git_repo_with_head(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: None,
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    overlay_auto_checkpoint_max_uncommitted_ops: 1,
                    overlay_merge_min_delay_ms: 50,
                    overlay_merge_retry_delay_ms: 25,
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

    let _ = wait_for_search_count(&service, &repo_id, &latest, 1);

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

    let _ = wait_for_search_count(&service, &repo_id, "REPEAT_BASE", 0);
    for token in prior_tokens {
        let _ = wait_for_search_count(&service, &repo_id, &token, 0);
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
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    for _ in 0..3 {
        let (backend, _) =
            wait_for_search_count(&service, &repo_id, "REPEATED_DIRTY_TOKEN_20260410", 1);
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
    let _ = refresh_repo(&service, &repo_id, true);
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified >= 1
    });
    assert!(status.dirty_files.modified >= 1);

    let _ = wait_for_search_count(&service, &repo_id, "MIXED_EXISTING_TOKEN_20260410", 1);

    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("revert should succeed");
    refresh_repo(&service, &repo_id, true);
    wait_for_search_count(&service, &repo_id, "MIXED_EXISTING_TOKEN_20260410", 0);

    let nested = repo_path.join("nested");
    fs::create_dir_all(&nested).expect("nested dir should succeed");
    fs::write(
        nested.join("live.rs"),
        "const VALUE: &str = \"MIXED_NEWDIR_TOKEN_20260410\";\n",
    )
    .expect("nested write should succeed");
    refresh_repo(&service, &repo_id, true);

    let (_, status) = wait_for_search_count(&service, &repo_id, "MIXED_NEWDIR_TOKEN_20260410", 1);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    for _ in 0..3 {
        let (_, status) =
            wait_for_search_count(&service, &repo_id, "MIXED_NEWDIR_TOKEN_20260410", 1);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
    }
}

#[test]
fn workspace_search_does_not_run_dirty_diff_observation() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

    let observation_path = temp.path().join("dirty-diff.tsv");
    unsafe {
        std::env::set_var("FLASHGREP_OBSERVE_DIRTY_DIFF_FILE", &observation_path);
    }

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let (backend, _) = wait_for_search_count(&service, &repo_id, "BASE", 1);
    assert!(matches!(backend, SearchBackend::IndexedClean));
    assert!(!observation_path.exists());

    unsafe {
        std::env::remove_var("FLASHGREP_OBSERVE_DIRTY_DIFF_FILE");
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
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let query = QuerySpec {
        search_mode: SearchModeConfig::LineMatches,
        ..query("SCOPED_TOKEN")
    };
    let (backend, results, _) =
        wait_for_search_with_params_count(&service, &repo_id, query, vec![left.clone()], 1);
    assert!(matches!(
        backend,
        SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
    ));
    assert_eq!(results.line_matches.len(), 1);
    assert!(results.line_matches[0].path.ends_with("left.rs"));
    assert_eq!(results.line_matches[0].line_number, 1);
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

    match search_repo(&service, &repo_id, "EPHEMERAL_TOKEN") {
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

    let (backend, status) =
        wait_for_search_count(&service, &repo_id, "PROBE_DELETE_TOKEN_20260410", 1);
    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    fs::remove_file(&transient).expect("transient delete should succeed");
    fs::remove_dir(&nested).expect("nested dir delete should succeed");
    wait_for_workspace_overlay_match_count(&repo_path, "PROBE_DELETE_TOKEN_20260410", 0);

    let (backend, status) =
        wait_for_search_count(&service, &repo_id, "PROBE_DELETE_TOKEN_20260410", 0);
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
        match search_repo(&service, &repo_id, "PROBE_DELETE_TOKEN_20260410") {
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

    let (backend, status) =
        wait_for_search_count(&service, &repo_id, "PROBE_RESTORE_TOKEN_20260416", 1);
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
    let (backend, status) =
        wait_for_search_count(&service, &repo_id, "PROBE_RESTORE_TOKEN_20260416", 0);
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

    match search_repo(&service, &repo_id, "BASE") {
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
    let status = wait_for_glob_paths(&service, &repo_id, scope, std::slice::from_ref(&stable));
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

    let _ = wait_for_search_count(&service, &repo_id, "DIR_DELETE_ALPHA", 0);
    let _ = wait_for_search_count(&service, &repo_id, "DIR_DELETE_BETA", 0);

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

    let _ = wait_for_search_count(&service, &repo_id, "DIR_DELETE_ALPHA", 1);
    let _ = wait_for_search_count(&service, &repo_id, "DIR_DELETE_BETA", 1);
}
