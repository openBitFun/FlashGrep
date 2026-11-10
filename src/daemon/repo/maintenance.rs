use std::{
    sync::{Arc, Condvar, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

use std::sync::atomic::Ordering;

use super::{RepoPhase, RepoRuntime, WorkspaceOverlayMergeEvent};

#[derive(Debug, Default)]
pub(super) struct MaintenanceWorkerState {
    pub(super) requested_reasons: u32,
    pub(super) overlay_merge_due_at: Option<Instant>,
    pub(super) workspace_probe_due_at: Option<Instant>,
    // Separate from event wakeups: a drained event timer must not start a full walk.
    pub(super) full_reconcile_due_at: Option<Instant>,
    pub(super) shutdown: bool,
}

const MAINTENANCE_REASON_OVERLAY_MERGE: u32 = 1 << 0;
const MAINTENANCE_REASON_WORKSPACE_POLL: u32 = 1 << 1;
pub(super) const GIT_WORKSPACE_EVENT_RETRY_DELAY: Duration = Duration::from_millis(50);
const GIT_WORKSPACE_RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
const GIT_WORKSPACE_POLL_FALLBACK_INTERVAL: Duration = Duration::from_millis(100);
/// Retry cadence for probing a repo stranded in a transient or degraded phase.
/// Deliberately slower than the regular poll so a repo that cannot recover
/// retries steadily instead of spinning.
const STRANDED_RECOVERY_PROBE_INTERVAL: Duration = Duration::from_secs(5);

impl RepoRuntime {
    pub(super) fn start_maintenance_worker(self: &Arc<Self>) {
        let worker_state = Arc::clone(&self.maintenance_worker);
        let runtime = Arc::clone(self);
        let handle = thread::spawn(move || {
            runtime.run_maintenance_worker(worker_state, MAINTENANCE_REASON_WORKSPACE_POLL)
        });
        let mut slot = match self.maintenance_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(handle);
        let worker_state = Arc::clone(&self.maintenance_worker);
        let runtime = Arc::clone(self);
        *self
            .persistence_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thread::spawn(move || {
            runtime.run_maintenance_worker(worker_state, MAINTENANCE_REASON_OVERLAY_MERGE)
        }));
    }

    fn run_maintenance_worker(
        self: Arc<Self>,
        worker_state: Arc<(Mutex<MaintenanceWorkerState>, Condvar)>,
        mask: u32,
    ) {
        loop {
            let reasons = {
                let (lock, condvar) = &*worker_state;
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if state.shutdown {
                        break None;
                    }
                    if let Some(reasons) = take_ready_maintenance_reasons_masked(&mut state, mask) {
                        break Some(reasons);
                    }
                    let Some(deadline) = next_maintenance_deadline_masked(&state, mask) else {
                        state = condvar
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        continue;
                    };
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    let (next_state, _) = condvar
                        .wait_timeout(state, timeout)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = next_state;
                }
            };
            let Some(reasons) = reasons else {
                break;
            };
            self.run_maintenance_pass(reasons);
        }
    }

    fn run_maintenance_pass(&self, reasons: u32) {
        // Discovery must not wait behind disk encoding/checkpoint work.
        if reasons & MAINTENANCE_REASON_WORKSPACE_POLL != 0 {
            self.run_workspace_probe_maintenance();
        }
        if reasons & MAINTENANCE_REASON_OVERLAY_MERGE != 0 {
            self.run_overlay_merge_maintenance();
        }
    }

    fn run_overlay_merge_maintenance(&self) {
        let overlay = match self.workspace_overlay_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
        .as_ref()
        .cloned();
        let Some(overlay) = overlay else {
            return;
        };
        match overlay.run_requested_merge() {
            Ok(_) => {}
            Err(error) => {
                overlay.retry_failed_merge();
                self.schedule_overlay_merge_retry();
                eprintln!(
                    "flashgrep[maintenance][warn] failed to run workspace overlay maintenance for {}: {error}",
                    self.build_config.repo_path.display()
                );
            }
        }
    }

    fn run_workspace_probe_maintenance(&self) {
        let probe_interval = self.workspace_probe_interval();
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }

        let state = match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let ready_for_probe = matches!(state.phase, RepoPhase::ReadyClean | RepoPhase::ReadyDirty)
            && state.snapshot_key.is_some();
        // A repo sitting in a transient or degraded phase with nothing running
        // behind it is stranded: every other recovery path is gated on the very
        // phase it is stuck in, and only a probe re-derives the phase from what
        // is actually on disk. Probe it on a slow cadence so a repo that keeps
        // failing to recover does not spin.
        let stranded = !ready_for_probe
            && state.active_task_id.is_none()
            && matches!(
                state.phase,
                RepoPhase::BuildingBaseSnapshot
                    | RepoPhase::RebuildingBaseSnapshot
                    | RepoPhase::Degraded
            );

        if !ready_for_probe && !stranded {
            self.schedule_workspace_probe(probe_interval);
            return;
        }

        if self.has_active_task() {
            let retry_delay = if self.has_pending_workspace_events() {
                GIT_WORKSPACE_EVENT_RETRY_DELAY
            } else {
                probe_interval
            };
            self.schedule_workspace_probe(retry_delay);
            return;
        }

        // Recovering a stranded repo needs the authoritative full probe; the
        // event fast path only reconciles paths it was told about.
        let full_due = self
            .maintenance_worker
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .full_reconcile_due_at;
        // Continuous events must not postpone periodic recovery indefinitely.
        let full_overdue = full_due.is_some_and(|due| due <= Instant::now());
        let event_driven_probe = !stranded && !full_overdue && self.has_pending_workspace_events();
        if !event_driven_probe
            && !stranded
            && state.probe_healthy
            && !self.workspace_probe_pending.load(Ordering::Acquire)
        {
            if let Some(remaining) =
                full_due.and_then(|due| due.checked_duration_since(Instant::now()))
            {
                self.schedule_workspace_probe(remaining.min(probe_interval));
                return;
            }
        }
        let probe_result = if event_driven_probe {
            self.probe_workspace_overlay_from_events()
        } else {
            self.probe_workspace_overlay()
        };

        if let Err(error) = probe_result {
            self.set_probe_healthy(false);
            self.set_last_error(Some(format!("background workspace poll failed: {error}")));
        } else {
            self.set_probe_healthy(true);
            if !event_driven_probe {
                self.maintenance_worker
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .full_reconcile_due_at = Some(Instant::now() + probe_interval);
            }
        }
        self.schedule_workspace_probe(if stranded {
            STRANDED_RECOVERY_PROBE_INTERVAL
        } else if self.has_pending_workspace_events() {
            GIT_WORKSPACE_EVENT_RETRY_DELAY
        } else {
            probe_interval
        });
    }

    pub(super) fn schedule_maintenance(&self, reasons: u32) {
        let (lock, condvar) = &*self.maintenance_worker;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested_reasons |= reasons;
        if reasons & MAINTENANCE_REASON_OVERLAY_MERGE != 0 {
            let due_at = Instant::now() + self.overlay_merge_min_delay;
            state.overlay_merge_due_at = Some(
                state
                    .overlay_merge_due_at
                    .map_or(due_at, |existing| existing.min(due_at)),
            );
        }
        condvar.notify_all();
    }

    pub(super) fn schedule_workspace_probe(&self, delay: Duration) {
        let (lock, condvar) = &*self.maintenance_worker;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested_reasons |= MAINTENANCE_REASON_WORKSPACE_POLL;
        let due_at = Instant::now() + delay;
        state.workspace_probe_due_at = Some(
            state
                .workspace_probe_due_at
                .map_or(due_at, |existing| existing.min(due_at)),
        );
        condvar.notify_all();
    }

    pub(super) fn workspace_probe_interval(&self) -> Duration {
        if self.workspace_watcher_active() {
            GIT_WORKSPACE_RECONCILE_INTERVAL
        } else {
            GIT_WORKSPACE_POLL_FALLBACK_INTERVAL
        }
    }

    fn schedule_overlay_merge_retry(&self) {
        let (lock, condvar) = &*self.maintenance_worker;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested_reasons |= MAINTENANCE_REASON_OVERLAY_MERGE;
        state.overlay_merge_due_at = Some(Instant::now() + self.overlay_merge_retry_delay);
        condvar.notify_all();
    }

    pub(super) fn workspace_overlay_merge_state_hook(
        &self,
    ) -> Arc<dyn Fn(WorkspaceOverlayMergeEvent) + Send + Sync> {
        let runtime = self
            .self_handle
            .get()
            .cloned()
            .unwrap_or_else(Weak::<RepoRuntime>::new);
        Arc::new(move |event| {
            if let Some(runtime) = runtime.upgrade() {
                if matches!(event, WorkspaceOverlayMergeEvent::Requested) {
                    runtime.schedule_maintenance(MAINTENANCE_REASON_OVERLAY_MERGE);
                }
                runtime.emit_workspace_status_changed();
            }
        })
    }
}

