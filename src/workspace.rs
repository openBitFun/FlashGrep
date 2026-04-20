#[path = "workspace/base.rs"]
mod base;
#[path = "workspace/freshness.rs"]
mod freshness;
mod runtime;

use std::{
    collections::BTreeSet,
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use crate::{
    base_snapshot_build::{BaseSnapshotBuildMode, BaseSnapshotBuildOptions},
    config::BuildConfig,
    error::{AppError, Result},
    index_engine::{
        query::{QueryFilters, QueryRequest},
        reader::IndexSnapshotReader,
        WorkspaceBaseSnapshotIndex, WorkspaceSearchView,
    },
    path_filter::PathFilter,
    search::SearchResults,
    search_engine::{search_documents, SearchBackend, SearchResponse},
    worktree_diff::IndexWorktreeDiff,
    TokenizerMode,
};

use self::runtime::{
    build_overlay_snapshot_state, diff_against_base_snapshot, merge_search_results,
    reconcile_dirty_paths_against_current_diff, scan_text_repository_files,
};

/// Options for opening a [`WorkspaceIndex`].
///
/// This facade exposes the current workspace view over a stable base snapshot.
#[derive(Debug, Clone)]
pub struct WorkspaceIndexOptions {
    /// Build-time/index-location configuration for the workspace.
    pub build_config: BuildConfig,
}

impl From<BuildConfig> for WorkspaceIndexOptions {
    fn from(build_config: BuildConfig) -> Self {
        Self { build_config }
    }
}

/// Identity and metadata for the current base snapshot.
#[derive(Debug, Clone)]
pub struct BaseSnapshotInfo {
    /// Stable cache key for the base snapshot.
    pub snapshot_key: String,
    /// Root directory that stores base snapshot index data.
    pub base_snapshot_root: PathBuf,
    /// Repository root for this workspace.
    pub repo_path: PathBuf,
    /// Tokenizer used by the base snapshot.
    pub tokenizer: TokenizerMode,
    /// Number of indexed documents in the base snapshot.
    pub doc_count: usize,
    /// Kind of base snapshot identity currently in use.
    pub snapshot_kind: BaseSnapshotKind,
    /// Git `HEAD` commit used for the snapshot when available.
    pub head_commit: Option<String>,
    /// Fingerprint of the build configuration used to materialize the snapshot.
    pub config_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSnapshotKind {
    GitCommit,
    RepositoryFallback,
    Legacy,
}

/// Current observable workspace state.
#[derive(Debug, Clone)]
pub struct IndexStatus {
    /// Metadata for the current base snapshot, if one exists on disk.
    pub base: Option<BaseSnapshotInfo>,
    /// Worktree diff against the base snapshot, if the base index can be opened.
    pub dirty_files: Option<IndexWorktreeDiff>,
}

/// High-level freshness state for the currently opened workspace index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceFreshnessState {
    MissingBaseSnapshot,
    Fresh,
    Stale,
}

/// Structured freshness probe result for external schedulers/orchestrators.
#[derive(Debug, Clone)]
pub struct WorkspaceFreshness {
    /// Probe capture time.
    pub checked_at: SystemTime,
    /// High-level freshness state.
    pub state: WorkspaceFreshnessState,
    /// Active base snapshot key, when available.
    pub base_snapshot_key: Option<String>,
    /// Number of tracked files changed since base snapshot.
    pub modified_files: usize,
    /// Number of tracked files deleted since base snapshot.
    pub deleted_files: usize,
    /// Number of new files missing from base snapshot.
    pub new_files: usize,
}

impl WorkspaceFreshness {
    pub fn is_stale(&self) -> bool {
        self.state == WorkspaceFreshnessState::Stale
    }

    pub fn needs_base_snapshot(&self) -> bool {
        self.state == WorkspaceFreshnessState::MissingBaseSnapshot
    }

    pub fn dirty_file_count(&self) -> usize {
        self.modified_files + self.deleted_files + self.new_files
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceSearchTimings {
    pub diff: Duration,
    pub overlay_build: Duration,
    pub base_search: Duration,
    pub dirty_search: Duration,
    pub merge: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSearchProfile {
    pub diff: IndexWorktreeDiff,
    pub shadowed_base_paths: usize,
    pub dirty_documents: usize,
    pub base_candidate_docs: usize,
    pub base_matched_lines: usize,
    pub dirty_candidate_docs: usize,
    pub dirty_matched_lines: usize,
    pub final_candidate_docs: usize,
    pub final_matched_lines: usize,
    pub timings: WorkspaceSearchTimings,
}

/// High-level workspace facade for base snapshot management and searching
/// against the current effective repository view.
#[derive(Clone)]
pub struct WorkspaceIndex {
    inner: Arc<WorkspaceIndexInner>,
}

/// Read-only workspace query view pinned to one base snapshot and one overlay snapshot.
#[derive(Clone)]
pub struct WorkspaceSnapshot {
    build_config: BuildConfig,
    base_snapshot_key: String,
    base_snapshot: IndexSnapshotReader,
    overlay_snapshot: Option<IndexSnapshotReader>,
}

struct WorkspaceIndexInner {
    options: WorkspaceIndexOptions,
    cached_freshness: Mutex<Option<CachedFreshnessProbe>>,
}

#[derive(Clone)]
struct CachedFreshnessProbe {
    captured_at: Instant,
    probe: WorkspaceFreshness,
}

impl WorkspaceIndex {
    /// Opens a workspace facade over the configured repository and index root.
    pub fn open(options: WorkspaceIndexOptions) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(WorkspaceIndexInner {
                options: WorkspaceIndexOptions {
                    build_config: options.build_config.normalized()?,
                },
                cached_freshness: Mutex::new(None),
            }),
        })
    }

    /// Ensures that a base snapshot exists and returns its identity metadata.
    pub fn ensure_base_snapshot(&self) -> Result<BaseSnapshotInfo> {
        self.ensure_base_snapshot_with_options(BaseSnapshotBuildOptions::default())
    }

    pub fn ensure_base_snapshot_with_options(
        &self,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<BaseSnapshotInfo> {
        self.clear_freshness_probe_cache();
        let _ = WorkspaceBaseSnapshotIndex::build_with_options(
            &self.inner.options.build_config,
            &self.repo_id(),
            BaseSnapshotBuildOptions {
                build_mode: BaseSnapshotBuildMode::ForceRebuildSnapshot,
                ..options
            },
        )?;
        self.base_snapshot_info()
    }

    /// Reports the current base snapshot and worktree diff state.
    pub fn status(&self) -> Result<IndexStatus> {
        let base = self.base_snapshot_info().ok();
        let dirty_files = match self.diff_against_worktree() {
            Ok(diff) => Some(diff),
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(IndexStatus { base, dirty_files })
    }

    /// Performs a full freshness probe against the current worktree.
    pub fn probe_freshness(&self) -> Result<WorkspaceFreshness> {
        let probe = self.compute_freshness_probe()?;
        self.cache_freshness_probe(probe.clone());
        Ok(probe)
    }

    /// Returns a cached freshness probe when still within `ttl`; otherwise runs
    /// a new full probe.
    pub fn probe_freshness_if_due(&self, ttl: Duration) -> Result<WorkspaceFreshness> {
        if !ttl.is_zero() {
            let cached = match self.inner.cached_freshness.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            if let Some(cached) = cached {
                if cached.captured_at.elapsed() < ttl {
                    return Ok(cached.probe);
                }
            }
        }

        self.probe_freshness()
    }

    /// Searches the current effective workspace view.
    ///
    /// This resolves the current worktree diff on demand, materializes the
    /// workspace overlay snapshot, and searches `base snapshot + overlay snapshot`.
    pub fn search(&self, query: &crate::QueryConfig) -> Result<SearchResults> {
        self.search_with_filter(query, None)
    }

    /// Searches the current effective workspace view with an optional path filter.
    pub fn search_with_filter(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        match self.open_base_snapshot_reader() {
            Ok(base_snapshot) => {
                let dirty = self.diff_against_worktree()?;
                let search_view = self.workspace_search_view()?;
                let overlay_snapshot = self.materialize_overlay_snapshot_for_diff(&dirty)?;
                search_workspace_view(
                    &search_view,
                    &base_snapshot,
                    overlay_snapshot.as_ref(),
                    query,
                    filter,
                )
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                self.scan_search_with_filter(query, filter)
            }
            Err(error) => Err(error),
        }
    }

    pub fn profile_search_with_filter(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<WorkspaceSearchProfile> {
        let total_started = Instant::now();
        let base_snapshot = self.open_base_snapshot_reader()?;
        let search_view = self.workspace_search_view()?;

        let diff_started = Instant::now();
        let diff = self.diff_against_worktree()?;
        let diff_elapsed = diff_started.elapsed();

        let overlay_started = Instant::now();
        let overlay_snapshot = self.materialize_overlay_snapshot_for_diff(&diff)?;
        let overlay_elapsed = overlay_started.elapsed();

        let workspace_snapshot = search_view
            .open_workspace_query_snapshot(Some(base_snapshot.clone()), overlay_snapshot.clone())?;

        let base_started = Instant::now();
        let base_results = search_view.search_base_snapshot(
            query,
            &QueryFilters::default(),
            filter,
            workspace_snapshot.shadowed_base_paths(),
            workspace_snapshot.base_snapshot(),
        )?;
        let base_elapsed = base_started.elapsed();

        let dirty_started = Instant::now();
        let dirty_results = match query_with_remaining_global_limit(query, &base_results) {
            Some(dirty_query) if workspace_snapshot.has_overlay_snapshot() => search_view
                .search_overlay_snapshot(
                    &dirty_query,
                    &QueryFilters::default(),
                    filter,
                    workspace_snapshot.overlay_snapshot(),
                )?,
            _ => empty_search_results(),
        };
        let dirty_elapsed = dirty_started.elapsed();

        let merge_started = Instant::now();
        let final_results = if dirty_results.candidate_docs == 0
            && dirty_results.matched_lines == 0
            && dirty_results.matched_occurrences == 0
            && dirty_results.file_counts.is_empty()
            && dirty_results.file_match_counts.is_empty()
            && dirty_results.hits.is_empty()
        {
            base_results.clone()
        } else {
            merge_search_results(base_results.clone(), dirty_results.clone())
        };
        let merge_elapsed = merge_started.elapsed();

        let dirty_documents = diff.modified_files.len() + diff.new_files.len();
        Ok(WorkspaceSearchProfile {
            diff,
            shadowed_base_paths: workspace_snapshot.shadowed_base_paths_len(),
            dirty_documents,
            base_candidate_docs: base_results.candidate_docs,
            base_matched_lines: base_results.matched_lines,
            dirty_candidate_docs: dirty_results.candidate_docs,
            dirty_matched_lines: dirty_results.matched_lines,
            final_candidate_docs: final_results.candidate_docs,
            final_matched_lines: final_results.matched_lines,
            timings: WorkspaceSearchTimings {
                diff: diff_elapsed,
                overlay_build: overlay_elapsed,
                base_search: base_elapsed,
                dirty_search: dirty_elapsed,
                merge: merge_elapsed,
                total: total_started.elapsed(),
            },
        })
    }

    /// Captures a reusable read-only workspace view pinned to the current base
    /// snapshot and the corresponding materialized overlay snapshot.
    pub fn snapshot(&self) -> Result<WorkspaceSnapshot> {
        let base_snapshot = match self.open_base_snapshot_reader() {
            Ok(base_snapshot) => base_snapshot,
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                return Err(AppError::InvalidIndex(format!(
                    "base snapshot does not exist at {}",
                    self.inner.options.build_config.storage_root.display()
                )));
            }
            Err(error) => return Err(error),
        };
        let dirty = self.diff_against_worktree()?;
        let base_snapshot_key = self.current_base_snapshot_key()?;
        let overlay_snapshot = self.materialize_overlay_snapshot_for_diff(&dirty)?;
        Ok(WorkspaceSnapshot {
            build_config: self.inner.options.build_config.clone(),
            base_snapshot_key,
            base_snapshot,
            overlay_snapshot,
        })
    }

    /// Searches only the current base snapshot and reuses the cached searcher when available.
    pub fn search_snapshot_with_filter(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
        allow_scan_fallback: bool,
    ) -> Result<SearchResponse> {
        match self.open_base_snapshot_reader() {
            Ok(base_snapshot) => Ok(SearchResponse {
                backend: SearchBackend::Index,
                results: self.workspace_search_view()?.search_current_snapshot(
                    query,
                    &QueryFilters::default(),
                    filter,
                    Some(&base_snapshot),
                )?,
            }),
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                if allow_scan_fallback {
                    return Ok(SearchResponse {
                        backend: SearchBackend::Scan,
                        results: self.scan_search_with_filter(query, filter)?,
                    });
                }
                Err(AppError::InvalidIndex(format!(
                    "base snapshot does not exist at {}",
                    self.inner.options.build_config.storage_root.display()
                )))
            }
            Err(error) => Err(error),
        }
    }

    pub fn scan_search_with_filter(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        search_documents(
            query,
            &scan_text_repository_files(&self.inner.options.build_config, filter)?,
        )
    }

    /// Returns the current full worktree diff. Prefer this only for initialization
    /// or recovery; regular daemon queries should use dirty-path overlays.
    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        let catalog = self.open_base_path_catalog()?;
        diff_against_base_snapshot(&self.inner.options.build_config, &catalog)
    }

    /// Reconciles a caller-maintained dirty overlay against the current base snapshot.
    pub fn reconcile_dirty_diff(&self, dirty: &IndexWorktreeDiff) -> Result<IndexWorktreeDiff> {
        if dirty.is_empty() {
            return Ok(IndexWorktreeDiff::default());
        }

        let current = self.diff_against_worktree()?;
        Ok(reconcile_dirty_paths_against_current_diff(dirty, &current))
    }

    fn repo_id(&self) -> crate::index_engine::RepoId {
        crate::index_engine::RepoId(
            self.inner
                .options
                .build_config
                .repo_path
                .to_string_lossy()
                .into_owned(),
        )
    }

    fn workspace_search_view(&self) -> Result<WorkspaceSearchView> {
        WorkspaceSearchView::open(self.inner.options.build_config.clone())
    }

    fn materialize_overlay_snapshot_for_diff(
        &self,
        dirty: &IndexWorktreeDiff,
    ) -> Result<Option<IndexSnapshotReader>> {
        if dirty.is_empty() {
            return Ok(None);
        }
        let overlay_state =
            build_overlay_snapshot_state(&self.repo_id(), &self.inner.options.build_config, dirty)?;
        self.materialize_overlay_snapshot_reader(&overlay_state)
            .map(Some)
    }
}

impl WorkspaceSnapshot {
    pub fn base_snapshot_key(&self) -> &str {
        &self.base_snapshot_key
    }

    pub fn search(&self, query: &crate::QueryConfig) -> Result<SearchResults> {
        self.search_with_filter(query, None)
    }

    pub fn search_with_filter(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        let search_view = WorkspaceSearchView::open(self.build_config.clone())?;
        let base_info = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
            .ok_or_else(|| {
                AppError::InvalidIndex("base snapshot shard metadata is missing".into())
            })?;
        if base_info.snapshot_key != self.base_snapshot_key {
            return Err(AppError::Protocol(format!(
                "snapshot handle {} is stale; current base snapshot is {}",
                self.base_snapshot_key, base_info.snapshot_key
            )));
        }
        let _base_snapshot = WorkspaceBaseSnapshotIndex::open_if_matching(
            &self.build_config,
            &self.base_snapshot_key,
        )?
        .ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "base snapshot shard marker is missing or stale for {}",
                self.build_config.repo_path.display()
            ))
        })?
        .open_committed()?;
        search_workspace_view(
            &search_view,
            &self.base_snapshot,
            self.overlay_snapshot.as_ref(),
            query,
            filter,
        )
    }

    pub fn visible_paths(&self, filter: Option<&PathFilter>) -> Result<Vec<String>> {
        workspace_visible_paths(
            &self.build_config,
            &self.base_snapshot,
            self.overlay_snapshot.as_ref(),
            filter,
        )
    }

    pub fn candidate_paths(
        &self,
        query: &crate::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        workspace_candidate_paths(
            &self.build_config,
            &workspace_query_request(query),
            &self.base_snapshot,
            self.overlay_snapshot.as_ref(),
            filter,
        )
    }
}

