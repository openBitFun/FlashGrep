use std::{path::PathBuf, sync::Arc};

use crate::files::RepositoryFile;

#[derive(Debug, Clone)]
pub(crate) struct SearchDocument {
    pub(crate) logical_path: String,
    pub(crate) size: u64,
    pub(crate) mtime_nanos: u64,
    pub(crate) source: SearchDocumentSource,
}

#[derive(Debug, Clone)]
pub(crate) enum SearchDocumentSource {
    Path(PathBuf),
    LoadedBytes(Arc<[u8]>),
}

impl SearchDocument {
    pub(crate) fn from_repository_file(file: &RepositoryFile) -> Self {
        Self {
            logical_path: file.path.to_string_lossy().into_owned(),
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            source: SearchDocumentSource::Path(file.path.clone()),
        }
    }

    pub(crate) fn from_loaded_bytes(
        logical_path: String,
        size: u64,
        mtime_nanos: u64,
        bytes: Arc<[u8]>,
    ) -> Self {
        Self {
            logical_path,
            size,
            mtime_nanos,
            source: SearchDocumentSource::LoadedBytes(bytes),
        }
    }
}
