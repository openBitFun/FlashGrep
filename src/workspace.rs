mod runtime;

use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    index::{
        build_index_with_options,
        format::{read_docs_file, IndexLayout},
        overlay::{
            current_base_snapshot_key, ensure_fresh_overlay_with_progress_and_cancel,
            prune_overlay_caches, read_overlay_snapshot, read_overlay_state, OverlaySnapshotInfo,
            OverlaySnapshotState,
        },
        IndexBuildOptions, IndexSearcher, IndexWorktreeDiff,
    },
    progress::IndexProgress,
    search::SearchResults,
    search_engine::search_documents,
    QueryConfig, TokenizerMode,
};

use self::runtime::{
    candidate_documents as workspace_candidate_documents, scan_text_repository_files,
    CachedOverlayRef,
};

/// Options for opening a [`WorkspaceIndex`].
///
/// This facade exposes a two-layer view over an indexed repository:
/// a stable base snapshot plus an optional dirty-worktree overlay.
#[derive(Debug, Clone)]
pub struct WorkspaceIndexOptions {
    /// Build-time/index-location configuration for the workspace.
    pub build_config: BuildConfig,
    /// Legacy compatibility toggle.
    ///
    /// Query paths no longer refresh overlays automatically; callers should
    /// invoke [`WorkspaceIndex::refresh_overlay`] explicitly when they need
    /// fresher dirty-worktree coverage.
    pub auto_refresh_overlay: bool,
    /// Maximum number of overlay caches to retain per base snapshot.
    ///
    /// `None` disables automatic pruning.
    pub overlay_cache_limit: Option<usize>,
}

impl From<BuildConfig> for WorkspaceIndexOptions {
    fn from(build_config: BuildConfig) -> Self {
        Self {
            build_config,
            auto_refresh_overlay: false,
            overlay_cache_limit: Some(8),
        }
    }
}

/// Identity and metadata for the current base snapshot.
#[derive(Debug, Clone)]
pub struct BaseSnapshotInfo {
    /// Stable cache key for the base snapshot.
    pub snapshot_key: String,
    /// Root directory that stores base and overlay index data.
    pub index_path: PathBuf,
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
    /// Snapshot materialized from a concrete Git commit.
    GitCommit,
    /// Snapshot materialized from a non-Git repository fingerprint.
    RepositoryFallback,
    /// Older on-disk snapshot that predates the current identity model.
    Legacy,
}

/// Current observable workspace state.
#[derive(Debug, Clone)]
pub struct IndexStatus {
    /// Metadata for the current base snapshot, if one exists on disk.
    pub base: Option<BaseSnapshotInfo>,
    /// Metadata for the currently active overlay cache, if one exists.
    pub overlay: Option<OverlaySnapshotInfo>,
    /// Worktree diff against the base snapshot, if the base index can be opened.
    pub dirty_files: Option<IndexWorktreeDiff>,
}

/// High-level freshness state for the currently opened workspace index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceFreshnessState {
    /// No base snapshot exists on disk yet.
    MissingBaseSnapshot,
    /// Base snapshot exists and worktree currently matches it.
    Fresh,
    /// Base snapshot exists but worktree has drifted.
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
    /// Active overlay key, when available.
    pub active_overlay_key: Option<String>,
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

    pub fn needs_overlay_refresh(&self) -> bool {
        self.state == WorkspaceFreshnessState::Stale
    }

    pub fn dirty_file_count(&self) -> usize {
        self.modified_files + self.deleted_files + self.new_files
    }
}

/// High-level workspace facade for base snapshot management, overlay refresh,
/// and searching against the latest effective repository view.
///
/// Prefer this type when your integration needs freshness orchestration:
/// base snapshot lifecycle, overlay refresh, and probe caching.
#[derive(Clone)]
pub struct WorkspaceIndex {
    inner: Arc<WorkspaceIndexInner>,
}

/// Reusable read-only workspace view captured at a point in time.
///
/// A snapshot may resolve to:
/// - an indexed clean base view,
/// - an indexed dirty view backed by an overlay cache, or
/// - a scan fallback when no index exists yet.
///
/// This type exists to provide a stable read view across multiple queries,
/// even while workspace state may keep changing.
#[derive(Clone)]
pub struct WorkspaceSnapshot {
    workspace: Arc<WorkspaceIndexInner>,
    view: WorkspaceReadView,
}

