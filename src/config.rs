use std::path::PathBuf;

use crate::search::SearchMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerMode {
    Trigram,
    SparseNgram,
}

impl TokenizerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trigram => "trigram",
            Self::SparseNgram => "sparse-ngram",
        }
    }

    pub fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Trigram),
            2 => Some(Self::SparseNgram),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::Trigram => 1,
            Self::SparseNgram => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorpusMode {
    RespectIgnore,
    NoIgnore,
}

impl CorpusMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RespectIgnore => "ignore",
            Self::NoIgnore => "no-ignore",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub repo_path: PathBuf,
    pub index_path: PathBuf,
    pub tokenizer: TokenizerMode,
    pub corpus_mode: CorpusMode,
    pub max_file_size: u64,
    pub min_sparse_len: usize,
    pub max_sparse_len: usize,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            repo_path: PathBuf::new(),
            index_path: PathBuf::new(),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::RespectIgnore,
            max_file_size: 2 * 1024 * 1024,
            min_sparse_len: 5,
            max_sparse_len: 16,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryConfig {
    pub regex_pattern: String,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub dot_matches_new_line: bool,
    pub top_k_tokens: usize,
    pub search_mode: SearchMode,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            regex_pattern: String::new(),
            case_insensitive: false,
            multiline: false,
            dot_matches_new_line: false,
            top_k_tokens: 6,
            search_mode: SearchMode::MaterializeMatches,
        }
    }
}
