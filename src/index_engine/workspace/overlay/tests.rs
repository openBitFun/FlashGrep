use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use tempfile::TempDir;

use super::{
    WorkspaceOverlayIndex, WorkspaceOverlayIngestBatch, WorkspaceOverlayPublishPolicy,
    WorkspaceOverlaySnapshotState, DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
};
use crate::{
    config::{BuildConfig, CorpusMode, TokenizerMode},
    index_engine::{
        config::{EngineConfig, MergePolicyConfig, PostingTokenizerConfig},
        query::{QueryFilters, QueryRequest},
        writer::IndexEngine,
        RepoId, ShardId,
    },
    search::SearchMode,
    search_engine::SearchDocument,
};

fn build_config(repo: &Path) -> BuildConfig {
    BuildConfig {
        repo_path: repo.to_path_buf(),
        storage_root: repo.join(".codgrep-index-engine"),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::NoIgnore,
        include_hidden: true,
        max_file_size: 1 << 20,
        min_sparse_len: 3,
        max_sparse_len: 8,
    }
}

fn query(pattern: &str) -> QueryRequest {
    QueryRequest {
        pattern: pattern.into(),
        mode: SearchMode::CountOnly,
        case_insensitive: false,
        top_k: None,
        filters: QueryFilters::default(),
        prefer_worktree_for_git_head_blob: false,
    }
}

