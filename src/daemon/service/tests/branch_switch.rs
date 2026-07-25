use super::*;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

fn read_base_manifest(storage_root: &Path) -> crate::index_engine::manifest::Manifest {
    let current_manifest_name =
        fs::read_to_string(storage_root.join("base-snapshot").join("CURRENT_MANIFEST"))
            .expect("current manifest pointer should be readable");
    serde_json::from_slice(
        &fs::read(
            storage_root
                .join("base-snapshot")
                .join("manifests")
                .join(current_manifest_name.trim()),
        )
        .expect("current manifest should be readable"),
    )
    .expect("current manifest should decode")
}

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
            && Path::new(&status.storage_root) == storage_b
    });
    assert!(settled.probe_healthy);
    assert_eq!(settled.last_error, None);

    let (backend, status) = wait_for_search_count(&service, &repo_id, "STORAGE_SWITCH_TOKEN", 1);
    assert!(matches!(backend, SearchBackend::IndexedClean));
    assert_eq!(PathBuf::from(&status.storage_root), storage_b);
}

#[test]
fn daemon_restart_reuses_compatible_base_when_existing_snapshot_head_is_stale() {
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
    service.shutdown_all();
    let service = DaemonService::new();

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
            assert_eq!(indexed_docs, None, "ensure must reuse the compatible base");
            let snapshot_key = status
                .snapshot_key
                .expect("reused status should include a snapshot key");
            assert!(snapshot_key.contains(&base_head));
            assert!(!snapshot_key.contains(&updated_head));
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }
    let _ = wait_for_search_count(&service, &repo_id, "UPDATED", 1);
    let _ = wait_for_search_count(&service, &repo_id, "BASE", 0);
}

#[test]
fn incompatible_index_configuration_uses_the_initial_build_boundary() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"CONFIG\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");

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
            indexed_docs: Some(1),
            ..
        } => repo_id,
        other => panic!("unexpected response: {other:?}"),
    };
    service
        .handle_request_only(Request::CloseRepo {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.clone(),
            },
        })
        .expect("close should succeed");

    let changed_config = RepoConfig {
        tokenizer: crate::daemon::protocol::TokenizerModeConfig::Trigram,
        ..RepoConfig::default()
    };
    match service
        .handle_request_only(Request::EnsureRepo {
            params: EnsureRepoParams {
                repo_path,
                storage_root: Some(storage_root),
                config: changed_config,
                refresh: Default::default(),
            },
        })
        .expect("incompatible ensure should succeed")
    {
        Response::RepoEnsured {
            indexed_docs,
            status,
            ..
        } => {
            assert_eq!(indexed_docs, Some(1));
            assert!(!matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
            ));
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

#[test]
fn corrupted_unpublished_candidate_does_not_replace_the_searchable_parent() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"PARENT_STILL_SEARCHABLE\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path,
                storage_root: Some(storage_root.clone()),
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
    let parent = read_base_manifest(&storage_root).manifest_id;
    fs::write(
        storage_root
            .join("base-snapshot")
            .join("manifests")
            .join("manifest-corrupt-candidate.json"),
        b"{not valid json",
    )
    .expect("corrupt candidate write should succeed");

    let _ = assert_search_count_now(&service, &repo_id, "PARENT_STILL_SEARCHABLE", 1);
    assert_eq!(read_base_manifest(&storage_root).manifest_id, parent);
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
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.active_task_id.is_none()
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
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
fn branch_switch_advances_base_without_automatic_full_rebuild() {
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
                storage_root: Some(storage_root.clone()),
                config: RepoConfig::default(),
                refresh: crate::daemon::protocol::RefreshPolicyConfig {
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
    let feature_head = git_head_commit(&repo_path);
    git_checkout(&repo_path, "feature");

    let refreshed = refresh_repo(&service, &repo_id, true);
    assert!(matches!(
        refreshed.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));

    let dirty = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
            && status.dirty_files.modified >= 1
            && status.dirty_files.deleted >= 1
            && status.dirty_files.new >= 1
    });
    assert!(dirty.probe_healthy);
    let settled = wait_for_status(&service, &repo_id, |status| {
        status.active_task_id.is_none()
            && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.dirty_files.modified == 0
            && status.dirty_files.deleted == 0
            && status.dirty_files.new == 0
    });
    assert_eq!(settled.last_rebuild_unix_secs, Some(_initial_rebuild_at));
    let current_manifest = read_base_manifest(&storage_root);
    assert!(matches!(
        current_manifest.source,
        Some(crate::index_engine::manifest::ManifestSource::GitBase {
            head_commit,
            delta_depth: 1,
            ..
        }) if head_commit == feature_head
    ));

    let _ = assert_search_count_now(&service, &repo_id, "FEATURE_TOKEN", 1);
    let _ = assert_search_count_now(&service, &repo_id, "MAIN_TOKEN", 0);
}

