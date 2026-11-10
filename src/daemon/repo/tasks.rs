use std::{
    sync::{atomic::Ordering, Arc},
    thread,
    time::SystemTime,
};

use crate::{
    base_snapshot_build::BaseSnapshotBuildOptions,
    error::{AppError, Result},
    index_engine::WorkspaceBaseSnapshotIndex,
    progress::IndexProgress,
};

use super::{
    ActiveTaskHandle, RepoPhase, RepoRuntime, RepoTaskKind, RepoTaskRecord, RepoTaskState,
    RepoTaskStatus,
};

impl RepoTaskRecord {
    fn new(
        task_id: String,
        repo_id: String,
        kind: RepoTaskKind,
        owns_repo_phase: bool,
        target_head: Option<String>,
    ) -> Self {
        let now = SystemTime::now();
        Self {
            kind,
            target_head,
            owns_repo_phase,
            cancel_requested: std::sync::atomic::AtomicBool::new(false),
            state: std::sync::Mutex::new(RepoTaskStatus {
                task_id,
                repo_id,
                kind,
                state: RepoTaskState::Queued,
                phase: None,
                message: String::new(),
                processed: 0,
                total: None,
                started_at: now,
                updated_at: now,
                finished_at: None,
                cancellable: true,
                error: None,
            }),
        }
    }

    fn snapshot(&self) -> RepoTaskStatus {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn mark_running(&self) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state = RepoTaskState::Running;
        state.updated_at = SystemTime::now();
    }

    fn update_progress(&self, progress: IndexProgress) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state = RepoTaskState::Running;
        state.phase = Some(progress.phase);
        state.message = progress.message;
        state.processed = progress.processed;
        state.total = progress.total;
        state.updated_at = SystemTime::now();
    }

    fn finish_completed(&self, message: String) {
        let now = SystemTime::now();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state = RepoTaskState::Completed;
        if self.kind != RepoTaskKind::AdvanceBaseSnapshot || state.message.is_empty() {
            state.message = message;
        }
        state.updated_at = now;
        state.finished_at = Some(now);
        state.cancellable = false;
        state.error = None;
    }

    fn finish_cancelled(&self, message: String) {
        let now = SystemTime::now();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state = RepoTaskState::Cancelled;
        state.message = message;
        state.updated_at = now;
        state.finished_at = Some(now);
        state.cancellable = false;
        state.error = None;
    }

    fn finish_failed(&self, message: String) {
        let now = SystemTime::now();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.state = RepoTaskState::Failed;
        state.message = message.clone();
        state.updated_at = now;
        state.finished_at = Some(now);
        state.cancellable = false;
        state.error = Some(message);
    }

    fn request_cancel(&self) -> bool {
        let was = self.cancel_requested.swap(true, Ordering::Relaxed);
        !was && self.snapshot().cancellable
    }

    fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Relaxed)
    }
}

impl RepoRuntime {
    pub(super) fn start_task(
        self: &Arc<Self>,
        task_id: String,
        kind: RepoTaskKind,
    ) -> Result<RepoTaskStatus> {
        self.start_task_with_repo_phase(task_id, kind, true)
    }

    pub(super) fn start_background_task(
        self: &Arc<Self>,
        task_id: String,
        kind: RepoTaskKind,
    ) -> Result<RepoTaskStatus> {
        self.start_task_with_repo_phase(task_id, kind, false)
    }

    pub(super) fn schedule_base_advance(&self) {
        if self.has_active_task() {
            return;
        }
        let Some(runtime) = self.self_handle.get().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        let seq = self
            .next_background_task_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let task_id = format!("auto:advance:{}:{seq}", self.repo_id);
        let _ = runtime.start_background_task(task_id, RepoTaskKind::AdvanceBaseSnapshot);
    }

    pub(super) fn schedule_base_compaction(&self) {
        if self.has_active_task() {
            return;
        }
        let Some(runtime) = self.self_handle.get().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        let seq = self
            .next_background_task_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let task_id = format!("auto:compact:{}:{seq}", self.repo_id);
        let _ = runtime.start_background_task(task_id, RepoTaskKind::CompactBaseDeltas);
    }

