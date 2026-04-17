use std::{sync::mpsc, sync::Arc};

use super::{
    protocol::{RequestEnvelope, ResponseEnvelope, ServerMessage},
    repo::RepoManager,
};

#[path = "service/connections.rs"]
mod connections;
#[path = "service/notifications.rs"]
mod notifications;
#[path = "service/router.rs"]
mod router;

use connections::ConnectionRegistry;
pub use notifications::ServiceNotificationEvent;
use router::protocol_error;

#[derive(Clone)]
pub struct DaemonService {
    repos: Arc<RepoManager>,
    connections: Arc<ConnectionRegistry>,
}

impl DaemonService {
    pub fn new() -> Self {
        let connections = Arc::new(ConnectionRegistry::default());
        let notify_connections = Arc::clone(&connections);
        Self {
            repos: Arc::new(RepoManager::with_notifier(Arc::new(move |event| {
                notify_connections.broadcast(event);
            }))),
            connections,
        }
    }

    pub fn shutdown_all(&self) {
        self.repos.shutdown_all();
    }

    #[allow(dead_code)]
    pub fn handle(&self, envelope: RequestEnvelope) -> ResponseEnvelope {
        self.handle_for_connection(None, envelope)
            .expect("direct service handle should always have a response")
    }

    pub fn register_connection(&self, sender: mpsc::Sender<ServerMessage>) -> u64 {
        self.connections.register(sender)
    }

    pub fn unregister_connection(&self, connection_id: u64) {
        self.connections.unregister(connection_id);
    }

    pub fn handle_for_connection(
        &self,
        connection_id: Option<u64>,
        envelope: RequestEnvelope,
    ) -> Option<ResponseEnvelope> {
        let id = envelope.id;
        match self.handle_envelope(envelope) {
            Ok((result, initialize_caps, initialized)) => {
                if let (Some(connection_id), Some(capabilities)) = (connection_id, initialize_caps)
                {
                    self.connections
                        .set_capabilities(connection_id, capabilities);
                }
                if let (Some(connection_id), true) = (connection_id, initialized) {
                    self.connections.mark_initialized(connection_id);
                }
                id.map(|id| ResponseEnvelope::success(Some(id), result))
            }
            Err(error) => id.map(|id| ResponseEnvelope::error(Some(id), protocol_error(error))),
        }
    }
}

