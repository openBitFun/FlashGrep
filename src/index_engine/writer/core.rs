use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

mod admin;
mod flush;
mod state;

use crate::{
    index_engine::{
        config::EngineConfig,
        delete::{DeleteStore, FileDeleteStore, MemoryDeleteStore},
        manifest::{FileManifestStore, Manifest, ManifestStore, MemoryManifestStore},
        merge::{StoreMergeExecutor, TieredMergePolicy},
        query::QueryRequest,
        reader::{
            state::{SharedSnapshotState, SnapshotData},
            IndexReaderFactory,
        },
        segment::{FileSegmentStore, MemorySegmentStore, SegmentStore},
        types::{CommitStamp, DocKey, FlushResult, OpStamp, RefreshStamp, RepoId, SeqNo},
    },
    Result,
};

pub use self::admin::IndexAdmin;
use self::{
    flush::{apply_completed_flush, build_flush_plan},
    state::{apply_add, apply_delete, build_snapshot, DocHistoryEntry, WriterState},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDocument {
    pub key: DocKey,
    pub repo_id: RepoId,
    pub logical_path: String,
    pub file_name: String,
    pub language: Option<String>,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
    pub source_bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, Default)]
pub struct WriterStats {
    pub committed_seq_no: SeqNo,
    pub last_seq_no: SeqNo,
    pub uncommitted_ops: SeqNo,
    pub pending_docs: usize,
}

pub(crate) struct CapturedCheckpoint {
    pub(crate) snapshot: SnapshotData,
    replacement_epoch: u64,
    expected_manifest_id: crate::index_engine::types::ManifestId,
}

pub struct IndexWriter {
    manifest_store: Arc<dyn ManifestStore>,
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
    state: Arc<Mutex<WriterState>>,
    snapshots: SharedSnapshotState,
}

impl IndexWriter {
    pub(crate) fn new(
        manifest_store: Arc<dyn ManifestStore>,
        segment_store: Arc<dyn SegmentStore>,
        delete_store: Arc<dyn DeleteStore>,
        snapshots: SharedSnapshotState,
    ) -> Self {
        Self {
            manifest_store,
            segment_store,
            delete_store,
            state: Arc::new(Mutex::new(WriterState::default())),
            snapshots,
        }
    }

    pub fn add_document(&self, doc: IndexDocument) -> Result<OpStamp> {
        let seq_no = self.next_seq_no();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply_add(&mut state, seq_no, doc);
        Ok(OpStamp { seq_no })
    }

    pub fn delete_document(&self, key: &DocKey) -> Result<OpStamp> {
        let seq_no = self.next_seq_no();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply_delete(&mut state, seq_no, key);
        Ok(OpStamp { seq_no })
    }

    pub(crate) fn retract_document(&self, key: &DocKey) -> Result<OpStamp> {
        let seq_no = self.next_seq_no();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply_delete(&mut state, seq_no, key);
        state.pending_retractions.insert(key.clone());
        Ok(OpStamp { seq_no })
    }

    pub fn update_document(&self, doc: IndexDocument) -> Result<OpStamp> {
        let _ = self.delete_document(&doc.key)?;
        self.add_document(doc)
    }

    pub fn rename_document(&self, old_key: &DocKey, new_doc: IndexDocument) -> Result<OpStamp> {
        let _ = self.delete_document(old_key)?;
        self.add_document(new_doc)
    }

    /// Start a replacement without altering either published reader view or the
    /// durable manifest. Caller excludes other writes/maintenance until commit.
    pub(crate) fn begin_replacement(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = WriterState {
            generation: state.generation + 1,
            replacement_epoch: state.replacement_epoch + 1,
            committed_seq_no: state.committed_seq_no,
            last_flushed_seq_no: state.last_seq_no,
            last_seq_no: state.last_seq_no + 1,
            manifest_source: state.manifest_source.take(),
            ..WriterState::default()
        };
    }

