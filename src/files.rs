use std::{collections::HashSet, fs, path::PathBuf, time::UNIX_EPOCH};

use walkdir::{DirEntry, WalkDir};

use crate::{config::BuildConfig, error::Result};

const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
];

#[derive(Debug, Clone)]
pub struct RepositoryFile {
    pub ordinal: usize,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_secs: u64,
}

pub fn scan_repository(config: &BuildConfig) -> Result<Vec<RepositoryFile>> {
    let ignored: HashSet<&str> = IGNORED_DIRS.iter().copied().collect();
    let mut files = Vec::new();

    for entry in WalkDir::new(&config.repo_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_visit(entry, &ignored))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let metadata = entry.metadata()?;
        if metadata.len() > config.max_file_size {
            continue;
        }

        let mtime_secs = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();

        files.push(RepositoryFile {
            ordinal: files.len(),
            path: entry.path().to_path_buf(),
            size: metadata.len(),
            mtime_secs,
        });
    }

    Ok(files)
}

pub fn read_text_file(path: &std::path::Path) -> Result<Option<String>> {
    let bytes = fs::read(path)?;
    if looks_binary(&bytes) {
        return Ok(None);
    }

    match String::from_utf8(bytes) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

fn should_visit(entry: &DirEntry, ignored: &HashSet<&str>) -> bool {
    if !entry.file_type().is_dir() {
        return true;
    }

    let name = entry.file_name().to_string_lossy();
    !ignored.contains(name.as_ref())
}

fn looks_binary(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].iter().any(|byte| *byte == 0)
}
