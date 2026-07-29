use std::{path::PathBuf, time::SystemTime};

use crate::{
    error::{AppError, Result},
    git_snapshot::resolve_git_head_blocking,
    index_engine::{
        manifest::{FileManifestStore, ManifestSource, ManifestStore},
        WorkspaceBaseSnapshotIndex,
    },
};

use super::RepoRuntime;
use crate::daemon::repo::status::apply_workspace_dirty_status;

impl RepoRuntime {
    pub(super) fn bootstrap_from_existing_snapshot(&self) -> Result<()> {
        self.recover_dirty_catalog_for_startup()?;
        WorkspaceBaseSnapshotIndex::ensure_manifest_source_identity(&self.build_config)?;
        let now = SystemTime::now();
        let compatible_metadata = match self.current_compatible_base_snapshot_metadata() {
            Ok(metadata) => metadata,
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(AppError::InvalidIndex(_)) => None,
            Err(error) => return Err(error),
        };
        let snapshot_key = compatible_metadata
            .as_ref()
            .map(|metadata| metadata.snapshot_key.clone());
        let base_head_commit = compatible_metadata.as_ref().and_then(|metadata| {
            metadata.head_commit.clone().or_else(|| {
                metadata
                    .revision
                    .as_ref()
                    .map(|revision| revision.0.clone())
            })
        });
        let observed_head_commit = self.current_git_head().ok();
        let opened_base_snapshot = compatible_metadata.as_ref().and_then(|_| {
            match WorkspaceBaseSnapshotIndex::open_read_only(&self.build_config) {
                Ok(index) => Some(index),
                Err(error) => {
                    eprintln!(
                        "flashgrep[bootstrap][warn] failed to open base snapshot index engine for {} during bootstrap: {error}",
                        self.build_config.repo_path.display()
                    );
                    None
                }
            }
        });
        let base_snapshot_ready = opened_base_snapshot.is_some();
        let active_manifest = base_snapshot_ready
            .then(|| {
                FileManifestStore::open(&WorkspaceBaseSnapshotIndex::index_root(
                    &self.build_config,
                ))?
                .load_current()
            })
            .transpose()?;
        let (
            base_manifest_id,
            base_delta_depth,
            base_delta_bytes,
            base_delete_segment_count,
            base_foundation_bytes,
        ) = active_manifest
            .as_ref()
            .map_or((None, 0, 0, 0, 0), |manifest| {
                let (delta_depth, delta_bytes, foundation_bytes) = match manifest.source.as_ref() {
                    Some(ManifestSource::GitBase {
                        delta_depth,
                        delta_bytes,
                        foundation_segment_ids,
                        ..
                    }) => {
                        let foundation_bytes = manifest
                            .active_segments
                            .iter()
                            .filter(|segment| foundation_segment_ids.contains(&segment.id))
                            .map(|segment| segment.size_bytes)
                            .sum();
                        (*delta_depth, *delta_bytes, foundation_bytes)
                    }
                    _ => (0, 0, 0),
                };
                (
                    Some(manifest.manifest_id.0.clone()),
                    delta_depth,
                    delta_bytes,
                    manifest.active_delete_segments.len(),
                    foundation_bytes,
                )
            });
        {
            let mut slot = match self.base_snapshot_engine.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = opened_base_snapshot;
        }
        self.clear_cached_base_snapshot_reader();
        if base_snapshot_ready {
            WorkspaceBaseSnapshotIndex::record_active_head_in_cache(
                &self.build_config,
                self.base_head_cache_entries,
            )?;
            let prewarm_result = {
                let guard = match self.base_snapshot_engine.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard
                    .as_ref()
                    .map(|index| {
                        let reader = index.open_committed()?;
                        self.store_cached_base_snapshot_reader(reader);
                        Ok::<(), AppError>(())
                    })
                    .transpose()
            };
            if let Err(error) = prewarm_result {
                eprintln!(
                    "flashgrep[bootstrap][warn] failed to prewarm base snapshot search state for {} during bootstrap: {error}",
                    self.build_config.repo_path.display()
                );
            }
        }
        self.clear_base_snapshot_catalog();
        if base_snapshot_ready {
            if let Err(error) = self.preload_base_snapshot_catalog() {
                eprintln!(
                    "flashgrep[bootstrap][warn] failed to preload base snapshot catalog for {} during bootstrap: {error}",
                    self.build_config.repo_path.display()
                );
                self.clear_base_snapshot_catalog();
            }
        }

        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.snapshot_key = base_snapshot_ready.then_some(snapshot_key).flatten();
        state.base_head_commit = base_snapshot_ready.then_some(base_head_commit).flatten();
        state.observed_head_commit = observed_head_commit;
        state.overlay_head_commit = None;
        state.overlay_base_manifest_id = None;
        state.base_manifest_id = base_manifest_id;
        state.base_delta_depth = base_delta_depth;
        state.base_delta_bytes = base_delta_bytes;
        state.base_delete_segment_count = base_delete_segment_count;
        state.base_foundation_bytes = base_foundation_bytes;
        state.last_probe_at = Some(now);
        state.last_error = None;
        apply_workspace_dirty_status(&mut state, super::DirtyStats::default());
        drop(state);
        self.emit_workspace_status_changed();
        Ok(())
    }

