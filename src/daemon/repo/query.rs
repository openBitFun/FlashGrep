use std::{
    env,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use crate::{
    config::CorpusMode,
    error::{AppError, Result},
    files::{scan_paths, ScanOptions},
    index_engine::query::QueryFilters,
    path_filter::{normalize_path, PathFilter, PathFilterArgs},
};

use super::{QueryBackend, RepoPhase, RepoRuntime, RepoStatus, SearchRequest, SearchResponse};

struct SearchActivityGuard<'a> {
    runtime: &'a RepoRuntime,
}

impl Drop for SearchActivityGuard<'_> {
    fn drop(&mut self) {
        self.runtime.active_searches.fetch_sub(1, Ordering::Relaxed);
    }
}

impl RepoRuntime {
    fn begin_search_activity(&self) -> SearchActivityGuard<'_> {
        self.active_searches.fetch_add(1, Ordering::Relaxed);
        SearchActivityGuard { runtime: self }
    }

    pub(super) fn active_search_count(&self) -> u64 {
        self.active_searches.load(Ordering::Relaxed)
    }

    pub(super) fn refresh(&self, force: bool) -> Result<RepoStatus> {
        if force || self.workspace_probe_pending() {
            self.refresh_workspace_overlay()?;
        }
        Ok(self.status_snapshot())
    }

    pub(super) fn search(&self, request: SearchRequest) -> Result<SearchResponse> {
        let _search_activity = self.begin_search_activity();
        let scope =
            normalize_repo_scope_args(&self.build_config.repo_path, &request.path_filter_args)?;
        let filter = build_path_filter(&self.build_config.repo_path, &scope)?;
        let query_filters = QueryFilters::default();
        let status = self.status_snapshot();
        let can_search_opening_snapshot =
            matches!(status.phase, RepoPhase::Opening) && status.snapshot_key.is_some();
        match status.phase {
            RepoPhase::Opening if !can_search_opening_snapshot => {
                self.search_during_transition(&request, &scope, filter.as_ref(), status)
            }
            RepoPhase::MissingBaseSnapshot
            | RepoPhase::BuildingBaseSnapshot
            | RepoPhase::RebuildingBaseSnapshot => {
                self.search_during_transition(&request, &scope, filter.as_ref(), status)
            }
            RepoPhase::Opening | RepoPhase::ReadyClean | RepoPhase::ReadyDirty => {
                let overlay_engine = match self.workspace_overlay_engine.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                }
                .as_ref()
                .cloned();
                let overlay_snapshot = overlay_engine
                    .as_ref()
                    .map(|overlay| match overlay.open_snapshot() {
                        Ok(snapshot) => Ok(Some(snapshot)),
                        Err(AppError::InvalidIndex(_)) => Ok(None),
                        Err(AppError::Io(error))
                            if error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            Ok(None)
                        }
                        Err(error) => Err(error),
                    })
                    .transpose()?
                    .flatten();
                let base_snapshot = match self.open_base_snapshot_reader() {
                    Ok(snapshot) => snapshot,
                    Err(AppError::InvalidIndex(_)) => None,
                    Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        None
                    }
                    Err(error) => return Err(error),
                };
                let tracked_dirty = self.dirty_catalog_visible_paths();
                let overlay_snapshot = if tracked_dirty.is_empty() {
                    None
                } else {
                    overlay_snapshot
                };
                let workspace_snapshot = self
                    .workspace_search_view
                    .open_workspace_query_snapshot(base_snapshot, overlay_snapshot)?;
                let mut response_status = status.clone();
                super::status::apply_repo_status_dirty_fields(
                    &mut response_status,
                    self.rebuild_dirty_threshold,
                    tracked_dirty.stats(),
                );
                let backend = if tracked_dirty.is_empty() {
                    QueryBackend::IndexedClean
                } else {
                    QueryBackend::IndexedWorkspaceView
                };
                let overlay_snapshot_unavailable_for_dirty =
                    !tracked_dirty.is_empty() && !workspace_snapshot.has_overlay_snapshot();
                if overlay_snapshot_unavailable_for_dirty {
                    self.schedule_workspace_probe(Duration::from_millis(0));
                }
                if !workspace_snapshot.has_base_snapshot() {
                    self.schedule_workspace_probe(Duration::from_millis(0));
                    return self.search_during_transition(
                        &request,
                        &scope,
                        filter.as_ref(),
                        response_status,
                    );
                }
                let results = match workspace_snapshot.search(
                    &self.workspace_search_view,
                    &request.query,
                    &query_filters,
                    filter.as_ref(),
                    true,
                ) {
                    Ok(results) => results,
                    Err(AppError::InvalidIndex(_)) => {
                        self.schedule_workspace_probe(Duration::from_millis(0));
                        match self.workspace_search_view.search_workspace_snapshots(
                            &request.query,
                            &query_filters,
                            filter.as_ref(),
                            workspace_snapshot.base_snapshot(),
                            None,
                        ) {
                            Ok(results) => results,
                            Err(AppError::InvalidIndex(_)) => {
                                return self.search_during_transition(
                                    &request,
                                    &scope,
                                    filter.as_ref(),
                                    response_status,
                                );
                            }
                            Err(AppError::Io(error))
                                if error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                return self.search_during_transition(
                                    &request,
                                    &scope,
                                    filter.as_ref(),
                                    response_status,
                                );
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.schedule_workspace_probe(Duration::from_millis(0));
                        match self.workspace_search_view.search_workspace_snapshots(
                            &request.query,
                            &query_filters,
                            filter.as_ref(),
                            workspace_snapshot.base_snapshot(),
                            None,
                        ) {
                            Ok(results) => results,
                            Err(AppError::InvalidIndex(_)) => {
                                return self.search_during_transition(
                                    &request,
                                    &scope,
                                    filter.as_ref(),
                                    response_status,
                                );
                            }
                            Err(AppError::Io(error))
                                if error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                return self.search_during_transition(
                                    &request,
                                    &scope,
                                    filter.as_ref(),
                                    response_status,
                                );
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) => return Err(error),
                };
                Ok(SearchResponse {
                    repo_id: self.repo_id.clone(),
                    backend,
                    status: response_status,
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

    pub(super) fn glob(&self, request: super::GlobRequest) -> Result<super::GlobResponse> {
        let scope =
            normalize_repo_scope_args(&self.build_config.repo_path, &request.path_filter_args)?;
        let filter = build_path_filter(&self.build_config.repo_path, &scope)?;
        let cache_key = glob_scope_cache_key(&scope);
        let cache_epoch = self.glob_cache_epoch.load(Ordering::Relaxed);
        if let Some(paths) = self.lookup_glob_cache(cache_epoch, &cache_key) {
            return Ok(super::GlobResponse {
                repo_id: self.repo_id.clone(),
                status: self.status_snapshot(),
                paths,
            });
        }
        let paths = match super::run_rg_glob(&self.build_config, &scope, filter.as_ref()) {
            Ok(paths) => paths,
            Err(_) => {
                let roots = scan_roots(&self.build_config.repo_path, &scope);
                let mut paths = scan_paths(
                    &roots,
                    Some(&self.build_config.storage_root),
                    ScanOptions {
                        respect_ignore: matches!(
                            self.build_config.corpus_mode,
                            CorpusMode::RespectIgnore
                        ),
                        include_hidden: self.build_config.include_hidden,
                        max_file_size: self.build_config.max_file_size,
                        max_depth: None,
                        ignore_files: Vec::new(),
                    },
                    filter.as_ref(),
                )?
                .into_iter()
                .map(|file| file.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
                paths.sort_unstable();
                paths
            }
        };
        if self.glob_cache_epoch.load(Ordering::Relaxed) == cache_epoch {
            self.store_glob_cache(cache_epoch, cache_key, &paths);
        }
        Ok(super::GlobResponse {
            repo_id: self.repo_id.clone(),
            status: self.status_snapshot(),
            paths,
        })
    }

    pub(super) fn cached_published_overlay_paths(&self) -> super::DirtyPathSet {
        match self.published_overlay_paths_cache.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub(super) fn workspace_probe_pending(&self) -> bool {
        self.workspace_probe_pending.load(Ordering::Relaxed)
    }

    pub(super) fn rebuild_dirty_paths_from_overlay_snapshot(
        &self,
        overlay_snapshot: &crate::index_engine::IndexSnapshotReader,
    ) -> Result<super::DirtyPathSet> {
        let visible = overlay_snapshot.visible_keys()?;
        let base_catalog = self.base_snapshot_catalog()?;
        let mut dirty = super::DirtyPathSet::default();

        for key in visible.live {
            let path = self
                .build_config
                .repo_path
                .join(&key.logical_path)
                .to_string_lossy()
                .into_owned();
            if base_catalog
                .as_ref()
                .and_then(|catalog| catalog.get(&key.logical_path))
                .is_some()
            {
                dirty.modified.insert(path);
            } else {
                dirty.new.insert(path);
            }
        }

        for key in visible.deleted {
            if base_catalog
                .as_ref()
                .and_then(|catalog| catalog.get(&key.logical_path))
                .is_some()
            {
                dirty.deleted.insert(
                    self.build_config
                        .repo_path
                        .join(&key.logical_path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }

        Ok(dirty)
    }

    pub(super) fn replace_published_overlay_paths_cache(
        &self,
        published_overlay_paths: super::DirtyPathSet,
    ) {
        let mut cache = match self.published_overlay_paths_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cache = published_overlay_paths;
    }

    fn lookup_glob_cache(&self, epoch: u64, key: &str) -> Option<Vec<String>> {
        let mut cache = match self.glob_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache.epoch != epoch {
            cache.epoch = epoch;
            cache.entries.clear();
            return None;
        }
        cache.entries.get(key).cloned()
    }

    fn store_glob_cache(&self, epoch: u64, key: String, paths: &[String]) {
        const MAX_GLOB_CACHE_ENTRIES: usize = 32;

        let mut cache = match self.glob_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if cache.epoch != epoch {
            cache.epoch = epoch;
            cache.entries.clear();
        }
        if !cache.entries.contains_key(&key) && cache.entries.len() >= MAX_GLOB_CACHE_ENTRIES {
            cache.entries.clear();
        }
        cache.entries.insert(key, paths.to_vec());
    }

    fn search_during_transition(
        &self,
        request: &SearchRequest,
        scope: &PathFilterArgs,
        filter: Option<&PathFilter>,
        status: RepoStatus,
    ) -> Result<SearchResponse> {
        match super::run_rg_search(&self.build_config, &request.query, scope) {
            Ok(results) => Ok(SearchResponse {
                repo_id: self.repo_id.clone(),
                backend: QueryBackend::RgFallback,
                status: self.status_snapshot(),
                results,
            }),
            Err(error) => {
                if transition_fallback_logging_enabled() {
                    eprintln!(
                        "flashgrep[query][debug] transition rg fallback failed for repo={} phase={} dirty=({},{},{}) pattern={:?}: {error}",
                        self.repo_id,
                        repo_phase_label(status.phase),
                        status.dirty_files.modified,
                        status.dirty_files.deleted,
                        status.dirty_files.new,
                        request.query.regex_pattern
                    );
                }
                Ok(SearchResponse {
                    repo_id: self.repo_id.clone(),
                    backend: QueryBackend::ScanFallback,
                    status: self.status_snapshot(),
                    results: self.scan_search_fallback(&request.query, filter)?,
                })
            }
        }
    }

    fn scan_search_fallback(
        &self,
        query: &crate::config::QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<crate::search::SearchResults> {
        self.search_engine.scan_search_with_filter(query, filter)
    }
}

fn repo_phase_label(phase: RepoPhase) -> &'static str {
    match phase {
        RepoPhase::Opening => "opening",
        RepoPhase::MissingBaseSnapshot => "missing_base_snapshot",
        RepoPhase::BuildingBaseSnapshot => "building_base_snapshot",
        RepoPhase::ReadyClean => "ready_clean",
        RepoPhase::ReadyDirty => "ready_dirty",
        RepoPhase::RebuildingBaseSnapshot => "rebuilding_base_snapshot",
        RepoPhase::Degraded => "degraded",
    }
}

fn transition_fallback_logging_enabled() -> bool {
    env::var_os("FLASHGREP_LOG_TRANSITION_FALLBACKS").is_some()
}

fn build_path_filter(repo_root: &Path, args: &PathFilterArgs) -> Result<Option<PathFilter>> {
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

fn normalize_repo_scope_args(repo_root: &Path, args: &PathFilterArgs) -> Result<PathFilterArgs> {
    let mut normalized = args.clone();
    normalized.roots = args
        .roots
        .iter()
        .map(|root| {
            let normalized = normalize_path(root, repo_root);
            if normalized == *repo_root || normalized.starts_with(repo_root) {
                Ok(normalized)
            } else {
                Err(AppError::Protocol(format!(
                    "scope root escapes repo boundary: {}",
                    normalized.to_string_lossy()
                )))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(normalized)
}

fn glob_scope_cache_key(args: &PathFilterArgs) -> String {
    fn append_path_list(key: &mut String, values: &[PathBuf]) {
        for value in values {
            key.push('\u{1f}');
            key.push_str(&value.to_string_lossy());
        }
    }

    fn append_string_list(key: &mut String, values: &[String]) {
        for value in values {
            key.push('\u{1f}');
            key.push_str(value);
        }
    }

    let mut key = String::new();
    append_path_list(&mut key, &args.roots);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.globs);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.iglobs);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.type_add);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.type_clear);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.types);
    key.push('\u{1e}');
    append_string_list(&mut key, &args.type_not);
    key
}

fn scan_roots(repo_root: &Path, args: &PathFilterArgs) -> Vec<PathBuf> {
    if args.roots.is_empty() {
        return vec![repo_root.to_path_buf()];
    }
    args.roots
        .iter()
        .map(|root| normalize_path(root, repo_root))
        .collect()
}
