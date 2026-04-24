use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::BinaryHeap,
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    base_snapshot_build::{BaseSnapshotBuildMode, BaseSnapshotBuildOptions},
    config::{BuildConfig, CorpusMode, TokenizerMode},
    error::AppError,
    files::{repo_relative_path, scan_paths, RepositoryFile, ScanOptions},
    git_snapshot::{
        build_config_fingerprint, canonical_repo_root, dirty_tracked_paths, head_tracked_entries,
        read_git_blob, resolve_git_head, tracked_worktree_files, tracked_worktree_matches_head,
    },
    index_engine::{
        config::{EngineConfig, PostingTokenizerConfig},
        delete::FileDeleteStore,
        manifest::{FileManifestStore, Manifest, ManifestStore, SegmentRef},
        reader::{state::SharedSnapshotState, IndexReaderFactory, IndexSnapshotReader},
        segment::{
            collect_document_posting_terms_from_text, write_path_offsets_file, write_segment_meta,
            FileSegmentStore, PostingsStreamWriter, SegmentDocSourceKind, SegmentMeta,
            TermDictStreamWriter, DOCS_FILE, PATH_MAP_FILE, PATH_OFFSETS_FILE, SEGMENT_META_FILE,
        },
        writer::IndexDocument,
        DocKey, GitRevision, ManifestId, RepoId, SegmentId, ShardId,
    },
    progress::{IndexProgress, IndexProgressPhase},
    Result,
};

