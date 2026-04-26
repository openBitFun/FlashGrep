use super::*;

#[test]
fn workspace_probe_freshness_reports_missing_base_snapshot() {
    let repo = TestRepo::new();
    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let probe = workspace
        .probe_freshness()
        .expect("freshness probe should succeed");
    assert_eq!(probe.state, WorkspaceFreshnessState::MissingBaseSnapshot);
    assert!(probe.base_snapshot_key.is_none());
    assert_eq!(probe.dirty_file_count(), 0);
    assert!(probe.needs_base_snapshot());
}

#[test]
fn workspace_probe_freshness_reports_stale_after_new_file() {
    let repo = TestRepo::new();
    repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
    repo.build();
    repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let probe = workspace
        .probe_freshness()
        .expect("freshness probe should succeed");
    assert_eq!(probe.state, WorkspaceFreshnessState::Stale);
    assert_eq!(probe.new_files, 1);
    assert_eq!(probe.dirty_file_count(), 1);
}

#[test]
fn workspace_probe_freshness_if_due_reuses_cache_until_ttl_expires() {
    let repo = TestRepo::new();
    repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let first = workspace
        .probe_freshness_if_due(Duration::from_secs(60))
        .expect("freshness probe should succeed");
    assert_eq!(first.state, WorkspaceFreshnessState::Fresh);

    repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

    let cached = workspace
        .probe_freshness_if_due(Duration::from_secs(60))
        .expect("cached freshness probe should succeed");
    assert_eq!(cached.state, WorkspaceFreshnessState::Fresh);

    let refreshed = workspace
        .probe_freshness_if_due(Duration::ZERO)
        .expect("fresh freshness probe should succeed");
    assert_eq!(refreshed.state, WorkspaceFreshnessState::Stale);
    assert_eq!(refreshed.new_files, 1);
}

#[test]
fn ensure_base_snapshot_invalidates_cached_searcher_after_rebuild() {
    let repo = TestRepo::new();
    repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let initial_status = workspace.status().expect("status should succeed");
    let initial_dirty = initial_status.dirty_files.expect("dirty diff should exist");
    assert!(initial_dirty.modified_files.is_empty());
    assert!(initial_dirty.deleted_files.is_empty());
    assert!(initial_dirty.new_files.is_empty());

    repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");

    let stale_status = workspace.status().expect("status should succeed");
    let stale_dirty = stale_status.dirty_files.expect("dirty diff should exist");
    assert_eq!(stale_dirty.new_files.len(), 1);

    workspace
        .ensure_base_snapshot()
        .expect("rebuild should succeed");

    let refreshed_status = workspace.status().expect("status should succeed");
    let refreshed_dirty = refreshed_status
        .dirty_files
        .expect("dirty diff should exist");
    assert!(refreshed_dirty.modified_files.is_empty());
    assert!(refreshed_dirty.deleted_files.is_empty());
    assert!(refreshed_dirty.new_files.is_empty());
}

#[test]
fn reconcile_dirty_diff_keeps_only_paths_that_are_still_dirty() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
    let removed = repo.write("removed.rs", "const NAME: &str = \"REMOVED\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let added = repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");
    fs::remove_file(&removed).expect("remove should succeed");
    fs::remove_file(&added).expect("remove should succeed");

    let reconciled = workspace
        .reconcile_dirty_diff(&IndexWorktreeDiff {
            modified_files: vec![tracked.to_string_lossy().into_owned()],
            deleted_files: vec![removed.to_string_lossy().into_owned()],
            new_files: vec![added.to_string_lossy().into_owned()],
        })
        .expect("reconcile should succeed");

    assert!(reconciled.modified_files.is_empty());
    assert_eq!(
        reconciled.deleted_files,
        vec![removed.to_string_lossy().into_owned()]
    );
    assert!(reconciled.new_files.is_empty());
}
