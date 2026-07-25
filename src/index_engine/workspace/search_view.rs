use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

mod results;

use crate::{
    config::BuildConfig,
    error::AppError,
    index_engine::{
        query::{QueryFilters, QueryRequest},
        reader::IndexSnapshotReader,
        GitRevision, WorkspaceBaseSnapshotIndex,
    },
    path_filter::PathFilter,
    search::SearchResults,
    QueryConfig, Result,
};

use self::results::{
    absolutize_search_results, empty_search_results, merge_search_results,
    query_with_remaining_global_limit,
};
use super::git_snapshot::GitSnapshotIndex;

#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    pub snapshot: GitSnapshotIndex,
}

pub(crate) struct WorkspaceQuerySnapshot {
    base_snapshot: Option<IndexSnapshotReader>,
    overlay_snapshot: Option<IndexSnapshotReader>,
    shadowed_base_paths: Arc<HashSet<String>>,
}

impl WorkspaceQuerySnapshot {
    pub(crate) fn base_snapshot(&self) -> Option<&IndexSnapshotReader> {
        self.base_snapshot.as_ref()
    }

    pub(crate) fn overlay_snapshot(&self) -> Option<&IndexSnapshotReader> {
        self.overlay_snapshot.as_ref()
    }

    pub(crate) fn has_base_snapshot(&self) -> bool {
        self.base_snapshot().is_some()
    }

    pub(crate) fn has_overlay_snapshot(&self) -> bool {
        self.overlay_snapshot().is_some()
    }

    pub(crate) fn shadowed_base_paths(&self) -> Option<&HashSet<String>> {
        (!self.shadowed_base_paths.is_empty()).then_some(self.shadowed_base_paths.as_ref())
    }

    pub(crate) fn shadowed_base_paths_len(&self) -> usize {
        self.shadowed_base_paths.len()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        !self.shadowed_base_paths.is_empty()
    }

    pub(crate) fn search(
        &self,
        view: &WorkspaceSearchView,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        prefer_worktree_for_git_head_blob: bool,
    ) -> Result<SearchResults> {
        let base_results = view.search_base_snapshot(
            query,
            query_filters,
            filter,
            Some(&self.shadowed_base_paths),
            self.base_snapshot(),
            prefer_worktree_for_git_head_blob,
        )?;
        let Some(overlay_query) = query_with_remaining_global_limit(query, &base_results) else {
            return Ok(base_results);
        };

        let overlay_results = view.search_overlay_snapshot(
            &overlay_query,
            query_filters,
            filter,
            self.overlay_snapshot(),
        )?;
        Ok(merge_search_results(base_results, overlay_results))
    }
}

pub struct WorkspaceSearchView {
    build_config: BuildConfig,
    shadowed_base_paths_cache: Mutex<Option<ShadowedBasePathsCache>>,
}

struct ShadowedBasePathsCache {
    manifest_id: crate::index_engine::ManifestId,
    refreshed_seq_no: u64,
    paths: Arc<HashSet<String>>,
}

impl WorkspaceSearchView {
    pub fn open(build_config: BuildConfig) -> Result<Self> {
        let build_config = build_config.normalized()?;
        Ok(Self {
            build_config,
            shadowed_base_paths_cache: Mutex::new(None),
        })
    }

    pub fn current_snapshot(&self) -> Result<SnapshotHandle> {
        let base = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
            .ok_or_else(|| {
                AppError::InvalidIndex("base snapshot shard metadata is missing".into())
            })?;
        Ok(SnapshotHandle {
            snapshot: GitSnapshotIndex {
                repo_root: self.build_config.repo_path.clone(),
                snapshot_key: base.snapshot_key,
                revision: base.revision,
            },
        })
    }

    pub fn ensure_git_snapshot(&self, revision: GitRevision) -> Result<SnapshotHandle> {
        let snapshot = self.current_snapshot()?;
        if snapshot.snapshot.revision.as_ref() == Some(&revision) {
            return Ok(snapshot);
        }
        Err(AppError::Protocol(format!(
            "git snapshot {} is not available in current base snapshot",
            revision.0
        )))
    }

