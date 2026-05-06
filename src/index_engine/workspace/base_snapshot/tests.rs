#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::{
    base_snapshot_input_files_from_repository_files, estimated_term_entry_bytes,
    merge_sorted_posting_runs, write_sorted_posting_runs, write_spilled_posting_record,
    BaseSnapshotInputFile, PostingBuckets, PostingBucketsState, PostingRecord,
    WorkspaceBaseSnapshotIndex,
};
use crate::base_snapshot_build::BaseSnapshotBuildOptions;
use crate::config::{BuildConfig, CorpusMode, TokenizerMode};
use crate::files::{scan_paths, ScanOptions};
use crate::index_engine::segment::{
    read_postings_file, read_term_dict_file, FileSegmentDocTable, PostingsStreamWriter,
    SegmentDocSourceKind, TermDictStreamWriter,
};
use crate::index_engine::workspace::base_snapshot::reuse::reusable_base_snapshot_shards;
use crate::index_engine::RepoId;
use crate::progress::{IndexProgress, IndexProgressPhase};

#[test]
fn posting_buckets_keep_postings_in_memory_below_threshold() {
    let temp = TempDir::new().expect("temp dir");
    let mut buckets = PostingBuckets::with_memory_threshold(
        temp.path(),
        estimated_term_entry_bytes(&0x00636261) + std::mem::size_of::<u32>() + 64,
    );
    let terms = vec![0x00636261];

    buckets.append_terms(0, &terms).expect("append");

    assert!(matches!(buckets.state, PostingBucketsState::InMemory(_)));
}

#[test]
fn posting_buckets_spill_and_finish_preserves_postings() {
    let temp = TempDir::new().expect("temp dir");
    let mut buckets = PostingBuckets::with_memory_threshold(temp.path(), 1);
    let terms_a = vec![0x00636261, 0x00666564];
    let terms_b = vec![0x00636261];

    buckets.append_terms(0, &terms_a).expect("append");
    buckets.append_terms(1, &terms_b).expect("append");
    assert!(matches!(buckets.state, PostingBucketsState::Spilled(_)));

    let term_dict_path = temp.path().join("term_dict.bin");
    let postings_path = temp.path().join("postings.bin");
    buckets
        .finish(&term_dict_path, &postings_path)
        .expect("finish");

    let keys = read_term_dict_file(&term_dict_path).expect("read term dict");
    let postings = read_postings_file(&postings_path, &keys).expect("read postings");
    assert_eq!(keys.len(), 2);
    assert_eq!(
        postings
            .get(&0x00636261)
            .expect("posting lookup")
            .as_deref(),
        Some(&[0, 1][..])
    );
    assert_eq!(
        postings
            .get(&0x00666564)
            .expect("posting lookup")
            .as_deref(),
        Some(&[0][..])
    );
    assert!(!temp.path().join("posting-buckets").exists());
}

#[test]
fn spilled_posting_runs_merge_preserves_postings_across_multiple_runs() {
    let temp = TempDir::new().expect("temp dir");
    let bucket_path = temp.path().join("bucket-000.bin");
    let mut bucket_writer = std::io::BufWriter::new(
        std::fs::File::create(&bucket_path).expect("bucket create should succeed"),
    );
    for record in [
        PostingRecord {
            term: 0x00666564,
            doc_id: 3,
        },
        PostingRecord {
            term: 0x00636261,
            doc_id: 0,
        },
        PostingRecord {
            term: 0x00666564,
            doc_id: 4,
        },
        PostingRecord {
            term: 0x00636261,
            doc_id: 1,
        },
        PostingRecord {
            term: 0x00636261,
            doc_id: 2,
        },
    ] {
        write_spilled_posting_record(&mut bucket_writer, record)
            .expect("write spilled record should succeed");
    }
    use std::io::Write as _;
    bucket_writer.flush().expect("bucket flush should succeed");

    let run_root = temp.path().join("runs");
    std::fs::create_dir_all(&run_root).expect("run root should succeed");
    let run_paths =
        write_sorted_posting_runs(&bucket_path, &run_root, 0, 2).expect("run split should succeed");
    assert!(run_paths.len() > 1);
    std::fs::remove_file(&bucket_path).expect("bucket remove should succeed");

    let term_dict_path = temp.path().join("term_dict.bin");
    let postings_path = temp.path().join("postings.bin");
    let mut term_dict_writer =
        TermDictStreamWriter::create(&term_dict_path).expect("term dict create should succeed");
    let mut postings_writer =
        PostingsStreamWriter::create(&postings_path).expect("postings create should succeed");
    merge_sorted_posting_runs(&run_paths, &mut term_dict_writer, &mut postings_writer)
        .expect("run merge should succeed");
    term_dict_writer
        .finish()
        .expect("term dict finish should succeed");
    postings_writer
        .finish()
        .expect("postings finish should succeed");

    let keys = read_term_dict_file(&term_dict_path).expect("read term dict");
    let postings = read_postings_file(&postings_path, &keys).expect("read postings");
    assert_eq!(keys.len(), 2);
    assert_eq!(
        postings
            .get(&0x00636261)
            .expect("posting lookup")
            .as_deref(),
        Some(&[0, 1, 2][..])
    );
    assert_eq!(
        postings
            .get(&0x00666564)
            .expect("posting lookup")
            .as_deref(),
        Some(&[3, 4][..])
    );
    assert!(run_paths.iter().all(|path| !path.exists()));
}

