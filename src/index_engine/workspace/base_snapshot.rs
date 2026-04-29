use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::BinaryHeap,
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::{SystemTime, UNIX_EPOCH},
};

use rayon::prelude::*;

mod build_flow;
mod input;
mod metadata;
mod reuse;
mod writer;

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
            collect_document_posting_terms_from_text, encoded_source_bytes_len,
            write_path_offsets_file, write_segment_meta, write_source_bytes, FileSegmentStore,
            PostingsStreamWriter, SegmentDocSourceKind, SegmentMeta, TermDictStreamWriter,
            DOCS_FILE, PATH_MAP_FILE, PATH_OFFSETS_FILE, SEGMENT_META_FILE,
        },
        writer::IndexDocument,
        DocKey, GitRevision, ManifestId, RepoId, SegmentId, ShardId,
    },
    progress::{IndexProgress, IndexProgressPhase},
    Result,
};

use self::{
    build_flow::{
        build_sharded_base_snapshot, check_cancel, cleanup_unpublished_base_snapshot_builds,
        next_staging_root, publish_staged_base_snapshot, report_progress,
    },
    input::{
        base_snapshot_input_files_from_repository_files, build_head_snapshot_input_files,
        prepare_base_document, should_parallel_prepare_base_documents, BaseSnapshotFileFingerprint,
        BaseSnapshotInputFile, PreparedBaseDocument,
    },
    metadata::{
        parse_snapshot_revision, parse_tokenizer_mode, write_marker_for_root,
        write_metadata_file_for_root, WorkspaceBaseSnapshotMetadataFile,
    },
    reuse::{
        base_snapshot_config_fingerprint, base_snapshot_segment_id,
        maybe_link_reusable_base_snapshot_shard, partition_base_snapshot_shards,
        resolve_base_snapshot_build, reusable_base_snapshot_shards, reusable_snapshot_metadata,
        ReusableBaseSnapshotShards,
    },
    writer::BaseSnapshotBulkSegmentWriter as BulkBaseSegmentWriter,
};

#[cfg(test)]
use self::reuse::base_snapshot_shard_id;
#[cfg(test)]
use self::writer::{
    estimated_term_entry_bytes, merge_sorted_posting_runs, write_sorted_posting_runs,
    write_spilled_posting_record, PostingBuckets, PostingBucketsState, PostingRecord,
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
const DOC_RECORD_LEN: u64 = 72;

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

pub struct WorkspaceBaseSnapshotIndex {
    reader: Arc<IndexReaderFactory>,
}

pub(crate) struct WorkspaceBaseSnapshotBuildResult {
    pub(crate) indexed_docs: usize,
    pub(crate) metadata: WorkspaceBaseSnapshotMetadata,
    #[allow(dead_code)]
    pub(crate) reused_existing: bool,
}

#[derive(Clone)]
struct BaseSnapshotShardBuild {
    shard_id: u32,
    fingerprint: String,
    files: Vec<BaseSnapshotInputFile>,
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
        let reader = self.reader.open_persisted_current()?;
        reader.prewarm_search_state()?;
        Ok(reader)
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
mod tests;