const BASE_SNAPSHOT_BUILD_STAGING_PREFIX: &str = "base-snapshot.build-";
const BASE_SNAPSHOT_BUILD_STALE_PREFIX: &str = "base-snapshot.stale-";
const BASE_SNAPSHOT_PREPARE_CHUNK_DOCS: usize = 1024;
const BASE_SNAPSHOT_PREPARE_PARALLEL_DOC_THRESHOLD: usize = 64;
const BASE_SNAPSHOT_PREPARE_PARALLEL_BYTE_THRESHOLD: u64 = 8 * 1024 * 1024;
const BASE_SNAPSHOT_PROGRESS_REPORT_DOCS: usize = 64;
const BASE_SNAPSHOT_POSTING_BUCKET_COUNT: usize = 64;
const BASE_SNAPSHOT_IN_MEMORY_POSTING_THRESHOLD_BYTES: usize = 128 * 1024 * 1024;
const BASE_SNAPSHOT_SHARD_COUNT: usize = 16;
const BASE_SNAPSHOT_SPILLED_BUCKET_SORT_MAX_RECORDS_PER_RUN: usize = 512 * 1024;
const BASE_SNAPSHOT_MANIFEST_ID: &str = "manifest-0000000000000001";
const SEGMENT_SCHEMA_FINGERPRINT: &str = "segment-publish-v3";
const DOCS_MAGIC: &[u8; 8] = b"CGDOCS01";
const DOCS_VERSION: u32 = 4;
const PATH_MAP_MAGIC: &[u8; 8] = b"CGPATH01";
const SEGMENT_FILE_VERSION: u32 = 1;
const COUNT_OFFSET: u64 = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBaseSnapshotMetadata {
    pub snapshot_key: String,
    pub revision: Option<GitRevision>,
    pub tokenizer: Option<TokenizerMode>,
    pub doc_count: Option<usize>,
    pub head_commit: Option<String>,
    pub config_fingerprint: Option<String>,
    pub repo_root: Option<String>,
    pub shards: Vec<WorkspaceBaseSnapshotShardMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBaseSnapshotShardMetadata {
    pub shard_id: u32,
    pub fingerprint: String,
    pub segment_id: String,
    pub doc_count: usize,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceBaseSnapshotMetadataFile {
    snapshot_key: String,
    #[serde(default)]
    revision: Option<GitRevision>,
    #[serde(default)]
    tokenizer: Option<String>,
    #[serde(default)]
    doc_count: Option<usize>,
    #[serde(default)]
    head_commit: Option<String>,
    #[serde(default)]
    config_fingerprint: Option<String>,
    #[serde(default)]
    repo_root: Option<String>,
    #[serde(default)]
    shards: Vec<WorkspaceBaseSnapshotShardMetadataFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceBaseSnapshotShardMetadataFile {
    shard_id: u32,
    fingerprint: String,
    segment_id: String,
    doc_count: usize,
    size_bytes: u64,
}

pub struct WorkspaceBaseSnapshotIndex {
    reader: Arc<IndexReaderFactory>,
    committed_reader: OnceLock<IndexSnapshotReader>,
}

pub(crate) struct WorkspaceBaseSnapshotBuildResult {
    pub(crate) indexed_docs: usize,
    pub(crate) metadata: WorkspaceBaseSnapshotMetadata,
    #[allow(dead_code)]
    pub(crate) reused_existing: bool,
}

struct ResolvedBaseSnapshotBuild {
    snapshot_key: String,
    head_commit: Option<String>,
    config_fingerprint: String,
}

#[derive(Clone)]
struct BaseSnapshotInputFile {
    path: PathBuf,
    logical_path: String,
    size: u64,
    mtime_nanos: u64,
    source: BaseSnapshotFileSource,
    fingerprint: BaseSnapshotFileFingerprint,
}

#[derive(Clone)]
enum BaseSnapshotFileSource {
    Worktree,
    GitHeadBlob { head_commit: String },
}

#[derive(Clone)]
enum BaseSnapshotFileFingerprint {
    Worktree { mtime_nanos: u64 },
    GitObject { object_id: String, size: u64 },
}

#[derive(Clone)]
struct BaseSnapshotShardBuild {
    shard_id: u32,
    fingerprint: String,
    files: Vec<BaseSnapshotInputFile>,
}

struct BulkBaseSegmentWriter {
    segment_dir: PathBuf,
    segment_id: SegmentId,
    tokenizer: PostingTokenizerConfig,
    path_map: BufWriter<File>,
    path_entry_offsets: Vec<u64>,
    path_map_len: u64,
    docs: BufWriter<File>,
    posting_buckets: PostingBuckets,
    doc_count: u32,
    total_source_bytes: u64,
}

struct PreparedBaseDocument {
    document: IndexDocument,
    posting_terms: Vec<u64>,
    source_kind: SegmentDocSourceKind,
}

struct ShardBuildOutcome {
    segment_ref: Option<SegmentRef>,
    shard_metadata: Option<WorkspaceBaseSnapshotShardMetadata>,
    doc_count: usize,
}

enum ShardBuildMessage {
    Progress {
        shard_id: u32,
        processed_files: usize,
    },
    Finished {
        shard_id: u32,
        total_files: usize,
        outcome: Result<ShardBuildOutcome>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PostingRecord {
    term: u64,
    doc_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PostingRunHead {
    record: PostingRecord,
    run_index: usize,
}

struct PostingBuckets {
    state: PostingBucketsState,
}

enum PostingBucketsState {
    InMemory(InMemoryPostingBuckets),
    Spilled(SpilledPostingBuckets),
}

struct InMemoryPostingBuckets {
    spill_root: PathBuf,
    memory_threshold_bytes: usize,
    approximate_bytes: usize,
    buckets: Vec<HashMap<u64, Vec<u32>>>,
}

struct SpilledPostingBuckets {
    bucket_dir: PathBuf,
    bucket_paths: Vec<PathBuf>,
    writers: Vec<BufWriter<File>>,
}

impl BulkBaseSegmentWriter {
    fn new(
        index_root: &Path,
        segment_id: SegmentId,
        tokenizer: PostingTokenizerConfig,
    ) -> Result<Self> {
        let segment_dir = index_root.join("segments").join(&segment_id.0);
        fs::create_dir_all(&segment_dir)?;

        let mut path_map = BufWriter::new(File::create(segment_dir.join(PATH_MAP_FILE))?);
        let mut docs = BufWriter::new(File::create(segment_dir.join(DOCS_FILE))?);
        let posting_buckets = PostingBuckets::new(&segment_dir);

        write_segment_header(&mut path_map, PATH_MAP_MAGIC)?;
        write_segment_header_with_version(&mut docs, DOCS_MAGIC, DOCS_VERSION)?;

        Ok(Self {
            segment_dir,
            segment_id,
            tokenizer,
            path_map,
            path_entry_offsets: Vec::new(),
            path_map_len: COUNT_OFFSET + 4,
            docs,
            posting_buckets,
            doc_count: 0,
            total_source_bytes: 0,
        })
    }

    fn add_document(&mut self, prepared: PreparedBaseDocument) -> Result<()> {
        let doc_id = self.doc_count;
        let PreparedBaseDocument {
            document,
            posting_terms,
            source_kind,
        } = prepared;

        self.path_entry_offsets.push(self.path_map_len);
        write_string_to(&mut self.path_map, &document.repo_id.0)?;
        write_string_to(&mut self.path_map, &document.logical_path)?;
        let repo_id_len = encoded_string_len(&document.repo_id.0)?;
        let logical_path_len = encoded_string_len(&document.logical_path)?;
        self.path_map_len = self
            .path_map_len
            .checked_add(repo_id_len)
            .and_then(|len| len.checked_add(logical_path_len))
            .ok_or_else(|| AppError::ValueOutOfRange("path map offset overflow".into()))?;

        write_u32_to(&mut self.docs, doc_id)?;
        write_u64_to(&mut self.docs, 0)?;
        write_u32_to(&mut self.docs, 0)?;
        write_u64_to(&mut self.docs, document.size)?;
        write_u64_to(&mut self.docs, document.modified_at)?;
        self.docs.write_all(&document.content_hash)?;
        self.docs.write_all(&[source_kind as u8])?;
        self.docs.write_all(&[0u8; 7])?;

        self.posting_buckets.append_terms(doc_id, &posting_terms)?;

        self.total_source_bytes = self
            .total_source_bytes
            .saturating_add(document.source_bytes.len() as u64);
        self.doc_count = self.doc_count.checked_add(1).ok_or_else(|| {
            AppError::ValueOutOfRange("base snapshot doc count exceeds u32 range".into())
        })?;
        Ok(())
    }

    fn finish(mut self) -> Result<Option<SegmentRef>> {
        patch_segment_count(&mut self.path_map, self.doc_count)?;
        patch_segment_count(&mut self.docs, self.doc_count)?;

        self.path_map.flush()?;
        self.docs.flush()?;

        if self.doc_count == 0 {
            fs::remove_dir_all(&self.segment_dir)?;
            return Ok(None);
        }

        let segment_dir = self.segment_dir.clone();
        let segment_id = self.segment_id.clone();
        let doc_count = self.doc_count;
        let total_source_bytes = self.total_source_bytes;
        let meta = SegmentMeta {
            segment_id: segment_id.clone(),
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count,
            live_doc_count: doc_count,
            created_by_merge: false,
        };
        let posting_buckets = self.posting_buckets;
        posting_buckets.finish(
            &segment_dir.join("term_dict.bin"),
            &segment_dir.join("postings.bin"),
        )?;
        write_path_offsets_file(
            &segment_dir.join(PATH_OFFSETS_FILE),
            &self.path_entry_offsets,
        )?;
        write_segment_meta(&segment_dir.join(SEGMENT_META_FILE), &meta)?;

        Ok(Some(SegmentRef {
            id: segment_id,
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count,
            size_bytes: total_source_bytes,
        }))
    }
}

impl PostingBuckets {
    fn new(index_root: &Path) -> Self {
        Self {
            state: PostingBucketsState::InMemory(InMemoryPostingBuckets::new(
                index_root,
                BASE_SNAPSHOT_IN_MEMORY_POSTING_THRESHOLD_BYTES,
            )),
        }
    }

    #[cfg(test)]
    fn with_memory_threshold(index_root: &Path, memory_threshold_bytes: usize) -> Self {
        Self {
            state: PostingBucketsState::InMemory(InMemoryPostingBuckets::new(
                index_root,
                memory_threshold_bytes,
            )),
        }
    }

    fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) -> Result<()> {
        match &mut self.state {
            PostingBucketsState::InMemory(buckets) => {
                buckets.append_terms(doc_id, posting_terms);
                if buckets.should_spill() {
                    let mut spilled = SpilledPostingBuckets::new(&buckets.spill_root)?;
                    buckets.drain_into_spilled(&mut spilled)?;
                    self.state = PostingBucketsState::Spilled(spilled);
                }
                Ok(())
            }
            PostingBucketsState::Spilled(buckets) => buckets.append_terms(doc_id, posting_terms),
        }
    }

    fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        match self.state {
            PostingBucketsState::InMemory(buckets) => buckets.finish(term_dict_path, postings_path),
            PostingBucketsState::Spilled(buckets) => buckets.finish(term_dict_path, postings_path),
        }
    }
}

impl InMemoryPostingBuckets {
    fn new(index_root: &Path, memory_threshold_bytes: usize) -> Self {
        let mut buckets = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        for _ in 0..BASE_SNAPSHOT_POSTING_BUCKET_COUNT {
            buckets.push(HashMap::new());
        }
        Self {
            spill_root: index_root.to_path_buf(),
            memory_threshold_bytes,
            approximate_bytes: 0,
            buckets,
        }
    }

    fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) {
        for term in posting_terms {
            let bucket_id = posting_bucket_id(*term);
            let bucket = &mut self.buckets[bucket_id];
            let entry = bucket.entry(*term).or_insert_with(|| {
                self.approximate_bytes = self
                    .approximate_bytes
                    .saturating_add(estimated_term_entry_bytes(term));
                Vec::new()
            });
            entry.push(doc_id);
            self.approximate_bytes = self
                .approximate_bytes
                .saturating_add(std::mem::size_of::<u32>());
        }
    }

    fn should_spill(&self) -> bool {
        self.approximate_bytes >= self.memory_threshold_bytes
    }

    fn drain_into_spilled(&mut self, spilled: &mut SpilledPostingBuckets) -> Result<()> {
        for bucket in &mut self.buckets {
            for (term, doc_ids) in bucket.drain() {
                for doc_id in doc_ids {
                    spilled.append_term_doc_id(term, doc_id)?;
                }
            }
        }
        self.approximate_bytes = 0;
        Ok(())
    }

    fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        let mut term_dict_writer = TermDictStreamWriter::create(term_dict_path)?;
        let mut postings_writer = PostingsStreamWriter::create(postings_path)?;

        for bucket in self.buckets {
            let mut entries = bucket.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (key, doc_ids) in entries {
                let postings_offset = postings_writer.push_doc_ids(&doc_ids)?;
                term_dict_writer.push_entry(key, postings_offset)?;
            }
        }

        term_dict_writer.finish()?;
        postings_writer.finish()?;
        Ok(())
    }
}

impl SpilledPostingBuckets {
    fn new(index_root: &Path) -> Result<Self> {
        let bucket_dir = index_root.join("posting-buckets");
        fs::create_dir_all(&bucket_dir)?;
        let mut bucket_paths = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        let mut writers = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        for bucket_id in 0..BASE_SNAPSHOT_POSTING_BUCKET_COUNT {
            let path = bucket_dir.join(format!("bucket-{bucket_id:03}.bin"));
            bucket_paths.push(path.clone());
            writers.push(BufWriter::new(File::create(path)?));
        }
        Ok(Self {
            bucket_dir,
            bucket_paths,
            writers,
        })
    }

    fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) -> Result<()> {
        for term in posting_terms {
            self.append_term_doc_id(*term, doc_id)?;
        }
        Ok(())
    }

    fn append_term_doc_id(&mut self, term: u64, doc_id: u32) -> Result<()> {
        let bucket_id = posting_bucket_id(term);
        let writer = self
            .writers
            .get_mut(bucket_id)
            .ok_or_else(|| AppError::InvalidIndex(format!("missing posting bucket {bucket_id}")))?;
        write_spilled_posting_record(writer, PostingRecord { term, doc_id })?;
        Ok(())
    }

    fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        let SpilledPostingBuckets {
            bucket_dir,
            bucket_paths,
            mut writers,
        } = self;

        for writer in &mut writers {
            writer.flush()?;
        }
        drop(writers);

        let mut term_dict_writer = TermDictStreamWriter::create(term_dict_path)?;
        let mut postings_writer = PostingsStreamWriter::create(postings_path)?;
        let run_root = bucket_dir.join("runs");
        fs::create_dir_all(&run_root)?;

        for (bucket_id, bucket_path) in bucket_paths.iter().enumerate() {
            if fs::metadata(bucket_path).map_or(true, |metadata| metadata.len() == 0) {
                let _ = fs::remove_file(bucket_path);
                continue;
            }
            let run_paths = write_sorted_posting_runs(
                bucket_path,
                &run_root,
                bucket_id,
                BASE_SNAPSHOT_SPILLED_BUCKET_SORT_MAX_RECORDS_PER_RUN,
            )?;
            fs::remove_file(bucket_path)?;
            merge_sorted_posting_runs(&run_paths, &mut term_dict_writer, &mut postings_writer)?;
        }

        term_dict_writer.finish()?;
        postings_writer.finish()?;
        let _ = fs::remove_dir_all(bucket_dir);
        Ok(())
    }
}