fn build_config(repo: &Path, index: &Path) -> BuildConfig {
    BuildConfig {
        repo_path: repo.to_path_buf(),
        storage_root: index.to_path_buf(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::NoIgnore,
        include_hidden: true,
        max_file_size: 1 << 20,
        min_sparse_len: 3,
        max_sparse_len: 8,
    }
}

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git command should launch");
    assert!(
        status.success(),
        "git command failed: git -C {:?} {:?}",
        repo,
        args
    );
}

fn collect_base_input_files(build_config: &BuildConfig) -> Vec<BaseSnapshotInputFile> {
    let files = scan_paths(
        std::slice::from_ref(&build_config.repo_path),
        Some(&build_config.storage_root),
        ScanOptions {
            respect_ignore: false,
            include_hidden: build_config.include_hidden,
            max_file_size: build_config.max_file_size,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )
    .expect("scan should succeed");
    base_snapshot_input_files_from_repository_files(&files, &build_config.repo_path)
}

#[test]
#[cfg(unix)]
fn sharded_base_snapshot_reuses_unchanged_segment_files_via_hard_links() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    let storage = temp.path().join("index");
    std::fs::create_dir_all(&repo).expect("repo dir should succeed");

    let mut shard_paths = std::collections::HashMap::<u32, std::path::PathBuf>::new();
    for index in 0..256 {
        let path = repo.join(format!("file-{index:03}.txt"));
        std::fs::write(&path, format!("TOKEN_{index:03}\n")).expect("write should succeed");
        let shard_id = super::base_snapshot_shard_id(&format!("file-{index:03}.txt"));
        shard_paths.entry(shard_id).or_insert(path);
        if shard_paths.len() >= 2 {
            break;
        }
    }
    assert!(
        shard_paths.len() >= 2,
        "expected at least two non-empty shards"
    );

    let build_config = build_config(&repo, &storage);
    let repo_id = RepoId("repo".into());
    WorkspaceBaseSnapshotIndex::build_with_options(
        &build_config,
        &repo_id,
        BaseSnapshotBuildOptions::default(),
    )
    .expect("initial build should succeed");

    let initial_metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&build_config)
        .expect("metadata read should succeed")
        .expect("metadata should exist");
    assert!(initial_metadata.shards.len() >= 2);

    let changed_shard_id = *shard_paths.keys().min().expect("shard should exist");
    let unchanged_shard_id = *shard_paths
        .keys()
        .filter(|shard_id| **shard_id != changed_shard_id)
        .min()
        .expect("second shard should exist");
    let changed_path = shard_paths
        .get(&changed_shard_id)
        .expect("changed path should exist");
    std::fs::write(changed_path, "CHANGED_TOKEN\n").expect("rewrite should succeed");

    let previous_snapshot = reusable_base_snapshot_shards(&build_config)
        .expect("reusable shards should load")
        .expect("reusable shards should exist");
    let staging_root = temp.path().join("staging");
    let (doc_count, next_shards) = super::build_sharded_base_snapshot(
        &staging_root,
        &build_config,
        &repo_id,
        &repo,
        collect_base_input_files(&build_config),
        Some(&previous_snapshot),
        &mut |_| {},
    )
    .expect("staged build should succeed");
    assert!(doc_count > 0);

    let initial_unchanged = initial_metadata
        .shards
        .iter()
        .find(|shard| shard.shard_id == unchanged_shard_id)
        .expect("initial unchanged shard should exist");
    let next_unchanged = next_shards
        .iter()
        .find(|shard| shard.shard_id == unchanged_shard_id)
        .expect("next unchanged shard should exist");
    assert_eq!(initial_unchanged.segment_id, next_unchanged.segment_id);

    let initial_changed = initial_metadata
        .shards
        .iter()
        .find(|shard| shard.shard_id == changed_shard_id)
        .expect("initial changed shard should exist");
    let next_changed = next_shards
        .iter()
        .find(|shard| shard.shard_id == changed_shard_id)
        .expect("next changed shard should exist");
    assert_ne!(initial_changed.fingerprint, next_changed.fingerprint);
    assert_ne!(initial_changed.segment_id, next_changed.segment_id);

    let source_meta = build_config
        .storage_root
        .join("base-snapshot/segments")
        .join(&initial_unchanged.segment_id)
        .join("SEGMENT_META.json");
    let staged_meta = staging_root
        .join("segments")
        .join(&next_unchanged.segment_id)
        .join("SEGMENT_META.json");
    let source_links = std::fs::metadata(&source_meta)
        .expect("source meta should exist")
        .nlink();
    let staged_links = std::fs::metadata(&staged_meta)
        .expect("staged meta should exist")
        .nlink();
    assert!(
        source_links >= 2,
        "expected hard-linked source segment file"
    );
    assert!(
        staged_links >= 2,
        "expected hard-linked staged segment file"
    );
}

