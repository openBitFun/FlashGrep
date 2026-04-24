use crate::index_engine::{config::MergePolicyConfig, manifest::Manifest};

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
            let source_segments = manifest
                .active_segments
                .iter()
                .filter(|segment| segment.max_seq_no <= max_delete_seq_no)
                .map(|segment| segment.id.clone())
                .collect::<Vec<_>>();
            if source_segments.is_empty() {
                return Vec::new();
            }
            return vec![MergeTask {
                source_segments,
                source_delete_segments: manifest
                    .active_delete_segments
                    .iter()
                    .map(|segment| segment.id.clone())
                    .collect(),
            }];
        }

        if stats.segment_count <= self.config.max_segments {
            return Vec::new();
        }

        let mut candidates = manifest.active_segments.to_vec();
        candidates.sort_by(|left, right| {
            left.max_seq_no
                .cmp(&right.max_seq_no)
                .then_with(|| left.min_seq_no.cmp(&right.min_seq_no))
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
