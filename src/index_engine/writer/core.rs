use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    index_engine::{
        config::EngineConfig,
        delete::{DeleteRecord, DeleteStore, FileDeleteStore},
        gc::Reclaimer,
        manifest::{DeleteSegmentRef, FileManifestStore, Manifest, ManifestStore, SegmentRef},
        merge::{
            MergeExecutor, MergePolicy, MergeStats, MergeTask, StoreMergeExecutor,
            TieredMergePolicy,
        },
        query::QueryRequest,
        reader::{
            state::{SharedSnapshotState, SnapshotData, VersionedDocument},
            IndexReaderFactory,
        },
        segment::{
            DeleteSegmentBuildOutput, DeleteSegmentMeta, FileSegmentStore, SegmentBuildOutput,
            SegmentMeta, SegmentStore,
        },
        types::{
            CommitStamp, DeleteSegmentId, DocKey, FlushResult, OpStamp, RefreshStamp, RepoId,
            SegmentId, SeqNo,
        },
    },
    Result,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceStats {
    pub merge_tasks: u64,
    pub vacuumed_files: usize,
}

#[derive(Debug, Clone)]
struct PendingDocument {
    seq_no: SeqNo,
    document: IndexDocument,
}

#[derive(Debug, Default)]
pub(super) struct WriterState {
    pub(super) generation: u64,
    pub(super) committed_seq_no: SeqNo,
    pub(super) last_flushed_seq_no: SeqNo,
    pub(super) last_seq_no: SeqNo,
    pending_docs: HashMap<DocKey, PendingDocument>,
    pending_delete_seq_by_key: HashMap<DocKey, SeqNo>,
    history: HashMap<DocKey, Vec<DocHistoryEntry>>,
    pub(super) active_segments: Vec<SegmentRef>,
    pub(super) active_delete_segments: Vec<DeleteSegmentRef>,
    pub(super) pending_segments: Vec<SegmentRef>,
    pub(super) pending_delete_segments: Vec<DeleteSegmentRef>,
}

