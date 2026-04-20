use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use crate::{
    config::CorpusMode,
    error::Result,
    files::{
        is_indexable_text_file, is_text_file, is_workspace_internal_path, repo_relative_path,
        resolve_repo_path, scan_paths, ScanOptions,
    },
    git_snapshot::resolve_git_head,
    index_engine::{reader::SnapshotPathCatalog, WorkspaceBaseSnapshotIndex},
    path_filter::{PathFilter, PathFilterArgs},
    search_engine::SearchDocument,
};

use super::{
    overlay::WorkspaceOverlayWritePlan, CachedBaseSnapshotCatalog, DirtyPathSet, RepoRuntime,
    WorkspaceOverlayIngestBatch, WorkspaceOverlaySnapshotState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayUpdateMode {
    NoChange,
    Rebuild,
    ApplyDelta,
}

impl RepoRuntime {
    pub(super) fn preload_base_snapshot_catalog(&self) -> Result<()> {
        let _ = self.base_snapshot_catalog()?;
        Ok(())
    }

    pub(super) fn plan_overlay_full_rebuild_from_worktree(
        &self,
    ) -> Result<WorkspaceOverlayWritePlan> {
        let dirty = self.diff_against_base_snapshot_worktree()?;
        self.reconcile_dirty_catalog(&dirty)?;
        self.build_overlay_rebuild_plan(dirty)
    }

    pub(super) fn plan_overlay_update_from_incremental_probe(
        &self,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let Some(dirty) = self.diff_against_base_snapshot_incremental()? else {
            return Ok(None);
        };
        self.reconcile_dirty_catalog(&dirty)?;
        let published_overlay_paths = self.published_overlay_paths()?;
        let pending_dirty = self.dirty_catalog_pending_paths();
        let published_overlay_snapshot_available =
            self.snapshot_published_overlay_paths()?.is_some() || dirty.is_empty();

        // Incremental probe rules:
        // - If the current dirty set already matches the published overlay and nothing is pending,
        //   keep the current published snapshot if it is readable.
        // - If any previously published overlay path became clean back to base, force a rebuild.
        //   Delta publish can add/update/delete overlay entries, but it cannot retract already
        //   published overlay-visible paths that should disappear entirely.
        // - Otherwise publish only the pending delta paths.
        match decide_overlay_update_mode(
            &dirty,
            &published_overlay_paths,
            &pending_dirty,
            published_overlay_snapshot_available,
        ) {
            OverlayUpdateMode::NoChange => Ok(None),
            OverlayUpdateMode::Rebuild => self.build_overlay_rebuild_plan(dirty).map(Some),
            OverlayUpdateMode::ApplyDelta => self
                .build_overlay_delta_plan(dirty, pending_dirty)
                .map(Some),
        }
    }

    pub(super) fn build_overlay_rebuild_plan(
        &self,
        dirty: DirtyPathSet,
    ) -> Result<WorkspaceOverlayWritePlan> {
        if dirty.is_empty() {
            return Ok(WorkspaceOverlayWritePlan::clear());
        }
        let documents = self.load_overlay_documents(&dirty)?;
        let mut deleted_paths = dirty.deleted.clone();
        for path in &dirty.modified {
            if !documents.contains_key(path) {
                deleted_paths.insert(path.clone());
            }
        }
        let snapshot = WorkspaceOverlaySnapshotState::from_paths(
            &self.overlay_repo_id,
            &self.build_config.repo_path,
            &documents,
            &deleted_paths,
        )?;
        Ok(WorkspaceOverlayWritePlan::rebuild(dirty, snapshot))
    }

    fn build_overlay_delta_plan(
        &self,
        dirty: DirtyPathSet,
        delta: DirtyPathSet,
    ) -> Result<WorkspaceOverlayWritePlan> {
        if dirty.is_empty() {
            return Ok(WorkspaceOverlayWritePlan::clear());
        }
        let documents = self.load_delta_overlay_documents(&delta)?;
        let batch = WorkspaceOverlayIngestBatch::from_paths(
            &self.overlay_repo_id,
            &self.build_config.repo_path,
            &delta.modified,
            &delta.new,
            &delta.deleted,
            &documents,
        )?;
        Ok(WorkspaceOverlayWritePlan::apply_delta(dirty, delta, batch))
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

    pub(super) fn base_snapshot_catalog(&self) -> Result<Option<SnapshotPathCatalog>> {
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
                    return Ok(Some(cached.catalog.clone()));
                }
            }
        }

        let Some(reader) = self.open_base_snapshot_reader()? else {
            self.clear_base_snapshot_catalog();
            return Ok(None);
        };
        let catalog = reader.path_catalog()?;
        let mut slot = match self.base_snapshot_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *slot = Some(CachedBaseSnapshotCatalog {
            snapshot_key,
            catalog: catalog.clone(),
        });
        Ok(Some(catalog))
    }

    fn diff_against_base_snapshot_worktree(&self) -> Result<DirtyPathSet> {
        if let Some(dirty) = self.fast_empty_dirty_diff_from_git()? {
            return Ok(dirty);
        }

        let Some(catalog) = self.base_snapshot_catalog()? else {
            return Ok(DirtyPathSet::default());
        };

        if let Some(git_dirty) = self.diff_against_base_snapshot_git_worktree(&catalog)? {
            return Ok(git_dirty);
        }

        let scanned = scan_paths(
            std::slice::from_ref(&self.build_config.repo_path),
            Some(&self.build_config.storage_root),
            ScanOptions {
                respect_ignore: matches!(self.build_config.corpus_mode, CorpusMode::RespectIgnore),
                include_hidden: self.build_config.include_hidden,
                max_file_size: self.build_config.max_file_size,
                max_depth: None,
                ignore_files: Vec::new(),
            },
            None,
        )?;

        let mut dirty = DirtyPathSet::default();
        let mut seen = HashSet::new();

        for file in scanned {
            let logical_path = repo_relative_path(&file.path, &self.build_config.repo_path);
            match catalog.get(&logical_path) {
                Some(_) => {
                    let entry = catalog
                        .get(&logical_path)
                        .expect("catalog entry should exist for tracked path");
                    if entry.size == file.size {
                        if entry.modified_at == file.mtime_nanos
                            || current_file_content_hash(&file.path)? == entry.content_hash
                        {
                            seen.insert(logical_path);
                            continue;
                        }
                    }
                    if !is_text_file(&file.path)? {
                        continue;
                    }
                    seen.insert(logical_path);
                    let absolute_path = file.path.to_string_lossy().into_owned();
                    dirty.modified.insert(absolute_path);
                }
                None => {
                    if !is_text_file(&file.path)? {
                        continue;
                    }
                    seen.insert(logical_path);
                    let absolute_path = file.path.to_string_lossy().into_owned();
                    dirty.new.insert(absolute_path);
                }
            }
        }

        for (logical_path, _) in catalog.iter() {
            if seen.contains(logical_path) {
                continue;
            }
            dirty.deleted.insert(
                resolve_repo_path(&self.build_config.repo_path, logical_path)
                    .to_string_lossy()
                    .into_owned(),
            );
        }

        Ok(dirty)
    }

    fn diff_against_base_snapshot_incremental(&self) -> Result<Option<DirtyPathSet>> {
        if let Some(dirty) = self.fast_empty_dirty_diff_from_git()? {
            return Ok(Some(dirty));
        }

        let Some(catalog) = self.base_snapshot_catalog()? else {
            return Ok(None);
        };

        self.diff_against_base_snapshot_git_worktree(&catalog)
    }

    fn fast_empty_dirty_diff_from_git(&self) -> Result<Option<DirtyPathSet>> {
        if !matches!(self.build_config.corpus_mode, CorpusMode::RespectIgnore) {
            return Ok(None);
        }
        let Some(base_metadata) =
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
        else {
            return Ok(None);
        };
        let Some(base_head_commit) = base_metadata
            .head_commit
            .clone()
            .or_else(|| base_metadata.revision.map(|revision| revision.0))
        else {
            return Ok(None);
        };

        let mut should_cancel = || false;
        let current_head_commit =
            resolve_git_head(&self.build_config.repo_path, &mut should_cancel)?;
        if current_head_commit.as_deref() != Some(base_head_commit.as_str()) {
            return Ok(None);
        }

        let tracked_paths = match self.git_visible_paths([
            "diff",
            "--name-only",
            "-z",
            base_head_commit.as_str(),
            "--",
            ".",
        ]) {
            Ok(paths) => paths,
            Err(_) => return Ok(None),
        };
        if !tracked_paths.is_empty() {
            return Ok(None);
        }
        let untracked_paths = match self.git_visible_paths([
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ]) {
            Ok(paths) => paths,
            Err(_) => return Ok(None),
        };
        if !untracked_paths.is_empty()
            && !self
                .classify_paths_against_base_snapshot(untracked_paths)?
                .new
                .is_empty()
        {
            return Ok(None);
        }

        Ok(Some(DirtyPathSet::default()))
    }

    fn diff_against_base_snapshot_git_worktree(
        &self,
        catalog: &SnapshotPathCatalog,
    ) -> Result<Option<DirtyPathSet>> {
        if !matches!(self.build_config.corpus_mode, CorpusMode::RespectIgnore) {
            return Ok(None);
        }
        let Some(base_metadata) =
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
        else {
            return Ok(None);
        };
        let Some(head_commit) = base_metadata
            .head_commit
            .clone()
            .or_else(|| base_metadata.revision.map(|revision| revision.0))
        else {
            return Ok(None);
        };

        let tracked_paths = match self.git_visible_paths([
            "diff",
            "--name-only",
            "-z",
            head_commit.as_str(),
            "--",
            ".",
        ]) {
            Ok(paths) => paths,
            Err(_) => return Ok(None),
        };
        let untracked_visible_paths = match self.git_visible_paths([
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ]) {
            Ok(paths) => paths,
            Err(_) => return Ok(None),
        };

        let mut dirty = self.classify_paths_against_base_snapshot(tracked_paths)?;
        let untracked_candidates = untracked_visible_paths
            .into_iter()
            .filter(|path| self.path_is_missing_from_base_catalog(path, catalog))
            .collect::<Vec<_>>();
        dirty.merge(self.classify_paths_against_base_snapshot(untracked_candidates)?);

        Ok(Some(dirty))
    }

    fn classify_paths_against_base_snapshot<I>(&self, paths: I) -> Result<DirtyPathSet>
    where
        I: IntoIterator<Item = String>,
    {
        let Some(catalog) = self.base_snapshot_catalog()? else {
            return Ok(DirtyPathSet::default());
        };

        let mut dirty = DirtyPathSet::default();
        let mut seen = HashSet::new();
        for path in paths {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(logical_path) = self.logical_repo_path(Path::new(&path)) else {
                continue;
            };
            match catalog.get(&logical_path) {
                Some(entry) => match fs::metadata(&path) {
                    Ok(metadata) => {
                        if !self.current_path_matches_corpus(Path::new(&path))? {
                            dirty.deleted.insert(path);
                            continue;
                        }
                        let modified_at = file_mtime_nanos(&metadata);
                        if metadata.len() != entry.size {
                            dirty.modified.insert(path);
                            continue;
                        }
                        if modified_at == entry.modified_at {
                            continue;
                        }
                        let content_hash = current_file_content_hash(Path::new(&path))?;
                        if content_hash != entry.content_hash {
                            dirty.modified.insert(path);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        dirty.deleted.insert(path);
                    }
                    Err(error) => return Err(error.into()),
                },
                None => {
                    if self.current_path_matches_corpus(Path::new(&path))? {
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
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() > self.build_config.max_file_size {
            return Ok(None);
        }

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let probe_len = bytes.len().min(8 * 1024);
        if bytes[..probe_len].contains(&0) || std::str::from_utf8(&bytes).is_err() {
            return Ok(None);
        }

        Ok(Some(SearchDocument::from_loaded_bytes(
            path.to_string_lossy().into_owned(),
            metadata.len(),
            file_mtime_nanos(&metadata),
            Arc::<[u8]>::from(bytes),
        )))
    }

    fn current_path_matches_corpus(&self, path: &Path) -> Result<bool> {
        if !path.starts_with(&self.build_config.repo_path)
            || is_workspace_internal_path(path)
            || self.is_storage_path(path)
        {
            return Ok(false);
        }

        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() || metadata.len() > self.build_config.max_file_size {
            return Ok(false);
        }
        if !self.build_config.include_hidden && self.path_is_hidden_for_build(path) {
            return Ok(false);
        }

        if matches!(self.build_config.corpus_mode, CorpusMode::RespectIgnore) {
            let Some(parent) = path.parent() else {
                return Ok(false);
            };
            let filter = PathFilter::new(
                PathFilterArgs {
                    roots: vec![path.to_path_buf()],
                    ..PathFilterArgs::default()
                },
                &self.build_config.repo_path,
            )?;
            let scanned = scan_paths(
                &[parent.to_path_buf()],
                Some(&self.build_config.storage_root),
                ScanOptions {
                    respect_ignore: true,
                    include_hidden: self.build_config.include_hidden,
                    max_file_size: self.build_config.max_file_size,
                    max_depth: Some(1),
                    ignore_files: Vec::new(),
                },
                Some(&filter),
            )?;
            if scanned.is_empty() {
                return Ok(false);
            }
        }

        is_indexable_text_file(path)
    }

    fn logical_repo_path(&self, path: &Path) -> Option<String> {
        path.starts_with(&self.build_config.repo_path)
            .then(|| repo_relative_path(path, &self.build_config.repo_path))
    }

    fn path_is_missing_from_base_catalog(&self, path: &str, catalog: &SnapshotPathCatalog) -> bool {
        self.logical_repo_path(Path::new(path))
            .is_none_or(|logical_path| catalog.get(&logical_path).is_none())
    }

    fn path_is_hidden_for_build(&self, path: &Path) -> bool {
        let relative = path
            .strip_prefix(&self.build_config.repo_path)
            .unwrap_or(path);
        relative.components().any(|component| match component {
            std::path::Component::Normal(value) => value.to_string_lossy().starts_with('.'),
            _ => false,
        })
    }

    fn git_visible_paths<const N: usize>(&self, args: [&str; N]) -> Result<Vec<String>> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.build_config.repo_path)
            .args(args);
        if let Some(pathspec) = self.git_exclude_pathspec() {
            command.arg(pathspec);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(crate::error::AppError::InvalidIndex(format!(
                "failed to inspect git worktree state for {}",
                self.build_config.repo_path.display()
            )));
        }

        Ok(output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| {
                resolve_repo_path(&self.build_config.repo_path, &String::from_utf8_lossy(path))
            })
            .filter(|path| !is_workspace_internal_path(path) && !self.is_storage_path(path))
            .filter(|path| self.build_config.include_hidden || !self.path_is_hidden_for_build(path))
            .map(|path| path.to_string_lossy().into_owned())
            .collect())
    }

    fn git_exclude_pathspec(&self) -> Option<String> {
        let relative = self
            .build_config
            .storage_root
            .strip_prefix(&self.build_config.repo_path)
            .ok()?;
        if relative.as_os_str().is_empty() {
            return None;
        }
        let path = relative.to_string_lossy().replace('\\', "/");
        Some(format!(":(exclude){path}"))
    }
}

fn file_mtime_nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| match u64::try_from(duration.as_nanos()) {
            Ok(value) => value,
            Err(_) => u64::MAX,
        })
        .unwrap_or_default()
}

