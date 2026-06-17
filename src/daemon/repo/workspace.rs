use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::SystemTime,
};

use crate::{
    error::{AppError, Result},
    progress::{IndexProgress, IndexProgressPhase},
    search_engine::SearchDocument,
    worktree_diff::DirtyPathKind,
};

use super::{DirtyPathSet, RepoPhase, RepoRuntime};
use crate::daemon::repo::status::apply_workspace_dirty_status;

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
    pub(super) fn refresh_workspace_overlay(&self) -> Result<()> {
        let mut noop_progress = |_event: IndexProgress| {};
        self.refresh_workspace_overlay_with_progress(&mut noop_progress)
    }

    pub(super) fn refresh_workspace_overlay_with_progress(
        &self,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    "Planning workspace overlay refresh",
                    0,
                    Some(2),
                ));
                let plan = self.plan_overlay_full_rebuild_from_worktree()?;
                let refreshed_paths = plan.dirty_path_count().max(plan.build_path_count());
                on_progress(IndexProgress::new(
                    IndexProgressPhase::RefreshingOverlay,
                    format!("Applying workspace overlay refresh for {refreshed_paths} path(s)"),
                    1,
                    Some(2),
                ));
                self.apply_refreshed_overlay(snapshot_key, plan);
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
                    None if self.workspace_watcher_active()
                        && self.dirty_catalog_pending_paths().is_empty()
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
                let plan = match self
                    .plan_overlay_update_from_event_probe(events.candidate_paths.into_iter())?
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
        apply_workspace_dirty_status(&mut state, self.rebuild_dirty_threshold, dirty_stats);
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
        apply_workspace_dirty_status(
            &mut state,
            self.rebuild_dirty_threshold,
            super::DirtyStats::default(),
        );
        drop(state);
        self.emit_workspace_status_changed();
    }

    pub(super) fn load_overlay_documents(
        &self,
        dirty: &DirtyPathSet,
    ) -> Result<HashMap<String, SearchDocument>> {
        let mut documents = HashMap::new();
        for path in dirty.modified.iter().chain(dirty.new.iter()) {
            if let Some(document) = self.load_known_corpus_search_document(path)? {
                documents.insert(path.clone(), document);
            }
        }
        Ok(documents)
    }

    pub(super) fn load_delta_overlay_documents(
        &self,
        delta: &DirtyPathSet,
    ) -> Result<HashMap<String, SearchDocument>> {
        let mut documents = HashMap::new();
        for path in delta.modified.iter().chain(delta.new.iter()) {
            if let Some(document) = self.load_known_corpus_search_document(path)? {
                documents.insert(path.clone(), document);
            }
        }
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
