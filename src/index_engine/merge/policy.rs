use std::collections::BTreeSet;

use crate::index_engine::{
    config::MergePolicyConfig,
    manifest::{Manifest, ManifestSource},
};

use super::{MergeStats, MergeTask};

pub trait MergePolicy: Send + Sync {
    fn pick_merges(&self, manifest: &Manifest, stats: &MergeStats) -> Vec<MergeTask>;
}

#[derive(Debug, Clone, Default)]
pub struct TieredMergePolicy {
    config: MergePolicyConfig,
}

impl TieredMergePolicy {
    pub fn new(config: MergePolicyConfig) -> Self {
        Self { config }
    }
}

impl MergePolicy for TieredMergePolicy {
    fn pick_merges(&self, manifest: &Manifest, stats: &MergeStats) -> Vec<MergeTask> {
        // Current policy intentionally stays minimal:
        // - If any delete segment exists, compact deletes into the affected live segments first.
        // - Otherwise, if live segment count exceeds max_segments, run a simple tiered merge.
        // It does not yet score merges by deleted-doc ratio or other heuristics.
        if stats.delete_segment_count > 0 {
            let Some(max_delete_seq_no) = manifest
                .active_delete_segments
                .iter()
                .map(|segment| segment.max_seq_no)
                .max()
            else {
                return Vec::new();
            };
            let mut source_segments = Vec::new();
            let mut source_bytes = 0u64;
            for segment in manifest.active_segments.iter().filter(|segment| {
                segment.max_seq_no <= max_delete_seq_no
                    && segment.applied_delete_seq_no < max_delete_seq_no
            }) {
                let bytes = segment.size_bytes.max(self.config.floor_segment_bytes);
                // One legacy oversized segment must still make progress. New
                // segments are bounded by the ingest/merge source budgets.
                if !source_segments.is_empty()
                    && source_bytes.saturating_add(bytes) > self.config.max_merged_segment_bytes
                {
                    break;
                }
                source_bytes = source_bytes.saturating_add(bytes);
                source_segments.push(segment.id.clone());
            }
            if !source_segments.is_empty() || stats.delete_segment_count > 1 {
                return vec![MergeTask {
                    source_segments,
                    source_delete_segments: manifest
                        .active_delete_segments
                        .iter()
                        .map(|segment| segment.id.clone())
                        .collect(),
                }];
            }
            // A single retained overlay mask is already compact. It must not
            // retrigger the same merge or prevent ordinary tiered merging.
        }

        if stats.segment_count <= self.config.max_segments {
            return Vec::new();
        }

        let mut candidates = manifest.active_segments.to_vec();
        // Merge smaller segments first. A budget-sized old segment must not
        // prevent later small segments from being compacted indefinitely.
        candidates.sort_by_key(|segment| {
            (
                segment.size_bytes.max(self.config.floor_segment_bytes),
                segment.max_seq_no,
                segment.min_seq_no,
            )
        });

        let target_count = stats
            .segment_count
            .saturating_sub(self.config.max_segments)
            .saturating_add(1)
            .max(2);
        let mut merged_bytes = 0u64;
        let mut source_segments = Vec::new();
        for segment in candidates {
            let effective_bytes = segment.size_bytes.max(self.config.floor_segment_bytes);
            if !source_segments.is_empty()
                && merged_bytes.saturating_add(effective_bytes)
                    > self.config.max_merged_segment_bytes
            {
                break;
            }
            merged_bytes = merged_bytes.saturating_add(effective_bytes);
            source_segments.push(segment.id);
            if source_segments.len() >= target_count {
                break;
            }
        }

        if source_segments.len() < 2 {
            return Vec::new();
        }

        vec![MergeTask {
            source_segments,
            source_delete_segments: Vec::new(),
        }]
    }
}

#[derive(Debug, Clone)]
pub struct BaseDeltaMergePolicy {
    max_delta_segments: usize,
    max_delete_segments: usize,
    max_delta_bytes_ratio: f64,
}