fn take_ready_maintenance_reasons_masked(
    state: &mut MaintenanceWorkerState,
    mask: u32,
) -> Option<u32> {
    let mut reasons = (state.requested_reasons & mask)
        & !(MAINTENANCE_REASON_OVERLAY_MERGE | MAINTENANCE_REASON_WORKSPACE_POLL);
    let overlay_merge_ready = state.requested_reasons & mask & MAINTENANCE_REASON_OVERLAY_MERGE
        != 0
        && state
            .overlay_merge_due_at
            .is_none_or(|due_at| Instant::now() >= due_at);
    if overlay_merge_ready {
        reasons |= MAINTENANCE_REASON_OVERLAY_MERGE;
        state.overlay_merge_due_at = None;
    }
    let workspace_probe_ready = state.requested_reasons & mask & MAINTENANCE_REASON_WORKSPACE_POLL
        != 0
        && state
            .workspace_probe_due_at
            .is_none_or(|due_at| Instant::now() >= due_at);
    if workspace_probe_ready {
        reasons |= MAINTENANCE_REASON_WORKSPACE_POLL;
        state.workspace_probe_due_at = None;
    }
    if reasons == 0 {
        return None;
    }
    state.requested_reasons &= !reasons;
    Some(reasons)
}

fn next_maintenance_deadline_masked(state: &MaintenanceWorkerState, mask: u32) -> Option<Instant> {
    let overlay_deadline = if state.requested_reasons & mask & MAINTENANCE_REASON_OVERLAY_MERGE != 0
    {
        state.overlay_merge_due_at
    } else {
        None
    };
    let workspace_deadline =
        if state.requested_reasons & mask & MAINTENANCE_REASON_WORKSPACE_POLL != 0 {
            state.workspace_probe_due_at
        } else {
            None
        };
    [overlay_deadline, workspace_deadline]
        .into_iter()
        .flatten()
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn drained_event_wakeup_does_not_scan_but_expired_reconciliation_does() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let tracked = root.join("base.rs");
        fs::write(&tracked, "BASE\n").unwrap();
        let runtime = RepoRuntime::new(
            "repo".into(),
            crate::config::BuildConfig {
                repo_path: root.clone(),
                storage_root: temp.path().join("index"),
                git_mode: crate::config::GitMode::Filesystem,
                ..Default::default()
            },
            Default::default(),
            Arc::new(|_| {}),
        )
        .unwrap();
        runtime.build_base_snapshot().unwrap();
        // Drive the real worker synchronously; notifications are admitted explicitly.
        runtime
            .workspace_watcher_active
            .store(true, Ordering::Relaxed);
        runtime.run_workspace_probe_maintenance();
        let periodic_due = runtime
            .maintenance_worker
            .0
            .lock()
            .unwrap()
            .full_reconcile_due_at
            .unwrap();
        let admit = || {
            let mut events = runtime.workspace_event_buffer.lock().unwrap();
            events.observed = true;
            events.revision = runtime.observe_workspace_revision();
            events.candidate_paths.insert(tracked.clone());
        };
        fs::write(&tracked, "CHANGED\n").unwrap();
        admit();
        runtime.run_workspace_probe_maintenance();
        assert!(!runtime.has_pending_workspace_events());
        assert_eq!(runtime.dirty_catalog_visible_paths().modified.len(), 1);
        assert_eq!(
            runtime
                .maintenance_worker
                .0
                .lock()
                .unwrap()
                .full_reconcile_due_at,
            Some(periodic_due)
        );

        // A file without a notification exposes an unwanted full scan deterministically.
        fs::write(root.join("unobserved.rs"), "UNOBSERVED\n").unwrap();
        let progress = runtime.workspace_progress_snapshot();
        runtime.run_workspace_probe_maintenance();
        assert_eq!(
            runtime.workspace_progress_snapshot().reconciled_revision,
            progress.reconciled_revision
        );
        assert!(runtime.dirty_catalog_visible_paths().new.is_empty());

        // An explicit recovery request must bypass the periodic deadline too.
        runtime.mark_workspace_probe_pending();
        runtime.run_workspace_probe_maintenance();
        assert_eq!(runtime.dirty_catalog_visible_paths().new.len(), 1);
        fs::write(root.join("second-unobserved.rs"), "SECOND_UNOBSERVED\n").unwrap();

        // The deadline wins even if a continuous stream supplies another scoped event.
        runtime
            .maintenance_worker
            .0
            .lock()
            .unwrap()
            .full_reconcile_due_at = Some(Instant::now());
        admit();
        runtime.run_workspace_probe_maintenance();
        assert_eq!(runtime.dirty_catalog_visible_paths().new.len(), 2);
        assert!(
            runtime.workspace_progress_snapshot().reconciled_revision
                > progress.reconciled_revision
        );
    }
}

