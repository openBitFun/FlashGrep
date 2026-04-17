use std::path::PathBuf;

use crate::index_engine::types::GitRevision;

#[derive(Debug, Clone)]
pub struct GitSnapshotIndex {
    pub repo_root: PathBuf,
    pub snapshot_key: String,
    pub revision: Option<GitRevision>,
}
