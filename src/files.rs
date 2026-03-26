use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;

use crate::{
    config::{BuildConfig, CorpusMode},
    error::Result,
};

#[derive(Debug, Clone)]
pub struct RepositoryFile {
    pub ordinal: usize,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_secs: u64,
}

pub fn scan_repository(config: &BuildConfig) -> Result<Vec<RepositoryFile>> {
    let mut files = Vec::new();

    if config.repo_path.is_file() {
        if let Some(file) = repository_file(&config.repo_path, 0, config.max_file_size)? {
            files.push(file);
        }
        return Ok(files);
    }

    let walker = build_walker(config);
    for entry in walker.build() {
        let entry = entry?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        if let Some(file) = repository_file(entry.path(), files.len(), config.max_file_size)? {
            files.push(file);
        }
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

fn build_walker(config: &BuildConfig) -> WalkBuilder {
    let mut builder = WalkBuilder::new(&config.repo_path);
    builder.follow_links(false);
    builder.max_filesize(Some(config.max_file_size));

    match config.corpus_mode {
        CorpusMode::RespectIgnore => {
            builder.hidden(false);
            builder.ignore(true);
            builder.git_ignore(true);
            builder.git_global(true);
            builder.git_exclude(true);
            builder.parents(true);
        }
        CorpusMode::NoIgnore => {
            builder.hidden(false);
            builder.ignore(false);
            builder.git_ignore(false);
            builder.git_global(false);
            builder.git_exclude(false);
            builder.parents(false);
            builder.require_git(false);
        }
    }

    builder
}

fn repository_file(
    path: &Path,
    ordinal: usize,
    max_file_size: u64,
) -> Result<Option<RepositoryFile>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_file_size {
        return Ok(None);
    }

    let mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    Ok(Some(RepositoryFile {
        ordinal,
        path: path.to_path_buf(),
        size: metadata.len(),
        mtime_secs,
    }))
}

fn looks_binary(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].contains(&0)
}
