mod cli;
pub mod config;
pub mod error;
pub mod files;
pub mod index;
pub mod json_output;
pub mod path_filter;
pub mod planner;
pub mod search;
mod search_engine;
pub mod tokenizer;
mod tools;
mod workspace;

pub use config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode};
pub use error::{AppError, Result};
pub use index::{
    build_index, rebuild_index, IndexSearcher, IndexWorktreeDiff, OverlaySnapshotInfo,
};
pub use json_output::{
    JsonBegin, JsonContext, JsonData, JsonDuration, JsonEnd, JsonMatch, JsonMessage,
    JsonSearchFile, JsonSearchReport, JsonStats, JsonSubmatch, JsonSummary,
};
pub use path_filter::{normalize_path, PathFilter, PathFilterArgs};
pub use search::{
    FileContext, FileCount, FileMatch, FileMatchCount, MatchLocation, SearchHit, SearchLine,
    SearchMode, SearchResults,
};
pub use search_engine::{SearchBackend, SearchEngine, SearchOutcome, SearchPolicy};
pub use workspace::{
    BaseSnapshotInfo, BaseSnapshotKind, IndexStatus, WorkspaceIndex, WorkspaceIndexOptions,
    WorkspaceSnapshot,
};

pub fn run_cli() -> Result<bool> {
    cli::run()
}
