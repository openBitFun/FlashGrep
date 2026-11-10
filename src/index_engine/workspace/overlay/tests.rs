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
    WorkspaceOverlaySnapshotState,
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
        git_mode: Default::default(),
        repo_path: repo.to_path_buf(),
        storage_root: repo.join(".flashgrep-index-engine"),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::NoIgnore,
        include_hidden: true,
        max_file_size: 1 << 20,
        min_sparse_len: 3,
        max_sparse_len: 8,
    }
}

fn query(pattern: &str) -> QueryRequest {
    QueryRequest::from_config(
        &crate::QueryConfig {
            regex_pattern: pattern.into(),
            search_mode: SearchMode::CountOnly,
            ..crate::QueryConfig::default()
        },
        QueryFilters::default(),
        false,
    )
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

        // Exercise an ordinary refreshed update. Replacements deliberately commit
        // their complete publication, so they are not an uncommitted fixture.
        let batch = WorkspaceOverlayIngestBatch::from_paths(
            &repo_id,
            &repo,
            &HashSet::new(),
            &documents.keys().cloned().collect(),
            &HashSet::new(),
            &documents,
        )
        .expect("build batch");
        overlay.ingest_batch(&batch).expect("refresh update");
        assert_eq!(
            overlay
                .open_snapshot()
                .unwrap()
                .search(&query("REOPEN"))
                .unwrap()
                .matched_lines,
            1
        );
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
fn manifest_only_reopen_keeps_published_overlay_searchable() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let config = build_config(&repo);
    let repo_id = RepoId("repo".into());
    let tracked_path = repo.join("tracked.rs");
    let mut documents = HashMap::new();
    documents.insert(
        tracked_path.to_string_lossy().into_owned(),
        loaded_doc(&tracked_path, "const VALUE: &str = \"MANIFEST_ONLY\";\n"),
    );
    let overlay = WorkspaceOverlayIndex::open(&config).expect("open overlay");
    let snapshot =
        WorkspaceOverlaySnapshotState::from_paths(&repo_id, &repo, &documents, &HashSet::new())
            .expect("build snapshot");
    overlay.set_manifest_source(
        crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
            base_manifest_id: crate::index_engine::ManifestId("base".into()),
            observed_head: Some("head".into()),
            probe_epoch: 0,
        },
    );
    overlay
        .rebuild_snapshot(&snapshot)
        .expect("refresh snapshot");
    overlay.checkpoint().expect("checkpoint snapshot");

    let reopened = WorkspaceOverlayIndex::open_with_policy_and_hook(
        &config,
        WorkspaceOverlayPublishPolicy::Refresh,
        Arc::new(|_| {}),
    )
    .expect("manifest-only reopen");
    let results = reopened
        .open_snapshot()
        .expect("open snapshot")
        .search(&query("MANIFEST_ONLY"))
        .expect("search should succeed");
    assert_eq!(results.matched_lines, 1);
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
fn background_publication_stays_volatile_until_maintenance() {
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
        WorkspaceOverlayPublishPolicy::Background,
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
    let first_publication = overlay
        .ingest_batch(&first_batch)
        .expect("ingest first batch");
    assert_eq!(
        first_publication,
        super::WorkspaceOverlayPublication::Refreshed
    );

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
    let second_publication = overlay
        .ingest_batch(&second_batch)
        .expect("ingest second batch");
    assert_eq!(
        second_publication,
        super::WorkspaceOverlayPublication::Refreshed
    );
    assert!(
        overlay
            .engine
            .reader()
            .open_committed()
            .unwrap()
            .search(&query("SECOND"))
            .unwrap()
            .matched_lines
            == 0
    );
    overlay.run_requested_merge().unwrap();

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
fn open_defaults_to_background_publish_policy() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("create repo");

    let overlay = WorkspaceOverlayIndex::open(&build_config(&repo)).expect("open overlay index");

    assert_eq!(
        overlay.publish_policy,
        WorkspaceOverlayPublishPolicy::Background
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

    let delete_batch = WorkspaceOverlayIngestBatch::from_deleted_paths(
        &repo_id,
        &repo,
        std::slice::from_ref(&tracked),
    )
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
fn run_requested_merge_yields_between_tasks_and_requests_continuation() {
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

    let first = overlay.runtime_stats().unwrap();
    assert_eq!(first.merge_completed, 1, "one task per background turn");
    assert!(first.merge_requested, "remaining work must be scheduled");
    assert_eq!(
        overlay
            .engine
            .admin()
            .manifest()
            .unwrap()
            .active_segments
            .len(),
        2
    );
    // Ingestion must be able to acquire the budget between merge turns.
    let budget = overlay
        .write_budget
        .try_lock()
        .expect("merge yielded writer lock");
    drop(budget);
    assert!(overlay.run_requested_merge().unwrap());
    assert!(!overlay.run_requested_merge().unwrap());
    assert!(!overlay.runtime_stats().unwrap().merge_requested);

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

#[test]
fn staged_overlay_batch_survives_merge_and_reopen() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..EngineConfig::default()
    };
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(config.clone()).unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    let held = overlay.open_snapshot().unwrap();
    let repo_id = RepoId("repo".into());
    let documents: HashMap<_, _> = (0..super::OVERLAY_STAGE_OPERATIONS + 1)
        .map(|i| {
            let path = root.join(format!("file{i}.rs"));
            (
                path.to_string_lossy().into_owned(),
                loaded_doc(&path, "STAGED_TOKEN\n"),
            )
        })
        .collect();
    let batch = WorkspaceOverlayIngestBatch::from_paths(
        &repo_id,
        &root,
        &HashSet::new(),
        &documents.keys().cloned().collect(),
        &HashSet::new(),
        &documents,
    )
    .unwrap();
    overlay.ingest_batch(&batch).unwrap();
    assert_eq!(overlay.writer_stats().pending_docs, 1);
    assert_eq!(
        held.search(&query("STAGED_TOKEN")).unwrap().matched_lines,
        0
    );
    assert_eq!(
        overlay
            .open_snapshot()
            .unwrap()
            .search(&query("STAGED_TOKEN"))
            .unwrap()
            .matched_lines,
        documents.len()
    );
    overlay.request_background_merge();
    overlay.run_requested_merge().unwrap();
    assert_eq!(overlay.writer_stats().uncommitted_ops, 0);
    let reopened = IndexEngine::open(config).unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("STAGED_TOKEN"))
            .unwrap()
            .matched_lines,
        documents.len()
    );
}

