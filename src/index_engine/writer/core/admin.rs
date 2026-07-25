use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{
    index_engine::{
        delete::DeleteStore,
        gc::Reclaimer,
        manifest::{DeleteSegmentRef, Manifest, ManifestStore, SegmentRef},
        merge::{MergeExecutor, MergePolicy, MergeStats, MergeTask},
        reader::state::{SharedSnapshotState, SnapshotData},
        segment::SegmentStore,
        types::{DeleteSegmentId, SegmentId},
    },
    Result,
};

use super::{
    state::{next_manifest_id, now_unix_ms},
    WriterState,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceStats {
    pub merge_tasks: u64,
    pub vacuumed_files: usize,
}

pub struct IndexAdmin {
    index_root: PathBuf,
    manifest_store: Arc<dyn ManifestStore>,
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
    state: Arc<Mutex<WriterState>>,
    snapshots: SharedSnapshotState,
    merge_policy: Box<dyn MergePolicy>,
    merge_executor: Box<dyn MergeExecutor>,
}

impl IndexAdmin {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        index_root: PathBuf,
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
            source: manifest.source.clone(),
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
