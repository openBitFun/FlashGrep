use std::{collections::BTreeSet, sync::Arc};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    index_engine::{
        config::PostingTokenizerConfig,
        delete::{DeleteStore, FileDeleteStore},
        gc::Reclaimer,
        manifest::{FileManifestStore, ManifestSnapshot, ManifestSource, ManifestStore},
        merge::{BaseDeltaMergePolicy, MergeExecutor, MergePolicy, MergeStats, StoreMergeExecutor},
        segment::{FileSegmentStore, SegmentStore},
        ManifestId,
    },
};

use super::{
    delta::next_candidate_nonce, now_unix_ms, BaseSnapshotBuildLock, WorkspaceBaseSnapshotIndex,
};

impl WorkspaceBaseSnapshotIndex {
    pub(crate) fn compact_base_deltas(
        build_config: &BuildConfig,
        max_delta_segments: usize,
        max_delete_segments: usize,
        max_delta_bytes_ratio: f64,
        head_cache_entries: usize,
        should_cancel: &mut impl FnMut() -> bool,
    ) -> Result<usize> {
        let _build_lock = BaseSnapshotBuildLock::acquire(build_config, should_cancel)?;
        let index_root = Self::index_root(build_config);
        let manifest_store = FileManifestStore::open(&index_root)?;
        let segment_store: Arc<dyn SegmentStore> = Arc::new(FileSegmentStore::open(
            &index_root,
            PostingTokenizerConfig::from(build_config),
        )?);
        let delete_store: Arc<dyn DeleteStore> = Arc::new(FileDeleteStore::open(&index_root)?);
        let executor =
            StoreMergeExecutor::new(Arc::clone(&segment_store), Arc::clone(&delete_store));
        let policy = BaseDeltaMergePolicy::new(
            max_delta_segments,
            max_delete_segments,
            max_delta_bytes_ratio,
        );
        let mut completed = 0usize;

        loop {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            let parent = manifest_store.load_current()?;
            let stats = MergeStats {
                segment_count: parent.active_segments.len(),
                delete_segment_count: parent.active_delete_segments.len(),
            };
            let Some(task) = policy.pick_merges(&parent, &stats).into_iter().next() else {
                break;
            };
            let source_segments = task
                .source_segments
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let source_deletes = task
                .source_delete_segments
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let result = executor.execute(task, &ManifestSnapshot::new(parent.clone()))?;
            let mut candidate = parent.clone();
            candidate.manifest_id = ManifestId(format!(
                "manifest-compact-{}-{}",
                parent.manifest_id.0,
                next_candidate_nonce()
            ));
            candidate.generation = parent.generation.saturating_add(1);
            candidate.created_at_unix_ms = now_unix_ms();
            candidate
                .active_segments
                .retain(|segment| !source_segments.contains(&segment.id));
            candidate.active_segments.extend(result.output_segments);
            candidate
                .active_delete_segments
                .retain(|segment| !source_deletes.contains(&segment.id));
            candidate
                .active_delete_segments
                .extend(result.output_delete_segments);
            if let Some(ManifestSource::GitBase {
                parent_manifest_id,
                foundation_segment_ids,
                delta_depth,
                delta_bytes,
                ..
            }) = candidate.source.as_mut()
            {
                *parent_manifest_id = Some(parent.manifest_id.clone());
                let foundation = foundation_segment_ids
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                *delta_depth = candidate
                    .active_segments
                    .iter()
                    .filter(|segment| !foundation.contains(&segment.id))
                    .count() as u32;
                *delta_bytes = candidate
                    .active_segments
                    .iter()
                    .filter(|segment| !foundation.contains(&segment.id))
                    .map(|segment| segment.size_bytes)
                    .chain(
                        candidate
                            .active_delete_segments
                            .iter()
                            .map(|segment| segment.size_bytes),
                    )
                    .sum();
            }
            manifest_store.write_candidate(&candidate)?;
            if !manifest_store
                .activate_candidate_if_current(&parent.manifest_id, &candidate.manifest_id)?
            {
                continue;
            }
            completed = completed.saturating_add(1);
        }
        let active = manifest_store.load_current()?;
        Self::record_manifest_in_cache(build_config, &active, head_cache_entries)?;
        Ok(completed)
    }

    pub(crate) fn vacuum_base_artifacts(build_config: &BuildConfig) -> Result<usize> {
        let _build_lock = BaseSnapshotBuildLock::acquire(build_config, &mut || false)?;
        let index_root = Self::index_root(build_config);
        let manifest_store = FileManifestStore::open(&index_root)?;
        let segment_store =
            FileSegmentStore::open(&index_root, PostingTokenizerConfig::from(build_config))?;
        let delete_store = FileDeleteStore::open(&index_root)?;
        let pinned = Self::cached_manifest_ids(build_config)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let reclaimer = Reclaimer::default();
        let candidates = reclaimer.plan_with_pins(&index_root, &manifest_store, &pinned)?;
        reclaimer.execute(&candidates, &manifest_store, &segment_store, &delete_store)?;
        Ok(candidates.len())
    }
}
