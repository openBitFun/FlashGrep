use std::{
    collections::{HashMap, HashSet},
    fs,
};

use crate::{
    config::BuildConfig, error::Result, files::scan_repository,
    index_engine::reader::SnapshotPathCatalog,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexWorktreeDiff {
    pub modified_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub new_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirtyPathKind {
    Modified,
    Deleted,
    New,
}

impl IndexWorktreeDiff {
    pub fn is_empty(&self) -> bool {
        self.modified_files.is_empty() && self.deleted_files.is_empty() && self.new_files.is_empty()
    }

    pub fn dirty_paths(&self) -> HashSet<&str> {
        self.modified_files
            .iter()
            .chain(self.deleted_files.iter())
            .chain(self.new_files.iter())
            .map(String::as_str)
            .collect()
    }
}

pub(crate) fn diff_against_snapshot_catalog(
    build_config: &BuildConfig,
    catalog: &SnapshotPathCatalog,
) -> Result<IndexWorktreeDiff> {
    let current = current_snapshot_file_states(build_config)?;

    let base = catalog
        .iter()
        .map(|(logical_path, entry)| {
            (
                build_config
                    .repo_path
                    .join(logical_path)
                    .to_string_lossy()
                    .into_owned(),
                (entry.size, entry.content_hash),
            )
        })
        .collect::<HashMap<_, _>>();

    let mut modified_files = Vec::new();
    let mut deleted_files = Vec::new();
    for (path, state) in &base {
        match current.get(path.as_str()) {
            Some(current_state) if current_state == state => {}
            Some(_) => modified_files.push(path.clone()),
            None => deleted_files.push(path.clone()),
        }
    }

    let mut new_files = current
        .keys()
        .filter(|path| !base.contains_key(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    modified_files.sort_unstable();
    deleted_files.sort_unstable();
    new_files.sort_unstable();

    Ok(IndexWorktreeDiff {
        modified_files,
        deleted_files,
        new_files,
    })
}

fn current_snapshot_file_states(
    build_config: &BuildConfig,
) -> Result<HashMap<String, (u64, [u8; 32])>> {
    let mut current = HashMap::new();
    for file in scan_repository(build_config)? {
        let Some(content_hash) = current_file_content_hash(&file.path)? else {
            continue;
        };
        current.insert(
            file.path.to_string_lossy().into_owned(),
            (file.size, content_hash),
        );
    }
    Ok(current)
}

fn current_file_content_hash(path: &std::path::Path) -> Result<Option<[u8; 32]>> {
    let bytes = fs::read(path)?;
    let probe_len = bytes.len().min(8 * 1024);
    if bytes[..probe_len].contains(&0) {
        return Ok(None);
    }
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    Ok(Some(content_hash))
}
