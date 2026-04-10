use std::{
    collections::HashMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    sync::mpsc,
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    path_filter::PathFilterArgs,
    search::SearchMode,
};

use super::{
    protocol::{
        ClientCapabilities, ConsistencyMode, DirtyFileStats, EnsureRepoParams, ErrorResponse,
        Notification, NotificationEnvelope, ProgressNotificationParams, QuerySpec,
        RepoStatus as RepoStatusPayload, Request, RequestEnvelope, Response, ResponseEnvelope,
        SearchProtocolCapabilities, ServerCapabilities, ServerInfo, ServerMessage,
        TaskFinishedParams, WorkspaceStatusChangedParams,
    },
    repo::{
        build_config_from_open, convert_consistency, convert_query_backend, convert_repo_phase,
        convert_search_results, convert_task_status, system_time_to_unix_secs, OpenRepoRequest,
        RepoManager, RepoRefreshPolicy, RepoTaskStatus, SearchConsistency, SearchRequest,
    },
};

#[derive(Clone)]
pub struct DaemonService {
    repos: Arc<RepoManager>,
    connections: Arc<ConnectionRegistry>,
}

#[derive(Default)]
struct ConnectionRegistry {
    next_id: AtomicU64,
    connections: std::sync::Mutex<HashMap<u64, ConnectionState>>,
}

#[derive(Clone)]
struct ConnectionState {
    sender: mpsc::Sender<ServerMessage>,
    capabilities: ClientCapabilities,
    initialized: bool,
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

    fn handle_envelope(
        &self,
        envelope: RequestEnvelope,
    ) -> Result<(Response, Option<ClientCapabilities>, bool)> {
        if envelope.jsonrpc != "2.0" {
            return Err(AppError::Protocol(format!(
                "unsupported jsonrpc version: {}",
                envelope.jsonrpc
            )));
        }
        self.handle_request(envelope.request)
    }

    #[allow(dead_code)]
    fn handle_request_only(&self, request: Request) -> Result<Response> {
        self.handle_request(request)
            .map(|(response, _, _)| response)
    }

