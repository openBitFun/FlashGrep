use std::sync::{atomic::Ordering, Arc};

use crate::{error::AppError, Result};

use super::{
    DirtyPathSet, RepoRuntime, WorkspaceOverlayIndex, WorkspaceOverlayPublication,
    WorkspaceOverlayPublishPolicy, WorkspaceOverlayRuntimeStats, WorkspaceOverlayStatus,
};

enum WorkspaceOverlayWritePlanKind {
    Clear,
    Rebuild,
    ApplyDelta,
}

pub(super) struct WorkspaceOverlayWritePlan {
    dirty_paths: DirtyPathSet,
    build_paths: DirtyPathSet,
    retracted_paths: Vec<String>,
    source_head: Option<String>,
    manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    catalog_generation: u64,
    discovery_revision: Option<u64>,
    kind: WorkspaceOverlayWritePlanKind,
}

impl WorkspaceOverlayWritePlan {
    pub(super) fn clear() -> Self {
        Self {
            dirty_paths: DirtyPathSet::default(),
            build_paths: DirtyPathSet::default(),
            retracted_paths: Vec::new(),
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
            discovery_revision: None,
            kind: WorkspaceOverlayWritePlanKind::Clear,
        }
    }

    pub(super) fn rebuild(dirty_paths: DirtyPathSet) -> Self {
        Self {
            build_paths: dirty_paths.clone(),
            dirty_paths,
            retracted_paths: Vec::new(),
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
            discovery_revision: None,
            kind: WorkspaceOverlayWritePlanKind::Rebuild,
        }
    }

    pub(super) fn apply_delta(dirty_paths: DirtyPathSet, build_paths: DirtyPathSet) -> Self {
        Self {
            build_paths,
            dirty_paths,
            retracted_paths: Vec::new(),
            source_head: None,
            manifest_source: None,
            catalog_generation: 0,
            discovery_revision: None,
            kind: WorkspaceOverlayWritePlanKind::ApplyDelta,
        }
    }

    pub(super) fn with_discovery_revision(mut self, revision: u64) -> Self {
        self.discovery_revision = Some(revision);
        self
    }

    pub(super) fn with_retracted_paths(mut self, paths: Vec<String>) -> Self {
        self.retracted_paths = paths;
        self
    }

    pub(super) fn base_manifest_id(&self) -> Option<&crate::index_engine::ManifestId> {
        match &self.manifest_source {
            Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                base_manifest_id,
                ..
            }) => Some(base_manifest_id),
            _ => None,
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
            + self.retracted_paths.len()
    }

    pub(super) fn with_source_identity(
        mut self,
        source_head: Option<String>,
        manifest_source: crate::index_engine::manifest::ManifestSource,
    ) -> Self {
        self.source_head = source_head;
        self.manifest_source = Some(manifest_source);
        self
    }

    pub(super) fn with_catalog_generation(mut self, generation: u64) -> Self {
        self.catalog_generation = generation;
        self
    }
}

impl RepoRuntime {
    pub(super) fn apply_overlay_write_plan(&self, plan: WorkspaceOverlayWritePlan) -> Result<()> {
        let revision = plan.discovery_revision;
        let result = self.publish_overlay_write_plan(plan);
        if result.is_ok() {
            if let Some(revision) = revision {
                self.record_workspace_published(revision);
            }
        }
        if let Err(error) = &result {
            self.workspace_probe_pending.store(true, Ordering::Release);
            self.set_probe_healthy(false);
            self.set_last_error(Some(format!("workspace publication failed: {error}")));
        }
        result
    }

