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
const WATCH_EVENT_CANDIDATE_LIMIT: usize = 16_384;
const WATCH_EVENT_QUEUE_CAPACITY: usize = 16_384;
const WATCH_EVENT_PATH_BYTES: usize = 8 * 1024 * 1024;

// Keep only data the worker needs; notify attributes may contain arbitrary strings.
struct QueuedWorkspaceEvent {
    kind: EventKind,
    paths: Vec<PathBuf>,
    bytes: usize,
}

fn reserve_event_bytes(bytes: &std::sync::atomic::AtomicUsize, requested: usize) -> bool {
    bytes
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |current| {
                current
                    .checked_add(requested)
                    .filter(|total| *total <= WATCH_EVENT_PATH_BYTES)
            },
        )
        .is_ok()
}

#[derive(Debug, Default)]
pub(super) struct WorkspaceEventBuffer {
    pub(super) observed: bool,
    pub(super) revision: u64,
    pub(super) full_probe_required: bool,
    pub(super) candidate_paths: HashSet<PathBuf>,
    candidate_bytes: usize,
}

impl WorkspaceEventBuffer {
    pub(super) fn use_fast_path(&self) -> bool {
        self.observed && !self.full_probe_required && !self.candidate_paths.is_empty()
    }
}

impl RepoRuntime {
    pub(super) fn start_workspace_watcher(self: &Arc<Self>) {
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let runtime = Arc::clone(self);
        let handle = thread::spawn(move || runtime.run_workspace_watcher(ready_tx));
        let mut slot = match self.watcher_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(handle);
        let _ = ready_rx.recv_timeout(Duration::from_secs(1));
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

    /// Synchronize completion with event admission so a notification arriving
    /// during publication remains pending after the older batch completes.
    pub(super) fn complete_workspace_event_probe(&self) {
        let buffer = self
            .workspace_event_buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.workspace_probe_pending
            .store(buffer.observed, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn take_workspace_events(&self) -> WorkspaceEventBuffer {
        let mut buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut *buffer)
    }

    fn run_workspace_watcher(self: Arc<Self>, ready: mpsc::SyncSender<()>) {
        let (tx, rx) = mpsc::sync_channel(WATCH_EVENT_QUEUE_CAPACITY);
        let overflow = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflow);
        let queued_bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_bytes = Arc::clone(&queued_bytes);
        let repo_path = self.build_config.repo_path.clone();
        if let Some(reason) = linux_workspace_watcher_skip_reason(&repo_path) {
            eprintln!(
                "flashgrep[watch][warn] workspace watcher disabled for {}: {reason}; using polling fallback",
                repo_path.display()
            );
            self.workspace_watcher_active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = ready.send(());
            return;
        }
        let mut watcher = match notify::recommended_watcher(move |result: notify::Result<Event>| {
            let event = match result {
                Ok(event) if !event.need_rescan() => event,
                _ => {
                    callback_overflow.store(true, std::sync::atomic::Ordering::Release);
                    return;
                }
            };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let bytes = event.paths.iter().fold(
                event
                    .paths
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PathBuf>()),
                |total, path| total.saturating_add(path.capacity()),
            );
            if !reserve_event_bytes(&callback_bytes, bytes) {
                callback_overflow.store(true, std::sync::atomic::Ordering::Release);
                return;
            }
            let queued = QueuedWorkspaceEvent {
                kind: event.kind,
                paths: event.paths,
                bytes,
            };
            if let Err(error) = tx.try_send(queued) {
                callback_bytes.fetch_sub(bytes, std::sync::atomic::Ordering::AcqRel);
                if matches!(error, mpsc::TrySendError::Full(_)) {
                    callback_overflow.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!(
                    "flashgrep[watch][warn] failed to start workspace watcher for {}: {error}",
                    repo_path.display()
                );
                self.workspace_watcher_active
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = ready.send(());
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
            let _ = ready.send(());
            return;
        }
        if self.build_config.git_mode == crate::config::GitMode::Auto {
            if let Some(index) = super::membership::git_index_path(&repo_path) {
                if let Some(directory) = index.parent().filter(|p| !p.starts_with(&repo_path)) {
                    if let Err(error) = watcher.watch(directory, RecursiveMode::NonRecursive) {
                        eprintln!("flashgrep[watch][warn] Git metadata watch unavailable for {}: {error}; periodic reconciliation remains enabled", directory.display());
                    }
                }
            }
        }
        self.workspace_watcher_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = ready.send(());
        eprintln!(
            "flashgrep[watch][info] workspace watcher active for {}",
            repo_path.display()
        );

        while !self
            .shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            if overflow.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.enqueue_full_workspace_probe();
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(event) => {
                    queued_bytes.fetch_sub(event.bytes, std::sync::atomic::Ordering::AcqRel);
                    self.handle_workspace_notify_event(Event {
                        kind: event.kind,
                        paths: event.paths,
                        attrs: Default::default(),
                    });
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
        if event.need_rescan() {
            self.enqueue_full_workspace_probe();
            return;
        }
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

    pub(super) fn enqueue_full_workspace_probe(&self) {
        self.enqueue_workspace_events(Vec::new(), true);
    }

    fn enqueue_workspace_events(&self, paths: Vec<PathBuf>, full_probe_required: bool) {
        let mut buffer = match self.workspace_event_buffer.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buffer.revision = self.observe_workspace_revision();
        buffer.observed = true;
        if full_probe_required {
            buffer.full_probe_required = true;
            buffer.candidate_paths.clear();
            buffer.candidate_bytes = 0;
        } else if !buffer.full_probe_required {
            for path in paths {
                if buffer.candidate_paths.contains(&path) {
                    continue;
                }
                buffer.candidate_bytes = buffer.candidate_bytes.saturating_add(path.capacity());
                buffer.candidate_paths.insert(path);
                if buffer.candidate_paths.len() > WATCH_EVENT_CANDIDATE_LIMIT
                    || buffer.candidate_bytes > WATCH_EVENT_PATH_BYTES
                {
                    buffer.full_probe_required = true;
                    buffer.candidate_paths.clear();
                    buffer.candidate_bytes = 0;
                    break;
                }
            }
        }
        drop(buffer);
        self.workspace_probe_pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.invalidate_glob_cache();
        self.schedule_workspace_probe(super::maintenance::GIT_WORKSPACE_EVENT_RETRY_DELAY);
        self.emit_workspace_status_changed();
    }

    fn normalize_workspace_watch_path(&self, path: &Path) -> Option<PathBuf> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.build_config.repo_path.join(path)
        };
        if self.build_config.git_mode == crate::config::GitMode::Auto {
            if let Some(index) = super::membership::git_index_path(&self.build_config.repo_path) {
                if path == index
                    || index
                        .parent()
                        .is_some_and(|parent| path == parent.join("HEAD"))
                {
                    return Some(path);
                }
            }
        }
        if !path.starts_with(&self.build_config.repo_path) {
            return None;
        }
        if self.build_config.git_mode == crate::config::GitMode::Filesystem
            && path
                .strip_prefix(&self.build_config.repo_path)
                .ok()
                .is_some_and(|relative| {
                    relative
                        .components()
                        .next()
                        .is_some_and(|part| part.as_os_str() == ".git")
                })
        {
            return None;
        }
        if path.starts_with(&self.build_config.storage_root) || is_workspace_internal_path(&path) {
            return None;
        }
        if is_git_index_noise_path(&path, &self.build_config.repo_path) {
            return None;
        }
        Some(path)
    }

    fn watch_path_requires_full_probe(&self, path: &Path) -> bool {
        if !path.starts_with(&self.build_config.repo_path) {
            return true;
        }
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
                ".ignore"
                    | ".gitignore"
                    | ".gitmodules"
                    | ".gitattributes"
                    | "HEAD"
                    | "config"
                    | "packed-refs"
            )
        }) {
            return true;
        }
        false
    }
}