fn current_file_content_hash(path: &Path) -> Result<[u8; 32]> {
    let bytes = fs::read(path)?;
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    Ok(content_hash)
}

fn decide_overlay_update_mode(
    dirty: &DirtyPathSet,
    published_overlay_paths: &DirtyPathSet,
    pending_dirty: &DirtyPathSet,
    published_overlay_snapshot_available: bool,
) -> OverlayUpdateMode {
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

    if dirty.has_paths_cleaned_since(published_overlay_paths) {
        return OverlayUpdateMode::Rebuild;
    }

    if pending_dirty.is_empty() {
        return if published_overlay_snapshot_available {
            OverlayUpdateMode::NoChange
        } else {
            OverlayUpdateMode::Rebuild
        };
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
    fn clean_back_to_base_requires_overlay_rebuild() {
        let dirty = dirty_paths(&[], &[], &[]);
        let published_overlay_paths = dirty_paths(&["/repo/tracked.rs"], &[], &[]);
        let pending_dirty = dirty_paths(&[], &[], &[]);

        let mode =
            decide_overlay_update_mode(&dirty, &published_overlay_paths, &pending_dirty, true);

        assert_eq!(mode, OverlayUpdateMode::Rebuild);
    }

    #[test]
    fn incremental_probe_keeps_published_snapshot_when_nothing_changed() {
        let dirty = dirty_paths(&["/repo/tracked.rs"], &[], &[]);
        let pending_dirty = dirty_paths(&[], &[], &[]);

        let mode = decide_overlay_update_mode(&dirty, &dirty, &pending_dirty, true);

        assert_eq!(mode, OverlayUpdateMode::NoChange);
    }
}
