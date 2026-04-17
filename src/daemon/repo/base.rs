use std::{path::PathBuf, time::SystemTime};

use crate::{
    error::{AppError, Result},
    index_engine::WorkspaceBaseSnapshotIndex,
};

use super::RepoRuntime;
use crate::daemon::repo::status::apply_workspace_dirty_status;

impl RepoRuntime {
    pub(super) fn bootstrap_from_existing_snapshot(&self) -> Result<()> {
        self.recover_dirty_catalog_for_startup()?;
        let now = SystemTime::now();
        let snapshot_key = match self.current_base_snapshot_key() {
            Ok(snapshot_key) => snapshot_key,
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(AppError::InvalidIndex(_)) => None,
            Err(error) => return Err(error),
        };
        let opened_base_snapshot = match snapshot_key.as_deref() {
            Some(snapshot_key) => {
                match WorkspaceBaseSnapshotIndex::open_if_matching(&self.build_config, snapshot_key)
                {
                    Ok(index) => index,
                    Err(error) => {
                        eprintln!(
                        "codgrep: failed to open base snapshot index engine for {} during bootstrap: {error}",
                        self.build_config.repo_path.display()
                    );
                        None
                    }
                }
            }
            None => None,
        };
        let base_snapshot_ready = opened_base_snapshot.is_some();
        {
            let mut slot = match self.base_snapshot_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = opened_base_snapshot;
        }
        self.clear_base_snapshot_catalog();
        if base_snapshot_ready {
            if let Err(error) = self.preload_base_snapshot_catalog() {
                eprintln!(
                    "codgrep: failed to preload base snapshot catalog for {} during bootstrap: {error}",
                    self.build_config.repo_path.display()
                );
                self.clear_base_snapshot_catalog();
            }
        }

        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = base_snapshot_ready.then(|| snapshot_key).flatten();
        state.last_probe_at = Some(now);
        state.last_error = None;
        apply_workspace_dirty_status(
            &mut state,
            self.rebuild_dirty_threshold,
            super::DirtyStats::default(),
        );
        drop(state);
        self.emit_workspace_status_changed();
        Ok(())
    }

    pub(super) fn open_base_snapshot_reader(
        &self,
    ) -> Result<Option<crate::index_engine::IndexSnapshotReader>> {
        let base_engine = match self.base_snapshot_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        base_engine
            .as_ref()
            .map(WorkspaceBaseSnapshotIndex::open_committed)
            .transpose()
    }

    pub(super) fn is_storage_path(&self, path: &std::path::Path) -> bool {
        self.normalize_repo_workspace_path(path.to_path_buf())
            .is_some_and(|candidate| {
                candidate.starts_with(self.normalized_storage_root())
                    || candidate.starts_with(self.normalized_workspace_engine_root())
            })
    }

    pub(super) fn normalized_storage_root(&self) -> PathBuf {
        if let Ok(canonical) = std::fs::canonicalize(&self.build_config.storage_root) {
            return canonical;
        }
        let Some(parent) = self.build_config.storage_root.parent() else {
            return self.build_config.storage_root.clone();
        };
        let Some(file_name) = self.build_config.storage_root.file_name() else {
            return self.build_config.storage_root.clone();
        };
        match std::fs::canonicalize(parent) {
            Ok(canonical_parent) => canonical_parent.join(file_name),
            Err(_) => self.build_config.storage_root.clone(),
        }
    }

    pub(super) fn normalized_workspace_engine_root(&self) -> PathBuf {
        normalize_repo_sidecar_path(workspace_engine_root(&self.build_config))
    }
}

pub(super) fn workspace_engine_root(build_config: &crate::config::BuildConfig) -> PathBuf {
    build_config.storage_root.clone()
}

fn normalize_repo_sidecar_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path;
    };
    let Some(file_name) = path.file_name() else {
        return path;
    };
    match std::fs::canonicalize(parent) {
        Ok(canonical_parent) => canonical_parent.join(file_name),
        Err(_) => path,
    }
}
