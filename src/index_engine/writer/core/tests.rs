use std::{fs, sync::Arc};

use crate::{
    index_engine::{
        config::MergePolicyConfig,
        query::{QueryFilters, QueryRequest},
        types::{DocKey, RepoId},
        EngineConfig, IndexEngine,
    },
    search::SearchMode,
    TokenizerMode,
};
use tempfile::tempdir;

fn test_doc(path: &str, text: &str) -> super::IndexDocument {
    super::IndexDocument {
        key: DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: path.into(),
        },
        repo_id: RepoId("repo".into()),
        logical_path: path.into(),
        file_name: path.rsplit('/').next().unwrap_or(path).into(),
        language: Some("rust".into()),
        size: text.len() as u64,
        modified_at: 1,
        content_hash: [0u8; 32],
        source_bytes: Arc::<[u8]>::from(text.as_bytes().to_vec()),
    }
}

fn query(pattern: &str) -> QueryRequest {
    QueryRequest::from_config(
        &crate::QueryConfig {
            regex_pattern: pattern.into(),
            search_mode: SearchMode::LineMatches,
            ..crate::QueryConfig::default()
        },
        QueryFilters::default(),
        false,
    )
}

fn test_config() -> EngineConfig {
    let temp = tempdir().expect("tempdir should succeed");
    EngineConfig {
        index_root: temp.keep(),
        ..EngineConfig::default()
    }
}

fn sparse_test_config() -> EngineConfig {
    EngineConfig {
        tokenizer: crate::index_engine::PostingTokenizerConfig {
            mode: TokenizerMode::SparseNgram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        },
        ..test_config()
    }
}

