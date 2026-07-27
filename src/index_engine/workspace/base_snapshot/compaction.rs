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

#[derive(Default)]
struct BaseCompactionMetrics {
    input_bytes: u64,
    output_bytes: u64,
    live_segments: usize,
    delete_segments: usize,
}

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
        let compaction_started = std::time::Instant::now();
        let mut completed = 0usize;
        let mut metrics = BaseCompactionMetrics::default();

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
            let merge_input_bytes = parent
                .active_segments
                .iter()
                .filter(|segment| source_segments.contains(&segment.id))
                .map(|segment| segment.size_bytes)
                .chain(
                    parent
                        .active_delete_segments
                        .iter()
                        .filter(|segment| source_deletes.contains(&segment.id))
                        .map(|segment| segment.size_bytes),
                )
                .sum::<u64>();
            let merge_started = std::time::Instant::now();
            let result = executor.execute(task, &ManifestSnapshot::new(parent.clone()))?;
            let merge_output_bytes = result
                .output_segments
                .iter()
                .map(|segment| segment.size_bytes)
                .chain(
                    result
                        .output_delete_segments
                        .iter()
                        .map(|segment| segment.size_bytes),
                )
                .sum::<u64>();
            metrics.input_bytes = metrics.input_bytes.saturating_add(merge_input_bytes);
            metrics.output_bytes = metrics.output_bytes.saturating_add(merge_output_bytes);
            metrics.live_segments = metrics.live_segments.saturating_add(source_segments.len());
            metrics.delete_segments = metrics.delete_segments.saturating_add(source_deletes.len());
            eprintln!(
                "[flashgrep][base-compaction-metrics] stage=merge elapsed_ms={} live_segments={} delete_segments={} input_bytes={} output_bytes={}",
                merge_started.elapsed().as_millis(),
                source_segments.len(),
                source_deletes.len(),
                merge_input_bytes,
                merge_output_bytes,
            );
            let mut candidate = parent.clone();
            candidate.manifest_id = next_compaction_manifest_id(&parent);
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
        eprintln!(
            "[flashgrep][base-compaction-metrics] stage=complete elapsed_ms={} merge_tasks={} live_segments={} delete_segments={} input_bytes={} output_bytes={}",
            compaction_started.elapsed().as_millis(),
            completed,
            metrics.live_segments,
            metrics.delete_segments,
            metrics.input_bytes,
            metrics.output_bytes,
        );
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

fn next_compaction_manifest_id(parent: &crate::index_engine::manifest::Manifest) -> ManifestId {
    ManifestId(format!(
        "manifest-compact-g{}-{}",
        parent.generation,
        next_candidate_nonce()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::manifest::Manifest;

    #[test]
    fn compaction_manifest_id_does_not_embed_an_unbounded_parent_id() {
        let parent = Manifest {
            manifest_id: ManifestId("parent-".repeat(100)),
            generation: 42,
            ..Manifest::default()
        };

        let candidate = next_compaction_manifest_id(&parent);

        assert!(candidate.0.starts_with("manifest-compact-g42-"));
        assert!(candidate.0.len() < 96);
        assert!(!candidate.0.contains(&parent.manifest_id.0));
    }
}