    pub fn search_current_snapshot(
        &self,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        base_snapshot: Option<&IndexSnapshotReader>,
    ) -> Result<SearchResults> {
        let snapshot = self.current_snapshot()?;
        self.search_snapshot(&snapshot, query, query_filters, filter, base_snapshot)
    }

    pub(crate) fn search_workspace_snapshots(
        &self,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        base_snapshot: Option<&IndexSnapshotReader>,
        overlay_snapshot: Option<&IndexSnapshotReader>,
    ) -> Result<SearchResults> {
        // Workspace query truth comes only from the read-only base snapshot plus the currently
        // published overlay snapshot. Dirty catalog/runtime state may schedule future publishes,
        // but it does not participate in query result correctness.
        WorkspaceQuerySnapshot {
            base_snapshot: base_snapshot.cloned(),
            overlay_snapshot: overlay_snapshot.cloned(),
            shadowed_base_paths: overlay_snapshot
                .map(|snapshot| self.shadowed_base_paths_for_overlay_snapshot(snapshot))
                .transpose()?
                .unwrap_or_else(|| Arc::new(HashSet::new())),
        }
        .search(self, query, query_filters, filter, true)
    }

    pub(crate) fn open_workspace_query_snapshot(
        &self,
        base_snapshot: Option<IndexSnapshotReader>,
        overlay_snapshot: Option<IndexSnapshotReader>,
    ) -> Result<WorkspaceQuerySnapshot> {
        let shadowed_base_paths = overlay_snapshot
            .as_ref()
            .map(|snapshot| self.shadowed_base_paths_for_overlay_snapshot(snapshot))
            .transpose()?
            .unwrap_or_else(|| Arc::new(HashSet::new()));
        Ok(WorkspaceQuerySnapshot {
            base_snapshot,
            overlay_snapshot,
            shadowed_base_paths,
        })
    }

    pub fn search_snapshot(
        &self,
        snapshot: &SnapshotHandle,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        base_snapshot: Option<&IndexSnapshotReader>,
    ) -> Result<SearchResults> {
        let current = self.current_snapshot()?;
        if current.snapshot.snapshot_key != snapshot.snapshot.snapshot_key {
            return Err(AppError::Protocol(format!(
                "snapshot handle {} is stale; current base snapshot is {}",
                snapshot.snapshot.snapshot_key, current.snapshot.snapshot_key
            )));
        }
        self.search_base_snapshot(query, query_filters, filter, None, base_snapshot, false)
    }

    pub(crate) fn search_overlay_snapshot(
        &self,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        overlay_snapshot: Option<&IndexSnapshotReader>,
    ) -> Result<SearchResults> {
        let mut effective_filters = query_filters.clone();
        if let Some(filter) = filter {
            effective_filters.set_path_filter(self.build_config.repo_path.clone(), filter.clone());
        }
        let mut results = if let Some(snapshot) = overlay_snapshot {
            snapshot.search(&QueryRequest::from_config(query, effective_filters, false))?
        } else {
            return Ok(empty_search_results());
        };

        absolutize_search_results(&mut results, &self.build_config.repo_path);
        Ok(results)
    }

    pub(crate) fn search_base_snapshot(
        &self,
        query: &QueryConfig,
        query_filters: &QueryFilters,
        filter: Option<&PathFilter>,
        excluded_paths: Option<&HashSet<String>>,
        base_snapshot: Option<&IndexSnapshotReader>,
        prefer_worktree_for_git_head_blob: bool,
    ) -> Result<SearchResults> {
        let snapshot = base_snapshot
            .ok_or_else(|| AppError::InvalidIndex("base snapshot reader is unavailable".into()))?;
        let mut effective_filters = query_filters.clone();
        if let Some(filter) = filter {
            effective_filters.set_path_filter(self.build_config.repo_path.clone(), filter.clone());
        }
        let request =
            QueryRequest::from_config(query, effective_filters, prefer_worktree_for_git_head_blob);
        let mut results = if let Some(excluded_paths) = excluded_paths {
            snapshot.search_excluding_paths(&request, excluded_paths)?
        } else {
            snapshot.search(&request)?
        };
        absolutize_search_results(&mut results, &self.build_config.repo_path);
        Ok(results)
    }

