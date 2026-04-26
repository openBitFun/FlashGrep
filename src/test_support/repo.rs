use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::{tempdir, TempDir};

use crate::{BuildConfig, CorpusMode, SearchEngine, TokenizerMode};

pub(crate) struct TestRepo {
    _temp: TempDir,
    pub(crate) repo: PathBuf,
    pub(crate) index: PathBuf,
}

impl TestRepo {
    pub(crate) fn new() -> Self {
        let temp = tempdir().expect("temp dir should succeed");
        let repo_dir = temp.path().join("repo");
        fs::create_dir_all(&repo_dir).expect("repo dir should succeed");
        let repo = fs::canonicalize(&repo_dir).expect("repo dir should canonicalize");
        let index = repo.join(".codgrep-index-engine");
        Self {
            _temp: temp,
            repo,
            index,
        }
    }

    pub(crate) fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.repo.join(relative)
    }

    pub(crate) fn write(&self, relative: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir should succeed");
        }
        fs::write(&path, contents).expect("write should succeed");
        path
    }

    pub(crate) fn write_bytes(&self, relative: impl AsRef<Path>, contents: &[u8]) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent dir should succeed");
        }
        fs::write(&path, contents).expect("write should succeed");
        path
    }

    pub(crate) fn trigram_build_config(&self) -> BuildConfig {
        BuildConfig {
            repo_path: self.repo.clone(),
            storage_root: self.index.clone(),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 32,
        }
    }

    pub(crate) fn build_config(&self) -> BuildConfig {
        self.trigram_build_config()
    }

    pub(crate) fn build(&self) {
        self.build_with(self.trigram_build_config());
    }

    pub(crate) fn build_with(&self, config: BuildConfig) {
        SearchEngine::new(config)
            .build_base_snapshot()
            .expect("build should succeed");
    }

    pub(crate) fn init_git(&self) {
        self.run_git(["init", "-q"]);
        self.run_git(["config", "user.name", "Test User"]);
        self.run_git(["config", "user.email", "test@example.com"]);
    }

    pub(crate) fn commit_all(&self, message: &str) {
        self.run_git(["add", "."]);
        self.run_git(["commit", "-qm", message]);
    }

    fn run_git<const N: usize>(&self, args: [&str; N]) -> std::process::Output {
        let output = Command::new("git")
            .current_dir(&self.repo)
            .args(args)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
