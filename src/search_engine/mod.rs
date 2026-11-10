use std::{collections::BTreeMap, io::ErrorKind};

use crate::{
    base_snapshot_build::BaseSnapshotBuildOptions,
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::{scan_paths, RepositoryFile},
    index_engine::{reader::SnapshotPathCatalog, RepoId, WorkspaceBaseSnapshotIndex},
    path_filter::PathFilter,
    search::{FileCount, FileMatchCount, SearchResults},
    workspace::{WorkspaceIndex, WorkspaceIndexOptions},
    worktree_diff::{diff_against_snapshot_catalog, IndexWorktreeDiff},
};

mod document;
mod scan;

pub(crate) use document::{SearchDocument, SearchDocumentSource};
use scan::{scan_options, scan_text_files};
pub(crate) use scan::{search_documents, search_scanned_files};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPolicy {
    IndexOnly,
    FallbackToScan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBackend {
    Index,
    Scan,
}

/// Complete response for one `SearchEngine` query invocation.
///
/// - `backend` records which execution backend was used.
/// - `results` contains the match payload for that execution.
#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub backend: SearchBackend,
    pub results: SearchResults,
}

/// Simple index-or-scan search facade.
///
/// Prefer this type when you want direct search execution with automatic
/// workspace overlay materialization but without a long-lived daemon/runtime.
/// For richer workspace lifecycle, status, and probe-driven maintenance,
/// prefer `WorkspaceIndex` or the daemon API.
#[derive(Debug, Clone)]
pub struct SearchEngine {
    build_config: BuildConfig,
}

impl SearchEngine {
    pub fn new(build_config: BuildConfig) -> Self {
        Self {
            build_config: build_config.normalize_lossy(),
        }
    }

    pub fn build_base_snapshot(&self) -> Result<usize> {
        self.build_base_snapshot_with_options(BaseSnapshotBuildOptions::default())
    }

    pub fn build_base_snapshot_with_options(
        &self,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<usize> {
        self.run_base_snapshot_build_with_options(options)
    }

    pub fn rebuild_base_snapshot(&self) -> Result<usize> {
        self.rebuild_base_snapshot_with_options(BaseSnapshotBuildOptions::default())
    }

    pub fn rebuild_base_snapshot_with_options(
        &self,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<usize> {
        self.run_base_snapshot_rebuild_with_options(options)
    }

    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        let catalog = self.open_base_path_catalog()?;
        diff_against_snapshot_catalog(&self.build_config, &catalog)
    }

    /// Returns a simplified human-readable stale reason, if any.
    ///
    /// For structured freshness state, prefer `WorkspaceIndex::probe_freshness`.
    pub fn stale_reason(&self) -> Result<Option<String>> {
        let diff = self.diff_against_worktree()?;
        if let Some(path) = diff.modified_files.first() {
            return Ok(Some(format!("indexed file changed: {path}")));
        }
        if let Some(path) = diff.deleted_files.first() {
            return Ok(Some(format!("indexed file disappeared: {path}")));
        }
        if let Some(path) = diff.new_files.first() {
            return Ok(Some(format!("new file is missing from the index: {path}")));
        }
        Ok(None)
    }

    pub fn search(&self, config: &QueryConfig, policy: SearchPolicy) -> Result<SearchResponse> {
        self.search_with_filter(config, None, policy)
    }

    pub fn scan_search(&self, config: &QueryConfig) -> Result<SearchResults> {
        self.scan_search_with_filter(config, None)
    }

    pub fn scan_search_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        let files = self.scan_repo_files(filter)?;
        search_scanned_files(config, &files)
    }