    fn shadowed_base_paths_for_overlay_snapshot(
        &self,
        overlay_snapshot: &IndexSnapshotReader,
    ) -> Result<Arc<HashSet<String>>> {
        let manifest_id = overlay_snapshot.manifest().manifest_id.clone();
        let refreshed_seq_no = overlay_snapshot.manifest().refreshed_seq_no;
        let mut cache = self
            .shadowed_base_paths_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.as_ref().filter(|cached| {
            cached.manifest_id == manifest_id && cached.refreshed_seq_no == refreshed_seq_no
        }) {
            return Ok(Arc::clone(&cached.paths));
        }

        let visible = overlay_snapshot.visible_keys()?;
        let paths = Arc::new(
            visible
                .live
                .into_iter()
                .chain(visible.deleted)
                .map(|key| key.logical_path)
                .collect(),
        );
        *cache = Some(ShadowedBasePathsCache {
            manifest_id,
            refreshed_seq_no,
            paths: Arc::clone(&paths),
        });
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tempfile::TempDir;

    use super::WorkspaceSearchView;
    use crate::{
        config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
        error::AppError,
        index_engine::{
            query::QueryFilters, DocKey, EngineConfig, IndexDocument, IndexEngine, RepoId, ShardId,
            WorkspaceBaseSnapshotIndex,
        },
        search::SearchMode,
    };

    fn build_config(repo: &std::path::Path, index: &std::path::Path) -> BuildConfig {
        BuildConfig {
            repo_path: repo.to_path_buf(),
            storage_root: index.to_path_buf(),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::NoIgnore,
            include_hidden: true,
            max_file_size: 1 << 20,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    fn fixed_query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: pattern.to_string(),
            patterns: vec![pattern.to_string()],
            fixed_strings: true,
            search_mode: SearchMode::CountOnly,
            ..QueryConfig::default()
        }
    }

    fn test_doc(repo_id: &RepoId, path: &str, text: &str) -> IndexDocument {
        IndexDocument {
            key: DocKey {
                repo_id: repo_id.clone(),
                logical_path: path.into(),
            },
            repo_id: repo_id.clone(),
            logical_path: path.into(),
            file_name: path.rsplit('/').next().unwrap_or(path).into(),
            language: Some("txt".into()),
            size: text.len() as u64,
            modified_at: 1,
            content_hash: [0u8; 32],
            source_bytes: Arc::<[u8]>::from(text.as_bytes().to_vec()),
        }
    }

    fn materialize_base_snapshot_shard(repo: &std::path::Path, index: &std::path::Path) {
        let build_config = build_config(repo, index);
        let snapshot_key = "base-repo-test-snapshot".to_string();
        let repo_id = RepoId(repo.to_string_lossy().into_owned());
        let mut on_progress = |_event| {};
        let mut should_cancel = || false;
        WorkspaceBaseSnapshotIndex::rebuild_for_snapshot_identity(
            &build_config,
            &repo_id,
            &snapshot_key,
            None,
            &mut on_progress,
            &mut should_cancel,
        )
        .expect("write base shard marker");
    }

    #[test]
    fn search_current_snapshot_requires_base_snapshot_reader() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        let index = repo.join(".flashgrep-index-engine");
        fs::create_dir_all(&repo).expect("create repo");
        fs::write(repo.join("base.txt"), "BASE_SNAPSHOT_FALLBACK_TOKEN\n").expect("write base");
        materialize_base_snapshot_shard(&repo, &index);

        let view = WorkspaceSearchView::open(build_config(&repo, &index)).expect("open view");
        let error = view
            .search_current_snapshot(
                &fixed_query("BASE_SNAPSHOT_FALLBACK_TOKEN"),
                &QueryFilters::default(),
                None,
                None,
            )
            .expect_err("search should require base snapshot");
        assert!(matches!(error, AppError::InvalidIndex(_)));
    }

    #[test]
    fn search_workspace_snapshots_shadow_base_with_overlay_live_keys() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        let index = repo.join(".flashgrep-index-engine");
        fs::create_dir_all(&repo).expect("create repo");
        let file_path = repo.join("base.txt");
        fs::write(&file_path, "OLD_TOKEN\n").expect("write base file");