fn one_streamed_document(root: &Path, i: usize) -> WorkspaceOverlayIngestBatch {
    let path = root.join(format!("stream{i}.rs"));
    let path_text = path.to_string_lossy().into_owned();
    WorkspaceOverlayIngestBatch::from_paths(
        &RepoId("repo".into()),
        root,
        &HashSet::new(),
        &HashSet::from([path_text.clone()]),
        &HashSet::new(),
        &HashMap::from([(path_text, loaded_doc(&path, "STREAMED_TOKEN\n"))]),
    )
    .unwrap()
}

#[test]
fn streamed_overlay_batches_publish_only_after_input_exhaustion() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            index_root: temp.path().join("index"),
            ..Default::default()
        })
        .unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    let held = overlay.open_snapshot().unwrap();
    let mut prepared = 0;
    let batches = (0..3).map(|i| {
        // Runs between preparation/writes while the publication mutex is held.
        // Snapshot readers must remain available and must not see a prefix.
        assert_eq!(
            overlay
                .open_snapshot()
                .unwrap()
                .search(&query("STREAMED_TOKEN"))
                .unwrap()
                .matched_lines,
            0
        );
        prepared += 1;
        Ok(one_streamed_document(&root, i))
    });
    overlay.ingest_batches(batches).unwrap();
    assert_eq!(prepared, 3);
    assert_eq!(
        held.search(&query("STREAMED_TOKEN")).unwrap().matched_lines,
        0
    );
    assert_eq!(
        overlay
            .open_snapshot()
            .unwrap()
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        3
    );
}