#[derive(Debug, Clone)]
struct DocHistoryEntry {
    seq_no: SeqNo,
    document: Option<IndexDocument>,
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

#[derive(Debug, Clone)]
struct FlushPlan {
    segment_build: Option<SegmentBuildOutput>,
    segment_ref: Option<SegmentRef>,
    delete_build: Option<DeleteSegmentBuildOutput>,
    delete_segment_ref: Option<DeleteSegmentRef>,
    max_seq_no: SeqNo,
}

#[derive(Debug, Clone)]
struct AppliedFlush {
    result: FlushResult,
    snapshot: SnapshotData,
}

fn build_flush_plan(state: &WriterState) -> Option<FlushPlan> {
    if state.pending_docs.is_empty() && state.pending_delete_seq_by_key.is_empty() {
        return None;
    }

    let generation = state.generation + 1;
    let next_segment_ordinal = state.active_segments.len() + state.pending_segments.len() + 1;
    let next_delete_ordinal =
        state.active_delete_segments.len() + state.pending_delete_segments.len() + 1;

    let mut live_docs = state
        .pending_docs
        .values()
        .map(|pending| pending.document.clone())
        .collect::<Vec<_>>();
    live_docs.sort_by(|left, right| left.key.cmp(&right.key));

    let segment = if live_docs.is_empty() {
        None
    } else {
        let segment_id = SegmentId(format!("seg-{generation:016}-{next_segment_ordinal:016}"));
        let max_seq_no = live_docs
            .iter()
            .filter_map(|doc| {
                state
                    .pending_docs
                    .get(&doc.key)
                    .map(|pending| pending.seq_no)
            })
            .max()
            .unwrap_or_default();
        let min_seq_no = live_docs
            .iter()
            .filter_map(|doc| {
                state
                    .pending_docs
                    .get(&doc.key)
                    .map(|pending| pending.seq_no)
            })
            .min()
            .unwrap_or_default();
        let meta = SegmentMeta {
            segment_id: segment_id.clone(),
            min_seq_no,
            max_seq_no,
            doc_count: live_docs.len() as u32,
            live_doc_count: live_docs.len() as u32,
            created_by_merge: false,
        };
        let segment_ref = SegmentRef {
            id: segment_id,
            min_seq_no,
            max_seq_no,
            doc_count: meta.doc_count,
            size_bytes: live_docs
                .iter()
                .map(|doc| doc.source_bytes.len() as u64)
                .sum(),
        };
        Some((
            SegmentBuildOutput {
                meta,
                documents: live_docs,
            },
            segment_ref,
        ))
    };

    let delete_segment = build_delete_segment(state, generation, next_delete_ordinal);
    let max_seq_no = state
        .pending_docs
        .values()
        .map(|pending| pending.seq_no)
        .chain(state.pending_delete_seq_by_key.values().copied())
        .max()
        .unwrap_or_default();

    Some(FlushPlan {
        segment_build: segment.clone().map(|(build, _)| build),
        segment_ref: segment.map(|(_, segment_ref)| segment_ref),
        delete_build: delete_segment.clone().map(|(build, _)| build),
        delete_segment_ref: delete_segment.map(|(_, delete_ref)| delete_ref),
        max_seq_no,
    })
}

fn build_delete_segment(
    state: &WriterState,
    generation: u64,
    ordinal: usize,
) -> Option<(DeleteSegmentBuildOutput, DeleteSegmentRef)> {
    if state.pending_delete_seq_by_key.is_empty() {
        return None;
    }

    let mut records = state
        .pending_delete_seq_by_key
        .iter()
        .map(|(key, seq_no)| DeleteRecord {
            seq_no: *seq_no,
            key: key.clone(),
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.seq_no
            .cmp(&right.seq_no)
            .then_with(|| left.key.cmp(&right.key))
    });

    let delete_segment_id = DeleteSegmentId(format!("del-{generation:016}-{ordinal:016}"));
    let min_seq_no = records
        .iter()
        .map(|record| record.seq_no)
        .min()
        .unwrap_or_default();
    let max_seq_no = records
        .iter()
        .map(|record| record.seq_no)
        .max()
        .unwrap_or_default();
    let meta = DeleteSegmentMeta {
        delete_segment_id: delete_segment_id.clone(),
        min_seq_no,
        max_seq_no,
        delete_count: records.len() as u32,
    };
    let segment_ref = DeleteSegmentRef {
        id: delete_segment_id,
        min_seq_no,
        max_seq_no,
        delete_count: records.len() as u32,
        size_bytes: records
            .iter()
            .map(|record| delete_record_size_bytes(&record.key) as u64)
            .sum(),
    };

    Some((
        DeleteSegmentBuildOutput {
            meta,
            records: records.clone(),
        },
        segment_ref,
    ))
}

fn apply_completed_flush(state: &mut WriterState, plan: &FlushPlan) -> AppliedFlush {
    state.last_flushed_seq_no = plan.max_seq_no;
    state.pending_docs.clear();
    state.pending_delete_seq_by_key.clear();
    if let Some(segment_ref) = plan.segment_ref.clone() {
        state.pending_segments.push(segment_ref);
    }
    if let Some(delete_segment_ref) = plan.delete_segment_ref.clone() {
        state.pending_delete_segments.push(delete_segment_ref);
    }
    let generation = state.generation + 1;
    let snapshot = build_snapshot(
        state,
        generation,
        state.committed_seq_no,
        state.last_seq_no,
        SnapshotKind::Refreshed,
    );

    AppliedFlush {
        result: FlushResult {
            segment_ids: plan
                .segment_ref
                .clone()
                .into_iter()
                .map(|segment| segment.id)
                .collect(),
            delete_segment_ids: plan
                .delete_segment_ref
                .clone()
                .into_iter()
                .map(|segment| segment.id)
                .collect(),
            max_seq_no: plan.max_seq_no,
        },
        snapshot,
    }
}

pub struct IndexAdmin {
    index_root: std::path::PathBuf,
    manifest_store: Arc<dyn ManifestStore>,
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
    state: Arc<Mutex<WriterState>>,
    snapshots: SharedSnapshotState,
    merge_policy: Box<dyn MergePolicy>,
    merge_executor: Box<dyn MergeExecutor>,
}

impl IndexAdmin {
    fn new(
        index_root: std::path::PathBuf,
        manifest_store: Arc<dyn ManifestStore>,
        segment_store: Arc<dyn SegmentStore>,
        delete_store: Arc<dyn DeleteStore>,
        state: Arc<Mutex<WriterState>>,
        snapshots: SharedSnapshotState,
        merge_policy: Box<dyn MergePolicy>,
        merge_executor: Box<dyn MergeExecutor>,
    ) -> Self {
        Self {
            index_root,
            manifest_store,
            segment_store,
            delete_store,
            state,
            snapshots,
            merge_policy,
            merge_executor,
        }
    }

