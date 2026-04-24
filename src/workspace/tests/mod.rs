use std::{fs, time::Duration};

use super::{BaseSnapshotKind, WorkspaceFreshnessState, WorkspaceIndex, WorkspaceIndexOptions};
use crate::{
    index_engine::WorkspaceBaseSnapshotIndex, search_engine::SearchBackend, test_support::TestRepo,
    worktree_diff::IndexWorktreeDiff, QueryConfig,
};

mod freshness;
mod git;
mod snapshot;

fn query(pattern: &str) -> QueryConfig {
    QueryConfig {
        regex_pattern: pattern.into(),
        patterns: vec![pattern.into()],
        fixed_strings: true,
        ..QueryConfig::default()
    }
}
