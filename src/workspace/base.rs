use crate::{
    error::{AppError, Result},
    index_engine::{
        reader::{IndexSnapshotReader, SnapshotPathCatalog},
        workspace::WorkspaceOverlaySnapshotState,
        WorkspaceBaseSnapshotIndex, WorkspaceOverlayIndex,
    },
};

use super::{BaseSnapshotInfo, BaseSnapshotKind, WorkspaceIndex};

impl WorkspaceIndex {
    pub(crate) fn base_snapshot_info(&self) -> Result<BaseSnapshotInfo> {
        let metadata =
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.inner.options.build_config)?
                .ok_or_else(|| {
                AppError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!(
                        "base snapshot shard metadata is missing for {}",
                        self.inner.options.build_config.repo_path.display()
                    ),
                ))
            })?;
        let tokenizer = metadata.tokenizer.ok_or_else(|| {
            AppError::InvalidIndex("base snapshot tokenizer metadata is missing".into())
        })?;
        let doc_count = metadata.doc_count.ok_or_else(|| {
            AppError::InvalidIndex("base snapshot document count metadata is missing".into())
        })?;

        let snapshot_kind = if metadata.snapshot_key.starts_with("base-git-") {
            BaseSnapshotKind::GitCommit
        } else if metadata.snapshot_key.starts_with("base-repo-") {
            BaseSnapshotKind::RepositoryFallback
        } else {
            BaseSnapshotKind::Legacy
        };

        Ok(BaseSnapshotInfo {
            snapshot_key: metadata.snapshot_key,
            base_snapshot_root: WorkspaceBaseSnapshotIndex::index_root(
                &self.inner.options.build_config,
            ),
            repo_path: self.inner.options.build_config.repo_path.clone(),
            tokenizer,
            doc_count,
            snapshot_kind,
            head_commit: metadata.head_commit,
            config_fingerprint: metadata.config_fingerprint,
        })
    }

    pub(super) fn current_base_snapshot_key(&self) -> Result<String> {
        Ok(self.base_snapshot_info()?.snapshot_key)
    }

    pub(super) fn open_base_snapshot_reader(&self) -> Result<IndexSnapshotReader> {
        let snapshot_key = self.current_base_snapshot_key()?;
        self.open_base_snapshot_reader_for_key(&snapshot_key)
    }

    pub(super) fn open_base_snapshot_reader_for_key(
        &self,
        snapshot_key: &str,
    ) -> Result<IndexSnapshotReader> {
        let index = WorkspaceBaseSnapshotIndex::open_if_matching(
            &self.inner.options.build_config,
            snapshot_key,
        )?
        .ok_or_else(|| {
            AppError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "base snapshot shard marker is missing or stale for {}",
                    self.inner.options.build_config.repo_path.display()
                ),
            ))
        })?;
        index.open_committed()
    }

    pub(super) fn open_base_path_catalog(&self) -> Result<SnapshotPathCatalog> {
        self.open_base_snapshot_reader()?.path_catalog()
    }

    pub(super) fn materialize_overlay_snapshot_reader(
        &self,
        state: &WorkspaceOverlaySnapshotState,
    ) -> Result<IndexSnapshotReader> {
        WorkspaceOverlayIndex::clear_storage(&self.inner.options.build_config)?;
        let overlay = WorkspaceOverlayIndex::open(&self.inner.options.build_config)?;
        overlay.rebuild_snapshot(state)?;
        overlay.checkpoint()?;
        overlay.open_snapshot()
    }
}
