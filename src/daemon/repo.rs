use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{
    event::{ModifyKind, RemoveKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use serde_json::Value;

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    error::{AppError, Result},
    files::{scan_paths, ScanOptions},
    index::{DirtyPathKind, IndexBuildOptions, IndexWorktreeDiff},
    path_filter::{PathFilter, PathFilterArgs},
    progress::{IndexProgress, IndexProgressPhase},
    search::{
        FileContext, FileMatch, MatchLocation, SearchHit, SearchLine, SearchMode, SearchResults,
    },
    search_engine::{SearchBackend, SearchDocument, SearchDocumentIndex, SearchEngine},
    tokenizer::TokenizerOptions,
    workspace::{IndexStatus, WorkspaceIndex, WorkspaceIndexOptions},
};

use super::service::ServiceNotificationEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPhase {
    Opening,
    MissingIndex,
    Indexing,
    ReadyClean,
    ReadyDirty,
    Rebuilding,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchConsistency {
    SnapshotOnly,
    WorkspaceEventual,
    WorkspaceStrict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryBackend {
    IndexedSnapshot,
    IndexedClean,
    IndexedWorkspaceRepair,
    RgFallback,
    ScanFallback,
}

#[derive(Debug, Clone, Default)]
pub struct DirtyStats {
    pub modified: usize,
    pub deleted: usize,
    pub new: usize,
}

#[derive(Debug, Clone)]
pub struct RepoStatus {
    pub repo_id: String,
    pub repo_path: PathBuf,
    pub index_path: PathBuf,
    pub phase: RepoPhase,
    pub snapshot_key: Option<String>,
    pub last_probe_at: Option<SystemTime>,
    pub last_rebuild_at: Option<SystemTime>,
    pub dirty_files: DirtyStats,
    pub rebuild_recommended: bool,
    pub active_task_id: Option<String>,
    pub watcher_healthy: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RepoTaskKind {
    BuildIndex,
    RebuildIndex,
    RefreshWorkspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoTaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct RepoTaskStatus {
    pub task_id: String,
    pub repo_id: String,
    pub kind: RepoTaskKind,
    pub state: RepoTaskState,
    pub phase: Option<IndexProgressPhase>,
    pub message: String,
    pub processed: usize,
    pub total: Option<usize>,
    pub started_at: SystemTime,
    pub updated_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub cancellable: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoRefreshPolicy {
    pub rebuild_dirty_threshold: usize,
}

impl Default for RepoRefreshPolicy {
    fn default() -> Self {
        Self {
            rebuild_dirty_threshold: 256,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenRepoRequest {
    pub build_config: BuildConfig,
    pub refresh_policy: RepoRefreshPolicy,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub repo_id: String,
    pub query: QueryConfig,
    pub path_filter_args: PathFilterArgs,
    pub consistency: SearchConsistency,
    pub allow_scan_fallback: bool,
}

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub repo_id: String,
    pub backend: QueryBackend,
    pub consistency_applied: SearchConsistency,
    pub status: RepoStatus,
    pub results: SearchResults,
}

pub struct RepoManager {
    repos: Mutex<HashMap<String, Arc<RepoRuntime>>>,
    next_task_id: AtomicU64,
    notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>,
}

impl RepoManager {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::with_notifier(Arc::new(|_event| {}))
    }

    pub fn with_notifier(notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>) -> Self {
        Self {
            repos: Mutex::new(HashMap::new()),
            next_task_id: AtomicU64::new(0),
            notifier,
        }
    }

    pub fn open_repo(&self, request: OpenRepoRequest) -> Result<RepoStatus> {
        let build_config = request.build_config.normalized()?;
        let repo_id = build_config.repo_path.to_string_lossy().into_owned();

        let runtime = {
            let mut repos = match self.repos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(existing) = repos.get(&repo_id) {
                return existing.status_snapshot();
            }
            let runtime = Arc::new(RepoRuntime::new(
                repo_id.clone(),
                build_config,
                request.refresh_policy,
                Arc::clone(&self.notifier),
            )?);
            repos.insert(repo_id, Arc::clone(&runtime));
            runtime
        };

        RepoRuntime::start_watcher(&runtime)?;
        let task_id = self.allocate_task_id();
        let _ = runtime.start_task(task_id, RepoTaskKind::RefreshWorkspace)?;
        runtime.status_snapshot()
    }

    pub fn get_status(&self, repo_id: &str) -> Result<RepoStatus> {
        self.repo(repo_id)?.refresh(false)
    }

    pub fn refresh_repo(&self, repo_id: &str, force: bool) -> Result<RepoStatus> {
        self.repo(repo_id)?.refresh(force)
    }

    pub fn build_index(&self, repo_id: &str) -> Result<(usize, RepoStatus)> {
        self.repo(repo_id)?.build_index()
    }

    pub fn rebuild_index(&self, repo_id: &str) -> Result<(usize, RepoStatus)> {
        self.repo(repo_id)?.rebuild_index()
    }

    pub fn start_build_index(&self, repo_id: &str) -> Result<RepoTaskStatus> {
        let runtime = self.repo(repo_id)?;
        let task_id = self.allocate_task_id();
        runtime.start_task(task_id, RepoTaskKind::BuildIndex)
    }

    pub fn start_rebuild_index(&self, repo_id: &str) -> Result<RepoTaskStatus> {
        let runtime = self.repo(repo_id)?;
        let task_id = self.allocate_task_id();
        runtime.start_task(task_id, RepoTaskKind::RebuildIndex)
    }

    pub fn get_task_status(&self, task_id: &str) -> Result<RepoTaskStatus> {
        let repos = match self.repos.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for runtime in repos.values() {
            if let Some(task) = runtime.task_status(task_id) {
                return Ok(task);
            }
        }
        Err(AppError::Protocol(format!("unknown task_id: {task_id}")))
    }

    pub fn cancel_task(&self, task_id: &str) -> Result<bool> {
        let repos = match self.repos.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for runtime in repos.values() {
            if let Some(accepted) = runtime.cancel_task(task_id) {
                return Ok(accepted);
            }
        }
        Err(AppError::Protocol(format!("unknown task_id: {task_id}")))
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        self.repo(&request.repo_id)?.search(request)
    }

    pub fn close_repo(&self, repo_id: &str) -> Result<()> {
        let runtime = {
            let mut repos = match self.repos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            repos.remove(repo_id)
        };
        match runtime {
            Some(runtime) => {
                runtime.shutdown();
                Ok(())
            }
            None => Err(AppError::Protocol(format!("unknown repo_id: {repo_id}"))),
        }
    }

    pub fn shutdown_all(&self) {
        let runtimes = {
            let mut repos = match self.repos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            repos
                .drain()
                .map(|(_, runtime)| runtime)
                .collect::<Vec<_>>()
        };
        for runtime in runtimes {
            runtime.shutdown();
        }
    }

    fn repo(&self, repo_id: &str) -> Result<Arc<RepoRuntime>> {
        let repos = match self.repos.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        repos
            .get(repo_id)
            .cloned()
            .ok_or_else(|| AppError::Protocol(format!("unknown repo_id: {repo_id}")))
    }

    fn allocate_task_id(&self) -> String {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("task-{id}")
    }
}

impl Drop for RepoManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

struct RepoRuntime {
    repo_id: String,
    build_config: BuildConfig,
    search_engine: SearchEngine,
    workspace: WorkspaceIndex,
    rebuild_dirty_threshold: usize,
    notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>,
    state: Mutex<RepoState>,
    dirty_overlay: Mutex<DirtyOverlayState>,
    resync_required: AtomicBool,
    watcher_stop: Mutex<Option<mpsc::Sender<()>>>,
    watcher_handle: Mutex<Option<JoinHandle<()>>>,
    task_handle: Mutex<Option<ActiveTaskHandle>>,
    tasks: Mutex<HashMap<String, Arc<RepoTaskRecord>>>,
    shutting_down: AtomicBool,
}

struct ActiveTaskHandle {
    handle: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct RepoState {
    phase: RepoPhase,
    snapshot_key: Option<String>,
    last_probe_at: Option<SystemTime>,
    last_rebuild_at: Option<SystemTime>,
    dirty_files: DirtyStats,
    rebuild_recommended: bool,
    active_task_id: Option<String>,
    watcher_healthy: bool,
    last_error: Option<String>,
}

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
            watcher_healthy: true,
            last_error: None,
        }
    }
}

struct RepoTaskRecord {
    kind: RepoTaskKind,
    cancel_requested: AtomicBool,
    state: Mutex<RepoTaskStatus>,
}

#[derive(Debug, Clone, Default)]
struct DirtyPathSet {
    modified: std::collections::HashSet<String>,
    deleted: std::collections::HashSet<String>,
    new: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, Default)]
struct DirtyOverlayState {
    paths: DirtyPathSet,
    documents: HashMap<String, SearchDocument>,
    search_index: Option<Arc<SearchDocumentIndex>>,
}

#[derive(Debug, Clone)]
struct DirtyOverlaySnapshot {
    paths: DirtyPathSet,
    shadowed_base_paths: std::collections::HashSet<String>,
    search_index: Option<Arc<SearchDocumentIndex>>,
}

impl RepoTaskRecord {
    fn new(task_id: String, repo_id: String, kind: RepoTaskKind) -> Self {
        let now = SystemTime::now();
        Self {
            kind,
            cancel_requested: AtomicBool::new(false),
            state: Mutex::new(RepoTaskStatus {
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
        state.message = message;
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

impl DirtyPathSet {
    fn from_diff(diff: IndexWorktreeDiff) -> Self {
        Self {
            modified: diff.modified_files.into_iter().collect(),
            deleted: diff.deleted_files.into_iter().collect(),
            new: diff.new_files.into_iter().collect(),
        }
    }

    fn to_diff(&self) -> IndexWorktreeDiff {
        let mut modified_files = self.modified.iter().cloned().collect::<Vec<_>>();
        modified_files.sort_unstable();
        let mut deleted_files = self.deleted.iter().cloned().collect::<Vec<_>>();
        deleted_files.sort_unstable();
        let mut new_files = self.new.iter().cloned().collect::<Vec<_>>();
        new_files.sort_unstable();
        IndexWorktreeDiff {
            modified_files,
            deleted_files,
            new_files,
        }
    }

    fn stats(&self) -> DirtyStats {
        DirtyStats {
            modified: self.modified.len(),
            deleted: self.deleted.len(),
            new: self.new.len(),
        }
    }

    fn len(&self) -> usize {
        self.modified.len() + self.deleted.len() + self.new.len()
    }

    fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.new.is_empty()
    }

    fn upsert(&mut self, path: String, kind: DirtyPathKind) -> bool {
        let changed = self.remove(&path);
        match kind {
            DirtyPathKind::Modified => changed || self.modified.insert(path),
            DirtyPathKind::Deleted => changed || self.deleted.insert(path),
            DirtyPathKind::New => changed || self.new.insert(path),
        }
    }

    fn remove(&mut self, path: &str) -> bool {
        let removed_modified = self.modified.remove(path);
        let removed_deleted = self.deleted.remove(path);
        let removed_new = self.new.remove(path);
        removed_modified || removed_deleted || removed_new
    }

    fn merge(&mut self, other: DirtyPathSet) -> bool {
        let mut changed = false;
        for path in other.modified {
            changed |= self.upsert(path, DirtyPathKind::Modified);
        }
        for path in other.deleted {
            changed |= self.upsert(path, DirtyPathKind::Deleted);
        }
        for path in other.new {
            changed |= self.upsert(path, DirtyPathKind::New);
        }
        changed
    }
}

impl DirtyOverlayState {
    fn snapshot(&self) -> DirtyOverlaySnapshot {
        DirtyOverlaySnapshot {
            paths: self.paths.clone(),
            shadowed_base_paths: self
                .paths
                .modified
                .iter()
                .chain(self.paths.deleted.iter())
                .cloned()
                .collect(),
            search_index: self.search_index.clone(),
        }
    }
}

impl RepoRuntime {
    fn new(
        repo_id: String,
        build_config: BuildConfig,
        refresh_policy: RepoRefreshPolicy,
        notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>,
    ) -> Result<Self> {
        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: build_config.clone(),
        })?;
        let search_engine = SearchEngine::new(build_config.clone());
        Ok(Self {
            repo_id,
            build_config,
            search_engine,
            workspace,
            rebuild_dirty_threshold: refresh_policy.rebuild_dirty_threshold,
            notifier,
            state: Mutex::new(RepoState::default()),
            dirty_overlay: Mutex::new(DirtyOverlayState::default()),
            resync_required: AtomicBool::new(false),
            watcher_stop: Mutex::new(None),
            watcher_handle: Mutex::new(None),
            task_handle: Mutex::new(None),
            tasks: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn status_snapshot(&self) -> Result<RepoStatus> {
        let state = match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Ok(RepoStatus {
            repo_id: self.repo_id.clone(),
            repo_path: self.build_config.repo_path.clone(),
            index_path: self.build_config.index_path.clone(),
            phase: state.phase,
            snapshot_key: state.snapshot_key,
            last_probe_at: state.last_probe_at,
            last_rebuild_at: state.last_rebuild_at,
            dirty_files: state.dirty_files,
            rebuild_recommended: state.rebuild_recommended,
            active_task_id: state.active_task_id,
            watcher_healthy: state.watcher_healthy,
            last_error: state.last_error,
        })
    }

    fn emit_workspace_status_changed(&self) {
        if let Ok(status) = self.status_snapshot() {
            (self.notifier)(ServiceNotificationEvent::WorkspaceStatus(
                super::service::convert_repo_status(status),
            ));
        }
    }

    fn emit_task_progress(&self, task: RepoTaskStatus) {
        (self.notifier)(ServiceNotificationEvent::TaskProgress(task));
    }

    fn emit_task_finished(&self, task: RepoTaskStatus) {
        (self.notifier)(ServiceNotificationEvent::TaskFinished(task));
    }

    fn refresh(&self, force: bool) -> Result<RepoStatus> {
        if force || self.resync_required.load(Ordering::Relaxed) {
            self.refresh_overlay_from_worktree()?;
        }
        self.status_snapshot()
    }

    fn start_task(self: &Arc<Self>, task_id: String, kind: RepoTaskKind) -> Result<RepoTaskStatus> {
        self.cleanup_finished_task_handle();
        self.ensure_no_active_task()?;

        let task = Arc::new(RepoTaskRecord::new(
            task_id.clone(),
            self.repo_id.clone(),
            kind,
        ));
        {
            let mut tasks = match self.tasks.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            tasks.insert(task_id.clone(), Arc::clone(&task));
        }
        self.mark_task_started(kind, Some(task_id.clone()));

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

    fn task_status(&self, task_id: &str) -> Option<RepoTaskStatus> {
        let tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tasks.get(task_id).map(|task| task.snapshot())
    }

    fn cancel_task(&self, task_id: &str) -> Option<bool> {
        let tasks = match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tasks.get(task_id).map(|task| task.request_cancel())
    }

    fn build_index(&self) -> Result<(usize, RepoStatus)> {
        self.ensure_no_active_task()?;
        self.set_phase(RepoPhase::Indexing, None);
        let indexed_docs = self.workspace.ensure_base_snapshot()?.doc_count;
        self.refresh_overlay_from_worktree()?;
        {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        Ok((indexed_docs, self.status_snapshot()?))
    }

    fn rebuild_index(&self) -> Result<(usize, RepoStatus)> {
        self.ensure_no_active_task()?;
        self.set_phase(RepoPhase::Rebuilding, None);
        let indexed_docs = self.search_engine.rebuild_index()?;
        self.refresh_overlay_from_worktree()?;
        {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        Ok((indexed_docs, self.status_snapshot()?))
    }

    fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        let filter = build_path_filter(&self.build_config.repo_path, &request.path_filter_args)?;
        match request.consistency {
            SearchConsistency::SnapshotOnly => {
                let response = self.workspace.search_snapshot_with_filter(
                    &request.query,
                    filter.as_ref(),
                    request.allow_scan_fallback,
                )?;
                let backend = match response.backend {
                    SearchBackend::Index => QueryBackend::IndexedSnapshot,
                    SearchBackend::Scan => QueryBackend::ScanFallback,
                };
                Ok(SearchResponse {
                    repo_id: self.repo_id.clone(),
                    backend,
                    consistency_applied: request.consistency,
                    status: self.status_snapshot()?,
                    results: response.results,
                })
            }
            SearchConsistency::WorkspaceEventual | SearchConsistency::WorkspaceStrict => {
                if matches!(request.consistency, SearchConsistency::WorkspaceStrict)
                    || self.resync_required.load(Ordering::Relaxed)
                {
                    self.refresh(true)?;
                }

                let status = self.status_snapshot()?;
                match status.phase {
                    RepoPhase::Opening
                    | RepoPhase::MissingIndex
                    | RepoPhase::Indexing
                    | RepoPhase::Rebuilding => {
                        self.search_during_transition(&request, filter.as_ref(), status)
                    }
                    RepoPhase::ReadyClean | RepoPhase::ReadyDirty => {
                        let overlay = self.current_overlay_snapshot();
                        let backend = if overlay.paths.is_empty() {
                            QueryBackend::IndexedClean
                        } else {
                            QueryBackend::IndexedWorkspaceRepair
                        };
                        let results = if overlay.paths.is_empty() {
                            self.workspace.search_with_dirty_diff(
                                &request.query,
                                &IndexWorktreeDiff::default(),
                                filter.as_ref(),
                            )?
                        } else {
                            self.workspace.search_with_dirty_overlay(
                                &request.query,
                                &overlay.shadowed_base_paths,
                                overlay
                                    .search_index
                                    .as_deref()
                                    .expect("dirty overlay index should exist"),
                                filter.as_ref(),
                            )?
                        };
                        if !overlay.paths.is_empty() {
                            self.reconcile_dirty_paths()?;
                        }
                        Ok(SearchResponse {
                            repo_id: self.repo_id.clone(),
                            backend,
                            consistency_applied: request.consistency,
                            status: self.status_snapshot()?,
                            results,
                        })
                    }
                    RepoPhase::Degraded => Err(AppError::Protocol(
                        status
                            .last_error
                            .unwrap_or_else(|| "repo runtime is degraded".into()),
                    )),
                }
            }
        }
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

        let result = match task.kind {
            RepoTaskKind::BuildIndex => self
                .workspace
                .ensure_base_snapshot_with_options(
                    IndexBuildOptions::new()
                        .with_progress(&mut progress)
                        .with_cancel(&mut should_cancel),
                )
                .map(|info| info.doc_count),
            RepoTaskKind::RebuildIndex => self.search_engine.rebuild_index_with_options(
                IndexBuildOptions::new()
                    .with_progress(&mut progress)
                    .with_cancel(&mut should_cancel),
            ),
            RepoTaskKind::RefreshWorkspace => self.refresh_overlay_from_worktree().map(|_| 0),
        };

        match result.and_then(|indexed_docs| self.finish_task_success(task.kind, indexed_docs)) {
            Ok(message) => {
                task.finish_completed(message);
                self.set_active_task(None);
                self.emit_task_finished(task.snapshot());
                self.emit_workspace_status_changed();
            }
            Err(AppError::Cancelled) => {
                task.finish_cancelled("operation cancelled".into());
                self.set_active_task(None);
                self.emit_task_finished(task.snapshot());
                self.emit_workspace_status_changed();
            }
            Err(error) => {
                let message = error.to_string();
                task.finish_failed(message.clone());
                self.set_last_error(Some(message));
                self.set_active_task(None);
                self.emit_task_finished(task.snapshot());
                self.emit_workspace_status_changed();
            }
        }
    }

    fn finish_task_success(&self, kind: RepoTaskKind, indexed_docs: usize) -> Result<String> {
        if !matches!(kind, RepoTaskKind::RefreshWorkspace) {
            self.refresh_overlay_from_worktree()?;
        }
        {
            let mut state = match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            state.last_rebuild_at = Some(SystemTime::now());
            state.last_error = None;
        }
        let message = match kind {
            RepoTaskKind::BuildIndex => format!("indexed {indexed_docs} documents"),
            RepoTaskKind::RebuildIndex => format!("rebuilt index with {indexed_docs} documents"),
            RepoTaskKind::RefreshWorkspace => "refreshed workspace".into(),
        };
        Ok(message)
    }

    fn refresh_overlay_from_worktree(&self) -> Result<()> {
        match self.workspace.status() {
            Ok(IndexStatus { base, dirty_files }) => {
                let overlay = DirtyPathSet::from_diff(dirty_files.unwrap_or_default());
                self.resync_required.store(false, Ordering::Relaxed);
                let mut state = match self.state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.snapshot_key = base.map(|base| base.snapshot_key);
                state.last_probe_at = Some(SystemTime::now());
                state.last_error = None;
                drop(state);
                self.replace_dirty_paths(overlay)?;
                Ok(())
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.clear_overlay();
                let mut state = match self.state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.snapshot_key = None;
                state.last_probe_at = Some(SystemTime::now());
                state.last_error = None;
                state.dirty_files = DirtyStats::default();
                state.rebuild_recommended = false;
                state.phase = RepoPhase::MissingIndex;
                drop(state);
                self.emit_workspace_status_changed();
                Ok(())
            }
            Err(error) => {
                self.set_phase(RepoPhase::Degraded, Some(error.to_string()));
                Err(error)
            }
        }
    }

    fn reconcile_dirty_paths(&self) -> Result<()> {
        let current = self.current_dirty_paths();
        let remaining = self.workspace.reconcile_dirty_diff(&current)?;
        self.replace_dirty_paths(DirtyPathSet::from_diff(remaining))?;
        Ok(())
    }

    fn current_dirty_paths(&self) -> IndexWorktreeDiff {
        match self.dirty_overlay.lock() {
            Ok(guard) => guard.paths.to_diff(),
            Err(poisoned) => poisoned.into_inner().paths.to_diff(),
        }
    }

    fn current_overlay_snapshot(&self) -> DirtyOverlaySnapshot {
        match self.dirty_overlay.lock() {
            Ok(guard) => guard.snapshot(),
            Err(poisoned) => poisoned.into_inner().snapshot(),
        }
    }

    fn replace_dirty_paths(&self, dirty: DirtyPathSet) -> Result<()> {
        let documents = self.load_overlay_documents(&dirty)?;
        let search_index =
            self.build_overlay_search_index(documents.values().cloned().collect::<Vec<_>>());
        {
            let mut overlay = match self.dirty_overlay.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *overlay = DirtyOverlayState {
                paths: dirty.clone(),
                documents,
                search_index: Some(search_index),
            };
        }
        self.update_overlay_state(&dirty);
        Ok(())
    }

    fn add_dirty_paths(&self, delta: DirtyPathSet) {
        let dirty = {
            let mut overlay = match self.dirty_overlay.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !overlay.paths.merge(delta.clone()) {
                return;
            }
            if let Err(error) = self.apply_overlay_delta(&mut overlay, &delta) {
                drop(overlay);
                self.mark_resync_required();
                self.set_last_error(Some(format!("failed to refresh dirty overlay: {error}")));
                return;
            }
            self.sync_overlay_search_index_delta(&mut overlay, &delta);
            overlay.paths.clone()
        };
        self.update_overlay_state(&dirty);
    }

    fn load_overlay_documents(
        &self,
        dirty: &DirtyPathSet,
    ) -> Result<HashMap<String, SearchDocument>> {
        let mut documents = HashMap::new();
        for path in dirty.modified.iter().chain(dirty.new.iter()) {
            if let Some(document) = self.workspace.load_dirty_document(path)? {
                documents.insert(path.clone(), document);
            }
        }
        Ok(documents)
    }

    fn apply_overlay_delta(
        &self,
        overlay: &mut DirtyOverlayState,
        delta: &DirtyPathSet,
    ) -> Result<()> {
        for path in &delta.deleted {
            overlay.documents.remove(path);
        }
        for path in delta.modified.iter().chain(delta.new.iter()) {
            match self.workspace.load_dirty_document(path)? {
                Some(document) => {
                    overlay.documents.insert(path.clone(), document);
                }
                None => {
                    overlay.documents.remove(path);
                }
            }
        }
        Ok(())
    }

    fn update_overlay_state(&self, dirty: &DirtyPathSet) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let dirty_len = dirty.len();
        state.dirty_files = dirty.stats();
        state.rebuild_recommended = dirty_len >= self.rebuild_dirty_threshold;
        if state.snapshot_key.is_none() {
            state.phase = RepoPhase::MissingIndex;
        } else if dirty.is_empty() && !self.resync_required.load(Ordering::Relaxed) {
            state.phase = RepoPhase::ReadyClean;
        } else {
            state.phase = RepoPhase::ReadyDirty;
        }
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn clear_overlay(&self) {
        {
            let mut overlay = match self.dirty_overlay.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *overlay = DirtyOverlayState::default();
        }
        self.resync_required.store(false, Ordering::Relaxed);
    }

    fn build_overlay_search_index(
        &self,
        mut documents: Vec<SearchDocument>,
    ) -> Arc<SearchDocumentIndex> {
        documents.sort_unstable_by(|left, right| left.logical_path.cmp(&right.logical_path));
        Arc::new(SearchDocumentIndex::build(
            self.build_config.tokenizer,
            TokenizerOptions {
                min_sparse_len: self.build_config.min_sparse_len,
                max_sparse_len: self.build_config.max_sparse_len,
            },
            documents,
        ))
    }

    fn sync_overlay_search_index_delta(
        &self,
        overlay: &mut DirtyOverlayState,
        delta: &DirtyPathSet,
    ) {
        let search_index = overlay
            .search_index
            .get_or_insert_with(|| self.build_overlay_search_index(Vec::new()));
        let index = Arc::make_mut(search_index);

        for path in &delta.deleted {
            index.remove_document(path);
        }
        for path in delta.modified.iter().chain(delta.new.iter()) {
            match overlay.documents.get(path).cloned() {
                Some(document) => index.upsert_document(document),
                None => index.remove_document(path),
            }
        }
    }

    fn start_watcher(runtime: &Arc<Self>) -> Result<()> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        {
            let mut stop = match runtime.watcher_stop.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *stop = Some(stop_tx);
        }

        let thread_runtime = Arc::clone(runtime);
        let handle = thread::spawn(move || {
            let (event_tx, event_rx) = mpsc::channel();
            let mut watcher = match RecommendedWatcher::new(
                move |result| {
                    let _ = event_tx.send(result);
                },
                Config::default(),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("failed to start file watcher: {error}")));
                    thread_runtime.set_watcher_healthy(false);
                    thread_runtime.set_phase(
                        RepoPhase::Degraded,
                        Some(format!("failed to start file watcher: {error}")),
                    );
                    return;
                }
            };

            if let Err(error) = watcher.watch(
                &thread_runtime.build_config.repo_path,
                RecursiveMode::Recursive,
            ) {
                let _ = ready_tx.send(Err(format!("failed to watch repository: {error}")));
                thread_runtime.set_watcher_healthy(false);
                thread_runtime.set_phase(
                    RepoPhase::Degraded,
                    Some(format!("failed to watch repository: {error}")),
                );
                return;
            }
            let _ = ready_tx.send(Ok(()));

            while !thread_runtime.shutting_down.load(Ordering::Relaxed) {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match event_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(Ok(event)) => thread_runtime.handle_watch_event(event),
                    Ok(Err(error)) => {
                        thread_runtime.set_watcher_healthy(false);
                        thread_runtime.mark_resync_required();
                        thread_runtime.set_last_error(Some(format!("watch error: {error}")));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        let mut worker = match runtime.watcher_handle.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *worker = Some(handle);
        drop(worker);

        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(AppError::Protocol(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(AppError::Protocol(
                "timed out waiting for file watcher to start".into(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(AppError::Protocol(
                "file watcher stopped before reporting startup status".into(),
            )),
        }
    }

    fn handle_watch_event(&self, event: Event) {
        let mut dirty = DirtyPathSet::default();
        let mut needs_resync = matches!(
            event.kind,
            EventKind::Any
                | EventKind::Other
                | EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Remove(RemoveKind::Folder)
        );

        for path in event.paths {
            let candidate = if path.is_absolute() {
                path
            } else {
                self.build_config.repo_path.join(path)
            };
            if self.is_index_path(&candidate) {
                continue;
            }
            match self.normalize_watch_path(&candidate) {
                Some(normalized) => {
                    if candidate.exists() && candidate.is_dir() {
                        match self.collect_directory_dirty_paths(&candidate) {
                            Ok(directory_dirty) => {
                                if !directory_dirty.is_empty() {
                                    dirty.merge(directory_dirty);
                                } else {
                                    needs_resync = true;
                                }
                            }
                            Err(_) => needs_resync = true,
                        }
                        continue;
                    }
                    match self.classify_watch_path(&normalized) {
                        Ok(Some(kind)) => {
                            dirty.upsert(normalized, kind);
                        }
                        Ok(None) => {}
                        Err(_) => needs_resync = true,
                    }
                }
                None => {
                    needs_resync = true;
                }
            }
        }

        if !dirty.is_empty() {
            self.add_dirty_paths(dirty);
        }
        if needs_resync {
            self.mark_resync_required();
        }
    }

    fn collect_directory_dirty_paths(&self, directory: &Path) -> Result<DirtyPathSet> {
        let scanned = scan_paths(
            &[directory.to_path_buf()],
            Some(&self.build_config.index_path),
            ScanOptions {
                respect_ignore: matches!(self.build_config.corpus_mode, CorpusMode::RespectIgnore),
                include_hidden: self.build_config.include_hidden,
                max_file_size: self.build_config.max_file_size,
                max_depth: None,
                ignore_files: Vec::new(),
            },
            None,
        )?;

        let mut dirty = DirtyPathSet::default();
        for file in scanned {
            if self.is_index_path(&file.path) {
                continue;
            }
            let Some(normalized) = self.normalize_watch_path(&file.path) else {
                continue;
            };
            if let Some(kind) = self.classify_watch_path(&normalized)? {
                dirty.upsert(normalized, kind);
            }
        }
        Ok(dirty)
    }

    fn classify_watch_path(&self, path: &str) -> Result<Option<DirtyPathKind>> {
        match self.workspace.classify_dirty_path(path) {
            Ok(kind) => Ok(kind),
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn normalize_watch_path(&self, path: &Path) -> Option<String> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.build_config.repo_path.join(path)
        };
        let candidate = self.normalize_repo_watch_path(absolute)?;
        if !candidate.starts_with(&self.build_config.repo_path) {
            return None;
        }
        Some(candidate.to_string_lossy().into_owned())
    }

    fn normalize_repo_watch_path(&self, absolute: PathBuf) -> Option<PathBuf> {
        if absolute.starts_with(&self.build_config.repo_path) {
            return Some(absolute);
        }
        if let Ok(canonical) = std::fs::canonicalize(&absolute) {
            if canonical.starts_with(&self.build_config.repo_path) {
                return Some(canonical);
            }
        }
        let parent = absolute.parent()?;
        let file_name = absolute.file_name()?;
        let canonical_parent = std::fs::canonicalize(parent).ok()?;
        let adjusted = canonical_parent.join(file_name);
        adjusted
            .starts_with(&self.build_config.repo_path)
            .then_some(adjusted)
    }

    fn is_index_path(&self, path: &Path) -> bool {
        self.normalize_repo_watch_path(path.to_path_buf())
            .is_some_and(|candidate| candidate.starts_with(self.normalized_index_path()))
    }

    fn normalized_index_path(&self) -> PathBuf {
        if let Ok(canonical) = std::fs::canonicalize(&self.build_config.index_path) {
            return canonical;
        }
        let Some(parent) = self.build_config.index_path.parent() else {
            return self.build_config.index_path.clone();
        };
        let Some(file_name) = self.build_config.index_path.file_name() else {
            return self.build_config.index_path.clone();
        };
        match std::fs::canonicalize(parent) {
            Ok(canonical_parent) => canonical_parent.join(file_name),
            Err(_) => self.build_config.index_path.clone(),
        }
    }

    fn ensure_no_active_task(&self) -> Result<()> {
        self.cleanup_finished_task_handle();
        let active_task_id = match self.state.lock() {
            Ok(guard) => guard.active_task_id.clone(),
            Err(poisoned) => poisoned.into_inner().active_task_id.clone(),
        };
        if let Some(task_id) = active_task_id {
            if let Some(task) = self.task_status(&task_id) {
                if matches!(task.state, RepoTaskState::Queued | RepoTaskState::Running) {
                    return Err(AppError::Protocol(format!(
                        "repo {} already has a running task: {task_id}",
                        self.repo_id
                    )));
                }
            }
        }
        Ok(())
    }

    fn cleanup_finished_task_handle(&self) {
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

    fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        if let Some(task_id) = match self.state.lock() {
            Ok(guard) => guard.active_task_id.clone(),
            Err(poisoned) => poisoned.into_inner().active_task_id.clone(),
        } {
            let _ = self.cancel_task(&task_id);
        }
        if let Some(stop) = match self.watcher_stop.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = stop.send(());
        }
        if let Some(handle) = match self.watcher_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = handle.join();
        }
        if let Some(active) = match self.task_handle.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        } {
            let _ = active.handle.join();
        }
    }

    fn set_phase(&self, phase: RepoPhase, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.phase = phase;
        state.last_error = last_error;
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn set_last_error(&self, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.last_error = last_error;
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn set_active_task(&self, active_task_id: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active_task_id = active_task_id;
    }

    fn set_watcher_healthy(&self, watcher_healthy: bool) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.watcher_healthy = watcher_healthy;
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn mark_resync_required(&self) {
        self.resync_required.store(true, Ordering::Relaxed);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.snapshot_key.is_some() {
            state.phase = RepoPhase::ReadyDirty;
        }
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn mark_task_started(&self, kind: RepoTaskKind, active_task_id: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active_task_id = active_task_id;
        state.last_error = None;
        state.phase = match kind {
            RepoTaskKind::BuildIndex => RepoPhase::Indexing,
            RepoTaskKind::RebuildIndex => RepoPhase::Rebuilding,
            RepoTaskKind::RefreshWorkspace => RepoPhase::Opening,
        };
        drop(state);
        self.emit_workspace_status_changed();
    }

    fn search_during_transition(
        &self,
        request: &SearchRequest,
        filter: Option<&PathFilter>,
        status: RepoStatus,
    ) -> Result<SearchResponse> {
        if matches!(request.consistency, SearchConsistency::WorkspaceStrict) {
            return Err(AppError::Protocol(format!(
                "repo {} is not ready yet ({})",
                self.repo_id,
                repo_phase_label(status.phase)
            )));
        }

        match run_rg_search(
            &self.build_config,
            &request.query,
            &request.path_filter_args,
        ) {
            Ok(results) => Ok(SearchResponse {
                repo_id: self.repo_id.clone(),
                backend: QueryBackend::RgFallback,
                consistency_applied: request.consistency,
                status: self.status_snapshot()?,
                results,
            }),
            Err(_) => {
                Ok(SearchResponse {
                    repo_id: self.repo_id.clone(),
                    backend: QueryBackend::ScanFallback,
                    consistency_applied: request.consistency,
                    status: self.status_snapshot()?,
                    results: self.workspace.scan_search_with_filter(&request.query, filter)?,
                })
            }
        }
    }
}

fn repo_phase_label(phase: RepoPhase) -> &'static str {
    match phase {
        RepoPhase::Opening => "opening",
        RepoPhase::MissingIndex => "missing_index",
        RepoPhase::Indexing => "indexing",
        RepoPhase::ReadyClean => "ready_clean",
        RepoPhase::ReadyDirty => "ready_dirty",
        RepoPhase::Rebuilding => "rebuilding",
        RepoPhase::Degraded => "degraded",
    }
}

fn run_rg_search(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Result<SearchResults> {
    match query.search_mode {
        SearchMode::CountOnly => run_rg_count_search(build_config, query, scope, false),
        SearchMode::CountMatches => run_rg_count_search(build_config, query, scope, true),
        SearchMode::FirstHitOnly | SearchMode::MaterializeMatches => {
            run_rg_json_search(build_config, query, scope)
        }
    }
}

fn run_rg_count_search(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
    count_matches: bool,
) -> Result<SearchResults> {
    let (matched_lines, searches_with_match) =
        run_rg_count_command(build_config, query, scope, false)?;
    let matched_occurrences = if count_matches {
        run_rg_count_command(build_config, query, scope, true)?.0
    } else {
        0
    };
    Ok(SearchResults {
        candidate_docs: searches_with_match,
        searches_with_match,
        bytes_searched: 0,
        matched_lines,
        matched_occurrences,
        hits: Vec::new(),
    })
}

fn run_rg_count_command(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
    count_matches: bool,
) -> Result<(usize, usize)> {
    let mut command = build_rg_base_command(build_config, query, scope);
    if count_matches {
        command.arg("--count-matches");
    } else {
        command.arg("--count");
    }

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::Protocol(format!(
            "rg fallback failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let mut matched_total = 0usize;
    let mut searches_with_match = 0usize;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((_, count)) = line.rsplit_once(':') else {
            continue;
        };
        let count = count.parse::<usize>().map_err(|error| {
            AppError::Protocol(format!("failed to parse rg count output {line:?}: {error}"))
        })?;
        if count == 0 {
            continue;
        }
        matched_total += count;
        searches_with_match += 1;
    }
    Ok((matched_total, searches_with_match))
}

fn run_rg_json_search(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Result<SearchResults> {
    let mut command = build_rg_base_command(build_config, query, scope);
    command.arg("--json");
    if query.before_context > 0 {
        command.arg("-B").arg(query.before_context.to_string());
    }
    if query.after_context > 0 {
        command.arg("-A").arg(query.after_context.to_string());
    }
    if let Some(max_count) = query.max_count {
        command.arg("-m").arg(max_count.to_string());
    } else if matches!(query.search_mode, SearchMode::FirstHitOnly) {
        command.arg("-m").arg("1");
    }

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::Protocol(format!(
            "rg fallback failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let reader = BufReader::new(output.stdout.as_slice());
    let mut hits = Vec::<SearchHit>::new();
    let mut hit_indexes = HashMap::<String, usize>::new();
    let mut matched_lines = 0usize;
    let mut matched_occurrences = 0usize;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Protocol(format!("failed to decode rg json event: {error}"))
        })?;
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        let data = event.get("data").unwrap_or(&Value::Null);
        match kind {
            "match" => {
                let Some(path) = rg_text_field(data.get("path")) else {
                    continue;
                };
                let line_number = rg_line_number(data).unwrap_or(1);
                let snippet = rg_text_field(data.get("lines")).unwrap_or_default();
                let snippet = trim_rg_newline(&snippet).to_string();
                let index = ensure_rg_hit(&mut hits, &mut hit_indexes, &path);
                push_rg_context_break_if_needed(&mut hits[index], line_number);
                let submatches = data
                    .get("submatches")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if submatches.is_empty() {
                    continue;
                }
                matched_lines += 1;
                for submatch in submatches {
                    let file_match = rg_file_match(&snippet, line_number, &submatch)
                        .unwrap_or_else(|| FileMatch {
                            location: MatchLocation {
                                line: line_number,
                                column: 1,
                            },
                            snippet: snippet.clone(),
                            matched_text: snippet.clone(),
                        });
                    hits[index].matches.push(file_match.clone());
                    hits[index].lines.push(SearchLine::Match(file_match));
                    matched_occurrences += 1;
                }
            }
            "context" => {
                let Some(path) = rg_text_field(data.get("path")) else {
                    continue;
                };
                let line_number = rg_line_number(data).unwrap_or(1);
                let snippet = rg_text_field(data.get("lines")).unwrap_or_default();
                let snippet = trim_rg_newline(&snippet).to_string();
                let index = ensure_rg_hit(&mut hits, &mut hit_indexes, &path);
                push_rg_context_break_if_needed(&mut hits[index], line_number);
                hits[index].lines.push(SearchLine::Context(FileContext {
                    line_number,
                    snippet,
                }));
            }
            _ => {}
        }
    }

    let searches_with_match = hits.iter().filter(|hit| !hit.matches.is_empty()).count();
    hits.retain(|hit| !hit.matches.is_empty());
    Ok(SearchResults {
        candidate_docs: searches_with_match,
        searches_with_match,
        bytes_searched: 0,
        matched_lines,
        matched_occurrences,
        hits,
    })
}

fn build_rg_base_command(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Command {
    let mut command = Command::new("rg");
    command
        .current_dir(&build_config.repo_path)
        .arg("--with-filename")
        .arg("--color")
        .arg("never")
        .arg("--no-messages");

    if query.case_insensitive {
        command.arg("--ignore-case");
    }
    if query.multiline {
        command.arg("--multiline");
    }
    if query.dot_matches_new_line {
        command.arg("--multiline-dotall");
    }
    if query.fixed_strings {
        command.arg("--fixed-strings");
        for pattern in &query.patterns {
            command.arg("-e").arg(pattern);
        }
    } else {
        if query.word_regexp {
            command.arg("--word-regexp");
        }
        if query.line_regexp {
            command.arg("--line-regexp");
        }
        command.arg("-e").arg(&query.regex_pattern);
    }

    command
        .arg("--max-filesize")
        .arg(build_config.max_file_size.to_string());
    if matches!(build_config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
    if build_config.include_hidden {
        command.arg("--hidden");
    }

    for glob in &scope.globs {
        command.arg("-g").arg(glob);
    }
    for glob in &scope.iglobs {
        command.arg("--iglob").arg(glob);
    }
    for spec in &scope.type_add {
        command.arg("--type-add").arg(spec);
    }
    for name in &scope.type_clear {
        command.arg("--type-clear").arg(name);
    }
    for name in &scope.types {
        command.arg("-t").arg(name);
    }
    for name in &scope.type_not {
        command.arg("-T").arg(name);
    }
    if let Some(exclude_glob) =
        rg_index_exclude_glob(&build_config.repo_path, &build_config.index_path)
    {
        command.arg("-g").arg(format!("!{exclude_glob}"));
    }

    command.arg("--");
    let roots = rg_scope_roots(&build_config.repo_path, &scope.roots);
    if roots.is_empty() {
        command.arg(".");
    } else {
        for root in roots {
            command.arg(root);
        }
    }

    command
}

fn rg_scope_roots(repo_root: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    if roots.is_empty() {
        return Vec::new();
    }

    roots
        .iter()
        .map(|root| {
            if root.is_absolute() {
                if let Ok(relative) = root.strip_prefix(repo_root) {
                    if relative.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        relative.to_path_buf()
                    }
                } else {
                    root.clone()
                }
            } else if root.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                root.clone()
            }
        })
        .collect()
}

fn rg_index_exclude_glob(repo_root: &Path, index_path: &Path) -> Option<String> {
    let relative = index_path.strip_prefix(repo_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(format!(
        "{}/**",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn rg_text_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|field| field.get("text"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn rg_line_number(data: &Value) -> Option<usize> {
    data.get("line_number")
        .and_then(Value::as_u64)
        .and_then(|line| usize::try_from(line).ok())
}

fn trim_rg_newline(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn ensure_rg_hit(
    hits: &mut Vec<SearchHit>,
    indexes: &mut HashMap<String, usize>,
    path: &str,
) -> usize {
    if let Some(index) = indexes.get(path).copied() {
        return index;
    }
    let index = hits.len();
    hits.push(SearchHit {
        path: path.to_string(),
        matches: Vec::new(),
        lines: Vec::new(),
    });
    indexes.insert(path.to_string(), index);
    index
}

fn push_rg_context_break_if_needed(hit: &mut SearchHit, line_number: usize) {
    let previous_line = hit.lines.iter().rev().find_map(|line| match line {
        SearchLine::Match(value) => Some(value.location.line),
        SearchLine::Context(value) => Some(value.line_number),
        SearchLine::ContextBreak => None,
    });
    if previous_line.is_some_and(|previous| line_number > previous.saturating_add(1))
        && !matches!(hit.lines.last(), Some(SearchLine::ContextBreak))
    {
        hit.lines.push(SearchLine::ContextBreak);
    }
}

fn rg_file_match(snippet: &str, line_number: usize, submatch: &Value) -> Option<FileMatch> {
    let start = submatch.get("start")?.as_u64()?;
    let end = submatch.get("end")?.as_u64()?;
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    let matched_text = submatch
        .get("match")
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| snippet.get(start..end).map(ToOwned::to_owned))
        .unwrap_or_default();
    let column = snippet
        .get(..start)
        .map(|prefix| prefix.chars().count() + 1)
        .unwrap_or(1);
    Some(FileMatch {
        location: MatchLocation {
            line: line_number,
            column,
        },
        snippet: snippet.to_string(),
        matched_text,
    })
}

fn build_path_filter(repo_root: &PathBuf, args: &PathFilterArgs) -> Result<Option<PathFilter>> {
    let has_scope = !args.roots.is_empty()
        || !args.globs.is_empty()
        || !args.iglobs.is_empty()
        || !args.type_add.is_empty()
        || !args.type_clear.is_empty()
        || !args.types.is_empty()
        || !args.type_not.is_empty();
    if !has_scope {
        return Ok(None);
    }
    Ok(Some(PathFilter::new(args.clone(), repo_root)?))
}

pub fn system_time_to_unix_secs(time: Option<SystemTime>) -> Option<u64> {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

pub fn convert_search_results(results: SearchResults) -> crate::daemon::protocol::SearchResults {
    crate::daemon::protocol::SearchResults {
        candidate_docs: results.candidate_docs,
        searches_with_match: results.searches_with_match,
        bytes_searched: results.bytes_searched,
        matched_lines: results.matched_lines,
        matched_occurrences: results.matched_occurrences,
        hits: results
            .hits
            .into_iter()
            .map(|hit| crate::daemon::protocol::SearchHit {
                path: hit.path,
                matches: hit
                    .matches
                    .into_iter()
                    .map(|matched| crate::daemon::protocol::FileMatch {
                        location: crate::daemon::protocol::MatchLocation {
                            line: matched.location.line,
                            column: matched.location.column,
                        },
                        snippet: matched.snippet,
                        matched_text: matched.matched_text,
                    })
                    .collect(),
                lines: hit
                    .lines
                    .into_iter()
                    .map(|line| match line {
                        SearchLine::Match(value) => crate::daemon::protocol::SearchLine::Match {
                            value: crate::daemon::protocol::FileMatch {
                                location: crate::daemon::protocol::MatchLocation {
                                    line: value.location.line,
                                    column: value.location.column,
                                },
                                snippet: value.snippet,
                                matched_text: value.matched_text,
                            },
                        },
                        SearchLine::Context(context) => {
                            crate::daemon::protocol::SearchLine::Context {
                                line_number: context.line_number,
                                snippet: context.snippet,
                            }
                        }
                        SearchLine::ContextBreak => {
                            crate::daemon::protocol::SearchLine::ContextBreak
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub fn convert_task_status(task: RepoTaskStatus) -> crate::daemon::protocol::TaskStatus {
    crate::daemon::protocol::TaskStatus {
        task_id: task.task_id,
        workspace_id: task.repo_id,
        kind: convert_task_kind(task.kind),
        state: convert_task_state(task.state),
        phase: task.phase.map(convert_task_phase),
        message: task.message,
        processed: task.processed,
        total: task.total,
        started_unix_secs: task
            .started_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        updated_unix_secs: task
            .updated_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        finished_unix_secs: system_time_to_unix_secs(task.finished_at),
        cancellable: task.cancellable,
        error: task.error,
    }
}

pub fn convert_task_kind(kind: RepoTaskKind) -> crate::daemon::protocol::TaskKind {
    match kind {
        RepoTaskKind::BuildIndex => crate::daemon::protocol::TaskKind::BuildIndex,
        RepoTaskKind::RebuildIndex => crate::daemon::protocol::TaskKind::RebuildIndex,
        RepoTaskKind::RefreshWorkspace => crate::daemon::protocol::TaskKind::RefreshWorkspace,
    }
}

pub fn convert_task_state(state: RepoTaskState) -> crate::daemon::protocol::TaskState {
    match state {
        RepoTaskState::Queued => crate::daemon::protocol::TaskState::Queued,
        RepoTaskState::Running => crate::daemon::protocol::TaskState::Running,
        RepoTaskState::Completed => crate::daemon::protocol::TaskState::Completed,
        RepoTaskState::Failed => crate::daemon::protocol::TaskState::Failed,
        RepoTaskState::Cancelled => crate::daemon::protocol::TaskState::Cancelled,
    }
}

pub fn convert_task_phase(phase: IndexProgressPhase) -> crate::daemon::protocol::TaskPhase {
    match phase {
        IndexProgressPhase::Scanning => crate::daemon::protocol::TaskPhase::Scanning,
        IndexProgressPhase::Tokenizing => crate::daemon::protocol::TaskPhase::Tokenizing,
        IndexProgressPhase::Writing => crate::daemon::protocol::TaskPhase::Writing,
        IndexProgressPhase::Finalizing => crate::daemon::protocol::TaskPhase::Finalizing,
        IndexProgressPhase::RefreshingOverlay => {
            crate::daemon::protocol::TaskPhase::RefreshingOverlay
        }
    }
}

pub fn convert_query_backend(backend: QueryBackend) -> crate::daemon::protocol::SearchBackend {
    match backend {
        QueryBackend::IndexedSnapshot => crate::daemon::protocol::SearchBackend::IndexedSnapshot,
        QueryBackend::IndexedClean => crate::daemon::protocol::SearchBackend::IndexedClean,
        QueryBackend::IndexedWorkspaceRepair => {
            crate::daemon::protocol::SearchBackend::IndexedWorkspaceRepair
        }
        QueryBackend::RgFallback => crate::daemon::protocol::SearchBackend::RgFallback,
        QueryBackend::ScanFallback => crate::daemon::protocol::SearchBackend::ScanFallback,
    }
}

pub fn convert_repo_phase(phase: RepoPhase) -> crate::daemon::protocol::RepoPhase {
    match phase {
        RepoPhase::Opening => crate::daemon::protocol::RepoPhase::Opening,
        RepoPhase::MissingIndex => crate::daemon::protocol::RepoPhase::MissingIndex,
        RepoPhase::Indexing => crate::daemon::protocol::RepoPhase::Indexing,
        RepoPhase::ReadyClean => crate::daemon::protocol::RepoPhase::ReadyClean,
        RepoPhase::ReadyDirty => crate::daemon::protocol::RepoPhase::ReadyDirty,
        RepoPhase::Rebuilding => crate::daemon::protocol::RepoPhase::Rebuilding,
        RepoPhase::Degraded => crate::daemon::protocol::RepoPhase::Degraded,
    }
}

pub fn convert_consistency(
    consistency: SearchConsistency,
) -> crate::daemon::protocol::ConsistencyMode {
    match consistency {
        SearchConsistency::SnapshotOnly => crate::daemon::protocol::ConsistencyMode::SnapshotOnly,
        SearchConsistency::WorkspaceEventual => {
            crate::daemon::protocol::ConsistencyMode::WorkspaceEventual
        }
        SearchConsistency::WorkspaceStrict => {
            crate::daemon::protocol::ConsistencyMode::WorkspaceStrict
        }
    }
}

pub fn build_config_from_open(
    repo_path: PathBuf,
    index_path: Option<PathBuf>,
    tokenizer: crate::daemon::protocol::TokenizerModeConfig,
    corpus_mode: crate::daemon::protocol::CorpusModeConfig,
    include_hidden: bool,
    max_file_size: u64,
    min_sparse_len: usize,
    max_sparse_len: usize,
) -> BuildConfig {
    BuildConfig {
        index_path: index_path.unwrap_or_else(|| repo_path.join(".codgrep-index")),
        repo_path,
        tokenizer: match tokenizer {
            crate::daemon::protocol::TokenizerModeConfig::Trigram => TokenizerMode::Trigram,
            crate::daemon::protocol::TokenizerModeConfig::SparseNgram => TokenizerMode::SparseNgram,
        },
        corpus_mode: match corpus_mode {
            crate::daemon::protocol::CorpusModeConfig::RespectIgnore => CorpusMode::RespectIgnore,
            crate::daemon::protocol::CorpusModeConfig::NoIgnore => CorpusMode::NoIgnore,
        },
        include_hidden,
        max_file_size,
        min_sparse_len,
        max_sparse_len,
    }
}