#[test]
fn streamed_overlay_input_error_cannot_be_committed_by_maintenance() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            index_root: temp.path().join("index"),
            ..Default::default()
        })
        .unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    let batches = [
        Ok(one_streamed_document(&root, 0)),
        Err(crate::error::AppError::Io(std::io::Error::other(
            "source read failed",
        ))),
    ];
    assert!(overlay.ingest_batches(batches).is_err());
    assert_eq!(
        overlay
            .open_snapshot()
            .unwrap()
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        0
    );
    overlay.request_background_merge();
    assert!(overlay.run_requested_merge().is_err());
    assert!(overlay.checkpoint().is_err());
    assert_eq!(overlay.writer_stats().committed_seq_no, 0);
}

#[test]
fn replacement_retains_lazy_refreshed_readers_until_their_last_lease_drops() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let index_root = temp.path().join("index");
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(EngineConfig {
            index_root: index_root.clone(),
            ..Default::default()
        })
        .unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    overlay
        .ingest_batches(
            (0..super::OVERLAY_STAGE_OPERATIONS + 1).map(|i| Ok(one_streamed_document(&root, i))),
        )
        .unwrap();
    // Keep this reader lazy: its first search happens after multiple replacements/GC.
    let held = overlay.open_snapshot().unwrap();
    let old_segments: Vec<_> = held
        .manifest()
        .active_segments
        .iter()
        .map(|segment| index_root.join("segments").join(&segment.id.0))
        .collect();
    assert!(!old_segments.is_empty());
    for _ in 0..4 {
        let previous_count = overlay
            .open_snapshot()
            .unwrap()
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines;
        let batches = (0..2).map(|i| {
            assert_eq!(
                overlay
                    .open_snapshot()
                    .unwrap()
                    .search(&query("STREAMED_TOKEN"))
                    .unwrap()
                    .matched_lines,
                previous_count
            );
            Ok(one_streamed_document(&root, i))
        });
        // First iteration has a larger previous publication; record it explicitly.
        overlay.replace_batches(batches).unwrap();
        overlay.run_requested_merge().unwrap();
        assert!(old_segments.iter().all(|path| path.exists()));
    }
    assert_eq!(
        held.search(&query("STREAMED_TOKEN")).unwrap().matched_lines,
        super::OVERLAY_STAGE_OPERATIONS + 1
    );
    drop(held);
    overlay.request_background_merge();
    overlay.run_requested_merge().unwrap();
    assert!(old_segments.iter().all(|path| !path.exists()));
    let reopened = IndexEngine::open(EngineConfig {
        index_root,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        2
    );
}

#[test]
fn failed_replacement_preserves_the_previous_durable_index() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(config.clone()).unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    overlay
        .replace_batches((0..3).map(|i| Ok(one_streamed_document(&root, i))))
        .unwrap();
    let held = overlay.open_snapshot().unwrap();
    let previous_id = held.manifest().manifest_id.clone();
    let result = overlay.replace_batches([
        Ok(one_streamed_document(&root, 0)),
        Err(crate::error::AppError::Io(std::io::Error::other(
            "replacement read failed",
        ))),
    ]);
    assert!(result.is_err());
    let current = overlay.open_snapshot().unwrap();
    assert_eq!(current.manifest().manifest_id, previous_id);
    assert_eq!(
        current
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        3
    );
    assert!(overlay.checkpoint().is_err());
    let reopened = IndexEngine::open(config).unwrap();
    let recovered = reopened.reader().open_snapshot().unwrap();
    assert_eq!(recovered.manifest().manifest_id, previous_id);
    assert_eq!(
        recovered
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        3
    );
}