        let repo_id = RepoId("repo".into());
        let base_engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId("base-test".into()),
            index_root: temp.path().join("base-engine"),
            ..EngineConfig::default()
        })
        .expect("open base engine");
        base_engine
            .writer()
            .add_document(test_doc(&repo_id, "base.txt", "OLD_TOKEN\n"))
            .expect("add base doc");
        base_engine.writer().commit().expect("commit base doc");

        let overlay_engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-test".into()),
            index_root: temp.path().join("overlay-engine"),
            ..EngineConfig::default()
        })
        .expect("open overlay engine");
        overlay_engine
            .writer()
            .add_document(test_doc(&repo_id, "base.txt", "NEW_TOKEN\n"))
            .expect("add overlay doc");
        overlay_engine
            .writer()
            .refresh()
            .expect("refresh overlay doc");

        let view = WorkspaceSearchView::open(build_config(&repo, &index)).expect("open view");
        let base_snapshot = base_engine
            .reader()
            .open_committed()
            .expect("open base snapshot");
        let overlay_snapshot = overlay_engine
            .reader()
            .open_snapshot()
            .expect("open overlay snapshot");

        let cached_paths_first = view
            .shadowed_base_paths_for_overlay_snapshot(&overlay_snapshot)
            .expect("build shadow path cache");
        let cached_paths_second = view
            .shadowed_base_paths_for_overlay_snapshot(&overlay_snapshot)
            .expect("reuse shadow path cache");
        assert!(Arc::ptr_eq(&cached_paths_first, &cached_paths_second));

        let new_results = view
            .search_workspace_snapshots(
                &fixed_query("NEW_TOKEN"),
                &QueryFilters::default(),
                None,
                Some(&base_snapshot),
                Some(&overlay_snapshot),
            )
            .expect("search new token");
        assert_eq!(new_results.matched_lines, 1);
        assert_eq!(new_results.file_counts.len(), 1);
        assert_eq!(
            fs::canonicalize(&new_results.file_counts[0].path).expect("canonicalize result path"),
            fs::canonicalize(&file_path).expect("canonicalize file path"),
        );

        let old_results = view
            .search_workspace_snapshots(
                &fixed_query("OLD_TOKEN"),
                &QueryFilters::default(),
                None,
                Some(&base_snapshot),
                Some(&overlay_snapshot),
            )
            .expect("search old token");
        assert_eq!(old_results.matched_lines, 0);
    }

    #[test]
    fn search_workspace_snapshots_shadow_base_with_overlay_deletes() {
        let temp = TempDir::new().expect("temp dir");
        let repo = temp.path().join("repo");
        let index = repo.join(".flashgrep-index-engine");
        fs::create_dir_all(&repo).expect("create repo");
        let file_path = repo.join("base.txt");
        fs::write(&file_path, "OLD_TOKEN\n").expect("write base file");

        let repo_id = RepoId("repo".into());
        let key = DocKey {
            repo_id: repo_id.clone(),
            logical_path: "base.txt".into(),
        };
        let base_engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId("base-delete-test".into()),
            index_root: temp.path().join("base-delete-engine"),
            ..EngineConfig::default()
        })
        .expect("open base engine");
        base_engine
            .writer()
            .add_document(test_doc(&repo_id, "base.txt", "OLD_TOKEN\n"))
            .expect("add base doc");
        base_engine.writer().commit().expect("commit base doc");

        let overlay_engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId("overlay-delete-test".into()),
            index_root: temp.path().join("overlay-delete-engine"),
            ..EngineConfig::default()
        })
        .expect("open overlay engine");
        overlay_engine
            .writer()
            .delete_document(&key)
            .expect("delete overlay key");
        overlay_engine
            .writer()
            .refresh()
            .expect("refresh overlay delete");

        let view = WorkspaceSearchView::open(build_config(&repo, &index)).expect("open view");
        let base_snapshot = base_engine
            .reader()
            .open_committed()
            .expect("open base snapshot");
        let overlay_snapshot = overlay_engine
            .reader()
            .open_snapshot()
            .expect("open overlay snapshot");

        let old_results = view
            .search_workspace_snapshots(
                &fixed_query("OLD_TOKEN"),
                &QueryFilters::default(),
                None,
                Some(&base_snapshot),
                Some(&overlay_snapshot),
            )
            .expect("search old token");
        assert_eq!(old_results.matched_lines, 0);
        assert!(old_results.file_counts.is_empty());
    }
}