    pub fn refresh(&self) -> Result<RefreshStamp> {
        let snapshot = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation += 1;
            build_snapshot(
                &state,
                state.generation,
                state.committed_seq_no,
                state.last_seq_no,
            )
        };
        let seq_no = snapshot.manifest.refreshed_seq_no;
        self.snapshots.publish_refreshed(snapshot);
        Ok(RefreshStamp { seq_no })
    }

    pub fn flush(&self) -> Result<FlushResult> {
        self.flush_with_publication(true)
    }

    /// Stage segments without changing the query snapshot. The caller must exclude
    /// maintenance until a refresh/commit publishes these staged artifacts.
    pub(crate) fn flush_unpublished(&self) -> Result<FlushResult> {
        self.flush_with_publication(false)
    }

    fn flush_with_publication(&self, publish: bool) -> Result<FlushResult> {
        let plan = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            build_flush_plan(&state)
        };
        let Some(plan) = plan else {
            return Ok(FlushResult {
                segment_ids: Vec::new(),
                delete_segment_ids: Vec::new(),
                max_seq_no: 0,
            });
        };

        if let Some(build) = plan.segment_build.clone() {
            self.segment_store.write_segment(build)?;
        }
        if let Some(build) = plan.delete_build.clone() {
            self.delete_store.write_delete_segment(build)?;
        }

        let refreshed = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            apply_completed_flush(&mut state, &plan)
        };
        if publish {
            self.snapshots.publish_refreshed(refreshed.snapshot);
        }
        Ok(refreshed.result)
    }

    pub fn commit(&self) -> Result<CommitStamp> {
        if self.has_pending_in_memory_changes() {
            self.flush()?;
        }
        let captured = self.capture_checkpoint();
        self.persist_snapshot(&captured.snapshot.manifest)?;
        let seq_no = captured.snapshot.manifest.committed_seq_no;
        if !self.finish_checkpoint(captured)? {
            return Err(crate::AppError::InvalidIndex(
                "replacement superseded checkpoint".into(),
            ));
        }
        Ok(CommitStamp { seq_no })
    }

    pub(crate) fn visible_manifest(&self) -> Manifest {
        self.snapshots.load_refreshed().manifest
    }

    pub(crate) fn retire(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retired = true;
        state.replacement_epoch += 1;
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retired
    }

    pub(crate) fn needs_checkpoint(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.retired
            && (state.last_seq_no > state.committed_seq_no
                || state.manifest_source != self.snapshots.load_committed().manifest.source)
    }

    pub(crate) fn capture_checkpoint(&self) -> CapturedCheckpoint {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation += 1;
        let snapshot = build_snapshot(
            &state,
            state.generation,
            state.last_seq_no,
            state.last_seq_no,
        );
        debug_assert!(
            snapshot.docs.is_empty(),
            "checkpoint capture requires staged documents"
        );
        CapturedCheckpoint {
            snapshot,
            replacement_epoch: state.replacement_epoch,
            expected_manifest_id: self.snapshots.load_committed().manifest.manifest_id,
        }
    }

    /// Caller holds the workspace publication lock. Disk preparation happens before this
    /// method; writes arriving in the meantime remain in the refreshed view.
    pub(crate) fn finish_checkpoint(&self, captured: CapturedCheckpoint) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retired || state.replacement_epoch != captured.replacement_epoch {
            return Ok(false);
        }
        let durable = captured.snapshot;
        if !self
            .manifest_store
            .publish_if_current(&captured.expected_manifest_id, &durable.manifest)?
        {
            return Err(crate::AppError::InvalidIndex(
                "checkpoint manifest was replaced by another writer".into(),
            ));
        }
        let segment_ids: BTreeSet<_> = durable
            .manifest
            .active_segments
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let delete_ids: BTreeSet<_> = durable
            .manifest
            .active_delete_segments
            .iter()
            .map(|s| s.id.clone())
            .collect();
        state.active_segments = durable.manifest.active_segments.clone();
        state.active_delete_segments = durable.manifest.active_delete_segments.clone();
        state
            .pending_segments
            .retain(|s| !segment_ids.contains(&s.id));
        state
            .pending_delete_segments
            .retain(|s| !delete_ids.contains(&s.id));
        state.committed_seq_no = durable.manifest.committed_seq_no;
        state.generation += 1;
        let refreshed = if state.last_seq_no == durable.manifest.committed_seq_no
            && state.manifest_source == durable.manifest.source
        {
            durable.clone()
        } else {
            build_snapshot(
                &state,
                state.generation,
                state.committed_seq_no,
                state.last_seq_no,
            )
        };
        self.snapshots.publish_committed(durable);
        self.snapshots.publish_refreshed(refreshed);
        Ok(true)
    }

    pub(crate) fn persist_snapshot(&self, manifest: &Manifest) -> Result<()> {
        for segment in &manifest.active_segments {
            self.segment_store.persist_segment(&segment.id)?;
        }
        for segment in &manifest.active_delete_segments {
            self.delete_store.persist_delete_segment(&segment.id)?;
        }
        Ok(())
    }

    pub fn discard_uncommitted(&self) -> Result<()> {
        let manifest = self.manifest_store.load_current()?;
        self.restore_from_manifest(&manifest)?;
        Ok(())
    }

    pub fn stats(&self) -> WriterStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        WriterStats {
            committed_seq_no: state.committed_seq_no,
            last_seq_no: state.last_seq_no,
            uncommitted_ops: state.last_seq_no.saturating_sub(state.committed_seq_no),
            pending_docs: state.pending_docs.len(),
        }
    }

    pub(crate) fn set_manifest_source(
        &self,
        source: Option<crate::index_engine::manifest::ManifestSource>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.manifest_source = source;
    }

    pub(crate) fn restore_from_manifest(&self, manifest: &Manifest) -> Result<()> {
        let durable_docs = self.load_durable_history(manifest)?;
        self.restore_manifest_state(manifest, durable_docs);
        Ok(())
    }

    pub(crate) fn restore_from_manifest_only(&self, manifest: &Manifest) {
        self.restore_manifest_state(manifest, BTreeMap::new());
    }

    fn restore_manifest_state(
        &self,
        manifest: &Manifest,
        durable_docs: BTreeMap<DocKey, (SeqNo, IndexDocument)>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.replacement_epoch += 1;
        state.generation = manifest.generation;
        state.committed_seq_no = manifest.committed_seq_no;
        state.last_flushed_seq_no = manifest.committed_seq_no;
        state.last_seq_no = manifest.committed_seq_no;
        state.pending_docs.clear();
        state.pending_delete_seq_by_key.clear();
        state.pending_retractions.clear();
        state.history.clear();
        state.active_segments = manifest.active_segments.clone();
        state.active_delete_segments = manifest.active_delete_segments.clone();
        state.pending_segments.clear();
        state.pending_delete_segments.clear();
        state.manifest_source = manifest.source.clone();
        for (key, (seq_no, document)) in durable_docs {
            state.history.insert(
                key,
                vec![DocHistoryEntry {
                    seq_no,
                    document: Some(document),
                }],
            );
        }

        let committed = SnapshotData {
            manifest: manifest.clone(),
            docs: BTreeMap::new(),
            deleted_keys: BTreeSet::new(),
            ..SnapshotData::default()
        };
        self.snapshots.publish_committed(committed.clone());
        self.snapshots.publish_refreshed(committed);
    }

    fn next_seq_no(&self) -> SeqNo {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = state.last_seq_no + 1;
        state.last_seq_no = next;
        next
    }

    fn has_pending_in_memory_changes(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.pending_docs.is_empty() || !state.pending_delete_seq_by_key.is_empty()
    }

    fn load_durable_history(
        &self,
        manifest: &Manifest,
    ) -> Result<BTreeMap<DocKey, (SeqNo, IndexDocument)>> {
        let mut durable_deleted_keys = BTreeSet::<DocKey>::new();
        for delete_segment_ref in &manifest.active_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment_ref.id.clone())?;
            for record in reader.records() {
                durable_deleted_keys.insert(record.key.clone());
            }
        }

        let mut durable_docs = BTreeMap::<DocKey, (SeqNo, IndexDocument)>::new();
        for segment_ref in &manifest.active_segments {
            let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                let document = reader.document(doc_id)?;
                if durable_deleted_keys.contains(&document.key) {
                    continue;
                }
                let replace = durable_docs
                    .get(&document.key)
                    .is_none_or(|(seq_no, _)| *seq_no <= segment_ref.max_seq_no);
                if replace {
                    durable_docs.insert(
                        document.key.clone(),
                        (segment_ref.max_seq_no, document.clone()),
                    );
                }
            }
        }
        Ok(durable_docs)
    }
}