    pub fn manifest(&self) -> Result<Manifest> {
        self.manifest_store.load_current()
    }

    pub fn segments(&self) -> Result<Vec<SegmentRef>> {
        Ok(self.manifest()?.active_segments)
    }

    pub fn delete_segments(&self) -> Result<Vec<DeleteSegmentRef>> {
        Ok(self.manifest()?.active_delete_segments)
    }

    pub fn maybe_merge(&self) -> Result<bool> {
        let manifest = self.manifest()?;
        let stats = MergeStats {
            segment_count: manifest.active_segments.len(),
            delete_segment_count: manifest.active_delete_segments.len(),
        };
        let Some(task) = self
            .merge_policy
            .pick_merges(&manifest, &stats)
            .into_iter()
            .next()
        else {
            return Ok(false);
        };
        self.execute_merge_task(&manifest, task)?;
        Ok(true)
    }

    pub fn maintain(&self) -> Result<MaintenanceStats> {
        let mut merge_tasks = 0u64;
        while self.maybe_merge()? {
            merge_tasks += 1;
        }
        let vacuumed_files = self.vacuum()?;
        Ok(MaintenanceStats {
            merge_tasks,
            vacuumed_files,
        })
    }

    pub fn force_merge(&self, max_segments: usize) -> Result<()> {
        let manifest = self.manifest()?;
        if manifest.active_segments.len() <= max_segments
            && manifest.active_delete_segments.is_empty()
        {
            let _ = self.vacuum()?;
            return Ok(());
        }
        self.execute_merge_task(
            &manifest,
            MergeTask {
                source_segments: manifest
                    .active_segments
                    .iter()
                    .map(|segment| segment.id.clone())
                    .collect(),
                source_delete_segments: manifest
                    .active_delete_segments
                    .iter()
                    .map(|segment| segment.id.clone())
                    .collect(),
            },
        )?;
        let _ = self.vacuum()?;
        Ok(())
    }

    pub fn vacuum(&self) -> Result<usize> {
        let reclaimer = Reclaimer::default();
        let pinned_manifest_ids = self.snapshots.pinned_manifest_ids();
        let candidates = reclaimer.plan_with_pins(
            &self.index_root,
            self.manifest_store.as_ref(),
            &pinned_manifest_ids,
        )?;
        let candidate_count = candidates.len();
        reclaimer.execute(
            &candidates,
            self.manifest_store.as_ref(),
            self.segment_store.as_ref(),
            self.delete_store.as_ref(),
        )?;
        Ok(candidate_count)
    }

    pub fn recover(&self) -> Result<()> {
        let manifest = self.manifest()?;
        let snapshot = SnapshotData {
            manifest,
            docs: BTreeMap::new(),
            deleted_keys: BTreeSet::new(),
            ..SnapshotData::default()
        };
        self.snapshots.publish_committed(snapshot.clone());
        self.snapshots.publish_refreshed(snapshot);
        Ok(())
    }

