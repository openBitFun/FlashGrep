use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use crate::{
    error::{AppError, Result},
    progress::{IndexProgress, IndexProgressPhase},
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
        // Always insert after removing the previous classification. Short-circuit
        // boolean evaluation here would silently discard every existing path.
        let inserted = match kind {
            DirtyPathKind::Modified => self.modified.insert(path),
            DirtyPathKind::Deleted => self.deleted.insert(path),
            DirtyPathKind::New => self.new.insert(path),
        };
        changed || inserted
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
}

impl RepoRuntime {
    /// Reuses the overlay the previous session persisted, without probing the
    /// worktree.
    ///
    /// Authoritative reconciliation walks the filesystem, which can be costly
    /// on a large workspace, so `open_repo`
    /// only restores what is already on disk and leaves the reconcile to the
    /// maintenance worker. The restored overlay is exact for everything the
    /// daemon observed before it closed and stale for edits made while it was
    /// closed, so callers must treat it as last-known state: `open_repo` keeps
    /// `workspace_probe_pending` set until the deferred probe lands, and a
    /// caller that needs authoritative results asks for `refresh_repo` instead
    /// of silently accepting the restored snapshot.
    ///
    /// Returns whether a matching overlay was installed. A missing, invalid, or
    /// mismatched overlay is not an error here — the deferred probe rebuilds it
    /// — so this never walks the worktree itself.
    pub(super) fn restore_persisted_workspace_overlay(&self) -> Result<bool> {
        if self.current_base_snapshot_key()?.is_none() {
            self.apply_missing_base_overlay_state(None)?;
            return Ok(false);
        }

        let current_head = self.current_git_head()?;
        let overlay = match self.open_workspace_overlay_index() {
            Ok(overlay) => overlay,
            Err(AppError::InvalidIndex(error)) => {
                eprintln!(
                    "flashgrep[overlay][warn] persisted overlay is invalid for {}; the deferred probe will rebuild it: {error}",
                    self.build_config.repo_path.display()
                );
                return Ok(false);
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "flashgrep[overlay][info] persisted overlay is missing for {}; the deferred probe will rebuild it",
                    self.build_config.repo_path.display()
                );
                return Ok(false);
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
                "flashgrep[overlay][info] persisted overlay source does not match the active base for {}; the deferred probe will rebuild it",
                self.build_config.repo_path.display()
            );
            return Ok(false);
        }

