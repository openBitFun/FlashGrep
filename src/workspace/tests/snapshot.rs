use super::*;

#[test]
fn ensure_base_snapshot_publishes_base_shard_metadata_used_by_base_snapshot_info() {
    let repo = TestRepo::new();
    repo.write("base.rs", "const NAME: &str = \"BASE\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let base = workspace
        .ensure_base_snapshot()
        .expect("base snapshot should build");
    let metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&repo.build_config())
        .expect("metadata should load")
        .expect("metadata should exist");
    assert_eq!(metadata.snapshot_key, base.snapshot_key);
    assert_eq!(metadata.doc_count, Some(1));
    assert_eq!(metadata.tokenizer, Some(repo.build_config().tokenizer));
    assert!(metadata.config_fingerprint.is_some());
    assert!(
        !repo.index.join("CURRENT").exists(),
        "workspace base snapshot should not depend on legacy CURRENT"
    );

    let metadata_backed = workspace
        .base_snapshot_info()
        .expect("base snapshot info should use base shard metadata");
    assert_eq!(metadata_backed.snapshot_key, base.snapshot_key);
    assert_eq!(metadata_backed.doc_count, 1);
    assert_eq!(metadata_backed.tokenizer, repo.build_config().tokenizer);
}

#[test]
fn ensure_base_snapshot_publishes_compact_base_manifest() {
    let repo = TestRepo::new();
    repo.write("src/a.rs", "const A: &str = \"AAAA\";\n");
    repo.write("src/b.rs", "const B: &str = \"BBBB\";\n");
    repo.write("src/c.rs", "const C: &str = \"CCCC\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");
    workspace
        .ensure_base_snapshot()
        .expect("base snapshot should build");

    let snapshot = WorkspaceBaseSnapshotIndex::open_read_only(&repo.build_config())
        .expect("base snapshot should open")
        .open_committed()
        .expect("committed base snapshot should open");
    let metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&repo.build_config())
        .expect("metadata should load")
        .expect("metadata should exist");
    assert!(!metadata.shards.is_empty());
    assert_eq!(
        snapshot.manifest().active_segments.len(),
        metadata.shards.len()
    );

    let manifest_count = fs::read_dir(
        WorkspaceBaseSnapshotIndex::index_root(&repo.build_config()).join("manifests"),
    )
    .expect("manifests dir should exist")
    .filter_map(std::result::Result::ok)
    .count();
    assert_eq!(manifest_count, 1);
}

#[test]
fn workspace_search_and_snapshot_use_base_shard_without_legacy_index() {
    let repo = TestRepo::new();
    let tracked = repo.write("base.rs", "const NAME: &str = \"BASE\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");
    workspace
        .ensure_base_snapshot()
        .expect("base snapshot should build");

    assert!(
        !repo.index.join("CURRENT").exists(),
        "workspace search should not require legacy CURRENT"
    );

    let snapshot_search = workspace
        .search_snapshot_with_filter(&query("BASE"), None, false)
        .expect("snapshot search should succeed");
    assert_eq!(snapshot_search.backend, SearchBackend::Index);
    assert_eq!(snapshot_search.results.matched_lines, 1);

    let snapshot = workspace.snapshot().expect("snapshot should succeed");
    repo.write("base.rs", "const NAME: &str = \"DIRTY\";\n");

    let live_results = workspace
        .search(&query("DIRTY"))
        .expect("workspace search should see dirty contents");
    assert_eq!(live_results.matched_lines, 1);

    let pinned_results = snapshot
        .search(&query("BASE"))
        .expect("snapshot search should succeed");
    assert_eq!(pinned_results.matched_lines, 1);

    let stale_snapshot = workspace
        .ensure_base_snapshot()
        .expect("rebuild should succeed");
    assert_eq!(
        stale_snapshot.snapshot_key,
        workspace
            .base_snapshot_info()
            .expect("base snapshot info should succeed")
            .snapshot_key
    );
    let error = snapshot
        .search(&query("BASE"))
        .expect_err("stale snapshot should fail once base snapshot changes");
    assert!(matches!(error, crate::AppError::Protocol(_)));
    assert!(tracked.exists());
}

#[test]
fn workspace_exposes_git_snapshot_identity() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.build_config(),
    })
    .expect("workspace should open");

    let base = workspace
        .ensure_base_snapshot()
        .expect("base snapshot should build");
    assert_eq!(base.snapshot_kind, BaseSnapshotKind::GitCommit);
    assert!(base.snapshot_key.starts_with("base-git-"));
    assert!(base.head_commit.is_some());
    assert!(base.config_fingerprint.is_some());
}
