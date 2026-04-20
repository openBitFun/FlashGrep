use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
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
            collect_document_posting_terms, write_segment_meta, write_source_bytes,
            FileSegmentStore, PostingsStreamWriter, SegmentMeta, TermDictStreamWriter, DOCS_FILE,
            PATH_MAP_FILE, SEGMENT_META_FILE, STORED_FIELDS_FILE,
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
const BASE_SNAPSHOT_POSTING_BUCKET_COUNT: usize = 64;
const BASE_SNAPSHOT_IN_MEMORY_POSTING_THRESHOLD_BYTES: usize = 128 * 1024 * 1024;
const BASE_SNAPSHOT_SEGMENT_ID: &str = "seg-0000000000000001-base";
const BASE_SNAPSHOT_MANIFEST_ID: &str = "manifest-0000000000000001";
const SEGMENT_SCHEMA_FINGERPRINT: &str = "segment-publish-v1";
const DOCS_MAGIC: &[u8; 8] = b"CGDOCS01";
const STORED_FIELDS_MAGIC: &[u8; 8] = b"CGSTRD02";
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
}

pub struct WorkspaceBaseSnapshotIndex {
    reader: Arc<IndexReaderFactory>,
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
}

#[derive(Clone)]
enum BaseSnapshotFileSource {
    Worktree,
    GitHeadBlob { head_commit: String },
}

struct BulkBaseSegmentWriter {
    segment_dir: PathBuf,
    segment_id: SegmentId,
    tokenizer: PostingTokenizerConfig,
    path_map: BufWriter<File>,
    stored_fields: BufWriter<File>,
    docs: BufWriter<File>,
    posting_buckets: PostingBuckets,
    doc_count: u32,
    total_source_bytes: u64,
}

struct PreparedBaseDocument {
    document: IndexDocument,
    posting_terms: Vec<u64>,
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
        let mut stored_fields = BufWriter::new(File::create(segment_dir.join(STORED_FIELDS_FILE))?);
        let mut docs = BufWriter::new(File::create(segment_dir.join(DOCS_FILE))?);

        write_segment_header(&mut path_map, PATH_MAP_MAGIC)?;
        write_segment_header(&mut stored_fields, STORED_FIELDS_MAGIC)?;
        write_segment_header(&mut docs, DOCS_MAGIC)?;