#[test]
fn switching_back_to_cached_heads_reactivates_manifests_without_tokenizing() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"MAIN_CACHE\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "main");
    let main_branch = git_current_branch(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root.clone()),
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
    let main_manifest = read_base_manifest(&storage_root);

    git_checkout_new_branch(&repo_path, "feature-cache");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"FEATURE_CACHE\";\n",
    )
    .expect("feature write should succeed");
    git_commit_all(&repo_path, "feature");
    git_checkout(&repo_path, "feature-cache");
    let _ = refresh_repo(&service, &repo_id, true);
    let _ = wait_for_status(&service, &repo_id, |status| {
        status.active_task_id.is_none()
            && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
    });
    let feature_manifest = read_base_manifest(&storage_root);
    assert_ne!(feature_manifest.manifest_id, main_manifest.manifest_id);

    git_checkout(&repo_path, &main_branch);
    let _ = refresh_repo(&service, &repo_id, true);
    let _ = wait_for_status(&service, &repo_id, |status| {
        status.active_task_id.is_none()
            && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && read_base_manifest(&storage_root).manifest_id == main_manifest.manifest_id
    });
    let _ = assert_search_count_now(&service, &repo_id, "MAIN_CACHE", 1);

    git_checkout(&repo_path, "feature-cache");
    let _ = refresh_repo(&service, &repo_id, true);
    let _ = wait_for_status(&service, &repo_id, |status| {
        status.active_task_id.is_none()
            && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && read_base_manifest(&storage_root).manifest_id == feature_manifest.manifest_id
    });
    let _ = assert_search_count_now(&service, &repo_id, "FEATURE_CACHE", 1);
}

#[test]
fn branch_advance_has_no_special_behavior_at_the_old_256_file_threshold() {
    let _serial = daemon_test_serial();
    for changed_count in [1_usize, 256, 257] {
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        let storage_root = temp.path().join("storage");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        for index in 0..257 {
            fs::write(
                repo_path.join(format!("file-{index:03}.rs")),
                format!("const VALUE_{index}: &str = \"BASE_{index}\";\n"),
            )
            .expect("tracked write should succeed");
        }
        init_git_repo(&repo_path);
        git_commit_all(&repo_path, "base");

        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: Some(storage_root.clone()),
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
        let built = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.active_task_id.is_none()
        });

        for index in 0..changed_count {
            fs::write(
                repo_path.join(format!("file-{index:03}.rs")),
                format!("const VALUE_{index}: &str = \"CHANGED_{changed_count}_{index}\";\n"),
            )
            .expect("tracked rewrite should succeed");
        }
        git_commit_all(&repo_path, "changed");
        let target_head = git_head_commit(&repo_path);
        let transition = refresh_repo(&service, &repo_id, true);
        assert!(!matches!(
            transition.phase,
            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
        ));
        let settled = wait_for_status(&service, &repo_id, |status| {
            status.base_head_commit.as_deref() == Some(target_head.as_str())
                && status.active_task_id.is_none()
                && matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
        });
        assert_eq!(settled.last_rebuild_unix_secs, built.last_rebuild_unix_secs);
        let manifest = read_base_manifest(&storage_root);
        let foundation = match manifest.source.as_ref() {
            Some(crate::index_engine::manifest::ManifestSource::GitBase {
                foundation_segment_ids,
                ..
            }) => foundation_segment_ids,
            other => panic!("unexpected manifest source: {other:?}"),
        };
        let delta_docs = manifest
            .active_segments
            .iter()
            .filter(|segment| !foundation.contains(&segment.id))
            .map(|segment| segment.doc_count as usize)
            .sum::<usize>();
        assert_eq!(delta_docs, changed_count);
    }
}

#[test]
fn uncommitted_changes_remain_overlay_only_during_base_advance() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"BASE_COMMITTED\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root.clone()),
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

    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"FEATURE_COMMITTED\";\n",
    )
    .expect("feature write should succeed");
    git_commit_all(&repo_path, "feature");
    let target_head = git_head_commit(&repo_path);
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"UNCOMMITTED_OVERLAY\";\n",
    )
    .expect("uncommitted rewrite should succeed");
    fs::write(
        repo_path.join("untracked.rs"),
        "const VALUE: &str = \"UNTRACKED_OVERLAY\";\n",
    )
    .expect("untracked write should succeed");

    let transition = refresh_repo(&service, &repo_id, true);
    assert!(!matches!(
        transition.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));
    let settled = wait_for_status(&service, &repo_id, |status| {
        status.base_head_commit.as_deref() == Some(target_head.as_str())
            && status.active_task_id.is_none()
            && status.dirty_files.modified == 1
            && status.dirty_files.new == 1
            && status.overlay_base_manifest_id == status.base_manifest_id
    });
    assert!(matches!(
        settled.phase,
        crate::daemon::protocol::RepoPhase::ReadyDirty
    ));
    let _ = assert_search_count_now(&service, &repo_id, "UNCOMMITTED_OVERLAY", 1);
    let _ = assert_search_count_now(&service, &repo_id, "UNTRACKED_OVERLAY", 1);
    let _ = assert_search_count_now(&service, &repo_id, "FEATURE_COMMITTED", 0);
    let manifest = read_base_manifest(&storage_root);
    assert!(matches!(
        manifest.source,
        Some(crate::index_engine::manifest::ManifestSource::GitBase {
            head_commit,
            document_count: 1,
            ..
        }) if head_commit == target_head
    ));
}

