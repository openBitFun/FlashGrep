use std::{
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

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
    source_head: Option<String>,
    manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    catalog_generation: u64,
    kind: WorkspaceOverlayWritePlanKind,
}

impl WorkspaceOverlayWritePlan {
    pub(super) fn clear() -> Self {
        Self {
            dirty_paths: DirtyPathSet::default(),
            build_paths: DirtyPathSet::default(),
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
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
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
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
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
            kind: WorkspaceOverlayWritePlanKind::ApplyDelta { batch },
        }
    }

    pub(super) fn dirty_path_count(&self) -> usize {
        self.dirty_paths.modified.len()
            + self.dirty_paths.deleted.len()
            + self.dirty_paths.new.len()
    }

    pub(super) fn build_path_count(&self) -> usize {
        self.build_paths.modified.len()
            + self.build_paths.deleted.len()
            + self.build_paths.new.len()
    }

    pub(super) fn with_source_identity(
        mut self,
        source_head: String,
        manifest_source: crate::index_engine::manifest::ManifestSource,
    ) -> Self {
        self.source_head = Some(source_head);
        self.manifest_source = Some(manifest_source);
        self
    }

    pub(super) fn with_catalog_generation(mut self, generation: u64) -> Self {
        self.catalog_generation = generation;
        self
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let planned_dirty = plan.dirty_paths.clone();
        let build_paths = plan.build_paths.clone();
        let catalog_generation = plan.catalog_generation;
        let source_head = plan.source_head.clone();
        let manifest_source = plan.manifest_source.clone();
        let operation = match &plan.kind {
            WorkspaceOverlayWritePlanKind::Clear => "clear",
            WorkspaceOverlayWritePlanKind::Rebuild { .. } => "rebuild",
            WorkspaceOverlayWritePlanKind::ApplyDelta { .. } => "delta",
        };
        let dirty_path_count = plan.dirty_path_count();
        let build_path_count = plan.build_path_count();
        self.mark_dirty_catalog_building(&build_paths, catalog_generation);
        #[cfg(test)]
        if self.take_overlay_publication_failpoint(
            super::OverlayPublicationFailpoint::BeforeOverlayPublish,
        ) {
            return;
        }
        if let Err(error) = self.try_apply_overlay_write_plan(plan, manifest_source) {
            let _ = self.mark_dirty_catalog_failed(&build_paths, catalog_generation);
            self.handle_workspace_overlay_engine_error("apply plan", &error);
            return;
        }
        #[cfg(test)]
        if self.take_overlay_publication_failpoint(
            super::OverlayPublicationFailpoint::AfterOverlayPublish,
        ) {
            return;
        }
        if let Err(error) = self.mark_dirty_catalog_ready(&build_paths, catalog_generation) {
            self.handle_workspace_overlay_engine_error("mark dirty catalog ready", &error);
            return;
        }
        #[cfg(test)]
        if self.take_overlay_publication_failpoint(
            super::OverlayPublicationFailpoint::AfterCatalogReady,
        ) {
            return;
        }
        self.replace_published_overlay_paths_cache(planned_dirty);
        if source_head.is_some() {
            self.mark_overlay_published_for_head(source_head);
        }
        self.update_overlay_state();
        match operation {
            "clear" => eprintln!(
                "flashgrep[overlay][info] cleared workspace overlay storage for {}",
                self.build_config.repo_path.display()
            ),
            "rebuild" => eprintln!(
                "flashgrep[overlay][info] rebuilt workspace overlay for {}: dirty_paths={} build_paths={}",
                self.build_config.repo_path.display(),
                dirty_path_count,
                build_path_count
            ),
            _ => {}
        }
    }

    #[cfg(test)]
    fn take_overlay_publication_failpoint(
        &self,
        expected: super::OverlayPublicationFailpoint,
    ) -> bool {
        let mut slot = match self.overlay_publication_failpoint.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *slot != Some(expected) {
            return false;
        }
        slot.take();
        true
    }

    pub(super) fn reset_workspace_overlay_engine(&self) {
        self.apply_overlay_write_plan(WorkspaceOverlayWritePlan::clear());
    }