impl BaseDeltaMergePolicy {
    pub fn new(
        max_delta_segments: usize,
        max_delete_segments: usize,
        max_delta_bytes_ratio: f64,
    ) -> Self {
        Self {
            max_delta_segments,
            max_delete_segments,
            max_delta_bytes_ratio,
        }
    }
}

impl Default for BaseDeltaMergePolicy {
    fn default() -> Self {
        Self::new(8, 8, 0.10)
    }
}

impl MergePolicy for BaseDeltaMergePolicy {
    fn pick_merges(&self, manifest: &Manifest, _stats: &MergeStats) -> Vec<MergeTask> {
        let Some(ManifestSource::GitBase {
            foundation_segment_ids,
            ..
        }) = manifest.source.as_ref()
        else {
            return Vec::new();
        };
        let foundation = foundation_segment_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let foundation_bytes = manifest
            .active_segments
            .iter()
            .filter(|segment| foundation.contains(&segment.id))
            .map(|segment| segment.size_bytes)
            .sum::<u64>();
        let delta_bytes = manifest
            .active_segments
            .iter()
            .filter(|segment| !foundation.contains(&segment.id))
            .map(|segment| segment.size_bytes)
            .chain(
                manifest
                    .active_delete_segments
                    .iter()
                    .map(|segment| segment.size_bytes),
            )
            .sum::<u64>();
        let bytes_exceeded = foundation_bytes > 0
            && delta_bytes as f64 > foundation_bytes as f64 * self.max_delta_bytes_ratio;
        if manifest.active_delete_segments.len() >= 2
            && (manifest.active_delete_segments.len() > self.max_delete_segments || bytes_exceeded)
        {
            return vec![MergeTask {
                source_segments: Vec::new(),
                source_delete_segments: manifest
                    .active_delete_segments
                    .iter()
                    .map(|segment| segment.id.clone())
                    .collect(),
            }];
        }

        let mut deltas = manifest
            .active_segments
            .iter()
            .filter(|segment| !foundation.contains(&segment.id))
            .collect::<Vec<_>>();
        if deltas.len() < 2 || (deltas.len() <= self.max_delta_segments && !bytes_exceeded) {
            return Vec::new();
        }
        deltas.sort_unstable_by_key(|segment| (segment.max_seq_no, segment.min_seq_no));
        let merge_count = deltas
            .len()
            .saturating_sub(self.max_delta_segments)
            .saturating_add(1)
            .max(2);
        vec![MergeTask {
            source_segments: deltas
                .into_iter()
                .take(merge_count)
                .map(|segment| segment.id.clone())
                .collect(),
            source_delete_segments: Vec::new(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::{manifest::SegmentRef, ManifestId, RepoId, SegmentId};

    #[test]
    fn deletion_merges_respect_source_budget_and_advance_past_applied_segments() {
        use crate::index_engine::{manifest::DeleteSegmentRef, DeleteSegmentId};
        let policy = TieredMergePolicy::new(MergePolicyConfig {
            floor_segment_bytes: 1,
            max_merged_segment_bytes: 100,
            ..MergePolicyConfig::default()
        });
        let mut manifest = Manifest {
            active_segments: (0..5)
                .map(|i| SegmentRef {
                    applied_delete_seq_no: 0,
                    id: SegmentId(format!("segment-{i}")),
                    min_seq_no: i + 1,
                    max_seq_no: i + 1,
                    doc_count: 1,
                    size_bytes: 40,
                })
                .collect(),
            active_delete_segments: vec![DeleteSegmentRef {
                id: DeleteSegmentId("delete".into()),
                min_seq_no: 6,
                max_seq_no: 6,
                delete_count: 1,
                size_bytes: 1,
            }],
            ..Manifest::default()
        };
        let stats = MergeStats {
            segment_count: 5,
            delete_segment_count: 1,
        };
        for expected_count in [2, 2, 1] {
            let tasks = policy.pick_merges(&manifest, &stats);
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].source_segments.len(), expected_count);
            for segment in &mut manifest.active_segments {
                if tasks[0].source_segments.contains(&segment.id) {
                    segment.applied_delete_seq_no = 6;
                }
            }
        }
        assert!(policy.pick_merges(&manifest, &stats).is_empty());
        manifest.active_segments[0].size_bytes = 1000;
        manifest.active_segments[0].applied_delete_seq_no = 0;
        assert_eq!(
            policy.pick_merges(&manifest, &stats)[0]
                .source_segments
                .len(),
            1,
            "a single legacy oversized segment must not be starved"
        );
    }

    #[test]
    fn full_sized_old_segment_does_not_starve_small_segment_merges() {
        let policy = TieredMergePolicy::new(MergePolicyConfig {
            max_segments: 2,
            floor_segment_bytes: 1,
            max_merged_segment_bytes: 100,
            ..MergePolicyConfig::default()
        });
        let manifest = Manifest {
            active_segments: [100, 20, 20, 20]
                .into_iter()
                .enumerate()
                .map(|(i, bytes)| SegmentRef {
                    applied_delete_seq_no: 0,
                    id: SegmentId(format!("segment-{i}")),
                    min_seq_no: i as u64 + 1,
                    max_seq_no: i as u64 + 1,
                    doc_count: 1,
                    size_bytes: bytes,
                })
                .collect(),
            ..Manifest::default()
        };
        let tasks = policy.pick_merges(
            &manifest,
            &MergeStats {
                segment_count: 4,
                delete_segment_count: 0,
            },
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].source_segments.len(), 3);
        assert!(!tasks[0]
            .source_segments
            .contains(&SegmentId("segment-0".into())));
    }

    #[test]
    fn base_delta_policy_never_selects_foundation_segments() {
        let foundation = SegmentId("foundation".into());
        let mut manifest = Manifest {
            manifest_id: ManifestId("base".into()),
            source: Some(ManifestSource::GitBase {
                head_commit: "head".into(),
                config_fingerprint: "config".into(),
                repo_root: RepoId("repo".into()).0,
                document_count: 3,
                parent_manifest_id: None,
                foundation_segment_ids: vec![foundation.clone()],
                delta_depth: 3,
                delta_bytes: 3,
            }),
            ..Manifest::default()
        };
        manifest.active_segments = [
            foundation.clone(),
            SegmentId("d1".into()),
            SegmentId("d2".into()),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, id)| SegmentRef {
            applied_delete_seq_no: 0,
            id,
            min_seq_no: index as u64,
            max_seq_no: index as u64,
            doc_count: 1,
            size_bytes: 1,
        })
        .collect();
        let tasks = BaseDeltaMergePolicy::new(1, 8, 0.10).pick_merges(
            &manifest,
            &MergeStats {
                segment_count: 3,
                delete_segment_count: 0,
            },
        );
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].source_segments.contains(&foundation));
        assert_eq!(tasks[0].source_segments.len(), 2);
    }

    #[test]
    fn base_delta_policy_does_not_schedule_single_segment_noop_merges() {
        let delta = SegmentId("delta".into());
        let manifest = Manifest {
            source: Some(ManifestSource::GitBase {
                head_commit: "head".into(),
                config_fingerprint: "config".into(),
                repo_root: "repo".into(),
                document_count: 1,
                parent_manifest_id: None,
                foundation_segment_ids: Vec::new(),
                delta_depth: 1,
                delta_bytes: 1,
            }),
            active_segments: vec![SegmentRef {
                applied_delete_seq_no: 0,
                id: delta,
                min_seq_no: 1,
                max_seq_no: 1,
                doc_count: 1,
                size_bytes: 1,
            }],
            ..Manifest::default()
        };
        assert!(BaseDeltaMergePolicy::new(0, 0, 0.0)
            .pick_merges(
                &manifest,
                &MergeStats {
                    segment_count: 1,
                    delete_segment_count: 0,
                },
            )
            .is_empty());
    }
}
