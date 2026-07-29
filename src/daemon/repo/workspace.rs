use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::{Instant, SystemTime},
};

use rayon::prelude::*;

use crate::{
    error::{AppError, Result},
    progress::{IndexProgress, IndexProgressPhase},
    search_engine::SearchDocument,
    worktree_diff::DirtyPathKind,
};

use super::{overlay::WorkspaceOverlayWritePlan, DirtyPathSet, RepoPhase, RepoRuntime};
use crate::daemon::repo::status::apply_workspace_dirty_status;

#[derive(Debug, Clone, Copy)]
enum OverlayRefreshMode {
    Incremental,
    FullRebuild,
}

impl DirtyPathSet {
    pub(super) fn stats(&self) -> super::DirtyStats {
        super::DirtyStats {
            modified: self.modified.len(),
            deleted: self.deleted.len(),
            new: self.new.len(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.new.is_empty()
    }

    pub(super) fn paths(&self) -> Vec<PathBuf> {
        self.modified
            .iter()
            .chain(self.deleted.iter())
            .chain(self.new.iter())
            .map(PathBuf::from)
            .collect()
    }

    pub(super) fn upsert(&mut self, path: String, kind: DirtyPathKind) -> bool {
        let changed = self.remove(&path);
        match kind {
            DirtyPathKind::Modified => changed || self.modified.insert(path),
            DirtyPathKind::Deleted => changed || self.deleted.insert(path),
            DirtyPathKind::New => changed || self.new.insert(path),
        }
    }

    pub(super) fn remove(&mut self, path: &str) -> bool {
        let removed_modified = self.modified.remove(path);
        let removed_deleted = self.deleted.remove(path);
        let removed_new = self.new.remove(path);
        removed_modified || removed_deleted || removed_new
    }

    pub(super) fn remove_path_scope(&mut self, path: &Path) -> bool {
        self.remove_matching(|dirty_path| {
            let dirty_path = Path::new(dirty_path);
            dirty_path == path || dirty_path.starts_with(path)
        })
    }

    fn remove_matching(&mut self, mut matches: impl FnMut(&str) -> bool) -> bool {
        let modified_before = self.modified.len();
        self.modified.retain(|path| !matches(path));
        let deleted_before = self.deleted.len();
        self.deleted.retain(|path| !matches(path));
        let new_before = self.new.len();
        self.new.retain(|path| !matches(path));
        modified_before != self.modified.len()
            || deleted_before != self.deleted.len()
            || new_before != self.new.len()
    }

    pub(super) fn merge(&mut self, other: DirtyPathSet) -> bool {
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

    pub(super) fn kind_for_path(&self, path: &str) -> Option<DirtyPathKind> {
        if self.modified.contains(path) {
            Some(DirtyPathKind::Modified)
        } else if self.deleted.contains(path) {
            Some(DirtyPathKind::Deleted)
        } else if self.new.contains(path) {
            Some(DirtyPathKind::New)
        } else {
            None
        }
    }

    pub(super) fn has_paths_cleaned_since(&self, previous: &DirtyPathSet) -> bool {
        previous
            .modified
            .iter()
            .chain(previous.deleted.iter())
            .chain(previous.new.iter())
            .any(|path| self.kind_for_path(path).is_none())
    }
}

impl RepoRuntime {
    pub(super) fn restore_or_refresh_workspace_overlay(&self) -> Result<()> {
        let Some(snapshot_key) = self.current_base_snapshot_key()? else {
            self.apply_missing_base_overlay_state(None);
            return Ok(());
        };

        let (candidate_dirty, candidate_paths) = self.startup_worktree_candidates()?;
        let dirty = if self.dirty_catalog_visible_paths() == candidate_dirty {
            candidate_dirty
        } else {
            self.diff_against_base_snapshot_git_worktree_paths(&candidate_paths)?
        };
        let current_head = self.current_git_head()?;
        let overlay = match self.open_workspace_overlay_index_manifest_only() {
            Ok(overlay) => overlay,
            Err(AppError::InvalidIndex(error)) => {
                eprintln!(
                    "flashgrep[overlay][warn] persisted overlay is invalid for {}; rebuilding: {error}",
                    self.build_config.repo_path.display()
                );
                return self.rebuild_workspace_overlay();
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "flashgrep[overlay][info] persisted overlay is missing for {}; rebuilding",
                    self.build_config.repo_path.display()
                );
                return self.rebuild_workspace_overlay();
            }
            Err(error) => return Err(error),
        };
        let source_matches = match overlay.manifest_source()? {
            Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                base_manifest_id,
                observed_head,
                ..
            }) => {
                let state = match self.state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                state.base_manifest_id.as_deref() == Some(base_manifest_id.0.as_str())
                    && observed_head == current_head
            }
            _ => false,
        };
        if !source_matches {
            eprintln!(
                "flashgrep[overlay][info] persisted overlay source does not match the active base for {}; rebuilding",
                self.build_config.repo_path.display()
            );
            return self.rebuild_workspace_overlay();
        }

        {
            let mut slot = match self.workspace_overlay_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = Some(std::sync::Arc::new(overlay));
        }
        let plan = self.plan_overlay_update_from_dirty_probe(dirty)?;
        if let Some(plan) = plan {
            self.apply_refreshed_overlay(snapshot_key, plan);
        } else {
            self.mark_overlay_published_for_head(Some(current_head));
            self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
        }
        Ok(())
    }