fn is_git_index_noise_path(path: &Path, repo_root: &Path) -> bool {
    let Some(relative) = path.strip_prefix(repo_root).ok() else {
        return false;
    };
    relative == Path::new(".git/index.lock")
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
    use std::path::Path;

    #[test]
    fn concurrent_event_reservations_obey_the_path_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let bytes = AtomicUsize::new(0);
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let bytes = &bytes;
                let admitted = &admitted;
                scope.spawn(move || {
                    if super::reserve_event_bytes(bytes, super::WATCH_EVENT_PATH_BYTES / 4) {
                        admitted.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(admitted.load(Ordering::Relaxed), 4);
        assert_eq!(bytes.load(Ordering::Relaxed), super::WATCH_EVENT_PATH_BYTES);
        assert!(!super::reserve_event_bytes(&bytes, usize::MAX));
        bytes.fetch_sub(super::WATCH_EVENT_PATH_BYTES / 4, Ordering::AcqRel);
        assert!(super::reserve_event_bytes(
            &bytes,
            super::WATCH_EVENT_PATH_BYTES / 4
        ));
    }

    #[test]
    fn git_index_probe_artifacts_are_ignored_without_hiding_worktree_paths() {
        let repo = Path::new("/tmp/repo");
        assert!(!super::is_git_index_noise_path(
            &repo.join(".git/index"),
            repo
        ));
        assert!(super::is_git_index_noise_path(
            &repo.join(".git/index.lock"),
            repo
        ));
        assert!(!super::is_git_index_noise_path(
            &repo.join(".git/HEAD"),
            repo
        ));
        assert!(!super::is_git_index_noise_path(
            &repo.join("src/lib.rs"),
            repo
        ));
    }

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
