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
        let handle = thread::spawn(move || runtime.run_maintenance_worker(worker_state));
        let mut slot = match self.maintenance_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(handle);
    }

    fn run_maintenance_worker(
        self: Arc<Self>,
        worker_state: Arc<(Mutex<MaintenanceWorkerState>, Condvar)>,
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
                    if let Some(reasons) = take_ready_maintenance_reasons(&mut state) {
                        break Some(reasons);
                    }
                    let Some(deadline) = next_maintenance_deadline(&state) else {
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
        if reasons & MAINTENANCE_REASON_OVERLAY_MERGE != 0 {
            self.run_overlay_merge_maintenance();
        }
        if reasons & MAINTENANCE_REASON_WORKSPACE_POLL != 0 {
            self.run_workspace_probe_maintenance();
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
        let event_driven_probe = !stranded && self.has_pending_workspace_events();
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
        }
        self.schedule_workspace_probe(if stranded {
            STRANDED_RECOVERY_PROBE_INTERVAL
        } else if event_driven_probe {
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
        condvar.notify_one();
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
        condvar.notify_one();
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
        condvar.notify_one();
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

fn take_ready_maintenance_reasons(state: &mut MaintenanceWorkerState) -> Option<u32> {
    let mut reasons = state.requested_reasons
        & !(MAINTENANCE_REASON_OVERLAY_MERGE | MAINTENANCE_REASON_WORKSPACE_POLL);
    let overlay_merge_ready = state.requested_reasons & MAINTENANCE_REASON_OVERLAY_MERGE != 0
        && state
            .overlay_merge_due_at
            .is_none_or(|due_at| Instant::now() >= due_at);
    if overlay_merge_ready {
        reasons |= MAINTENANCE_REASON_OVERLAY_MERGE;
        state.overlay_merge_due_at = None;
    }
    let workspace_probe_ready = state.requested_reasons & MAINTENANCE_REASON_WORKSPACE_POLL != 0
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

fn next_maintenance_deadline(state: &MaintenanceWorkerState) -> Option<Instant> {
    let overlay_deadline = if state.requested_reasons & MAINTENANCE_REASON_OVERLAY_MERGE != 0 {
        state.overlay_merge_due_at
    } else {
        None
    };
    let workspace_deadline = if state.requested_reasons & MAINTENANCE_REASON_WORKSPACE_POLL != 0 {
        state.workspace_probe_due_at
    } else {
        None
    };
    [overlay_deadline, workspace_deadline]
        .into_iter()
        .flatten()
        .min()
}
