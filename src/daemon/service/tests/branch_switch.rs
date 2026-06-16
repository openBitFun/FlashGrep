use super::*;

#[test]
fn ensure_repo_replaces_runtime_when_storage_root_changes() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"STORAGE_SWITCH_TOKEN\";\n",
    )
    .expect("tracked write should succeed");
    ensure_git_repo_with_head(&repo_path);

    let storage_a = temp.path().join("storage-a");
    let storage_b = temp.path().join("storage-b");

    let (repo_id, first_status) = match service
        .handle_request_only(Request::EnsureRepo {
            params: EnsureRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_a.clone()),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("first ensure should succeed")
    {
        Response::RepoEnsured {
            repo_id,
            status,
            indexed_docs,
        } => {
            assert!(
                indexed_docs.is_some(),
                "first ensure should build a snapshot"
            );
            (repo_id, status)
        }
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(PathBuf::from(&first_status.storage_root), storage_a);
    let _ = wait_for_search_count(&service, &repo_id, "STORAGE_SWITCH_TOKEN", 1);

    fs::remove_dir_all(&storage_a).expect("old storage root delete should succeed");

    match service
        .handle_request_only(Request::EnsureRepo {
            params: EnsureRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_b.clone()),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("second ensure should succeed")
    {
        Response::RepoEnsured {
            repo_id: second_repo_id,
            status,
            indexed_docs,
        } => {
            assert_eq!(second_repo_id, repo_id);
            assert_eq!(PathBuf::from(&status.storage_root), storage_b);
            assert!(
                indexed_docs.is_some(),
                "second ensure should rebuild into the new storage root"
            );
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let settled = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && PathBuf::from(&status.storage_root) == storage_b
    });
    assert!(settled.probe_healthy);
    assert_eq!(settled.last_error, None);

    let (backend, status) = wait_for_search_count(&service, &repo_id, "STORAGE_SWITCH_TOKEN", 1);
    assert!(matches!(backend, SearchBackend::IndexedClean));
    assert_eq!(PathBuf::from(&status.storage_root), storage_b);
}

#[test]
fn ensure_repo_rebuilds_when_existing_snapshot_head_is_stale() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"BASE\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");
    let base_head = git_head_commit(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::EnsureRepo {
            params: EnsureRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root.clone()),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("initial ensure should succeed")
    {
        Response::RepoEnsured {
            repo_id,
            status,
            indexed_docs,
        } => {
            assert!(indexed_docs.is_some());
            assert!(status
                .snapshot_key
                .as_deref()
                .is_some_and(|key| key.contains(&base_head)));
            repo_id
        }
        other => panic!("unexpected response: {other:?}"),
    };

    service
        .handle_request_only(Request::CloseRepo {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.clone(),
            },
        })
        .expect("close repo should succeed");

    fs::write(
        repo_path.join("tracked.rs"),
        "const NAME: &str = \"UPDATED\";\n",
    )
    .expect("tracked rewrite should succeed");
    git_commit_all(&repo_path, "updated");
    let updated_head = git_head_commit(&repo_path);
    assert_ne!(updated_head, base_head);

    match service
        .handle_request_only(Request::EnsureRepo {
            params: EnsureRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("ensure with stale base snapshot should succeed")
    {
        Response::RepoEnsured {
            status,
            indexed_docs,
            ..
        } => {
            assert!(
                indexed_docs.is_some(),
                "ensure should rebuild when the existing base snapshot HEAD is stale"
            );
            let snapshot_key = status
                .snapshot_key
                .expect("rebuilt status should include a snapshot key");
            assert!(snapshot_key.contains(&updated_head));
            assert!(!snapshot_key.contains(&base_head));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn git_branch_checkout_reconciles_large_worktree_switches() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let tracked = repo_path.join("tracked.rs");
    let old_dir = repo_path.join("old");
    fs::create_dir_all(&old_dir).expect("old dir should succeed");
    fs::write(&tracked, "const NAME: &str = \"MAIN_TOKEN\";\n")
        .expect("tracked write should succeed");
    fs::write(
        old_dir.join("legacy.rs"),
        "const NAME: &str = \"LEGACY_BRANCH_TOKEN\";\n",
    )
    .expect("legacy write should succeed");

    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");
    let main_branch = git_current_branch(&repo_path);

    let storage_root = temp.path().join("storage");
    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root),
                config: RepoConfig::default(),
                refresh: Default::default(),
            },
        })
        .expect("open repo should succeed")
    {
        Response::RepoOpened { repo_id, .. } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    build_repo(&service, &repo_id);
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert!(status.probe_healthy);
    let _ = wait_for_search_count(&service, &repo_id, "MAIN_TOKEN", 1);
    let _ = wait_for_search_count(&service, &repo_id, "LEGACY_BRANCH_TOKEN", 1);

    git_checkout_new_branch(&repo_path, "feature");
    fs::remove_dir_all(&old_dir).expect("old dir delete should succeed");
    fs::write(&tracked, "const NAME: &str = \"FEATURE_TOKEN\";\n")
        .expect("tracked rewrite should succeed");
    fs::create_dir_all(repo_path.join("feature")).expect("feature dir should succeed");
    fs::write(
        repo_path.join("feature/new.rs"),
        "const NAME: &str = \"FEATURE_DIR_TOKEN\";\n",
    )
    .expect("feature file write should succeed");
    git_commit_all(&repo_path, "feature");

    git_checkout(&repo_path, "feature");
    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let _ = wait_for_search_count(&service, &repo_id, "FEATURE_TOKEN", 1);
    let _ = wait_for_search_count(&service, &repo_id, "FEATURE_DIR_TOKEN", 1);
    let _ = wait_for_search_count(&service, &repo_id, "MAIN_TOKEN", 0);
    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified >= 1
            && status.dirty_files.deleted >= 1
            && status.dirty_files.new >= 1
    });
    assert!(status.probe_healthy);

    git_checkout(&repo_path, &main_branch);
    let status = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        status.phase,
        crate::daemon::protocol::RepoPhase::ReadyClean
            | crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let status = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert!(status.probe_healthy);

    let _ = wait_for_search_count(&service, &repo_id, "MAIN_TOKEN", 1);
    let _ = wait_for_search_count(&service, &repo_id, "FEATURE_TOKEN", 0);
    let _ = wait_for_search_count(&service, &repo_id, "FEATURE_DIR_TOKEN", 0);
}