#[derive(Clone)]
struct CachedOverlay {
    state: OverlaySnapshotState,
    searcher: Arc<IndexSearcher>,
}

#[derive(Clone)]
enum WorkspaceReadView {
    IndexedClean {
        searcher: Arc<IndexSearcher>,
    },
    IndexedDirty {
        searcher: Arc<IndexSearcher>,
        diff: IndexWorktreeDiff,
        overlay: Option<CachedOverlay>,
    },
    ScanFallback,
}

struct WorkspaceIndexInner {
    options: WorkspaceIndexOptions,
    cached_searcher: Mutex<Option<CachedBaseSearcher>>,
    cached_freshness: Mutex<Option<CachedFreshnessProbe>>,
}

#[derive(Clone)]
struct CachedBaseSearcher {
    snapshot_key: String,
    searcher: Arc<IndexSearcher>,
}

#[derive(Clone)]
struct CachedFreshnessProbe {
    captured_at: Instant,
    probe: WorkspaceFreshness,
}

impl WorkspaceIndex {
    /// Opens a workspace facade over the configured repository and index root.
    pub fn open(options: WorkspaceIndexOptions) -> Result<Self> {
        let build_config = options.build_config.normalized()?;
        Ok(Self {
            inner: Arc::new(WorkspaceIndexInner {
                options: WorkspaceIndexOptions {
                    build_config,
                    auto_refresh_overlay: options.auto_refresh_overlay,
                    overlay_cache_limit: options.overlay_cache_limit,
                },
                cached_searcher: Mutex::new(None),
                cached_freshness: Mutex::new(None),
            }),
        })
    }

    /// Ensures that a base snapshot exists and returns its identity metadata.
    pub fn ensure_base_snapshot(&self) -> Result<BaseSnapshotInfo> {
        self.ensure_base_snapshot_with_options(IndexBuildOptions::default())
    }

    pub fn ensure_base_snapshot_with_options(
        &self,
        options: IndexBuildOptions<'_>,
    ) -> Result<BaseSnapshotInfo> {
        self.clear_freshness_probe_cache();
        build_index_with_options(&self.inner.options.build_config, options)?;
        self.base_snapshot_info()
    }

    /// Refreshes the dirty-worktree overlay cache if the worktree is dirty.
    ///
    /// Returns `Ok(None)` when the worktree matches the base snapshot.
    pub fn refresh_overlay(&self) -> Result<Option<OverlaySnapshotInfo>> {
        self.refresh_overlay_with_progress(|_| {})
    }

    pub fn refresh_overlay_with_progress<F>(
        &self,
        on_progress: F,
    ) -> Result<Option<OverlaySnapshotInfo>>
    where
        F: FnMut(IndexProgress),
    {
        self.refresh_overlay_with_progress_and_cancel(on_progress, || false)
    }

    pub fn refresh_overlay_with_progress_and_cancel<F, C>(
        &self,
        mut on_progress: F,
        mut should_cancel: C,
    ) -> Result<Option<OverlaySnapshotInfo>>
    where
        F: FnMut(IndexProgress),
        C: FnMut() -> bool,
    {
        self.clear_freshness_probe_cache();
        if self.base_snapshot_info().is_err() {
            let _ = self.ensure_base_snapshot_with_options(
                IndexBuildOptions::new()
                    .with_progress(&mut on_progress)
                    .with_cancel(&mut should_cancel),
            )?;
        }

        let searcher = self.open_searcher()?;
        let diff = searcher.diff_against_worktree()?;
        if diff.is_empty() {
            return Ok(None);
        }

        let overlay = ensure_fresh_overlay_with_progress_and_cancel(
            &self.inner.options.build_config,
            &diff,
            &mut on_progress,
            &mut should_cancel,
        )?
        .map(|state| state.info)
        .ok_or_else(|| crate::AppError::InvalidIndex("failed to refresh overlay".into()))?;
        self.prune_overlay_cache_with_configured_limit()?;
        Ok(Some(overlay))
    }

