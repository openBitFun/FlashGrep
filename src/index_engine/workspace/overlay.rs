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
        config::{EngineConfig, MergePolicyConfig, PostingTokenizerConfig},
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
    retracted_keys: Vec<DocKey>,
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
            retracted_keys: Vec::new(),
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

    pub(crate) fn from_retracted_paths(
        repo_id: &RepoId,
        repo_root: &Path,
        paths: &[String],
    ) -> Result<Self> {
        Ok(Self {
            retracted_keys: build_delete_keys(repo_id, repo_root, paths)?,
            ..Default::default()
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WorkspaceOverlayPublishPolicy {
    #[default]
    Refresh,
    Checkpoint,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceOverlayPublication {
    Refreshed,
    Committed,
}

impl WorkspaceOverlayPublication {
    pub(crate) fn is_durable(self) -> bool {
        self == Self::Committed
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceOverlayRuntimeStats {
    pub base_manifest_id: Option<crate::index_engine::ManifestId>,
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
    checkpoint_serial: Mutex<()>,
    write_budget: Mutex<OverlayWriteBudget>,
    merge_state_hook: MergeStateHook,
}

// Bounds the source payload accumulated by the writer between staged flushes.
// This does not bound the caller's prepared batch or tokenizer expansion.
const OVERLAY_STAGE_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const OVERLAY_STAGE_OPERATIONS: usize = 1024;

#[derive(Default)]
struct OverlayWriteBudget {
    failed: bool,
    source_bytes: usize,
    operations: usize,
}

impl OverlayWriteBudget {
    fn ensure_healthy(&self) -> Result<()> {
        if self.failed {
            return Err(AppError::InvalidIndex(
                "overlay write failed; reopen before further publication or maintenance".into(),
            ));
        }
        Ok(())
    }

    fn account(&mut self, writer: &IndexWriter, bytes: usize, operations: usize) -> Result<()> {
        self.source_bytes = self.source_bytes.saturating_add(bytes);
        self.operations = self.operations.saturating_add(operations);
        if self.source_bytes >= OVERLAY_STAGE_SOURCE_BYTES
            || self.operations >= OVERLAY_STAGE_OPERATIONS
        {
            writer.flush_unpublished()?;
            self.source_bytes = 0;
            self.operations = 0;
        }
        Ok(())
    }
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
        Self::open_with_policy(build_config, WorkspaceOverlayPublishPolicy::Background)
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
        let engine_config = EngineConfig {
            // Keep commonly queried delta content hot without hydrating the base.
            query_source_cache_bytes: 512 * 1024 * 1024,
            // Keep background tokenization bounded even when the dirty set is huge.
            merge_policy: MergePolicyConfig {
                max_merged_segment_bytes: 64 * 1024 * 1024,
                ..MergePolicyConfig::default()
            },
            shard_id: ShardId(format!(
                "{}#workspace-overlay",
                build_config.repo_path.to_string_lossy()
            )),
            index_root: workspace_overlay_index_root(build_config),
            tokenizer: PostingTokenizerConfig::from(build_config),
            ..EngineConfig::default()
        };
        let engine = IndexEngine::open_staged(engine_config)?;
        Ok(Self::new_with_policy_and_hook(
            engine,
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
            checkpoint_serial: Mutex::new(()),
            write_budget: Mutex::new(OverlayWriteBudget::default()),
            merge_state_hook,
        }
    }

    pub fn open_snapshot(&self) -> Result<IndexSnapshotReader> {
        self.engine.reader().open_snapshot()
    }

    pub(crate) fn retire(&self) {
        self.engine.writer().retire();
    }

    pub(crate) fn needs_checkpoint(&self) -> bool {
        self.engine.writer().needs_checkpoint()
    }

    pub(crate) fn writer_stats(&self) -> WriterStats {
        self.engine.writer().stats()
    }

    pub(crate) fn set_manifest_source(
        &self,
        source: crate::index_engine::manifest::ManifestSource,
    ) {
        self.engine.writer().set_manifest_source(Some(source));
    }

    pub(crate) fn manifest_source(
        &self,
    ) -> Result<Option<crate::index_engine::manifest::ManifestSource>> {
        Ok(self.engine.writer().visible_manifest().source)
    }

    pub(crate) fn runtime_stats(&self) -> Result<WorkspaceOverlayRuntimeStats> {
        let writer_stats = self.engine.writer().stats();
        let manifest = self.engine.writer().visible_manifest();
        let state = match self.merge_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(WorkspaceOverlayRuntimeStats {
            base_manifest_id: match manifest.source.as_ref() {
                Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                    base_manifest_id,
                    ..
                }) => Some(base_manifest_id.clone()),
                _ => None,
            },
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
        let _serial = self
            .checkpoint_serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Vacuum must not reclaim staged, unpublished segments. Queries continue
        // reading their previous snapshot while this lock serializes mutations.
        let write_guard = self
            .write_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write_guard.ensure_healthy()?;
        if self.engine.writer().is_retired() {
            return Ok(false);
        }
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

        let writer = self.engine.writer();
        let captured = writer
            .flush_unpublished()
            .map(|_| (writer.needs_checkpoint()).then(|| writer.capture_checkpoint()));
        drop(write_guard);
        // Query publication and discovery continue while immutable artifacts are encoded.
        let prepared = captured.and_then(|captured| {
            if let Some(c) = &captured {
                writer.persist_snapshot(&c.snapshot.manifest)?;
            }
            Ok(captured)
        });
        let write_guard = self
            .write_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let merge_result = (|| {
            if let Some(captured) = prepared? {
                if !writer.finish_checkpoint(captured)? {
                    return Ok(Default::default());
                }
            }
            if writer.is_retired() {
                return Ok(Default::default());
            }
            // A newer publication must not be replaced by a durable-only merge snapshot.
            if writer.needs_checkpoint() {
                return Ok(Default::default());
            }
            self.engine.admin().maintain_step()
        })();
        if let Err(error) = &merge_result {
            eprintln!(
                "flashgrep[overlay][warn] workspace overlay background merge failed: {error}"
            );
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
        drop(write_guard);
        if merge_result
            .as_ref()
            .is_ok_and(|stats| stats.merge_tasks > 0 || writer.needs_checkpoint())
        {
            // Do not drain every task while holding the writer budget. The
            // maintenance worker gets a chance to publish queued file changes
            // before the next bounded merge. A final no-op step clears the request.
            self.request_background_merge();
        }
        (self.merge_state_hook)(WorkspaceOverlayMergeEvent::StateChanged);
        merge_result.map(|stats| stats.merge_tasks > 0 || stats.vacuumed_files > 0)
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        let _serial = self
            .checkpoint_serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut budget = self
            .write_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        budget.ensure_healthy()?;
        budget.failed = true;
        let writer = self.engine.writer();
        let _ = writer.flush()?;
        let _ = writer.commit()?;
        *budget = OverlayWriteBudget::default();
        self.request_background_merge();
        Ok(())
    }

    pub(crate) fn rebuild_snapshot(&self, snapshot: &WorkspaceOverlaySnapshotState) -> Result<()> {
        self.replace_batches(std::iter::once(Ok(WorkspaceOverlayIngestBatch {
            new_documents: snapshot.existing_documents.clone(),
            deleted_keys: snapshot.deleted_keys.clone(),
            ..Default::default()
        })))?;
        Ok(())
    }

    pub(crate) fn ingest_batch(
        &self,
        batch: &WorkspaceOverlayIngestBatch,
    ) -> Result<WorkspaceOverlayPublication> {
        self.ingest_batches(std::iter::once(Ok(batch)))
    }

    /// Lazily prepare each input batch while preserving one publication boundary.
    pub(crate) fn ingest_batches<I, B>(&self, batches: I) -> Result<WorkspaceOverlayPublication>
    where
        I: IntoIterator<Item = Result<B>>,
        B: std::borrow::Borrow<WorkspaceOverlayIngestBatch>,
    {
        self.write_batches(batches, false)
    }

    pub(crate) fn replace_batches<I, B>(&self, batches: I) -> Result<WorkspaceOverlayPublication>
    where
        I: IntoIterator<Item = Result<B>>,
        B: std::borrow::Borrow<WorkspaceOverlayIngestBatch>,
    {
        self.write_batches(batches, true)
    }

    fn write_batches<I, B>(&self, batches: I, replace: bool) -> Result<WorkspaceOverlayPublication>
    where
        I: IntoIterator<Item = Result<B>>,
        B: std::borrow::Borrow<WorkspaceOverlayIngestBatch>,
    {
        let mut budget = self
            .write_budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        budget.ensure_healthy()?;
        budget.failed = true;
        let writer = self.engine.writer();
        if replace {
            writer.begin_replacement();
            budget.source_bytes = 0;
            budget.operations = 0;
        }

        for batch in batches {
            let batch = batch?;
            let batch = batch.borrow();
            for key in &batch.deleted_keys {
                writer.delete_document(key)?;
                budget.account(&writer, 0, 1)?;
            }
            for document in &batch.modified_documents {
                writer.update_document(document.clone())?;
                budget.account(&writer, document.source_bytes.len(), 2)?;
            }
            for document in &batch.new_documents {
                writer.add_document(document.clone())?;
                budget.account(&writer, document.source_bytes.len(), 1)?;
            }
            for key in &batch.missing_keys {
                writer.delete_document(key)?;
                budget.account(&writer, 0, 1)?;
            }
            for key in &batch.retracted_keys {
                writer.retract_document(key)?;
                budget.account(&writer, 0, 1)?;
            }
        }
        // Empty input still publishes updated manifest source metadata.
        let publication = if replace {
            writer.flush_unpublished()?;
            let publication = if matches!(
                self.publish_policy,
                WorkspaceOverlayPublishPolicy::Background
            ) {
                self.publish_after_write(&writer)?
            } else {
                writer.commit()?;
                WorkspaceOverlayPublication::Committed
            };
            self.request_background_merge();
            publication
        } else {
            self.publish_after_write(&writer)?
        };
        if publication.is_durable() {
            *budget = OverlayWriteBudget::default();
        }
        budget.failed = false;
        Ok(publication)
    }

    fn publish_after_write(
        &self,
        writer: &Arc<IndexWriter>,
    ) -> Result<WorkspaceOverlayPublication> {
        match self.publish_policy {
            WorkspaceOverlayPublishPolicy::Refresh => {
                writer.refresh()?;
                Ok(WorkspaceOverlayPublication::Refreshed)
            }
            WorkspaceOverlayPublishPolicy::Checkpoint => {
                let _ = writer.commit()?;
                self.request_background_merge();
                Ok(WorkspaceOverlayPublication::Committed)
            }
            WorkspaceOverlayPublishPolicy::Background => {
                writer.flush_unpublished()?;
                writer.refresh()?;
                self.request_background_merge();
                Ok(WorkspaceOverlayPublication::Refreshed)
            }
        }
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