    fn handle_request(
        &self,
        request: Request,
    ) -> Result<(Response, Option<ClientCapabilities>, bool)> {
        match request {
            Request::Initialize { params } => {
                let caps = params.capabilities.clone();
                Ok((
                    Response::InitializeResult {
                        protocol_version: 1,
                        server_info: ServerInfo {
                            name: "codgrep".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                        },
                        capabilities: build_server_capabilities(params.capabilities),
                        search: SearchProtocolCapabilities {
                            consistency_modes: vec![
                                ConsistencyMode::SnapshotOnly,
                                ConsistencyMode::WorkspaceEventual,
                                ConsistencyMode::WorkspaceStrict,
                            ],
                            search_modes: vec![
                                super::protocol::SearchModeConfig::CountOnly,
                                super::protocol::SearchModeConfig::CountMatches,
                                super::protocol::SearchModeConfig::FirstHitOnly,
                                super::protocol::SearchModeConfig::MaterializeMatches,
                            ],
                        },
                    },
                    Some(caps),
                    false,
                ))
            }
            Request::Initialized => Ok((Response::InitializedAck, None, true)),
            Request::Ping => Ok((
                Response::Pong {
                    now_unix_secs: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                },
                None,
                false,
            )),
            Request::IndexBuild { params } => Ok((
                Response::TaskStarted {
                    task: convert_task_status(self.repos.start_build_index(&params.repo_id)?),
                },
                None,
                false,
            )),
            Request::IndexRebuild { params } => Ok((
                Response::TaskStarted {
                    task: convert_task_status(self.repos.start_rebuild_index(&params.repo_id)?),
                },
                None,
                false,
            )),
            Request::TaskStatus { params } => Ok((
                Response::TaskStatus {
                    task: convert_task_status(self.repos.get_task_status(&params.task_id)?),
                },
                None,
                false,
            )),
            Request::TaskCancel { params } => Ok((
                Response::TaskCancelled {
                    task_id: params.task_id.clone(),
                    accepted: self.repos.cancel_task(&params.task_id)?,
                },
                None,
                false,
            )),
            Request::OpenRepo { params } => {
                let build_config =
                    build_config_from_request(params.repo_path, params.index_path, params.config);
                let status = self.repos.open_repo(OpenRepoRequest {
                    build_config,
                    refresh_policy: RepoRefreshPolicy {
                        rebuild_dirty_threshold: params.refresh.rebuild_dirty_threshold,
                    },
                })?;
                Ok((
                    Response::RepoOpened {
                        repo_id: status.repo_id.clone(),
                        status: convert_repo_status(status),
                    },
                    None,
                    false,
                ))
            }
            Request::EnsureRepo { params } => self.ensure_repo(params),
            Request::GetRepoStatus { params } => Ok((
                Response::RepoStatus {
                    status: convert_repo_status(self.repos.get_status(&params.repo_id)?),
                },
                None,
                false,
            )),
            Request::RefreshRepo { params } => Ok((
                Response::RepoStatus {
                    status: convert_repo_status(
                        self.repos.refresh_repo(&params.repo_id, params.force)?,
                    ),
                },
                None,
                false,
            )),
            Request::BuildIndex { params } => {
                let (indexed_docs, status) = self.repos.build_index(&params.repo_id)?;
                Ok((
                    Response::RepoBuilt {
                        indexed_docs,
                        status: convert_repo_status(status),
                    },
                    None,
                    false,
                ))
            }
            Request::RebuildIndex { params } => {
                let (indexed_docs, status) = self.repos.rebuild_index(&params.repo_id)?;
                Ok((
                    Response::RepoRebuilt {
                        indexed_docs,
                        status: convert_repo_status(status),
                    },
                    None,
                    false,
                ))
            }
            Request::Search { params } => {
                let query = convert_query_spec(params.query)?;
                let scope = PathFilterArgs {
                    roots: normalize_scope_roots(params.scope.roots),
                    globs: params.scope.globs,
                    iglobs: params.scope.iglobs,
                    type_add: params.scope.type_add,
                    type_clear: params.scope.type_clear,
                    types: params.scope.types,
                    type_not: params.scope.type_not,
                };
                let response = self.repos.search(SearchRequest {
                    repo_id: params.repo_id,
                    query,
                    path_filter_args: scope,
                    consistency: convert_consistency_mode(params.consistency),
                    allow_scan_fallback: params.allow_scan_fallback,
                })?;
                Ok((
                    Response::SearchCompleted {
                        repo_id: response.repo_id,
                        backend: convert_query_backend(response.backend),
                        consistency_applied: convert_consistency(response.consistency_applied),
                        status: convert_repo_status(response.status),
                        results: convert_search_results(response.results),
                    },
                    None,
                    false,
                ))
            }
            Request::CloseRepo { params } => {
                self.repos.close_repo(&params.repo_id)?;
                Ok((
                    Response::RepoClosed {
                        repo_id: params.repo_id,
                    },
                    None,
                    false,
                ))
            }
            Request::Shutdown => Ok((Response::ShutdownAck, None, false)),
            Request::Exit => Ok((Response::ExitAck, None, false)),
        }
    }

    fn ensure_repo(
        &self,
        params: EnsureRepoParams,
    ) -> Result<(Response, Option<ClientCapabilities>, bool)> {
        let build_config =
            build_config_from_request(params.repo_path, params.index_path, params.config);
        let repo_id = build_config
            .normalized()?
            .repo_path
            .to_string_lossy()
            .into_owned();
        let mut status = self.repos.open_repo(OpenRepoRequest {
            build_config,
            refresh_policy: RepoRefreshPolicy {
                rebuild_dirty_threshold: params.refresh.rebuild_dirty_threshold,
            },
        })?;
        if matches!(status.phase, crate::daemon::repo::RepoPhase::Opening) {
            status = wait_for_repo_open(&self.repos, &repo_id)?;
        }

        let (indexed_docs, status) = match status.phase {
            crate::daemon::repo::RepoPhase::MissingIndex => {
                let (indexed_docs, status) = self.repos.build_index(&repo_id)?;
                (Some(indexed_docs), status)
            }
            _ => (None, status),
        };

        Ok((
            Response::RepoEnsured {
                repo_id,
                status: convert_repo_status(status),
                indexed_docs,
            },
            None,
            false,
        ))
    }
}

