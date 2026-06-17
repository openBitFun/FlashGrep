use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{fs, io};

use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::files::is_workspace_internal_path;

use super::RepoRuntime;

#[cfg(target_os = "linux")]
const LINUX_INOTIFY_WATCH_BUDGET_PERCENT: u64 = 75;
const WATCH_EVENT_CANDIDATE_LIMIT: usize = 512;

#[derive(Debug, Default)]
pub(super) struct WorkspaceEventBuffer {
    pub(super) observed: bool,
    pub(super) full_probe_required: bool,
    pub(super) candidate_paths: HashSet<PathBuf>,
}

impl WorkspaceEventBuffer {
    pub(super) fn use_fast_path(&self) -> bool {
        self.observed && !self.full_probe_required && !self.candidate_paths.is_empty()
    }
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
        buffer.observed
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
        if let Some(reason) = linux_workspace_watcher_skip_reason(&repo_path) {
            eprintln!(
                "flashgrep[watch][warn] workspace watcher disabled for {}: {reason}; using polling fallback",
                repo_path.display()
            );
            self.workspace_watcher_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let mut watcher = match notify::recommended_watcher(move |result| {
            let _ = tx.send(result);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!(
                    "flashgrep[watch][warn] failed to start workspace watcher for {}: {error}",
                    repo_path.display()
                );
                self.workspace_watcher_active
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        if let Err(error) = watcher.watch(&repo_path, RecursiveMode::Recursive) {
            eprintln!(
                "flashgrep[watch][warn] failed to watch workspace {}: {error}",
                repo_path.display()
            );
            self.workspace_watcher_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        self.workspace_watcher_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "flashgrep[watch][info] workspace watcher active for {}",
            repo_path.display()
        );

        while !self
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ok(event)) => self.handle_workspace_notify_event(event),
                Ok(Err(error)) => {
                    eprintln!(
                        "flashgrep[watch][warn] workspace watcher error for {}: {error}",
                        self.build_config.repo_path.display()
                    );
                    self.enqueue_full_workspace_probe();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        self.workspace_watcher_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "flashgrep[watch][info] workspace watcher stopped for {}",
            self.build_config.repo_path.display()
        );
    }

    fn handle_workspace_notify_event(&self, event: Event) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }

        let mut root_event_seen = false;
        let mut full_probe_required = false;
        let mut paths = Vec::new();
        for path in event.paths {
            if path == self.build_config.repo_path {
                root_event_seen = true;
                continue;
            }
            if let Some(path) = self.normalize_workspace_watch_path(&path) {
                if self.watch_path_requires_full_probe(&path) {
                    full_probe_required = true;
                } else {
                    paths.push(path);
                }
            }
        }
        if root_event_seen && paths.is_empty() {
            full_probe_required = true;
        }
        if paths.len() > WATCH_EVENT_CANDIDATE_LIMIT {
            paths.clear();
            full_probe_required = true;
        }
        if !full_probe_required && paths.is_empty() {
            return;
        }
        self.enqueue_workspace_events(paths, full_probe_required);
    }

    fn enqueue_full_workspace_probe(&self) {
        self.enqueue_workspace_events(Vec::new(), true);
    }

    fn enqueue_workspace_events(&self, paths: Vec<PathBuf>, full_probe_required: bool) {
        let mut buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let changed = !buffer.observed;
        buffer.observed = true;
        if full_probe_required {
            buffer.full_probe_required = true;
            buffer.candidate_paths.clear();
        } else if !buffer.full_probe_required {
            for path in paths {
                buffer.candidate_paths.insert(path);
                if buffer.candidate_paths.len() > WATCH_EVENT_CANDIDATE_LIMIT {
                    buffer.full_probe_required = true;
                    buffer.candidate_paths.clear();
                    break;
                }
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

    fn watch_path_requires_full_probe(&self, path: &Path) -> bool {
        if path
            .strip_prefix(&self.build_config.repo_path)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|component| matches!(component, Component::Normal(name) if name == ".git"))
        {
            return true;
        }
        if path.file_name().is_some_and(|name| {
            matches!(
                name.to_string_lossy().as_ref(),
                ".gitignore" | ".gitmodules" | ".gitattributes" | "HEAD" | "config" | "packed-refs"
            )
        }) {
            return true;
        }
        false
    }
}

#[cfg(target_os = "linux")]
fn linux_workspace_watcher_skip_reason(repo_path: &Path) -> Option<String> {
    let max_user_watches = linux_max_user_watches()?;
    let watch_budget = max_user_watches
        .saturating_mul(LINUX_INOTIFY_WATCH_BUDGET_PERCENT)
        .saturating_div(100)
        .max(1);
    let directory_count = count_directories_until(repo_path, watch_budget.saturating_add(1));
    if directory_count <= watch_budget {
        return None;
    }
    Some(format!(
        "workspace has at least {directory_count} directories, current Linux inotify budget is {watch_budget} of max_user_watches={max_user_watches}"
    ))
}

#[cfg(not(target_os = "linux"))]
fn linux_workspace_watcher_skip_reason(_repo_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn linux_max_user_watches() -> Option<u64> {
    fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn count_directories_until(root: &Path, limit: u64) -> u64 {
    let mut count = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        count = count.saturating_add(1);
        if count >= limit {
            break;
        }
        push_child_directories(&mut stack, &path);
    }
    count
}

#[cfg(target_os = "linux")]
fn push_child_directories(stack: &mut Vec<PathBuf>, path: &Path) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            stack.push(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn count_directories_stops_at_limit() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        std::fs::create_dir_all(temp.path().join("a/b/c")).expect("dirs should be created");
        std::fs::create_dir_all(temp.path().join("d/e")).expect("dirs should be created");

        assert_eq!(super::count_directories_until(temp.path(), 3), 3);
        assert_eq!(super::count_directories_until(temp.path(), 32), 6);
    }
}
