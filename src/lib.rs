pub mod bench;
pub mod cli;
pub mod config;
pub mod error;
pub mod files;
pub mod index;
pub mod json_output;
pub mod path_filter;
pub mod planner;
pub mod search;
pub mod tokenizer;

pub use config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode};
pub use error::{AppError, Result};
pub use index::{
    build_index, rebuild_index, IndexSearcher, IndexWorktreeDiff, SearchBackend, SearchEngine,
    SearchOutcome, SearchPolicy,
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
