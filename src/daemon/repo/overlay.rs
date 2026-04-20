use std::sync::{atomic::Ordering, Arc};

use crate::{error::AppError, Result};

use super::{
    DirtyPathSet, RepoRuntime, WorkspaceOverlayIndex, WorkspaceOverlayIngestBatch,
    WorkspaceOverlayPublishPolicy, WorkspaceOverlayRuntimeStats, WorkspaceOverlaySnapshotState,
    WorkspaceOverlayStatus,
};

enum WorkspaceOverlayWritePlanKind {
    Clear,
    Rebuild {
        snapshot: WorkspaceOverlaySnapshotState,
    },
    ApplyDelta {
        batch: WorkspaceOverlayIngestBatch,
    },
}

pub(super) struct WorkspaceOverlayWritePlan {
    dirty_paths: DirtyPathSet,
    build_paths: DirtyPathSet,
    kind: WorkspaceOverlayWritePlanKind,
}

impl WorkspaceOverlayWritePlan {
    pub(super) fn clear() -> Self {
        Self {
            dirty_paths: DirtyPathSet::default(),
            build_paths: DirtyPathSet::default(),
            kind: WorkspaceOverlayWritePlanKind::Clear,
        }
    }

    pub(super) fn rebuild(
        dirty_paths: DirtyPathSet,
        snapshot: WorkspaceOverlaySnapshotState,
    ) -> Self {
        Self {
            build_paths: dirty_paths.clone(),
            dirty_paths,
            kind: WorkspaceOverlayWritePlanKind::Rebuild { snapshot },
        }
    }

    pub(super) fn apply_delta(
        dirty_paths: DirtyPathSet,
        build_paths: DirtyPathSet,
        batch: WorkspaceOverlayIngestBatch,
    ) -> Self {
        Self {
            build_paths,
            dirty_paths,
            kind: WorkspaceOverlayWritePlanKind::ApplyDelta { batch },
        }
    }
}

impl RepoRuntime {
    pub(super) fn apply_overlay_write_plan(&self, plan: WorkspaceOverlayWritePlan) {
        // Developer view of the overlay write path:
        // 1. Reconcile/probe builds a write plan from the current worktree and dirty catalog.
        // 2. Apply the plan to the overlay storage as either a full rebuild or an incremental
        //    publish.
        // 3. Mark the published paths as ready in the dirty catalog.
        // 4. Refresh the published-overlay cache from the visible snapshot so status and fallback
        //    readers mirror the query-visible state.
        let _write_guard = self
            .overlay_write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let planned_dirty = plan.dirty_paths.clone();
        let build_paths = plan.build_paths.clone();
        if let Err(error) = self.mark_dirty_catalog_building(&build_paths) {
            self.handle_workspace_overlay_engine_error("mark dirty catalog building", &error);
            return;
        }
        if let Err(error) = self.try_apply_overlay_write_plan(plan) {
            let _ = self.mark_dirty_catalog_failed(&build_paths);
            self.handle_workspace_overlay_engine_error("apply plan", &error);
            return;
        }
        if let Err(error) = self.mark_dirty_catalog_ready(&build_paths) {
            self.handle_workspace_overlay_engine_error("mark dirty catalog ready", &error);
            return;
        }
        self.replace_published_overlay_paths_cache(planned_dirty);
        self.update_overlay_state();
    }

    pub(super) fn reset_workspace_overlay_engine(&self) {
        self.apply_overlay_write_plan(WorkspaceOverlayWritePlan::clear());
    }

    fn try_apply_overlay_write_plan(&self, plan: WorkspaceOverlayWritePlan) -> Result<()> {
        let dirty_paths = plan.dirty_paths;
        match plan.kind {
            WorkspaceOverlayWritePlanKind::Clear => self.try_reset_workspace_overlay_engine(),
            WorkspaceOverlayWritePlanKind::Rebuild { snapshot } => {
                self.try_apply_overlay_snapshot(snapshot)
            }
            WorkspaceOverlayWritePlanKind::ApplyDelta { batch } => {
                self.try_apply_overlay_ingest_batch(&dirty_paths, batch)
            }
        }
    }

