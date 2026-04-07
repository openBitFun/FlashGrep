use std::{path::PathBuf, sync::Arc};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    path_filter::PathFilterArgs,
    search::SearchMode,
};

use super::{
    protocol::{
        ConsistencyMode, DirtyFileStats, EnsureRepoParams, ErrorResponse, QuerySpec,
        RepoStatus as RepoStatusPayload, Request, RequestEnvelope, Response, ResponseEnvelope,
        ResponsePayload,
    },
    repo::{
        build_config_from_open, convert_consistency, convert_query_backend, convert_repo_phase,
        convert_search_results, system_time_to_unix_secs, OpenRepoRequest, RepoManager,
        RepoRefreshPolicy, SearchConsistency, SearchRequest,
    },
};

#[derive(Clone, Default)]
pub struct DaemonService {
    repos: Arc<RepoManager>,
}

impl DaemonService {
    pub fn new() -> Self {
        Self {
            repos: Arc::new(RepoManager::new()),
        }
    }

    pub fn shutdown_all(&self) {
        self.repos.shutdown_all();
    }

    pub fn handle(&self, envelope: RequestEnvelope) -> ResponseEnvelope {
        let id = envelope.id;
        match self.handle_request(envelope.request) {
            Ok(result) => ResponseEnvelope {
                id,
                payload: ResponsePayload::Ok { result },
            },
            Err(error) => ResponseEnvelope {
                id,
                payload: ResponsePayload::Error {
                    error: ErrorResponse {
                        message: error.to_string(),
                    },
                },
            },
        }
    }

    fn handle_request(&self, request: Request) -> Result<Response> {
        match request {
            Request::OpenRepo { params } => {
                let build_config =
                    build_config_from_request(params.repo_path, params.index_path, params.config);
                let status = self.repos.open_repo(OpenRepoRequest {
                    build_config,
                    refresh_policy: RepoRefreshPolicy {
                        rebuild_dirty_threshold: params.refresh.rebuild_dirty_threshold,
                    },
                })?;
                Ok(Response::RepoOpened {
                    repo_id: status.repo_id.clone(),
                    status: convert_repo_status(status),
                })
            }
            Request::EnsureRepo { params } => self.ensure_repo(params),
            Request::GetRepoStatus { params } => Ok(Response::RepoStatus {
                status: convert_repo_status(self.repos.get_status(&params.repo_id)?),
            }),
            Request::RefreshRepo { params } => Ok(Response::RepoStatus {
                status: convert_repo_status(
                    self.repos.refresh_repo(&params.repo_id, params.force)?,
                ),
            }),
            Request::BuildIndex { params } => {
                let (indexed_docs, status) = self.repos.build_index(&params.repo_id)?;
                Ok(Response::RepoBuilt {
                    indexed_docs,
                    status: convert_repo_status(status),
                })
            }
            Request::RebuildIndex { params } => {
                let (indexed_docs, status) = self.repos.rebuild_index(&params.repo_id)?;
                Ok(Response::RepoRebuilt {
                    indexed_docs,
                    status: convert_repo_status(status),
                })
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
                Ok(Response::SearchCompleted {
                    repo_id: response.repo_id,
                    backend: convert_query_backend(response.backend),
                    consistency_applied: convert_consistency(response.consistency_applied),
                    status: convert_repo_status(response.status),
                    results: convert_search_results(response.results),
                })
            }
            Request::CloseRepo { params } => {
                self.repos.close_repo(&params.repo_id)?;
                Ok(Response::RepoClosed {
                    repo_id: params.repo_id,
                })
            }
            Request::Shutdown => Ok(Response::ShutdownAck),
        }
    }

    fn ensure_repo(&self, params: EnsureRepoParams) -> Result<Response> {
        let build_config =
            build_config_from_request(params.repo_path, params.index_path, params.config);
        let repo_id = build_config
            .normalized()?
            .repo_path
            .to_string_lossy()
            .into_owned();
        let status = self.repos.open_repo(OpenRepoRequest {
            build_config,
            refresh_policy: RepoRefreshPolicy {
                rebuild_dirty_threshold: params.refresh.rebuild_dirty_threshold,
            },
        })?;

        let (indexed_docs, status) = match status.phase {
            crate::daemon::repo::RepoPhase::MissingIndex => {
                let (indexed_docs, status) = self.repos.build_index(&repo_id)?;
                (Some(indexed_docs), status)
            }
            _ => (None, status),
        };

        Ok(Response::RepoEnsured {
            repo_id,
            status: convert_repo_status(status),
            indexed_docs,
        })
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

fn convert_consistency_mode(mode: ConsistencyMode) -> SearchConsistency {
    match mode {
        ConsistencyMode::SnapshotOnly => SearchConsistency::SnapshotOnly,
        ConsistencyMode::WorkspaceEventual => SearchConsistency::WorkspaceEventual,
        ConsistencyMode::WorkspaceStrict => SearchConsistency::WorkspaceStrict,
    }
}

fn convert_repo_status(status: crate::daemon::repo::RepoStatus) -> RepoStatusPayload {
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
        last_error: status.last_error,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use tempfile::tempdir;

    use super::*;
    use crate::daemon::protocol::{
        EnsureRepoParams, OpenRepoParams, PathScope, RepoConfig, Request, SearchBackend,
        SearchModeConfig,
    };

    fn open_repo(service: &DaemonService, repo_path: PathBuf) -> (String, RepoStatusPayload) {
        match service
            .handle_request(Request::OpenRepo {
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
        }
    }

    fn ensure_repo(service: &DaemonService, repo_path: PathBuf) -> Response {
        service
            .handle_request(Request::EnsureRepo {
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
            .handle_request(Request::BuildIndex {
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
            .handle_request(Request::Search {
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

    fn get_status(service: &DaemonService, repo_id: &str) -> RepoStatusPayload {
        match service
            .handle_request(Request::GetRepoStatus {
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
            .handle_request(Request::BuildIndex {
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
        });
        assert_eq!(status.dirty_files.deleted, 0);

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
        });
        assert_eq!(status.dirty_files.modified, 0);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

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

        fs::create_dir_all(repo_path.join("nested")).expect("mkdir should succeed");

        let status = wait_for_status(&service, &repo_id, |status| {
            matches!(status.phase, crate::daemon::protocol::RepoPhase::ReadyDirty)
        });
        assert_eq!(status.dirty_files.modified, 0);
        assert_eq!(status.dirty_files.deleted, 0);
        assert_eq!(status.dirty_files.new, 0);

        let refreshed = service
            .handle_request(Request::RefreshRepo {
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