#[test]
fn sharded_base_snapshot_reports_incremental_tokenizing_progress() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    let storage = temp.path().join("index");
    std::fs::create_dir_all(&repo).expect("repo dir should succeed");

    let target_shard_id = 0;
    let mut file_count = 0usize;
    for index in 0..10_000 {
        let file_name = format!("file-{index:05}.txt");
        if super::base_snapshot_shard_id(&file_name) != target_shard_id {
            continue;
        }
        std::fs::write(repo.join(&file_name), format!("TOKEN_{index:05}\n"))
            .expect("write should succeed");
        file_count += 1;
        if file_count >= super::BASE_SNAPSHOT_PROGRESS_REPORT_DOCS + 32 {
            break;
        }
    }
    assert!(
        file_count > super::BASE_SNAPSHOT_PROGRESS_REPORT_DOCS,
        "expected enough files in one shard to trigger incremental progress"
    );

    let build_config = build_config(&repo, &storage);
    let repo_id = RepoId("repo".into());
    let staging_root = temp.path().join("staging");
    let mut events = Vec::<IndexProgress>::new();
    let (_doc_count, shard_metadata) = super::build_sharded_base_snapshot(
        &staging_root,
        &build_config,
        &repo_id,
        &repo,
        collect_base_input_files(&build_config),
        None,
        &mut |event| events.push(event),
    )
    .expect("staged build should succeed");

    let tokenizing_events = events
        .iter()
        .filter(|event| event.phase == IndexProgressPhase::Tokenizing)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !tokenizing_events.is_empty(),
        "expected tokenizing progress events"
    );
    assert_eq!(shard_metadata.len(), 1);
    assert!(tokenizing_events
        .iter()
        .all(|event| event.total == Some(file_count)));
    assert_eq!(
        tokenizing_events.first().map(|event| event.processed),
        Some(0)
    );
    assert!(tokenizing_events.iter().any(|event| {
        event.processed >= super::BASE_SNAPSHOT_PROGRESS_REPORT_DOCS && event.processed < file_count
    }));
    assert_eq!(
        tokenizing_events.last().map(|event| event.processed),
        Some(file_count)
    );
}

#[test]
fn git_base_snapshot_writes_per_doc_source_kind() {
    let temp = TempDir::new().expect("temp dir");
    let repo = temp.path().join("repo");
    let storage = temp.path().join("index");
    std::fs::create_dir_all(&repo).expect("repo dir should succeed");

    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["config", "user.email", "test@example.com"]);
    run_git(&repo, &["config", "user.name", "Test User"]);

    std::fs::write(
        repo.join("clean.rs"),
        "const VALUE: &str = \"CLEAN_BASE\";\n",
    )
    .expect("clean write should succeed");
    std::fs::write(
        repo.join("dirty.rs"),
        "const VALUE: &str = \"DIRTY_BASE\";\n",
    )
    .expect("dirty write should succeed");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-qm", "initial"]);

    std::fs::write(
        repo.join("dirty.rs"),
        "const VALUE: &str = \"DIRTY_WORKTREE\";\n",
    )
    .expect("dirty rewrite should succeed");

    let build_config = build_config(&repo, &storage);
    let repo_id = RepoId("repo".into());
    WorkspaceBaseSnapshotIndex::build_with_options(
        &build_config,
        &repo_id,
        BaseSnapshotBuildOptions::default(),
    )
    .expect("base snapshot build should succeed");

    let metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&build_config)
        .expect("metadata read should succeed")
        .expect("metadata should exist");
    let segments_root = build_config.storage_root.join("base-snapshot/segments");
    let mut source_kinds = std::collections::HashMap::<String, SegmentDocSourceKind>::new();
    for shard in metadata.shards {
        let table = FileSegmentDocTable::open(&segments_root.join(&shard.segment_id))
            .expect("segment table should open");
        for doc_id in 0..table.len() {
            let key = table.doc_key(doc_id).expect("doc key should load");
            if key.logical_path == "clean.rs" || key.logical_path == "dirty.rs" {
                let record = table.record(doc_id).expect("doc record should load");
                source_kinds.insert(key.logical_path, record.source_kind);
            }
        }
    }

    assert_eq!(
        source_kinds.get("clean.rs"),
        Some(&SegmentDocSourceKind::GitHeadBlob)
    );
    assert_eq!(
        source_kinds.get("dirty.rs"),
        Some(&SegmentDocSourceKind::GitHeadBlob)
    );
}
