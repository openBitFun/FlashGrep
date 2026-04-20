use std::{collections::HashMap, path::PathBuf, sync::atomic::Ordering, time::SystemTime};

use crate::{
    error::{AppError, Result},
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

    fn upsert(&mut self, path: String, kind: DirtyPathKind) -> bool {
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
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                let plan = self.plan_overlay_full_rebuild_from_worktree()?;
                self.apply_refreshed_overlay(snapshot_key, plan);
                Ok(())
            }
            Ok(None) => {
                self.apply_missing_base_overlay_state(None);
                Ok(())
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.apply_missing_base_overlay_state(None);
                Ok(())
            }
            Err(AppError::InvalidIndex(error)) => {
                self.apply_missing_base_overlay_state(Some(error));
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
        match self.current_base_snapshot_key() {
            Ok(Some(snapshot_key)) => {
                let plan = match self.plan_overlay_update_from_incremental_probe()? {
                    Some(plan) => plan,
                    None => self.plan_overlay_full_rebuild_from_worktree()?,
                };
                self.apply_refreshed_overlay(snapshot_key, plan);
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

    fn apply_refreshed_overlay(
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

    fn clear_overlay(&self) {
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