    pub(super) fn refresh_workspace_overlay(&self) -> Result<()> {
        let mut noop_progress = |_event: IndexProgress| {};
        self.refresh_workspace_overlay_with_progress(&mut noop_progress)
    }

    pub(super) fn rebuild_workspace_overlay(&self) -> Result<()> {
        let mut noop_progress = |_event: IndexProgress| {};
        self.rebuild_workspace_overlay_with_progress(&mut noop_progress)
    }

    pub(super) fn refresh_workspace_overlay_with_progress(
        &self,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        self.refresh_workspace_overlay_with_mode(on_progress, OverlayRefreshMode::Incremental)
    }

    pub(super) fn rebuild_workspace_overlay_with_progress(
        &self,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        self.refresh_workspace_overlay_with_mode(on_progress, OverlayRefreshMode::FullRebuild)
    }

    pub(super) fn finish_base_transition_overlay(
        &self,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        let started = Instant::now();
        on_progress(IndexProgress::new(
            IndexProgressPhase::RefreshingOverlay,
            "Reconciling workspace overlay with the new base",
            0,
            Some(2),
        ));
        let candidate_paths = self.dirty_catalog_visible_paths().paths();
        let dirty = self.classify_paths_against_base_snapshot(
            candidate_paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned()),
        )?;
        let generation = self.reconcile_dirty_catalog(&dirty)?;
        let dirty_count = dirty.stats().modified + dirty.stats().deleted + dirty.stats().new;
        let plan = self
            .build_overlay_rebuild_plan(dirty)?
            .with_catalog_generation(generation);
        self.apply_overlay_write_plan(plan);
        on_progress(IndexProgress::new(
            IndexProgressPhase::RefreshingOverlay,
            format!("Published workspace overlay refresh for {dirty_count} path(s)"),
            2,
            Some(2),
        ));
        eprintln!(
            "flashgrep[overlay-metrics][info] stage=finish_base_transition_overlay elapsed_ms={} action=catalog_rebuild candidate_paths={} dirty_paths={}",
            started.elapsed().as_millis(),
            candidate_paths.len(),
            dirty_count,
        );
        Ok(())
    }

    fn refresh_workspace_overlay_with_mode(
        &self,
        on_progress: &mut dyn FnMut(IndexProgress),
        mode: OverlayRefreshMode,
    ) -> Result<()> {
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Planning workspace overlay refresh",
                    0,
                    Some(2),
                ));
                let planning_started_at = Instant::now();
                let plan = match mode {
                    OverlayRefreshMode::Incremental => {
                        self.plan_overlay_update_from_incremental_probe()?
                    }
                    OverlayRefreshMode::FullRebuild => {
                        Some(self.plan_overlay_full_rebuild_from_worktree()?)
                    }
                };
                let planning_stage = match mode {
                    OverlayRefreshMode::Incremental => "plan_incremental",
                    OverlayRefreshMode::FullRebuild => "plan_full_rebuild",
                };
                let dirty_path_count = plan
                    .as_ref()
                    .map(WorkspaceOverlayWritePlan::dirty_path_count)
                    .unwrap_or_default();
                let build_path_count = plan
                    .as_ref()
                    .map(WorkspaceOverlayWritePlan::build_path_count)
                    .unwrap_or_default();
                eprintln!(
                    "flashgrep[overlay-metrics][info] stage={} elapsed_ms={} dirty_paths={} build_paths={}",
                    planning_stage,
                    planning_started_at.elapsed().as_millis(),
                    dirty_path_count,
                    build_path_count
                );
                let refreshed_paths = dirty_path_count.max(build_path_count);
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    format!("Applying workspace overlay refresh for {refreshed_paths} path(s)"),
                    1,
                    Some(2),
                ));
                if let Some(plan) = plan {
                    self.apply_refreshed_overlay(snapshot_key.clone(), plan);
                } else {
                    self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
                }
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    format!("Published workspace overlay refresh for {refreshed_paths} path(s)"),
                    2,
                    Some(2),
                ));
                Ok(())
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None);
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Skipped workspace overlay refresh because the base snapshot is missing",
                    1,
                    Some(1),
                ));
                Ok(())
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None);
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Skipped workspace overlay refresh because the base snapshot is missing",
                    1,
                    Some(1),
                ));
                Ok(())
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error));
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Skipped workspace overlay refresh because the base snapshot is invalid",
                    1,
                    Some(1),
                ));
                Ok(())
            }
            Err(error) => {
                self.set_probe_healthy(false);
                self.set_phase(RepoPhase::Degraded, Some(error.to_string()));
                Err(error)
            }
        }
    }

    pub(super) fn probe_workspace_overlay(&self) -> Result<bool> {
        record_probe_mode_for_test("full");
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                let plan = match self.plan_overlay_update_from_incremental_probe()? {
                    Some(plan) => Some(plan),
                    None if self.dirty_catalog_pending_paths().is_empty()
                        && self.snapshot_published_overlay_paths()?.is_some() =>
                    {
                        None
                    }
                    None => Some(self.plan_overlay_full_rebuild_from_worktree()?),
                };
                if let Some(plan) = plan {
                    self.apply_refreshed_overlay(snapshot_key, plan);
                } else {
                    self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
                }
                Ok(true)
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None);
                Ok(true)
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None);
                Ok(true)
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error));
                Ok(true)
            }
            Err(error) => {
                self.set_probe_healthy(false);
                self.set_phase(RepoPhase::Degraded, Some(error.to_string()));
                Err(error)
            }
        }
    }

    pub(super) fn probe_workspace_overlay_from_events(&self) -> Result<bool> {
        let events = self.take_workspace_events();
        if !events.use_fast_path() {
            return self.probe_workspace_overlay();
        }
        record_probe_mode_for_test("fast");
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                let plan =
                    match self.plan_overlay_update_from_event_probe(events.candidate_paths)? {
                        Some(plan) => Some(plan),
                        None if self.dirty_catalog_pending_paths().is_empty()
                            && self.snapshot_published_overlay_paths()?.is_some() =>
                        {
                            None
                        }
                        None => Some(self.plan_overlay_full_rebuild_from_worktree()?),
                    };
                if let Some(plan) = plan {
                    self.apply_refreshed_overlay(snapshot_key, plan);
                } else {
                    self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
                }
                Ok(true)
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None);
                Ok(true)
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None);
                Ok(true)
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error));
                Ok(true)
            }
            Err(error) => {
                self.set_probe_healthy(false);
                self.set_phase(RepoPhase::Degraded, Some(error.to_string()));
                Err(error)
            }
        }
    }

    pub(super) fn apply_refreshed_overlay(
        &self,
        snapshot_key: String,
        plan: super::overlay::WorkspaceOverlayWritePlan,
    ) {
        self.workspace_probe_pending.store(false, Ordering::Relaxed);
        self.set_probe_healthy(true);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = Some(snapshot_key);
        state.last_probe_at = Some(SystemTime::now());
        state.last_error = None;
        drop(state);
        self.apply_overlay_write_plan(plan);
    }

    pub(super) fn finish_workspace_probe_without_overlay_change(
        &self,
        snapshot_key: Option<String>,
    ) {
        self.workspace_probe_pending.store(false, Ordering::Relaxed);
        self.set_probe_healthy(true);
        let dirty_stats = self.dirty_catalog_stats();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = snapshot_key;
        state.last_probe_at = Some(SystemTime::now());
        state.last_error = None;
        apply_workspace_dirty_status(&mut state, dirty_stats);
        drop(state);
        self.invalidate_glob_cache();
        self.emit_workspace_status_changed();
    }

    fn apply_missing_base_overlay_state(&self, last_error: Option<String>) {
        self.clear_overlay();
        self.set_probe_healthy(true);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = None;
        state.last_probe_at = Some(SystemTime::now());
        state.last_error = last_error;
        apply_workspace_dirty_status(&mut state, super::DirtyStats::default());
        drop(state);
        self.emit_workspace_status_changed();
    }

    pub(super) fn load_overlay_documents(
        &self,
        dirty: &DirtyPathSet,
    ) -> Result<HashMap<String, SearchDocument>> {
        self.load_overlay_documents_for_paths(dirty.modified.iter().chain(dirty.new.iter()))
    }

    pub(super) fn load_delta_overlay_documents(
        &self,
        delta: &DirtyPathSet,
    ) -> Result<HashMap<String, SearchDocument>> {
        self.load_overlay_documents_for_paths(delta.modified.iter().chain(delta.new.iter()))
    }

    fn load_overlay_documents_for_paths<'a>(
        &self,
        paths: impl Iterator<Item = &'a String>,
    ) -> Result<HashMap<String, SearchDocument>> {
        let paths = paths.cloned().collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let worker_count =
            std::thread::available_parallelism().map_or(1, |parallelism| parallelism.get().min(8));
        let started_at = Instant::now();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count)
            .build()
            .map_err(|error| {
                AppError::Protocol(format!("failed to build overlay worker pool: {error}"))
            })?;
        let documents = pool.install(|| {
            paths
                .par_iter()
                .map(|path| {
                    self.load_known_corpus_search_document(path)
                        .map(|document| (path.clone(), document))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let documents = documents
            .into_iter()
            .filter_map(|(path, document)| document.map(|document| (path, document)))
            .collect::<HashMap<_, _>>();
        let source_bytes = documents
            .values()
            .map(|document| document.size)
            .sum::<u64>();
        eprintln!(
            "flashgrep[overlay-metrics][info] stage=prepare_documents elapsed_ms={} input_paths={} documents={} source_bytes={} workers={}",
            started_at.elapsed().as_millis(),
            paths.len(),
            documents.len(),
            source_bytes,
            worker_count
        );
        Ok(documents)
    }

    pub(super) fn clear_overlay(&self) {
        {
            let mut published_overlay_paths_cache = match self.published_overlay_paths_cache.lock()
            {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *published_overlay_paths_cache = DirtyPathSet::default();
        }
        self.reset_workspace_overlay_engine();
        self.invalidate_glob_cache();
        self.workspace_probe_pending.store(false, Ordering::Relaxed);
    }

    pub(super) fn normalize_repo_workspace_path(&self, absolute: PathBuf) -> Option<PathBuf> {
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
}

#[cfg(test)]
fn record_probe_mode_for_test(mode: &'static str) {
    use std::io::Write;

    let Some(path) = std::env::var_os("FLASHGREP_TEST_PROBE_MODE_FILE") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{mode}");
    }
}

#[cfg(not(test))]
fn record_probe_mode_for_test(_mode: &'static str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_path_set_removes_directory_scope_without_prefix_bleed() {
        let mut dirty = DirtyPathSet::default();
        dirty.modified.insert("/repo/src/lib.rs".into());
        dirty.deleted.insert("/repo/src/nested/old.rs".into());
        dirty.new.insert("/repo/src2/kept.rs".into());

        assert!(dirty.remove_path_scope(Path::new("/repo/src")));

        assert!(dirty.modified.is_empty());
        assert!(dirty.deleted.is_empty());
        assert!(dirty.new.contains("/repo/src2/kept.rs"));
    }
}
