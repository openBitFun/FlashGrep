use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use bitfun_grep::{
    build_index, BuildConfig, CorpusMode, IndexSearcher, QueryConfig, SearchMode, TokenizerMode,
};
use tempfile::{tempdir, TempDir};

pub const SHERLOCK: &str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";

pub struct TestRepo {
    _temp: TempDir,
    pub repo: PathBuf,
    pub index: PathBuf,
}

impl TestRepo {
    pub fn new() -> Self {
        let temp = tempdir().expect("test should succeed");
        let repo = temp.path().join("repo");
        let index = repo.join(".bitfun-index");
        fs::create_dir_all(&repo).expect("test should succeed");
        Self {
            _temp: temp,
            repo,
            index,
        }
    }

    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.repo.join(relative)
    }

    pub fn create_dir(&self, relative: impl AsRef<Path>) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(&path).expect("test should succeed");
        path
    }

    pub fn write(&self, relative: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test should succeed");
        }
        fs::write(&path, contents).expect("test should succeed");
        path
    }

    pub fn trigram_build_config(&self) -> BuildConfig {
        BuildConfig {
            repo_path: self.repo.clone(),
            index_path: self.index.clone(),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 32,
        }
    }

    pub fn sparse_build_config(&self) -> BuildConfig {
        BuildConfig {
            tokenizer: TokenizerMode::SparseNgram,
            min_sparse_len: 5,
            max_sparse_len: 16,
            ..self.trigram_build_config()
        }
    }

    pub fn build(&self) {
        self.build_with(self.trigram_build_config());
    }

    pub fn build_sparse(&self) {
        self.build_with(self.sparse_build_config());
    }

    pub fn build_with(&self, config: BuildConfig) {
        build_index(&config).expect("test should succeed");
    }

    pub fn searcher(&self) -> IndexSearcher {
        IndexSearcher::open(self.index.clone()).expect("test should succeed")
    }

    pub fn cli(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bitfun_grep"));
        command.arg("--index").arg(&self.index);
        command
    }

    pub fn rg(&self) -> Command {
        Command::new("rg")
    }
}

pub fn query(pattern: &str) -> QueryConfig {
    QueryConfig {
        regex_pattern: pattern.into(),
        patterns: vec![pattern.into()],
        ..QueryConfig::default()
    }
}

pub fn count_query(pattern: &str) -> QueryConfig {
    QueryConfig {
        search_mode: SearchMode::CountOnly,
        ..query(pattern)
    }
}

pub fn count_matches_query(pattern: &str) -> QueryConfig {
    QueryConfig {
        search_mode: SearchMode::CountMatches,
        ..query(pattern)
    }
}

pub fn json_lines(output: &[u8]) -> Vec<String> {
    stdout_string(output)
        .lines()
        .map(ToString::to_string)
        .collect()
}

pub fn json_values(output: &[u8]) -> Vec<serde_json::Value> {
    json_lines(output)
        .into_iter()
        .map(|line| serde_json::from_str(&line).expect("json line should parse"))
        .collect()
}

pub fn normalize_json_messages(values: &mut [serde_json::Value]) {
    for value in values {
        normalize_json_value(value);
    }
}

fn normalize_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match key.as_str() {
                    "elapsed" | "elapsed_total" => {
                        *value = serde_json::Value::String("<duration>".into());
                    }
                    "bytes_printed" => {
                        *value = serde_json::Value::Number(0u64.into());
                    }
                    _ => normalize_json_value(value),
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                normalize_json_value(value);
            }
        }
        _ => {}
    }
}

pub fn stdout_string(output: &[u8]) -> String {
    String::from_utf8(output.to_vec()).expect("stdout should be utf-8")
}

pub fn stderr_string(output: &[u8]) -> String {
    String::from_utf8(output.to_vec()).expect("stderr should be utf-8")
}

pub fn sort_lines(lines: &str) -> String {
    let mut lines = lines.trim_end_matches('\n').lines().collect::<Vec<_>>();
    lines.sort_unstable();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}