    pub fn visible_paths(&self, filter: Option<&PathFilter>) -> Result<Vec<String>> {
        WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: self.build_config.clone(),
        })?
        .snapshot()?
        .visible_paths(filter)
    }

    pub fn candidate_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: self.build_config.clone(),
        })?
        .snapshot()?
        .candidate_paths(config, filter)
    }

    pub fn count_matches_by_file_including_zero(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileCount>> {
        let mut counts = self
            .search_with_filter(config, filter, SearchPolicy::IndexOnly)?
            .results
            .file_counts
            .into_iter()
            .map(|count| (count.path.clone(), count.matched_lines))
            .collect::<BTreeMap<_, _>>();

        Ok(self
            .visible_paths(filter)?
            .into_iter()
            .map(|path| FileCount {
                matched_lines: counts.remove(&path).unwrap_or_default(),
                path,
            })
            .collect())
    }

    pub fn count_total_matches_by_file(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileMatchCount>> {
        Ok(self
            .search_with_filter(config, filter, SearchPolicy::IndexOnly)?
            .results
            .file_match_counts)
    }

    pub fn count_total_matches_by_file_including_zero(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileMatchCount>> {
        let mut counts = self
            .search_with_filter(config, filter, SearchPolicy::IndexOnly)?
            .results
            .file_match_counts
            .into_iter()
            .map(|count| (count.path.clone(), count.matched_occurrences))
            .collect::<BTreeMap<_, _>>();

        Ok(self
            .visible_paths(filter)?
            .into_iter()
            .map(|path| FileMatchCount {
                matched_occurrences: counts.remove(&path).unwrap_or_default(),
                path,
            })
            .collect())
    }

    pub fn files_without_matches(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        Ok(self
            .count_matches_by_file_including_zero(config, filter)?
            .into_iter()
            .filter(|count| count.matched_lines == 0)
            .map(|count| count.path)
            .collect())
    }

    /// Convenience search entry with an optional external path filter.
    ///
    /// This keeps path-scoping opt-in without introducing additional
    /// `search_with_*` API variants.
    pub fn search_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
    ) -> Result<SearchResponse> {
        match self.open_base_snapshot_reader() {
            Ok(_) => Ok(SearchResponse {
                backend: SearchBackend::Index,
                results: WorkspaceIndex::open(WorkspaceIndexOptions {
                    build_config: self.build_config.clone(),
                })?
                .search_with_filter(config, filter)?,
            }),
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                self.search_without_index(config, filter, policy)
            }
            Err(error) => Err(error),
        }
    }

    fn search_without_index(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
    ) -> Result<SearchResponse> {
        match policy {
            SearchPolicy::IndexOnly => Err(AppError::InvalidIndex(format!(
                "base snapshot does not exist at {}",
                self.build_config.storage_root.display()
            ))),
            SearchPolicy::FallbackToScan => {
                let files = self.scan_repo_files(filter)?;
                Ok(SearchResponse {
                    backend: SearchBackend::Scan,
                    results: search_scanned_files(config, &files)?,
                })
            }
        }
    }

    fn run_base_snapshot_build_with_options(
        &self,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<usize> {
        Ok(WorkspaceBaseSnapshotIndex::build_with_options(
            &self.build_config,
            &self.repo_id(),
            options,
        )?
        .indexed_docs)
    }

    fn run_base_snapshot_rebuild_with_options(
        &self,
        options: BaseSnapshotBuildOptions<'_>,
    ) -> Result<usize> {
        Ok(WorkspaceBaseSnapshotIndex::rebuild_with_options(
            &self.build_config,
            &self.repo_id(),
            options,
        )?
        .indexed_docs)
    }

    fn repo_id(&self) -> RepoId {
        RepoId(self.build_config.repo_path.to_string_lossy().into_owned())
    }

    fn open_base_snapshot_reader(
        &self,
    ) -> Result<crate::index_engine::reader::IndexSnapshotReader> {
        let metadata = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&self.build_config)?
            .ok_or_else(|| {
                AppError::Io(std::io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "base snapshot shard is missing for {}",
                        self.build_config.repo_path.display()
                    ),
                ))
            })?;
        let index = WorkspaceBaseSnapshotIndex::open_if_matching(
            &self.build_config,
            &metadata.snapshot_key,
        )?
        .ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "base snapshot shard marker is missing or stale for {}",
                self.build_config.repo_path.display()
            ))
        })?;
        index.open_committed()
    }

    fn open_base_path_catalog(&self) -> Result<SnapshotPathCatalog> {
        self.open_base_snapshot_reader()?.path_catalog()
    }

    fn scan_repo_files(&self, filter: Option<&PathFilter>) -> Result<Vec<RepositoryFile>> {
        scan_text_files(scan_paths(
            std::slice::from_ref(&self.build_config.repo_path),
            Some(&self.build_config.storage_root),
            scan_options(&self.build_config),
            filter,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tempfile::tempdir;

    use super::{search_documents, SearchDocument};
    use crate::{
        config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
        search::SearchMode,
    };

    use super::{SearchBackend, SearchEngine, SearchPolicy};

    fn line_match_fixed_query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: regex::escape(pattern),
            patterns: vec![pattern.to_string()],
            fixed_strings: true,
            search_mode: SearchMode::LineMatches,
            ..QueryConfig::default()
        }
    }

    fn line_match_regex_query(pattern: &str, global_max_results: Option<usize>) -> QueryConfig {
        QueryConfig {
            regex_pattern: pattern.to_string(),
            patterns: vec![pattern.to_string()],
            global_max_results,
            search_mode: SearchMode::LineMatches,
            ..QueryConfig::default()
        }
    }

    fn line_match_fixed_query_with_limit(pattern: &str, max_count: usize) -> QueryConfig {
        QueryConfig {
            regex_pattern: regex::escape(pattern),
            patterns: vec![pattern.to_string()],
            fixed_strings: true,
            max_count: Some(max_count),
            search_mode: SearchMode::LineMatches,
            ..QueryConfig::default()
        }
    }

    fn line_match_fixed_query_with_global_limit(
        pattern: &str,
        global_max_results: Option<usize>,
    ) -> QueryConfig {
        QueryConfig {
            regex_pattern: regex::escape(pattern),
            patterns: vec![pattern.to_string()],
            fixed_strings: true,
            global_max_results,
            search_mode: SearchMode::LineMatches,
            ..QueryConfig::default()
        }
    }

    #[test]
    fn search_documents_matches_fixed_string_phrase_with_spaces_in_loaded_bytes() {
        let pattern = "VersionTuple probe validation content gamma";
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            pattern.len() as u64,
            0,
            Arc::<[u8]>::from(format!("prefix {pattern} suffix\n").into_bytes()),
        )];

        let results = search_documents(&line_match_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 1);
        assert_eq!(results.matched_occurrences, 1);
        let line_matches = results.flattened_line_matches();
        assert_eq!(line_matches.len(), 1);
        assert_eq!(line_matches[0].line_number, 1);
    }

    #[test]
    fn search_documents_matches_fixed_single_token_in_loaded_bytes() {
        let pattern = "VERSIONTUPLE_WATCHER_VALIDATION_GAMMA_20260410";
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            pattern.len() as u64,
            0,
            Arc::<[u8]>::from(format!("prefix {pattern} suffix\n").into_bytes()),
        )];

        let results = search_documents(&line_match_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 1);
        assert_eq!(results.matched_occurrences, 1);
        let line_matches = results.flattened_line_matches();
        assert_eq!(line_matches.len(), 1);
        assert_eq!(line_matches[0].line_number, 1);
    }

    #[test]
    fn search_documents_returns_each_matching_line_for_fixed_string_queries() {
        let pattern = "needle";
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            32,
            0,
            Arc::<[u8]>::from(b"needle one\nskip me\nneedle two\n".to_vec()),
        )];

        let results = search_documents(&line_match_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 2);
        assert_eq!(results.matched_occurrences, 2);
        let line_matches = results.flattened_line_matches();
        assert_eq!(line_matches.len(), 2);
        assert_eq!(line_matches[0].line_number, 1);
        assert_eq!(line_matches[1].line_number, 3);
    }

    #[test]
    fn search_documents_reuses_line_number_state_without_changing_line_matches() {
        let pattern = "needle";
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            128,
            0,
            Arc::<[u8]>::from(
                b"needle first\r\nneedle second needle again\r\nskip\r\nneedle fourth\r\n".to_vec(),
            ),
        )];

        let results = search_documents(&line_match_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 3);
        assert_eq!(results.matched_occurrences, 4);
        let line_matches = results.flattened_line_matches();
        assert_eq!(line_matches.len(), 3);
        assert_eq!(line_matches[0].line_number, 1);
        assert_eq!(line_matches[1].line_number, 2);
        assert_eq!(line_matches[2].line_number, 4);
    }

    #[test]
    fn search_documents_honors_global_result_limit_for_regex_verification() {
        let documents = vec![
            SearchDocument::from_loaded_bytes(
                "one.txt".into(),
                12,
                0,
                Arc::<[u8]>::from(b"foo1\nfoo2\n".to_vec()),
            ),
            SearchDocument::from_loaded_bytes(
                "two.txt".into(),
                5,
                0,
                Arc::<[u8]>::from(b"foo3\n".to_vec()),
            ),
        ];

        let results = search_documents(&line_match_regex_query(r"foo\d", Some(2)), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 2);
        assert_eq!(results.searches_with_match, 1);
        assert_eq!(results.flattened_line_matches().len(), 2);
    }

    #[test]
    fn search_engine_builds_and_searches_from_base_shard_without_legacy_index() {
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");
        let file_path = repo_path.join("main.rs");
        fs::write(&file_path, "const NAME: &str = \"BASE_SHARD_ONLY\";\n")
            .expect("write should succeed");
        let build_config = BuildConfig {
            git_mode: Default::default(),
            repo_path: repo_path.clone(),
            storage_root: repo_path.join(".flashgrep-index-engine"),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 32,
        };
        let engine = SearchEngine::new(build_config.clone());

        let indexed_docs = engine.build_base_snapshot().expect("build should succeed");

        assert_eq!(indexed_docs, 1);
        assert!(
            !build_config.storage_root.join("CURRENT").exists(),
            "search engine build should not publish legacy CURRENT"
        );

        let response = engine
            .search(
                &line_match_fixed_query("BASE_SHARD_ONLY"),
                SearchPolicy::IndexOnly,
            )
            .expect("search should succeed");

        assert_eq!(response.backend, SearchBackend::Index);
        assert_eq!(response.results.matched_lines, 1);
        assert!(engine
            .diff_against_worktree()
            .expect("diff should succeed")
            .is_empty());

        fs::write(&file_path, "const NAME: &str = \"WORKTREE_ONLY\";\n")
            .expect("rewrite should succeed");
        let diff = engine.diff_against_worktree().expect("diff should succeed");
        let expected_path = fs::canonicalize(&file_path)
            .expect("file should canonicalize")
            .to_string_lossy()
            .into_owned();

        assert_eq!(diff.modified_files, vec![expected_path.clone()]);
        assert_eq!(
            engine.stale_reason().expect("stale reason should succeed"),
            Some(format!("indexed file changed: {expected_path}"))
        );
    }

    #[test]
    fn indexed_search_honors_match_limit_in_parallel_verify() {
        let temp = tempdir().expect("temp dir should succeed");
        let repo_path = temp.path().join("repo");
        fs::create_dir_all(&repo_path).expect("repo dir should succeed");

        for i in 0..130 {
            let file_path = repo_path.join(format!("src/file_{i:03}.txt"));
            let parent = file_path.parent().expect("file should have parent");
            fs::create_dir_all(parent).expect("parent dir should succeed");
            fs::write(&file_path, "before\nTARGET\nbetween\nTARGET\nafter\n")
                .expect("write should succeed");
        }

        let engine = SearchEngine::new(BuildConfig {
            git_mode: Default::default(),
            repo_path: repo_path.clone(),
            storage_root: repo_path.join(".flashgrep-index-engine"),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 32,
        });
        engine.build_base_snapshot().expect("build should succeed");

        let response = engine
            .search(
                &line_match_fixed_query_with_limit("TARGET", 1),
                SearchPolicy::IndexOnly,
            )
            .expect("search should succeed");

        assert_eq!(response.backend, SearchBackend::Index);
        assert_eq!(response.results.searches_with_match, 130);
        assert_eq!(response.results.matched_lines, 130);
        assert_eq!(response.results.matched_occurrences, 130);
        let line_matches = response.results.flattened_line_matches();
        assert_eq!(line_matches.len(), 130);

        assert!(line_matches.iter().all(|matched| matched.line_number == 2));
    }

    #[test]
    fn line_matches_apply_default_global_result_limit() {
        let contents = (0..20_000)
            .map(|_| "TARGET\n")
            .collect::<String>()
            .into_bytes();
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            contents.len() as u64,
            0,
            Arc::<[u8]>::from(contents),
        )];

        let results = search_documents(
            &line_match_fixed_query_with_global_limit("TARGET", None),
            &documents,
        )
        .expect("search should succeed");

        assert_eq!(
            results.flattened_line_matches().len(),
            crate::config::DEFAULT_LINE_MATCH_LIMIT
        );
        assert_eq!(
            results.matched_lines,
            crate::config::DEFAULT_LINE_MATCH_LIMIT
        );
        assert_eq!(
            results.matched_occurrences,
            crate::config::DEFAULT_LINE_MATCH_LIMIT
        );
    }
}
