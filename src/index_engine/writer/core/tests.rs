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
