mod cli;
pub mod config;
pub mod daemon;
pub mod error;
mod files;
mod index;
mod json_output;
mod path_filter;
mod planner;
mod progress;
mod query_preflight;
mod search;
mod search_engine;
mod tokenizer;
mod tools;
mod workspace;

pub use config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode};
pub use error::{AppError, Result};
pub use index::{
    build_index, build_index_with_options, rebuild_index, rebuild_index_with_options,
    IndexBuildOptions,
};
pub use search::{SearchMode, SearchResults};
pub use search_engine::{SearchBackend, SearchEngine, SearchPolicy, SearchResponse};
pub use workspace::{
    WorkspaceFreshness, WorkspaceFreshnessState, WorkspaceIndex, WorkspaceIndexOptions,
};

/// Expert-facing API surface for advanced integration scenarios.
pub mod advanced {
    pub use crate::files::{
        read_text_file, repo_relative_path, resolve_repo_path, scan_paths, scan_repository,
        RepositoryFile, ScanOptions,
    };
    pub use crate::index::{IndexSearcher, IndexWorktreeDiff};
    pub use crate::path_filter::{normalize_path, PathFilter, PathFilterArgs};
    pub use crate::progress::{IndexProgress, IndexProgressPhase};
    pub use crate::search::{
        FileContext, FileCount, FileMatch, FileMatchCount, MatchLocation, SearchHit, SearchLine,
    };
    pub use crate::workspace::{
        BaseSnapshotInfo, BaseSnapshotKind, IndexStatus, WorkspaceSnapshot,
    };

    /// JSON event/output model types.
    ///
    /// Primarily intended for CLI/integration output adapters; schema may
    /// evolve over time.
    pub mod json {
        pub use crate::json_output::{
            JsonBegin, JsonContext, JsonData, JsonDuration, JsonEnd, JsonMatch, JsonMessage,
            JsonSearchFile, JsonSearchReport, JsonStats, JsonSubmatch, JsonSummary,
        };
    }
}

/// Unstable or evolving API surface intended for experimentation.
pub mod experimental {
    pub use crate::planner::{plan, PureLiteralAlternation, QueryBranch, QueryPlan};
    pub use crate::tokenizer::{create, hash_token, unique_sorted, Tokenizer, TokenizerOptions};

    /// Low-level on-disk index format helpers.
    pub mod index_format {
        pub use crate::index::format::*;
    }
}

#[doc(hidden)]
pub fn run_cli() -> Result<bool> {
    cli::run()
}