#[cfg(test)]
mod worker_isolation_tests {
    use super::*;
    #[test]
    fn persistence_worker_does_not_consume_discovery_wakeup_or_deadline() {
        let now = Instant::now();
        let mut state = MaintenanceWorkerState {
            requested_reasons: MAINTENANCE_REASON_OVERLAY_MERGE | MAINTENANCE_REASON_WORKSPACE_POLL,
            overlay_merge_due_at: Some(now),
            workspace_probe_due_at: Some(now),
            ..Default::default()
        };
        assert_eq!(
            take_ready_maintenance_reasons_masked(&mut state, MAINTENANCE_REASON_OVERLAY_MERGE),
            Some(MAINTENANCE_REASON_OVERLAY_MERGE)
        );
        assert_eq!(
            next_maintenance_deadline_masked(&state, MAINTENANCE_REASON_OVERLAY_MERGE),
            None
        );
        assert_eq!(
            next_maintenance_deadline_masked(&state, MAINTENANCE_REASON_WORKSPACE_POLL),
            Some(now)
        );
        assert_eq!(
            take_ready_maintenance_reasons_masked(&mut state, MAINTENANCE_REASON_WORKSPACE_POLL),
            Some(MAINTENANCE_REASON_WORKSPACE_POLL)
        );
    }
}
