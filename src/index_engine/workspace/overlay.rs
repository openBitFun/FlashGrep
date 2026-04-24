use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crate::{
    config::BuildConfig,
    error::AppError,
    index_engine::{
        config::{EngineConfig, PostingTokenizerConfig},
        reader::IndexSnapshotReader,
        types::DocKey,
        writer::{IndexDocument, IndexEngine, IndexWriter, WriterStats},
        RepoId, ShardId,
    },
    search_engine::{SearchDocument, SearchDocumentSource},
    Result,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceOverlaySnapshotState {
    existing_documents: Vec<IndexDocument>,
    deleted_keys: Vec<DocKey>,
}

impl WorkspaceOverlaySnapshotState {
    pub(crate) fn from_paths(
        repo_id: &RepoId,
        repo_root: &Path,
        documents: &HashMap<String, SearchDocument>,
        deleted_paths: &HashSet<String>,
    ) -> Result<Self> {
        Ok(Self {
            existing_documents: build_index_documents(
                repo_id,
                repo_root,
                &sorted_paths(documents.keys()),
                documents,
            )?,
            deleted_keys: build_delete_keys(
                repo_id,
                repo_root,
                &sorted_paths(deleted_paths.iter()),
            )?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceOverlayIngestBatch {
    modified_documents: Vec<IndexDocument>,
    new_documents: Vec<IndexDocument>,
    deleted_keys: Vec<DocKey>,
    missing_keys: Vec<DocKey>,
}

impl WorkspaceOverlayIngestBatch {
    pub(crate) fn from_paths(
        repo_id: &RepoId,
        repo_root: &Path,
        modified_paths: &HashSet<String>,
        new_paths: &HashSet<String>,
        deleted_paths: &HashSet<String>,
        documents: &HashMap<String, SearchDocument>,
    ) -> Result<Self> {
        let modified_paths = sorted_paths(modified_paths.iter());
        let new_paths = sorted_paths(new_paths.iter());
        Ok(Self {
            modified_documents: build_index_documents(
                repo_id,
                repo_root,
                &modified_paths,
                documents,
            )?,
            new_documents: build_index_documents(repo_id, repo_root, &new_paths, documents)?,
            deleted_keys: build_delete_keys(
                repo_id,
                repo_root,
                &sorted_paths(deleted_paths.iter()),
            )?,
            missing_keys: build_delete_keys(
                repo_id,
                repo_root,
                &sorted_missing_paths(&modified_paths, &new_paths, documents),
            )?,
        })
    }

    pub(crate) fn from_deleted_paths(
        repo_id: &RepoId,
        repo_root: &Path,
        paths: &[String],
    ) -> Result<Self> {
        Ok(Self {
            deleted_keys: build_delete_keys(repo_id, repo_root, paths)?,
            ..Self::default()
        })
    }

    fn is_empty(&self) -> bool {
        self.modified_documents.is_empty()
            && self.new_documents.is_empty()
            && self.deleted_keys.is_empty()
            && self.missing_keys.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WorkspaceOverlayPublishPolicy {
    #[default]
    Refresh,
    Checkpoint,
    AutoCheckpoint {
        max_uncommitted_ops: u64,
    },
}

pub(crate) const DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS: u64 = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceOverlayRuntimeStats {
    pub committed_seq_no: u64,
    pub last_seq_no: u64,
    pub uncommitted_ops: u64,
    pub pending_docs: usize,
    pub active_segments: usize,
    pub active_delete_segments: usize,
    pub merge_requested: bool,
    pub merge_running: bool,
    pub merge_attempts: u64,
    pub merge_completed: u64,
    pub merge_failed: u64,
    pub last_merge_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceOverlayMergeEvent {
    Requested,
    StateChanged,
}

#[derive(Debug, Default)]
struct MergeRuntimeState {
    requested: bool,
    running: bool,
    merge_attempts: u64,
    merge_completed: u64,
    merge_failed: u64,
    last_merge_error: Option<String>,
}

pub struct WorkspaceOverlayIndex {
    engine: IndexEngine,
    publish_policy: WorkspaceOverlayPublishPolicy,
    merge_state: Mutex<MergeRuntimeState>,
    merge_state_hook: MergeStateHook,
}

type MergeStateHook = Arc<dyn Fn(WorkspaceOverlayMergeEvent) + Send + Sync>;
const LEGACY_OVERLAY_ARTIFACT_NAMES: &[&str] = &[
    "CURRENT_MANIFEST",
    "CURRENT_MANIFEST.tmp",
    "deletes",
    "manifests",
    "segments",
];

impl WorkspaceOverlayIndex {
    pub(crate) fn index_root(build_config: &BuildConfig) -> PathBuf {
        workspace_overlay_index_root(build_config)
    }

    pub(crate) fn clear_storage(build_config: &BuildConfig) -> Result<()> {
        let root = Self::index_root(build_config);
        remove_overlay_artifact(&root)
    }

    pub(crate) fn open(build_config: &BuildConfig) -> Result<Self> {
        Self::open_with_policy(
            build_config,
            WorkspaceOverlayPublishPolicy::AutoCheckpoint {
                max_uncommitted_ops: DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
            },
        )
    }

    pub(crate) fn open_with_policy(
        build_config: &BuildConfig,
        publish_policy: WorkspaceOverlayPublishPolicy,
    ) -> Result<Self> {
        Self::open_with_policy_and_hook(build_config, publish_policy, Arc::new(|_| {}))
    }

    pub(crate) fn open_with_policy_and_hook(
        build_config: &BuildConfig,
        publish_policy: WorkspaceOverlayPublishPolicy,
        merge_state_hook: MergeStateHook,
    ) -> Result<Self> {
        migrate_legacy_overlay_storage(build_config)?;
        Ok(Self::new_with_policy_and_hook(
            IndexEngine::open(EngineConfig {
                shard_id: ShardId(format!(
                    "{}#workspace-overlay",
                    build_config.repo_path.to_string_lossy()
                )),
                index_root: workspace_overlay_index_root(build_config),
                tokenizer: PostingTokenizerConfig::from(build_config),
                ..EngineConfig::default()
            })?,
            publish_policy,
            merge_state_hook,
        ))
    }

    pub fn new(engine: IndexEngine) -> Self {
        Self::new_with_policy(engine, WorkspaceOverlayPublishPolicy::default())
    }

    pub(crate) fn new_with_policy(
        engine: IndexEngine,
        publish_policy: WorkspaceOverlayPublishPolicy,
    ) -> Self {
        Self::new_with_policy_and_hook(engine, publish_policy, Arc::new(|_| {}))
    }

    pub(crate) fn new_with_policy_and_hook(
        engine: IndexEngine,
        publish_policy: WorkspaceOverlayPublishPolicy,
        merge_state_hook: MergeStateHook,
    ) -> Self {
        Self {
            engine,
            publish_policy,
            merge_state: Mutex::new(MergeRuntimeState::default()),
            merge_state_hook,
        }
    }

    pub fn open_snapshot(&self) -> Result<IndexSnapshotReader> {
        self.engine.reader().open_snapshot()
    }

    pub(crate) fn writer_stats(&self) -> WriterStats {
        self.engine.writer().stats()
    }

    pub(crate) fn runtime_stats(&self) -> Result<WorkspaceOverlayRuntimeStats> {
        let writer_stats = self.engine.writer().stats();
        let manifest = self.engine.admin().manifest()?;
        let state = match self.merge_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(WorkspaceOverlayRuntimeStats {
            committed_seq_no: writer_stats.committed_seq_no,
            last_seq_no: writer_stats.last_seq_no,
            uncommitted_ops: writer_stats.uncommitted_ops,
            pending_docs: writer_stats.pending_docs,
            active_segments: manifest.active_segments.len(),
            active_delete_segments: manifest.active_delete_segments.len(),
            merge_requested: state.requested,
            merge_running: state.running,
            merge_attempts: state.merge_attempts,
            merge_completed: state.merge_completed,
            merge_failed: state.merge_failed,
            last_merge_error: state.last_merge_error.clone(),
        })
    }

    pub(crate) fn run_requested_merge(&self) -> Result<bool> {
        {
            let mut state = match self.merge_state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if state.running || !state.requested {
                return Ok(false);
            }
            state.requested = false;
            state.running = true;
            state.merge_attempts += 1;
        }
        (self.merge_state_hook)(WorkspaceOverlayMergeEvent::StateChanged);

        let merge_result = self.engine.admin().maintain();
        if let Err(error) = &merge_result {
            eprintln!("codgrep: workspace overlay background merge failed: {error}");
        }

        {
            let mut state = match self.merge_state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.running = false;
            match &merge_result {
                Ok(stats) => {
                    state.merge_completed += stats.merge_tasks;
                    state.last_merge_error = None;
                }
                Err(error) => {
                    state.merge_failed += 1;
                    state.last_merge_error = Some(error.to_string());
                }
            }
        }
        (self.merge_state_hook)(WorkspaceOverlayMergeEvent::StateChanged);
        merge_result.map(|stats| stats.merge_tasks > 0 || stats.vacuumed_files > 0)
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        let writer = self.engine.writer();
        let _ = writer.flush()?;
        let _ = writer.commit()?;
        self.request_background_merge();
        Ok(())
    }

    pub(crate) fn rebuild_snapshot(&self, snapshot: &WorkspaceOverlaySnapshotState) -> Result<()> {
        let writer = self.engine.writer();
        for document in &snapshot.existing_documents {
            writer.add_document(document.clone())?;
        }
        for key in &snapshot.deleted_keys {
            writer.delete_document(key)?;
        }
        self.publish_after_write(&writer)
    }

    pub(crate) fn ingest_batch(&self, batch: &WorkspaceOverlayIngestBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let writer = self.engine.writer();
        for key in &batch.deleted_keys {
            writer.delete_document(key)?;
        }
        for document in &batch.modified_documents {
            writer.update_document(document.clone())?;
        }
        for document in &batch.new_documents {
            writer.add_document(document.clone())?;
        }
        for key in &batch.missing_keys {
            writer.delete_document(key)?;
        }
        self.publish_after_write(&writer)
    }

    fn publish_after_write(&self, writer: &Arc<IndexWriter>) -> Result<()> {
        match self.publish_policy {
            WorkspaceOverlayPublishPolicy::Refresh => {
                writer.refresh()?;
            }
            WorkspaceOverlayPublishPolicy::Checkpoint => {
                let _ = writer.commit()?;
                self.request_background_merge();
            }
            WorkspaceOverlayPublishPolicy::AutoCheckpoint {
                max_uncommitted_ops,
            } => {
                let stats = writer.stats();
                if stats.uncommitted_ops >= max_uncommitted_ops {
                    let _ = writer.commit()?;
                    self.request_background_merge();
                } else {
                    writer.refresh()?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn request_background_merge(&self) {
        let mut state = match self.merge_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let notify = !state.requested;
        state.requested = true;
        drop(state);
        if notify {
            (self.merge_state_hook)(WorkspaceOverlayMergeEvent::Requested);
        }
    }

    pub(crate) fn retry_failed_merge(&self) {
        let mut state = match self.merge_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let notify = !state.requested;
        state.requested = true;
        drop(state);
        if notify {
            (self.merge_state_hook)(WorkspaceOverlayMergeEvent::StateChanged);
        }
    }
}

fn sorted_paths<'a>(paths: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut paths = paths.into_iter().cloned().collect::<Vec<_>>();
    paths.sort_unstable();
    paths
}

fn migrate_legacy_overlay_storage(build_config: &BuildConfig) -> Result<()> {
    let storage_root = &build_config.storage_root;
    let overlay_root = workspace_overlay_index_root(build_config);
    let existing = LEGACY_OVERLAY_ARTIFACT_NAMES
        .iter()
        .map(|name| (storage_root.join(name), overlay_root.join(name)))
        .filter(|(source, _)| source.exists())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(&overlay_root)?;
    for (source, destination) in existing {
        if destination.exists() {
            remove_overlay_artifact(&source)?;
            continue;
        }
        fs::rename(source, destination)?;
    }
    Ok(())
}

fn remove_overlay_artifact(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        remove_dir_all_with_retry(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_dir_all_with_retry(path: &Path) -> Result<()> {
    const REMOVE_DIR_RETRY_ATTEMPTS: usize = 5;
    const REMOVE_DIR_RETRY_DELAY_MS: u64 = 20;

    for attempt in 0..REMOVE_DIR_RETRY_ATTEMPTS {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if is_directory_not_empty_error(&error) => {
                if attempt + 1 == REMOVE_DIR_RETRY_ATTEMPTS {
                    return Err(error.into());
                }
                thread::sleep(Duration::from_millis(REMOVE_DIR_RETRY_DELAY_MS));
            }
            Err(error) => return Err(error.into()),
        }
    }

    Ok(())
}

fn is_directory_not_empty_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::DirectoryNotEmpty || error.raw_os_error() == Some(66)
}

fn workspace_overlay_index_root(build_config: &BuildConfig) -> PathBuf {
    build_config.storage_root.join("workspace-overlay")
}

fn sorted_missing_paths(
    modified_paths: &[String],
    new_paths: &[String],
    documents: &HashMap<String, SearchDocument>,
) -> Vec<String> {
    let mut missing = modified_paths
        .iter()
        .chain(new_paths.iter())
        .filter(|path| !documents.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    missing.sort_unstable();
    missing
}

fn build_index_documents(
    repo_id: &RepoId,
    repo_root: &Path,
    paths: &[String],
    documents: &HashMap<String, SearchDocument>,
) -> Result<Vec<IndexDocument>> {
    let mut index_documents = Vec::new();
    for path in paths {
        if let Some(document) = documents.get(path) {
            index_documents.push(search_document_to_index_document(
                repo_id, repo_root, document,
            )?);
        }
    }
    Ok(index_documents)
}

fn build_delete_keys(repo_id: &RepoId, repo_root: &Path, paths: &[String]) -> Result<Vec<DocKey>> {
    let mut keys = Vec::with_capacity(paths.len());
    for path in paths {
        keys.push(doc_key_for_path(repo_id, repo_root, path)?);
    }
    Ok(keys)
}

fn doc_key_for_path(repo_id: &RepoId, repo_root: &Path, path: &str) -> Result<DocKey> {
    let absolute_path = PathBuf::from(path);
    let logical_path = absolute_path
        .strip_prefix(repo_root)
        .map_err(|_| {
            AppError::Protocol(format!(
                "path {} is not under repo root {}",
                absolute_path.display(),
                repo_root.display()
            ))
        })?
        .to_string_lossy()
        .into_owned();

    Ok(DocKey {
        repo_id: repo_id.clone(),
        logical_path,
    })
}

fn search_document_to_index_document(
    repo_id: &RepoId,
    repo_root: &Path,
    document: &SearchDocument,
) -> Result<IndexDocument> {
    let key = doc_key_for_path(repo_id, repo_root, &document.logical_path)?;
    let absolute_path = PathBuf::from(&document.logical_path);
    let file_name = absolute_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Protocol(format!("invalid file name: {}", absolute_path.display()))
        })?
        .to_string();
    let source_bytes: Arc<[u8]> = match &document.source {
        SearchDocumentSource::LoadedBytes(bytes) => Arc::clone(bytes),
        SearchDocumentSource::Path(path) => Arc::<[u8]>::from(fs::read(path)?),
    };
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&source_bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());

    Ok(IndexDocument {
        key: key.clone(),
        repo_id: repo_id.clone(),
        logical_path: key.logical_path,
        file_name,
        language: absolute_path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase()),
        size: document.size,
        modified_at: document.mtime_nanos,
        content_hash,
        source_bytes,
    })
}

#[cfg(test)]
mod tests {
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
        DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
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

            let snapshot = WorkspaceOverlaySnapshotState::from_paths(
                &repo_id,
                &repo,
                &documents,
                &HashSet::new(),
            )
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

        let overlay =
            WorkspaceOverlayIndex::open(&build_config(&repo)).expect("open overlay index");

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
}