    fn try_apply_overlay_snapshot(&self, snapshot: WorkspaceOverlaySnapshotState) -> Result<()> {
        if !self.build_config.repo_path.exists() {
            return Ok(());
        }
        let previous = {
            let mut slot = match self.workspace_overlay_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            slot.take()
        };
        drop(previous);

        WorkspaceOverlayIndex::clear_storage(&self.build_config)?;

        let overlay_index = Arc::new(WorkspaceOverlayIndex::open_with_policy_and_hook(
            &self.build_config,
            self.workspace_overlay_publish_policy(),
            self.workspace_overlay_merge_state_hook(),
        )?);
        overlay_index.rebuild_snapshot(&snapshot)?;
        overlay_index.checkpoint()?;
        let mut slot = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(overlay_index);
        Ok(())
    }

    fn try_reset_workspace_overlay_engine(&self) -> Result<()> {
        {
            let mut slot = match self.workspace_overlay_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = None;
        }
        WorkspaceOverlayIndex::clear_storage(&self.build_config)
    }

    fn try_apply_overlay_ingest_batch(
        &self,
        dirty: &DirtyPathSet,
        batch: WorkspaceOverlayIngestBatch,
    ) -> Result<()> {
        if !self.build_config.repo_path.exists() {
            return Ok(());
        }
        let overlay_index = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .as_ref()
        .cloned();
        if overlay_index.is_none() {
            if dirty.is_empty() {
                return self.try_reset_workspace_overlay_engine();
            }
            let rebuild = self.build_overlay_rebuild_plan(dirty.clone())?;
            return self.try_apply_overlay_write_plan(rebuild);
        }
        let overlay_index = overlay_index.expect("workspace overlay engine should be initialized");
        overlay_index.ingest_batch(&batch)?;
        Ok(())
    }

    fn handle_workspace_overlay_engine_error(&self, operation: &str, error: &AppError) {
        {
            let mut slot = match self.workspace_overlay_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = None;
        }

        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }

        if matches!(error, AppError::Io(io_error) if io_error.kind() == std::io::ErrorKind::NotFound)
            && !self.build_config.repo_path.exists()
        {
            return;
        }

        self.mark_workspace_probe_pending_with_error(format!(
            "failed to {operation} workspace overlay engine: {error}"
        ));
        eprintln!(
            "codgrep: failed to {operation} workspace overlay engine for {}: {error}",
            self.build_config.repo_path.display()
        );
    }

    fn workspace_overlay_publish_policy(&self) -> WorkspaceOverlayPublishPolicy {
        WorkspaceOverlayPublishPolicy::RefreshWithAutoCheckpoint {
            max_uncommitted_ops: self.overlay_auto_checkpoint_max_uncommitted_ops,
        }
    }

    pub(super) fn workspace_overlay_status(&self) -> Option<WorkspaceOverlayStatus> {
        let overlay = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .as_ref()
        .cloned();
        overlay
            .as_ref()
            .and_then(|overlay| overlay.runtime_stats().ok())
            .map(workspace_overlay_status_from_runtime)
    }
}

fn workspace_overlay_status_from_runtime(
    stats: WorkspaceOverlayRuntimeStats,
) -> WorkspaceOverlayStatus {
    WorkspaceOverlayStatus {
        committed_seq_no: stats.committed_seq_no,
        last_seq_no: stats.last_seq_no,
        uncommitted_ops: stats.uncommitted_ops,
        pending_docs: stats.pending_docs,
        active_segments: stats.active_segments,
        active_delete_segments: stats.active_delete_segments,
        merge_requested: stats.merge_requested,
        merge_running: stats.merge_running,
        merge_attempts: stats.merge_attempts,
        merge_completed: stats.merge_completed,
        merge_failed: stats.merge_failed,
        last_merge_error: stats.last_merge_error,
    }
}
