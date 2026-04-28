use std::{collections::HashSet, path::Path};

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

use super::git_snapshot::GitSnapshotIndex;

#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    pub snapshot: GitSnapshotIndex,
}

pub(crate) struct WorkspaceQuerySnapshot {
    base_snapshot: Option<IndexSnapshotReader>,
    overlay_snapshot: Option<IndexSnapshotReader>,
    shadowed_base_paths: HashSet<String>,
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
        (!self.shadowed_base_paths.is_empty()).then_some(&self.shadowed_base_paths)
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
}

impl WorkspaceSearchView {
    pub fn open(build_config: BuildConfig) -> Result<Self> {
        let build_config = build_config.normalized()?;
        Ok(Self { build_config })
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
                .unwrap_or_default(),
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
            .unwrap_or_default();
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
        let mut results = if let Some(snapshot) = overlay_snapshot {
            snapshot.search(&QueryRequest {
                pattern: query.regex_pattern.clone(),
                mode: query.search_mode,
                case_insensitive: query.case_insensitive,
                top_k: query.global_max_results,
                filters: query_filters.clone(),
                prefer_worktree_for_git_head_blob: false,
            })?
        } else {
            return Ok(empty_search_results());
        };

        absolutize_search_results(&mut results, &self.build_config.repo_path);
        if let Some(filter) = filter {
            filter_search_results(&mut results, filter);
        }
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
        let mut results = snapshot.search(&QueryRequest {
            pattern: query.regex_pattern.clone(),
            mode: query.search_mode,
            case_insensitive: query.case_insensitive,
            top_k: query.global_max_results,
            filters: query_filters.clone(),
            prefer_worktree_for_git_head_blob,
        })?;
        absolutize_search_results(&mut results, &self.build_config.repo_path);
        if let Some(excluded_paths) = excluded_paths {
            exclude_search_results(&mut results, excluded_paths);
        }
        if let Some(filter) = filter {
            filter_search_results(&mut results, filter);
        }
        Ok(results)
    }

    fn shadowed_base_paths_for_overlay_snapshot(
        &self,
        overlay_snapshot: &IndexSnapshotReader,
    ) -> Result<HashSet<String>> {
        let visible = overlay_snapshot.visible_keys()?;
        Ok(visible
            .live
            .into_iter()
            .chain(visible.deleted)
            .map(|key| absolutize_path(&key.logical_path, &self.build_config.repo_path))
            .collect())
    }
}

fn query_with_remaining_global_limit(
    query: &QueryConfig,
    current_results: &SearchResults,
) -> Option<QueryConfig> {
    let Some(limit) = query.effective_global_max_results() else {
        return Some(query.clone());
    };
    let consumed = current_results.result_units(query.search_mode);
    if consumed >= limit {
        return None;
    }

    let mut next_query = query.clone();
    next_query.global_max_results = Some(limit - consumed);
    Some(next_query)
}

fn merge_search_results(mut base: SearchResults, mut overlay: SearchResults) -> SearchResults {
    let overlay_paths = search_result_paths(&overlay);
    let base_paths = search_result_paths(&base);
    let overlapping_paths = base_paths.intersection(&overlay_paths).count();

    base.candidate_docs = base
        .candidate_docs
        .saturating_add(overlay.candidate_docs)
        .saturating_sub(overlapping_paths);
    base.bytes_searched += overlay.bytes_searched;
    base.matched_lines += overlay.matched_lines;
    base.matched_occurrences += overlay.matched_occurrences;
    base.hits
        .retain(|hit| !overlay_paths.contains(hit.path.as_str()));
    base.file_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.file_match_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.file_counts.append(&mut overlay.file_counts);
    base.file_match_counts
        .append(&mut overlay.file_match_counts);
    base.hits.append(&mut overlay.hits);
    base.hits
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    base.file_counts
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    base.file_match_counts
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    recompute_result_totals(&mut base);
    base
}

fn empty_search_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
        hits: Vec::new(),
    }
}

fn filter_search_results(results: &mut SearchResults, filter: &PathFilter) {
    results
        .hits
        .retain(|hit| filter.matches_file(Path::new(&hit.path)));
    results
        .file_counts
        .retain(|count| filter.matches_file(Path::new(&count.path)));
    results
        .file_match_counts
        .retain(|count| filter.matches_file(Path::new(&count.path)));

    let unique_paths = results
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .chain(results.file_counts.iter().map(|count| count.path.as_str()))
        .chain(
            results
                .file_match_counts
                .iter()
                .map(|count| count.path.as_str()),
        )
        .collect::<std::collections::HashSet<_>>();
    results.candidate_docs = unique_paths.len();
    results.bytes_searched = 0;
    recompute_result_totals(results);
}

fn exclude_search_results(results: &mut SearchResults, excluded_paths: &HashSet<String>) {
    results
        .hits
        .retain(|hit| !excluded_paths.contains(&hit.path));
    results
        .file_counts
        .retain(|count| !excluded_paths.contains(&count.path));
    results
        .file_match_counts
        .retain(|count| !excluded_paths.contains(&count.path));

    let unique_paths = results
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .chain(results.file_counts.iter().map(|count| count.path.as_str()))
        .chain(
            results
                .file_match_counts
                .iter()
                .map(|count| count.path.as_str()),
        )
        .collect::<std::collections::HashSet<_>>();
    results.candidate_docs = unique_paths.len();
    recompute_result_totals(results);
}

fn absolutize_search_results(results: &mut SearchResults, repo_root: &Path) {
    for hit in &mut results.hits {
        hit.path = absolutize_path(&hit.path, repo_root);
    }
    for count in &mut results.file_counts {
        count.path = absolutize_path(&count.path, repo_root);
    }
    for count in &mut results.file_match_counts {
        count.path = absolutize_path(&count.path, repo_root);
    }
}

fn absolutize_path(path: &str, repo_root: &Path) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        repo_root.join(path).to_string_lossy().into_owned()
    }
}

fn search_result_paths(results: &SearchResults) -> HashSet<&str> {
    results
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .chain(results.file_counts.iter().map(|count| count.path.as_str()))
        .chain(
            results
                .file_match_counts
                .iter()
                .map(|count| count.path.as_str()),
        )
        .collect()
}

fn recompute_result_totals(results: &mut SearchResults) {
    let matched_lines_from_hits = results
        .hits
        .iter()
        .map(|hit| hit.matches.len())
        .sum::<usize>();
    let matched_occurrences_from_hits = results
        .hits
        .iter()
        .map(|hit| hit.matches.len())
        .sum::<usize>();

    results.searches_with_match = if !results.hits.is_empty() {
        results.hits.len()
    } else if !results.file_counts.is_empty() {
        results.file_counts.len()
    } else {
        results.file_match_counts.len()
    };
    results.matched_lines = if !results.file_counts.is_empty() {
        results
            .file_counts
            .iter()
            .map(|count| count.matched_lines)
            .sum()
    } else if !results.hits.is_empty() {
        matched_lines_from_hits
    } else if !results.file_match_counts.is_empty() {
        results.matched_lines
    } else {
        0
    };
    results.matched_occurrences = if !results.file_match_counts.is_empty() {
        results
            .file_match_counts
            .iter()
            .map(|count| count.matched_occurrences)
            .sum()
    } else if !results.hits.is_empty() {
        matched_occurrences_from_hits
    } else {
        0
    };
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
