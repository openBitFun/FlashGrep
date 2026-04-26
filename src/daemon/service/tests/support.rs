use std::{
    cell::Cell,
    fs,
    path::PathBuf,
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
    time::Duration,
};

use super::*;
use crate::daemon::protocol::{
    ConsistencyMode, EnsureRepoParams, GlobParams, OpenRepoParams, PathScope, QuerySpec,
    RepoConfig, RepoStatus as RepoStatusPayload, Request, Response, SearchBackend,
    SearchModeConfig, TaskStatus as TaskStatusPayload,
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
pub(super) const SEARCH_WAIT_ATTEMPTS: usize = 600;
const TASK_WAIT_ATTEMPTS: usize = 400;

pub(super) enum DaemonTestGuard {
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

pub(super) fn daemon_test_serial() -> DaemonTestGuard {
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

pub(super) fn open_repo(
    service: &DaemonService,
    repo_path: PathBuf,
) -> (String, RepoStatusPayload) {
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

pub(super) fn ensure_repo(service: &DaemonService, repo_path: PathBuf) -> Response {
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

pub(super) fn build_repo(service: &DaemonService, repo_id: &str) {
    service
        .handle_request_only(Request::BuildBaseSnapshot {
            params: crate::daemon::protocol::RepoRef {
                repo_id: repo_id.to_string(),
            },
        })
        .expect("build should succeed");
}

pub(super) fn init_git_repo(repo_path: &std::path::Path) {
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

pub(super) fn git_commit_all(repo_path: &std::path::Path, message: &str) {
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

pub(super) fn git_current_branch(repo_path: &std::path::Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["branch", "--show-current"])
        .output()
        .expect("git branch should succeed");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("branch output should decode")
        .trim()
        .to_string()
}

pub(super) fn git_checkout(repo_path: &std::path::Path, target: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["checkout", "-q", target])
        .status()
        .expect("git checkout should succeed");
    assert!(status.success(), "git checkout {target} should succeed");
}

pub(super) fn git_checkout_new_branch(repo_path: &std::path::Path, branch: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["checkout", "-q", "-b", branch])
        .status()
        .expect("git checkout -b should succeed");
    assert!(status.success(), "git checkout -b {branch} should succeed");
}

pub(super) fn query(pattern: &str) -> QuerySpec {
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

pub(super) fn search_repo(
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

pub(super) fn refresh_repo(
    service: &DaemonService,
    repo_id: &str,
    force: bool,
) -> RepoStatusPayload {
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

pub(super) fn clear_workspace_overlay_engine(service: &DaemonService, repo_id: &str) {
    service
        .repos
        .clear_workspace_overlay_engine_for_tests(repo_id)
        .expect("clear workspace overlay engine should succeed");
}

pub(super) fn is_transition_or_indexed_workspace_backend(backend: SearchBackend) -> bool {
    matches!(
        backend,
        SearchBackend::IndexedWorkspaceView
            | SearchBackend::RgFallback
            | SearchBackend::ScanFallback
    )
}

pub(super) fn has_base_snapshot_engine(service: &DaemonService, repo_id: &str) -> bool {
    service
        .repos
        .has_base_snapshot_engine_for_tests(repo_id)
        .expect("read base snapshot engine slot should succeed")
}

pub(super) fn mark_workspace_probe_pending(service: &DaemonService, repo_id: &str) {
    service
        .repos
        .mark_workspace_probe_pending_for_tests(repo_id)
        .expect("mark workspace probe pending should succeed");
}

pub(super) fn clear_published_overlay_paths_cache(service: &DaemonService, repo_id: &str) {
    service
        .repos
        .clear_published_overlay_paths_cache_for_tests(repo_id)
        .expect("clear published overlay paths cache should succeed");
}

pub(super) fn glob_repo(service: &DaemonService, repo_id: &str, scope: PathScope) -> Response {
    service
        .handle_request_only(Request::Glob {
            params: GlobParams {
                repo_id: repo_id.to_string(),
                scope,
            },
        })
        .expect("glob should succeed")
}

pub(super) fn get_status(service: &DaemonService, repo_id: &str) -> RepoStatusPayload {
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

pub(super) fn wait_for_status<F>(
    service: &DaemonService,
    repo_id: &str,
    predicate: F,
) -> RepoStatusPayload
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

pub(super) fn wait_for_search_count(
    service: &DaemonService,
    repo_id: &str,
    pattern: &str,
    consistency: ConsistencyMode,
    expected: usize,
) -> (SearchBackend, RepoStatusPayload) {
    let mut last_observed = None;
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
            Ok(Response::SearchCompleted {
                backend,
                results,
                status,
                ..
            }) => {
                last_observed = Some((backend, results.matched_lines, status));
            }
            Err(crate::error::AppError::InvalidIndex(_)) => {}
            Err(crate::error::AppError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(other) => panic!("unexpected response: {other:?}"),
            Err(error) => panic!("search should succeed: {error:?}"),
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "timed out waiting for search count for pattern {pattern} expecting {expected}; last observed: {last_observed:?}"
    );
}

pub(super) fn assert_search_count_now(
    service: &DaemonService,
    repo_id: &str,
    pattern: &str,
    consistency: ConsistencyMode,
    expected: usize,
) -> (SearchBackend, RepoStatusPayload) {
    match search_repo(service, repo_id, pattern, consistency) {
        Response::SearchCompleted {
            backend,
            results,
            status,
            ..
        } => {
            assert_eq!(
                results.matched_lines, expected,
                "unexpected immediate search count for pattern {pattern}"
            );
            (backend, status)
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

pub(super) fn wait_for_search_with_params_count(
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

pub(super) fn wait_for_glob_paths(
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

pub(super) fn wait_for_task_status<F>(
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

pub(super) fn workspace_overlay_index_root(repo_path: &std::path::Path) -> PathBuf {
    fs::canonicalize(repo_path)
        .unwrap_or_else(|_| repo_path.to_path_buf())
        .join(".codgrep-index-engine")
        .join("workspace-overlay")
}

pub(super) fn workspace_overlay_match_count(
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
        prefer_worktree_for_git_head_blob: false,
    })?;
    Ok(results.matched_lines)
}

pub(super) fn workspace_overlay_committed_match_count(
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
        prefer_worktree_for_git_head_blob: false,
    })?;
    Ok(results.matched_lines)
}

pub(super) fn wait_for_workspace_overlay_match_count(
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

pub(super) fn wait_for_workspace_overlay_committed_match_count(
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

pub(super) fn assert_exact_dirty_stats(
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