    fn execute_merge_task(&self, manifest: &Manifest, task: MergeTask) -> Result<()> {
        let base_manifest_id = manifest.manifest_id.clone();
        let source_segment_ids = task
            .source_segments
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let source_delete_segment_ids = task
            .source_delete_segments
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let result = self.merge_executor.execute(
            task,
            &crate::index_engine::manifest::ManifestSnapshot::new(manifest.clone()),
        )?;
        let next_generation = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation + 1
        };
        let merged_manifest = Manifest {
            manifest_id: next_manifest_id(next_generation),
            generation: next_generation,
            created_at_unix_ms: now_unix_ms(),
            committed_seq_no: manifest.committed_seq_no,
            refreshed_seq_no: manifest.committed_seq_no,
            writer_epoch: manifest.writer_epoch,
            schema_fingerprint: manifest.schema_fingerprint.clone(),
            active_segments: merge_active_segments(
                &manifest.active_segments,
                &source_segment_ids,
                result.output_segments.clone(),
            ),
            active_delete_segments: merge_active_delete_segments(
                &manifest.active_delete_segments,
                &source_delete_segment_ids,
                result.output_delete_segments.clone(),
            ),
        };
        if !self
            .manifest_store
            .publish_if_current(&base_manifest_id, &merged_manifest)?
        {
            return Ok(());
        }
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.generation = next_generation;
            state.committed_seq_no = manifest.committed_seq_no;
            state.last_flushed_seq_no = manifest.committed_seq_no;
            state.last_seq_no = state.last_seq_no.max(manifest.committed_seq_no);
            state.active_segments = merged_manifest.active_segments.clone();
            state.active_delete_segments = merged_manifest.active_delete_segments.clone();
        }
        let snapshot = SnapshotData {
            manifest: merged_manifest,
            docs: BTreeMap::new(),
            deleted_keys: BTreeSet::new(),
            ..SnapshotData::default()
        };
        self.snapshots.publish_committed(snapshot.clone());
        self.snapshots.publish_refreshed(snapshot);
        Ok(())
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

#[derive(Debug, Clone, Copy)]
pub(super) enum SnapshotKind {
    Committed,
    Refreshed,
}

pub(super) fn build_snapshot(
    state: &WriterState,
    generation: u64,
    committed_seq_no: SeqNo,
    refreshed_seq_no: SeqNo,
    snapshot_kind: SnapshotKind,
) -> SnapshotData {
    let mut docs = BTreeMap::new();
    let mut deleted_keys = BTreeSet::new();

    for (key, entries) in &state.history {
        let Some(latest) = latest_state_entry(entries, refreshed_seq_no) else {
            continue;
        };
        if latest.seq_no <= state.last_flushed_seq_no {
            continue;
        }
        if let Some(document) = latest.document.clone() {
            docs.insert(
                key.clone(),
                VersionedDocument {
                    seq_no: latest.seq_no,
                    document,
                },
            );
        } else {
            deleted_keys.insert(key.clone());
        }
    }

    SnapshotData {
        manifest: Manifest {
            manifest_id: next_manifest_id(generation),
            generation,
            created_at_unix_ms: now_unix_ms(),
            committed_seq_no,
            refreshed_seq_no,
            writer_epoch: 0,
            schema_fingerprint: "segment-publish-v2".into(),
            active_segments: visible_segment_refs(state, snapshot_kind),
            active_delete_segments: visible_delete_segment_refs(state, snapshot_kind),
        },
        docs,
        deleted_keys,
        ..SnapshotData::default()
    }
}

pub(super) fn next_manifest_id(generation: u64) -> crate::index_engine::types::ManifestId {
    crate::index_engine::types::ManifestId(format!("manifest-{generation:016}"))
}

fn visible_segment_refs(state: &WriterState, snapshot_kind: SnapshotKind) -> Vec<SegmentRef> {
    let mut segments = state.active_segments.clone();
    if matches!(snapshot_kind, SnapshotKind::Refreshed) {
        segments.extend(state.pending_segments.clone());
    }
    segments
}

fn visible_delete_segment_refs(
    state: &WriterState,
    snapshot_kind: SnapshotKind,
) -> Vec<DeleteSegmentRef> {
    let mut segments = state.active_delete_segments.clone();
    if matches!(snapshot_kind, SnapshotKind::Refreshed) {
        segments.extend(state.pending_delete_segments.clone());
    }
    segments
}

fn apply_add(state: &mut WriterState, seq_no: SeqNo, doc: IndexDocument) {
    state.pending_delete_seq_by_key.remove(&doc.key);
    state.pending_docs.insert(
        doc.key.clone(),
        PendingDocument {
            seq_no,
            document: doc.clone(),
        },
    );
    state
        .history
        .entry(doc.key.clone())
        .or_default()
        .push(DocHistoryEntry {
            seq_no,
            document: Some(doc),
        });
    state.last_seq_no = state.last_seq_no.max(seq_no);
}

fn apply_delete(state: &mut WriterState, seq_no: SeqNo, key: &DocKey) {
    state.pending_docs.remove(key);
    state.pending_delete_seq_by_key.insert(key.clone(), seq_no);
    state
        .history
        .entry(key.clone())
        .or_default()
        .push(DocHistoryEntry {
            seq_no,
            document: None,
        });
    state.last_seq_no = state.last_seq_no.max(seq_no);
}

fn latest_state_entry(entries: &[DocHistoryEntry], upper_bound: SeqNo) -> Option<&DocHistoryEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.seq_no <= upper_bound)
}

