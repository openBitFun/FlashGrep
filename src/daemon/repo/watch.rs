use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::files::is_workspace_internal_path;

use super::RepoRuntime;

#[derive(Debug, Default)]
pub(super) struct WorkspaceEventBuffer {
    pub(super) paths: HashSet<PathBuf>,
    pub(super) force_full_rescan: bool,
}

impl RepoRuntime {
    pub(super) fn start_workspace_watcher(self: &Arc<Self>) {
        let runtime = Arc::clone(self);
        let handle = thread::spawn(move || runtime.run_workspace_watcher());
        let mut slot = match self.watcher_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(handle);
    }

    pub(super) fn workspace_watcher_active(&self) -> bool {
        self.workspace_watcher_active
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(super) fn has_pending_workspace_events(&self) -> bool {
        let buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buffer.force_full_rescan || !buffer.paths.is_empty()
    }

    pub(super) fn take_workspace_events(&self) -> WorkspaceEventBuffer {
        let mut buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut *buffer)
    }

    fn run_workspace_watcher(self: Arc<Self>) {
        let (tx, rx) = mpsc::channel();
        let repo_path = self.build_config.repo_path.clone();
        let mut watcher = match notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!(
                    "flashgrep: failed to start workspace watcher for {}: {error}",
                    repo_path.display()
                );
                self.workspace_watcher_active
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        if let Err(error) = watcher.watch(&repo_path, RecursiveMode::Recursive) {
            eprintln!(
                "flashgrep: failed to watch workspace {}: {error}",
                repo_path.display()
            );
            self.workspace_watcher_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        self.workspace_watcher_active
            .store(true, std::sync::atomic::Ordering::Relaxed);

        while !self
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(event)) => self.handle_workspace_notify_event(event),
                Ok(Err(error)) => {
                    eprintln!(
                        "flashgrep: workspace watcher error for {}: {error}",
                        self.build_config.repo_path.display()
                    );
                    self.enqueue_workspace_events(Vec::new(), true);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        self.workspace_watcher_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn handle_workspace_notify_event(&self, event: Event) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }

        let mut saw_repo_root = false;
        let mut paths = Vec::new();
        for path in event.paths {
            if path == self.build_config.repo_path {
                saw_repo_root = true;
                continue;
            }
            if let Some(path) = self.normalize_workspace_watch_path(&path) {
                paths.push(path);
            }
        }
        let mut force_full_rescan = saw_repo_root && paths.is_empty();
        if paths.len() > 4_096 {
            force_full_rescan = true;
            paths.clear();
        }
        if !force_full_rescan && paths.is_empty() {
            return;
        }
        self.enqueue_workspace_events(paths, force_full_rescan);
    }

    fn enqueue_workspace_events(&self, paths: Vec<PathBuf>, force_full_rescan: bool) {
        let mut buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut changed = false;
        if force_full_rescan && !buffer.force_full_rescan {
            buffer.force_full_rescan = true;
            buffer.paths.clear();
            changed = true;
        }
        if !buffer.force_full_rescan {
            for path in paths {
                changed |= buffer.paths.insert(path);
            }
        }
        drop(buffer);
        if !changed {
            return;
        }
        self.workspace_probe_pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.invalidate_glob_cache();
        self.schedule_workspace_probe(Duration::from_millis(0));
        self.emit_workspace_status_changed();
    }

    fn normalize_workspace_watch_path(&self, path: &Path) -> Option<PathBuf> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.build_config.repo_path.join(path)
        };
        if !path.starts_with(&self.build_config.repo_path) {
            return None;
        }
        if path.starts_with(&self.build_config.storage_root) || is_workspace_internal_path(&path) {
            return None;
        }
        Some(path)
    }
}
