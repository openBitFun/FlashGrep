use std::sync::atomic::Ordering;

use crate::{
    daemon::convert,
    error::{AppError, Result},
    index_engine::{WorkspaceBaseSnapshotIndex, WorkspaceOverlayIndex},
};

use super::{
    DirtyPathSet, DirtyStats, RepoPhase, RepoRuntime, RepoState, RepoStatus, RepoTaskStatus,
};

impl Default for RepoState {
    fn default() -> Self {
        Self {
            phase: RepoPhase::Opening,
            snapshot_key: None,
            last_probe_at: None,
            last_rebuild_at: None,
            dirty_files: DirtyStats::default(),
            rebuild_recommended: false,
            active_task_id: None,
            probe_healthy: true,
            last_error: None,
        }
    }
}

impl RepoRuntime {
    pub(super) fn status_snapshot(&self) -> RepoStatus {
        let state = match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let overlay = self.workspace_overlay_status();
        let mut status = RepoStatus {
            repo_id: self.repo_id.clone(),
            repo_path: self.build_config.repo_path.clone(),
            storage_root: self.build_config.storage_root.clone(),
            base_snapshot_root: WorkspaceBaseSnapshotIndex::index_root(&self.build_config),
            workspace_overlay_root: WorkspaceOverlayIndex::index_root(&self.build_config),
            phase: state.phase,
            snapshot_key: state.snapshot_key,
            last_probe_at: state.last_probe_at,
            last_rebuild_at: state.last_rebuild_at,
            dirty_files: state.dirty_files,
            rebuild_recommended: state.rebuild_recommended,
            active_task_id: state.active_task_id,
            probe_healthy: state.probe_healthy,
            last_error: state.last_error,
            overlay,
        };
        if matches!(
            status.phase,
            RepoPhase::Opening | RepoPhase::ReadyClean | RepoPhase::ReadyDirty
        ) && status.snapshot_key.is_some()
        {
            apply_repo_status_dirty_fields(
                &mut status,
                self.rebuild_dirty_threshold,
                self.dirty_catalog_stats(),
            );
        }
        status
    }

    pub(super) fn emit_workspace_status_changed(&self) {
        (self.notifier)(super::ServiceNotificationEvent::WorkspaceStatus(
            convert::convert_repo_status(self.status_snapshot()),
        ));
    }

    pub(super) fn emit_task_progress(&self, task: RepoTaskStatus) {
        (self.notifier)(super::ServiceNotificationEvent::TaskProgress(task));
    }

    pub(super) fn emit_task_finished(&self, task: RepoTaskStatus) {
        (self.notifier)(super::ServiceNotificationEvent::TaskFinished(task));
    }

    pub(super) fn set_phase(&self, phase: RepoPhase, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.phase = phase;
        state.last_error = last_error;
        drop(state);
        self.emit_workspace_status_changed();
    }

    pub(super) fn set_last_error(&self, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.last_error = last_error;
        drop(state);
        self.emit_workspace_status_changed();
    }

    pub(super) fn set_probe_healthy(&self, probe_healthy: bool) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.probe_healthy = probe_healthy;
        drop(state);
        self.emit_workspace_status_changed();
    }

    #[cfg(test)]
    pub(super) fn mark_workspace_probe_pending(&self) {
        self.workspace_probe_pending.store(true, Ordering::Relaxed);
        self.schedule_workspace_probe(self.workspace_probe_interval());
        self.invalidate_glob_cache();
        self.emit_workspace_status_changed();
    }

    pub(super) fn mark_workspace_probe_pending_with_error(&self, error: String) {
        self.workspace_probe_pending.store(true, Ordering::Relaxed);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.last_error = Some(error);
        state.probe_healthy = false;
        drop(state);
        self.schedule_workspace_probe(self.workspace_probe_interval());
        self.invalidate_glob_cache();
        self.emit_workspace_status_changed();
    }

    pub(super) fn invalidate_glob_cache(&self) {
        self.glob_cache_epoch.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot_published_overlay_paths(&self) -> Result<Option<DirtyPathSet>> {
        let overlay = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .as_ref()
        .cloned();
        let Some(overlay) = overlay else {
            return Ok(None);
        };
        match overlay.open_snapshot() {
            Ok(snapshot) => self
                .rebuild_dirty_paths_from_overlay_snapshot(&snapshot)
                .map(Some),
            Err(AppError::InvalidIndex(_)) => Ok(None),
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn published_overlay_paths(&self) -> Result<DirtyPathSet> {
        // Query correctness comes from the published overlay snapshot itself. The in-memory cache
        // is only a fallback mirror for status/strict-readiness paths when the published snapshot
        // is temporarily unavailable.
        if let Some(published_overlay_paths) = self.snapshot_published_overlay_paths()? {
            return Ok(published_overlay_paths);
        }
        Ok(self.cached_published_overlay_paths())
    }

    pub(super) fn update_overlay_state(&self) {
        let published_overlay_paths = match self.published_overlay_paths() {
            Ok(paths) => paths,
            Err(error) => {
                if self.should_suppress_workspace_overlay_io_error(&error) {
                    return;
                }
                self.mark_workspace_probe_pending_with_error(format!(
                    "failed to derive workspace overlay state: {error}"
                ));
                return;
            }
        };
        self.replace_published_overlay_paths_cache(published_overlay_paths);
        let dirty_stats = self.dirty_catalog_stats();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        apply_workspace_dirty_status(&mut state, self.rebuild_dirty_threshold, dirty_stats);
        let rebuild_recommended = state.rebuild_recommended;
        drop(state);
        if rebuild_recommended {
            self.schedule_background_base_rebuild();
        }
        self.invalidate_glob_cache();
        self.emit_workspace_status_changed();
    }
}

pub(super) fn apply_workspace_dirty_status(
    state: &mut RepoState,
    rebuild_dirty_threshold: usize,
    dirty_stats: DirtyStats,
) {
    state.dirty_files = dirty_stats;
    state.rebuild_recommended = dirty_stats_total(dirty_stats) >= rebuild_dirty_threshold;
    state.phase = derive_workspace_phase(state.snapshot_key.as_deref(), dirty_stats);
}

pub(super) fn apply_repo_status_dirty_fields(
    status: &mut RepoStatus,
    rebuild_dirty_threshold: usize,
    dirty_stats: DirtyStats,
) {
    status.dirty_files = dirty_stats;
    status.rebuild_recommended = dirty_stats_total(dirty_stats) >= rebuild_dirty_threshold;
    status.phase = derive_workspace_phase(status.snapshot_key.as_deref(), dirty_stats);
}

fn derive_workspace_phase(snapshot_key: Option<&str>, dirty_stats: DirtyStats) -> RepoPhase {
    if snapshot_key.is_none() {
        RepoPhase::MissingBaseSnapshot
    } else if dirty_stats_total(dirty_stats) == 0 {
        RepoPhase::ReadyClean
    } else {
        RepoPhase::ReadyDirty
    }
}

fn dirty_stats_total(dirty_stats: DirtyStats) -> usize {
    dirty_stats.modified + dirty_stats.deleted + dirty_stats.new
}