pub(super) fn promote_pending_segments(state: &mut WriterState) {
    if !state.pending_segments.is_empty() {
        state.active_segments.append(&mut state.pending_segments);
    }
    if !state.pending_delete_segments.is_empty() {
        state
            .active_delete_segments
            .append(&mut state.pending_delete_segments);
    }
}

fn merge_active_segments(
    existing: &[SegmentRef],
    source_ids: &BTreeSet<SegmentId>,
    mut outputs: Vec<SegmentRef>,
) -> Vec<SegmentRef> {
    let mut segments = existing
        .iter()
        .filter(|segment| !source_ids.contains(&segment.id))
        .cloned()
        .collect::<Vec<_>>();
    segments.append(&mut outputs);
    segments.sort_by(|left, right| {
        left.max_seq_no
            .cmp(&right.max_seq_no)
            .then_with(|| left.min_seq_no.cmp(&right.min_seq_no))
    });
    segments
}

fn merge_active_delete_segments(
    existing: &[DeleteSegmentRef],
    source_ids: &BTreeSet<DeleteSegmentId>,
    mut outputs: Vec<DeleteSegmentRef>,
) -> Vec<DeleteSegmentRef> {
    let mut segments = existing
        .iter()
        .filter(|segment| !source_ids.contains(&segment.id))
        .cloned()
        .collect::<Vec<_>>();
    segments.append(&mut outputs);
    segments.sort_by(|left, right| {
        left.max_seq_no
            .cmp(&right.max_seq_no)
            .then_with(|| left.min_seq_no.cmp(&right.min_seq_no))
    });
    segments
}

fn delete_record_size_bytes(key: &DocKey) -> usize {
    key.repo_id.0.len() + key.logical_path.len() + std::mem::size_of::<SeqNo>()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
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
        QueryRequest {
            pattern: pattern.into(),
            mode: SearchMode::MaterializeMatches,
            case_insensitive: false,
            top_k: None,
            filters: QueryFilters::default(),
            prefer_worktree_for_git_head_blob: false,
        }
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
            .filter_map(|entry| entry.ok())
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
}