impl WorkspaceBaseSnapshotIndex {
    pub(crate) fn storage_root(build_config: &BuildConfig) -> PathBuf {
        build_config.storage_root.clone()
    }

    pub(crate) fn index_root(build_config: &BuildConfig) -> PathBuf {
        Self::storage_root(build_config).join("base-snapshot")
    }

    pub(crate) fn marker_path(build_config: &BuildConfig) -> PathBuf {
        Self::marker_path_for_root(&Self::index_root(build_config))
    }

    pub(crate) fn metadata_path(build_config: &BuildConfig) -> PathBuf {
        Self::metadata_path_for_root(&Self::index_root(build_config))
    }

    pub fn open(build_config: &BuildConfig) -> Result<Self> {
        Self::open_read_only(build_config)
    }

    pub fn open_read_only(build_config: &BuildConfig) -> Result<Self> {
        let config = Self::engine_config(build_config);
        let base_head_commit =
            Self::read_metadata_file(build_config)?.and_then(|metadata| metadata.head_commit);
        let reader = Arc::new(IndexReaderFactory::new(
            Arc::new(FileManifestStore::open(&config.index_root)?),
            Arc::new(FileSegmentStore::open_with_repo_root(
                &config.index_root,
                config.tokenizer.clone(),
                Some(build_config.repo_path.clone()),
                base_head_commit,
            )?),
            Arc::new(FileDeleteStore::open(&config.index_root)?),
            SharedSnapshotState::new(),
            config.tokenizer.clone(),
        ));
        Ok(Self {
            reader,
            committed_reader: OnceLock::new(),
        })
    }

    pub(crate) fn marker_matches(
        build_config: &BuildConfig,
        expected_snapshot_key: &str,
    ) -> Result<bool> {
        Ok(Self::read_marker(build_config)?.as_deref() == Some(expected_snapshot_key))
    }

    pub(crate) fn open_if_matching(
        build_config: &BuildConfig,
        expected_snapshot_key: &str,
    ) -> Result<Option<Self>> {
        if !build_config.repo_path.exists() {
            return Ok(None);
        }

        if !Self::marker_matches(build_config, expected_snapshot_key)? {
            return Ok(None);
        }

        let index = Self::open_read_only(build_config)?;
        if !Self::marker_matches(build_config, expected_snapshot_key)? {
            return Ok(None);
        }

        Ok(Some(index))
    }