    /// Prunes cached overlay generations under the configured index root.
    ///
    /// The `keep_per_base` limit is applied independently per base snapshot.
    pub fn prune_overlay_cache(&self, keep_per_base: usize) -> Result<usize> {
        prune_overlay_caches(&self.inner.options.build_config.index_path, keep_per_base)
    }

    /// Reports the current base snapshot, active overlay, and worktree diff state.
    pub fn status(&self) -> Result<IndexStatus> {
        let base = self.base_snapshot_info().ok();
        let dirty_files = match self.open_searcher() {
            Ok(searcher) => Some(searcher.diff_against_worktree()?),
            Err(crate::AppError::Io(error)) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let overlay = if let Some(base) = &base {
            read_overlay_snapshot(
                &self.inner.options.build_config.index_path,
                &base.snapshot_key,
            )?
        } else {
            None
        };

        Ok(IndexStatus {
            base,
            overlay,
            dirty_files,
        })
    }

    /// Performs a full freshness probe against the current worktree.
    ///
    /// This always scans current file metadata and does not mutate index state.
    pub fn probe_freshness(&self) -> Result<WorkspaceFreshness> {
        let probe = self.compute_freshness_probe()?;
        self.cache_freshness_probe(probe.clone());
        Ok(probe)
    }

    /// Returns a cached freshness probe when still within `ttl`; otherwise runs
    /// a new full probe.
    ///
    /// "if due" means the previous probe age has exceeded `ttl`.
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
    /// Depending on configuration and current state this may query:
    /// - the clean base index,
    /// - the currently activated base+overlay snapshot view, or
    /// - a scan fallback when no index exists.
    pub fn search(&self, query: &QueryConfig) -> Result<SearchResults> {
        self.snapshot()?.search(query)
    }

    /// Captures a reusable read-only workspace snapshot for repeated queries.
    pub fn snapshot(&self) -> Result<WorkspaceSnapshot> {
        match self.open_searcher() {
            Ok(searcher) => {
                let snapshot_key = self.current_snapshot_key_on_disk()?;
                let overlay =
                    read_overlay_state(&self.inner.options.build_config.index_path, &snapshot_key)?
                        .map(|state| -> Result<CachedOverlay> {
                            Ok(CachedOverlay {
                                searcher: Arc::new(IndexSearcher::open(state.data_path.clone())?),
                                state,
                            })
                        })
                        .transpose()?;
                let view = if let Some(overlay) = overlay {
                    WorkspaceReadView::IndexedDirty {
                        searcher,
                        diff: shadow_diff_from_overlay_state(&overlay.state),
                        overlay: Some(overlay),
                    }
                } else {
                    WorkspaceReadView::IndexedClean { searcher }
                };
                self.prune_overlay_cache_with_configured_limit()?;
                Ok(WorkspaceSnapshot {
                    workspace: Arc::clone(&self.inner),
                    view,
                })
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                Ok(WorkspaceSnapshot {
                    workspace: Arc::clone(&self.inner),
                    view: WorkspaceReadView::ScanFallback,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn base_snapshot_info(&self) -> Result<BaseSnapshotInfo> {
        let layout = IndexLayout::resolve(&self.inner.options.build_config.index_path)?;
        let snapshot_key = current_base_snapshot_key(&self.inner.options.build_config.index_path)?
            .or_else(|| {
                layout
                    .data_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or_else(|| crate::AppError::InvalidIndex("base generation is missing".into()))?;
        let (metadata, docs) = read_docs_file(&layout.docs_path)?;
        let snapshot_kind = if snapshot_key.starts_with("base-git-") {
            BaseSnapshotKind::GitCommit
        } else if snapshot_key.starts_with("base-repo-") {
            BaseSnapshotKind::RepositoryFallback
        } else {
            BaseSnapshotKind::Legacy
        };
        let (head_commit, config_fingerprint) = metadata
            .build
            .as_ref()
            .map(|build| (build.head_commit.clone(), build.config_fingerprint.clone()))
            .unwrap_or((None, None));

        Ok(BaseSnapshotInfo {
            snapshot_key,
            index_path: self.inner.options.build_config.index_path.clone(),
            repo_path: self.inner.options.build_config.repo_path.clone(),
            tokenizer: metadata.tokenizer,
            doc_count: docs.len(),
            snapshot_kind,
            head_commit,
            config_fingerprint,
        })
    }

    fn open_searcher(&self) -> Result<Arc<IndexSearcher>> {
        let snapshot_key = self.current_snapshot_key_on_disk()?;
        let mut cached_searcher = match self.inner.cached_searcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(cached) = cached_searcher.as_ref() {
            if cached.snapshot_key == snapshot_key {
                return Ok(Arc::clone(&cached.searcher));
            }
        }

        let searcher = Arc::new(IndexSearcher::open(
            self.inner.options.build_config.index_path.clone(),
        )?);
        *cached_searcher = Some(CachedBaseSearcher {
            snapshot_key,
            searcher: Arc::clone(&searcher),
        });
        Ok(searcher)
    }

    fn current_snapshot_key_on_disk(&self) -> Result<String> {
        let layout = IndexLayout::resolve(&self.inner.options.build_config.index_path)?;
        current_base_snapshot_key(&self.inner.options.build_config.index_path)?
            .or_else(|| {
                layout
                    .data_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or_else(|| crate::AppError::InvalidIndex("base generation is missing".into()))
    }

    fn prune_overlay_cache_with_configured_limit(&self) -> Result<()> {
        if let Some(limit) = self.inner.options.overlay_cache_limit {
            let _ = self.prune_overlay_cache(limit)?;
        }
        Ok(())
    }

    fn compute_freshness_probe(&self) -> Result<WorkspaceFreshness> {
        let checked_at = SystemTime::now();
        let base = self.base_snapshot_info().ok();
        let active_overlay_key = if let Some(base) = &base {
            read_overlay_snapshot(
                &self.inner.options.build_config.index_path,
                &base.snapshot_key,
            )?
            .map(|overlay| overlay.overlay_key)
        } else {
            None
        };

        match self.open_searcher() {
            Ok(searcher) => {
                let diff = searcher.diff_against_worktree()?;
                let state = if diff.is_empty() {
                    WorkspaceFreshnessState::Fresh
                } else {
                    WorkspaceFreshnessState::Stale
                };
                Ok(WorkspaceFreshness {
                    checked_at,
                    state,
                    base_snapshot_key: base.map(|base| base.snapshot_key),
                    active_overlay_key,
                    modified_files: diff.modified_files.len(),
                    deleted_files: diff.deleted_files.len(),
                    new_files: diff.new_files.len(),
                })
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                Ok(WorkspaceFreshness {
                    checked_at,
                    state: WorkspaceFreshnessState::MissingBaseSnapshot,
                    base_snapshot_key: None,
                    active_overlay_key: None,
                    modified_files: 0,
                    deleted_files: 0,
                    new_files: 0,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn cache_freshness_probe(&self, probe: WorkspaceFreshness) {
        let mut cache = match self.inner.cached_freshness.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cache = Some(CachedFreshnessProbe {
            captured_at: Instant::now(),
            probe,
        });
    }

    fn clear_freshness_probe_cache(&self) {
        let mut cache = match self.inner.cached_freshness.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cache = None;
    }
}

impl WorkspaceSnapshot {
    /// Searches this captured snapshot without re-evaluating its read view.
    pub fn search(&self, query: &QueryConfig) -> Result<SearchResults> {
        match &self.view {
            WorkspaceReadView::IndexedClean { searcher } => searcher.search(query),
            WorkspaceReadView::IndexedDirty {
                searcher,
                diff,
                overlay,
            } => {
                let documents = workspace_candidate_documents(
                    searcher,
                    diff,
                    query,
                    &self.workspace.options.build_config,
                    cached_overlay_ref(overlay.as_ref()),
                )?
                .into_documents();
                search_documents(query, &documents)
            }
            WorkspaceReadView::ScanFallback => search_documents(
                query,
                &scan_text_repository_files(&self.workspace.options.build_config)?,
            ),
        }
    }
}

fn cached_overlay_ref(overlay: Option<&CachedOverlay>) -> Option<CachedOverlayRef<'_>> {
    overlay.map(|overlay| CachedOverlayRef {
        state: &overlay.state,
        searcher: overlay.searcher.as_ref(),
    })
}

fn shadow_diff_from_overlay_state(state: &OverlaySnapshotState) -> IndexWorktreeDiff {
    let mut modified_files = state.shadowed_paths.iter().cloned().collect::<Vec<_>>();
    modified_files.sort_unstable();
    IndexWorktreeDiff {
        modified_files,
        deleted_files: Vec::new(),
        new_files: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use tempfile::TempDir;

    use super::{
        WorkspaceFreshnessState, WorkspaceIndex, WorkspaceIndexOptions, WorkspaceReadView,
    };
    use crate::{
        build_index,
        config::{BuildConfig, CorpusMode, TokenizerMode},
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
            let index = repo.join(".codgrep-index");
            Self {
                _temp: temp,
                repo,
                index,
            }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.repo.join(relative)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.path(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should succeed");
            }
            fs::write(&path, contents).expect("write should succeed");
            path
        }

        fn build_config(&self) -> BuildConfig {
            BuildConfig {
                repo_path: self.repo.clone(),
                index_path: self.index.clone(),
                tokenizer: TokenizerMode::Trigram,
                corpus_mode: CorpusMode::RespectIgnore,
                include_hidden: false,
                max_file_size: 1024 * 1024,
                min_sparse_len: 3,
                max_sparse_len: 32,
            }
        }

        fn build(&self) -> BuildConfig {
            let config = self.build_config();
            build_index(&config).expect("build should succeed");
            config
        }
    }

    fn query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: pattern.into(),
            patterns: vec![pattern.into()],
            ..QueryConfig::default()
        }
    }

    #[test]
    fn workspace_snapshot_uses_scan_fallback_view_when_index_is_missing() {
        let repo = TestRepo::new();
        let source = repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
        })
        .expect("workspace should open");
        let snapshot = workspace.snapshot().expect("snapshot should open");

        assert!(matches!(snapshot.view, WorkspaceReadView::ScanFallback));

        let results = snapshot
            .search(&query("PM_RESUME"))
            .expect("search should succeed");
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].path, source.to_string_lossy());
    }

    #[test]
    fn workspace_snapshot_uses_indexed_clean_view_for_clean_worktree() {
        let repo = TestRepo::new();
        let source = repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");
        let _ = repo.build();

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
        })
        .expect("workspace should open");
        let snapshot = workspace.snapshot().expect("snapshot should open");

        assert!(matches!(
            snapshot.view,
            WorkspaceReadView::IndexedClean { .. }
        ));

        let results = snapshot
            .search(&query("PM_RESUME"))
            .expect("search should succeed");
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].path, source.to_string_lossy());
    }

    #[test]
    fn workspace_snapshot_uses_indexed_dirty_view_when_overlay_is_active() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
        let added = repo.write("added.rs", "const NAME: &str = \"OTHER\";\n");
        let _ = repo.build();
        repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
        })
        .expect("workspace should open");
        workspace
            .refresh_overlay()
            .expect("overlay refresh should succeed");
        let snapshot = workspace.snapshot().expect("snapshot should open");

        assert!(matches!(
            snapshot.view,
            WorkspaceReadView::IndexedDirty {
                overlay: Some(_),
                ..
            }
        ));

        let results = snapshot
            .search(&query("DIRTY"))
            .expect("search should succeed");
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].path, added.to_string_lossy());
    }

    #[test]
    fn workspace_probe_freshness_reports_missing_base_snapshot() {
        let repo = TestRepo::new();
        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
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
        let _ = repo.build();
        repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
        })
        .expect("workspace should open");

        let probe = workspace
            .probe_freshness()
            .expect("freshness probe should succeed");
        assert_eq!(probe.state, WorkspaceFreshnessState::Stale);
        assert_eq!(probe.new_files, 1);
        assert_eq!(probe.dirty_file_count(), 1);
        assert!(probe.needs_overlay_refresh());
    }

    #[test]
    fn workspace_probe_freshness_if_due_reuses_cache_until_ttl_expires() {
        let repo = TestRepo::new();
        repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
        let _ = repo.build();

        let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.build_config(),
            auto_refresh_overlay: false,
            overlay_cache_limit: None,
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
}
