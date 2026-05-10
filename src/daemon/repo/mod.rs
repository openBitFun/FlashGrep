use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak,
    },
    thread::JoinHandle,
    time::SystemTime,
};

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    index_engine::WorkspaceSearchView,
    index_engine::{
        workspace::{
            WorkspaceOverlayIngestBatch, WorkspaceOverlayMergeEvent, WorkspaceOverlayPublishPolicy,
            WorkspaceOverlayRuntimeStats, WorkspaceOverlaySnapshotState,
            DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
        },
        RepoId, WorkspaceBaseSnapshotIndex, WorkspaceOverlayIndex,
    },
    path_filter::PathFilterArgs,
    progress::IndexProgressPhase,
    search::SearchResults,
    search_engine::SearchEngine,
};

use super::{
    rg_backend::{run_rg_glob, run_rg_search},
    service::ServiceNotificationEvent,
};
mod base;
mod catalog;
mod dirty_catalog;
mod maintenance;
mod overlay;
mod query;
mod status;
mod tasks;
mod watch;
mod workspace;

use maintenance::MaintenanceWorkerState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPhase {
    Opening,
    MissingBaseSnapshot,
    BuildingBaseSnapshot,
    ReadyClean,
    ReadyDirty,
    RebuildingBaseSnapshot,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryBackend {
    IndexedClean,
    IndexedWorkspaceView,
    RgFallback,
    ScanFallback,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DirtyStats {
    pub modified: usize,
    pub deleted: usize,
    pub new: usize,
}

#[derive(Debug, Clone)]
pub struct RepoStatus {
    pub repo_id: String,
    pub repo_path: PathBuf,
    pub storage_root: PathBuf,
    pub base_snapshot_root: PathBuf,
    pub workspace_overlay_root: PathBuf,
    pub phase: RepoPhase,
    pub snapshot_key: Option<String>,
    pub last_probe_at: Option<SystemTime>,
    pub last_rebuild_at: Option<SystemTime>,
    pub dirty_files: DirtyStats,
    pub rebuild_recommended: bool,
    pub active_task_id: Option<String>,
    pub probe_healthy: bool,
    pub last_error: Option<String>,
    pub overlay: Option<WorkspaceOverlayStatus>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceOverlayStatus {
    pub committed_seq_no: u64,
    pub last_seq_no: u64,
    pub uncommitted_ops: u64,
    pub pending_docs: usize,
    pub active_segments: usize,
    pub active_delete_segments: usize,
    pub merge_requested: bool,
    pub merge_running: bool,
    pub merge_attempts: u64,
    pub merge_completed: u64,
    pub merge_failed: u64,
    pub last_merge_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RepoTaskKind {
    BuildBaseSnapshot,
    RebuildBaseSnapshot,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshPolicy {
    pub rebuild_dirty_threshold: usize,
    pub overlay_auto_checkpoint_max_uncommitted_ops: u64,
    pub overlay_merge_min_delay_ms: u64,
    pub overlay_merge_retry_delay_ms: u64,
}

impl Default for RepoRefreshPolicy {
    fn default() -> Self {
        Self {
            rebuild_dirty_threshold: 256,
            overlay_auto_checkpoint_max_uncommitted_ops:
                DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
            overlay_merge_min_delay_ms: 2_000,
            overlay_merge_retry_delay_ms: 10_000,
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
}

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub repo_id: String,
    pub backend: QueryBackend,
    pub status: RepoStatus,
    pub results: SearchResults,
}

#[derive(Debug, Clone)]
pub struct GlobRequest {
    pub repo_id: String,
    pub path_filter_args: PathFilterArgs,
}

#[derive(Debug, Clone)]
pub struct GlobResponse {
    pub repo_id: String,
    pub status: RepoStatus,
    pub paths: Vec<String>,
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
        crate::gitignore::ensure_storage_root_ignored(&build_config)?;
        let repo_id = build_config.repo_path.to_string_lossy().into_owned();
        let stale_runtime = {
            let mut repos = match self.repos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match repos.get(&repo_id) {
                Some(existing)
                    if existing.matches_open_request(&build_config, &request.refresh_policy) =>
                {
                    return Ok(existing.status_snapshot());
                }
                Some(_) => repos.remove(&repo_id),
                None => None,
            }
        };
        if let Some(runtime) = stale_runtime {
            runtime.shutdown();
        }

        let runtime = {
            let mut repos = match self.repos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let runtime = Arc::new(RepoRuntime::new(
                repo_id.clone(),
                build_config,
                request.refresh_policy,
                Arc::clone(&self.notifier),
            )?);
            runtime.install_self_handle(Arc::downgrade(&runtime));
            RepoRuntime::start_maintenance_worker(&runtime);
            RepoRuntime::start_workspace_watcher(&runtime);
            repos.insert(repo_id, Arc::clone(&runtime));
            runtime
        };

        runtime.bootstrap_from_existing_snapshot()?;
        if runtime.status_snapshot().snapshot_key.is_some() {
            if let Err(error) = runtime.refresh_workspace_overlay() {
                runtime.mark_workspace_probe_pending_with_error(format!(
                    "initial workspace refresh failed: {error}"
                ));
            }
        }
        runtime.schedule_workspace_probe(runtime.workspace_probe_interval());
        Ok(runtime.status_snapshot())
    }

    pub fn get_status(&self, repo_id: &str) -> Result<RepoStatus> {
        Ok(self.repo(repo_id)?.status_snapshot())
    }

    pub fn refresh_repo(&self, repo_id: &str, force: bool) -> Result<RepoStatus> {
        self.repo(repo_id)?.refresh(force)
    }

    pub fn build_base_snapshot(&self, repo_id: &str) -> Result<(usize, RepoStatus)> {
        self.repo(repo_id)?.build_base_snapshot()
    }

    pub fn rebuild_base_snapshot(&self, repo_id: &str) -> Result<(usize, RepoStatus)> {
        self.repo(repo_id)?.rebuild_base_snapshot()
    }

    pub fn start_build_base_snapshot(&self, repo_id: &str) -> Result<RepoTaskStatus> {
        let runtime = self.repo(repo_id)?;
        let task_id = self.allocate_task_id();
        runtime.start_task(task_id, RepoTaskKind::BuildBaseSnapshot)
    }

    pub fn start_rebuild_base_snapshot(&self, repo_id: &str) -> Result<RepoTaskStatus> {
        let runtime = self.repo(repo_id)?;
        let task_id = self.allocate_task_id();
        runtime.start_task(task_id, RepoTaskKind::RebuildBaseSnapshot)
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

    pub fn has_active_tasks(&self) -> bool {
        let repos = match self.repos.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        repos.values().any(|runtime| runtime.has_active_task())
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        self.repo(&request.repo_id)?.search(request)
    }

    pub fn glob(&self, request: GlobRequest) -> Result<GlobResponse> {
        self.repo(&request.repo_id)?.glob(request)
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

    #[cfg(test)]
    pub(crate) fn clear_workspace_overlay_engine_for_tests(&self, repo_id: &str) -> Result<()> {
        let runtime = self.repo(repo_id)?;
        runtime.reset_workspace_overlay_engine();
        runtime.mark_workspace_probe_pending();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn has_base_snapshot_engine_for_tests(&self, repo_id: &str) -> Result<bool> {
        let runtime = self.repo(repo_id)?;
        let slot = match runtime.base_snapshot_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(slot.is_some())
    }

    #[cfg(test)]
    pub(crate) fn mark_workspace_probe_pending_for_tests(&self, repo_id: &str) -> Result<()> {
        let runtime = self.repo(repo_id)?;
        runtime.mark_workspace_probe_pending();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn clear_published_overlay_paths_cache_for_tests(
        &self,
        repo_id: &str,
    ) -> Result<()> {
        let runtime = self.repo(repo_id)?;
        let mut published_overlay_paths_cache = match runtime.published_overlay_paths_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *published_overlay_paths_cache = DirtyPathSet::default();
        Ok(())
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
    overlay_repo_id: RepoId,
    build_config: BuildConfig,
    search_engine: SearchEngine,
    workspace_search_view: WorkspaceSearchView,
    base_snapshot_engine: Arc<Mutex<Option<WorkspaceBaseSnapshotIndex>>>,
    base_snapshot_catalog: Mutex<Option<CachedBaseSnapshotCatalog>>,
    workspace_overlay_engine: Mutex<Option<Arc<WorkspaceOverlayIndex>>>,
    dirty_catalog: Mutex<dirty_catalog::DirtyCatalog>,
    overlay_write_lock: Mutex<()>,
    rebuild_dirty_threshold: usize,
    overlay_auto_checkpoint_max_uncommitted_ops: u64,
    overlay_merge_min_delay: std::time::Duration,
    overlay_merge_retry_delay: std::time::Duration,
    notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>,
    self_handle: OnceLock<Weak<RepoRuntime>>,
    maintenance_worker: Arc<(Mutex<MaintenanceWorkerState>, Condvar)>,
    maintenance_handle: Mutex<Option<JoinHandle<()>>>,
    watcher_handle: Mutex<Option<JoinHandle<()>>>,
    state: Mutex<RepoState>,
    published_overlay_paths_cache: Mutex<DirtyPathSet>,
    workspace_event_buffer: Mutex<watch::WorkspaceEventBuffer>,
    workspace_watcher_active: AtomicBool,
    glob_cache: Mutex<GlobCacheState>,
    glob_cache_epoch: AtomicU64,
    active_searches: AtomicU64,
    next_background_task_id: AtomicU64,
    workspace_probe_pending: AtomicBool,
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
    probe_healthy: bool,
    last_error: Option<String>,
}

struct RepoTaskRecord {
    kind: RepoTaskKind,
    cancel_requested: AtomicBool,
    state: Mutex<RepoTaskStatus>,
}

#[derive(Debug, Clone)]
struct CachedBaseSnapshotCatalog {
    snapshot_key: String,
    catalog: crate::index_engine::reader::SnapshotPathCatalog,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DirtyPathSet {
    modified: std::collections::HashSet<String>,
    deleted: std::collections::HashSet<String>,
    new: std::collections::HashSet<String>,
}

#[derive(Debug, Default)]
struct GlobCacheState {
    epoch: u64,
    entries: HashMap<String, Vec<String>>,
}

impl RepoRuntime {
    fn matches_open_request(
        &self,
        build_config: &BuildConfig,
        refresh_policy: &RepoRefreshPolicy,
    ) -> bool {
        self.build_config == *build_config
            && self.rebuild_dirty_threshold == refresh_policy.rebuild_dirty_threshold
            && self.overlay_auto_checkpoint_max_uncommitted_ops
                == refresh_policy.overlay_auto_checkpoint_max_uncommitted_ops
            && self.overlay_merge_min_delay
                == std::time::Duration::from_millis(refresh_policy.overlay_merge_min_delay_ms)
            && self.overlay_merge_retry_delay
                == std::time::Duration::from_millis(refresh_policy.overlay_merge_retry_delay_ms)
    }

    fn new(
        repo_id: String,
        build_config: BuildConfig,
        refresh_policy: RepoRefreshPolicy,
        notifier: Arc<dyn Fn(ServiceNotificationEvent) + Send + Sync>,
    ) -> Result<Self> {
        let dirty_catalog_path = dirty_catalog::dirty_catalog_path(&build_config.storage_root);
        let workspace_search_view = WorkspaceSearchView::open(build_config.clone())?;
        let search_engine = SearchEngine::new(build_config.clone());
        Ok(Self {
            overlay_repo_id: RepoId(repo_id.clone()),
            repo_id,
            build_config,
            search_engine,
            workspace_search_view,
            base_snapshot_engine: Arc::new(Mutex::new(None)),
            base_snapshot_catalog: Mutex::new(None),
            workspace_overlay_engine: Mutex::new(None),
            dirty_catalog: Mutex::new(dirty_catalog::DirtyCatalog::load(&dirty_catalog_path)?),
            overlay_write_lock: Mutex::new(()),
            rebuild_dirty_threshold: refresh_policy.rebuild_dirty_threshold,
            overlay_auto_checkpoint_max_uncommitted_ops: refresh_policy
                .overlay_auto_checkpoint_max_uncommitted_ops,
            overlay_merge_min_delay: std::time::Duration::from_millis(
                refresh_policy.overlay_merge_min_delay_ms,
            ),
            overlay_merge_retry_delay: std::time::Duration::from_millis(
                refresh_policy.overlay_merge_retry_delay_ms,
            ),
            notifier,
            self_handle: OnceLock::new(),
            maintenance_worker: Arc::new((
                Mutex::new(MaintenanceWorkerState::default()),
                Condvar::new(),
            )),
            maintenance_handle: Mutex::new(None),
            watcher_handle: Mutex::new(None),
            state: Mutex::new(RepoState::default()),
            published_overlay_paths_cache: Mutex::new(DirtyPathSet::default()),
            workspace_event_buffer: Mutex::new(watch::WorkspaceEventBuffer::default()),
            workspace_watcher_active: AtomicBool::new(false),
            glob_cache: Mutex::new(GlobCacheState::default()),
            glob_cache_epoch: AtomicU64::new(0),
            active_searches: AtomicU64::new(0),
            next_background_task_id: AtomicU64::new(0),
            workspace_probe_pending: AtomicBool::new(false),
            task_handle: Mutex::new(None),
            tasks: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn install_self_handle(&self, handle: Weak<RepoRuntime>) {
        let _ = self.self_handle.set(handle);
    }
}
