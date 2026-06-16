use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    config::{BuildConfig, CorpusMode, TokenizerMode},
    error::{AppError, Result},
    path_utils::normalize_path_from_cwd,
};

use super::protocol::{CorpusModeConfig, RepoConfig, TokenizerModeConfig};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(super) struct GitWorkspace {
    pub(super) worktree_root: PathBuf,
    pub(super) head_commit: String,
    pub(super) git_dir: PathBuf,
    pub(super) common_git_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedRepoConfig {
    pub(super) build_config: BuildConfig,
    pub(super) workspace: GitWorkspace,
}

impl ResolvedRepoConfig {
    pub(super) fn repo_id(&self) -> String {
        self.workspace.worktree_root.to_string_lossy().into_owned()
    }
}

pub(super) fn resolve_repo_config(
    repo_path: PathBuf,
    storage_root: Option<PathBuf>,
    config: RepoConfig,
) -> Result<ResolvedRepoConfig> {
    let workspace = GitWorkspace::discover(&repo_path)?;
    let storage_root = match storage_root {
        Some(storage_root) => normalize_path_from_cwd(&storage_root)?,
        None => workspace.worktree_root.join(".flashgrep-index-engine"),
    };
    Ok(ResolvedRepoConfig {
        build_config: BuildConfig {
            repo_path: workspace.worktree_root.clone(),
            storage_root,
            tokenizer: tokenizer_mode(config.tokenizer),
            corpus_mode: corpus_mode(config.corpus_mode),
            include_hidden: config.include_hidden,
            max_file_size: config.max_file_size,
            min_sparse_len: config.min_sparse_len,
            max_sparse_len: config.max_sparse_len,
        },
        workspace,
    })
}

impl GitWorkspace {
    pub(super) fn discover(input_path: &Path) -> Result<Self> {
        let inside_worktree = git_output(input_path, &["rev-parse", "--is-inside-work-tree"])?;
        if inside_worktree.trim() != "true" {
            return Err(discovery_error(input_path));
        }

        let bare = git_output(input_path, &["rev-parse", "--is-bare-repository"])?;
        if bare.trim() == "true" {
            return Err(discovery_error(input_path));
        }

        let worktree_root =
            PathBuf::from(git_output(input_path, &["rev-parse", "--show-toplevel"])?.trim());
        let worktree_root =
            std::fs::canonicalize(&worktree_root).map_err(|_| discovery_error(input_path))?;

        let head_commit = git_output(input_path, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        let head_commit = head_commit.trim().to_string();
        if head_commit.is_empty() {
            return Err(discovery_error(input_path));
        }

        let git_dir = resolve_git_dir(
            &worktree_root,
            git_output(input_path, &["rev-parse", "--git-dir"])?.trim(),
        );
        let common_git_dir = resolve_git_dir(
            &worktree_root,
            git_output(input_path, &["rev-parse", "--git-common-dir"])?.trim(),
        );

        Ok(Self {
            worktree_root,
            head_commit,
            git_dir,
            common_git_dir,
        })
    }
}

fn git_output(input_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(input_path)
        .args(args)
        .output()
        .map_err(|_| discovery_error(input_path))?;
    if !output.status.success() {
        return Err(discovery_error(input_path));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn discovery_error(path: &Path) -> AppError {
    AppError::Protocol(format!(
        "indexed daemon workspace requires a Git worktree with a HEAD commit: {}",
        path.display()
    ))
}

fn resolve_git_dir(worktree_root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        worktree_root.join(path)
    };
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn tokenizer_mode(value: TokenizerModeConfig) -> TokenizerMode {
    match value {
        TokenizerModeConfig::Trigram => TokenizerMode::Trigram,
        TokenizerModeConfig::SparseNgram => TokenizerMode::SparseNgram,
    }
}

fn corpus_mode(value: CorpusModeConfig) -> CorpusMode {
    match value {
        CorpusModeConfig::RespectIgnore => CorpusMode::RespectIgnore,
        CorpusModeConfig::NoIgnore => CorpusMode::NoIgnore,
    }
}
