//! Optional Git index membership. Ordinary edits inspect files directly; Git is
//! consulted only when its index identity changes (or during reconciliation).
use super::RepoRuntime;
use crate::{
    error::{AppError, Result},
    path_utils::normalize_lexical_path,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::SystemTime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexStamp {
    modified: Option<SystemTime>,
    length: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Default)]
pub(super) struct GitMembershipCache {
    index_path: Option<PathBuf>,
    stamp: Option<IndexStamp>,
    paths: Arc<BTreeSet<String>>,
    initialized: bool,
}

pub(super) fn git_index_path(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("index"));
    }
    let contents = fs::read_to_string(dot_git).ok()?;
    let directory = Path::new(contents.trim().strip_prefix("gitdir:")?.trim());
    Some(normalize_lexical_path(&root.join(directory).join("index")))
}

fn stamp(path: &Path) -> Result<Option<IndexStamp>> {
    match fs::metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            Ok(Some(IndexStamp {
                modified: metadata.modified().ok(),
                length: metadata.len(),
                #[cfg(unix)]
                inode: metadata.ino(),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl RepoRuntime {
    pub(super) fn git_membership(&self, reconcile: bool) -> Result<Arc<BTreeSet<String>>> {
        if self.build_config.git_mode == crate::config::GitMode::Filesystem {
            return Ok(Arc::new(BTreeSet::new()));
        }

        let mut cache = self
            .git_membership_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index_path = git_index_path(&self.build_config.repo_path);
        let current_stamp = index_path.as_deref().map(stamp).transpose()?.flatten();
        if cache.initialized
            && !reconcile
            && cache.index_path == index_path
            && cache.stamp == current_stamp
        {
            return Ok(Arc::clone(&cache.paths));
        }
        let paths = if let Some(index_path) = &index_path {
            // A replaced index must not get cached under the identity read before
            // ls-files. Leave pending discovery retryable if Git is still mutating.
            let output = Command::new("git")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .arg("-C")
                .arg(&self.build_config.repo_path)
                .args(["ls-files", "--cached", "-z"])
                .output()?;
            if !output.status.success() {
                return Err(AppError::InvalidIndex(format!(
                    "could not read Git membership: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            if stamp(index_path)? != current_stamp {
                return Err(AppError::InvalidIndex(
                    "Git index changed while reading membership; retry discovery".into(),
                ));
            }
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(|path| {
                    std::str::from_utf8(path)
                        .map(|path| path.replace('/', std::path::MAIN_SEPARATOR_STR))
                        .map_err(|_| AppError::InvalidIndex("Git member path is not UTF-8".into()))
                })
                .collect::<Result<BTreeSet<_>>>()?
        } else {
            BTreeSet::new()
        };
        *cache = GitMembershipCache {
            index_path,
            stamp: current_stamp,
            paths: Arc::new(paths),
            initialized: true,
        };
        Ok(Arc::clone(&cache.paths))
    }
}

pub(super) fn scoped_members<'a>(
    paths: &'a BTreeSet<String>,
    scope: &str,
) -> impl Iterator<Item = &'a str> {
    let exact = paths.get(scope).into_iter();
    let prefix = if scope.is_empty() {
        String::new()
    } else {
        format!("{scope}{}", std::path::MAIN_SEPARATOR)
    };
    exact
        .chain(
            paths
                .range(prefix.clone()..)
                .take_while(move |path| path.starts_with(&prefix)),
        )
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_membership_changes_only_with_git_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "BEFORE\n").unwrap();
        let runtime = RepoRuntime::new(
            "repo".into(),
            crate::config::BuildConfig {
                repo_path: root.clone(),
                storage_root: temp.path().join("index"),
                ..Default::default()
            },
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        let empty = runtime.git_membership(false).unwrap();
        assert!(empty.is_empty());
        git(&["add", "-f", "ignored.rs"]);
        let tracked = runtime.git_membership(false).unwrap();
        assert!(tracked.contains("ignored.rs"));
        fs::write(root.join("ignored.rs"), "AFTER\n").unwrap();
        assert!(Arc::ptr_eq(
            &tracked,
            &runtime.git_membership(false).unwrap()
        ));
        git(&["rm", "--cached", "-f", "ignored.rs"]);
        assert!(!runtime
            .git_membership(false)
            .unwrap()
            .contains("ignored.rs"));
    }

    #[test]
    fn linked_worktree_index_resolves_relative_to_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worktree");
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".git"), "gitdir: ../metadata/worktrees/topic\n").unwrap();
        assert_eq!(
            git_index_path(&root),
            Some(temp.path().join("metadata/worktrees/topic/index"))
        );
    }
}
