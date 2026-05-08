use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{config::BuildConfig, error::Result, path_utils::normalize_lexical_path};

pub(crate) fn ensure_storage_root_ignored(build_config: &BuildConfig) -> Result<()> {
    if !is_git_repo(&build_config.repo_path) {
        return Ok(());
    }

    let relative = match repo_relative_storage_root(build_config) {
        Some(path) => path,
        None => return Ok(()),
    };
    let normalized_relative = normalize_ignore_path(&relative);
    if normalized_relative.is_empty() {
        return Ok(());
    }

    let gitignore_path = build_config.repo_path.join(".gitignore");
    let mut contents = if gitignore_path.exists() {
        fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    if contents
        .lines()
        .any(|line| ignore_entry_matches(line, &normalized_relative))
    {
        return Ok(());
    }

    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push('/');
    contents.push_str(&normalized_relative);
    contents.push('/');
    contents.push('\n');
    fs::write(gitignore_path, contents)?;
    Ok(())
}

fn is_git_repo(repo_path: &Path) -> bool {
    let git_path = repo_path.join(".git");
    git_path.is_dir() || git_path.is_file()
}

fn repo_relative_storage_root(build_config: &BuildConfig) -> Option<PathBuf> {
    let storage_root = canonicalize_through_existing_ancestor(&build_config.storage_root)?;
    let relative = storage_root.strip_prefix(&build_config.repo_path).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(relative.to_path_buf())
}

fn canonicalize_through_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let normalized = normalize_lexical_path(path);
    if normalized.exists() {
        return Some(fs::canonicalize(&normalized).unwrap_or(normalized));
    }

    let mut suffix = Vec::new();
    let mut current = normalized.as_path();
    while !current.exists() {
        let name = current.file_name()?.to_os_string();
        suffix.push(name);
        current = current.parent()?;
    }

    let mut resolved = fs::canonicalize(current).unwrap_or_else(|_| current.to_path_buf());
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Some(resolved)
}

fn ignore_entry_matches(line: &str, normalized_relative: &str) -> bool {
    normalize_ignore_entry(line) == normalized_relative
}

fn normalize_ignore_entry(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
        return String::new();
    }
    let without_prefix = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let without_leading = without_prefix.trim_start_matches('/');
    let without_trailing = without_leading.trim_end_matches('/');
    without_trailing.replace('\\', "/")
}

fn normalize_ignore_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_ignore_entries_match_storage_root_path() {
        for entry in [
            ".flashgrep-index-engine",
            ".flashgrep-index-engine/",
            "/.flashgrep-index-engine",
            "/.flashgrep-index-engine/",
            "./.flashgrep-index-engine/",
        ] {
            assert!(ignore_entry_matches(entry, ".flashgrep-index-engine"));
        }
    }

    #[test]
    fn comments_and_negations_do_not_match_storage_root_path() {
        assert!(!ignore_entry_matches(
            "# /.flashgrep-index-engine/",
            ".flashgrep-index-engine"
        ));
        assert!(!ignore_entry_matches(
            "!/.flashgrep-index-engine/",
            ".flashgrep-index-engine"
        ));
    }
}
