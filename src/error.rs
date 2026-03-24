use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("walkdir error: {0}")]
    Walkdir(#[from] walkdir::Error),
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("invalid regex pattern: {0}")]
    InvalidPattern(String),
    #[error("unsupported index format: {0}")]
    InvalidIndex(String),
    #[error("binary data is truncated")]
    TruncatedData,
    #[error("utf-8 decode failed for {path}: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
}

pub type Result<T> = std::result::Result<T, AppError>;