    pub(super) fn current_git_head(&self) -> Result<String> {
        resolve_git_head_blocking(&self.build_config.repo_path)?.ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "daemon workspace requires a Git HEAD commit for {}",
                self.build_config.repo_path.display()
            ))
        })
    }

    pub(super) fn observe_git_head_transition(&self) -> Result<bool> {
        let current_head = self.current_git_head()?;
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.observed_head_commit.as_deref() == Some(&current_head) {
            return Ok(false);
        }
        state.observed_head_commit = Some(current_head);
        state.overlay_head_commit = None;
        state.overlay_base_manifest_id = None;
        if state.snapshot_key.is_some() {
            state.phase = super::RepoPhase::ReadyDirty;
        }
        drop(state);
        self.emit_workspace_status_changed();
        Ok(true)
    }

    pub(super) fn branch_transition_pending(&self) -> bool {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.base_head_commit != state.observed_head_commit
            && state.overlay_head_commit != state.observed_head_commit
    }

    pub(super) fn overlay_manifest_source(
        &self,
        observed_head: &str,
    ) -> Result<crate::index_engine::manifest::ManifestSource> {
        let state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let base_manifest_id = state.base_manifest_id.clone().ok_or_else(|| {
            AppError::InvalidIndex("base manifest identity is unavailable".into())
        })?;
        Ok(
            crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                base_manifest_id: crate::index_engine::ManifestId(base_manifest_id),
                observed_head: observed_head.to_string(),
                probe_epoch: self
                    .glob_cache_epoch
                    .load(std::sync::atomic::Ordering::Relaxed),
            },
        )
    }

    pub(super) fn mark_overlay_published_for_head(&self, source_head: Option<String>) {
        let current_head = self.current_git_head().ok();
        let aligned = source_head.is_some() && source_head == current_head;
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.observed_head_commit = current_head;
        state.overlay_head_commit = aligned.then_some(source_head).flatten();
        state.overlay_base_manifest_id = aligned.then(|| state.base_manifest_id.clone()).flatten();
        let should_advance = aligned
            && state.base_head_commit.is_some()
            && state.base_head_commit != state.observed_head_commit;
        drop(state);
        if !aligned {
            self.schedule_workspace_probe(std::time::Duration::from_millis(0));
        } else if should_advance {
            self.schedule_base_advance();
        }
    }

    pub(super) fn open_base_snapshot_reader(
        &self,
    ) -> Result<Option<crate::index_engine::IndexSnapshotReader>> {
        if let Some(reader) = self.cached_base_snapshot_reader() {
            return Ok(Some(reader));
        }
        let base_engine = match self.base_snapshot_engine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let reader = base_engine
            .as_ref()
            .map(WorkspaceBaseSnapshotIndex::open_committed)
            .transpose()?;
        if let Some(reader) = reader.as_ref() {
            self.store_cached_base_snapshot_reader(reader.clone());
        }
        Ok(reader)
    }

    fn cached_base_snapshot_reader(&self) -> Option<crate::index_engine::IndexSnapshotReader> {
        let cache = match self.cached_base_snapshot_reader.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.clone()
    }

    fn store_cached_base_snapshot_reader(&self, reader: crate::index_engine::IndexSnapshotReader) {
        let mut cache = match self.cached_base_snapshot_reader.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cache = Some(reader);
    }

    fn clear_cached_base_snapshot_reader(&self) {
        let mut cache = match self.cached_base_snapshot_reader.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *cache = None;
    }

    pub(super) fn is_storage_path(&self, path: &std::path::Path) -> bool {
        self.normalize_repo_workspace_path(path.to_path_buf())
            .is_some_and(|candidate| {
                candidate.starts_with(self.normalized_storage_root())
                    || candidate.starts_with(self.normalized_workspace_engine_root())
            })
    }

    pub(super) fn normalized_storage_root(&self) -> PathBuf {
        if let Ok(canonical) = std::fs::canonicalize(&self.build_config.storage_root) {
            return canonical;
        }
        let Some(parent) = self.build_config.storage_root.parent() else {
            return self.build_config.storage_root.clone();
        };
        let Some(file_name) = self.build_config.storage_root.file_name() else {
            return self.build_config.storage_root.clone();
        };
        match std::fs::canonicalize(parent) {
            Ok(canonical_parent) => canonical_parent.join(file_name),
            Err(_) => self.build_config.storage_root.clone(),
        }
    }

    pub(super) fn normalized_workspace_engine_root(&self) -> PathBuf {
        normalize_repo_sidecar_path(workspace_engine_root(&self.build_config))
    }
}

pub(super) fn workspace_engine_root(build_config: &crate::config::BuildConfig) -> PathBuf {
    build_config.storage_root.clone()
}

fn normalize_repo_sidecar_path(path: PathBuf) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }
    let Some(parent) = path.parent() else {
        return path;
    };
    let Some(file_name) = path.file_name() else {
        return path;
    };
    match std::fs::canonicalize(parent) {
        Ok(canonical_parent) => canonical_parent.join(file_name),
        Err(_) => path,
    }
}