#[test]
fn rapid_head_changes_coalesce_to_the_latest_target() {
    let _serial = daemon_test_serial();
    let service = DaemonService::new();
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    let storage_root = temp.path().join("storage");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"H0_TOKEN\";\n",
    )
    .expect("tracked write should succeed");
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "h0");
    let main_branch = git_current_branch(&repo_path);

    let repo_id = match service
        .handle_request_only(Request::OpenRepo {
            params: OpenRepoParams {
                repo_path: repo_path.clone(),
                storage_root: Some(storage_root.clone()),
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
    let built = wait_for_status(&service, &repo_id, |status| {
        matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
            && status.active_task_id.is_none()
    });

    git_checkout_new_branch(&repo_path, "h2");
    fs::write(
        repo_path.join("tracked.rs"),
        "const VALUE: &str = \"H2_TOKEN\";\n",
    )
    .expect("h2 write should succeed");
    git_commit_all(&repo_path, "h2");
    let h2 = git_head_commit(&repo_path);

    git_checkout(&repo_path, &main_branch);
    git_checkout_new_branch(&repo_path, "h1");
    for index in 0..2_000 {
        fs::write(
            repo_path.join(format!("h1-{index:04}.rs")),
            format!("const H1_{index}: &str = \"H1_{index}\";\n"),
        )
        .expect("h1 write should succeed");
    }
    git_commit_all(&repo_path, "h1");
    let h1 = git_head_commit(&repo_path);
    let _ = refresh_repo(&service, &repo_id, true);

    git_checkout(&repo_path, "h2");
    let transition = refresh_repo(&service, &repo_id, true);
    assert!(!matches!(
        transition.phase,
        crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
    ));
    let settled = wait_for_status(&service, &repo_id, |status| {
        status.base_head_commit.as_deref() == Some(h2.as_str())
            && status.workspace_head_commit.as_deref() == Some(h2.as_str())
            && status.overlay_head_commit.as_deref() == Some(h2.as_str())
            && status.active_task_id.is_none()
    });
    assert_eq!(settled.last_rebuild_unix_secs, built.last_rebuild_unix_secs);
    let _ = assert_search_count_now(&service, &repo_id, "H2_TOKEN", 1);
    let _ = assert_search_count_now(&service, &repo_id, "H0_TOKEN", 0);
    let cache = fs::read_to_string(
        storage_root
            .join("base-snapshot")
            .join("HEAD_MANIFESTS.json"),
    )
    .expect("HEAD cache should be readable");
    assert!(cache.contains(&h1), "H1 candidate should remain cacheable");
}

#[test]
fn searches_remain_available_while_base_manifest_activates() {
    let _serial = daemon_test_serial();
    let service = Arc::new(DaemonService::new());
    let temp = tempdir().expect("temp dir should succeed");
    let repo_path = temp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("repo dir should succeed");
    fs::write(
        repo_path.join("common.rs"),
        "const VALUE: &str = \"ALWAYS_SEARCHABLE\";\n",
    )
    .expect("common write should succeed");
    for index in 0..300 {
        fs::write(
            repo_path.join(format!("changed-{index:04}.rs")),
            format!("const VALUE_{index}: &str = \"OLD_{index}\";\n"),
        )
        .expect("base write should succeed");
    }
    init_git_repo(&repo_path);
    git_commit_all(&repo_path, "base");
    let (repo_id, _) = open_repo(&service, repo_path.clone());
    build_repo(&service, &repo_id);

    for index in 0..300 {
        fs::write(
            repo_path.join(format!("changed-{index:04}.rs")),
            format!("const VALUE_{index}: &str = \"NEW_{index}\";\n"),
        )
        .expect("target write should succeed");
    }
    git_commit_all(&repo_path, "target");
    let target_head = git_head_commit(&repo_path);

    let stop = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicUsize::new(0));
    let mut searchers = Vec::new();
    for _ in 0..2 {
        let service = Arc::clone(&service);
        let repo_id = repo_id.clone();
        let stop = Arc::clone(&stop);
        let completed = Arc::clone(&completed);
        searchers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match search_repo(&service, &repo_id, "ALWAYS_SEARCHABLE") {
                    Response::SearchCompleted {
                        results, status, ..
                    } => {
                        assert_eq!(results.matched_lines, 1);
                        assert!(!matches!(
                            status.phase,
                            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
                        ));
                        completed.fetch_add(1, Ordering::Relaxed);
                    }
                    other => panic!("unexpected response: {other:?}"),
                }
            }
        }));
    }

    let _ = refresh_repo(&service, &repo_id, true);
    let _ = wait_for_status(&service, &repo_id, |status| {
        status.base_head_commit.as_deref() == Some(target_head.as_str())
            && status.active_task_id.is_none()
            && status.overlay_base_manifest_id == status.base_manifest_id
    });
    stop.store(true, Ordering::Relaxed);
    for searcher in searchers {
        searcher.join().expect("search thread should succeed");
    }
    assert!(completed.load(Ordering::Relaxed) >= 2);
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