    fn publish_overlay_write_plan(&self, plan: WorkspaceOverlayWritePlan) -> Result<()> {
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
        if let Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
            base_manifest_id,
            ..
        }) = &plan.manifest_source
        {
            let current = self.current_base_publication_id()?;
            if current != *base_manifest_id {
                return Err(AppError::InvalidIndex(
                    "base publication changed while preparing the workspace update".into(),
                ));
            }
        }
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
            return Err(AppError::Protocol(
                "injected overlay publication interruption".into(),
            ));
        }
        let publication = match self.try_apply_overlay_write_plan(plan, manifest_source) {
            Ok(publication) => publication,
            Err(error) => {
                let _ = self.mark_dirty_catalog_failed(&build_paths, catalog_generation);
                self.handle_workspace_overlay_engine_error("apply plan", &error);
                return Err(error);
            }
        };
        #[cfg(test)]
        if self.take_overlay_publication_failpoint(
            super::OverlayPublicationFailpoint::AfterOverlayPublish,
        ) {
            return Err(AppError::Protocol(
                "injected overlay publication interruption".into(),
            ));
        }
        self.mark_dirty_catalog_ready_in_memory(&build_paths, catalog_generation);
        if publication.is_durable() {
            if let Err(error) = self.persist_dirty_catalog() {
                self.handle_workspace_overlay_engine_error("persist dirty catalog ready", &error);
                return Err(error);
            }
        }
        #[cfg(test)]
        if self.take_overlay_publication_failpoint(
            super::OverlayPublicationFailpoint::AfterCatalogReady,
        ) {
            return Err(AppError::Protocol(
                "injected overlay publication interruption".into(),
            ));
        }
        self.replace_published_overlay_paths_cache(planned_dirty);
        self.mark_overlay_published_for_head(source_head);
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
        Ok(())
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

    pub(super) fn reset_workspace_overlay_engine(&self) -> Result<()> {
        self.apply_overlay_write_plan(WorkspaceOverlayWritePlan::clear())
    }

    fn try_apply_overlay_write_plan(
        &self,
        plan: WorkspaceOverlayWritePlan,
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<WorkspaceOverlayPublication> {
        let dirty_paths = plan.dirty_paths;
        match plan.kind {
            WorkspaceOverlayWritePlanKind::Clear => {
                self.try_reset_workspace_overlay_engine(manifest_source)
            }
            WorkspaceOverlayWritePlanKind::Rebuild => {
                self.try_replace_workspace_overlay(&dirty_paths, manifest_source)
            }
            WorkspaceOverlayWritePlanKind::ApplyDelta => self.try_apply_overlay_ingest_batch(
                &dirty_paths,
                &plan.build_paths,
                &plan.retracted_paths,
                manifest_source,
            ),
        }
    }

    fn try_replace_workspace_overlay(
        &self,
        dirty: &DirtyPathSet,
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<WorkspaceOverlayPublication> {
        let existing = self
            .workspace_overlay_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let overlay = match existing {
            Some(overlay) => overlay,
            None => Arc::new(self.open_workspace_overlay_index()?),
        };
        if let Some(source) = manifest_source {
            overlay.set_manifest_source(source);
        }
        let publication = overlay.replace_batches(self.stream_overlay_batches(dirty))?;
        *self
            .workspace_overlay_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(overlay);
        Ok(publication)
    }

    fn try_reset_workspace_overlay_engine(
        &self,
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<WorkspaceOverlayPublication> {
        self.try_replace_workspace_overlay(&DirtyPathSet::default(), manifest_source)
    }

    fn try_apply_overlay_ingest_batch(
        &self,
        dirty: &DirtyPathSet,
        delta: &DirtyPathSet,
        retracted_paths: &[String],
        manifest_source: Option<crate::index_engine::manifest::ManifestSource>,
    ) -> Result<WorkspaceOverlayPublication> {
        let overlay = self
            .workspace_overlay_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(overlay) = overlay else {
            return self.try_replace_workspace_overlay(dirty, manifest_source);
        };
        if let Some(source) = manifest_source {
            overlay.set_manifest_source(source);
        }
        let retractions = retracted_paths.iter().map(|path| {
            super::WorkspaceOverlayIngestBatch::from_retracted_paths(
                &self.overlay_repo_id,
                &self.build_config.repo_path,
                std::slice::from_ref(path),
            )
        });
        overlay.ingest_batches(self.stream_overlay_batches(delta).chain(retractions))
    }

    pub(super) fn open_workspace_overlay_index(&self) -> Result<WorkspaceOverlayIndex> {
        WorkspaceOverlayIndex::open_with_policy_and_hook(
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
            if let Some(old) = slot.take() {
                old.retire();
            }
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
        WorkspaceOverlayPublishPolicy::Background
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

    pub(super) fn checkpoint_workspace_overlay_for_shutdown(&self) -> Result<()> {
        if !self.build_config.repo_path.exists() || !self.build_config.storage_root.exists() {
            return Ok(());
        }
        let _write_guard = self
            .overlay_write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let overlay = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .as_ref()
        .cloned();
        let Some(overlay) = overlay else {
            return Ok(());
        };
        if overlay.needs_checkpoint() {
            overlay.checkpoint()?;
        }
        self.persist_dirty_catalog()
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

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::fs;

    fn runtime(temp: &tempfile::TempDir) -> RepoRuntime {
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        RepoRuntime::new(
            "repo".into(),
            crate::config::BuildConfig {
                repo_path: root,
                storage_root: temp.path().join("index"),
                git_mode: crate::config::GitMode::Filesystem,
                ..Default::default()
            },
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap()
    }

    #[test]
    fn committing_published_file_contents_updates_identity_without_reindexing() {
        let repo = crate::test_support::TestRepo::new();
        repo.write("tracked.rs", "BEFORE_COMMIT\n");
        repo.init_git();
        repo.commit_all("base");
        let runtime = RepoRuntime::new(
            "repo".into(),
            repo.build_config(),
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        runtime.build_base_snapshot().unwrap();
        repo.write("tracked.rs", "AFTER_COMMIT\n");
        runtime.refresh_workspace_overlay().unwrap();
        let overlay = runtime
            .workspace_overlay_engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let seq_before = overlay.writer_stats().last_seq_no;
        repo.commit_all("commit already indexed contents");
        let head = runtime.current_git_head().unwrap();
        runtime.observe_git_head_transition().unwrap();
        let plan = runtime
            .plan_overlay_update_from_incremental_probe()
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.build_path_count(),
            0,
            "metadata publication must reuse indexed contents"
        );
        runtime.apply_overlay_write_plan(plan).unwrap();
        assert_eq!(overlay.writer_stats().last_seq_no, seq_before);
        let snapshot = overlay.open_snapshot().unwrap();
        assert!(matches!(snapshot.manifest().source.as_ref(),
            Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay { observed_head, .. })
            if *observed_head == head
        ));
        assert_eq!(runtime.state.lock().unwrap().overlay_head_commit, head);
    }

    #[test]
    fn visible_publication_survives_catalog_failure_and_checkpoint_retries() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let catalog_path = temp.path().join("index/dirty-catalog.json");
        fs::create_dir_all(&catalog_path).unwrap();
        fs::write(catalog_path.join("blocker"), "prevent catalog publication").unwrap();
        runtime
            .apply_refreshed_overlay("snapshot".into(), WorkspaceOverlayWritePlan::clear())
            .unwrap();
        assert!(runtime.state.lock().unwrap().probe_healthy);
        assert!(runtime.checkpoint_workspace_overlay_for_shutdown().is_err());
        assert!(runtime
            .dirty_catalog_persist_pending
            .load(Ordering::Acquire));
        fs::remove_dir_all(&catalog_path).unwrap();
        runtime.checkpoint_workspace_overlay_for_shutdown().unwrap();
        assert!(catalog_path.is_file());
        assert!(!runtime
            .dirty_catalog_persist_pending
            .load(Ordering::Acquire));
    }

    #[test]
    fn publication_keeps_notifications_admitted_after_the_consumed_batch_pending() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        runtime.workspace_event_buffer.lock().unwrap().observed = true;
        runtime
            .apply_refreshed_overlay("snapshot".into(), WorkspaceOverlayWritePlan::clear())
            .unwrap();
        assert!(runtime.workspace_probe_pending.load(Ordering::Acquire));
        assert!(runtime.take_workspace_events().observed);
        runtime.finish_workspace_probe_without_overlay_change(Some("snapshot".into()));
        assert!(!runtime.workspace_probe_pending.load(Ordering::Acquire));
    }
    #[test]
    fn stale_overlay_plan_and_snapshot_are_rejected_after_base_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        fs::write(
            runtime.build_config.repo_path.join("base.rs"),
            "BASE_GENERATION
",
        )
        .unwrap();
        runtime.build_base_snapshot().unwrap();
        fs::write(
            runtime.build_config.repo_path.join("added.rs"),
            "OVERLAY_GENERATION
",
        )
        .unwrap();
        let initial_plan = runtime.plan_overlay_full_rebuild_from_worktree().unwrap();
        runtime.apply_overlay_write_plan(initial_plan).unwrap();
        let old_overlay = runtime
            .workspace_overlay_engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .open_snapshot()
            .unwrap();
        let old_base = runtime.open_base_snapshot_reader().unwrap().unwrap();
        assert!(runtime
            .workspace_search_view
            .open_workspace_query_snapshot(Some(old_base), Some(old_overlay.clone()))
            .is_ok());
        let stale_plan = runtime.plan_overlay_full_rebuild_from_worktree().unwrap();
        runtime.rebuild_base_snapshot().unwrap();
        assert!(runtime.apply_overlay_write_plan(stale_plan).is_err());
        let new_base = runtime.open_base_snapshot_reader().unwrap().unwrap();
        assert!(runtime
            .workspace_search_view
            .open_workspace_query_snapshot(Some(new_base), Some(old_overlay))
            .is_err());
    }
    #[test]
    fn repeated_dirty_updates_and_retraction_preserve_unrelated_overlay_postings() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let root = &runtime.build_config.repo_path;
        fs::write(root.join("first.rs"), "FIRST_BASE\n").unwrap();
        fs::write(root.join("second.rs"), "SECOND_BASE\n").unwrap();
        runtime.build_base_snapshot().unwrap();
        fs::write(root.join("first.rs"), "FIRST_DIRTY_INITIAL\n").unwrap();
        fs::write(root.join("second.rs"), "SECOND_DIRTY_INITIAL\n").unwrap();
        runtime.refresh_workspace_overlay().unwrap();
        fs::write(root.join("first.rs"), "FIRST_DIRTY_UPDATED_LONGER\n").unwrap();
        runtime.refresh_workspace_overlay().unwrap();
        let overlay = runtime
            .workspace_overlay_engine
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let before = overlay.open_snapshot().unwrap();
        let docs = before.visible_documents().unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs
            .values()
            .any(|doc| doc.source_bytes.as_ref() == b"FIRST_DIRTY_UPDATED_LONGER\n"));
        assert!(docs
            .values()
            .any(|doc| doc.source_bytes.as_ref() == b"SECOND_DIRTY_INITIAL\n"));
        let sequence = overlay.writer_stats().last_seq_no;
        fs::write(root.join("first.rs"), "FIRST_BASE\n").unwrap();
        runtime.refresh_workspace_overlay().unwrap();
        let restored = overlay.open_snapshot().unwrap();
        assert_eq!(
            overlay.writer_stats().last_seq_no,
            sequence + 1,
            "restoring one path must not rebuild unrelated dirty documents"
        );
        assert_eq!(
            restored.manifest().active_segments,
            before.manifest().active_segments
        );
        let keys = restored.visible_keys().unwrap();
        assert_eq!(keys.live.len(), 1);
        assert_eq!(keys.live.iter().next().unwrap().logical_path, "second.rs");
        assert!(
            keys.deleted.is_empty(),
            "restored path must stop shadowing its base document"
        );
        assert_eq!(before.visible_documents().unwrap().len(), 2);
    }

    #[test]
    fn workspace_progress_does_not_publish_later_events_or_failed_batches() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let root = &runtime.build_config.repo_path;
        fs::write(root.join("base.rs"), "BASE\n").unwrap();
        runtime.build_base_snapshot().unwrap();
        fs::write(root.join("base.rs"), "FIRST_DIRTY\n").unwrap();
        let plan = runtime.plan_overlay_full_rebuild_from_worktree().unwrap();
        let planned = runtime.workspace_progress_snapshot();
        assert_eq!(planned.reconciled_revision, planned.observed_revision);
        assert!(planned.published_revision < planned.reconciled_revision);
        runtime.enqueue_full_workspace_probe();
        let later = runtime.workspace_progress_snapshot().observed_revision;
        assert!(later > planned.reconciled_revision);
        runtime.apply_overlay_write_plan(plan).unwrap();
        let published = runtime.workspace_progress_snapshot();
        assert_eq!(published.published_revision, planned.reconciled_revision);
        assert_eq!(published.observed_revision, later);
        runtime.refresh_workspace_overlay().unwrap();
        let settled = runtime.workspace_progress_snapshot();
        assert_eq!(settled.published_revision, settled.observed_revision);
        fs::write(root.join("base.rs"), "SECOND_DIRTY_LONGER\n").unwrap();
        let failed_plan = runtime.plan_overlay_full_rebuild_from_worktree().unwrap();
        *runtime.overlay_publication_failpoint.lock().unwrap() =
            Some(super::super::OverlayPublicationFailpoint::BeforeOverlayPublish);
        assert!(runtime.apply_overlay_write_plan(failed_plan).is_err());
        let failed = runtime.workspace_progress_snapshot();
        assert_eq!(failed.published_revision, settled.published_revision);
        assert!(failed.reconciled_revision > failed.published_revision);
        assert!(runtime.workspace_probe_pending.load(Ordering::Acquire));
        runtime.refresh_workspace_overlay().unwrap();
        let recovered = runtime.workspace_progress_snapshot();
        assert_eq!(recovered.published_revision, recovered.reconciled_revision);
        assert_eq!(recovered.published_revision, recovered.observed_revision);
    }

    #[test]
    fn workspace_progress_scoped_batch_uses_its_consumed_revision() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(&temp);
        let path = runtime.build_config.repo_path.join("base.rs");
        fs::write(&path, "BASE\n").unwrap();
        runtime.build_base_snapshot().unwrap();
        fs::write(&path, "SCOPED_CHANGE\n").unwrap();
        let consumed_revision = runtime.observe_workspace_revision();
        let later_revision = runtime.observe_workspace_revision();
        let plan = runtime
            .plan_overlay_update_from_event_probe([path], consumed_revision)
            .unwrap()
            .unwrap();
        runtime.apply_overlay_write_plan(plan).unwrap();
        let progress = runtime.workspace_progress_snapshot();
        assert_eq!(progress.reconciled_revision, consumed_revision);
        assert_eq!(progress.published_revision, consumed_revision);
        assert_eq!(progress.observed_revision, later_revision);
    }
}