        Ok(Self {
            segment_dir,
            segment_id,
            tokenizer,
            path_map,
            stored_fields,
            docs,
            posting_buckets: PostingBuckets::new(index_root),
            doc_count: 0,
            total_source_bytes: 0,
        })
    }

    fn add_document(&mut self, prepared: PreparedBaseDocument) -> Result<()> {
        let doc_id = self.doc_count;
        let PreparedBaseDocument {
            document,
            posting_terms,
        } = prepared;

        write_string_to(&mut self.path_map, &document.repo_id.0)?;
        write_string_to(&mut self.path_map, &document.logical_path)?;

        let stored_offset = self.stored_fields.stream_position()?;
        write_string_to(&mut self.stored_fields, &document.file_name)?;
        write_optional_string_to(&mut self.stored_fields, document.language.as_deref())?;
        write_u64_to(&mut self.stored_fields, document.size)?;
        write_u64_to(&mut self.stored_fields, document.modified_at)?;
        self.stored_fields.write_all(&document.content_hash)?;
        write_source_bytes(&mut self.stored_fields, &document.source_bytes)?;
        let stored_len = self
            .stored_fields
            .stream_position()?
            .checked_sub(stored_offset)
            .ok_or_else(|| AppError::InvalidIndex("stored fields length underflow".into()))?;
        let stored_len = u32::try_from(stored_len).map_err(|_| {
            AppError::ValueOutOfRange("stored fields length exceeds u32 range".into())
        })?;

        write_u32_to(&mut self.docs, doc_id)?;
        write_u64_to(&mut self.docs, stored_offset)?;
        write_u32_to(&mut self.docs, stored_len)?;

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
        patch_segment_count(&mut self.stored_fields, self.doc_count)?;
        patch_segment_count(&mut self.docs, self.doc_count)?;

        self.path_map.flush()?;
        self.stored_fields.flush()?;
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
            let bucket = self
                .buckets
                .get_mut(bucket_id)
                .expect("posting bucket must exist");
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
            let mut keys = bucket.keys().cloned().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                let doc_ids = bucket.get(&key).expect("posting key must exist");
                let postings_offset = postings_writer.push_doc_ids(doc_ids)?;
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
        write_u64_to(writer, term)?;
        write_u32_to(writer, doc_id)?;
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

        for bucket_path in &bucket_paths {
            if fs::metadata(bucket_path).map_or(true, |metadata| metadata.len() == 0) {
                continue;
            }

            let mut terms = HashMap::<u64, Vec<u32>>::new();
            let mut reader = BufReader::new(File::open(bucket_path)?);
            while let Some((term, doc_id)) = read_spilled_posting_record(&mut reader)? {
                terms.entry(term).or_default().push(doc_id);
            }

            let mut keys = terms.keys().cloned().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                let doc_ids = terms.get(&key).expect("posting key must exist");
                let postings_offset = postings_writer.push_doc_ids(doc_ids)?;
                term_dict_writer.push_entry(key, postings_offset)?;
            }
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

    pub(crate) fn open(build_config: &BuildConfig) -> Result<Self> {
        Self::open_read_only(build_config)
    }

    pub(crate) fn open_read_only(build_config: &BuildConfig) -> Result<Self> {
        let config = Self::engine_config(build_config);
        let reader = Arc::new(IndexReaderFactory::new(
            Arc::new(FileManifestStore::open(&config.index_root)?),
            Arc::new(FileSegmentStore::open(
                &config.index_root,
                config.tokenizer.clone(),
            )?),
            Arc::new(FileDeleteStore::open(&config.index_root)?),
            SharedSnapshotState::new(),
            config.tokenizer.clone(),
        ));
        Ok(Self { reader })
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
            |event| on_progress(event),
            || should_cancel(),
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
        should_cancel: &mut impl FnMut() -> bool,
    ) -> Result<Self> {
        cleanup_unpublished_base_snapshot_builds(build_config)?;
        let staging_root = next_staging_root(build_config);
        fs::create_dir_all(&staging_root)?;

        let doc_count = if let Some(head_commit) = head_commit {
            check_cancel(should_cancel)?;
            if tracked_worktree_matches_head(&build_config.repo_path, should_cancel)? {
                let files = tracked_worktree_files(
                    &build_config.repo_path,
                    build_config.include_hidden,
                    build_config.max_file_size,
                    should_cancel,
                )?;
                build_single_segment_base_snapshot(
                    &staging_root,
                    build_config,
                    repo_id,
                    &build_config.repo_path,
                    &base_snapshot_input_files_from_repository_files(
                        &files,
                        &build_config.repo_path,
                    ),
                )?
            } else {
                let files = build_head_snapshot_input_files(
                    &build_config.repo_path,
                    head_commit,
                    build_config.include_hidden,
                    build_config.max_file_size,
                    should_cancel,
                )?;
                build_single_segment_base_snapshot(
                    &staging_root,
                    build_config,
                    repo_id,
                    &build_config.repo_path,
                    &files,
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
            build_single_segment_base_snapshot(
                &staging_root,
                build_config,
                repo_id,
                &build_config.repo_path,
                &base_snapshot_input_files_from_repository_files(&files, &build_config.repo_path),
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
                config_fingerprint: Some(build_config_fingerprint(build_config)),
                repo_root: Some(canonical_repo_root(&build_config.repo_path)),
            },
        )?;
        publish_staged_base_snapshot(build_config, &staging_root)?;
        Self::open_read_only(build_config)
    }

    pub fn open_committed(&self) -> Result<IndexSnapshotReader> {
        self.reader.open_persisted_current()
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

fn build_single_segment_base_snapshot(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    files: &[BaseSnapshotInputFile],
) -> Result<usize> {
    fs::create_dir_all(index_root)?;
    let _ = FileDeleteStore::open(index_root)?;

    let segment_id = SegmentId(BASE_SNAPSHOT_SEGMENT_ID.into());
    let mut builder = BulkBaseSegmentWriter::new(
        index_root,
        segment_id,
        PostingTokenizerConfig::from(build_config),
    )?;

    let mut files = files.to_vec();
    files.sort_unstable_by(|left, right| left.logical_path.cmp(&right.logical_path));

    let mut doc_count = 0usize;
    for chunk in files.chunks(BASE_SNAPSHOT_PREPARE_CHUNK_DOCS) {
        let prepared = if should_parallel_prepare_base_documents(chunk) {
            chunk
                .par_iter()
                .map(|file| prepare_base_document(repo_id, repo_root, file, &builder.tokenizer))
                .collect::<Vec<_>>()
        } else {
            chunk
                .iter()
                .map(|file| prepare_base_document(repo_id, repo_root, file, &builder.tokenizer))
                .collect::<Vec<_>>()
        };

        for prepared_document in prepared {
            let Some(prepared_document) = prepared_document? else {
                continue;
            };
            builder.add_document(prepared_document)?;
            doc_count += 1;
        }
    }

    let active_segments = builder.finish()?.into_iter().collect::<Vec<_>>();
    let committed_seq_no = if active_segments.is_empty() { 0 } else { 1 };
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

    Ok(doc_count)
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
    let config_fingerprint = build_config_fingerprint(build_config);
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

        let use_worktree = !dirty_paths.contains(&entry.logical_path)
            && fs::metadata(&path)
                .map(|metadata| metadata.is_file() && metadata.len() == entry.size)
                .unwrap_or(false);
        if use_worktree {
            let metadata = fs::metadata(&path)?;
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
    if !is_indexable_source_bytes(&source_bytes) {
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
    let mut posting_terms = collect_document_posting_terms(&document.source_bytes, tokenizer);
    posting_terms.sort_unstable();
    posting_terms.dedup();
    Ok(Some(PreparedBaseDocument {
        document,
        posting_terms,
    }))
}

fn should_parallel_prepare_base_documents(files: &[BaseSnapshotInputFile]) -> bool {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) <= 1 {
        return false;
    }

    files.len() >= BASE_SNAPSHOT_PREPARE_PARALLEL_DOC_THRESHOLD
        || files.iter().map(|file| file.size).sum::<u64>()
            >= BASE_SNAPSHOT_PREPARE_PARALLEL_BYTE_THRESHOLD
}

fn is_indexable_source_bytes(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    !bytes[..probe_len].contains(&0) && std::str::from_utf8(bytes).is_ok()
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

fn write_segment_header(writer: &mut BufWriter<File>, magic: &[u8; 8]) -> Result<()> {
    writer.write_all(magic)?;
    write_u32_to(writer, SEGMENT_FILE_VERSION)?;
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

fn write_optional_string_to(writer: &mut impl Write, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            writer.write_all(&[1])?;
            write_string_to(writer, value)?;
        }
        None => writer.write_all(&[0])?,
    }
    Ok(())
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
    use tempfile::TempDir;

    use super::{estimated_term_entry_bytes, PostingBuckets, PostingBucketsState};
    use crate::index_engine::segment::{read_postings_file, read_term_dict_file};

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
    }
}