#[test]
fn refresh_exposes_recent_adds_to_snapshot_reader() {
    let engine = IndexEngine::open(test_config()).expect("engine should open");
    let writer = engine.writer();
    let reader = engine.reader();

    writer
        .add_document(test_doc(
            "src/lib.rs",
            "fn main() { println!(\"HELLO\"); }\n",
        ))
        .expect("add should succeed");

    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("HELLO"))
            .expect("search should succeed")
            .matched_lines,
        0
    );

    writer.refresh().expect("refresh should succeed");

    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("HELLO"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn commit_persists_recent_adds_for_reopen() {
    let config = test_config();
    {
        let engine = IndexEngine::open(config.clone()).expect("engine should open");
        let writer = engine.writer();
        writer
            .add_document(test_doc("src/lib.rs", "const NAME: &str = \"REOPEN\";\n"))
            .expect("add should succeed");
        writer.commit().expect("commit should succeed");
    }

    let reopened = IndexEngine::open(config).expect("engine should reopen");
    assert_eq!(
        reopened
            .reader()
            .open_committed()
            .expect("committed snapshot should open")
            .search(&query("REOPEN"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn manifest_only_reopen_does_not_read_segment_documents() {
    let config = test_config();
    let engine = IndexEngine::open(config.clone()).expect("engine should open");
    engine
        .writer()
        .add_document(test_doc("src/lib.rs", "MANIFEST_ONLY\n"))
        .expect("add should succeed");
    engine.writer().commit().expect("commit should succeed");
    let manifest = engine.admin().manifest().expect("manifest should load");
    let segment_id = manifest
        .active_segments
        .first()
        .expect("segment should exist")
        .id
        .0
        .clone();
    drop(engine);
    fs::remove_file(
        config
            .index_root
            .join("segments")
            .join(segment_id)
            .join("docs.bin"),
    )
    .expect("document table should be removed");

    let reopened = IndexEngine::open_manifest_only(config)
        .expect("manifest-only reopen must not read segment documents");
    assert_eq!(reopened.writer().stats().committed_seq_no, 1);
}

#[test]
fn flush_writes_segment_files_and_commit_publishes_segment_refs() {
    let config = test_config();
    let engine = IndexEngine::open(config.clone()).expect("engine should open");
    let writer = engine.writer();
    let admin = engine.admin();

    writer
        .add_document(test_doc(
            "src/lib.rs",
            "const NAME: &str = \"SEGMENTED\";\n",
        ))
        .expect("add should succeed");
    let flush = writer.flush().expect("flush should succeed");
    assert_eq!(flush.segment_ids.len(), 1);

    let segment_dir = config
        .index_root
        .join("segments")
        .join(&flush.segment_ids[0].0);
    assert!(segment_dir.join("SEGMENT_META.json").exists());
    assert!(segment_dir.join("docs.bin").exists());
    assert!(!segment_dir.join("stored_fields.bin").exists());

    writer.commit().expect("commit should succeed");
    let manifest = admin.manifest().expect("manifest should load");
    assert_eq!(manifest.active_segments.len(), 1);
    assert_eq!(manifest.active_segments[0].id, flush.segment_ids[0]);
}

#[test]
fn reopen_drops_uncommitted_flush() {
    let config = test_config();
    {
        let engine = IndexEngine::open(config.clone()).expect("engine should open");
        let writer = engine.writer();
        writer
            .add_document(test_doc("src/lib.rs", "const NAME: &str = \"TEMP\";\n"))
            .expect("add should succeed");
        writer.flush().expect("flush should succeed");
    }

    let reopened = IndexEngine::open(config).expect("engine should reopen");
    assert_eq!(
        reopened
            .reader()
            .open_committed()
            .expect("committed snapshot should open")
            .search(&query("TEMP"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
}

#[test]
fn update_and_delete_change_visible_results_after_refresh() {
    let engine = IndexEngine::open(test_config()).expect("engine should open");
    let writer = engine.writer();
    let reader = engine.reader();

    writer
        .add_document(test_doc("src/lib.rs", "const NAME: &str = \"ALPHA\";\n"))
        .expect("add should succeed");
    writer.refresh().expect("refresh should succeed");
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("ALPHA"))
            .expect("search should succeed")
            .matched_lines,
        1
    );

    writer
        .update_document(test_doc("src/lib.rs", "const NAME: &str = \"OMEGA\";\n"))
        .expect("update should succeed");
    writer.refresh().expect("refresh should succeed");
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("ALPHA"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("OMEGA"))
            .expect("search should succeed")
            .matched_lines,
        1
    );

    writer
        .delete_document(&DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: "src/lib.rs".into(),
        })
        .expect("delete should succeed");
    writer.refresh().expect("refresh should succeed");
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("OMEGA"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
}

#[test]
fn discard_uncommitted_changes_drops_unpublished_snapshot_state() {
    let engine = IndexEngine::open(test_config()).expect("engine should open");
    let writer = engine.writer();
    let reader = engine.reader();

    writer
        .add_document(test_doc("src/lib.rs", "const NAME: &str = \"DROP_ME\";\n"))
        .expect("add should succeed");
    writer.refresh().expect("refresh should succeed");
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("DROP_ME"))
            .expect("search should succeed")
            .matched_lines,
        1
    );

    writer
        .discard_uncommitted()
        .expect("discard should succeed");
    assert_eq!(
        reader
            .open_snapshot()
            .expect("snapshot should open")
            .search(&query("DROP_ME"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
}

#[test]
fn vacuum_reclaims_superseded_manifests_and_keeps_latest_searchable_state() {
    let config = test_config();
    let engine = IndexEngine::open(config.clone()).expect("engine should open");
    let writer = engine.writer();
    let admin = engine.admin();

    writer
        .add_document(test_doc("src/old.rs", "const NAME: &str = \"OLD\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");
    let manifest1 = admin.manifest().expect("manifest should load");
    let manifest1_path = config
        .index_root
        .join("manifests")
        .join(format!("{}.json", manifest1.manifest_id.0));

    writer
        .delete_document(&DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: "src/old.rs".into(),
        })
        .expect("delete should succeed");
    writer.commit().expect("commit should succeed");
    let manifest2 = admin.manifest().expect("manifest should load");
    let manifest2_path = config
        .index_root
        .join("manifests")
        .join(format!("{}.json", manifest2.manifest_id.0));

    writer
        .add_document(test_doc("src/new.rs", "const NAME: &str = \"NEW\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");
    let manifest3 = admin.manifest().expect("manifest should load");
    let newest_segment_id = manifest3.active_segments[0].id.clone();
    let manifest3_path = config
        .index_root
        .join("manifests")
        .join(format!("{}.json", manifest3.manifest_id.0));

    admin.vacuum().expect("vacuum should succeed");

    assert!(!manifest1_path.exists());
    assert!(manifest2_path.exists());
    assert!(manifest3_path.exists());
    assert!(config
        .index_root
        .join("segments")
        .join(&newest_segment_id.0)
        .exists());

    let committed = engine
        .reader()
        .open_committed()
        .expect("committed snapshot should open");
    assert_eq!(
        committed
            .search(&query("OLD"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
    assert_eq!(
        committed
            .search(&query("NEW"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn vacuum_retains_artifacts_needed_by_pinned_readers() {
    let temp = tempdir().expect("tempdir should succeed");
    let config = EngineConfig {
        index_root: temp.path().to_path_buf(),
        ..EngineConfig::default()
    };
    let engine = IndexEngine::open(config.clone()).expect("engine should open");
    let writer = engine.writer();
    let admin = engine.admin();
    let readers = engine.reader();

    writer
        .add_document(test_doc("src/old.rs", "const NAME: &str = \"OLD\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");
    let manifest1 = admin.manifest().expect("manifest should load");
    let manifest1_path = config
        .index_root
        .join("manifests")
        .join(format!("{}.json", manifest1.manifest_id.0));
    let segment1_path = config
        .index_root
        .join("segments")
        .join(&manifest1.active_segments[0].id.0);

    let pinned_reader = readers
        .open_at_manifest(manifest1.manifest_id.clone())
        .expect("manifest reader should open");

    writer
        .delete_document(&DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: "src/old.rs".into(),
        })
        .expect("delete should succeed");
    writer.commit().expect("commit should succeed");

    writer
        .add_document(test_doc("src/new.rs", "const NAME: &str = \"NEW\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    let maintain = admin.maintain().expect("maintenance should succeed");
    assert!(maintain.merge_tasks >= 1);
    assert!(manifest1_path.exists());

    writer
        .add_document(test_doc("src/later.rs", "const NAME: &str = \"LATER\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    let removed_while_pinned = admin.vacuum().expect("vacuum should succeed");
    assert!(removed_while_pinned > 0);
    assert!(manifest1_path.exists());
    assert!(segment1_path.exists());
    assert_eq!(
        pinned_reader
            .search(&query("OLD"))
            .expect("search should succeed")
            .matched_lines,
        1
    );

    drop(pinned_reader);

    let removed_after_drop = admin.vacuum().expect("vacuum should succeed");
    assert!(removed_after_drop > 0);
    assert!(!manifest1_path.exists());
    assert!(!segment1_path.exists());
}

#[test]
fn maintain_drains_multi_step_merges_and_vacuums_stale_files() {
    let temp = tempdir().expect("tempdir should succeed");
    let config = EngineConfig {
        index_root: temp.path().to_path_buf(),
        merge_policy: MergePolicyConfig {
            max_segments: 1,
            ..MergePolicyConfig::default()
        },
        ..EngineConfig::default()
    };
    let engine = IndexEngine::open(config.clone()).expect("engine should open");
    let writer = engine.writer();
    let admin = engine.admin();

    writer
        .add_document(test_doc("src/a.rs", "const NAME: &str = \"ALPHA\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    writer
        .add_document(test_doc("src/b.rs", "const NAME: &str = \"BETA\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    writer
        .delete_document(&DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: "src/a.rs".into(),
        })
        .expect("delete should succeed");
    writer.commit().expect("commit should succeed");

    writer
        .add_document(test_doc("src/c.rs", "const NAME: &str = \"GAMMA\";\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    let before = admin.manifest().expect("manifest should load");
    assert_eq!(before.active_segments.len(), 3);
    assert_eq!(before.active_delete_segments.len(), 1);

    let stats = admin.maintain().expect("maintenance should succeed");
    assert_eq!(stats.merge_tasks, 2);
    assert!(stats.vacuumed_files > 0);

    let after = admin.manifest().expect("manifest should load");
    assert_eq!(after.active_segments.len(), 1);
    assert!(after.active_delete_segments.is_empty());

    let manifest_count = fs::read_dir(config.index_root.join("manifests"))
        .expect("manifests dir should exist")
        .filter_map(std::result::Result::ok)
        .count();
    assert_eq!(manifest_count, 2);

    let committed = engine
        .reader()
        .open_committed()
        .expect("committed snapshot should open");
    assert_eq!(
        committed
            .search(&query("ALPHA"))
            .expect("search should succeed")
            .matched_lines,
        0
    );
    assert_eq!(
        committed
            .search(&query("BETA"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
    assert_eq!(
        committed
            .search(&query("GAMMA"))
            .expect("search should succeed")
            .matched_lines,
        1
    );
}

#[test]
fn sparse_engine_uses_covering_query_tokens_for_dry_run() {
    let engine = IndexEngine::open(sparse_test_config()).expect("engine should open");
    let writer = engine.writer();
    writer
        .add_document(test_doc("src/lib.rs", "fn getUserById() { }\n"))
        .expect("add should succeed");
    writer.commit().expect("commit should succeed");

    let profile = engine
        .dry_run_query(&query("getUserById"))
        .expect("dry run should succeed");
    assert_eq!(profile.candidate_docs, 1);
    assert_eq!(profile.plan.candidate_branches.len(), 1);
    assert!(!profile.plan.candidate_branches[0].grams.is_empty());
    assert!(profile.plan.candidate_branches[0].grams.len() < "getUserById".len() - 2);
}

#[test]
fn memory_segments_preserve_postings_deletes_and_pinned_snapshots() {
    let temp = tempdir().expect("temp dir");
    let root = temp.path().join("must-not-be-created");
    let engine = IndexEngine::in_memory(EngineConfig {
        index_root: root.clone(),
        ..EngineConfig::default()
    })
    .expect("memory engine");
    let writer = engine.writer();
    writer
        .add_document(test_doc("a.rs", "OLD_TOKEN"))
        .expect("add");
    writer.commit().expect("commit");
    let old = engine.reader().open_committed().expect("old snapshot");
    assert_eq!(
        old.search(&query("OLD_TOKEN"))
            .expect("search")
            .matched_lines,
        1
    );
    writer
        .update_document(test_doc("a.rs", "NEW_TOKEN"))
        .expect("update");
    writer
        .add_document(test_doc("b.rs", "NEW_TOKEN"))
        .expect("add");
    writer.commit().expect("commit");
    let updated = engine.reader().open_committed().expect("updated");
    assert_eq!(
        updated
            .search(&query("OLD_TOKEN"))
            .expect("search")
            .matched_lines,
        0
    );
    assert_eq!(
        updated
            .search(&query("NEW_TOKEN"))
            .expect("search")
            .matched_lines,
        2
    );
    writer
        .delete_document(&test_doc("a.rs", "").key)
        .expect("delete");
    writer.commit().expect("commit deletion");
    assert_eq!(
        engine
            .reader()
            .open_committed()
            .expect("deleted")
            .search(&query("NEW_TOKEN"))
            .expect("search")
            .matched_lines,
        1
    );
    assert_eq!(
        old.search(&query("OLD_TOKEN"))
            .expect("old still visible")
            .matched_lines,
        1
    );
    assert!(!root.exists(), "memory publication must not touch disk");
}

#[test]
fn unpublished_flush_preserves_readers_until_the_complete_batch_is_published() {
    let config = test_config();
    let engine = IndexEngine::open(config.clone()).unwrap();
    let writer = engine.writer();
    writer
        .add_document(test_doc("old.rs", "OLD_TOKEN\n"))
        .unwrap();
    writer.commit().unwrap();
    let held = engine.reader().open_snapshot().unwrap();
    writer.delete_document(&test_doc("old.rs", "").key).unwrap();
    for i in 0..3 {
        writer
            .add_document(test_doc(&format!("new{i}.rs"), "NEW_TOKEN\n"))
            .unwrap();
        writer.flush_unpublished().unwrap();
        assert_eq!(writer.stats().pending_docs, 0);
        let current = engine.reader().open_snapshot().unwrap();
        assert_eq!(
            current.search(&query("OLD_TOKEN")).unwrap().matched_lines,
            1
        );
        assert_eq!(
            current.search(&query("NEW_TOKEN")).unwrap().matched_lines,
            0
        );
    }
    writer.refresh().unwrap();
    let published = engine.reader().open_snapshot().unwrap();
    assert_eq!(
        published.search(&query("OLD_TOKEN")).unwrap().matched_lines,
        0
    );
    assert_eq!(
        published.search(&query("NEW_TOKEN")).unwrap().matched_lines,
        3
    );
    assert_eq!(held.search(&query("OLD_TOKEN")).unwrap().matched_lines, 1);
    writer.commit().unwrap();
    let reopened = IndexEngine::open(config).unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("NEW_TOKEN"))
            .unwrap()
            .matched_lines,
        3
    );
}

#[test]
fn abandoned_unpublished_flush_restores_the_durable_publication() {
    let engine = IndexEngine::open(test_config()).unwrap();
    let writer = engine.writer();
    writer
        .add_document(test_doc("old.rs", "OLD_TOKEN\n"))
        .unwrap();
    writer.commit().unwrap();
    writer
        .update_document(test_doc("old.rs", "NEW_TOKEN\n"))
        .unwrap();
    writer.flush_unpublished().unwrap();
    writer.discard_uncommitted().unwrap();
    writer.refresh().unwrap();
    let current = engine.reader().open_snapshot().unwrap();
    assert_eq!(
        current.search(&query("OLD_TOKEN")).unwrap().matched_lines,
        1
    );
    assert_eq!(
        current.search(&query("NEW_TOKEN")).unwrap().matched_lines,
        0
    );
}

#[test]
fn unpublished_flush_releases_source_payload_and_preserves_later_writes() {
    let engine = IndexEngine::open(test_config()).unwrap();
    let writer = engine.writer();
    let document = test_doc("first.rs", "FIRST_TOKEN\n");
    let source = Arc::downgrade(&document.source_bytes);
    writer.add_document(document).unwrap();
    writer.flush_unpublished().unwrap();
    assert!(
        source.upgrade().is_none(),
        "writer history must not retain staged source bytes"
    );

    // Deterministically model a write admitted while a flush plan is on disk.
    writer
        .add_document(test_doc("second.rs", "SECOND_TOKEN\n"))
        .unwrap();
    let plan = super::flush::build_flush_plan(&writer.state.lock().unwrap()).unwrap();
    writer
        .update_document(test_doc("second.rs", "LATEST_TOKEN\n"))
        .unwrap();
    if let Some(segment) = plan.segment_build.clone() {
        writer.segment_store.write_segment(segment).unwrap();
    }
    if let Some(deletes) = plan.delete_build.clone() {
        writer.delete_store.write_delete_segment(deletes).unwrap();
    }
    super::flush::apply_completed_flush(&mut writer.state.lock().unwrap(), &plan);
    assert_eq!(writer.stats().pending_docs, 1);
    writer.commit().unwrap();
    let snapshot = engine.reader().open_snapshot().unwrap();
    assert_eq!(
        snapshot
            .search(&query("FIRST_TOKEN"))
            .unwrap()
            .matched_lines,
        1
    );
    assert_eq!(
        snapshot
            .search(&query("SECOND_TOKEN"))
            .unwrap()
            .matched_lines,
        0
    );
    assert_eq!(
        snapshot
            .search(&query("LATEST_TOKEN"))
            .unwrap()
            .matched_lines,
        1
    );
}

#[test]
fn retained_overlay_deletes_do_not_repeat_compaction_after_reopen() {
    use crate::index_engine::{manifest::ManifestSource, ManifestId};
    let config = test_config();
    let engine = IndexEngine::open(config.clone()).unwrap();
    let writer = engine.writer();
    writer.set_manifest_source(Some(ManifestSource::WorkspaceOverlay {
        base_manifest_id: ManifestId("base".into()),
        observed_head: None,
        probe_epoch: 0,
    }));
    writer
        .add_document(test_doc("keep.rs", "KEEP_TOKEN\n"))
        .unwrap();
    writer
        .add_document(test_doc("remove.rs", "REMOVE_TOKEN\n"))
        .unwrap();
    writer.commit().unwrap();
    let held = engine.reader().open_committed().unwrap();
    writer
        .delete_document(&test_doc("remove.rs", "").key)
        .unwrap();
    writer.commit().unwrap();
    assert!(engine.admin().maybe_merge().unwrap());
    // Bounded calls deliberately expose a repeated task without hanging maintain().
    assert!(
        !engine.admin().maybe_merge().unwrap(),
        "retained mask must not reschedule the same merge"
    );
    let manifest = engine.admin().manifest().unwrap();
    assert_eq!(
        manifest.active_delete_segments.len(),
        1,
        "base deletion mask must survive"
    );
    assert!(manifest.active_segments[0].applied_delete_seq_no > 0);
    assert_eq!(
        held.search(&query("REMOVE_TOKEN")).unwrap().matched_lines,
        1
    );
    assert_eq!(
        engine
            .reader()
            .open_committed()
            .unwrap()
            .search(&query("REMOVE_TOKEN"))
            .unwrap()
            .matched_lines,
        0
    );
    assert_eq!(
        engine
            .reader()
            .open_committed()
            .unwrap()
            .search(&query("KEEP_TOKEN"))
            .unwrap()
            .matched_lines,
        1
    );
    drop(held);
    drop(engine);

    let reopened = IndexEngine::open(config).unwrap();
    assert!(
        !reopened.admin().maybe_merge().unwrap(),
        "durable watermark must survive reopen"
    );
    reopened
        .writer()
        .delete_document(&test_doc("keep.rs", "").key)
        .unwrap();
    reopened.writer().commit().unwrap();
    assert!(
        reopened.admin().maybe_merge().unwrap(),
        "later deletions must still be applied"
    );
    assert!(!reopened.admin().maybe_merge().unwrap());
    assert_eq!(
        reopened
            .reader()
            .open_committed()
            .unwrap()
            .search(&query("KEEP_TOKEN"))
            .unwrap()
            .matched_lines,
        0
    );
}

#[test]
fn budgeted_overlay_deletion_maintenance_terminates_and_preserves_reader_pins() {
    use crate::index_engine::{manifest::ManifestSource, ManifestId};
    let mut config = test_config();
    config.merge_policy.floor_segment_bytes = 1;
    config.merge_policy.max_merged_segment_bytes = 30;
    let engine = IndexEngine::open(config).unwrap();
    let writer = engine.writer();
    writer.set_manifest_source(Some(ManifestSource::WorkspaceOverlay {
        base_manifest_id: ManifestId("base".into()),
        observed_head: None,
        probe_epoch: 0,
    }));
    for i in 0..5 {
        writer
            .add_document(test_doc(&format!("keep-{i}.rs"), "KEPT_TOKEN\n"))
            .unwrap();
        writer
            .add_document(test_doc(&format!("drop-{i}.rs"), "DROP_TOKEN\n"))
            .unwrap();
        writer.commit().unwrap();
    }
    let held = engine.reader().open_committed().unwrap();
    for i in 0..5 {
        writer
            .delete_document(&test_doc(&format!("drop-{i}.rs"), "").key)
            .unwrap();
    }
    writer.commit().unwrap();
    // Each source segment is 22 bytes; the 30-byte budget permits one per task.
    for _ in 0..5 {
        assert!(engine.admin().maybe_merge().unwrap());
        engine.admin().vacuum().unwrap();
    }
    assert!(!engine.admin().maybe_merge().unwrap());
    assert_eq!(held.search(&query("DROP_TOKEN")).unwrap().matched_lines, 5);
    let current = engine.reader().open_committed().unwrap();
    assert_eq!(
        current.search(&query("DROP_TOKEN")).unwrap().matched_lines,
        0
    );
    assert_eq!(
        current.search(&query("KEPT_TOKEN")).unwrap().matched_lines,
        5
    );
    drop(held);
    assert!(engine.admin().vacuum().unwrap() > 0);
    assert_eq!(engine.admin().maintain().unwrap().merge_tasks, 0);
}

#[test]
fn memory_publication_checkpoints_captured_revision_without_losing_newer_writes() {
    let temp = tempdir().unwrap();
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let engine = IndexEngine::open_staged(config.clone()).unwrap();
    let writer = engine.writer();
    writer
        .add_document(test_doc("a.rs", "FIRST_VERSION"))
        .unwrap();
    writer.flush().unwrap();
    let held = engine.reader().open_snapshot().unwrap();
    assert_eq!(
        held.search(&query("FIRST_VERSION")).unwrap().matched_lines,
        1
    );
    assert_eq!(
        fs::read_dir(config.index_root.join("segments"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(writer.stats().committed_seq_no, 0);
    let captured = writer.capture_checkpoint();
    let captured_seq = captured.snapshot.manifest.committed_seq_no;
    writer
        .update_document(test_doc("a.rs", "SECOND_VERSION"))
        .unwrap();
    writer
        .add_document(test_doc("b.rs", "ADDED_VERSION"))
        .unwrap();
    writer.flush().unwrap();
    writer
        .persist_snapshot(&captured.snapshot.manifest)
        .unwrap();
    assert!(writer.finish_checkpoint(captured).unwrap());
    assert_eq!(writer.stats().committed_seq_no, captured_seq);
    assert!(writer.stats().uncommitted_ops > 0);
    let current = engine.reader().open_snapshot().unwrap();
    assert_eq!(
        current
            .search(&query("FIRST_VERSION"))
            .unwrap()
            .matched_lines,
        0
    );
    assert_eq!(
        current
            .search(&query("SECOND_VERSION|ADDED_VERSION"))
            .unwrap()
            .matched_lines,
        2
    );
    assert_eq!(
        held.search(&query("FIRST_VERSION")).unwrap().matched_lines,
        1
    );
    let reopened = IndexEngine::open_staged(config.clone()).unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("FIRST_VERSION"))
            .unwrap()
            .matched_lines,
        1
    );
    drop(reopened);
    writer.delete_document(&test_doc("a.rs", "").key).unwrap();
    writer.flush().unwrap();
    writer.commit().unwrap();
    let reopened = IndexEngine::open_staged(config).unwrap();
    let durable = reopened.reader().open_snapshot().unwrap();
    assert_eq!(
        durable
            .search(&query("FIRST_VERSION|SECOND_VERSION"))
            .unwrap()
            .matched_lines,
        0
    );
    assert_eq!(
        durable
            .search(&query("ADDED_VERSION"))
            .unwrap()
            .matched_lines,
        1
    );
}

#[test]
fn memory_checkpoint_failure_keeps_visible_bytes_and_durable_sequence_retryable() {
    let temp = tempdir().unwrap();
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let engine = IndexEngine::open_staged(config.clone()).unwrap();
    let writer = engine.writer();
    writer
        .add_document(test_doc("a.rs", "RETRY_CONTENT"))
        .unwrap();
    writer.flush().unwrap();
    let segments = config.index_root.join("segments");
    fs::remove_dir(&segments).unwrap();
    fs::write(&segments, "obstruct persistence").unwrap();
    assert!(writer.commit().is_err());
    assert_eq!(writer.stats().committed_seq_no, 0);
    assert_eq!(
        engine
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("RETRY_CONTENT"))
            .unwrap()
            .matched_lines,
        1
    );
    fs::remove_file(&segments).unwrap();
    writer.commit().unwrap();
    let reopened = IndexEngine::open_staged(config).unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("RETRY_CONTENT"))
            .unwrap()
            .matched_lines,
        1
    );
}

#[test]
fn replacement_rejects_inflight_checkpoint_from_previous_base() {
    let temp = tempdir().unwrap();
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let engine = IndexEngine::open_staged(config.clone()).unwrap();
    let writer = engine.writer();
    writer.add_document(test_doc("old.rs", "OLD_BASE")).unwrap();
    writer.flush().unwrap();
    let captured = writer.capture_checkpoint();
    writer.begin_replacement();
    writer.add_document(test_doc("new.rs", "NEW_BASE")).unwrap();
    writer.flush().unwrap();
    writer
        .persist_snapshot(&captured.snapshot.manifest)
        .unwrap();
    assert!(!writer.finish_checkpoint(captured).unwrap());
    assert_eq!(writer.stats().committed_seq_no, 0);
    assert_eq!(
        engine
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("OLD_BASE"))
            .unwrap()
            .matched_lines,
        0
    );
    writer.commit().unwrap();
    let reopened = IndexEngine::open_staged(config).unwrap();
    assert_eq!(
        reopened
            .reader()
            .open_snapshot()
            .unwrap()
            .search(&query("NEW_BASE"))
            .unwrap()
            .matched_lines,
        1
    );
}

#[test]
fn background_disk_wait_does_not_block_new_publication_or_queries() {
    use crate::index_engine::{
        delete::{FileDeleteStore, StagedDeleteStore},
        manifest::FileManifestStore,
        reader::state::SharedSnapshotState,
        segment::{
            FileSegmentStore, SegmentBuildOutput, SegmentMeta, SegmentReaderHandle, SegmentStore,
            StagedSegmentStore,
        },
        types::SegmentId,
        workspace::{
            WorkspaceOverlayIndex, WorkspaceOverlayIngestBatch, WorkspaceOverlayPublishPolicy,
        },
    };
    use std::{
        sync::{mpsc, Mutex},
        time::Duration,
    };
    fn batch(text: &str) -> WorkspaceOverlayIngestBatch {
        let path = "/repo/a.rs".to_string();
        WorkspaceOverlayIngestBatch::from_paths(
            &RepoId("repo".into()),
            std::path::Path::new("/repo"),
            &std::collections::HashSet::from([path.clone()]),
            &Default::default(),
            &Default::default(),
            &std::collections::HashMap::from([(
                path.clone(),
                crate::search_engine::SearchDocument::from_loaded_bytes(
                    path,
                    text.len() as u64,
                    1,
                    Arc::from(text.as_bytes()),
                ),
            )]),
        )
        .unwrap()
    }
    struct GatedStore {
        inner: StagedSegmentStore,
        gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    }
    impl SegmentStore for GatedStore {
        fn write_segment(&self, b: SegmentBuildOutput) -> crate::Result<SegmentMeta> {
            self.inner.write_segment(b)
        }
        fn open_segment(&self, id: SegmentId) -> crate::Result<SegmentReaderHandle> {
            self.inner.open_segment(id)
        }
        fn remove_segment(&self, id: SegmentId) -> crate::Result<()> {
            self.inner.remove_segment(id)
        }
        fn persist_segment(&self, id: &SegmentId) -> crate::Result<()> {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv().unwrap();
            }
            self.inner.persist_segment(id)
        }
    }
    let temp = tempdir().unwrap();
    let config = EngineConfig {
        index_root: temp.path().to_path_buf(),
        ..Default::default()
    };
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let segments = Arc::new(GatedStore {
        inner: StagedSegmentStore::new(
            FileSegmentStore::open(temp.path(), config.tokenizer.clone()).unwrap(),
            config.tokenizer.clone(),
            256 * 1024 * 1024,
        ),
        gate: Mutex::new(Some((entered_tx, release_rx))),
    });
    let engine = IndexEngine::from_stores(
        config,
        Arc::new(FileManifestStore::open(temp.path()).unwrap()),
        segments,
        Arc::new(StagedDeleteStore::new(
            FileDeleteStore::open(temp.path()).unwrap(),
        )),
        false,
        SharedSnapshotState::for_index_root(temp.path()).unwrap(),
    )
    .unwrap();
    let overlay = Arc::new(WorkspaceOverlayIndex::new_with_policy(
        engine,
        WorkspaceOverlayPublishPolicy::Background,
    ));
    overlay.ingest_batch(&batch("BEFORE_DISK")).unwrap();
    let maintenance = {
        let o = overlay.clone();
        std::thread::spawn(move || o.run_requested_merge())
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let publication = {
        let o = overlay.clone();
        std::thread::spawn(move || {
            let result = o.ingest_batch(&batch("DURING_DISK"));
            done_tx.send(result).unwrap();
        })
    };
    let published = done_rx.recv_timeout(Duration::from_secs(2));
    // Always release the worker so a regression cannot strand the test process.
    let visible = published
        .as_ref()
        .ok()
        .and_then(|r| r.as_ref().ok())
        .map(|_| {
            overlay
                .open_snapshot()
                .unwrap()
                .search(&query("DURING_DISK"))
                .unwrap()
                .matched_lines
        });
    release_tx.send(()).unwrap();
    publication.join().unwrap();
    maintenance.join().unwrap().unwrap();
    assert!(
        published.is_ok(),
        "disk encoding blocked memory publication"
    );
    published.unwrap().unwrap();
    assert_eq!(visible, Some(1));
    assert_eq!(
        overlay
            .open_snapshot()
            .unwrap()
            .search(&query("DURING_DISK"))
            .unwrap()
            .matched_lines,
        1
    );
    overlay.checkpoint().unwrap();
}

#[test]
fn retired_writer_cannot_publish_over_reopened_engine() {
    let temp = tempdir().unwrap();
    let config = EngineConfig {
        index_root: temp.path().join("index"),
        ..Default::default()
    };
    let old = IndexEngine::open_staged(config.clone()).unwrap();
    let writer = old.writer();
    writer
        .add_document(test_doc("old.rs", "OBSOLETE_CONTENT"))
        .unwrap();
    writer.flush().unwrap();
    let captured = writer.capture_checkpoint();
    writer.retire();
    assert!(!writer.needs_checkpoint());
    assert_eq!(old.admin().vacuum().unwrap(), 0);
    let replacement = IndexEngine::open_staged(config.clone()).unwrap();
    replacement
        .writer()
        .add_document(test_doc("new.rs", "CURRENT_CONTENT"))
        .unwrap();
    replacement.writer().commit().unwrap();
    writer
        .persist_snapshot(&captured.snapshot.manifest)
        .unwrap();
    assert!(!writer.finish_checkpoint(captured).unwrap());
    let reopened = IndexEngine::open_staged(config).unwrap();
    let snapshot = reopened.reader().open_snapshot().unwrap();
    assert_eq!(
        snapshot
            .search(&query("CURRENT_CONTENT"))
            .unwrap()
            .matched_lines,
        1
    );
    assert_eq!(
        snapshot
            .search(&query("OBSOLETE_CONTENT"))
            .unwrap()
            .matched_lines,
        0
    );
}