pub struct IndexEngine {
    _config: EngineConfig,
    writer: Arc<IndexWriter>,
    reader: Arc<IndexReaderFactory>,
    admin: Arc<IndexAdmin>,
}

impl IndexEngine {
    pub fn open(config: EngineConfig) -> Result<Self> {
        Self::open_with_restore_mode(config, true)
    }

    /// Volatile index with the same posting/query implementation as disk segments.
    /// Commit publishes in memory only; dropping the engine discards its contents.
    pub fn in_memory(config: EngineConfig) -> Result<Self> {
        let segments = Arc::new(MemorySegmentStore::new(config.tokenizer.clone()));
        Self::from_stores(
            config,
            Arc::new(MemoryManifestStore::default()),
            segments,
            Arc::new(MemoryDeleteStore::default()),
            true,
            SharedSnapshotState::new(),
        )
    }

    pub(crate) fn open_manifest_only(config: EngineConfig) -> Result<Self> {
        Self::open_with_restore_mode(config, false)
    }

    pub(crate) fn open_staged(config: EngineConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.index_root)?;
        let snapshots = SharedSnapshotState::for_index_root(&config.index_root)?;
        let manifest = Arc::new(FileManifestStore::open(&config.index_root)?);
        let disk = FileSegmentStore::open(&config.index_root, config.tokenizer.clone())?
            .with_query_source_cache_bytes(config.query_source_cache_bytes);
        let segments = Arc::new(crate::index_engine::segment::StagedSegmentStore::new(
            disk,
            config.tokenizer.clone(),
            256 * 1024 * 1024,
        ));
        let deletes = Arc::new(crate::index_engine::delete::StagedDeleteStore::new(
            FileDeleteStore::open(&config.index_root)?,
        ));
        Self::from_stores(config, manifest, segments, deletes, false, snapshots)
    }

    fn open_with_restore_mode(config: EngineConfig, load_durable_history: bool) -> Result<Self> {
        let manifest_store: Arc<dyn ManifestStore> =
            Arc::new(FileManifestStore::open(&config.index_root)?);
        let segment_store: Arc<dyn SegmentStore> = Arc::new(
            FileSegmentStore::open(&config.index_root, config.tokenizer.clone())?
                .with_query_source_cache_bytes(config.query_source_cache_bytes),
        );
        let delete_store: Arc<dyn DeleteStore> =
            Arc::new(FileDeleteStore::open(&config.index_root)?);
        let snapshots = SharedSnapshotState::for_index_root(&config.index_root)?;
        Self::from_stores(
            config,
            manifest_store,
            segment_store,
            delete_store,
            load_durable_history,
            snapshots,
        )
    }

    fn from_stores(
        config: EngineConfig,
        manifest_store: Arc<dyn ManifestStore>,
        segment_store: Arc<dyn SegmentStore>,
        delete_store: Arc<dyn DeleteStore>,
        load_durable_history: bool,
        snapshots: SharedSnapshotState,
    ) -> Result<Self> {
        let reader = Arc::new(IndexReaderFactory::new(
            Arc::clone(&manifest_store),
            Arc::clone(&segment_store),
            Arc::clone(&delete_store),
            snapshots.clone(),
            config.tokenizer.clone(),
        ));
        let writer = Arc::new(IndexWriter::new(
            Arc::clone(&manifest_store),
            Arc::clone(&segment_store),
            Arc::clone(&delete_store),
            snapshots.clone(),
        ));
        let admin = Arc::new(IndexAdmin::new(
            config.index_root.clone(),
            Arc::clone(&manifest_store),
            Arc::clone(&segment_store),
            Arc::clone(&delete_store),
            Arc::clone(&writer.state),
            snapshots,
            Box::new(TieredMergePolicy::new(config.merge_policy.clone())),
            Box::new(StoreMergeExecutor::new(segment_store, delete_store)),
        ));
        let engine = Self {
            _config: config,
            writer,
            reader,
            admin,
        };
        let manifest = engine.admin.manifest()?;
        if load_durable_history {
            engine.writer.restore_from_manifest(&manifest)?;
        } else {
            engine.writer.restore_from_manifest_only(&manifest);
        }
        Ok(engine)
    }

    pub fn writer(&self) -> Arc<IndexWriter> {
        Arc::clone(&self.writer)
    }

    pub fn reader(&self) -> Arc<IndexReaderFactory> {
        Arc::clone(&self.reader)
    }

    pub fn admin(&self) -> Arc<IndexAdmin> {
        Arc::clone(&self.admin)
    }

    pub fn dry_run_query(
        &self,
        query: &QueryRequest,
    ) -> Result<crate::index_engine::reader::DryRunQueryProfile> {
        self.reader.open_snapshot()?.dry_run_query(query)
    }
}

#[cfg(test)]
mod tests;