fn wait_for_delete_compaction(overlay: &WorkspaceOverlayIndex) {
    for _ in 0..100 {
        let _ = overlay
            .run_requested_merge()
            .expect("overlay merge should succeed");
        let manifest = overlay
            .engine
            .admin()
            .manifest()
            .expect("manifest should load");
        if manifest.active_delete_segments.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for overlay delete compaction");
}

fn loaded_doc(path: &Path, text: &str) -> SearchDocument {
    SearchDocument::from_loaded_bytes(
        path.to_string_lossy().into_owned(),
        text.len() as u64,
        1,
        Arc::<[u8]>::from(text.as_bytes().to_vec()),
    )
}

#[test]
fn ingest_batch_updates_overlay_snapshot() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay_root = WorkspaceOverlayIndex::index_root(&build_config(&repo));
    let overlay = WorkspaceOverlayIndex::new(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-test".into()),
            index_root: overlay_root,
            ..EngineConfig::default()
        })
        .expect("open engine"),
    );

    let tracked_path = repo.join("tracked.rs");
    let added_path = repo.join("added.rs");
    let mut documents = HashMap::new();
    documents.insert(
        tracked_path.to_string_lossy().into_owned(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"BASE\";\n"),
    );

    let snapshot =
        WorkspaceOverlaySnapshotState::from_paths(&repo_id, &repo, &documents, &HashSet::new())
            .expect("build snapshot");
    overlay
        .rebuild_snapshot(&snapshot)
        .expect("rebuild snapshot should succeed");

    let before = overlay
        .open_snapshot()
        .expect("open snapshot")
        .search(&query("BASE"))
        .expect("search should succeed");
    assert_eq!(before.matched_lines, 1);

    documents.insert(
        tracked_path.to_string_lossy().into_owned(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"UPDATED\";\n"),
    );
    documents.insert(
        added_path.to_string_lossy().into_owned(),
        loaded_doc(&added_path, "const VALUE: &str = \"ADDED\";\n"),
    );

    let batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::from([tracked_path.to_string_lossy().into_owned()]),
        &HashSet::from([added_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build ingest batch");
    overlay
        .ingest_batch(&batch)
        .expect("ingest batch should succeed");

    let snapshot = overlay.open_snapshot().expect("open snapshot");
    assert_eq!(
        snapshot
            .search(&query("BASE"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
    assert_eq!(
        snapshot
            .search(&query("UPDATED"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
    assert_eq!(
        snapshot
            .search(&query("ADDED"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn reopen_drops_uncommitted_overlay_snapshot_without_checkpoint() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay_root = WorkspaceOverlayIndex::index_root(&build_config(&repo));

    {
        let overlay = WorkspaceOverlayIndex::new(
            IndexEngine::open(EngineConfig {
                shard_id: ShardId("overlay-reopen-test".into()),
                index_root: overlay_root.clone(),
                ..EngineConfig::default()
            })
            .expect("open engine"),
        );

        let tracked_path = repo.join("tracked.rs");
        let mut documents = HashMap::new();
        documents.insert(
            tracked_path.to_string_lossy().into_owned(),
            loaded_doc(&tracked_path, "const VALUE: &str = \"REOPEN\";\n"),
        );

        let snapshot =
            WorkspaceOverlaySnapshotState::from_paths(&repo_id, &repo, &documents, &HashSet::new())
                .expect("build snapshot");
        overlay
            .rebuild_snapshot(&snapshot)
            .expect("rebuild snapshot should succeed");
    }

    let reopened = WorkspaceOverlayIndex::new(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-reopen-test".into()),
            index_root: overlay_root,
            ..EngineConfig::default()
        })
        .expect("reopen engine"),
    );

    let results = reopened
        .open_snapshot()
        .expect("open snapshot")
        .search(&query("REOPEN"))
        .expect("search should succeed");
    assert_eq!(results.matched_lines, 0);
}

#[test]
fn checkpoint_publish_policy_updates_committed_snapshot() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-checkpoint-test".into()),
            index_root: WorkspaceOverlayIndex::index_root(&build_config(&repo)),
            ..EngineConfig::default()
        })
        .expect("open engine"),
        WorkspaceOverlayPublishPolicy::Checkpoint,
    );

    let tracked_path = repo.join("tracked.rs");
    let mut documents = HashMap::new();
    documents.insert(
        tracked_path.to_string_lossy().into_owned(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"COMMITTED\";\n"),
    );

    let batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([tracked_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build ingest batch");
    overlay
        .ingest_batch(&batch)
        .expect("ingest batch should succeed");

    let committed = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot");
    let results = committed
        .search(&query("COMMITTED"))
        .expect("search should succeed");
    assert_eq!(results.matched_lines, 1);
}

#[test]
fn auto_checkpoint_publish_policy_commits_after_threshold() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-auto-checkpoint-test".into()),
            index_root: WorkspaceOverlayIndex::index_root(&build_config(&repo)),
            ..EngineConfig::default()
        })
        .expect("open engine"),
        WorkspaceOverlayPublishPolicy::AutoCheckpoint {
            max_uncommitted_ops: 2,
        },
    );

    let first_path = repo.join("first.rs");
    let second_path = repo.join("second.rs");

    let mut documents = HashMap::new();
    documents.insert(
        first_path.to_string_lossy().into_owned(),
        loaded_doc(&first_path, "const VALUE: &str = \"FIRST\";\n"),
    );
    let first_batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([first_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build first batch");
    overlay
        .ingest_batch(&first_batch)
        .expect("ingest first batch");

    let committed_before = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot")
        .search(&query("FIRST"))
        .expect("search should succeed");
    assert_eq!(committed_before.matched_lines, 0);

    documents.insert(
        second_path.to_string_lossy().into_owned(),
        loaded_doc(&second_path, "const VALUE: &str = \"SECOND\";\n"),
    );
    let second_batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([second_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build second batch");
    overlay
        .ingest_batch(&second_batch)
        .expect("ingest second batch");

    let committed_after = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot");
    assert_eq!(
        committed_after
            .search(&query("FIRST"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
    assert_eq!(
        committed_after
            .search(&query("SECOND"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn open_defaults_to_auto_checkpoint_publish_policy() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");

    let overlay = WorkspaceOverlayIndex::open(&build_config(&repo)).expect("open overlay index");

    assert_eq!(
        overlay.publish_policy,
        WorkspaceOverlayPublishPolicy::AutoCheckpoint {
            max_uncommitted_ops: DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
        }
    );
}

#[test]
fn open_migrates_legacy_overlay_storage_into_workspace_overlay_dir() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let build_config = build_config(&repo);
    let legacy_overlay_root = build_config.storage_root.clone();
    let workspace_overlay_root = WorkspaceOverlayIndex::index_root(&build_config);
    let repo_id = RepoId("repo".into());
    let tracked_path = repo.join("tracked.rs");
    let tracked = tracked_path.to_string_lossy().into_owned();

    let legacy_overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-legacy-layout-test".into()),
            index_root: legacy_overlay_root.clone(),
            tokenizer: PostingTokenizerConfig::from(&build_config),
            ..EngineConfig::default()
        })
        .expect("open legacy overlay engine"),
        WorkspaceOverlayPublishPolicy::Checkpoint,
    );
    let mut documents = HashMap::new();
    documents.insert(
        tracked.clone(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"MIGRATED\";\n"),
    );
    let batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([tracked.clone()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build ingest batch");
    legacy_overlay
        .ingest_batch(&batch)
        .expect("ingest legacy overlay batch");

    assert!(legacy_overlay_root.join("CURRENT_MANIFEST").exists());
    assert!(legacy_overlay_root.join("manifests").exists());
    assert!(!workspace_overlay_root.join("CURRENT_MANIFEST").exists());

    let migrated =
        WorkspaceOverlayIndex::open(&build_config).expect("open overlay index should migrate");
    let results = migrated
        .open_snapshot()
        .expect("open migrated snapshot")
        .search(&query("MIGRATED"))
        .expect("search should succeed");

    assert_eq!(results.matched_lines, 1);
    assert!(!legacy_overlay_root.join("CURRENT_MANIFEST").exists());
    assert!(!legacy_overlay_root.join("manifests").exists());
    assert!(workspace_overlay_root.join("CURRENT_MANIFEST").exists());
    assert!(workspace_overlay_root.join("manifests").exists());
}

#[test]
fn checkpoint_publish_policy_compacts_overlay_delete_segments() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-delete-merge-test".into()),
            index_root: WorkspaceOverlayIndex::index_root(&build_config(&repo)),
            ..EngineConfig::default()
        })
        .expect("open engine"),
        WorkspaceOverlayPublishPolicy::Checkpoint,
    );

    let tracked_path = repo.join("tracked.rs");
    let tracked = tracked_path.to_string_lossy().into_owned();
    let mut documents = HashMap::new();
    documents.insert(
        tracked.clone(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"LIVE\";\n"),
    );

    let add_batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([tracked.clone()]),
        &HashSet::new(),
        &documents,
    )
    .expect("build add batch");
    overlay.ingest_batch(&add_batch).expect("ingest add batch");

    let delete_batch =
        WorkspaceOverlayIngestBatch::from_deleted_paths(&repo_id, &repo, &[tracked.clone()])
            .expect("build delete batch");
    overlay
        .ingest_batch(&delete_batch)
        .expect("ingest delete batch");

    wait_for_delete_compaction(&overlay);

    let manifest = overlay
        .engine
        .admin()
        .manifest()
        .expect("manifest should load");
    assert_eq!(manifest.active_delete_segments.len(), 0);

    let results = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot")
        .search(&query("LIVE"))
        .expect("search should succeed");
    assert_eq!(results.matched_lines, 0);
}

#[test]
fn run_requested_merge_drains_overlay_until_stable() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let repo_id = RepoId("repo".into());
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-maintain-test".into()),
            index_root: WorkspaceOverlayIndex::index_root(&build_config(&repo)),
            merge_policy: MergePolicyConfig {
                max_segments: 1,
                ..MergePolicyConfig::default()
            },
            ..EngineConfig::default()
        })
        .expect("open engine"),
        WorkspaceOverlayPublishPolicy::Checkpoint,
    );

    let a_path = repo.join("a.rs");
    let b_path = repo.join("b.rs");
    let c_path = repo.join("c.rs");

    let add_a = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([a_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &HashMap::from([(
            a_path.to_string_lossy().into_owned(),
            loaded_doc(&a_path, "const VALUE: &str = \"ALPHA\";\n"),
        )]),
    )
    .expect("build add batch");
    overlay.ingest_batch(&add_a).expect("ingest add batch");

    let add_b = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([b_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &HashMap::from([(
            b_path.to_string_lossy().into_owned(),
            loaded_doc(&b_path, "const VALUE: &str = \"BETA\";\n"),
        )]),
    )
    .expect("build add batch");
    overlay.ingest_batch(&add_b).expect("ingest add batch");

    let delete_a = WorkspaceOverlayIngestBatch::from_deleted_paths(
        &repo_id,
        &repo,
        &[a_path.to_string_lossy().into_owned()],
    )
    .expect("build delete batch");
    overlay
        .ingest_batch(&delete_a)
        .expect("ingest delete batch");

    let add_c = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &repo,
        &HashSet::new(),
        &HashSet::from([c_path.to_string_lossy().into_owned()]),
        &HashSet::new(),
        &HashMap::from([(
            c_path.to_string_lossy().into_owned(),
            loaded_doc(&c_path, "const VALUE: &str = \"GAMMA\";\n"),
        )]),
    )
    .expect("build add batch");
    overlay.ingest_batch(&add_c).expect("ingest add batch");

    let merged = overlay
        .run_requested_merge()
        .expect("overlay merge should succeed");
    assert!(merged);

    let manifest = overlay
        .engine
        .admin()
        .manifest()
        .expect("manifest should load");
    assert_eq!(manifest.active_segments.len(), 1);
    assert!(manifest.active_delete_segments.is_empty());

    let stats = overlay.runtime_stats().expect("runtime stats should load");
    assert_eq!(stats.merge_completed, 2);

    let alpha = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot")
        .search(&query("ALPHA"))
        .expect("search should succeed");
    assert_eq!(alpha.matched_lines, 0);

    let beta = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot")
        .search(&query("BETA"))
        .expect("search should succeed");
    assert_eq!(beta.matched_lines, 1);

    let gamma = overlay
        .engine
        .reader()
        .open_committed()
        .expect("open committed snapshot")
        .search(&query("GAMMA"))
        .expect("search should succeed");
    assert_eq!(gamma.matched_lines, 1);
}
