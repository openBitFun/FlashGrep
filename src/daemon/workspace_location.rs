use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    config::{BuildConfig, CorpusMode, TokenizerMode},
    error::{AppError, Result},
    path_utils::{normalize_existing_path, normalize_path_from_cwd},
};

use super::protocol::{CorpusModeConfig, RepoConfig, TokenizerModeConfig};

/// A workspace exists independently of a VCS. Git discovery can widen an
/// input subdirectory to its worktree root, but a HEAD is not a prerequisite.
#[derive(Debug, Clone)]
pub(super) struct WorkspaceLocation {
    pub(super) root: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedRepoConfig {
    pub(super) build_config: BuildConfig,
    pub(super) workspace: WorkspaceLocation,
}

impl ResolvedRepoConfig {
    pub(super) fn repo_id(&self) -> String {
        self.workspace.root.to_string_lossy().into_owned()
    }
}

pub(super) fn resolve_repo_config(
    repo_path: PathBuf,
    storage_root: Option<PathBuf>,
    config: RepoConfig,
) -> Result<ResolvedRepoConfig> {
    let workspace = WorkspaceLocation::discover(&repo_path, config.git_mode)?;
    let storage_root = match storage_root {
        Some(storage_root) => normalize_path_from_cwd(&storage_root)?,
        None => workspace.root.join(".flashgrep-index-engine"),
    };
    Ok(ResolvedRepoConfig {
        build_config: BuildConfig {
            git_mode: config.git_mode,
            repo_path: workspace.root.clone(),
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

impl WorkspaceLocation {
    fn discover(input_path: &Path, git_mode: crate::config::GitMode) -> Result<Self> {
        let input = normalize_existing_path(input_path)?;
        if !input.is_dir() {
            return Err(AppError::Protocol(format!(
                "workspace must be a directory: {}",
                input.display()
            )));
        }
        if git_mode == crate::config::GitMode::Filesystem {
            return Ok(Self { root: input });
        }
        // A bare repository is Git's object database, not a source worktree.
        // Keep rejecting that accidental target without requiring Git for
        // ordinary directories or repositories that have not made a commit.
        if input.join("HEAD").is_file() && input.join("objects").is_dir() {
            let bare = Command::new("git")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .arg("-C")
                .arg(&input)
                .args(["rev-parse", "--is-bare-repository"])
                .output();
            if bare
                .is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"true"))
            {
                return Err(AppError::Protocol(
                    "a bare Git repository has no workspace files".into(),
                ));
            }
        }
        let git = Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(&input)
            .args(["rev-parse", "--show-toplevel"])
            .output();
        let root = match git {
            Ok(output) if output.status.success() => {
                // Trim only Git's line terminator, preserving spaces in paths.
                let path = String::from_utf8(output.stdout)
                    .map_err(|_| AppError::Protocol("Git worktree path is not UTF-8".into()))?;
                std::fs::canonicalize(path.trim_end_matches(['\r', '\n']))?
            }
            _ => input,
        };
        Ok(Self { root })
    }
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