fn search_workspace_view(
    search_view: &WorkspaceSearchView,
    base_snapshot: &crate::index_engine::reader::IndexSnapshotReader,
    overlay_snapshot: Option<&crate::index_engine::reader::IndexSnapshotReader>,
    query: &crate::QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<SearchResults> {
    let workspace_snapshot = search_view
        .open_workspace_query_snapshot(Some(base_snapshot.clone()), overlay_snapshot.cloned())?;
    workspace_snapshot.search(search_view, query, &QueryFilters::default(), filter)
}

fn workspace_visible_paths(
    build_config: &BuildConfig,
    base_snapshot: &IndexSnapshotReader,
    overlay_snapshot: Option<&IndexSnapshotReader>,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    let shadowed_base_paths = overlay_snapshot
        .map(|snapshot| snapshot.visible_keys())
        .transpose()?
        .map(|keys| {
            keys.live
                .into_iter()
                .chain(keys.deleted)
                .map(|key| key.logical_path)
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let mut paths = BTreeSet::new();

    for document in base_snapshot.visible_documents()?.into_values() {
        if shadowed_base_paths.contains(&document.logical_path) {
            continue;
        }
        push_visible_path(&mut paths, build_config, &document.logical_path, filter);
    }

    if let Some(overlay_snapshot) = overlay_snapshot {
        for document in overlay_snapshot.visible_documents()?.into_values() {
            push_visible_path(&mut paths, build_config, &document.logical_path, filter);
        }
    }

    Ok(paths.into_iter().collect())
}

fn workspace_candidate_paths(
    build_config: &BuildConfig,
    query: &QueryRequest,
    base_snapshot: &IndexSnapshotReader,
    overlay_snapshot: Option<&IndexSnapshotReader>,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    let shadowed_base_paths = overlay_snapshot
        .map(|snapshot| snapshot.visible_keys())
        .transpose()?
        .map(|keys| {
            keys.live
                .into_iter()
                .chain(keys.deleted)
                .map(|key| key.logical_path)
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    let mut paths = BTreeSet::new();

    for logical_path in base_snapshot.candidate_paths(query)? {
        if shadowed_base_paths.contains(&logical_path) {
            continue;
        }
        push_visible_path(&mut paths, build_config, &logical_path, filter);
    }

    if let Some(overlay_snapshot) = overlay_snapshot {
        for logical_path in overlay_snapshot.candidate_paths(query)? {
            push_visible_path(&mut paths, build_config, &logical_path, filter);
        }
    }

    Ok(paths.into_iter().collect())
}

pub(crate) fn workspace_query_request(query: &crate::QueryConfig) -> QueryRequest {
    QueryRequest {
        pattern: query.regex_pattern.clone(),
        mode: query.search_mode,
        case_insensitive: query.case_insensitive,
        top_k: query.global_max_results,
        filters: QueryFilters::default(),
    }
}

fn push_visible_path(
    paths: &mut BTreeSet<String>,
    build_config: &BuildConfig,
    logical_path: &str,
    filter: Option<&PathFilter>,
) {
    let absolute = build_config.repo_path.join(logical_path);
    if filter.is_some_and(|filter| !filter.matches_file(&absolute)) {
        return;
    }
    paths.insert(absolute.to_string_lossy().into_owned());
}

fn query_with_remaining_global_limit(
    query: &crate::QueryConfig,
    current_results: &SearchResults,
) -> Option<crate::QueryConfig> {
    let Some(limit) = query.effective_global_max_results() else {
        return Some(query.clone());
    };
    let consumed = current_results.result_units(query.search_mode);
    if consumed >= limit {
        return None;
    }

    let mut next_query = query.clone();
    next_query.global_max_results = Some(limit - consumed);
    Some(next_query)
}

fn empty_search_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
        hits: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use tempfile::TempDir;

    use super::{BaseSnapshotKind, WorkspaceFreshnessState, WorkspaceIndex, WorkspaceIndexOptions};
    use crate::{
        config::{BuildConfig, CorpusMode, TokenizerMode},
        index_engine::WorkspaceBaseSnapshotIndex,
        search_engine::SearchBackend,
        search_engine::SearchEngine,
        worktree_diff::IndexWorktreeDiff,
        QueryConfig,
    };

    struct TestRepo {
        _temp: TempDir,
        repo: PathBuf,
        index: PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temp dir should succeed");
            let repo_dir = temp.path().join("repo");
            fs::create_dir_all(&repo_dir).expect("repo dir should succeed");
            let repo = fs::canonicalize(&repo_dir).expect("repo dir should canonicalize");
            let index = repo.join(".codgrep-index-engine");
            Self {
                _temp: temp,
                repo,
                index,
            }
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.repo.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should succeed");
            }
            fs::write(&path, contents).expect("write should succeed");
            path
        }

        fn write_bytes(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let path = self.repo.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should succeed");
            }
            fs::write(&path, bytes).expect("write should succeed");
            path
        }

        fn build_config(&self) -> BuildConfig {
            BuildConfig {
                repo_path: self.repo.clone(),
                storage_root: self.index.clone(),
                tokenizer: TokenizerMode::Trigram,
                corpus_mode: CorpusMode::RespectIgnore,
                include_hidden: false,
                max_file_size: 1024 * 1024,
                min_sparse_len: 3,
                max_sparse_len: 32,
            }
        }

        fn build(&self) {
            SearchEngine::new(self.build_config())
                .build_base_snapshot()
                .expect("build should succeed");
        }
    }

    fn query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: pattern.into(),
            patterns: vec![pattern.into()],
            fixed_strings: true,
            ..QueryConfig::default()
        }
    }

    #[test]
    fn workspace_probe_freshness_reports_missing_base_snapshot() {
        let repo = TestRepo::new();
        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let probe = workspace
            .probe_freshness()
            .expect("freshness probe should succeed");
        assert_eq!(probe.state, WorkspaceFreshnessState::MissingBaseSnapshot);
        assert!(probe.base_snapshot_key.is_none());
        assert_eq!(probe.dirty_file_count(), 0);
        assert!(probe.needs_base_snapshot());
    }

    #[test]
    fn workspace_probe_freshness_reports_stale_after_new_file() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
        repo.build();
        repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let probe = workspace
            .probe_freshness()
            .expect("freshness probe should succeed");
        assert_eq!(probe.state, WorkspaceFreshnessState::Stale);
        assert_eq!(probe.new_files, 1);
        assert_eq!(probe.dirty_file_count(), 1);
    }

    #[test]
    fn workspace_probe_freshness_if_due_reuses_cache_until_ttl_expires() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
        repo.build();

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let first = workspace
            .probe_freshness_if_due(Duration::from_secs(60))
            .expect("freshness probe should succeed");
        assert_eq!(first.state, WorkspaceFreshnessState::Fresh);

        repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

        let cached = workspace
            .probe_freshness_if_due(Duration::from_secs(60))
            .expect("cached freshness probe should succeed");
        assert_eq!(cached.state, WorkspaceFreshnessState::Fresh);

        let refreshed = workspace
            .probe_freshness_if_due(Duration::ZERO)
            .expect("fresh freshness probe should succeed");
        assert_eq!(refreshed.state, WorkspaceFreshnessState::Stale);
        assert_eq!(refreshed.new_files, 1);
    }

    #[test]
    fn ensure_base_snapshot_invalidates_cached_searcher_after_rebuild() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
        repo.build();

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let initial_status = workspace.status().expect("status should succeed");
        let initial_dirty = initial_status.dirty_files.expect("dirty diff should exist");
        assert!(initial_dirty.modified_files.is_empty());
        assert!(initial_dirty.deleted_files.is_empty());
        assert!(initial_dirty.new_files.is_empty());

        repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");

        let stale_status = workspace.status().expect("status should succeed");
        let stale_dirty = stale_status.dirty_files.expect("dirty diff should exist");
        assert_eq!(stale_dirty.new_files.len(), 1);

        workspace
            .ensure_base_snapshot()
            .expect("rebuild should succeed");

        let refreshed_status = workspace.status().expect("status should succeed");
        let refreshed_dirty = refreshed_status
            .dirty_files
            .expect("dirty diff should exist");
        assert!(refreshed_dirty.modified_files.is_empty());
        assert!(refreshed_dirty.deleted_files.is_empty());
        assert!(refreshed_dirty.new_files.is_empty());
    }

    #[test]
    fn ensure_base_snapshot_publishes_base_shard_metadata_used_by_base_snapshot_info() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let base = workspace
            .ensure_base_snapshot()
            .expect("base snapshot should build");
        let metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&repo.build_config())
            .expect("metadata should load")
            .expect("metadata should exist");
        assert_eq!(metadata.snapshot_key, base.snapshot_key);
        assert_eq!(metadata.doc_count, Some(1));
        assert_eq!(metadata.tokenizer, Some(repo.build_config().tokenizer));
        assert!(metadata.config_fingerprint.is_some());
        assert!(
            !repo.index.join("CURRENT").exists(),
            "workspace base snapshot should not depend on legacy CURRENT"
        );

        let metadata_backed = workspace
            .base_snapshot_info()
            .expect("base snapshot info should use base shard metadata");
        assert_eq!(metadata_backed.snapshot_key, base.snapshot_key);
        assert_eq!(metadata_backed.doc_count, 1);
        assert_eq!(metadata_backed.tokenizer, repo.build_config().tokenizer);
    }

    #[test]
    fn ensure_base_snapshot_publishes_one_compact_base_segment() {
        let repo = TestRepo::new();
        repo.write("src/a.rs", "const A: &str = \"AAAA\";\n");
        repo.write("src/b.rs", "const B: &str = \"BBBB\";\n");
        repo.write("src/c.rs", "const C: &str = \"CCCC\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");
        workspace
            .ensure_base_snapshot()
            .expect("base snapshot should build");

        let snapshot = WorkspaceBaseSnapshotIndex::open_read_only(&repo.build_config())
            .expect("base snapshot should open")
            .open_committed()
            .expect("committed base snapshot should open");
        assert_eq!(snapshot.manifest().active_segments.len(), 1);

        let manifest_count = fs::read_dir(
            WorkspaceBaseSnapshotIndex::index_root(&repo.build_config()).join("manifests"),
        )
        .expect("manifests dir should exist")
        .filter_map(|entry| entry.ok())
        .count();
        assert_eq!(manifest_count, 1);
    }

    #[test]
    fn workspace_search_and_snapshot_use_base_shard_without_legacy_index() {
        let repo = TestRepo::new();
        let tracked = repo.write("base.rs", "const NAME: &str = \"BASE\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");
        workspace
            .ensure_base_snapshot()
            .expect("base snapshot should build");

        assert!(
            !repo.index.join("CURRENT").exists(),
            "workspace search should not require legacy CURRENT"
        );

        let snapshot_search = workspace
            .search_snapshot_with_filter(&query("BASE"), None, false)
            .expect("snapshot search should succeed");
        assert_eq!(snapshot_search.backend, SearchBackend::Index);
        assert_eq!(snapshot_search.results.matched_lines, 1);

        let snapshot = workspace.snapshot().expect("snapshot should succeed");
        repo.write("base.rs", "const NAME: &str = \"DIRTY\";\n");

        let live_results = workspace
            .search(&query("DIRTY"))
            .expect("workspace search should see dirty contents");
        assert_eq!(live_results.matched_lines, 1);

        let pinned_results = snapshot
            .search(&query("BASE"))
            .expect("snapshot search should succeed");
        assert_eq!(pinned_results.matched_lines, 0);

        let stale_snapshot = workspace
            .ensure_base_snapshot()
            .expect("rebuild should succeed");
        assert_eq!(
            stale_snapshot.snapshot_key,
            workspace
                .base_snapshot_info()
                .expect("base snapshot info should succeed")
                .snapshot_key
        );
        let error = snapshot
            .search(&query("BASE"))
            .expect_err("stale snapshot should fail once base snapshot changes");
        assert!(matches!(error, crate::AppError::Protocol(_)));
        assert!(tracked.exists());
    }

    #[test]
    fn reconcile_dirty_diff_keeps_only_paths_that_are_still_dirty() {
        let repo = TestRepo::new();
        let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
        let removed = repo.write("removed.rs", "const NAME: &str = \"REMOVED\";\n");
        repo.build();

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let added = repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");
        fs::remove_file(&removed).expect("remove should succeed");
        fs::remove_file(&added).expect("remove should succeed");

        let reconciled = workspace
            .reconcile_dirty_diff(&IndexWorktreeDiff {
                modified_files: vec![tracked.to_string_lossy().into_owned()],
                deleted_files: vec![removed.to_string_lossy().into_owned()],
                new_files: vec![added.to_string_lossy().into_owned()],
            })
            .expect("reconcile should succeed");

        assert!(reconciled.modified_files.is_empty());
        assert_eq!(
            reconciled.deleted_files,
            vec![removed.to_string_lossy().into_owned()]
        );
        assert!(reconciled.new_files.is_empty());
    }

    #[test]
    fn git_workspace_status_uses_filtered_dirty_paths() {
        let repo = TestRepo::new();
        let git_repo = repo.repo.clone();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
        let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
        repo.write(".gitignore", "ignored.rs\n");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("git commit should succeed");
        repo.build();

        repo.write("tracked.rs", "const NAME: &str = \"DIRTY\";\n");
        let added = repo.write("added.rs", "const NAME: &str = \"ADDED\";\n");
        repo.write(".hidden.rs", "const NAME: &str = \"HIDDEN\";\n");
        repo.write("ignored.rs", "const NAME: &str = \"IGNORED\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");
        let status = workspace.status().expect("status should succeed");
        let dirty = status.dirty_files.expect("dirty diff should exist");

        assert_eq!(
            dirty.modified_files,
            vec![tracked.to_string_lossy().into_owned()]
        );
        assert_eq!(dirty.deleted_files, Vec::<String>::new());
        assert_eq!(dirty.new_files, vec![added.to_string_lossy().into_owned()]);
    }

    #[test]
    fn git_workspace_status_detects_files_revealed_by_gitignore_change() {
        let repo = TestRepo::new();
        let git_repo = repo.repo.clone();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
        repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
        repo.write(".gitignore", "revealed.rs\n");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("git commit should succeed");
        repo.build();

        let revealed = repo.write("revealed.rs", "const NAME: &str = \"REVEALED\";\n");
        repo.write(".gitignore", "");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");
        let status = workspace.status().expect("status should succeed");
        let dirty = status.dirty_files.expect("dirty diff should exist");

        assert_eq!(dirty.modified_files, Vec::<String>::new());
        assert_eq!(dirty.deleted_files, Vec::<String>::new());
        assert_eq!(
            dirty.new_files,
            vec![revealed.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn git_workspace_status_skips_new_files_that_cannot_enter_the_index() {
        let repo = TestRepo::new();
        let git_repo = repo.repo.clone();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
        repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("git commit should succeed");
        repo.build();

        let mut bytes = vec![b'a'; 8 * 1024];
        bytes.push(0xFF);
        repo.write_bytes("encoded.txt", &bytes);

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");
        let status = workspace.status().expect("status should succeed");
        let dirty = status.dirty_files.expect("dirty diff should exist");

        assert_eq!(dirty.modified_files, Vec::<String>::new());
        assert_eq!(dirty.deleted_files, Vec::<String>::new());
        assert_eq!(dirty.new_files, Vec::<String>::new());
    }

    #[test]
    fn workspace_exposes_git_snapshot_identity() {
        let repo = TestRepo::new();
        let git_repo = repo.repo.clone();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
        repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&git_repo)
            .args(["commit", "-qm", "initial"])
            .status()
            .expect("git commit should succeed");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
        })
        .expect("workspace should open");

        let base = workspace
            .ensure_base_snapshot()
            .expect("base snapshot should build");
        assert_eq!(base.snapshot_kind, BaseSnapshotKind::GitCommit);
        assert!(base.snapshot_key.starts_with("base-git-"));
        assert!(base.head_commit.is_some());
        assert!(base.config_fingerprint.is_some());
    }
}