#[test]
fn background_base_rebuild_after_branch_switch_keeps_search_available() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let tracked = repo_path.join("tracked.rs");
    let old_dir = repo_path.join("old");
    fs::create_dir_all(&old_dir).expect("old dir should succeed");
    fs::write(&tracked, "const NAME: &str = \"MAIN_TOKEN\";\n")
        .expect("tracked write should succeed");
    fs::write(
        old_dir.join("legacy.rs"),
        "const NAME: &str = \"LEGACY_BRANCH_TOKEN\";\n",
    )
    .expect("legacy write should succeed");

    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");

    let storage_root = temp.path().join("storage");
    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root),
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
                    rebuild_dirty_threshold: 1,
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
    let built = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.last_rebuild_unix_secs.is_some()
    });
    let _initial_rebuild_at = built
        .last_rebuild_unix_secs
        .expect("initial rebuild timestamp should exist");

    git_checkout_new_branch(&repo_path, "feature");
    fs::remove_dir_all(&old_dir).expect("old dir delete should succeed");
    fs::write(&tracked, "const NAME: &str = \"FEATURE_TOKEN\";\n")
        .expect("tracked rewrite should succeed");
    fs::create_dir_all(repo_path.join("feature")).expect("feature dir should succeed");
    fs::write(
        repo_path.join("feature/new.rs"),
        "const NAME: &str = \"FEATURE_DIR_TOKEN\";\n",
    )
    .expect("feature file write should succeed");
    git_commit_all(&repo_path, "feature");
    git_checkout(&repo_path, "feature");

    let refreshed = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        refreshed.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let dirty = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.rebuild_recommended
    });
    assert!(dirty.probe_healthy);

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_background_rebuild = false;
    let settled = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for background base rebuild"
        );
        let status = get_status(&service, &repo_id);
        if status.active_task_id.is_some() {
            saw_background_rebuild = true;
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyDirty
                    | crate::daemon::protocol::RepoPhase::ReadyClean
            ));

            match search_repo(&service, &repo_id, "FEATURE_TOKEN") {
                Response::SearchCompleted { results, .. } => {
                    assert_eq!(results.matched_lines, 1);
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }

        if status.active_task_id.is_none()
            && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && !status.rebuild_recommended
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
        {
            break status;
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(
        saw_background_rebuild,
        "expected auto background rebuild task"
    );
    assert!(settled.probe_healthy);
    assert!(!settled.rebuild_recommended);
    assert_exact_dirty_stats(&settled, 0, 0, 0);

    let _ = assert_search_count_now(&service, &repo_id, "FEATURE_TOKEN", 1);
    let _ = assert_search_count_now(&service, &repo_id, "MAIN_TOKEN", 0);
}

#[test]
fn atomic_rename_replace_updates_tracked_file_contents() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    let tracked = repo_path.join("tracked.rs");
    fs::write(&tracked, "const VALUE: &str = \"ATOMIC_BASE\";\n")
        .expect("tracked write should succeed");

    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    let temp_file = repo_path.join(".tracked.rs.tmp");
    fs::write(&temp_file, "const VALUE: &str = \"ATOMIC_REPLACED\";\n")
        .expect("temp write should succeed");
    fs::rename(&temp_file, &tracked).expect("atomic replace should succeed");

    let _ = wait_for_search_count(&service, &repo_id, "ATOMIC_REPLACED", 1);
    let _ = wait_for_search_count(&service, &repo_id, "ATOMIC_BASE", 0);
}