    fn start_task_with_repo_phase(
        self: &Arc<Self>,
        task_id: String,
        kind: RepoTaskKind,
        update_repo_phase: bool,
    ) -> Result<RepoTaskStatus> {
        self.cleanup_finished_task_handle();
        if let Some(active) = self.active_task_status() {
            if active.kind == kind
                && matches!(active.state, RepoTaskState::Queued | RepoTaskState::Running)
            {
                return Ok(active);
            }
        }
        self.ensure_no_active_task()?;

        let target_head = if kind == RepoTaskKind::AdvanceBaseSnapshot {
            Some(self.current_git_head()?.ok_or_else(|| {
                AppError::InvalidIndex("Git base advance requires a commit".into())
            })?)
        } else {
            None
        };
        let task = Arc::new(RepoTaskRecord::new(
            task_id.clone(),
            self.repo_id.clone(),
            kind,
            update_repo_phase,
            target_head,
        ));
        {
            let mut tasks = match self.tasks.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            tasks.insert(task_id.clone(), Arc::clone(&task));
        }
        self.mark_task_started(kind, Some(task_id.clone()), update_repo_phase);

        let runtime = Arc::clone(self);
        let thread_task = Arc::clone(&task);
        let handle = thread::spawn(move || {
            runtime.run_task(thread_task);
        });
        {
            let mut slot = match self.task_handle.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = Some(ActiveTaskHandle { handle });
        }

        Ok(task.snapshot())
    }

    pub(super) fn task_status(&self, task_id: &str) -> Option<RepoTaskStatus> {
        let tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tasks.get(task_id).map(|task| task.snapshot())
    }