    pub(crate) fn read_marker(build_config: &BuildConfig) -> Result<Option<String>> {
        match fs::read_to_string(Self::marker_path(build_config)) {
            Ok(contents) => {
                let snapshot_key = contents.trim().to_string();
                if snapshot_key.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(snapshot_key))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn read_snapshot_metadata(
        build_config: &BuildConfig,
    ) -> Result<Option<WorkspaceBaseSnapshotMetadata>> {
        if let Some(metadata) = Self::read_metadata_file(build_config)? {
            return Ok(Some(metadata));
        }

        Ok(
            Self::read_marker(build_config)?.map(|snapshot_key| WorkspaceBaseSnapshotMetadata {
                revision: parse_snapshot_revision(&snapshot_key),
                snapshot_key,
                tokenizer: None,
                doc_count: None,
                head_commit: None,
                config_fingerprint: None,
                repo_root: None,
                shards: Vec::new(),
            }),
        )
    }

    pub(crate) fn write_marker(build_config: &BuildConfig, snapshot_key: &str) -> Result<()> {
        write_marker_for_root(&Self::index_root(build_config), snapshot_key)
    }

    pub(crate) fn build_with_options(
        build_config: &BuildConfig,
        repo_id: &RepoId,
        mut options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<WorkspaceBaseSnapshotBuildResult> {
        let mut noop_progress = |_event: IndexProgress| {};
        let mut noop_cancel = || false;
        let on_progress = options.progress.take().unwrap_or(&mut noop_progress);
        let should_cancel = options.should_cancel.take().unwrap_or(&mut noop_cancel);
        Self::build_internal(
            build_config,
            repo_id,
            options.build_mode,
            on_progress,
            should_cancel,
        )
    }

    pub(crate) fn rebuild_with_options(
        build_config: &BuildConfig,
        repo_id: &RepoId,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<WorkspaceBaseSnapshotBuildResult> {
        Self::build_with_options(
            build_config,
            repo_id,
            BaseSnapshotBuildOptions {
                build_mode: BaseSnapshotBuildMode::ForceRebuildSnapshot,
                ..options
            },
        )
    }

    pub(crate) fn rebuild_for_snapshot_identity(
        build_config: &BuildConfig,
        repo_id: &RepoId,
        snapshot_key: &str,
        head_commit: Option<&str>,
        on_progress: &mut impl FnMut(IndexProgress),
        should_cancel: &mut impl FnMut() -> bool,
    ) -> Result<Self> {
        cleanup_unpublished_base_snapshot_builds(build_config)?;
        let staging_root = next_staging_root(build_config);
        fs::create_dir_all(&staging_root)?;
        let previous_snapshot = reusable_base_snapshot_shards(build_config)?;

        let (doc_count, shards) = if let Some(head_commit) = head_commit {
            check_cancel(should_cancel)?;
            if tracked_worktree_matches_head(&build_config.repo_path, should_cancel)? {
                let files = tracked_worktree_files(
                    &build_config.repo_path,
                    build_config.include_hidden,
                    build_config.max_file_size,
                    should_cancel,
                )?;
                build_sharded_base_snapshot(
                    &staging_root,
                    build_config,
                    repo_id,
                    &build_config.repo_path,
                    base_snapshot_input_files_from_repository_files(
                        &files,
                        &build_config.repo_path,
                    ),
                    previous_snapshot.as_ref(),
                    on_progress,
                )?
            } else {
                let files = build_head_snapshot_input_files(
                    &build_config.repo_path,
                    head_commit,
                    build_config.include_hidden,
                    build_config.max_file_size,
                    should_cancel,
                )?;
                build_sharded_base_snapshot(
                    &staging_root,
                    build_config,
                    repo_id,
                    &build_config.repo_path,
                    files,
                    previous_snapshot.as_ref(),
                    on_progress,
                )?
            }
        } else {
            let files = scan_paths(
                std::slice::from_ref(&build_config.repo_path),
                Some(&build_config.storage_root),
                ScanOptions {
                    respect_ignore: matches!(build_config.corpus_mode, CorpusMode::RespectIgnore),
                    include_hidden: build_config.include_hidden,
                    max_file_size: build_config.max_file_size,
                    max_depth: None,
                    ignore_files: Vec::new(),
                },
                None,
            )?;
            build_sharded_base_snapshot(
                &staging_root,
                build_config,
                repo_id,
                &build_config.repo_path,
                base_snapshot_input_files_from_repository_files(&files, &build_config.repo_path),
                previous_snapshot.as_ref(),
                on_progress,
            )?
        };

        write_marker_for_root(&staging_root, snapshot_key)?;
        write_metadata_file_for_root(
            &staging_root,
            &WorkspaceBaseSnapshotMetadata {
                snapshot_key: snapshot_key.to_string(),
                revision: head_commit.map(|commit| GitRevision(commit.to_string())),
                tokenizer: Some(build_config.tokenizer),
                doc_count: Some(doc_count),
                head_commit: head_commit.map(str::to_string),
                config_fingerprint: Some(base_snapshot_config_fingerprint(build_config)),
                repo_root: Some(canonical_repo_root(&build_config.repo_path)),
                shards,
            },
        )?;
        report_progress(
            on_progress,
            IndexProgressPhase::Writing,
            "Publishing base snapshot",
            2,
            Some(3),
        );
        publish_staged_base_snapshot(build_config, &staging_root)?;
        report_progress(
            on_progress,
            IndexProgressPhase::Writing,
            "Published base snapshot",
            3,
            Some(3),
        );
        Self::open_read_only(build_config)
    }

    pub fn open_committed(&self) -> Result<IndexSnapshotReader> {
        if let Some(reader) = self.committed_reader.get() {
            return Ok(reader.clone());
        }

        let reader = self.reader.open_persisted_current()?;
        reader.prewarm_search_state()?;
        if self.committed_reader.set(reader.clone()).is_err() {
            let cached = self.committed_reader.get().ok_or_else(|| {
                AppError::Protocol(
                    "failed to initialize cached committed base snapshot reader".into(),
                )
            })?;
            return Ok(cached.clone());
        }

        let cached = self.committed_reader.get().ok_or_else(|| {
            AppError::Protocol("failed to read cached committed base snapshot reader".into())
        })?;
        Ok(cached.clone())
    }

    fn engine_config(build_config: &BuildConfig) -> EngineConfig {
        EngineConfig {
            shard_id: ShardId(format!(
                "{}#base-snapshot",
                build_config.repo_path.to_string_lossy()
            )),
            index_root: Self::index_root(build_config),
            tokenizer: PostingTokenizerConfig::from(build_config),
            ..EngineConfig::default()
        }
    }

    fn read_metadata_file(
        build_config: &BuildConfig,
    ) -> Result<Option<WorkspaceBaseSnapshotMetadata>> {
        match fs::read(Self::metadata_path(build_config)) {
            Ok(bytes) => {
                let metadata = serde_json::from_slice::<WorkspaceBaseSnapshotMetadataFile>(&bytes)
                    .map_err(|error| {
                        AppError::InvalidIndex(format!(
                            "failed to decode base snapshot metadata for {}: {error}",
                            build_config.repo_path.display()
                        ))
                    })?;
                Ok(Some(WorkspaceBaseSnapshotMetadata {
                    revision: metadata
                        .revision
                        .or_else(|| parse_snapshot_revision(&metadata.snapshot_key)),
                    snapshot_key: metadata.snapshot_key,
                    tokenizer: metadata.tokenizer.as_deref().and_then(parse_tokenizer_mode),
                    doc_count: metadata.doc_count,
                    head_commit: metadata.head_commit,
                    config_fingerprint: metadata.config_fingerprint,
                    repo_root: metadata.repo_root,
                    shards: metadata
                        .shards
                        .into_iter()
                        .map(|shard| WorkspaceBaseSnapshotShardMetadata {
                            shard_id: shard.shard_id,
                            fingerprint: shard.fingerprint,
                            segment_id: shard.segment_id,
                            doc_count: shard.doc_count,
                            size_bytes: shard.size_bytes,
                        })
                        .collect(),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write_metadata_file(
        build_config: &BuildConfig,
        metadata: &WorkspaceBaseSnapshotMetadata,
    ) -> Result<()> {
        write_metadata_file_for_root(&Self::index_root(build_config), metadata)
    }

    fn marker_path_for_root(index_root: &Path) -> PathBuf {
        index_root.join("BASE_SNAPSHOT_KEY")
    }

    fn metadata_path_for_root(index_root: &Path) -> PathBuf {
        index_root.join("BASE_SNAPSHOT_META.json")
    }

    fn build_internal<F, C>(
        build_config: &BuildConfig,
        repo_id: &RepoId,
        build_mode: BaseSnapshotBuildMode,
        mut on_progress: F,
        mut should_cancel: C,
    ) -> Result<WorkspaceBaseSnapshotBuildResult>
    where
        F: FnMut(IndexProgress),
        C: FnMut() -> bool,
    {
        let build_config = build_config.normalized()?;
        check_cancel(&mut should_cancel)?;
        report_progress(
            &mut on_progress,
            IndexProgressPhase::Scanning,
            "Resolving base snapshot",
            0,
            None,
        );
        let resolved = resolve_base_snapshot_build(&build_config, &mut should_cancel)?;

        if build_mode == BaseSnapshotBuildMode::ReuseExistingSnapshot {
            if let Some(metadata) = reusable_snapshot_metadata(&build_config, &resolved)? {
                check_cancel(&mut should_cancel)?;
                report_progress(
                    &mut on_progress,
                    IndexProgressPhase::Finalizing,
                    "Reused existing base snapshot shard",
                    1,
                    Some(1),
                );
                return Ok(WorkspaceBaseSnapshotBuildResult {
                    indexed_docs: metadata.doc_count.unwrap_or_default(),
                    metadata,
                    reused_existing: true,
                });
            }
        }

        check_cancel(&mut should_cancel)?;
        report_progress(
            &mut on_progress,
            IndexProgressPhase::Scanning,
            if resolved.head_commit.is_some() {
                "Preparing HEAD-aligned files for base shard"
            } else {
                "Scanning repository files for base shard"
            },
            0,
            None,
        );
        Self::rebuild_for_snapshot_identity(
            &build_config,
            repo_id,
            &resolved.snapshot_key,
            resolved.head_commit.as_deref(),
            &mut on_progress,
            &mut should_cancel,
        )?;
        let metadata = Self::read_snapshot_metadata(&build_config)?
            .ok_or_else(|| AppError::InvalidIndex("base snapshot metadata is missing".into()))?;
        report_progress(
            &mut on_progress,
            IndexProgressPhase::Finalizing,
            "Published base snapshot shard",
            1,
            Some(1),
        );
        Ok(WorkspaceBaseSnapshotBuildResult {
            indexed_docs: metadata.doc_count.unwrap_or_default(),
            metadata,
            reused_existing: false,
        })
    }
}

struct ReusableBaseSnapshotShards {
    index_root: PathBuf,
    manifest_segments: HashMap<String, SegmentRef>,
    shards_by_id: HashMap<u32, WorkspaceBaseSnapshotShardMetadata>,
}

fn reusable_base_snapshot_shards(
    build_config: &BuildConfig,
) -> Result<Option<ReusableBaseSnapshotShards>> {
    let Some(metadata) = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(build_config)? else {
        return Ok(None);
    };
    if metadata.shards.is_empty()
        || metadata.tokenizer != Some(build_config.tokenizer)
        || metadata.config_fingerprint.as_deref()
            != Some(&base_snapshot_config_fingerprint(build_config))
        || metadata.repo_root.as_deref() != Some(&canonical_repo_root(&build_config.repo_path))
    {
        return Ok(None);
    }

    let index_root = WorkspaceBaseSnapshotIndex::index_root(build_config);
    let manifest = FileManifestStore::open(&index_root)?.load_current()?;
    let manifest_segments = manifest
        .active_segments
        .into_iter()
        .map(|segment| (segment.id.0.clone(), segment))
        .collect::<HashMap<_, _>>();
    let shards_by_id = metadata
        .shards
        .into_iter()
        .map(|shard| (shard.shard_id, shard))
        .collect::<HashMap<_, _>>();
    Ok(Some(ReusableBaseSnapshotShards {
        index_root,
        manifest_segments,
        shards_by_id,
    }))
}

fn build_sharded_base_snapshot(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    files: Vec<BaseSnapshotInputFile>,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
    on_progress: &mut impl FnMut(IndexProgress),
) -> Result<(usize, Vec<WorkspaceBaseSnapshotShardMetadata>)> {
    fs::create_dir_all(index_root)?;
    let _ = FileDeleteStore::open(index_root)?;
    let shard_builds = partition_base_snapshot_shards(files);
    let shard_total = shard_builds.len();
    let total_files = shard_builds
        .iter()
        .map(|shard| shard.files.len())
        .sum::<usize>();
    let tokenizing_progress_total = total_files.max(1);
    report_progress(
        on_progress,
        IndexProgressPhase::Tokenizing,
        if total_files == 0 {
            "No base snapshot shards to build".into()
        } else {
            format!("Tokenizing {total_files} base snapshot file(s) across {shard_total} shard(s)")
        },
        usize::from(total_files == 0),
        Some(tokenizing_progress_total),
    );
    let mut outcomes = Vec::with_capacity(shard_total);
    if shard_total > 0 {
        let (tx, rx) = mpsc::channel();
        let mut shard_processed = HashMap::<u32, usize>::new();
        let mut completed_shards = 0usize;
        std::thread::scope(|scope| -> Result<()> {
            for shard in shard_builds {
                let shard_id = shard.shard_id;
                let total_files = shard.files.len();
                let tx = tx.clone();
                scope.spawn(move || {
                    let progress_tx = tx.clone();
                    let report_shard_progress = move |processed_files| {
                        let _ = progress_tx.send(ShardBuildMessage::Progress {
                            shard_id,
                            processed_files,
                        });
                    };
                    let outcome = build_or_reuse_base_snapshot_shard(
                        index_root,
                        build_config,
                        repo_id,
                        repo_root,
                        shard,
                        previous_snapshot,
                        &report_shard_progress,
                    );
                    let _ = tx.send(ShardBuildMessage::Finished {
                        shard_id,
                        total_files,
                        outcome,
                    });
                });
            }
            drop(tx);

            while completed_shards < shard_total {
                match rx.recv().map_err(|_| {
                    AppError::Protocol(
                        "base snapshot shard worker exited before sending outcome".into(),
                    )
                })? {
                    ShardBuildMessage::Progress {
                        shard_id,
                        processed_files,
                    } => {
                        let current = shard_processed.entry(shard_id).or_default();
                        if processed_files <= *current {
                            continue;
                        }
                        *current = processed_files;
                        let global_processed = shard_processed.values().sum::<usize>();
                        report_progress(
                            on_progress,
                            IndexProgressPhase::Tokenizing,
                            format!(
                                "Tokenized {global_processed} of {total_files} base snapshot file(s) ({completed_shards} of {shard_total} shards complete)"
                            ),
                            global_processed,
                            Some(tokenizing_progress_total),
                        );
                    }
                    ShardBuildMessage::Finished {
                        shard_id,
                        total_files: shard_total_files,
                        outcome,
                    } => {
                        completed_shards += 1;
                        let current = shard_processed.entry(shard_id).or_default();
                        *current = (*current).max(shard_total_files);
                        let global_processed = shard_processed.values().sum::<usize>();
                        outcomes.push(outcome);
                        report_progress(
                            on_progress,
                            IndexProgressPhase::Tokenizing,
                            format!(
                                "Tokenized {global_processed} of {total_files} base snapshot file(s) ({completed_shards} of {shard_total} shards complete)"
                            ),
                            global_processed,
                            Some(tokenizing_progress_total),
                        );
                    }
                }
            }
            Ok(())
        })?;
    }

    let mut active_segments = Vec::new();
    let mut shard_metadata = Vec::new();
    let mut doc_count = 0usize;
    for outcome in outcomes {
        let outcome = outcome?;
        doc_count = doc_count.saturating_add(outcome.doc_count);
        if let Some(segment_ref) = outcome.segment_ref {
            active_segments.push(segment_ref);
        }
        if let Some(metadata) = outcome.shard_metadata {
            shard_metadata.push(metadata);
        }
    }

    active_segments.sort_unstable_by(|left, right| left.id.0.cmp(&right.id.0));
    report_progress(
        on_progress,
        IndexProgressPhase::Writing,
        format!(
            "Writing base snapshot shard manifest for {} segment(s)",
            active_segments.len()
        ),
        0,
        Some(3),
    );
    let committed_seq_no = u64::from(!active_segments.is_empty());
    let manifest_store = FileManifestStore::open(index_root)?;
    manifest_store.publish(&Manifest {
        manifest_id: ManifestId(BASE_SNAPSHOT_MANIFEST_ID.into()),
        generation: 1,
        created_at_unix_ms: now_unix_ms(),
        committed_seq_no,
        refreshed_seq_no: committed_seq_no,
        writer_epoch: 0,
        schema_fingerprint: SEGMENT_SCHEMA_FINGERPRINT.into(),
        active_segments,
        active_delete_segments: Vec::new(),
    })?;
    report_progress(
        on_progress,
        IndexProgressPhase::Writing,
        "Writing base snapshot metadata",
        1,
        Some(3),
    );

    shard_metadata.sort_unstable_by_key(|shard| shard.shard_id);
    Ok((doc_count, shard_metadata))
}

fn build_or_reuse_base_snapshot_shard(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    shard: BaseSnapshotShardBuild,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
    on_progress: &(impl Fn(usize) + Sync),
) -> Result<ShardBuildOutcome> {
    if let Some(reused) = maybe_link_reusable_base_snapshot_shard(
        index_root,
        shard.shard_id,
        &shard.fingerprint,
        previous_snapshot,
    )? {
        on_progress(shard.files.len());
        return Ok(ShardBuildOutcome {
            segment_ref: Some(reused.segment_ref.clone()),
            shard_metadata: Some(WorkspaceBaseSnapshotShardMetadata {
                shard_id: shard.shard_id,
                fingerprint: shard.fingerprint,
                segment_id: reused.segment_ref.id.0,
                doc_count: reused.doc_count,
                size_bytes: reused.segment_ref.size_bytes,
            }),
            doc_count: reused.doc_count,
        });
    }

    let segment_id = base_snapshot_segment_id(shard.shard_id, &shard.fingerprint);
    let Some(segment_ref) = build_base_snapshot_segment(
        index_root,
        build_config,
        repo_id,
        repo_root,
        &shard.files,
        segment_id,
        on_progress,
    )?
    else {
        return Ok(ShardBuildOutcome {
            segment_ref: None,
            shard_metadata: None,
            doc_count: 0,
        });
    };

    Ok(ShardBuildOutcome {
        doc_count: segment_ref.doc_count as usize,
        shard_metadata: Some(WorkspaceBaseSnapshotShardMetadata {
            shard_id: shard.shard_id,
            fingerprint: shard.fingerprint,
            segment_id: segment_ref.id.0.clone(),
            doc_count: segment_ref.doc_count as usize,
            size_bytes: segment_ref.size_bytes,
        }),
        segment_ref: Some(segment_ref),
    })
}

struct LinkedReusableShard {
    segment_ref: SegmentRef,
    doc_count: usize,
}

fn maybe_link_reusable_base_snapshot_shard(
    staging_root: &Path,
    shard_id: u32,
    fingerprint: &str,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
) -> Result<Option<LinkedReusableShard>> {
    let Some(previous_snapshot) = previous_snapshot else {
        return Ok(None);
    };
    let Some(previous_shard) = previous_snapshot.shards_by_id.get(&shard_id) else {
        return Ok(None);
    };
    if previous_shard.fingerprint != fingerprint {
        return Ok(None);
    }
    let Some(segment_ref) = previous_snapshot
        .manifest_segments
        .get(&previous_shard.segment_id)
        .cloned()
    else {
        return Ok(None);
    };
    let source_segment_dir = previous_snapshot
        .index_root
        .join("segments")
        .join(&previous_shard.segment_id);
    if !source_segment_dir.exists() {
        return Ok(None);
    }
    link_segment_dir_into_staging(
        &source_segment_dir,
        &staging_root
            .join("segments")
            .join(&previous_shard.segment_id),
    )?;
    Ok(Some(LinkedReusableShard {
        segment_ref,
        doc_count: previous_shard.doc_count,
    }))
}

fn partition_base_snapshot_shards(
    files: Vec<BaseSnapshotInputFile>,
) -> Vec<BaseSnapshotShardBuild> {
    let mut shards = BTreeMap::<u32, Vec<BaseSnapshotInputFile>>::new();
    for file in files {
        shards
            .entry(base_snapshot_shard_id(&file.logical_path))
            .or_default()
            .push(file);
    }

    shards
        .into_iter()
        .map(|(shard_id, mut shard_files)| {
            shard_files.sort_unstable_by(|left, right| left.logical_path.cmp(&right.logical_path));
            let fingerprint = base_snapshot_shard_fingerprint(&shard_files);
            BaseSnapshotShardBuild {
                shard_id,
                fingerprint,
                files: shard_files,
            }
        })
        .collect()
}

fn base_snapshot_shard_id(logical_path: &str) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(logical_path.as_bytes());
    hasher.finalize() % BASE_SNAPSHOT_SHARD_COUNT as u32
}

fn base_snapshot_shard_fingerprint(files: &[BaseSnapshotInputFile]) -> String {
    let mut left = crc32fast::Hasher::new();
    let mut right = crc32fast::Hasher::new();
    left.update(b"base-shard-fingerprint-v1:left");
    right.update(b"base-shard-fingerprint-v1:right");
    for file in files {
        update_base_snapshot_shard_hasher(&mut left, file, b"L");
        update_base_snapshot_shard_hasher(&mut right, file, b"R");
    }
    format!("{:08x}{:08x}", left.finalize(), right.finalize())
}

fn update_base_snapshot_shard_hasher(
    hasher: &mut crc32fast::Hasher,
    file: &BaseSnapshotInputFile,
    kind_prefix: &[u8],
) {
    hasher.update(kind_prefix);
    hasher.update(file.logical_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(&file.size.to_le_bytes());
    match &file.fingerprint {
        BaseSnapshotFileFingerprint::Worktree { mtime_nanos, .. } => {
            hasher.update(b"worktree");
            hasher.update(&mtime_nanos.to_le_bytes());
        }
        BaseSnapshotFileFingerprint::GitObject { object_id, size } => {
            hasher.update(b"git-object");
            hasher.update(object_id.as_bytes());
            hasher.update(&size.to_le_bytes());
        }
    }
}

fn base_snapshot_segment_id(shard_id: u32, fingerprint: &str) -> SegmentId {
    SegmentId(format!("seg-base-{shard_id:04}-{fingerprint}"))
}

fn link_segment_dir_into_staging(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)?;
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let source = entry.path();
        if entry.file_type()?.is_dir() {
            return Err(AppError::InvalidIndex(format!(
                "unexpected nested directory in base segment {}",
                source.display()
            )));
        }
        let destination = dest_dir.join(entry.file_name());
        match fs::hard_link(&source, &destination) {
            Ok(()) => {}
            Err(_) => {
                fs::copy(&source, &destination)?;
            }
        }
    }
    Ok(())
}

fn build_base_snapshot_segment(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    files: &[BaseSnapshotInputFile],
    segment_id: SegmentId,
    on_progress: &(impl Fn(usize) + Sync),
) -> Result<Option<SegmentRef>> {
    let mut builder = BulkBaseSegmentWriter::new(
        index_root,
        segment_id,
        PostingTokenizerConfig::from(build_config),
    )?;
    let processed_files = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for chunk in files.chunks(BASE_SNAPSHOT_PREPARE_CHUNK_DOCS) {
        let prepared = if should_parallel_prepare_base_documents(chunk) {
            let processed_files = Arc::clone(&processed_files);
            chunk
                .par_iter()
                .map(|file| {
                    let prepared =
                        prepare_base_document(repo_id, repo_root, file, &builder.tokenizer);
                    let processed = processed_files
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .saturating_add(1);
                    if processed % BASE_SNAPSHOT_PROGRESS_REPORT_DOCS == 0
                        || processed == files.len()
                    {
                        on_progress(processed);
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        } else {
            let processed_files = Arc::clone(&processed_files);
            chunk
                .iter()
                .map(|file| {
                    let prepared =
                        prepare_base_document(repo_id, repo_root, file, &builder.tokenizer);
                    let processed = processed_files
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .saturating_add(1);
                    if processed % BASE_SNAPSHOT_PROGRESS_REPORT_DOCS == 0
                        || processed == files.len()
                    {
                        on_progress(processed);
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        };

        for prepared_document in prepared {
            let Some(prepared_document) = prepared_document? else {
                continue;
            };
            builder.add_document(prepared_document)?;
        }
    }

    builder.finish()
}

fn write_marker_for_root(index_root: &Path, snapshot_key: &str) -> Result<()> {
    fs::create_dir_all(index_root)?;
    fs::write(
        WorkspaceBaseSnapshotIndex::marker_path_for_root(index_root),
        format!("{snapshot_key}\n"),
    )?;
    Ok(())
}

fn write_metadata_file_for_root(
    index_root: &Path,
    metadata: &WorkspaceBaseSnapshotMetadata,
) -> Result<()> {
    fs::create_dir_all(index_root)?;
    let encoded = serde_json::to_vec_pretty(&WorkspaceBaseSnapshotMetadataFile {
        snapshot_key: metadata.snapshot_key.clone(),
        revision: metadata.revision.clone(),
        tokenizer: metadata.tokenizer.map(|mode| mode.as_str().to_string()),
        doc_count: metadata.doc_count,
        head_commit: metadata.head_commit.clone(),
        config_fingerprint: metadata.config_fingerprint.clone(),
        repo_root: metadata.repo_root.clone(),
        shards: metadata
            .shards
            .iter()
            .map(|shard| WorkspaceBaseSnapshotShardMetadataFile {
                shard_id: shard.shard_id,
                fingerprint: shard.fingerprint.clone(),
                segment_id: shard.segment_id.clone(),
                doc_count: shard.doc_count,
                size_bytes: shard.size_bytes,
            })
            .collect(),
    })
    .map_err(|error| {
        AppError::Protocol(format!(
            "failed to encode base snapshot metadata for {}: {error}",
            index_root.display()
        ))
    })?;
    fs::write(
        WorkspaceBaseSnapshotIndex::metadata_path_for_root(index_root),
        encoded,
    )?;
    Ok(())
}

fn cleanup_unpublished_base_snapshot_builds(build_config: &BuildConfig) -> Result<()> {
    match fs::read_dir(WorkspaceBaseSnapshotIndex::storage_root(build_config)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.starts_with(BASE_SNAPSHOT_BUILD_STAGING_PREFIX)
                    || name.starts_with(BASE_SNAPSHOT_BUILD_STALE_PREFIX)
                {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = fs::remove_dir_all(path);
                    } else {
                        let _ = fs::remove_file(path);
                    }
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn next_staging_root(build_config: &BuildConfig) -> PathBuf {
    WorkspaceBaseSnapshotIndex::storage_root(build_config).join(format!(
        "{BASE_SNAPSHOT_BUILD_STAGING_PREFIX}{}-{}",
        std::process::id(),
        now_unix_nanos()
    ))
}

fn publish_staged_base_snapshot(build_config: &BuildConfig, staged_root: &Path) -> Result<()> {
    let final_root = WorkspaceBaseSnapshotIndex::index_root(build_config);
    let previous_root = final_root.exists().then(|| {
        WorkspaceBaseSnapshotIndex::storage_root(build_config).join(format!(
            "{BASE_SNAPSHOT_BUILD_STALE_PREFIX}{}",
            now_unix_nanos()
        ))
    });

    if let Some(previous_root) = &previous_root {
        fs::rename(&final_root, previous_root)?;
    }

    match fs::rename(staged_root, &final_root) {
        Ok(()) => {
            if let Some(previous_root) = previous_root {
                let _ = fs::remove_dir_all(previous_root);
            }
            Ok(())
        }
        Err(error) => {
            if let Some(previous_root) = previous_root {
                let _ = fs::rename(previous_root, &final_root);
            }
            Err(error.into())
        }
    }
}

fn parse_snapshot_revision(snapshot_key: &str) -> Option<GitRevision> {
    let suffix = snapshot_key.strip_prefix("base-git-")?;
    let (head_commit, _) = suffix.rsplit_once('-')?;
    (!head_commit.is_empty()).then(|| GitRevision(head_commit.to_string()))
}

fn parse_tokenizer_mode(value: &str) -> Option<TokenizerMode> {
    match value {
        "trigram" => Some(TokenizerMode::Trigram),
        "sparse-ngram" => Some(TokenizerMode::SparseNgram),
        _ => None,
    }
}

fn resolve_base_snapshot_build(
    build_config: &BuildConfig,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<ResolvedBaseSnapshotBuild> {
    let config_fingerprint = base_snapshot_config_fingerprint(build_config);
    match resolve_git_head(&build_config.repo_path, should_cancel)? {
        Some(head_commit) => {
            let snapshot_key = format!("base-git-{head_commit}-{config_fingerprint}");
            Ok(ResolvedBaseSnapshotBuild {
                snapshot_key,
                head_commit: Some(head_commit),
                config_fingerprint,
            })
        }
        None => {
            let snapshot_key = format!(
                "base-repo-{}-{}",
                repo_snapshot_fingerprint(build_config)?,
                config_fingerprint
            );
            Ok(ResolvedBaseSnapshotBuild {
                snapshot_key,
                head_commit: None,
                config_fingerprint,
            })
        }
    }
}

fn base_snapshot_config_fingerprint(build_config: &BuildConfig) -> String {
    format!(
        "{}-{}",
        build_config_fingerprint(build_config),
        SEGMENT_SCHEMA_FINGERPRINT
    )
}

fn reusable_snapshot_metadata(
    build_config: &BuildConfig,
    resolved: &ResolvedBaseSnapshotBuild,
) -> Result<Option<WorkspaceBaseSnapshotMetadata>> {
    let Some(metadata) = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(build_config)? else {
        return Ok(None);
    };
    if metadata.snapshot_key != resolved.snapshot_key
        || metadata.tokenizer != Some(build_config.tokenizer)
        || metadata.config_fingerprint.as_deref() != Some(&resolved.config_fingerprint)
        || metadata.repo_root.as_deref() != Some(&canonical_repo_root(&build_config.repo_path))
        || metadata.doc_count.is_none()
        || metadata.head_commit != resolved.head_commit
    {
        return Ok(None);
    }
    let Some(index) =
        WorkspaceBaseSnapshotIndex::open_if_matching(build_config, &resolved.snapshot_key)?
    else {
        return Ok(None);
    };
    if index.open_committed().is_err() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

fn repo_snapshot_fingerprint(build_config: &BuildConfig) -> Result<String> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(canonical_repo_root(&build_config.repo_path).as_bytes());
    let files = scan_paths(
        std::slice::from_ref(&build_config.repo_path),
        Some(&build_config.storage_root),
        ScanOptions {
            respect_ignore: matches!(build_config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: build_config.include_hidden,
            max_file_size: build_config.max_file_size,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )?;
    for file in files {
        hasher.update(repo_relative_path(&file.path, &build_config.repo_path).as_bytes());
        hasher.update(&file.size.to_le_bytes());
        hasher.update(&file.mtime_nanos.to_le_bytes());
    }
    Ok(format!("{:08x}", hasher.finalize()))
}

fn report_progress(
    on_progress: &mut impl FnMut(IndexProgress),
    phase: IndexProgressPhase,
    message: impl Into<String>,
    processed: usize,
    total: Option<usize>,
) {
    on_progress(IndexProgress {
        phase,
        message: message.into(),
        processed,
        total,
    });
}

fn check_cancel(should_cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if should_cancel() {
        return Err(AppError::Cancelled);
    }
    Ok(())
}

fn base_snapshot_input_files_from_repository_files(
    files: &[RepositoryFile],
    repo_root: &Path,
) -> Vec<BaseSnapshotInputFile> {
    files
        .iter()
        .map(|file| BaseSnapshotInputFile {
            path: file.path.clone(),
            logical_path: repo_relative_path(&file.path, repo_root),
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            source: BaseSnapshotFileSource::Worktree,
            fingerprint: BaseSnapshotFileFingerprint::Worktree {
                mtime_nanos: file.mtime_nanos,
            },
        })
        .collect()
}

fn build_head_snapshot_input_files(
    repo_root: &Path,
    head_commit: &str,
    include_hidden: bool,
    max_file_size: u64,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<BaseSnapshotInputFile>> {
    let dirty_paths = dirty_tracked_paths(repo_root, should_cancel)?;
    let mut files = Vec::new();

    for entry in head_tracked_entries(repo_root, head_commit, should_cancel)? {
        let path = crate::files::resolve_repo_path(repo_root, &entry.logical_path);
        if crate::files::is_workspace_internal_path(&path)
            || (!include_hidden && path_has_hidden_components(&path, repo_root))
            || entry.size > max_file_size
        {
            continue;
        }

        let worktree_metadata = fs::metadata(&path).ok();
        let use_worktree = !dirty_paths.contains(&entry.logical_path)
            && worktree_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_file() && metadata.len() == entry.size);
        if let Some(metadata) = worktree_metadata.filter(|_| use_worktree) {
            files.push(BaseSnapshotInputFile {
                path,
                logical_path: entry.logical_path,
                size: metadata.len(),
                mtime_nanos: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
                    .unwrap_or_default(),
                source: BaseSnapshotFileSource::Worktree,
                fingerprint: BaseSnapshotFileFingerprint::GitObject {
                    object_id: entry.object_id,
                    size: metadata.len(),
                },
            });
        } else {
            files.push(BaseSnapshotInputFile {
                path,
                logical_path: entry.logical_path,
                size: entry.size,
                mtime_nanos: 0,
                source: BaseSnapshotFileSource::GitHeadBlob {
                    head_commit: head_commit.to_string(),
                },
                fingerprint: BaseSnapshotFileFingerprint::GitObject {
                    object_id: entry.object_id,
                    size: entry.size,
                },
            });
        }
    }

    Ok(files)
}

fn read_base_snapshot_file_source_bytes(file: &BaseSnapshotInputFile) -> Result<Vec<u8>> {
    match &file.source {
        BaseSnapshotFileSource::Worktree => Ok(fs::read(&file.path)?),
        BaseSnapshotFileSource::GitHeadBlob { head_commit } => {
            let mut should_cancel = || false;
            read_git_blob(
                &infer_repo_root_from_logical_path(file),
                head_commit,
                &file.logical_path,
                &mut should_cancel,
            )
        }
    }
}

fn infer_repo_root_from_logical_path(file: &BaseSnapshotInputFile) -> PathBuf {
    let component_count = std::path::Path::new(&file.logical_path)
        .components()
        .count();
    let mut root = file.path.clone();
    for _ in 0..component_count {
        if let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
    }
    root
}

fn path_has_hidden_components(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| match component {
        std::path::Component::Normal(value) => value.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn repository_file_to_index_document(
    repo_id: &RepoId,
    _repo_root: &Path,
    file: &BaseSnapshotInputFile,
) -> Result<Option<IndexDocument>> {
    let logical_path = file.logical_path.clone();
    let key = DocKey {
        repo_id: repo_id.clone(),
        logical_path: logical_path.clone(),
    };
    let source_bytes = Arc::<[u8]>::from(read_base_snapshot_file_source_bytes(file)?);
    if has_binary_source_bytes(&source_bytes) {
        return Ok(None);
    }
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&source_bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    let file_name = file
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::Protocol(format!("invalid file name: {}", file.path.display())))?
        .to_string();

    Ok(Some(IndexDocument {
        key,
        repo_id: repo_id.clone(),
        logical_path,
        file_name,
        language: file
            .path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase()),
        size: file.size,
        modified_at: file.mtime_nanos,
        content_hash,
        source_bytes,
    }))
}

fn prepare_base_document(
    repo_id: &RepoId,
    repo_root: &Path,
    file: &BaseSnapshotInputFile,
    tokenizer: &PostingTokenizerConfig,
) -> Result<Option<PreparedBaseDocument>> {
    let Some(document) = repository_file_to_index_document(repo_id, repo_root, file)? else {
        return Ok(None);
    };
    let Some(text) = indexable_source_text(&document.source_bytes) else {
        return Ok(None);
    };
    let mut posting_terms = collect_document_posting_terms_from_text(text.as_ref(), tokenizer);
    posting_terms.sort_unstable();
    posting_terms.dedup();
    Ok(Some(PreparedBaseDocument {
        document,
        posting_terms,
        source_kind: segment_doc_source_kind(&file.source),
    }))
}

fn segment_doc_source_kind(source: &BaseSnapshotFileSource) -> SegmentDocSourceKind {
    match source {
        BaseSnapshotFileSource::Worktree => SegmentDocSourceKind::Worktree,
        BaseSnapshotFileSource::GitHeadBlob { .. } => SegmentDocSourceKind::GitHeadBlob,
    }
}

fn should_parallel_prepare_base_documents(files: &[BaseSnapshotInputFile]) -> bool {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) <= 1 {
        return false;
    }

    files.len() >= BASE_SNAPSHOT_PREPARE_PARALLEL_DOC_THRESHOLD
        || files.iter().map(|file| file.size).sum::<u64>()
            >= BASE_SNAPSHOT_PREPARE_PARALLEL_BYTE_THRESHOLD
}

fn has_binary_source_bytes(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].contains(&0)
}

fn indexable_source_text(bytes: &[u8]) -> Option<Cow<'_, str>> {
    (!has_binary_source_bytes(bytes)).then(|| String::from_utf8_lossy(bytes))
}

fn posting_bucket_id(term: u64) -> usize {
    let mixed = term ^ (term >> 33) ^ (term >> 17);
    (mixed as usize) % BASE_SNAPSHOT_POSTING_BUCKET_COUNT
}

fn estimated_term_entry_bytes(_term: &u64) -> usize {
    std::mem::size_of::<u64>()
        .saturating_add(std::mem::size_of::<Vec<u32>>())
        .saturating_add(std::mem::size_of::<usize>() * 2)
}

fn read_spilled_posting_record(reader: &mut impl Read) -> Result<Option<(u64, u32)>> {
    let Some(term) = read_optional_u64_from(reader)? else {
        return Ok(None);
    };
    let doc_id = read_u32_from(reader)?;
    Ok(Some((term, doc_id)))
}

fn write_spilled_posting_record(writer: &mut impl Write, record: PostingRecord) -> Result<()> {
    write_u64_to(writer, record.term)?;
    write_u32_to(writer, record.doc_id)?;
    Ok(())
}

fn read_posting_record(reader: &mut impl Read) -> Result<Option<PostingRecord>> {
    read_spilled_posting_record(reader)
        .map(|record| record.map(|(term, doc_id)| PostingRecord { term, doc_id }))
}

fn write_sorted_posting_runs(
    bucket_path: &Path,
    run_root: &Path,
    bucket_id: usize,
    max_records_per_run: usize,
) -> Result<Vec<PathBuf>> {
    let mut reader = BufReader::new(File::open(bucket_path)?);
    let mut run_paths = Vec::new();
    let mut records = Vec::with_capacity(max_records_per_run.max(1));

    loop {
        records.clear();
        while records.len() < max_records_per_run.max(1) {
            let Some(record) = read_posting_record(&mut reader)? else {
                break;
            };
            records.push(record);
        }
        if records.is_empty() {
            break;
        }

        records.sort_unstable_by_key(|record| (record.term, record.doc_id));
        let run_path = run_root.join(format!(
            "bucket-{bucket_id:03}-run-{:06}.bin",
            run_paths.len()
        ));
        let mut writer = BufWriter::new(File::create(&run_path)?);
        for record in &records {
            write_spilled_posting_record(&mut writer, *record)?;
        }
        writer.flush()?;
        run_paths.push(run_path);
    }

    Ok(run_paths)
}

fn merge_sorted_posting_runs(
    run_paths: &[PathBuf],
    term_dict_writer: &mut TermDictStreamWriter,
    postings_writer: &mut PostingsStreamWriter,
) -> Result<()> {
    if run_paths.is_empty() {
        return Ok(());
    }

    let mut readers = run_paths
        .iter()
        .map(|path| Ok(Some(BufReader::new(File::open(path)?))))
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<PostingRunHead>>::new();
    for (run_index, reader_slot) in readers.iter_mut().enumerate() {
        let Some(reader) = reader_slot.as_mut() else {
            continue;
        };
        if let Some(record) = read_posting_record(reader)? {
            heap.push(Reverse(PostingRunHead { record, run_index }));
        } else {
            *reader_slot = None;
            fs::remove_file(&run_paths[run_index])?;
        }
    }

    let mut current_term = None;
    let mut current_doc_ids = Vec::new();

    while let Some(Reverse(head)) = heap.pop() {
        let term = head.record.term;
        if current_term != Some(term) {
            flush_streamed_posting_term(
                current_term,
                &current_doc_ids,
                term_dict_writer,
                postings_writer,
            )?;
            current_term = Some(term);
            current_doc_ids.clear();
        }
        if current_doc_ids.last().copied() != Some(head.record.doc_id) {
            current_doc_ids.push(head.record.doc_id);
        }

        let run_index = head.run_index;
        let Some(reader) = readers.get_mut(run_index).and_then(Option::as_mut) else {
            return Err(AppError::InvalidIndex(format!(
                "missing posting run reader for index {run_index}"
            )));
        };
        if let Some(record) = read_posting_record(reader)? {
            heap.push(Reverse(PostingRunHead { record, run_index }));
        } else {
            readers[run_index] = None;
            fs::remove_file(&run_paths[run_index])?;
        }
    }

    flush_streamed_posting_term(
        current_term,
        &current_doc_ids,
        term_dict_writer,
        postings_writer,
    )?;
    Ok(())
}

fn flush_streamed_posting_term(
    term: Option<u64>,
    doc_ids: &[u32],
    term_dict_writer: &mut TermDictStreamWriter,
    postings_writer: &mut PostingsStreamWriter,
) -> Result<()> {
    let Some(term) = term else {
        return Ok(());
    };
    if doc_ids.is_empty() {
        return Ok(());
    }
    let postings_offset = postings_writer.push_doc_ids(doc_ids)?;
    term_dict_writer.push_entry(term, postings_offset)?;
    Ok(())
}

fn write_segment_header(writer: &mut BufWriter<File>, magic: &[u8; 8]) -> Result<()> {
    write_segment_header_with_version(writer, magic, SEGMENT_FILE_VERSION)
}

fn write_segment_header_with_version(
    writer: &mut BufWriter<File>,
    magic: &[u8; 8],
    version: u32,
) -> Result<()> {
    writer.write_all(magic)?;
    write_u32_to(writer, version)?;
    write_u32_to(writer, 0)?;
    Ok(())
}

fn patch_segment_count(writer: &mut BufWriter<File>, count: u32) -> Result<()> {
    writer.flush()?;
    writer.seek(SeekFrom::Start(COUNT_OFFSET))?;
    write_u32_to(writer, count)?;
    writer.seek(SeekFrom::End(0))?;
    Ok(())
}

fn write_u32_to(writer: &mut impl Write, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64_to(writer: &mut impl Write, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_string_to(writer: &mut impl Write, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("string length exceeds u32 range".into()))?;
    write_u32_to(writer, len)?;
    writer.write_all(bytes)?;
    Ok(())
}

fn encoded_string_len(value: &str) -> Result<u64> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("string length exceeds u32 range".into()))?;
    Ok(u64::from(len) + 4)
}

fn write_bytes_to(writer: &mut impl Write, value: &[u8]) -> Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| AppError::ValueOutOfRange("byte slice length exceeds u32 range".into()))?;
    write_u32_to(writer, len)?;
    writer.write_all(value)?;
    Ok(())
}

fn read_optional_u32_from(reader: &mut impl Read) -> Result<Option<u32>> {
    let mut bytes = [0u8; 4];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(u32::from_le_bytes(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_u64_from(reader: &mut impl Read) -> Result<Option<u64>> {
    let mut bytes = [0u8; 8];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(u64::from_le_bytes(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_u32_from(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{
        base_snapshot_input_files_from_repository_files, estimated_term_entry_bytes,
        merge_sorted_posting_runs, reusable_base_snapshot_shards, write_sorted_posting_runs,
        write_spilled_posting_record, BaseSnapshotInputFile, PostingBuckets, PostingBucketsState,
        PostingRecord, WorkspaceBaseSnapshotIndex,
    };
    use crate::base_snapshot_build::BaseSnapshotBuildOptions;
    use crate::config::{BuildConfig, CorpusMode, TokenizerMode};
    use crate::files::{scan_paths, ScanOptions};
    use crate::index_engine::segment::{
        read_postings_file, read_term_dict_file, FileSegmentDocTable, PostingsStreamWriter,
        SegmentDocSourceKind, TermDictStreamWriter,
    };
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
        let run_paths = write_sorted_posting_runs(&bucket_path, &run_root, 0, 2)
            .expect("run split should succeed");
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
            event.processed >= super::BASE_SNAPSHOT_PROGRESS_REPORT_DOCS
                && event.processed < file_count
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
            Some(&SegmentDocSourceKind::Worktree)
        );
        assert_eq!(
            source_kinds.get("dirty.rs"),
            Some(&SegmentDocSourceKind::GitHeadBlob)
        );
    }
}