impl Default for DaemonService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        path::PathBuf,
        process::Command,
        sync::{Mutex, MutexGuard, OnceLock},
        thread,
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;
    use crate::daemon::protocol::{
        ConsistencyMode, EnsureRepoParams, GlobParams, InitializeParams, OpenRepoParams, PathScope,
        QuerySpec, RepoConfig, RepoStatus as RepoStatusPayload, Request, Response, SearchBackend,
        SearchModeConfig, TaskState, TaskStatus as TaskStatusPayload,
    };
    use crate::index_engine::{
        query::{QueryFilters, QueryRequest},
        EngineConfig, IndexEngine, ShardId,
    };
    use crate::SearchMode;

    thread_local! {
        static DAEMON_TEST_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
    }

    const STATUS_WAIT_ATTEMPTS: usize = 400;
    const SEARCH_WAIT_ATTEMPTS: usize = 600;
    const TASK_WAIT_ATTEMPTS: usize = 400;

    enum DaemonTestGuard {
        Primary(MutexGuard<'static, ()>),
        Nested,
    }

    impl Drop for DaemonTestGuard {
        fn drop(&mut self) {
            DAEMON_TEST_LOCK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
            if let Self::Primary(guard) = self {
                let _ = &guard;
            }
        }
    }

    fn daemon_test_serial() -> DaemonTestGuard {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let nested = DAEMON_TEST_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            depth.set(current + 1);
            current > 0
        });
        if nested {
            DaemonTestGuard::Nested
        } else {
            let guard = match LOCK.get_or_init(|| Mutex::new(())).lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            DaemonTestGuard::Primary(guard)
        }
    }

    fn open_repo(service: &DaemonService, repo_path: PathBuf) -> (String, RepoStatusPayload) {
        let (repo_id, status) = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path,
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, status } => (repo_id, status),
            other => panic!("unexpected response: {other:?}"),
        };
        let status = if matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening) {
            wait_for_status(service, &repo_id, |status| {
                !matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening)
            })
        } else {
            status
        };
        (repo_id, status)
    }

    fn ensure_repo(service: &DaemonService, repo_path: PathBuf) -> Response {
        service
            .handle_request_only(Request::EnsureRepo {
                params: EnsureRepoParams {
                    repo_path,
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("ensure repo should succeed")
    }

    fn build_repo(service: &DaemonService, repo_id: &str) {
        service
            .handle_request_only(Request::BuildBaseSnapshot {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.to_string(),
                },
            })
            .expect("build should succeed");
    }

    fn init_git_repo(repo_path: &std::path::Path) {
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
    }

    fn git_commit_all(repo_path: &std::path::Path, message: &str) {
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["commit", "-qm", message])
            .status()
            .expect("git commit should succeed");
    }

    fn query(pattern: &str) -> QuerySpec {
        QuerySpec {
            pattern: pattern.into(),
            patterns: Vec::new(),
            case_insensitive: false,
            multiline: false,
            dot_matches_new_line: false,
            fixed_strings: false,
            word_regexp: false,
            line_regexp: false,
            before_context: 0,
            after_context: 0,
            top_k_tokens: 6,
            max_count: None,
            global_max_results: None,
            search_mode: SearchModeConfig::CountOnly,
        }
    }

    fn search_repo(
        service: &DaemonService,
        repo_id: &str,
        pattern: &str,
        consistency: ConsistencyMode,
    ) -> Response {
        service
            .handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.to_string(),
                    query: query(pattern),
                    scope: PathScope::default(),
                    consistency,
                    allow_scan_fallback: false,
                },
            })
            .expect("search should succeed")
    }

    fn refresh_repo(service: &DaemonService, repo_id: &str, force: bool) -> RepoStatusPayload {
        match service
            .handle_request_only(Request::RefreshRepo {
                params: crate::daemon::protocol::RefreshRepoParams {
                    repo_id: repo_id.to_string(),
                    force,
                },
            })
            .expect("refresh should succeed")
        {
            Response::RepoStatus { status } => status,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn clear_workspace_overlay_engine(service: &DaemonService, repo_id: &str) {
        service
            .repos
            .clear_workspace_overlay_engine_for_tests(repo_id)
            .expect("clear workspace overlay engine should succeed");
    }

    fn has_base_snapshot_engine(service: &DaemonService, repo_id: &str) -> bool {
        service
            .repos
            .has_base_snapshot_engine_for_tests(repo_id)
            .expect("read base snapshot engine slot should succeed")
    }

    fn mark_workspace_probe_pending(service: &DaemonService, repo_id: &str) {
        service
            .repos
            .mark_workspace_probe_pending_for_tests(repo_id)
            .expect("mark workspace probe pending should succeed");
    }

    fn clear_dirty_overlay(service: &DaemonService, repo_id: &str) {
        service
            .repos
            .clear_dirty_overlay_for_tests(repo_id)
            .expect("clear dirty overlay should succeed");
    }

    fn glob_repo(service: &DaemonService, repo_id: &str, scope: PathScope) -> Response {
        service
            .handle_request_only(Request::Glob {
                params: GlobParams {
                    repo_id: repo_id.to_string(),
                    scope,
                },
            })
            .expect("glob should succeed")
    }

    fn get_status(service: &DaemonService, repo_id: &str) -> RepoStatusPayload {
        match service
            .handle_request_only(Request::GetRepoStatus {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.to_string(),
                },
            })
            .expect("status should succeed")
        {
            Response::RepoStatus { status } => status,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn wait_for_status<F>(service: &DaemonService, repo_id: &str, predicate: F) -> RepoStatusPayload
    where
        F: Fn(&RepoStatusPayload) -> bool,
    {
        for _ in 0..STATUS_WAIT_ATTEMPTS {
            let status = get_status(service, repo_id);
            if predicate(&status) {
                return status;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for repo status");
    }

    fn wait_for_search_count(
        service: &DaemonService,
        repo_id: &str,
        pattern: &str,
        consistency: ConsistencyMode,
        expected: usize,
    ) -> (SearchBackend, RepoStatusPayload) {
        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match service.handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.to_string(),
                    query: query(pattern),
                    scope: PathScope::default(),
                    consistency,
                    allow_scan_fallback: false,
                },
            }) {
                Ok(Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                }) if results.matched_lines == expected => return (backend, status),
                Ok(Response::SearchCompleted { .. }) => {}
                Err(crate::error::AppError::InvalidIndex(_)) => {}
                Err(crate::error::AppError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(other) => panic!("unexpected response: {other:?}"),
                Err(error) => panic!("search should succeed: {error:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for search count");
    }

    fn wait_for_search_with_params_count(
        service: &DaemonService,
        repo_id: &str,
        query: QuerySpec,
        roots: Vec<PathBuf>,
        consistency: ConsistencyMode,
        expected: usize,
    ) -> (
        SearchBackend,
        crate::daemon::protocol::SearchResults,
        RepoStatusPayload,
    ) {
        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match service.handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.to_string(),
                    query: query.clone(),
                    scope: PathScope {
                        roots: roots.clone(),
                        ..PathScope::default()
                    },
                    consistency,
                    allow_scan_fallback: false,
                },
            }) {
                Ok(Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                }) if results.matched_lines == expected => return (backend, results, status),
                Ok(Response::SearchCompleted { .. }) => {}
                Err(crate::error::AppError::InvalidIndex(_)) => {}
                Err(crate::error::AppError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(other) => panic!("unexpected response: {other:?}"),
                Err(error) => panic!("search should succeed: {error:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for scoped search count");
    }

    fn wait_for_glob_paths(
        service: &DaemonService,
        repo_id: &str,
        scope: PathScope,
        expected: &[String],
    ) -> RepoStatusPayload {
        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match glob_repo(service, repo_id, scope.clone()) {
                Response::GlobCompleted { paths, status, .. } if paths == expected => {
                    return status;
                }
                Response::GlobCompleted { .. } => {}
                other => panic!("unexpected response: {other:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for glob paths");
    }

    fn wait_for_task_status<F>(
        service: &DaemonService,
        task_id: &str,
        predicate: F,
    ) -> TaskStatusPayload
    where
        F: Fn(&TaskStatusPayload) -> bool,
    {
        for _ in 0..TASK_WAIT_ATTEMPTS {
            let task = match service
                .handle_request_only(Request::TaskStatus {
                    params: crate::daemon::protocol::TaskRef {
                        task_id: task_id.to_string(),
                    },
                })
                .expect("task status should succeed")
            {
                Response::TaskStatus { task } => task,
                other => panic!("unexpected response: {other:?}"),
            };
            if predicate(&task) {
                return task;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out waiting for task status");
    }

    fn workspace_overlay_index_root(repo_path: &std::path::Path) -> PathBuf {
        fs::canonicalize(repo_path)
            .unwrap_or_else(|_| repo_path.to_path_buf())
            .join(".codgrep-index-engine")
            .join("workspace-overlay")
    }

    fn workspace_overlay_match_count(
        repo_path: &std::path::Path,
        pattern: &str,
    ) -> crate::error::Result<usize> {
        let repo_root = fs::canonicalize(repo_path).expect("repo path should canonicalize");
        let engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId(format!("{}#workspace-overlay", repo_root.to_string_lossy())),
            index_root: workspace_overlay_index_root(&repo_root),
            ..EngineConfig::default()
        })?;
        let results = engine.reader().open_snapshot()?.search(&QueryRequest {
            pattern: pattern.into(),
            mode: SearchMode::MaterializeMatches,
            case_insensitive: false,
            top_k: None,
            filters: QueryFilters::default(),
        })?;
        Ok(results.matched_lines)
    }

    fn workspace_overlay_committed_match_count(
        repo_path: &std::path::Path,
        pattern: &str,
    ) -> crate::error::Result<usize> {
        let repo_root = fs::canonicalize(repo_path).expect("repo path should canonicalize");
        let engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId(format!("{}#workspace-overlay", repo_root.to_string_lossy())),
            index_root: workspace_overlay_index_root(&repo_root),
            ..EngineConfig::default()
        })?;
        let results = engine.reader().open_committed()?.search(&QueryRequest {
            pattern: pattern.into(),
            mode: SearchMode::MaterializeMatches,
            case_insensitive: false,
            top_k: None,
            filters: QueryFilters::default(),
        })?;
        Ok(results.matched_lines)
    }

    fn wait_for_workspace_overlay_match_count(
        repo_path: &std::path::Path,
        pattern: &str,
        expected: usize,
    ) {
        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match workspace_overlay_match_count(repo_path, pattern) {
                Ok(count) if count == expected => return,
                Ok(_) => {}
                Err(crate::error::AppError::InvalidIndex(_)) => {}
                Err(crate::error::AppError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("workspace overlay engine should open: {error:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for workspace overlay engine count");
    }

    fn wait_for_workspace_overlay_committed_match_count(
        repo_path: &std::path::Path,
        pattern: &str,
        expected: usize,
    ) {
        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match workspace_overlay_committed_match_count(repo_path, pattern) {
                Ok(count) if count == expected => return,
                Ok(_) => {}
                Err(crate::error::AppError::InvalidIndex(_)) => {}
                Err(crate::error::AppError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("workspace overlay engine should open: {error:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for committed workspace overlay engine count");
    }

    fn assert_exact_dirty_stats(
        status: &RepoStatusPayload,
        modified: usize,
        deleted: usize,
        new: usize,
    ) {
        let exact = status.dirty_files.modified == modified
            && status.dirty_files.deleted == deleted
            && status.dirty_files.new == new;
        assert!(
            exact,
            "expected exact dirty stats, got modified={}, deleted={}, new={}",
            status.dirty_files.modified, status.dirty_files.deleted, status.dirty_files.new
        );
    }

    #[test]
    fn daemon_reports_missing_index_then_builds_and_searches() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"WORKTREE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, status) = open_repo(&service, repo_path.clone());
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
        ));

        let build = service
            .handle_request_only(Request::BuildBaseSnapshot {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.clone(),
                },
            })
            .expect("build should succeed");
        let status = match build {
            Response::RepoBaseSnapshotBuilt { status, .. } => status,
            other => panic!("unexpected response: {other:?}"),
        };
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
        ));

        let search = search_repo(
            &service,
            &repo_id,
            "WORKTREE",
            ConsistencyMode::WorkspaceEventual,
        );

        match search {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(
                    backend,
                    SearchBackend::IndexedClean | SearchBackend::IndexedWorkspaceView
                ));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn snapshot_only_search_uses_base_snapshot_without_dirty_overlay() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE_ONLY\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const VALUE: &str = \"DIRTY_ONLY\";\n")
            .expect("rewrite should succeed");
        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        match search_repo(
            &service,
            &repo_id,
            "BASE_ONLY",
            ConsistencyMode::SnapshotOnly,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedSnapshot));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        match search_repo(
            &service,
            &repo_id,
            "DIRTY_ONLY",
            ConsistencyMode::SnapshotOnly,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedSnapshot));
                assert_eq!(results.matched_lines, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn ready_status_and_search_recover_from_overlay_snapshot_when_dirty_overlay_is_empty() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASELINE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const VALUE: &str = \"SNAPSHOT_DIRTY\";\n")
            .expect("rewrite should succeed");
        let refreshed = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            refreshed.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert_eq!(refreshed.dirty_files.modified, 1);
        wait_for_workspace_overlay_match_count(&repo_path, "SNAPSHOT_DIRTY", 1);

        clear_dirty_overlay(&service, &repo_id);

        let status = get_status(&service, &repo_id);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert_eq!(status.dirty_files.modified, 1);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

        match search_repo(
            &service,
            &repo_id,
            "SNAPSHOT_DIRTY",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                assert_eq!(results.matched_lines, 1);
                assert_eq!(status.dirty_files.modified, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_delta_uses_overlay_snapshot_truth_when_dirty_overlay_mirror_is_empty() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let first = repo_path.join("first.rs");
        let second = repo_path.join("second.rs");
        fs::write(&first, "const VALUE: &str = \"BASE_FIRST\";\n")
            .expect("first write should succeed");
        fs::write(&second, "const VALUE: &str = \"BASE_SECOND\";\n")
            .expect("second write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(
            &first,
            "const VALUE: &str = \"FIRST_DIRTY_TOKEN_20260416\";\n",
        )
        .expect("first rewrite should succeed");
        let refreshed = refresh_repo(&service, &repo_id, true);
        assert_eq!(refreshed.dirty_files.modified, 1);
        wait_for_workspace_overlay_match_count(&repo_path, "FIRST_DIRTY_TOKEN_20260416", 1);

        clear_dirty_overlay(&service, &repo_id);

        fs::write(
            &second,
            "const VALUE: &str = \"SECOND_DIRTY_TOKEN_20260416\";\n",
        )
        .expect("second rewrite should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified >= 2
        });
        assert_eq!(status.dirty_files.modified, 2);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "FIRST_DIRTY_TOKEN_20260416",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
        ));

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "SECOND_DIRTY_TOKEN_20260416",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
        ));
        assert_eq!(status.dirty_files.modified, 2);
    }

    #[test]
    fn snapshot_only_search_can_fallback_to_scan_without_base_index() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"SCAN_ONLY\";\n",
        )
        .expect("write should succeed");

        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
        ));

        match service
            .handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id,
                    query: query("SCAN_ONLY"),
                    scope: PathScope::default(),
                    consistency: ConsistencyMode::SnapshotOnly,
                    allow_scan_fallback: true,
                },
            })
            .expect("search should succeed")
        {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::ScanFallback));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn reopening_repo_reuses_base_snapshot_shard_for_snapshot_search() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"REOPEN_BASE_SHARD\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
        build_repo(&setup_service, &repo_id);
        wait_for_status(&setup_service, &repo_id, |status| {
            matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            )
        });
        setup_service.shutdown_all();

        let service = DaemonService::new();
        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert_exact_dirty_stats(&status, 0, 0, 0);
        assert!(has_base_snapshot_engine(&service, &repo_id));

        match search_repo(
            &service,
            &repo_id,
            "REOPEN_BASE_SHARD",
            ConsistencyMode::SnapshotOnly,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedSnapshot));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn reopening_repo_detects_preexisting_dirty_files_during_open() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"BASELINE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
        build_repo(&setup_service, &repo_id);
        wait_for_status(&setup_service, &repo_id, |status| {
            matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            )
        });
        setup_service.shutdown_all();

        fs::write(
            repo_path.join("added.rs"),
            "const NAME: &str = \"REOPEN_DIRTY_FILE\";\n",
        )
        .expect("write should succeed");

        let service = DaemonService::new();
        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert!(has_base_snapshot_engine(&service, &repo_id));
        assert_exact_dirty_stats(&status, 0, 0, 1);

        let (backend, search_status) = wait_for_search_count(
            &service,
            &repo_id,
            "REOPEN_DIRTY_FILE",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert_eq!(search_status.dirty_files.new, 1);
    }

    #[test]
    fn opening_legacy_snapshot_without_base_shard_is_missing_index() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"LEGACY_ONLY\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
        build_repo(&setup_service, &repo_id);
        wait_for_status(&setup_service, &repo_id, |status| {
            matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            )
        });

        setup_service.shutdown_all();
        fs::remove_dir_all(repo_path.join(".codgrep-index-engine"))
            .expect("removing base shard should succeed");

        let service = DaemonService::new();
        let (_, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
        ));
    }

    #[test]
    fn daemon_ignores_corrupted_legacy_generation_when_base_snapshot_shard_exists() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"WORKTREE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&setup_service, repo_path.clone());
        build_repo(&setup_service, &repo_id);
        let built = wait_for_status(&setup_service, &repo_id, |status| {
            matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyClean
                    | crate::daemon::protocol::RepoPhase::ReadyDirty
            )
        });

        let storage_root = PathBuf::from(&built.storage_root);
        let generation = "legacy-corrupted";
        let layout = storage_root.join("generations").join(generation);
        fs::create_dir_all(&layout).expect("legacy generation dir should exist");
        fs::write(storage_root.join("CURRENT"), format!("{generation}\n"))
            .expect("legacy current should exist");
        fs::write(layout.join("docs.bin"), b"corrupted").expect("corrupt docs should succeed");

        setup_service.shutdown_all();

        let service = DaemonService::new();
        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        match search_repo(
            &service,
            &repo_id,
            "WORKTREE",
            ConsistencyMode::SnapshotOnly,
        ) {
            Response::SearchCompleted { results, .. } => {
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn glob_lists_paths_without_requiring_index_and_respects_scope_filters() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
        fs::create_dir_all(repo_path.join("tests")).expect("repo dir should succeed");
        fs::create_dir_all(repo_path.join(".git")).expect("git dir should succeed");
        fs::write(repo_path.join(".gitignore"), "ignored.rs\n").expect("write should succeed");
        let src = repo_path.join("src/lib.rs");
        let test = repo_path.join("tests/lib.rs");
        fs::write(&src, "pub const NAME: &str = \"SRC\";\n").expect("write should succeed");
        fs::write(&test, "pub const NAME: &str = \"TEST\";\n").expect("write should succeed");
        fs::write(
            repo_path.join("ignored.rs"),
            "pub const NAME: &str = \"IGNORED\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());

        let src = fs::canonicalize(src).expect("canonicalize should succeed");
        let test = fs::canonicalize(test).expect("canonicalize should succeed");

        match glob_repo(
            &service,
            &repo_id,
            PathScope {
                globs: vec!["*.rs".into()],
                types: vec!["rust".into()],
                ..PathScope::default()
            },
        ) {
            Response::GlobCompleted { paths, .. } => {
                assert_eq!(
                    paths,
                    vec![
                        src.to_string_lossy().into_owned(),
                        test.to_string_lossy().into_owned()
                    ]
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }

        match glob_repo(
            &service,
            &repo_id,
            PathScope {
                roots: vec![repo_path.join("src")],
                globs: vec!["*.rs".into()],
                ..PathScope::default()
            },
        ) {
            Response::GlobCompleted { paths, .. } => {
                assert_eq!(paths, vec![src.to_string_lossy().into_owned()]);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn scope_roots_must_stay_within_repo_boundary() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
        fs::write(
            repo_path.join("src/lib.rs"),
            "pub const NAME: &str = \"SRC\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        let outside_root = repo_path
            .parent()
            .expect("repo should have parent")
            .to_path_buf();

        let search_error = service
            .handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.clone(),
                    query: query("SRC"),
                    scope: PathScope {
                        roots: vec![outside_root.clone()],
                        ..PathScope::default()
                    },
                    consistency: ConsistencyMode::WorkspaceEventual,
                    allow_scan_fallback: false,
                },
            })
            .expect_err("search should reject roots outside repo");
        assert!(search_error
            .to_string()
            .contains("scope root escapes repo boundary"));

        let glob_error = service
            .handle_request_only(Request::Glob {
                params: GlobParams {
                    repo_id,
                    scope: PathScope {
                        roots: vec![outside_root],
                        ..PathScope::default()
                    },
                },
            })
            .expect_err("glob should reject roots outside repo");
        assert!(glob_error
            .to_string()
            .contains("scope root escapes repo boundary"));
    }

    #[test]
    fn async_base_snapshot_build_returns_task_and_completes() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
            .expect("write should succeed");

        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::MissingBaseSnapshot
        ));

        let started = service
            .handle_request_only(Request::BaseSnapshotBuild {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.clone(),
                },
            })
            .expect("async build should succeed");
        let task_id = match started {
            Response::TaskStarted { task } => {
                assert_eq!(task.workspace_id, repo_id);
                assert!(matches!(
                    task.state,
                    TaskState::Queued | TaskState::Running | TaskState::Completed
                ));
                task.task_id
            }
            other => panic!("unexpected response: {other:?}"),
        };

        let task = wait_for_task_status(&service, &task_id, |task| {
            matches!(task.state, TaskState::Completed)
        });
        assert!(matches!(
            task.kind,
            crate::daemon::protocol::TaskKind::BuildBaseSnapshot
        ));

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.active_task_id.is_none()
        });
        assert!(status.probe_healthy);

        match search_repo(
            &service,
            &repo_id,
            "HELLO",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted { results, .. } => {
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn eventual_search_uses_fallback_while_repo_is_still_opening_or_missing_index() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
            .expect("write should succeed");

        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, .. } => repo_id,
            other => panic!("unexpected response: {other:?}"),
        };

        match search_repo(
            &service,
            &repo_id,
            "HELLO",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(
                    backend,
                    SearchBackend::RgFallback | SearchBackend::ScanFallback
                ));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn eventual_search_prefers_current_snapshot_while_repo_is_still_opening() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        for index in 0..256 {
            fs::write(
                repo_path.join(format!("file-{index}.rs")),
                format!("const NAME_{index}: &str = \"HELLO\";\n"),
            )
            .expect("write should succeed");
        }

        match ensure_repo(&setup_service, repo_path.clone()) {
            Response::RepoEnsured { status, .. } => {
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                        | crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
                assert!(status.probe_healthy);
            }
            other => panic!("unexpected response: {other:?}"),
        }
        setup_service.shutdown_all();

        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let (repo_id, status) = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, status } => (repo_id, status),
            other => panic!("unexpected response: {other:?}"),
        };
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        match search_repo(
            &service,
            &repo_id,
            "HELLO",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(
                    matches!(backend, SearchBackend::IndexedClean),
                    "workspace eventual search should keep using the current snapshot, got {backend:?}"
                );
                assert_eq!(results.matched_lines, 256);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn eventual_search_keeps_indexed_reads_while_clean_repo_has_pending_reconcile() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const NAME: &str = \"BASE_PENDING\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path);
        build_repo(&service, &repo_id);

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
        });
        assert_exact_dirty_stats(&status, 0, 0, 0);

        mark_workspace_probe_pending(&service, &repo_id);

        match search_repo(
            &service,
            &repo_id,
            "BASE_PENDING",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedClean));
                assert_eq!(results.matched_lines, 1);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                ));
                assert_exact_dirty_stats(&status, 0, 0, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn background_reconcile_uses_git_incremental_recovery() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        init_git_repo(&repo_path);
        fs::write(
            repo_path.join("tracked.rs"),
            "const NAME: &str = \"BASE_GIT_PENDING\";\n",
        )
        .expect("write should succeed");
        git_commit_all(&repo_path, "initial");

        let (repo_id, _) = open_repo(&service, repo_path);
        build_repo(&service, &repo_id);
        let ready = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
        });
        let baseline_probe = ready.last_probe_unix_secs;
        assert_eq!(ready.dirty_files.modified, 0);
        assert_eq!(ready.dirty_files.deleted, 0);
        assert_eq!(ready.dirty_files.new, 0);

        mark_workspace_probe_pending(&service, &repo_id);

        let settled = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.last_probe_unix_secs != baseline_probe
        });
        assert_eq!(settled.dirty_files.modified, 0);
        assert_eq!(settled.dirty_files.deleted, 0);
        assert_eq!(settled.dirty_files.new, 0);
    }

    #[test]
    fn jsonrpc_initialize_and_ping_use_v2_envelope() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();

        let initialize = service.handle(RequestEnvelope {
            jsonrpc: "2.0".into(),
            id: Some(1),
            request: Request::Initialize {
                params: InitializeParams::default(),
            },
        });
        assert_eq!(initialize.jsonrpc, "2.0");
        assert_eq!(initialize.id, Some(1));
        assert!(initialize.error.is_none());
        match initialize.result.expect("initialize result should exist") {
            Response::InitializeResult {
                protocol_version,
                capabilities,
                ..
            } => {
                assert_eq!(protocol_version, 1);
                assert!(capabilities.workspace_open);
                assert!(capabilities.search_query);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let ping = service.handle(RequestEnvelope {
            jsonrpc: "2.0".into(),
            id: Some(2),
            request: Request::Ping,
        });
        assert_eq!(ping.jsonrpc, "2.0");
        assert_eq!(ping.id, Some(2));
        match ping.result.expect("ping result should exist") {
            Response::Pong { now_unix_secs } => {
                assert!(now_unix_secs > 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn ensure_repo_builds_missing_index_once() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("main.rs"),
            "const NAME: &str = \"WORKTREE\";\n",
        )
        .expect("write should succeed");

        match ensure_repo(&service, repo_path.clone()) {
            Response::RepoEnsured {
                repo_id,
                indexed_docs,
                status,
            } => {
                assert_eq!(
                    repo_id,
                    fs::canonicalize(&repo_path)
                        .expect("repo should canonicalize")
                        .to_string_lossy()
                );
                assert_eq!(indexed_docs, Some(1));
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                        | crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        match ensure_repo(&service, repo_path) {
            Response::RepoEnsured {
                indexed_docs,
                status,
                ..
            } => {
                assert_eq!(indexed_docs, None);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                        | crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn workspace_strict_search_uses_explicitly_refreshed_state() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const NAME: &str = \"DIRTY\";\n").expect("rewrite should succeed");

        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        for _ in 0..SEARCH_WAIT_ATTEMPTS {
            match service.handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.to_string(),
                    query: query("DIRTY"),
                    scope: PathScope::default(),
                    consistency: ConsistencyMode::WorkspaceStrict,
                    allow_scan_fallback: false,
                },
            }) {
                Ok(Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                }) => {
                    assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                    assert_eq!(results.matched_lines, 1);
                    assert!(matches!(
                        status.phase,
                        crate::daemon::protocol::RepoPhase::ReadyDirty
                    ));
                    return;
                }
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }
        panic!("timed out waiting for strict workspace search");
    }

    #[test]
    fn refresh_workspace_rebuilds_workspace_overlay_engine() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const NAME: &str = \"DIRTY_OVERLAY_ENGINE\";\n")
            .expect("rewrite should succeed");

        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        wait_for_workspace_overlay_match_count(&repo_path, "DIRTY_OVERLAY_ENGINE", 1);
    }

    #[test]
    fn watcher_marks_dirty_paths_for_eventual_workspace_search() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const NAME: &str = \"DIRTY\";\n").expect("rewrite should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified == 1
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, 1, 0, 0);

        let search = search_repo(
            &service,
            &repo_id,
            "DIRTY",
            ConsistencyMode::WorkspaceEventual,
        );

        match search {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                assert_eq!(results.matched_lines, 1);
                assert_eq!(status.dirty_files.modified, 1);
                assert_eq!(status.dirty_files.deleted, 0);
                assert_eq!(status.dirty_files.new, 0);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn eventual_search_serves_base_snapshot_while_overlay_engine_recovers() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const NAME: &str = \"DIRTY_FALLBACK\";\n")
            .expect("rewrite should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified == 1
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, 1, 0, 0);

        clear_workspace_overlay_engine(&service, &repo_id);

        match search_repo(
            &service,
            &repo_id,
            "DIRTY_FALLBACK",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                assert_eq!(results.matched_lines, 0);
                assert_eq!(status.dirty_files.modified, 1);
                assert_eq!(status.dirty_files.deleted, 0);
                assert_eq!(status.dirty_files.new, 0);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY_FALLBACK",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert_exact_dirty_stats(&status, 1, 0, 0);
    }

    #[test]
    fn eventual_search_rebuilds_full_overlay_after_overlay_engine_reset() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let first = repo_path.join("first.rs");
        let second = repo_path.join("second.rs");
        fs::write(&first, "const NAME: &str = \"BASE_FIRST\";\n").expect("write should succeed");
        fs::write(&second, "const NAME: &str = \"BASE_SECOND\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("rewrite should succeed");
        fs::write(&second, "const NAME: &str = \"DIRTY_SECOND\";\n")
            .expect("rewrite should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified == 2
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, 2, 0, 0);

        clear_workspace_overlay_engine(&service, &repo_id);

        fs::write(&first, "const NAME: &str = \"DIRTY_FIRST_AGAIN\";\n")
            .expect("rewrite should succeed");

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY_SECOND",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY_FIRST_AGAIN",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert!(status.dirty_files.modified >= 1);
    }

    #[test]
    fn eventual_search_honors_configured_overlay_auto_checkpoint_threshold() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");

        let (repo_id, _) = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: crate::daemon::protocol::RefreshPolicyConfig {
                        rebuild_dirty_threshold: 256,
                        overlay_auto_checkpoint_max_uncommitted_ops: 1,
                        ..crate::daemon::protocol::RefreshPolicyConfig::default()
                    },
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, status } => {
                let status = if matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening)
                {
                    wait_for_status(&service, &repo_id, |status| {
                        !matches!(status.phase, crate::daemon::protocol::RepoPhase::Opening)
                    })
                } else {
                    status
                };
                (repo_id, status)
            }
            other => panic!("unexpected response: {other:?}"),
        };
        build_repo(&service, &repo_id);

        let first = repo_path.join("first.rs");
        fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("write should succeed");

        let _ = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY_FIRST",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        wait_for_workspace_overlay_match_count(&repo_path, "DIRTY_FIRST", 1);
        wait_for_workspace_overlay_committed_match_count(&repo_path, "DIRTY_FIRST", 1);
    }

    #[test]
    fn repo_status_reports_overlay_runtime_stats() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");

        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: crate::daemon::protocol::RefreshPolicyConfig {
                        rebuild_dirty_threshold: 256,
                        overlay_auto_checkpoint_max_uncommitted_ops: 1,
                        ..crate::daemon::protocol::RefreshPolicyConfig::default()
                    },
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, .. } => repo_id,
            other => panic!("unexpected response: {other:?}"),
        };
        build_repo(&service, &repo_id);

        let first = repo_path.join("first.rs");
        fs::write(&first, "const NAME: &str = \"DIRTY_FIRST\";\n").expect("write should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            status.overlay.as_ref().is_some_and(|overlay| {
                overlay.active_segments >= 1 && overlay.committed_seq_no >= 1
            })
        });
        let overlay = status.overlay.expect("overlay stats should be present");
        assert!(overlay.active_segments >= 1);
        assert_eq!(overlay.active_delete_segments, 0);
        assert_eq!(overlay.uncommitted_ops, 0);
        assert!(overlay.merge_attempts >= overlay.merge_completed);
        assert_eq!(overlay.merge_failed, 0);
        assert_eq!(overlay.last_merge_error, None);
    }

    #[test]
    fn overlay_merge_is_deferred_and_runs_in_background() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");

        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: crate::daemon::protocol::RefreshPolicyConfig {
                        rebuild_dirty_threshold: 256,
                        overlay_auto_checkpoint_max_uncommitted_ops: 1,
                        overlay_merge_min_delay_ms: 500,
                        overlay_merge_retry_delay_ms: 100,
                    },
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, .. } => repo_id,
            other => panic!("unexpected response: {other:?}"),
        };
        build_repo(&service, &repo_id);

        let first = repo_path.join("first.rs");
        fs::write(&first, "const NAME: &str = \"DIRTY_MERGE_DELAY\";\n")
            .expect("write should succeed");

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY_MERGE_DELAY",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert!(status
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.committed_seq_no >= 1));

        let requested = wait_for_status(&service, &repo_id, |status| {
            status.overlay.as_ref().is_some_and(|overlay| {
                overlay.active_segments >= 1
                    && overlay.committed_seq_no >= 1
                    && overlay.merge_requested
                    && !overlay.merge_running
            })
        });
        assert!(requested
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.merge_requested));

        let settled = wait_for_status(&service, &repo_id, |status| {
            status.overlay.as_ref().is_some_and(|overlay| {
                overlay.active_segments >= 1
                    && overlay.committed_seq_no >= 1
                    && !overlay.merge_requested
                    && !overlay.merge_running
            })
        });
        assert!(settled
            .overlay
            .as_ref()
            .is_some_and(|overlay| !overlay.merge_requested));
    }

    #[test]
    fn watcher_reconciles_batched_tracked_file_updates() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");

        let dirty_count = 96usize;
        for index in 0..dirty_count {
            fs::write(
                repo_path.join(format!("tracked_{index:03}.rs")),
                "const NAME: &str = \"BASE\";\n",
            )
            .expect("write should succeed");
        }

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        for index in 0..dirty_count {
            fs::write(
                repo_path.join(format!("tracked_{index:03}.rs")),
                "const NAME: &str = \"DIRTY\";\n",
            )
            .expect("rewrite should succeed");
        }

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified == dirty_count
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, dirty_count, 0, 0);

        let (backend, search_status) = wait_for_search_count(
            &service,
            &repo_id,
            "DIRTY",
            ConsistencyMode::WorkspaceEventual,
            dirty_count,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert_eq!(search_status.dirty_files.modified, dirty_count);
    }

    #[test]
    fn watcher_tracks_new_text_files() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const NAME: &str = \"BASE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(
            repo_path.join("added.rs"),
            "const NAME: &str = \"ADDED\";\n",
        )
        .expect("write should succeed");

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "ADDED",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert_exact_dirty_stats(&status, 0, 0, 1);
    }

    #[test]
    fn watcher_tracks_new_text_files_inside_new_directory_without_probe() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const NAME: &str = \"BASE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        let nested = repo_path.join("nested");
        fs::create_dir_all(&nested).expect("nested dir should succeed");
        fs::write(
            nested.join("added.rs"),
            "const NAME: &str = \"NESTED_ADDED\";\n",
        )
        .expect("nested write should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified == 0
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 1
        });
        assert_exact_dirty_stats(&status, 0, 0, 1);

        match search_repo(
            &service,
            &repo_id,
            "NESTED_ADDED",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_tracks_deleted_files() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let deleted = repo_path.join("deleted.rs");
        fs::write(&deleted, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::remove_file(&deleted).expect("remove should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.deleted == 1
        });
        assert_eq!(status.dirty_files.modified, 0);
        assert_eq!(status.dirty_files.new, 0);

        match search_repo(
            &service,
            &repo_id,
            "BASE",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
                assert_eq!(results.matched_lines, 0);
                assert_eq!(status.dirty_files.deleted, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn reopened_daemon_materializes_fixed_string_phrase_from_dirty_overlay() {
        let _serial = daemon_test_serial();
        let setup_service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let cpp = repo_path.join("tracked.cpp");
        let header = repo_path.join("tracked.h");
        fs::write(
            &cpp,
            "/// VersionTuple baseline line.\nconst char *Name = \"VersionTuple\";\n",
        )
        .expect("write should succeed");
        fs::write(
            &header,
            "class VersionTuple {\n  /// VersionTuple baseline line.\n};\n",
        )
        .expect("write should succeed");

        match ensure_repo(&setup_service, repo_path.clone()) {
            Response::RepoEnsured { status, .. } => {
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }
        setup_service.shutdown_all();
        drop(setup_service);

        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    storage_root: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("open repo should succeed")
        {
            Response::RepoOpened { repo_id, .. } => repo_id,
            other => panic!("unexpected response: {other:?}"),
        };

        let phrase = "VersionTuple watcher validation content gamma";
        fs::write(
            &cpp,
            "/// VersionTuple baseline line.\n/// VersionTuple watcher edit baseline.\nconst char *Name = \"VersionTuple\";\n",
        )
        .expect("write should succeed");
        fs::write(
            &header,
            "class VersionTuple {\n  /// VersionTuple watcher validation content gamma.\n  /// VersionTuple baseline line.\n};\n",
        )
        .expect("write should succeed");

        let status = match service
            .handle_request_only(Request::RefreshRepo {
                params: crate::daemon::protocol::RefreshRepoParams {
                    repo_id: repo_id.clone(),
                    force: true,
                },
            })
            .expect("refresh should succeed")
        {
            Response::RepoStatus { status } => status,
            other => panic!("unexpected response: {other:?}"),
        };
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));
        assert!(status.probe_healthy);

        let phrase_query = QuerySpec {
            pattern: regex::escape(phrase),
            patterns: vec![phrase.into()],
            fixed_strings: true,
            search_mode: SearchModeConfig::MaterializeMatches,
            ..query(phrase)
        };
        let (backend, results, _) = wait_for_search_with_params_count(
            &service,
            &repo_id,
            phrase_query,
            vec![header],
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
        ));
        assert!(!results.hits.is_empty());
        assert!(results
            .hits
            .iter()
            .any(|hit| hit.path.ends_with("tracked.h")));
        assert!(results
            .hits
            .iter()
            .flat_map(|hit| hit.matches.iter())
            .any(|matched| matched
                .snippet
                .contains("VersionTuple watcher validation content gamma")));
    }

    #[test]
    fn dirty_overlay_uses_latest_contents_after_multiple_edits() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const VALUE: &str = \"FIRST_EDIT\";\n")
            .expect("first rewrite should succeed");
        let _ = refresh_repo(&service, &repo_id, true);
        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
        });
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "FIRST_EDIT",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));

        fs::write(&tracked, "const VALUE: &str = \"SECOND_EDIT\";\n")
            .expect("second rewrite should succeed");
        let _ = refresh_repo(&service, &repo_id, true);
        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
        });
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let _ = wait_for_search_count(
            &service,
            &repo_id,
            "FIRST_EDIT",
            ConsistencyMode::WorkspaceEventual,
            0,
        );

        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "SECOND_EDIT",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
        ));
    }

    #[test]
    fn repeated_dirty_overlay_searches_remain_stable() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(
            &tracked,
            "const VALUE: &str = \"REPEATED_DIRTY_TOKEN_20260410\";\n",
        )
        .expect("rewrite should succeed");
        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        for _ in 0..3 {
            let (backend, _) = wait_for_search_count(
                &service,
                &repo_id,
                "REPEATED_DIRTY_TOKEN_20260410",
                ConsistencyMode::WorkspaceEventual,
                1,
            );
            assert!(matches!(
                backend,
                SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
            ));
        }
    }

    #[test]
    fn mixed_incremental_sequence_remains_searchable() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(
            &tracked,
            "const VALUE: &str = \"MIXED_EXISTING_TOKEN_20260410\";\n",
        )
        .expect("rewrite should succeed");
        let status = refresh_repo(&service, &repo_id, true);
        assert!(status.dirty_files.modified >= 1);

        let _ = wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_EXISTING_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );

        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("revert should succeed");
        refresh_repo(&service, &repo_id, true);
        wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_EXISTING_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            0,
        );

        let nested = repo_path.join("nested");
        fs::create_dir_all(&nested).expect("nested dir should succeed");
        fs::write(
            nested.join("live.rs"),
            "const VALUE: &str = \"MIXED_NEWDIR_TOKEN_20260410\";\n",
        )
        .expect("nested write should succeed");
        refresh_repo(&service, &repo_id, true);

        let (_, status) = wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_NEWDIR_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        for _ in 0..3 {
            let (_, status) = wait_for_search_count(
                &service,
                &repo_id,
                "MIXED_NEWDIR_TOKEN_20260410",
                ConsistencyMode::WorkspaceEventual,
                1,
            );
            assert!(matches!(
                status.phase,
                crate::daemon::protocol::RepoPhase::ReadyDirty
            ));
        }
    }

    #[test]
    fn workspace_eventual_search_does_not_run_dirty_diff_observation() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("write should succeed");

        let observation_path = temp.path().join("dirty-diff.tsv");
        unsafe {
            std::env::set_var("CODGREP_OBSERVE_DIRTY_DIFF_FILE", &observation_path);
        }

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "BASE",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedClean));
        assert!(!observation_path.exists());

        unsafe {
            std::env::remove_var("CODGREP_OBSERVE_DIRTY_DIFF_FILE");
        }
    }

    #[test]
    fn path_scoped_search_limits_dirty_overlay_results() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let left = repo_path.join("left.rs");
        let right = repo_path.join("right.rs");
        fs::write(&left, "const VALUE: &str = \"BASE\";\n").expect("left write should succeed");
        fs::write(&right, "const VALUE: &str = \"BASE\";\n").expect("right write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&left, "const VALUE: &str = \"SCOPED_TOKEN\";\n")
            .expect("left rewrite should succeed");
        fs::write(&right, "const VALUE: &str = \"SCOPED_TOKEN\";\n")
            .expect("right rewrite should succeed");
        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let query = QuerySpec {
            search_mode: SearchModeConfig::MaterializeMatches,
            ..query("SCOPED_TOKEN")
        };
        match wait_for_search_with_params_count(
            &service,
            &repo_id,
            query,
            vec![left.clone()],
            ConsistencyMode::WorkspaceEventual,
            1,
        ) {
            (backend, results, _) => {
                assert!(matches!(
                    backend,
                    SearchBackend::IndexedWorkspaceView | SearchBackend::IndexedClean
                ));
                assert_eq!(results.hits.len(), 1);
                assert!(results.hits[0].path.ends_with("left.rs"));
                assert!(results.hits[0]
                    .matches
                    .iter()
                    .any(|matched| matched.snippet.contains("SCOPED_TOKEN")));
            }
        }
    }

    #[test]
    fn transient_new_file_does_not_leave_phantom_match_after_delete() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const VALUE: &str = \"BASE\";\n",
        )
        .expect("tracked write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        let transient = repo_path.join("transient.rs");
        fs::write(&transient, "const VALUE: &str = \"EPHEMERAL_TOKEN\";\n")
            .expect("transient write should succeed");
        fs::remove_file(&transient).expect("transient delete should succeed");

        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        match search_repo(
            &service,
            &repo_id,
            "EPHEMERAL_TOKEN",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted { results, .. } => {
                assert_eq!(results.matched_lines, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_removes_overlay_only_new_file_without_refresh() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const VALUE: &str = \"BASE\";\n",
        )
        .expect("tracked write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        let nested = repo_path.join("nested");
        fs::create_dir_all(&nested).expect("nested dir should succeed");
        let transient = nested.join("transient.rs");
        fs::write(
            &transient,
            "const VALUE: &str = \"WATCHER_DELETE_TOKEN_20260410\";\n",
        )
        .expect("transient write should succeed");
        wait_for_workspace_overlay_match_count(&repo_path, "WATCHER_DELETE_TOKEN_20260410", 1);

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "WATCHER_DELETE_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        fs::remove_file(&transient).expect("transient delete should succeed");
        fs::remove_dir(&nested).expect("nested dir delete should succeed");
        wait_for_workspace_overlay_match_count(&repo_path, "WATCHER_DELETE_TOKEN_20260410", 0);

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "WATCHER_DELETE_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            0,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedClean
                | SearchBackend::IndexedWorkspaceView
                | SearchBackend::RgFallback
                | SearchBackend::ScanFallback
        ));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::Opening
                | crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let status = wait_for_status(&service, &repo_id, |status| {
            status.dirty_files.modified == 0
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, 0, 0, 0);

        for _ in 0..3 {
            match search_repo(
                &service,
                &repo_id,
                "WATCHER_DELETE_TOKEN_20260410",
                ConsistencyMode::WorkspaceEventual,
            ) {
                Response::SearchCompleted { results, .. } => {
                    assert_eq!(results.matched_lines, 0);
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }

    #[test]
    fn watcher_clears_restored_tracked_file_without_refresh() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n")
            .expect("tracked write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(
            &tracked,
            "const VALUE: &str = \"WATCHER_RESTORE_TOKEN_20260416\";\n",
        )
        .expect("tracked rewrite should succeed");
        wait_for_workspace_overlay_match_count(&repo_path, "WATCHER_RESTORE_TOKEN_20260416", 1);

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "WATCHER_RESTORE_TOKEN_20260416",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceView));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n")
            .expect("tracked restore should succeed");
        wait_for_workspace_overlay_match_count(&repo_path, "WATCHER_RESTORE_TOKEN_20260416", 0);
        let repo_status = get_status(&service, &repo_id);
        let build_config = crate::BuildConfig {
            repo_path: repo_path.clone(),
            storage_root: PathBuf::from(&repo_status.storage_root),
            tokenizer: crate::TokenizerMode::SparseNgram,
            corpus_mode: crate::CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 8,
        };
        let metadata =
            crate::index_engine::WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&build_config)
                .expect("read base snapshot metadata should succeed")
                .expect("base snapshot metadata should exist");
        let base_snapshot = crate::index_engine::WorkspaceBaseSnapshotIndex::open_if_matching(
            &build_config,
            &metadata.snapshot_key,
        )
        .expect("open base snapshot index should succeed")
        .expect("base snapshot index should match")
        .open_committed()
        .expect("open committed base snapshot should succeed");
        let base_catalog = base_snapshot
            .path_catalog()
            .expect("path catalog should load");
        let tracked_entry = base_catalog
            .get("tracked.rs")
            .expect("tracked entry should exist");
        let tracked_bytes = fs::read(&tracked).expect("tracked bytes should load");
        let mut tracked_hash = [0u8; 32];
        let tracked_crc = crc32fast::hash(&tracked_bytes);
        tracked_hash[..4].copy_from_slice(&tracked_crc.to_le_bytes());
        assert_eq!(tracked_entry.content_hash, tracked_hash);
        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "WATCHER_RESTORE_TOKEN_20260416",
            ConsistencyMode::WorkspaceEventual,
            0,
        );
        assert!(matches!(
            backend,
            SearchBackend::IndexedClean
                | SearchBackend::IndexedWorkspaceView
                | SearchBackend::RgFallback
                | SearchBackend::ScanFallback
        ));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::Opening
                | crate::daemon::protocol::RepoPhase::ReadyClean
                | crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        let status = wait_for_status(&service, &repo_id, |status| {
            status.dirty_files.modified == 0
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_exact_dirty_stats(&status, 0, 0, 0);
    }

    #[test]
    fn watcher_keeps_indexed_reads_while_directory_events_wait_for_reconcile() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(
            repo_path.join("tracked.rs"),
            "const NAME: &str = \"BASE\";\n",
        )
        .expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::create_dir_all(repo_path.join("nested")).expect("mkdir should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.dirty_files.modified == 0
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
        });
        assert_eq!(status.dirty_files.modified, 0);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

        match search_repo(
            &service,
            &repo_id,
            "BASE",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedClean));
                assert_eq!(results.matched_lines, 1);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let refreshed = service
            .handle_request_only(Request::RefreshRepo {
                params: crate::daemon::protocol::RefreshRepoParams {
                    repo_id,
                    force: false,
                },
            })
            .expect("refresh should succeed");

        match refreshed {
            Response::RepoStatus { status } => {
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyClean
                ));
                assert_eq!(status.dirty_files.modified, 0);
                assert_eq!(status.dirty_files.deleted, 0);
                assert_eq!(status.dirty_files.new, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_updates_invalidate_glob_cache() {
        let _serial = daemon_test_serial();
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(repo_path.join("src")).expect("repo dir should succeed");
        let stable = repo_path.join("src/lib.rs");
        fs::write(&stable, "pub const NAME: &str = \"STABLE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);
        let stable = fs::canonicalize(&stable)
            .expect("canonicalize should succeed")
            .to_string_lossy()
            .into_owned();
        let scope = PathScope {
            globs: vec!["*.rs".into()],
            ..PathScope::default()
        };

        match glob_repo(&service, &repo_id, scope.clone()) {
            Response::GlobCompleted { paths, .. } => {
                assert_eq!(paths, vec![stable.clone()]);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let fresh = repo_path.join("src/fresh.rs");
        fs::write(&fresh, "pub const NAME: &str = \"FRESH\";\n").expect("write should succeed");
        let fresh = fs::canonicalize(&fresh)
            .expect("canonicalize should succeed")
            .to_string_lossy()
            .into_owned();

        let status = wait_for_glob_paths(
            &service,
            &repo_id,
            scope.clone(),
            &[fresh.clone(), stable.clone()],
        );
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::Opening
                | crate::daemon::protocol::RepoPhase::ReadyDirty
                | crate::daemon::protocol::RepoPhase::ReadyClean
        ));

        fs::remove_file(repo_path.join("src/fresh.rs")).expect("delete should succeed");
        let status = wait_for_glob_paths(&service, &repo_id, scope, &[stable.clone()]);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::Opening
                | crate::daemon::protocol::RepoPhase::ReadyDirty
                | crate::daemon::protocol::RepoPhase::ReadyClean
        ));
    }
}