    fn try_apply_overlay_write_plan(
        &self,
        plan: WorkspaceOverlayWritePlan,
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<()> {
        let dirty_paths = plan.dirty_paths;
        match plan.kind {
            WorkspaceOverlayWritePlanKind::Clear => self.try_reset_workspace_overlay_engine(),
            WorkspaceOverlayWritePlanKind::Rebuild { snapshot } => {
                self.try_apply_overlay_snapshot(snapshot, manifest_source)
            }
            WorkspaceOverlayWritePlanKind::ApplyDelta { batch } => {
                self.try_apply_overlay_ingest_batch(&dirty_paths, batch, manifest_source)
            }
        }
    }

    fn try_apply_overlay_snapshot(
        &self,
        snapshot: WorkspaceOverlaySnapshotState,
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<()> {
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

        let overlay_index = Arc::new(self.open_workspace_overlay_index()?);
        if let Some(source) = manifest_source {
            overlay_index.set_manifest_source(source);
        }
        let publish_started_at = Instant::now();
        overlay_index.rebuild_snapshot(&snapshot)?;
        let segment_elapsed = publish_started_at.elapsed();
        let checkpoint_started_at = Instant::now();
        overlay_index.checkpoint()?;
        eprintln!(
            "flashgrep[overlay-metrics][info] stage=publish_rebuild segment_ms={} checkpoint_ms={}",
            segment_elapsed.as_millis(),
            checkpoint_started_at.elapsed().as_millis()
        );
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
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
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
            if self.dirty_catalog_pending_paths() == *dirty {
                WorkspaceOverlayIndex::clear_storage(&self.build_config)?;
                let overlay_index = Arc::new(self.open_workspace_overlay_index()?);
                if let Some(source) = manifest_source {
                    overlay_index.set_manifest_source(source);
                }
                overlay_index.ingest_batch(&batch)?;
                let mut slot = match self.workspace_overlay_engine.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *slot = Some(overlay_index);
                return Ok(());
            }
            eprintln!(
                "flashgrep[overlay][info] workspace overlay engine missing for {}; rebuilding from dirty paths: modified={} deleted={} new={}",
                self.build_config.repo_path.display(),
                dirty.modified.len(),
                dirty.deleted.len(),
                dirty.new.len()
            );
            let rebuild = self.build_overlay_rebuild_plan(dirty.clone())?;
            let rebuild_source = rebuild.manifest_source.clone();
            return self.try_apply_overlay_write_plan(rebuild, rebuild_source);
        }
        let Some(overlay_index) = overlay_index else {
            return Err(AppError::Protocol(
                "workspace overlay engine missing during overlay delta publish".into(),
            ));
        };
        if let Some(source) = manifest_source {
            overlay_index.set_manifest_source(source);
        }
        let publish_started_at = Instant::now();
        overlay_index.ingest_batch(&batch)?;
        eprintln!(
            "flashgrep[overlay-metrics][info] stage=publish_delta elapsed_ms={}",
            publish_started_at.elapsed().as_millis()
        );
        Ok(())
    }

    fn open_workspace_overlay_index(&self) -> Result<WorkspaceOverlayIndex> {
        WorkspaceOverlayIndex::open_with_policy_and_hook(
            &self.build_config,
            self.workspace_overlay_publish_policy(),
            self.workspace_overlay_merge_state_hook(),
        )
    }

    pub(super) fn open_workspace_overlay_index_manifest_only(
        &self,
    ) -> Result<WorkspaceOverlayIndex> {
        WorkspaceOverlayIndex::open_with_policy_and_hook_manifest_only(
            &self.build_config,
            self.workspace_overlay_publish_policy(),
            self.workspace_overlay_merge_state_hook(),
        )
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

        if self.should_suppress_workspace_overlay_io_error(error) {
            return;
        }

        self.mark_workspace_probe_pending_with_error(format!(
            "failed to {operation} workspace overlay engine: {error}"
        ));
        eprintln!(
            "flashgrep[overlay][warn] failed to {operation} workspace overlay engine for {}: {error}",
            self.build_config.repo_path.display()
        );
    }

    fn workspace_overlay_publish_policy(&self) -> WorkspaceOverlayPublishPolicy {
        WorkspaceOverlayPublishPolicy::AutoCheckpoint {
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

    pub(super) fn should_suppress_workspace_overlay_io_error(&self, error: &AppError) -> bool {
        if !matches!(error, AppError::Io(_)) {
            return false;
        }
        if self.shutting_down.load(Ordering::Relaxed) {
            return true;
        }
        !self.build_config.repo_path.exists() || !self.build_config.storage_root.exists()
    }
}

fn workspace_overlay_status_from_runtime(
    stats: WorkspaceOverlayRuntimeStats,
) -> WorkspaceOverlayStatus {
    WorkspaceOverlayStatus {
        base_manifest_id: stats.base_manifest_id.map(|id| id.0),
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
