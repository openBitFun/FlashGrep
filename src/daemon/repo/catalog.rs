use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    config::CorpusMode,
    error::Result,
    files::{
        is_corpus_text_file, is_workspace_internal_path, repo_relative_path, resolve_repo_path,
    },
    git_snapshot::canonical_repo_root,
    index_engine::{
        reader::SnapshotPathCatalog, workspace::WorkspaceBaseSnapshotMetadata,
        WorkspaceBaseSnapshotIndex,
    },
    search_engine::SearchDocument,
};

use super::{
    overlay::WorkspaceOverlayWritePlan, CachedBaseSnapshotCatalog, DirtyPathSet, RepoRuntime,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayUpdateMode {
    NoChange,
    Rebuild,
    ApplyDelta,
}

impl RepoRuntime {
    pub(super) fn current_base_publication_id(&self) -> Result<crate::index_engine::ManifestId> {
        use crate::index_engine::manifest::ManifestStore;
        Ok(crate::index_engine::manifest::FileManifestStore::open(
            &WorkspaceBaseSnapshotIndex::index_root(&self.build_config),
        )?
        .load_current()?
        .manifest_id)
    }

    fn validate_discovery_base(
        &self,
        expected: &crate::index_engine::ManifestId,
        plan: Option<&WorkspaceOverlayWritePlan>,
    ) -> Result<()> {
        if self.current_base_publication_id()? != *expected
            || plan
                .and_then(WorkspaceOverlayWritePlan::base_manifest_id)
                .is_some_and(|id| id != expected)
        {
            return Err(crate::error::AppError::InvalidIndex(
                "base publication changed during workspace discovery".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn preload_base_snapshot_catalog(&self) -> Result<()> {
        let _ = self.base_snapshot_catalog()?;
        Ok(())
    }

    pub(super) fn plan_overlay_full_rebuild_from_worktree(
        &self,
    ) -> Result<WorkspaceOverlayWritePlan> {
        let revision = self.observe_workspace_revision();
        let base_id = self.current_base_publication_id()?;
        let dirty = self.diff_against_base_snapshot_worktree()?;
        let catalog_generation = self.reconcile_dirty_catalog(&dirty)?;
        let plan = self.build_overlay_rebuild_plan_at_generation(dirty, catalog_generation)?;
        self.validate_discovery_base(&base_id, Some(&plan))?;
        self.record_workspace_reconciled(revision);
        Ok(plan.with_discovery_revision(revision))
    }

    pub(super) fn plan_overlay_update_from_incremental_probe(
        &self,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let revision = self.observe_workspace_revision();
        let base_id = self.current_base_publication_id()?;
        let dirty = self.diff_against_base_snapshot_worktree()?;
        let plan = self.plan_overlay_update_from_dirty_probe(dirty)?;
        self.validate_discovery_base(&base_id, plan.as_ref())?;
        self.record_workspace_reconciled(revision);
        if plan.is_none() {
            // The validated published view already matches this discovery.
            self.record_workspace_published(revision);
        }
        Ok(plan.map(|plan| plan.with_discovery_revision(revision)))
    }

    pub(super) fn plan_overlay_update_from_event_probe(
        &self,
        candidate_paths: impl IntoIterator<Item = PathBuf>,
        revision: u64,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let candidate_paths = candidate_paths
            .into_iter()
            .filter(|path| !self.is_storage_path(path) && !is_workspace_internal_path(path))
            .collect::<Vec<_>>();
        if candidate_paths.is_empty() {
            return Ok(None);
        }
        let base_id = self.current_base_publication_id()?;
        let delta = self.diff_against_base_snapshot_event_paths(&candidate_paths)?;
        // A watcher notification is evidence beyond size/mtime. Re-observe dirty files
        // even when their metadata matches the previously published overlay.
        self.invalidate_observed_dirty_paths(&delta);
        let mut dirty = self.dirty_catalog_visible_paths();
        for path in candidate_paths {
            dirty.remove_path_scope(&path);
        }
        dirty.merge(delta);
        let plan = self.plan_overlay_update_from_dirty_probe(dirty)?;
        self.validate_discovery_base(&base_id, plan.as_ref())?;
        self.record_workspace_reconciled(revision);
        if plan.is_none() {
            // The validated published view already matches this discovery.
            self.record_workspace_published(revision);
        }
        Ok(plan.map(|plan| plan.with_discovery_revision(revision)))
    }

    pub(super) fn plan_overlay_update_from_dirty_probe(
        &self,
        dirty: DirtyPathSet,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let generation_before = self.dirty_catalog_generation();
        let catalog_generation = self.reconcile_dirty_catalog(&dirty)?;
        let catalog_changed = catalog_generation != generation_before;
        let published_overlay_paths = self.published_overlay_paths()?;
        let pending_dirty = self.dirty_catalog_pending_paths();
        let published_overlay_snapshot_available =
            self.snapshot_published_overlay_paths()?.is_some() || dirty.is_empty();

        // Incremental probe rules:
        // - If the current dirty set already matches the published overlay and nothing is pending,
        //   keep the current published snapshot if it is readable.
        // - Paths restored to the base receive a retraction in the delta, preserving
        //   unrelated dirty postings and removing their base-shadowing markers.
        // - Otherwise publish only the pending delta paths.
        let mode = decide_overlay_update_mode(
            &dirty,
            &published_overlay_paths,
            &pending_dirty,
            published_overlay_snapshot_available,
        );
        if mode == OverlayUpdateMode::NoChange
            && (catalog_changed
                || self
                    .dirty_catalog_persist_pending
                    .load(std::sync::atomic::Ordering::Acquire))
        {
            self.persist_dirty_catalog()?;
        }
        match mode {
            OverlayUpdateMode::NoChange => {
                let current_head = self.current_git_head()?;
                let overlay = self
                    .workspace_overlay_engine
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let source = overlay
                    .as_ref()
                    .map(|overlay| {
                        overlay
                            .open_snapshot()
                            .map(|snapshot| snapshot.manifest().source.clone())
                    })
                    .transpose()?
                    .flatten();
                let (published_base, published_head) = match source {
                    Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                        base_manifest_id,
                        observed_head,
                        ..
                    }) => (Some(base_manifest_id.0), observed_head),
                    _ => (None, None),
                };
                let base_changed = published_base
                    != self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .base_manifest_id;
                let head_changed = published_head != current_head;
                if base_changed {
                    self.build_overlay_rebuild_plan_at_generation(dirty, catalog_generation)
                        .map(Some)
                } else if head_changed {
                    // A commit can change HEAD without changing files already in
                    // the overlay. Publish its identity so base advance can proceed.
                    self.build_overlay_delta_plan(
                        dirty,
                        DirtyPathSet::default(),
                        catalog_generation,
                    )
                    .map(Some)
                } else {
                    Ok(None)
                }
            }
            OverlayUpdateMode::Rebuild => self
                .build_overlay_rebuild_plan_at_generation(dirty, catalog_generation)
                .map(Some),
            OverlayUpdateMode::ApplyDelta => self
                .build_overlay_delta_plan(dirty, pending_dirty, catalog_generation)
                .map(Some),
        }
    }

    pub(super) fn build_overlay_rebuild_plan(
        &self,
        dirty: DirtyPathSet,
    ) -> Result<WorkspaceOverlayWritePlan> {
        self.build_overlay_rebuild_plan_at_generation(dirty, self.dirty_catalog_generation())
    }

    fn build_overlay_rebuild_plan_at_generation(
        &self,
        dirty: DirtyPathSet,
        catalog_generation: u64,
    ) -> Result<WorkspaceOverlayWritePlan> {
        let source_head = self.current_git_head()?;
        let manifest_source = self.overlay_manifest_source(source_head.as_deref())?;
        if dirty.is_empty() {
            return Ok(WorkspaceOverlayWritePlan::clear()
                .with_source_identity(source_head, manifest_source)
                .with_catalog_generation(catalog_generation));
        }
        Ok(WorkspaceOverlayWritePlan::rebuild(dirty)
            .with_source_identity(source_head, manifest_source)
            .with_catalog_generation(catalog_generation))
    }

    fn build_overlay_delta_plan(
        &self,
        dirty: DirtyPathSet,
        delta: DirtyPathSet,
        catalog_generation: u64,
    ) -> Result<WorkspaceOverlayWritePlan> {
        let source_head = self.current_git_head()?;
        let manifest_source = self.overlay_manifest_source(source_head.as_deref())?;
        // A delta can only extend the exact base its existing overlay shadows.
        // Re-labeling older postings after a base transition would mix generations.
        let overlay = self
            .workspace_overlay_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(overlay) = overlay {
            let snapshot = overlay.open_snapshot()?;
            let published_base = match snapshot.manifest().source.as_ref() {
                Some(crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                    base_manifest_id,
                    ..
                }) => Some(base_manifest_id),
                _ => None,
            };
            let target_base = match &manifest_source {
                crate::index_engine::manifest::ManifestSource::WorkspaceOverlay {
                    base_manifest_id,
                    ..
                } => Some(base_manifest_id),
                _ => None,
            };
            if published_base != target_base {
                return self.build_overlay_rebuild_plan_at_generation(dirty, catalog_generation);
            }
        }
        let published = self.published_overlay_paths()?;
        let retracted = published
            .modified
            .iter()
            .chain(&published.new)
            .chain(&published.deleted)
            .filter(|path| dirty.kind_for_path(path).is_none())
            .cloned()
            .collect();
        Ok(WorkspaceOverlayWritePlan::apply_delta(dirty, delta)
            .with_retracted_paths(retracted)
            .with_source_identity(source_head, manifest_source)
            .with_catalog_generation(catalog_generation))
    }

    pub(super) fn clear_base_snapshot_catalog(&self) {
        let mut catalog = match self.base_snapshot_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *catalog = None;
    }

    pub(super) fn current_base_snapshot_key(&self) -> Result<Option<String>> {
        Ok(
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
                .map(|metadata| metadata.snapshot_key),
        )
    }

    pub(super) fn current_compatible_base_snapshot_metadata(
        &self,
    ) -> Result<Option<WorkspaceBaseSnapshotMetadata>> {
        let Some(metadata) =
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
        else {
            return Ok(None);
        };
        match self.base_snapshot_metadata_is_compatible(&metadata) {
            true => Ok(Some(metadata)),
            false => Ok(None),
        }
    }

    pub(super) fn base_snapshot_metadata_is_compatible(
        &self,
        metadata: &WorkspaceBaseSnapshotMetadata,
    ) -> bool {
        let Some(config_fingerprint) = metadata.config_fingerprint.as_deref() else {
            return false;
        };
        let Some(repo_root) = metadata.repo_root.as_deref() else {
            return false;
        };
        if config_fingerprint
            != crate::index_engine::workspace::base_snapshot_config_fingerprint_for(
                &self.build_config,
            )
        {
            return false;
        }
        if repo_root != canonical_repo_root(&self.build_config.repo_path) {
            return false;
        }
        true
    }

    pub(super) fn base_snapshot_catalog(&self) -> Result<Option<Arc<SnapshotPathCatalog>>> {
        let Some(snapshot_key) = self.current_base_snapshot_key()? else {
            self.clear_base_snapshot_catalog();
            return Ok(None);
        };

        {
            let catalog = match self.base_snapshot_catalog.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some(cached) = catalog.as_ref() {
                if cached.snapshot_key == snapshot_key {
                    return Ok(Some(Arc::clone(&cached.catalog)));
                }
            }
        }

        let Some(reader) = self.open_base_snapshot_reader()? else {
            self.clear_base_snapshot_catalog();
            return Ok(None);
        };
        let catalog = Arc::new(reader.path_catalog()?);
        let mut slot = match self.base_snapshot_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(CachedBaseSnapshotCatalog {
            snapshot_key,
            catalog: Arc::clone(&catalog),
        });
        Ok(Some(catalog))
    }

    fn diff_against_base_snapshot_worktree(&self) -> Result<DirtyPathSet> {
        self.diff_against_base_snapshot_filesystem(None)
    }

    pub(super) fn diff_against_base_snapshot_event_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<DirtyPathSet> {
        self.diff_against_base_snapshot_filesystem(Some(paths))
    }

    pub(super) fn workspace_inventory_paths(
        &self,
        scopes: Option<&[PathBuf]>,
    ) -> Result<HashSet<String>> {
        let root = &self.build_config.repo_path;
        let files = crate::files::scan_workspace_scopes(
            root,
            scopes,
            Some(&self.build_config.storage_root),
            crate::files::ScanOptions {
                respect_ignore: self.build_config.corpus_mode == CorpusMode::RespectIgnore,
                include_hidden: self.build_config.include_hidden,
                max_file_size: self.build_config.max_file_size,
                max_depth: None,
                ignore_files: Vec::new(),
            },
        )?;
        let in_scope = |path: &Path| {
            scopes.is_none_or(|scopes| scopes.iter().any(|scope| path.starts_with(scope)))
        };
        let mut visible: HashSet<String> = files
            .into_iter()
            .map(|f| f.path)
            .filter(|path| in_scope(path))
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let members = self.git_membership(scopes.is_none())?;
        let mut include_member = |logical: &str| -> Result<()> {
            let path = resolve_repo_path(root, logical);
            if visible.contains(path.to_string_lossy().as_ref()) {
                return Ok(());
            }
            if self.current_path_matches_corpus(&path)? {
                visible.insert(path.to_string_lossy().into_owned());
            }
            Ok(())
        };
        if let Some(scopes) = scopes {
            for scope in scopes {
                if let Some(logical) = self.logical_repo_path(scope) {
                    for member in super::membership::scoped_members(&members, &logical) {
                        include_member(member)?;
                    }
                }
            }
        } else {
            for member in members.iter() {
                include_member(member)?;
            }
        }
        Ok(visible)
    }

    fn diff_against_base_snapshot_filesystem(
        &self,
        scopes: Option<&[PathBuf]>,
    ) -> Result<DirtyPathSet> {
        let root = &self.build_config.repo_path;
        let visible = self.workspace_inventory_paths(scopes)?;
        let mut dirty = self.classify_paths_against_base_snapshot_with_verification(
            visible.iter().cloned(),
            scopes.is_some(),
        )?;
        if let Some(catalog) = self.base_snapshot_catalog()? {
            let mut record_missing = |logical: &str| {
                let path = resolve_repo_path(root, logical);
                if !visible.contains(path.to_string_lossy().as_ref()) {
                    dirty.deleted.insert(path.to_string_lossy().into_owned());
                }
            };
            if let Some(scopes) = scopes {
                for scope in scopes {
                    if let Some(logical_scope) = self.logical_repo_path(scope) {
                        for (logical, _) in catalog.iter_scope(&logical_scope) {
                            record_missing(logical);
                        }
                    }
                }
            } else {
                for (logical, _) in catalog.iter() {
                    record_missing(logical);
                }
            }
        }
        if scopes.is_none() {
            let mut observations = self
                .worktree_observations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            observations.retain(|logical| {
                visible.contains(resolve_repo_path(root, logical).to_string_lossy().as_ref())
            });
            if let Err(error) = observations.persist(&self.build_config.storage_root, root) {
                eprintln!(
                    "flashgrep: could not checkpoint disposable worktree observations: {error}"
                );
            }
        }
        Ok(dirty)
    }

    pub(super) fn classify_paths_against_base_snapshot<I>(&self, paths: I) -> Result<DirtyPathSet>
    where
        I: IntoIterator<Item = String>,
    {
        self.classify_paths_against_base_snapshot_with_verification(paths, false)
    }

    fn classify_paths_against_base_snapshot_with_verification<I>(
        &self,
        paths: I,
        verify_content: bool,
    ) -> Result<DirtyPathSet>
    where
        I: IntoIterator<Item = String>,
    {
        let Some(catalog) = self.base_snapshot_catalog()? else {
            return Ok(DirtyPathSet::default());
        };

        let root = &self.build_config.repo_path;
        let mut observations = self
            .worktree_observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observations.load(&self.build_config.storage_root, root);
        use crate::worktree_diff::DirtyPathKind;
        use rayon::prelude::*;
        // Separate bounded discovery workers prevent a full-tree reconcile from
        // occupying the query executor. Each worker reads at most one capped file.
        static DISCOVERY_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
        let pool = DISCOVERY_POOL.get_or_init(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(
                    std::thread::available_parallelism()
                        .map_or(1, |n| n.get())
                        .min(4),
                )
                .thread_name(|i| format!("workspace-discovery-{i}"))
                .build()
                .expect("workspace discovery worker pool")
        });
        let paths = paths
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let classify = |path: String| -> Result<_> {
            let Some(logical) = self.logical_repo_path(Path::new(&path)) else {
                return Ok((None, None));
            };
            let base = catalog.get(&logical);
            let eligible = crate::files::workspace_path_is_eligible(
                root,
                Path::new(&path),
                Some(&self.build_config.storage_root),
                self.build_config.include_hidden,
            ) && !self.is_storage_path(Path::new(&path));
            let metadata = if eligible {
                crate::files::workspace_file_metadata(root, Path::new(&path))?
                    .filter(|m| m.len() <= self.build_config.max_file_size)
            } else {
                None
            };
            let (hash, observation) = match metadata.as_ref() {
                Some(metadata) => observations.inspect(
                    &logical,
                    Path::new(&path),
                    metadata,
                    verify_content,
                    self.build_config.max_file_size,
                )?,
                None => (None, None),
            };
            let kind = match (base, hash) {
                (Some(entry), Some(hash))
                    if metadata.as_ref().unwrap().len() != entry.size
                        || hash != entry.content_hash =>
                {
                    Some(DirtyPathKind::Modified)
                }
                (Some(_), None) => Some(DirtyPathKind::Deleted),
                (None, Some(_)) => Some(DirtyPathKind::New),
                _ => None,
            };
            Ok((
                kind.map(|kind| (path, kind)),
                observation.map(|observation| (logical, observation)),
            ))
        };
        let outcomes = pool.install(|| {
            paths
                .into_par_iter()
                .map(classify)
                .collect::<Result<Vec<_>>>()
        })?;
        let mut dirty = DirtyPathSet::default();
        for (change, observation) in outcomes {
            if let Some((logical, observation)) = observation {
                observations.record(logical, observation);
            }
            if let Some((path, kind)) = change {
                match kind {
                    DirtyPathKind::Modified => {
                        dirty.modified.insert(path);
                    }
                    DirtyPathKind::Deleted => {
                        dirty.deleted.insert(path);
                    }
                    DirtyPathKind::New => {
                        dirty.new.insert(path);
                    }
                }
            }
        }

        Ok(dirty)
    }

    pub(super) fn load_known_corpus_search_document(
        &self,
        path: &str,
    ) -> Result<Option<SearchDocument>> {
        let path = PathBuf::from(path);
        let Some(metadata) =
            crate::files::workspace_file_metadata(&self.build_config.repo_path, &path)?
        else {
            return Ok(None);
        };
        if !metadata.is_file() || metadata.len() > self.build_config.max_file_size {
            return Ok(None);
        }

        use std::io::Read;
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(self.build_config.max_file_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > self.build_config.max_file_size {
            return Ok(None);
        }
        let probe_len = bytes.len().min(8 * 1024);
        if bytes[..probe_len].contains(&0) {
            return Ok(None);
        }

        Ok(Some(SearchDocument::from_loaded_bytes(
            path.to_string_lossy().into_owned(),
            bytes.len() as u64,
            file_mtime_nanos(&metadata),
            Arc::<[u8]>::from(bytes),
        )))
    }

    fn current_path_matches_corpus(&self, path: &Path) -> Result<bool> {
        if !crate::files::workspace_path_is_eligible(
            &self.build_config.repo_path,
            path,
            Some(&self.build_config.storage_root),
            self.build_config.include_hidden,
        ) || self.is_storage_path(path)
        {
            return Ok(false);
        }

        let Some(metadata) =
            crate::files::workspace_file_metadata(&self.build_config.repo_path, path)?
        else {
            return Ok(false);
        };
        if !metadata.is_file() || metadata.len() > self.build_config.max_file_size {
            return Ok(false);
        }

        is_corpus_text_file(path)
    }

    fn logical_repo_path(&self, path: &Path) -> Option<String> {
        path.starts_with(&self.build_config.repo_path)
            .then(|| repo_relative_path(path, &self.build_config.repo_path))
    }
}

fn file_mtime_nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn decide_overlay_update_mode(
    dirty: &DirtyPathSet,
    published_overlay_paths: &DirtyPathSet,
    pending_dirty: &DirtyPathSet,
    published_overlay_snapshot_available: bool,
) -> OverlayUpdateMode {
    if dirty.is_empty() {
        return if published_overlay_paths.is_empty() {
            OverlayUpdateMode::NoChange
        } else {
            OverlayUpdateMode::ApplyDelta
        };
    }

    if !published_overlay_snapshot_available {
        return OverlayUpdateMode::Rebuild;
    }

    if dirty == published_overlay_paths {
        if pending_dirty.is_empty() {
            return if published_overlay_snapshot_available {
                OverlayUpdateMode::NoChange
            } else {
                OverlayUpdateMode::Rebuild
            };
        }
        return OverlayUpdateMode::ApplyDelta;
    }

    let missing_ready_paths = [
        (
            &dirty.modified,
            &published_overlay_paths.modified,
            &pending_dirty.modified,
        ),
        (
            &dirty.deleted,
            &published_overlay_paths.deleted,
            &pending_dirty.deleted,
        ),
        (&dirty.new, &published_overlay_paths.new, &pending_dirty.new),
    ]
    .into_iter()
    .any(|(paths, published, pending)| {
        paths
            .difference(published)
            .any(|path| !pending.contains(path))
    });
    if missing_ready_paths {
        // A catalog entry marked ready is not proof that its data survived an
        // overlay reset. Reconstruct missing entries as well as new deltas.
        return OverlayUpdateMode::Rebuild;
    }

    OverlayUpdateMode::ApplyDelta
}

#[cfg(test)]
mod tests {
    use super::{decide_overlay_update_mode, DirtyPathSet, OverlayUpdateMode};

    fn dirty_paths(modified: &[&str], deleted: &[&str], new: &[&str]) -> DirtyPathSet {
        DirtyPathSet {
            modified: modified.iter().map(|path| (*path).to_string()).collect(),
            deleted: deleted.iter().map(|path| (*path).to_string()).collect(),
            new: new.iter().map(|path| (*path).to_string()).collect(),
        }
    }

    #[test]
    fn clean_back_to_base_uses_incremental_retraction() {
        let dirty = dirty_paths(&[], &[], &[]);
        let published_overlay_paths = dirty_paths(&["/repo/tracked.rs"], &[], &[]);
        let pending_dirty = dirty_paths(&[], &[], &[]);

        let mode =
            decide_overlay_update_mode(&dirty, &published_overlay_paths, &pending_dirty, true);

        assert_eq!(mode, OverlayUpdateMode::ApplyDelta);
    }

    #[test]
    fn incremental_probe_keeps_published_snapshot_when_nothing_changed() {
        let dirty = dirty_paths(&["/repo/tracked.rs"], &[], &[]);
        let pending_dirty = dirty_paths(&[], &[], &[]);

        let mode = decide_overlay_update_mode(&dirty, &dirty, &pending_dirty, true);

        assert_eq!(mode, OverlayUpdateMode::NoChange);
    }

    #[test]
    fn clean_repo_without_overlay_snapshot_stays_no_change() {
        let dirty = dirty_paths(&[], &[], &[]);
        let published_overlay_paths = dirty_paths(&[], &[], &[]);
        let pending_dirty = dirty_paths(&[], &[], &[]);

        let mode =
            decide_overlay_update_mode(&dirty, &published_overlay_paths, &pending_dirty, false);

        assert_eq!(mode, OverlayUpdateMode::NoChange);
    }
}
