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
        delete::{DeleteStore, FileDeleteStore},
        manifest::{FileManifestStore, Manifest, ManifestStore},
        merge::{StoreMergeExecutor, TieredMergePolicy},
        query::QueryRequest,
        reader::{
            state::{SharedSnapshotState, SnapshotData},
            IndexReaderFactory,
        },
        segment::{FileSegmentStore, SegmentStore},
        types::{CommitStamp, DocKey, FlushResult, OpStamp, RefreshStamp, RepoId, SeqNo},
    },
    Result,
};

pub use self::admin::IndexAdmin;
use self::{
    flush::{apply_completed_flush, build_flush_plan},
    state::{
        apply_add, apply_delete, build_snapshot, promote_pending_segments, DocHistoryEntry,
        SnapshotKind, WriterState,
    },
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

    pub fn update_document(&self, doc: IndexDocument) -> Result<OpStamp> {
        let _ = self.delete_document(&doc.key)?;
        self.add_document(doc)
    }

    pub fn rename_document(&self, old_key: &DocKey, new_doc: IndexDocument) -> Result<OpStamp> {
        let _ = self.delete_document(old_key)?;
        self.add_document(new_doc)
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
                SnapshotKind::Refreshed,
            )
        };
        let seq_no = snapshot.manifest.refreshed_seq_no;
        self.snapshots.publish_refreshed(snapshot);
        Ok(RefreshStamp { seq_no })
    }

    pub fn flush(&self) -> Result<FlushResult> {
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
        self.snapshots.publish_refreshed(refreshed.snapshot);
        Ok(refreshed.result)
    }

    pub fn commit(&self) -> Result<CommitStamp> {
        if self.has_pending_in_memory_changes() {
            self.flush()?;
        }

        let snapshot = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.committed_seq_no = state.last_seq_no;
            promote_pending_segments(&mut state);
            state.generation += 1;
            build_snapshot(
                &state,
                state.generation,
                state.committed_seq_no,
                state.committed_seq_no,
                SnapshotKind::Committed,
            )
        };
        self.manifest_store.publish(&snapshot.manifest)?;
        self.snapshots.publish_refreshed(snapshot.clone());
        self.snapshots.publish_committed(snapshot.clone());
        Ok(CommitStamp {
            seq_no: snapshot.manifest.committed_seq_no,
        })
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

    pub(crate) fn restore_from_manifest(&self, manifest: &Manifest) -> Result<()> {
        let durable_docs = self.load_durable_history(manifest)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = manifest.generation;
        state.committed_seq_no = manifest.committed_seq_no;
        state.last_flushed_seq_no = manifest.committed_seq_no;
        state.last_seq_no = manifest.committed_seq_no;
        state.pending_docs.clear();
        state.pending_delete_seq_by_key.clear();
        state.history.clear();
        state.active_segments = manifest.active_segments.clone();
        state.active_delete_segments = manifest.active_delete_segments.clone();
        state.pending_segments.clear();
        state.pending_delete_segments.clear();
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
        Ok(())
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
        let manifest_store: Arc<dyn ManifestStore> =
            Arc::new(FileManifestStore::open(&config.index_root)?);
        let segment_store: Arc<dyn SegmentStore> = Arc::new(FileSegmentStore::open(
            &config.index_root,
            config.tokenizer.clone(),
        )?);
        let delete_store: Arc<dyn DeleteStore> =
            Arc::new(FileDeleteStore::open(&config.index_root)?);
        let snapshots = SharedSnapshotState::new();
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
        engine.writer.restore_from_manifest(&manifest)?;
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
