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
            eprintln!("flashgrep: workspace overlay background merge failed: {error}");
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
    const REMOVE_DIR_RETRY_ATTEMPTS: usize = 20;
    const REMOVE_DIR_RETRY_DELAY_MS: u64 = 50;

    for attempt in 0..REMOVE_DIR_RETRY_ATTEMPTS {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if is_retryable_remove_dir_error(&error) => {
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

fn is_retryable_remove_dir_error(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::DirectoryNotEmpty
        || error.kind() == std::io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(5)
        || error.raw_os_error() == Some(32)
        || error.raw_os_error() == Some(66)
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
mod tests;
