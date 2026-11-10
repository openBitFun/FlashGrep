use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::config::{BuildConfig, TokenizerMode};

use super::types::ShardId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingTokenizerConfig {
    pub mode: TokenizerMode,
    pub min_sparse_len: usize,
    pub max_sparse_len: usize,
}

impl Default for PostingTokenizerConfig {
    fn default() -> Self {
        Self {
            mode: TokenizerMode::SparseNgram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }
}

impl From<&BuildConfig> for PostingTokenizerConfig {
    fn from(config: &BuildConfig) -> Self {
        Self {
            mode: config.tokenizer,
            min_sparse_len: config.min_sparse_len,
            max_sparse_len: config.max_sparse_len,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyConfig {
    pub max_segments: usize,
    pub floor_segment_bytes: u64,
    pub max_merged_segment_bytes: u64,
    // Reserved for future ratio-based delete heuristics. The current merge policy ignores this
    // field and only uses delete-segment presence plus max_segments tiered merging.
    pub deleted_ratio_threshold_percent: u8,
}

impl Default for MergePolicyConfig {
    fn default() -> Self {
        Self {
            max_segments: 12,
            floor_segment_bytes: 8 * 1024 * 1024,
            max_merged_segment_bytes: 512 * 1024 * 1024,
            deleted_ratio_threshold_percent: 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub shard_id: ShardId,
    pub index_root: PathBuf,
    pub refresh_interval: Duration,
    pub merge_policy: MergePolicyConfig,
    pub tokenizer: PostingTokenizerConfig,
    /// Decoded query payload admission budget; excludes metadata and working buffers.
    pub query_source_cache_bytes: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            shard_id: ShardId("default".into()),
            index_root: PathBuf::from(".flashgrep-index-engine"),
            refresh_interval: Duration::from_millis(500),
            merge_policy: MergePolicyConfig::default(),
            tokenizer: PostingTokenizerConfig::default(),
            query_source_cache_bytes: 0,
        }
    }
}