impl Default for DaemonService {
    fn default() -> Self {
        Self::new()
    }
}

fn wait_for_repo_open(
    repos: &RepoManager,
    repo_id: &str,
) -> Result<crate::daemon::repo::RepoStatus> {
    for _ in 0..400 {
        let status = repos.get_status(repo_id)?;
        if !matches!(status.phase, crate::daemon::repo::RepoPhase::Opening) {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err(AppError::Protocol(format!(
        "timed out waiting for repo {repo_id} to finish opening"
    )))
}

impl ConnectionRegistry {
    fn register(&self, sender: mpsc::Sender<ServerMessage>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut connections = match self.connections.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        connections.insert(
            id,
            ConnectionState {
                sender,
                capabilities: ClientCapabilities::default(),
                initialized: false,
            },
        );
        id
    }

    fn unregister(&self, connection_id: u64) {
        let mut connections = match self.connections.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        connections.remove(&connection_id);
    }

    fn set_capabilities(&self, connection_id: u64, capabilities: ClientCapabilities) {
        let mut connections = match self.connections.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(connection) = connections.get_mut(&connection_id) {
            connection.capabilities = capabilities;
        }
    }

    fn mark_initialized(&self, connection_id: u64) {
        let mut connections = match self.connections.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(connection) = connections.get_mut(&connection_id) {
            connection.initialized = true;
        }
    }

    fn broadcast(&self, event: ServiceNotificationEvent) {
        let notification = match notification_for_event(event) {
            Some(notification) => notification,
            None => return,
        };
        let recipients = {
            let connections = match self.connections.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            connections
                .values()
                .filter(|connection| should_send_notification(connection, &notification))
                .map(|connection| connection.sender.clone())
                .collect::<Vec<_>>()
        };
        for sender in recipients {
            let _ = sender.send(ServerMessage::Notification(NotificationEnvelope::new(
                notification.clone(),
            )));
        }
    }
}

fn should_send_notification(connection: &ConnectionState, notification: &Notification) -> bool {
    if !connection.initialized {
        return false;
    }
    match notification {
        Notification::Progress { .. } => connection.capabilities.progress,
        Notification::WorkspaceStatusChanged { .. } => connection.capabilities.status_notifications,
        Notification::TaskFinished { .. } => connection.capabilities.task_notifications,
    }
}

#[derive(Debug, Clone)]
pub enum ServiceNotificationEvent {
    TaskProgress(RepoTaskStatus),
    WorkspaceStatus(RepoStatusPayload),
    TaskFinished(RepoTaskStatus),
}

fn notification_for_event(event: ServiceNotificationEvent) -> Option<Notification> {
    match event {
        ServiceNotificationEvent::TaskProgress(task) => Some(Notification::Progress {
            params: ProgressNotificationParams {
                task_id: task.task_id,
                workspace_id: task.repo_id,
                kind: super::repo::convert_task_kind(task.kind),
                phase: task.phase.map(super::repo::convert_task_phase),
                message: task.message,
                processed: task.processed,
                total: task.total,
            },
        }),
        ServiceNotificationEvent::WorkspaceStatus(status) => {
            Some(Notification::WorkspaceStatusChanged {
                params: WorkspaceStatusChangedParams {
                    workspace_id: status.repo_id.clone(),
                    status,
                },
            })
        }
        ServiceNotificationEvent::TaskFinished(task) => Some(Notification::TaskFinished {
            params: TaskFinishedParams {
                task: convert_task_status(task),
            },
        }),
    }
}

fn build_config_from_request(
    repo_path: PathBuf,
    index_path: Option<PathBuf>,
    config: super::protocol::RepoConfig,
) -> crate::config::BuildConfig {
    build_config_from_open(
        repo_path,
        index_path,
        config.tokenizer,
        config.corpus_mode,
        config.include_hidden,
        config.max_file_size,
        config.min_sparse_len,
        config.max_sparse_len,
    )
}

fn convert_query_spec(query: QuerySpec) -> Result<QueryConfig> {
    if query.pattern.trim().is_empty() {
        return Err(AppError::InvalidPattern(
            "query pattern cannot be empty".into(),
        ));
    }
    let patterns = if query.patterns.is_empty() {
        vec![query.pattern.clone()]
    } else {
        query.patterns
    };
    Ok(QueryConfig {
        regex_pattern: query.pattern,
        patterns,
        case_insensitive: query.case_insensitive,
        multiline: query.multiline,
        dot_matches_new_line: query.dot_matches_new_line,
        fixed_strings: query.fixed_strings,
        word_regexp: query.word_regexp,
        line_regexp: query.line_regexp,
        before_context: query.before_context,
        after_context: query.after_context,
        top_k_tokens: query.top_k_tokens,
        max_count: query.max_count,
        search_mode: match query.search_mode {
            super::protocol::SearchModeConfig::CountOnly => SearchMode::CountOnly,
            super::protocol::SearchModeConfig::CountMatches => SearchMode::CountMatches,
            super::protocol::SearchModeConfig::FirstHitOnly => SearchMode::FirstHitOnly,
            super::protocol::SearchModeConfig::MaterializeMatches => SearchMode::MaterializeMatches,
        },
    })
}

fn normalize_scope_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots
}

fn build_server_capabilities(_client: ClientCapabilities) -> ServerCapabilities {
    ServerCapabilities {
        workspace_open: true,
        workspace_ensure: true,
        workspace_list: false,
        workspace_refresh: true,
        index_build: true,
        index_rebuild: true,
        task_status: true,
        task_cancel: true,
        search_query: true,
        progress_notifications: true,
        status_notifications: true,
    }
}

fn protocol_error(error: AppError) -> ErrorResponse {
    ErrorResponse {
        code: -32001,
        message: error.to_string(),
        data: None,
    }
}

fn convert_consistency_mode(mode: ConsistencyMode) -> SearchConsistency {
    match mode {
        ConsistencyMode::SnapshotOnly => SearchConsistency::SnapshotOnly,
        ConsistencyMode::WorkspaceEventual => SearchConsistency::WorkspaceEventual,
        ConsistencyMode::WorkspaceStrict => SearchConsistency::WorkspaceStrict,
    }
}

pub(super) fn convert_repo_status(status: crate::daemon::repo::RepoStatus) -> RepoStatusPayload {
    RepoStatusPayload {
        repo_id: status.repo_id,
        repo_path: status.repo_path.to_string_lossy().into_owned(),
        index_path: status.index_path.to_string_lossy().into_owned(),
        phase: convert_repo_phase(status.phase),
        snapshot_key: status.snapshot_key,
        last_probe_unix_secs: system_time_to_unix_secs(status.last_probe_at),
        last_rebuild_unix_secs: system_time_to_unix_secs(status.last_rebuild_at),
        dirty_files: DirtyFileStats {
            modified: status.dirty_files.modified,
            deleted: status.dirty_files.deleted,
            new: status.dirty_files.new,
        },
        rebuild_recommended: status.rebuild_recommended,
        active_task_id: status.active_task_id,
        watcher_healthy: status.watcher_healthy,
        last_error: status.last_error,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use tempfile::tempdir;

    use super::*;
    use crate::daemon::protocol::{
        EnsureRepoParams, InitializeParams, OpenRepoParams, PathScope, RepoConfig, Request,
        SearchBackend, SearchModeConfig, TaskState, TaskStatus as TaskStatusPayload,
    };

    fn open_repo(service: &DaemonService, repo_path: PathBuf) -> (String, RepoStatusPayload) {
        let (repo_id, status) = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path,
                    index_path: None,
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
                    index_path: None,
                    config: RepoConfig::default(),
                    refresh: Default::default(),
                },
            })
            .expect("ensure repo should succeed")
    }

    fn build_repo(service: &DaemonService, repo_id: &str) {
        service
            .handle_request_only(Request::BuildIndex {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.to_string(),
                },
            })
            .expect("build should succeed");
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

    fn search_repo_with_params(
        service: &DaemonService,
        repo_id: &str,
        query: QuerySpec,
        roots: Vec<PathBuf>,
        consistency: ConsistencyMode,
    ) -> Response {
        service
            .handle_request_only(Request::Search {
                params: crate::daemon::protocol::SearchParams {
                    repo_id: repo_id.to_string(),
                    query,
                    scope: PathScope {
                        roots,
                        ..PathScope::default()
                    },
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
        for _ in 0..200 {
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
        for _ in 0..200 {
            match search_repo(service, repo_id, pattern, consistency) {
                Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                } if results.matched_lines == expected => return (backend, status),
                Response::SearchCompleted { .. } => {}
                other => panic!("unexpected response: {other:?}"),
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for search count");
    }

    fn wait_for_task_status<F>(
        service: &DaemonService,
        task_id: &str,
        predicate: F,
    ) -> TaskStatusPayload
    where
        F: Fn(&TaskStatusPayload) -> bool,
    {
        for _ in 0..200 {
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
            crate::daemon::protocol::RepoPhase::MissingIndex
        ));

        let build = service
            .handle_request_only(Request::BuildIndex {
                params: crate::daemon::protocol::RepoRef {
                    repo_id: repo_id.clone(),
                },
            })
            .expect("build should succeed");
        let status = match build {
            Response::RepoBuilt { status, .. } => status,
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
                    SearchBackend::IndexedClean | SearchBackend::IndexedWorkspaceRepair
                ));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn async_index_build_returns_task_and_completes() {
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        fs::write(repo_path.join("main.rs"), "const NAME: &str = \"HELLO\";\n")
            .expect("write should succeed");

        let (repo_id, status) = open_repo(&service, repo_path);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::MissingIndex
        ));

        let started = service
            .handle_request_only(Request::IndexBuild {
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
            crate::daemon::protocol::TaskKind::BuildIndex
        ));

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.active_task_id.is_none()
        });
        assert!(status.watcher_healthy);

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
                    index_path: None,
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
    fn eventual_search_avoids_stale_snapshot_while_repo_is_still_opening() {
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
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let service = DaemonService::new();
        let (repo_id, status) = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    index_path: None,
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
            crate::daemon::protocol::RepoPhase::Opening
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
                    matches!(backend, SearchBackend::RgFallback | SearchBackend::ScanFallback),
                    "transition search should avoid stale snapshot, got {backend:?}"
                );
                assert_eq!(results.matched_lines, 256);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn jsonrpc_initialize_and_ping_use_v2_envelope() {
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
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn workspace_strict_search_repairs_dirty_worktree() {
        let service = DaemonService::new();
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let tracked = repo_path.join("tracked.rs");
        fs::write(&tracked, "const NAME: &str = \"BASE\";\n").expect("write should succeed");

        let (repo_id, _) = open_repo(&service, repo_path.clone());
        build_repo(&service, &repo_id);

        fs::write(&tracked, "const NAME: &str = \"DIRTY\";\n").expect("rewrite should succeed");

        let search = search_repo(
            &service,
            &repo_id,
            "DIRTY",
            ConsistencyMode::WorkspaceStrict,
        );

        match search {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
                assert!(matches!(
                    status.phase,
                    crate::daemon::protocol::RepoPhase::ReadyDirty
                ));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_marks_dirty_paths_for_eventual_workspace_search() {
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
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
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
    fn watcher_tracks_new_text_files() {
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
            "ADDED",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
                assert_eq!(status.dirty_files.new, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_tracks_new_text_files_inside_new_directory_without_probe() {
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

        let baseline_probe = get_status(&service, &repo_id).last_probe_unix_secs;
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
        assert_eq!(status.last_probe_unix_secs, baseline_probe);

        match search_repo(
            &service,
            &repo_id,
            "NESTED_ADDED",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
                assert_eq!(status.last_probe_unix_secs, baseline_probe);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn watcher_tracks_deleted_files() {
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
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 0);
                assert_eq!(status.dirty_files.deleted, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn reopened_daemon_materializes_fixed_string_phrase_from_dirty_overlay() {
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

        let service = DaemonService::new();
        let repo_id = match service
            .handle_request_only(Request::OpenRepo {
                params: OpenRepoParams {
                    repo_path: repo_path.clone(),
                    index_path: None,
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
        assert!(status.watcher_healthy);

        let phrase_query = QuerySpec {
            pattern: regex::escape(phrase),
            patterns: vec![phrase.into()],
            fixed_strings: true,
            search_mode: SearchModeConfig::MaterializeMatches,
            ..query(phrase)
        };
        match search_repo_with_params(
            &service,
            &repo_id,
            phrase_query,
            vec![header],
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(
                    backend,
                    SearchBackend::IndexedWorkspaceRepair | SearchBackend::IndexedClean
                ));
                assert!(results.matched_lines >= 1);
                assert!(!results.hits.is_empty());
                assert!(results.hits.iter().any(|hit| hit.path.ends_with("tracked.h")));
                assert!(
                    results
                        .hits
                        .iter()
                        .flat_map(|hit| hit.matches.iter())
                        .any(|matched| matched
                            .snippet
                            .contains("VersionTuple watcher validation content gamma"))
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn dirty_overlay_uses_latest_contents_after_multiple_edits() {
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
        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        match search_repo(
            &service,
            &repo_id,
            "FIRST_EDIT",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        fs::write(&tracked, "const VALUE: &str = \"SECOND_EDIT\";\n")
            .expect("second rewrite should succeed");
        let status = refresh_repo(&service, &repo_id, true);
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        match search_repo(
            &service,
            &repo_id,
            "FIRST_EDIT",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted { results, .. } => {
                assert_eq!(results.matched_lines, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }

        match search_repo(
            &service,
            &repo_id,
            "SECOND_EDIT",
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn repeated_dirty_overlay_searches_remain_stable() {
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
            match search_repo(
                &service,
                &repo_id,
                "REPEATED_DIRTY_TOKEN_20260410",
                ConsistencyMode::WorkspaceEventual,
            ) {
                Response::SearchCompleted {
                    backend, results, ..
                } => {
                    assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                    assert_eq!(results.matched_lines, 1);
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }

    #[test]
    fn mixed_incremental_sequence_remains_searchable() {
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
        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
                && status.dirty_files.modified >= 1
        });
        assert!(status.dirty_files.modified >= 1);

        let (backend, _) = wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_EXISTING_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));

        fs::write(&tracked, "const VALUE: &str = \"BASE\";\n").expect("revert should succeed");
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

        let (backend, status) = wait_for_search_count(
            &service,
            &repo_id,
            "MIXED_NEWDIR_TOKEN_20260410",
            ConsistencyMode::WorkspaceEventual,
            1,
        );
        assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
        assert!(matches!(
            status.phase,
            crate::daemon::protocol::RepoPhase::ReadyDirty
        ));

        for _ in 0..3 {
            match search_repo(
                &service,
                &repo_id,
                "MIXED_NEWDIR_TOKEN_20260410",
                ConsistencyMode::WorkspaceEventual,
            ) {
                Response::SearchCompleted {
                    backend,
                    results,
                    status,
                    ..
                } => {
                    assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                    assert_eq!(results.matched_lines, 1);
                    assert!(matches!(
                        status.phase,
                        crate::daemon::protocol::RepoPhase::ReadyDirty
                    ));
                }
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }

    #[test]
    fn path_scoped_search_limits_dirty_overlay_results() {
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
        match search_repo_with_params(
            &service,
            &repo_id,
            query,
            vec![left.clone()],
            ConsistencyMode::WorkspaceEventual,
        ) {
            Response::SearchCompleted {
                backend, results, ..
            } => {
                assert!(matches!(backend, SearchBackend::IndexedWorkspaceRepair));
                assert_eq!(results.matched_lines, 1);
                assert_eq!(results.hits.len(), 1);
                assert!(results.hits[0].path.ends_with("left.rs"));
                assert!(
                    results.hits[0]
                        .matches
                        .iter()
                        .any(|matched| matched.snippet.contains("SCOPED_TOKEN"))
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn transient_new_file_does_not_leave_phantom_match_after_delete() {
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
    fn watcher_marks_directory_events_for_resync() {
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

        let baseline_probe = get_status(&service, &repo_id).last_probe_unix_secs;
        thread::sleep(Duration::from_secs(1));

        fs::create_dir_all(repo_path.join("nested")).expect("mkdir should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyClean)
                && status.dirty_files.modified == 0
                && status.dirty_files.deleted == 0
                && status.dirty_files.new == 0
                && status.last_probe_unix_secs != baseline_probe
        });
        assert_eq!(status.dirty_files.modified, 0);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

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
}
