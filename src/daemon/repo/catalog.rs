use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Instant,
};

use crate::{
    config::CorpusMode,
    error::{AppError, Result},
    files::{
        is_corpus_text_file, is_workspace_internal_path, repo_relative_path, resolve_repo_path,
    },
    git_snapshot::canonical_repo_root,
    index_engine::{
        reader::SnapshotPathCatalog, workspace::WorkspaceBaseSnapshotMetadata,
        WorkspaceBaseSnapshotIndex,
    },
    search_engine::SearchDocument,
    worktree_diff::DirtyPathKind,
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

const GIT_PATHSPEC_CHUNK_MAX_BYTES: usize = 512 * 1024;
const GIT_PATHSPEC_CHUNK_MIN_BYTES: usize = 8 * 1024;
const GIT_ARGV_HEADROOM_BYTES: usize = 32 * 1024;

impl RepoRuntime {
    pub(super) fn preload_base_snapshot_catalog(&self) -> Result<()> {
        let _ = self.base_snapshot_catalog()?;
        Ok(())
    }

    pub(super) fn plan_overlay_full_rebuild_from_worktree(
        &self,
    ) -> Result<WorkspaceOverlayWritePlan> {
        let dirty = self.diff_against_base_snapshot_worktree()?;
        let catalog_generation = self.reconcile_dirty_catalog(&dirty)?;
        self.build_overlay_rebuild_plan_at_generation(dirty, catalog_generation)
    }

    pub(super) fn plan_overlay_update_from_incremental_probe(
        &self,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let Some(dirty) = self.diff_against_base_snapshot_incremental()? else {
            return Ok(None);
        };
        self.plan_overlay_update_from_dirty_probe(dirty)
    }

    pub(super) fn plan_overlay_update_from_event_probe(
        &self,
        candidate_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Option<WorkspaceOverlayWritePlan>> {
        let candidate_paths = candidate_paths
            .into_iter()
            .filter(|path| !self.is_storage_path(path) && !is_workspace_internal_path(path))
            .collect::<Vec<_>>();
        if candidate_paths.is_empty() {
            return Ok(None);
        }
        let delta = self.diff_against_base_snapshot_git_worktree_paths(&candidate_paths)?;
        let mut dirty = self.dirty_catalog_visible_paths();
        for path in candidate_paths {
            dirty.remove_path_scope(&path);
        }
        dirty.merge(delta);
        self.plan_overlay_update_from_dirty_probe(dirty)
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
        // - If any previously published overlay path became clean back to base, force a rebuild.
        //   Delta publish can add/update/delete overlay entries, but it cannot retract already
        //   published overlay-visible paths that should disappear entirely.
        // - Otherwise publish only the pending delta paths.
        let mode = decide_overlay_update_mode(
            &dirty,
            &published_overlay_paths,
            &pending_dirty,
            published_overlay_snapshot_available,
        );
        if mode == OverlayUpdateMode::NoChange && catalog_changed {
            self.persist_dirty_catalog()?;
        }
        match mode {
            OverlayUpdateMode::NoChange => Ok(None),
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
        let manifest_source = self.overlay_manifest_source(&source_head)?;
        if dirty.is_empty() {
            return Ok(WorkspaceOverlayWritePlan::clear()
                .with_source_identity(source_head, manifest_source)
                .with_catalog_generation(catalog_generation));
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
        Ok(WorkspaceOverlayWritePlan::rebuild(dirty, snapshot)
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
        let manifest_source = self.overlay_manifest_source(&source_head)?;
        if dirty.is_empty() {
            return Ok(WorkspaceOverlayWritePlan::clear()
                .with_source_identity(source_head, manifest_source)
                .with_catalog_generation(catalog_generation));
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
        Ok(WorkspaceOverlayWritePlan::apply_delta(dirty, delta, batch)
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
        if metadata.head_commit.is_none() && metadata.revision.is_none() {
            return false;
        }
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
        self.diff_against_base_snapshot_git_worktree()
    }

    fn diff_against_base_snapshot_incremental(&self) -> Result<Option<DirtyPathSet>> {
        self.diff_against_base_snapshot_git_worktree().map(Some)
    }

    fn diff_against_base_snapshot_git_worktree(&self) -> Result<DirtyPathSet> {
        self.diff_against_base_snapshot_git_worktree_with_scope(None)
    }

    fn git_status_dirty_paths_at_head(&self, expected_head: &str) -> Result<Option<DirtyPathSet>> {
        // `git status` can report tracked and untracked paths in one worktree walk. It compares
        // against the current HEAD, so only use it when its branch oid matches the base snapshot;
        // otherwise the existing diff + ls-files path is required to include committed changes.
        if self.build_config.corpus_mode != CorpusMode::RespectIgnore {
            return Ok(None);
        }
        let output = self.run_git_output(
            [
                "status",
                "--porcelain=v2",
                "--branch",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                "--ignore-submodules=all",
            ],
            &[],
        )?;
        let Some((head, records)) = parse_git_status_v2_z(&output) else {
            return Ok(None);
        };
        if head != expected_head {
            return Ok(None);
        }

        let mut dirty = DirtyPathSet::default();
        for (status, logical_path) in records {
            let path = resolve_repo_path(&self.build_config.repo_path, &logical_path);
            if is_workspace_internal_path(&path)
                || self.is_storage_path(&path)
                || (!self.build_config.include_hidden && self.path_is_hidden_for_build(&path))
            {
                continue;
            }
            let kind = if status == "?" {
                DirtyPathKind::New
            } else if status.chars().any(|status| status == 'D') {
                DirtyPathKind::Deleted
            } else if status.chars().any(|status| status == 'A') {
                DirtyPathKind::New
            } else {
                DirtyPathKind::Modified
            };
            dirty.upsert(path.to_string_lossy().into_owned(), kind);
        }
        Ok(Some(dirty))
    }

    pub(super) fn diff_against_base_snapshot_git_worktree_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<DirtyPathSet> {
        self.diff_against_base_snapshot_git_worktree_with_scope(Some(paths))
    }

    fn diff_against_base_snapshot_git_worktree_with_scope(
        &self,
        paths: Option<&[PathBuf]>,
    ) -> Result<DirtyPathSet> {
        let Some(base_metadata) =
            WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
        else {
            return Ok(DirtyPathSet::default());
        };
        let Some(head_commit) = base_metadata
            .head_commit
            .clone()
            .or_else(|| base_metadata.revision.map(|revision| revision.0))
        else {
            return Err(AppError::InvalidIndex(format!(
                "daemon dirty probe requires a Git HEAD base snapshot for {}",
                self.build_config.repo_path.display()
            )));
        };

        // A whole-worktree probe is the expensive case: `git diff` and
        // `git ls-files --others` each walk the tree, which is ~5.7 s on
        // Chromium. When HEAD still matches the base snapshot, one
        // `git status` walk reports tracked and untracked paths together, so
        // prefer it and keep the two-command path for the cases it cannot
        // answer (HEAD moved, `NoIgnore`, unparsable output).
        let probe_started_at = Instant::now();
        let (tracked_changes, untracked_visible_paths, probe_mode) = match paths {
            None => match self.git_status_dirty_paths_at_head(&head_commit)? {
                // `git status` already merges tracked and untracked paths, and
                // the shared classification below decides which of them really
                // differ from the base snapshot.
                Some(dirty) => (
                    dirty
                        .modified
                        .iter()
                        .map(|path| (path.clone(), DirtyPathKind::Modified))
                        .chain(
                            dirty
                                .deleted
                                .iter()
                                .map(|path| (path.clone(), DirtyPathKind::Deleted)),
                        )
                        .chain(
                            dirty
                                .new
                                .iter()
                                .map(|path| (path.clone(), DirtyPathKind::New)),
                        )
                        .collect::<Vec<_>>(),
                    Vec::new(),
                    "status",
                ),
                None => (
                    self.git_tracked_dirty_paths(&head_commit, None)?,
                    self.git_untracked_visible_paths(None)?,
                    "legacy",
                ),
            },
            Some(paths) => (
                self.git_tracked_dirty_paths(&head_commit, Some(paths))?,
                self.git_untracked_visible_paths(Some(paths))?,
                "scoped",
            ),
        };

        let mut dirty = DirtyPathSet::default();
        let mut tracked_current_paths = Vec::new();
        for (path, kind) in tracked_changes {
            match kind {
                DirtyPathKind::Deleted => {
                    dirty.upsert(path, DirtyPathKind::Deleted);
                }
                DirtyPathKind::Modified | DirtyPathKind::New => {
                    tracked_current_paths.push(path);
                }
            }
        }
        dirty.merge(self.classify_paths_against_base_snapshot(tracked_current_paths)?);
        dirty.merge(self.classify_paths_against_base_snapshot(untracked_visible_paths)?);

        if paths.is_none() {
            eprintln!(
                "flashgrep[overlay-metrics][info] stage=worktree_probe mode={} elapsed_ms={} dirty_paths={}",
                probe_mode,
                probe_started_at.elapsed().as_millis(),
                dirty.stats().modified + dirty.stats().deleted + dirty.stats().new,
            );
        }

        Ok(dirty)
    }

    pub(super) fn classify_paths_against_base_snapshot<I>(&self, paths: I) -> Result<DirtyPathSet>
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
        if bytes[..probe_len].contains(&0) {
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

        is_corpus_text_file(path)
    }

    fn logical_repo_path(&self, path: &Path) -> Option<String> {
        path.starts_with(&self.build_config.repo_path)
            .then(|| repo_relative_path(path, &self.build_config.repo_path))
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

    fn git_output<const N: usize>(&self, args: [&str; N]) -> Result<Vec<u8>> {
        self.git_output_with_pathspec(args, None)
    }

    fn git_output_with_pathspec<const N: usize>(
        &self,
        args: [&str; N],
        paths: Option<&[PathBuf]>,
    ) -> Result<Vec<u8>> {
        let exclude = self.git_exclude_pathspec().map(OsString::from);
        let Some(paths) = paths else {
            return self.run_git_output(args, exclude.as_slice());
        };
        let pathspecs = self.git_candidate_pathspecs(paths);
        if pathspecs.is_empty() {
            return Ok(Vec::new());
        }

        let mut combined = Vec::new();
        for chunk in git_pathspec_chunks(&pathspecs, git_pathspec_chunk_budget()) {
            let mut command_pathspecs =
                Vec::with_capacity(chunk.len() + usize::from(exclude.is_some()));
            command_pathspecs.extend(exclude.iter().cloned());
            command_pathspecs.extend(chunk.iter().cloned());
            combined.extend(self.run_git_output(args, &command_pathspecs)?);
        }
        Ok(combined)
    }

    fn run_git_output<const N: usize>(
        &self,
        args: [&str; N],
        pathspecs: &[OsString],
    ) -> Result<Vec<u8>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.build_config.repo_path)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args.iter().copied())
            .args(pathspecs)
            .output()?;
        if !output.status.success() {
            return Err(crate::error::AppError::InvalidIndex(format!(
                "failed to inspect git worktree state for {}: {}",
                self.build_config.repo_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    fn git_tracked_dirty_paths(
        &self,
        head_commit: &str,
        paths: Option<&[PathBuf]>,
    ) -> Result<Vec<(String, DirtyPathKind)>> {
        let output = match paths {
            Some(paths) => self.git_output_with_pathspec(
                [
                    "diff",
                    "--name-status",
                    "-z",
                    "--no-renames",
                    head_commit,
                    "--",
                ],
                Some(paths),
            )?,
            None => self.git_output([
                "diff",
                "--name-status",
                "-z",
                "--no-renames",
                head_commit,
                "--",
                ".",
            ])?,
        };
        Ok(parse_git_name_status_z(&output)
            .into_iter()
            .filter_map(|(status, logical_path)| {
                let path = resolve_repo_path(&self.build_config.repo_path, &logical_path);
                if is_workspace_internal_path(&path) || self.is_storage_path(&path) {
                    return None;
                }
                if !self.build_config.include_hidden && self.path_is_hidden_for_build(&path) {
                    return None;
                }
                let kind = if status.starts_with('D') {
                    DirtyPathKind::Deleted
                } else if status.starts_with('A') {
                    DirtyPathKind::New
                } else {
                    DirtyPathKind::Modified
                };
                Some((path.to_string_lossy().into_owned(), kind))
            })
            .collect())
    }

    fn git_untracked_visible_paths(&self, paths: Option<&[PathBuf]>) -> Result<Vec<String>> {
        let output = match (self.build_config.corpus_mode, paths) {
            (CorpusMode::RespectIgnore, Some(paths)) => self.git_output_with_pathspec(
                ["ls-files", "-z", "--others", "--exclude-standard", "--"],
                Some(paths),
            )?,
            (CorpusMode::RespectIgnore, None) => self.git_output([
                "ls-files",
                "-z",
                "--others",
                "--exclude-standard",
                "--",
                ".",
            ])?,
            (CorpusMode::NoIgnore, Some(paths)) => {
                self.git_output_with_pathspec(["ls-files", "-z", "--others", "--"], Some(paths))?
            }
            (CorpusMode::NoIgnore, None) => {
                self.git_output(["ls-files", "-z", "--others", "--", "."])?
            }
        };
        Ok(output
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

    fn git_candidate_pathspecs(&self, paths: &[PathBuf]) -> Vec<OsString> {
        let mut pathspecs = paths
            .iter()
            .filter_map(|path| self.logical_repo_path(path))
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();
        pathspecs.sort_unstable();
        pathspecs.dedup();
        pathspecs.into_iter().map(OsString::from).collect()
    }
}

fn git_pathspec_chunks(pathspecs: &[OsString], budget: usize) -> Vec<&[OsString]> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (index, pathspec) in pathspecs.iter().enumerate() {
        let pathspec_bytes = pathspec.to_string_lossy().len() + 1;
        if index > start && bytes.saturating_add(pathspec_bytes) > budget {
            chunks.push(&pathspecs[start..index]);
            start = index;
            bytes = 0;
        }
        bytes = bytes.saturating_add(pathspec_bytes);
    }
    if start < pathspecs.len() {
        chunks.push(&pathspecs[start..]);
    }
    chunks
}

fn git_pathspec_chunk_budget() -> usize {
    #[cfg(unix)]
    {
        // ARG_MAX includes the inherited environment and argv pointer storage. Use half of the
        // remaining space and retain fixed headroom so the same path works across Unix hosts.
        let arg_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
        let arg_max = usize::try_from(arg_max).unwrap_or(GIT_PATHSPEC_CHUNK_MIN_BYTES * 2);
        let environment_bytes = env::vars_os()
            .map(|(key, value)| key.to_string_lossy().len() + value.to_string_lossy().len() + 2)
            .sum::<usize>();
        arg_max
            .saturating_sub(environment_bytes + GIT_ARGV_HEADROOM_BYTES)
            .saturating_div(2)
            .clamp(GIT_PATHSPEC_CHUNK_MIN_BYTES, GIT_PATHSPEC_CHUNK_MAX_BYTES)
    }

    #[cfg(not(unix))]
    {
        24 * 1024
    }
}

fn parse_git_name_status_z(output: &[u8]) -> Vec<(String, String)> {
    let mut parsed = Vec::new();
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        if let Some(tab_index) = record.iter().position(|byte| *byte == b'\t') {
            let status = String::from_utf8_lossy(&record[..tab_index]).into_owned();
            let path = String::from_utf8_lossy(&record[tab_index + 1..]).into_owned();
            parsed.push((status, path));
            continue;
        }

        let status = String::from_utf8_lossy(record).into_owned();
        let Some(path) = records.next() else {
            break;
        };
        parsed.push((status, String::from_utf8_lossy(path).into_owned()));
    }

    parsed
}

fn parse_git_status_v2_z(output: &[u8]) -> Option<(String, Vec<(String, String)>)> {
    let mut head = None;
    let mut records = Vec::new();
    let chunks = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    for record in chunks {
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            head = Some(String::from_utf8_lossy(value).into_owned());
            continue;
        }
        if record.starts_with(b"# ") {
            continue;
        }
        if record.starts_with(b"?") {
            let path = record.strip_prefix(b"? ")?;
            records.push(("?".to_string(), String::from_utf8_lossy(path).into_owned()));
            continue;
        }
        if record.starts_with(b"1 ") {
            let mut fields = record.splitn(9, |byte| *byte == b' ');
            let _record_type = fields.next()?;
            let status = String::from_utf8_lossy(fields.next()?).into_owned();
            for _ in 0..6 {
                fields.next()?;
            }
            let path = fields.next()?;
            records.push((status, String::from_utf8_lossy(path).into_owned()));
            continue;
        }
        // `--no-renames` should keep the common path on record type 1. If Git emits a record
        // shape we do not understand, use the legacy probes instead of risking a partial set.
        return None;
    }
    Some((head?, records))
}

fn file_mtime_nanos(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
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
    if dirty.is_empty() {
        return if published_overlay_paths.is_empty() {
            OverlayUpdateMode::NoChange
        } else {
            OverlayUpdateMode::Rebuild
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
    use std::ffi::OsString;

    use super::{
        decide_overlay_update_mode, git_pathspec_chunks, parse_git_status_v2_z, DirtyPathSet,
        OverlayUpdateMode,
    };

    fn dirty_paths(modified: &[&str], deleted: &[&str], new: &[&str]) -> DirtyPathSet {
        DirtyPathSet {
            modified: modified.iter().map(|path| (*path).to_string()).collect(),
            deleted: deleted.iter().map(|path| (*path).to_string()).collect(),
            new: new.iter().map(|path| (*path).to_string()).collect(),
        }
    }

    #[test]
    fn parses_status_v2_branch_and_path_records() {
        let output = b"# branch.oid abc123\0# branch.head main\0"
            .iter()
            .chain(b"1 .M N... 100644 100644 100644 hash hash src/changed.rs\0")
            .chain(b"? new file.rs\0")
            .copied()
            .collect::<Vec<_>>();

        assert_eq!(
            parse_git_status_v2_z(&output),
            Some((
                "abc123".to_string(),
                vec![
                    (".M".to_string(), "src/changed.rs".to_string()),
                    ("?".to_string(), "new file.rs".to_string()),
                ],
            ))
        );
    }

    #[test]
    fn rejects_status_v2_records_without_branch_or_known_shape() {
        assert_eq!(parse_git_status_v2_z(b"? orphan.txt\0"), None);
        assert_eq!(
            parse_git_status_v2_z(b"# branch.oid abc123\0u invalid\0"),
            None
        );
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
    fn git_pathspec_chunks_keep_large_event_batches_bounded() {
        let budget = 64 * 1024;
        let pathspecs = (0..40_000)
            .map(|index| {
                OsString::from(format!(
                    "very/long/generated/path/{index:05}/candidate_file_for_git_probe.cc"
                ))
            })
            .collect::<Vec<_>>();

        let chunks = git_pathspec_chunks(&pathspecs, budget);

        assert!(chunks.len() > 1);
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).sum::<usize>(),
            40_000
        );
        for chunk in chunks {
            let bytes = chunk
                .iter()
                .map(|pathspec| pathspec.to_string_lossy().len() + 1)
                .sum::<usize>();
            assert!(bytes <= budget || chunk.len() == 1);
        }
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
