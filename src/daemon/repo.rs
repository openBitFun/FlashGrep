use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    error::{AppError, Result},
    index::{DirtyPathKind, IndexWorktreeDiff},
    path_filter::{PathFilter, PathFilterArgs},
    search::{SearchLine, SearchResults},
    search_engine::{SearchBackend, SearchDocument, SearchEngine, SearchPolicy},
    workspace::{IndexStatus, WorkspaceIndex, WorkspaceIndexOptions},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoPhase {
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
    pub last_error: Option<String>,
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

#[derive(Default)]
pub struct RepoManager {
    repos: Mutex<HashMap<String, Arc<RepoRuntime>>>,
}

impl RepoManager {
    pub fn new() -> Self {
        Self::default()
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
            )?);
            runtime.refresh_overlay_from_worktree()?;
            RepoRuntime::start_watcher(&runtime)?;
            repos.insert(repo_id, Arc::clone(&runtime));
            runtime
        };

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
    state: Mutex<RepoState>,
    dirty_overlay: Mutex<DirtyOverlayState>,
    resync_required: AtomicBool,
    watcher_stop: Mutex<Option<mpsc::Sender<()>>>,
    watcher_handle: Mutex<Option<JoinHandle<()>>>,
    shutting_down: AtomicBool,
}

#[derive(Debug, Clone)]
struct RepoState {
    phase: RepoPhase,
    snapshot_key: Option<String>,
    last_probe_at: Option<SystemTime>,
    last_rebuild_at: Option<SystemTime>,
    dirty_files: DirtyStats,
    rebuild_recommended: bool,
    last_error: Option<String>,
}

impl Default for RepoState {
    fn default() -> Self {
        Self {
            phase: RepoPhase::MissingIndex,
            snapshot_key: None,
            last_probe_at: None,
            last_rebuild_at: None,
            dirty_files: DirtyStats::default(),
            rebuild_recommended: false,
            last_error: None,
        }
    }
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
}

#[derive(Debug, Clone)]
struct DirtyOverlaySnapshot {
    paths: DirtyPathSet,
    shadowed_base_paths: std::collections::HashSet<String>,
    documents: Vec<SearchDocument>,
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
        let mut documents = self.documents.values().cloned().collect::<Vec<_>>();
        documents.sort_unstable_by(|left, right| left.logical_path.cmp(&right.logical_path));
        DirtyOverlaySnapshot {
            paths: self.paths.clone(),
            shadowed_base_paths: self
                .paths
                .modified
                .iter()
                .chain(self.paths.deleted.iter())
                .cloned()
                .collect(),
            documents,
        }
    }
}

impl RepoRuntime {
    fn new(
        repo_id: String,
        build_config: BuildConfig,
        refresh_policy: RepoRefreshPolicy,
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
            state: Mutex::new(RepoState::default()),
            dirty_overlay: Mutex::new(DirtyOverlayState::default()),
            resync_required: AtomicBool::new(false),
            watcher_stop: Mutex::new(None),
            watcher_handle: Mutex::new(None),
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
            last_error: state.last_error,
        })
    }

    fn refresh(&self, force: bool) -> Result<RepoStatus> {
        if force || self.resync_required.load(Ordering::Relaxed) {
            self.refresh_overlay_from_worktree()?;
        }
        self.status_snapshot()
    }

    fn build_index(&self) -> Result<(usize, RepoStatus)> {
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
                let response = self.search_engine.search_with_filter(
                    &request.query,
                    filter.as_ref(),
                    if request.allow_scan_fallback {
                        SearchPolicy::FallbackToScan
                    } else {
                        SearchPolicy::IndexOnly
                    },
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
                    RepoPhase::MissingIndex => {
                        if !request.allow_scan_fallback {
                            return Err(AppError::Protocol(format!(
                                "repo {} does not have an index yet",
                                self.repo_id
                            )));
                        }
                        let response = self.search_engine.search_with_filter(
                            &request.query,
                            filter.as_ref(),
                            SearchPolicy::FallbackToScan,
                        )?;
                        Ok(SearchResponse {
                            repo_id: self.repo_id.clone(),
                            backend: QueryBackend::ScanFallback,
                            consistency_applied: request.consistency,
                            status,
                            results: response.results,
                        })
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
                                &overlay.documents,
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
                    RepoPhase::Indexing | RepoPhase::Rebuilding => Err(AppError::Protocol(
                        format!("repo {} is busy building an index", self.repo_id),
                    )),
                    RepoPhase::Degraded => Err(AppError::Protocol(
                        status
                            .last_error
                            .unwrap_or_else(|| "repo runtime is degraded".into()),
                    )),
                }
            }
        }
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
        {
            let mut overlay = match self.dirty_overlay.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *overlay = DirtyOverlayState {
                paths: dirty.clone(),
                documents,
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

    fn start_watcher(runtime: &Arc<Self>) -> Result<()> {
        let (stop_tx, stop_rx) = mpsc::channel();
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
                thread_runtime.set_phase(
                    RepoPhase::Degraded,
                    Some(format!("failed to watch repository: {error}")),
                );
                return;
            }

            while !thread_runtime.shutting_down.load(Ordering::Relaxed) {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                match event_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(Ok(event)) => thread_runtime.handle_watch_event(event),
                    Ok(Err(error)) => {
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
        Ok(())
    }

    fn handle_watch_event(&self, event: Event) {
        let mut dirty = DirtyPathSet::default();
        let mut needs_resync = matches!(
            event.kind,
            EventKind::Any
                | EventKind::Other
                | EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Create(CreateKind::Folder)
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
                        needs_resync = true;
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

    fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
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
    }

    fn set_phase(&self, phase: RepoPhase, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.phase = phase;
        state.last_error = last_error;
    }

    fn set_last_error(&self, last_error: Option<String>) {
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.last_error = last_error;
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
    }
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

pub fn convert_query_backend(backend: QueryBackend) -> crate::daemon::protocol::SearchBackend {
    match backend {
        QueryBackend::IndexedSnapshot => crate::daemon::protocol::SearchBackend::IndexedSnapshot,
        QueryBackend::IndexedClean => crate::daemon::protocol::SearchBackend::IndexedClean,
        QueryBackend::IndexedWorkspaceRepair => {
            crate::daemon::protocol::SearchBackend::IndexedWorkspaceRepair
        }
        QueryBackend::ScanFallback => crate::daemon::protocol::SearchBackend::ScanFallback,
    }
}

pub fn convert_repo_phase(phase: RepoPhase) -> crate::daemon::protocol::RepoPhase {
    match phase {
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