#[test]
fn reopened_overlay_preserves_previous_instance_readers_and_artifact_names() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let old = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(config.clone()).unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    old.replace_batches(std::iter::once(Ok(one_streamed_document(&root, 0))))
        .unwrap();
    old.ingest_batches(
        (1..super::OVERLAY_STAGE_OPERATIONS + 1).map(|i| Ok(one_streamed_document(&root, i))),
    )
    .unwrap();
    let held = old.open_snapshot().unwrap();
    let previous_id = held.manifest().manifest_id.clone();
    let previous_segments: Vec<_> = held
        .manifest()
        .active_segments
        .iter()
        .map(|segment| segment.id.clone())
        .collect();
    drop(old); // Keep only the reader, including its segment leases.

    let reopened = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(config.clone()).unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    // Reproduce the same operation/generation pattern after the durable baseline.
    reopened
        .ingest_batches(
            (1..super::OVERLAY_STAGE_OPERATIONS + 1)
                .map(|i| Ok(one_streamed_document(&root, i + 2000))),
        )
        .unwrap();
    let current = reopened.open_snapshot().unwrap();
    assert_ne!(current.manifest().manifest_id, previous_id);
    for segment in current.manifest().active_segments.iter().skip(1) {
        assert!(
            !previous_segments.contains(&segment.id),
            "reopen must not overwrite a reader's staged segment"
        );
    }
    for _ in 0..4 {
        reopened
            .replace_batches(std::iter::once(Ok(one_streamed_document(&root, 0))))
            .unwrap();
        reopened.run_requested_merge().unwrap();
    }
    assert_eq!(
        held.search(&query("STREAMED_TOKEN")).unwrap().matched_lines,
        super::OVERLAY_STAGE_OPERATIONS + 1
    );
    for id in &previous_segments {
        assert!(config.index_root.join("segments").join(&id.0).exists());
    }
    drop(held);
    drop(current);
    reopened.request_background_merge();
    reopened.run_requested_merge().unwrap();
    for id in &previous_segments {
        assert!(!config.index_root.join("segments").join(&id.0).exists());
    }
}

#[test]
fn retraction_preserves_other_documents_and_survives_reopen() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let overlay = WorkspaceOverlayIndex::new_with_policy(
        IndexEngine::open(config.clone()).unwrap(),
        WorkspaceOverlayPublishPolicy::Refresh,
    );
    overlay
        .replace_batches((0..3).map(|i| Ok(one_streamed_document(&root, i))))
        .unwrap();
    let held = overlay.open_snapshot().unwrap();
    let previous_segments = held.manifest().active_segments.clone();
    let previous_seq = overlay.writer_stats().last_seq_no;
    let path = root.join("stream1.rs").to_string_lossy().into_owned();
    let retraction = WorkspaceOverlayIngestBatch::from_retracted_paths(
        &RepoId("repo".into()),
        &root,
        &[path.clone()],
    )
    .unwrap();
    let key = retraction.retracted_keys[0].clone();
    overlay.ingest_batch(&retraction).unwrap();
    assert_eq!(overlay.writer_stats().last_seq_no, previous_seq + 1);
    let current = overlay.open_snapshot().unwrap();
    assert_eq!(current.manifest().active_segments, previous_segments);
    assert_eq!(
        current
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        2
    );
    assert!(!current.visible_keys().unwrap().deleted.contains(&key));
    assert!(!current.visible_keys().unwrap().live.contains(&key));
    assert_eq!(
        held.search(&query("STREAMED_TOKEN")).unwrap().matched_lines,
        3
    );
    overlay.checkpoint().unwrap();
    let reopened = IndexEngine::open(config).unwrap();
    let recovered = reopened.reader().open_snapshot().unwrap();
    assert_eq!(
        recovered
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        2
    );
    assert!(!recovered.visible_keys().unwrap().deleted.contains(&key));
    // A new edit supersedes the retraction, and an ordinary deletion must mask the base again.
    overlay
        .ingest_batch(&one_streamed_document(&root, 1))
        .unwrap();
    assert_eq!(
        overlay
            .open_snapshot()
            .unwrap()
            .search(&query("STREAMED_TOKEN"))
            .unwrap()
            .matched_lines,
        3
    );
    overlay
        .ingest_batch(
            &WorkspaceOverlayIngestBatch::from_paths(
                &RepoId("repo".into()),
                &root,
                &HashSet::new(),
                &HashSet::new(),
                &HashSet::from([path]),
                &HashMap::new(),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(overlay
        .open_snapshot()
        .unwrap()
        .visible_keys()
        .unwrap()
        .deleted
        .contains(&key));
    overlay.ingest_batch(&retraction).unwrap();
    assert!(!overlay
        .open_snapshot()
        .unwrap()
        .visible_keys()
        .unwrap()
        .deleted
        .contains(&key));
}
