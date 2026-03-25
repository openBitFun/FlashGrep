pub mod bench;
pub mod cli;
pub mod config;
pub mod error;
pub mod files;
pub mod index;
pub mod planner;
pub mod search;
pub mod tokenizer;

pub use config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode};
pub use error::{AppError, Result};
pub use search::SearchMode;