    pub(super) fn task_target_head(&self, task_id: &str) -> Option<String> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .and_then(|task| task.target_head.clone())
    }

    pub(super) fn cancel_task(&self, task_id: &str) -> Option<bool> {
        let tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tasks.get(task_id).map(|task| task.request_cancel())
    }

    pub(super) fn build_base_snapshot(&self) -> Result<(usize, super::RepoStatus)> {
        self.ensure_no_active_task()?;
        let discovery = self
            .workspace_discovery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.set_phase(RepoPhase::BuildingBaseSnapshot, None);
        let indexed_docs = WorkspaceBaseSnapshotIndex::build_with_options(
            &self.build_config,
            &self.overlay_repo_id,
            BaseSnapshotBuildOptions::new(),
        )?
        .indexed_docs;
        self.bootstrap_from_existing_snapshot()?;
        drop(discovery);
        self.refresh_workspace_overlay()?;
        {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        Ok((indexed_docs, self.status_snapshot()))
    }

    pub(super) fn rebuild_base_snapshot(&self) -> Result<(usize, super::RepoStatus)> {
        self.ensure_no_active_task()?;
        let discovery = self
            .workspace_discovery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.set_phase(RepoPhase::RebuildingBaseSnapshot, None);
        let indexed_docs = WorkspaceBaseSnapshotIndex::rebuild_with_options(
            &self.build_config,
            &self.overlay_repo_id,
            BaseSnapshotBuildOptions::new(),
        )?
        .indexed_docs;
        self.bootstrap_from_existing_snapshot()?;
        drop(discovery);
        self.refresh_workspace_overlay()?;
        {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        Ok((indexed_docs, self.status_snapshot()))
    }

    fn run_task(&self, task: Arc<RepoTaskRecord>) {
        task.mark_running();

        let task_for_progress = Arc::clone(&task);
        let mut progress = move |event: IndexProgress| {
            task_for_progress.update_progress(event);
            self.emit_task_progress(task_for_progress.snapshot());
        };
        let task_for_cancel = Arc::clone(&task);
        let mut should_cancel = move || {
            task_for_cancel.cancel_requested() || self.shutting_down.load(Ordering::Relaxed)
        };

        // Base publication and its overlay transition share the discovery lock.
        // Readers retain their published view and do not acquire this lock.
        let discovery = (!matches!(task.kind, RepoTaskKind::RefreshWorkspace)).then(|| {
            self.workspace_discovery_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result = match task.kind {
            RepoTaskKind::BuildBaseSnapshot => WorkspaceBaseSnapshotIndex::build_with_options(
                &self.build_config,
                &self.overlay_repo_id,
                BaseSnapshotBuildOptions::new()
                    .with_progress(&mut progress)
                    .with_cancel(&mut should_cancel),
            )
            .map(|result| result.indexed_docs),
            RepoTaskKind::RebuildBaseSnapshot => WorkspaceBaseSnapshotIndex::rebuild_with_options(
                &self.build_config,
                &self.overlay_repo_id,
                BaseSnapshotBuildOptions::new()
                    .with_progress(&mut progress)
                    .with_cancel(&mut should_cancel),
            )
            .map(|result| result.indexed_docs),
            RepoTaskKind::AdvanceBaseSnapshot => {
                progress(IndexProgress::new(
                    crate::progress::IndexProgressPhase::Scanning,
                    "Planning commit-to-commit base delta",
                    0,
                    None,
                ));
                task.target_head.as_deref().ok_or_else(|| AppError::InvalidIndex(
                    "base advance task has no captured target commit".into(),
                )).and_then(|target_head| {
                    let base_catalog = self.base_snapshot_catalog()?.ok_or_else(|| {
                        AppError::InvalidIndex(
                            "base snapshot catalog is unavailable during branch advance".into(),
                        )
                    })?;
                    WorkspaceBaseSnapshotIndex::advance_to_head(
                        &self.build_config,
                        &self.overlay_repo_id,
                        &base_catalog,
                        target_head,
                        self.base_head_cache_entries,
                        &mut should_cancel,
                    )
                    .map(|result| {
                        progress(IndexProgress::new(
                            crate::progress::IndexProgressPhase::Writing,
                            format!(
                                "Advanced {} -> {}: paths={} (add={}, modify={}, delete={}), filtered={}, git_blobs={}, tokenized={}, deleted={}, segments={}/{}, delta_bytes={}, parent_manifest={}, candidate_manifest={}, cache_hit={}",
                                result.from_head,
                                result.to_head,
                                result.enumerated_paths,
                                result.added_paths,
                                result.modified_paths,
                                result.deleted_paths,
                                result.filtered_paths,
                                result.git_blobs_read,
                                result.tokenized_documents,
                                result.deleted_documents,
                                result.live_segments_written,
                                result.delete_segments_written,
                                result.delta_bytes,
                                result.parent_manifest_id.0,
                                result.manifest_id.0,
                                result.cache_hit
                            ),
                            result.tokenized_documents,
                            Some(result.enumerated_paths),
                        ));
                        result.tokenized_documents
                    })
                })
            }
            RepoTaskKind::CompactBaseDeltas => WorkspaceBaseSnapshotIndex::compact_base_deltas(
                &self.build_config,
                self.base_delta_max_segments,
                self.base_delta_max_delete_segments,
                self.base_delta_max_bytes_ratio,
                self.base_head_cache_entries,
                &mut should_cancel,
            ),
            RepoTaskKind::RefreshWorkspace => self
                .refresh_workspace_overlay_with_progress(&mut progress)
                .map(|_| 0),
        };

        let result = result.and_then(|indexed_docs| {
            self.finish_task_success(task.kind, indexed_docs, &mut progress)
                .map(|message| (message, indexed_docs))
        });
        // GC takes the artifact lock, while reconciled queries take that lock
        // before discovery. Release discovery before task-finalization GC.
        drop(discovery);
        match result {
            Ok((message, completed_units)) => {
                task.finish_completed(message);
                self.finish_task_execution(&task, Some(completed_units), None);
            }
            Err(AppError::Cancelled) => {
                task.finish_cancelled("operation cancelled".into());
                self.finish_task_execution(&task, None, None);
            }
            Err(error) => {
                let message = error.to_string();
                task.finish_failed(message.clone());
                self.finish_task_execution(&task, None, Some(Some(message)));
            }
        }
    }

    fn finish_task_success(
        &self,
        kind: RepoTaskKind,
        indexed_docs: usize,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<String> {
        if !matches!(kind, RepoTaskKind::RefreshWorkspace) {
            self.bootstrap_from_existing_snapshot()?;
            self.finish_base_transition_overlay(on_progress)?;
        }
        if matches!(
            kind,
            RepoTaskKind::BuildBaseSnapshot | RepoTaskKind::RebuildBaseSnapshot
        ) {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        let message = match kind {
            RepoTaskKind::BuildBaseSnapshot => {
                format!("built base snapshot with {indexed_docs} documents")
            }
            RepoTaskKind::RebuildBaseSnapshot => {
                format!("rebuilt base snapshot with {indexed_docs} documents")
            }
            RepoTaskKind::AdvanceBaseSnapshot => {
                format!("advanced base snapshot with {indexed_docs} tokenized documents")
            }
            RepoTaskKind::CompactBaseDeltas => {
                format!("compacted base deltas with {indexed_docs} merge tasks")
            }
            RepoTaskKind::RefreshWorkspace => "refreshed workspace".into(),
        };
        Ok(message)
    }

    fn finish_task_execution(
        &self,
        task: &RepoTaskRecord,
        completed_units: Option<usize>,
        last_error: Option<Option<String>>,
    ) {
        let succeeded = matches!(task.snapshot().state, RepoTaskState::Completed);
        // A task that claimed the repo phase must hand it back when it does not
        // succeed. Otherwise the repo keeps a transient phase (e.g. building)
        // with no active task, which no recovery path can leave: the maintenance
        // probe refuses to run outside the ready phases, and the build entry
        // points only fire on `MissingBaseSnapshot`.
        let restore_phase = task.owns_repo_phase && !succeeded;
        let dirty_stats = restore_phase.then(|| self.dirty_catalog_stats());
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active_task_id = None;
        if let Some(last_error) = last_error {
            state.last_error = last_error.clone();
            // Background base maintenance also reports into its own slot. The
            // shared `last_error` is cleared by every successful worktree
            // probe, which happens on a seconds timescale, so a failure left
            // only there is gone before a status poller can see it.
            if matches!(
                task.kind,
                RepoTaskKind::AdvanceBaseSnapshot | RepoTaskKind::CompactBaseDeltas
            ) {
                state.last_maintenance_error = last_error;
            }
        }
        // A maintenance task that got through clears the sticky slot; nothing
        // else does, so the failure survives until the same work succeeds.
        if succeeded
            && matches!(
                task.kind,
                RepoTaskKind::AdvanceBaseSnapshot | RepoTaskKind::CompactBaseDeltas
            )
        {
            state.last_maintenance_error = None;
        }
        if let Some(dirty_stats) = dirty_stats {
            super::status::apply_workspace_dirty_status(&mut state, dirty_stats);
        }
        drop(state);
        self.emit_task_finished(task.snapshot());
        self.emit_workspace_status_changed();
        if matches!(
            task.kind,
            RepoTaskKind::AdvanceBaseSnapshot | RepoTaskKind::CompactBaseDeltas
        ) {
            let _gc_guard = self
                .artifact_gc_lock
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) =
                WorkspaceBaseSnapshotIndex::vacuum_base_artifacts(&self.build_config)
            {
                eprintln!(
                    "flashgrep[base-gc][warn] failed to reclaim base artifacts for {}: {error}",
                    self.build_config.repo_path.display()
                );
            }
        }
        if matches!(task.kind, RepoTaskKind::AdvanceBaseSnapshot) {
            let should_advance_latest = {
                let state = match self.state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.base_head_commit != state.observed_head_commit
                    && state.overlay_head_commit == state.observed_head_commit
            };
            if should_advance_latest {
                self.schedule_base_advance();
            } else if self.status_snapshot().base_compaction_recommended {
                self.schedule_base_compaction();
            }
        } else if task.kind == RepoTaskKind::CompactBaseDeltas
            && completed_units.is_some_and(|units| units > 0)
            && self.status_snapshot().base_compaction_recommended
        {
            self.schedule_base_compaction();
        }
    }

    pub(super) fn ensure_no_active_task(&self) -> Result<()> {
        self.cleanup_finished_task_handle();
        if let Some(task) = self.active_task_status() {
            if matches!(task.state, RepoTaskState::Queued | RepoTaskState::Running) {
                return Err(AppError::Protocol(format!(
                    "repo {} already has a running task: {}",
                    self.repo_id, task.task_id
                )));
            }
        }
        Ok(())
    }

    fn active_task_status(&self) -> Option<RepoTaskStatus> {
        let active_task_id = match self.state.lock() {
            Ok(guard) => guard.active_task_id.clone(),
            Err(poisoned) => poisoned.into_inner().active_task_id.clone(),
        }?;
        self.task_status(&active_task_id)
    }

    pub(super) fn cleanup_finished_task_handle(&self) {
        let maybe_handle = {
            let mut slot = match self.task_handle.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(active) = slot.as_ref() else {
                return;
            };
            if !active.handle.is_finished() {
                return;
            }
            slot.take()
        };
        if let Some(active) = maybe_handle {
            let _ = active.handle.join();
        }
    }

    pub(super) fn has_active_task(&self) -> bool {
        match self.state.lock() {
            Ok(guard) => guard.active_task_id.is_some(),
            Err(poisoned) => poisoned.into_inner().active_task_id.is_some(),
        }
    }

    pub(super) fn mark_task_started(
        &self,
        kind: RepoTaskKind,
        active_task_id: Option<String>,
        update_repo_phase: bool,
    ) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active_task_id = active_task_id;
        state.last_error = None;
        if update_repo_phase {
            state.phase = match kind {
                RepoTaskKind::BuildBaseSnapshot => RepoPhase::BuildingBaseSnapshot,
                RepoTaskKind::RebuildBaseSnapshot => RepoPhase::RebuildingBaseSnapshot,
                RepoTaskKind::AdvanceBaseSnapshot | RepoTaskKind::CompactBaseDeltas => {
                    RepoPhase::ReadyDirty
                }
                RepoTaskKind::RefreshWorkspace => RepoPhase::Opening,
            };
        }
        drop(state);
        self.emit_workspace_status_changed();
    }

    pub(super) fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        {
            let (lock, condvar) = &*self.maintenance_worker;
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.shutdown = true;
            condvar.notify_all();
        }
        if let Some(task_id) = match self.state.lock() {
            Ok(guard) => guard.active_task_id.clone(),
            Err(poisoned) => poisoned.into_inner().active_task_id.clone(),
        } {
            let _ = self.cancel_task(&task_id);
        }
        if let Some(active) = match self.task_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = active.handle.join();
        }
        if let Some(handle) = match self.watcher_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = handle.join();
        }
        if let Some(handle) = match self.maintenance_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = handle.join();
        }
        if let Some(handle) = self
            .persistence_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
        if let Err(error) = self.checkpoint_workspace_overlay_for_shutdown() {
            eprintln!(
                "flashgrep[overlay][warn] failed to checkpoint workspace overlay during shutdown for {}: {error}",
                self.build_config.repo_path.display()
            );
        }
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn queued_advance_keeps_its_target_and_caches_a_superseded_candidate() {
        let repo = crate::test_support::TestRepo::new();
        repo.write("tracked.rs", "H0_TOKEN\n");
        repo.init_git();
        repo.commit_all("h0");
        let runtime = RepoRuntime::new(
            "repo".into(),
            repo.build_config(),
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        runtime.build_base_snapshot().unwrap();
        let h0 = runtime.current_git_head().unwrap();
        repo.write("tracked.rs", "H1_TOKEN\n");
        repo.commit_all("h1");
        let h1 = runtime.current_git_head().unwrap().unwrap();
        let task_id = "queued-h1".to_string();
        let task = Arc::new(RepoTaskRecord::new(
            task_id.clone(),
            "repo".into(),
            RepoTaskKind::AdvanceBaseSnapshot,
            false,
            Some(h1.clone()),
        ));
        runtime
            .tasks
            .lock()
            .unwrap()
            .insert(task_id.clone(), task.clone());
        runtime.mark_task_started(
            RepoTaskKind::AdvanceBaseSnapshot,
            Some(task_id.clone()),
            false,
        );
        // Reproduce the queue delay deterministically, before starting the worker.
        repo.write("tracked.rs", "H2_TOKEN\n");
        repo.commit_all("h2");
        runtime.observe_git_head_transition().unwrap();
        let status = runtime.status_snapshot();
        assert_ne!(status.workspace_head_commit.as_deref(), Some(h1.as_str()));
        assert_eq!(
            status.base_advance_target_head.as_deref(),
            Some(h1.as_str())
        );
        runtime.run_task(task.clone());
        assert_eq!(task.snapshot().state, RepoTaskState::Cancelled);
        assert_eq!(runtime.state.lock().unwrap().base_head_commit, h0);
        let cache = std::fs::read_to_string(
            runtime
                .build_config
                .storage_root
                .join("base-snapshot/HEAD_MANIFESTS.json"),
        )
        .unwrap();
        assert!(
            cache.contains(&h1),
            "the completed H1 candidate must remain cached"
        );
        assert!(runtime.status_snapshot().base_advance_target_head.is_none());
    }
    #[test]
    fn base_task_waits_for_inflight_workspace_discovery() {
        let repo = crate::test_support::TestRepo::new();
        repo.write("tracked.rs", "H0_TOKEN\n");
        repo.init_git();
        repo.commit_all("h0");
        let runtime = RepoRuntime::new(
            "repo".into(),
            repo.build_config(),
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        runtime.build_base_snapshot().unwrap();
        let original = runtime.current_base_publication_id().unwrap();
        repo.write("tracked.rs", "H1_TOKEN\n");
        repo.commit_all("h1");
        let task = Arc::new(RepoTaskRecord::new(
            "advance".into(),
            "repo".into(),
            RepoTaskKind::AdvanceBaseSnapshot,
            false,
            runtime.current_git_head().unwrap(),
        ));
        let discovery = runtime.workspace_discovery_lock.lock().unwrap();
        std::thread::scope(|scope| {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let runtime_ref = &runtime;
            let task_ref = &task;
            scope.spawn(move || {
                started_tx.send(()).unwrap();
                runtime_ref.run_task(task_ref.clone());
                done_tx.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(runtime.current_base_publication_id().unwrap(), original);
            let plan = runtime
                .plan_overlay_update_from_incremental_probe()
                .unwrap()
                .unwrap();
            runtime.apply_overlay_write_plan(plan).unwrap();
            drop(discovery);
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        assert_eq!(task.snapshot().state, RepoTaskState::Completed);
        assert_ne!(runtime.current_base_publication_id().unwrap(), original);
    }
}
