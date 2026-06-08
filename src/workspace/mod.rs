mod base;
mod freshness;
mod runtime;
mod search;

use std::{
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
        query::QueryFilters, reader::IndexSnapshotReader, WorkspaceBaseSnapshotIndex,
        WorkspaceSearchView,
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
use self::search::{
    empty_search_results, query_with_remaining_global_limit, search_workspace_view,
    workspace_candidate_paths, workspace_query_request, workspace_visible_paths,
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
            true,
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
            && dirty_results.matched_paths.is_empty()
            && dirty_results.line_matches.is_empty()
            && dirty_results.line_match_files.is_empty()
            && dirty_results.file_counts.is_empty()
            && dirty_results.file_match_counts.is_empty()
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

#[cfg(test)]
mod tests;