        {
            let mut slot = match self.workspace_overlay_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = Some(std::sync::Arc::new(overlay));
        }
        self.mark_overlay_published_for_head(current_head);
        Ok(true)
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
        // Called by the base task while it holds workspace_discovery_lock.

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
        self.apply_overlay_write_plan(plan)?;
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
        let _discovery = self
            .workspace_discovery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
                    self.apply_refreshed_overlay(snapshot_key.clone(), plan)?;
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
                self.apply_missing_base_overlay_state(None)?;
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Skipped workspace overlay refresh because the base snapshot is missing",
                    1,
                    Some(1),
                ));
                Ok(())
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None)?;
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Skipped workspace overlay refresh because the base snapshot is missing",
                    1,
                    Some(1),
                ));
                Ok(())
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error))?;
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
        let _discovery = self
            .workspace_discovery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

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
                    self.apply_refreshed_overlay(snapshot_key, plan)?;
                } else {
                    self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
                }
                Ok(true)
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None)?;
                Ok(true)
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None)?;
                Ok(true)
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error))?;
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
        let result = self.probe_workspace_event_batch();
        if result.is_err() {
            // A failed scan may not have reached the dirty catalog. Do not let
            // a later, unrelated event replace the consumed batch's recovery.
            self.enqueue_full_workspace_probe();
        }
        result
    }

    fn probe_workspace_event_batch(&self) -> Result<bool> {
        let events = self.take_workspace_events();
        if !events.use_fast_path() {
            return self.probe_workspace_overlay();
        }
        let _discovery = self
            .workspace_discovery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        record_probe_mode_for_test("fast");
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                let plan = match self
                    .plan_overlay_update_from_event_probe(events.candidate_paths, events.revision)?
                {
                    Some(plan) => Some(plan),
                    None if self.dirty_catalog_pending_paths().is_empty()
                        && self.snapshot_published_overlay_paths()?.is_some() =>
                    {
                        None
                    }
                    None => Some(self.plan_overlay_full_rebuild_from_worktree()?),
                };
                if let Some(plan) = plan {
                    self.apply_refreshed_overlay(snapshot_key, plan)?;
                } else {
                    self.finish_workspace_probe_without_overlay_change(Some(snapshot_key));
                }
                Ok(true)
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None)?;
                Ok(true)
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None)?;
                Ok(true)
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error))?;
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
    ) -> Result<()> {
        self.apply_overlay_write_plan(plan)?;
        self.complete_workspace_event_probe();
        self.set_probe_healthy(true);
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = Some(snapshot_key);
        state.last_probe_at = Some(SystemTime::now());
        state.last_error = None;
        drop(state);
        Ok(())
    }

    pub(super) fn finish_workspace_probe_without_overlay_change(
        &self,
        snapshot_key: Option<String>,
    ) {
        self.complete_workspace_event_probe();
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

    fn apply_missing_base_overlay_state(&self, last_error: Option<String>) -> Result<()> {
        self.clear_overlay()?;
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
        Ok(())
    }

    pub(super) fn stream_overlay_batches<'a>(
        &'a self,
        delta: &'a DirtyPathSet,
    ) -> impl Iterator<Item = Result<super::WorkspaceOverlayIngestBatch>> + 'a {
        delta
            .modified
            .iter()
            .map(|path| (path, DirtyPathKind::Modified))
            .chain(delta.new.iter().map(|path| (path, DirtyPathKind::New)))
            .chain(
                delta
                    .deleted
                    .iter()
                    .map(|path| (path, DirtyPathKind::Deleted)),
            )
            .map(|(path, kind)| {
                let mut paths = DirtyPathSet::default();
                let mut documents = HashMap::new();
                match kind {
                    DirtyPathKind::Modified => {
                        paths.modified.insert(path.clone());
                    }
                    DirtyPathKind::New => {
                        paths.new.insert(path.clone());
                    }
                    DirtyPathKind::Deleted => {
                        paths.deleted.insert(path.clone());
                    }
                }
                if kind != DirtyPathKind::Deleted {
                    if let Some(document) = self.load_known_corpus_search_document(path)? {
                        documents.insert(path.clone(), document);
                    }
                }
                super::WorkspaceOverlayIngestBatch::from_paths(
                    &self.overlay_repo_id,
                    &self.build_config.repo_path,
                    &paths.modified,
                    &paths.new,
                    &paths.deleted,
                    &documents,
                )
            })
    }

    pub(super) fn clear_overlay(&self) -> Result<()> {
        self.reset_workspace_overlay_engine()?;
        {
            let mut published_overlay_paths_cache = match self.published_overlay_paths_cache.lock()
            {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *published_overlay_paths_cache = DirtyPathSet::default();
        }
        self.invalidate_glob_cache();
        self.complete_workspace_event_probe();
        Ok(())
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
    #[test]
    fn repeated_dirty_updates_preserve_paths_across_all_kind_transitions() {
        let kinds = [
            DirtyPathKind::Modified,
            DirtyPathKind::Deleted,
            DirtyPathKind::New,
        ];
        for before in kinds {
            for after in kinds {
                let mut dirty = DirtyPathSet::default();
                dirty.upsert("/repo/changed.rs".into(), before);
                dirty.upsert("/repo/kept.rs".into(), DirtyPathKind::Modified);
                assert!(dirty.upsert("/repo/changed.rs".into(), after));
                assert_eq!(dirty.kind_for_path("/repo/changed.rs"), Some(after));
                assert_eq!(
                    dirty.kind_for_path("/repo/kept.rs"),
                    Some(DirtyPathKind::Modified)
                );
                let stats = dirty.stats();
                assert_eq!(stats.modified + stats.deleted + stats.new, 2);
                let mut event = DirtyPathSet::default();
                event.upsert("/repo/changed.rs".into(), after);
                dirty.merge(event);
                assert_eq!(dirty.kind_for_path("/repo/changed.rs"), Some(after));
            }
        }
    }
}
