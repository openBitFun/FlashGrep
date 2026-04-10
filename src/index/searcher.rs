use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Instant,
};

use aho_corasick::{AhoCorasick, MatchKind};
use memmap2::Mmap;

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::{is_text_file, resolve_repo_path, scan_repository},
    index::format::{
        DocMetaRef, DocsData, IndexBuildSettings, IndexLayout, IndexMetadata, LookupTable,
        PostingsData,
    },
    path_filter::PathFilter,
    planner::{plan, QueryBranch},
    search::{FileCount, FileMatchCount, SearchHit, SearchMode, SearchResults},
    tokenizer::{create, TokenizerOptions},
};

#[path = "searcher_count.rs"]
mod count;
#[path = "searcher_multi_literal.rs"]
mod multi_literal;
#[path = "searcher_query.rs"]
mod query;
#[path = "searcher_shared.rs"]
mod shared;
#[path = "searcher_verify.rs"]
pub(crate) mod verify;

use count::DocMatchCount;
pub(crate) use count::{CountKind as CountVerifyKind, VerifyPlan as CountVerifyPlan};
use multi_literal::{
    collect_candidates as collect_multi_literal_candidates, prepare as prepare_multi_literal,
    search as search_multi_literal,
};
pub(crate) use query::LinePrefilter;
#[cfg(test)]
pub(crate) use query::LiteralPrefilter;

use query::{fold_query_literal, SearchPreparation};
use shared::doc_by_id;
use verify::{build_matcher, requires_multiline_verification, verify_candidates, SearchProfile};

const CANDIDATE_PROBE_WINDOW: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexWorktreeDiff {
    pub modified_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub new_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirtyPathKind {
    Modified,
    Deleted,
    New,
}

impl IndexWorktreeDiff {
    pub fn is_empty(&self) -> bool {
        self.modified_files.is_empty() && self.deleted_files.is_empty() && self.new_files.is_empty()
    }

    pub fn dirty_paths(&self) -> HashSet<&str> {
        self.modified_files
            .iter()
            .chain(self.deleted_files.iter())
            .chain(self.new_files.iter())
            .map(String::as_str)
            .collect()
    }
}

/// Low-level read-only index query API.
///
/// Prefer `SearchEngine` for product-facing integrations, and use this type
/// for advanced scenarios that need direct control over index-level behavior.
pub struct IndexSearcher {
    index_path: PathBuf,
    metadata: IndexMetadata,
    docs: DocsData,
    lookup: LookupTable,
    postings: PostingsData,
    repo_root: Option<PathBuf>,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
}

struct MultiLiteralPreparation {
    prepared: multi_literal::PreparedMultiLiteral,
    candidate_ids: Vec<u32>,
}

impl IndexSearcher {
    pub fn open(index_path: PathBuf) -> Result<Self> {
        let layout = IndexLayout::resolve(&index_path)?;
        let docs = DocsData::open(&layout.docs_path)?;
        let metadata = docs.metadata().clone();
        let lookup = LookupTable::open(&layout.lookup_path)?;
        let postings = PostingsData::open(&layout.postings_path)?;

        Ok(Self {
            index_path,
            metadata: metadata.clone(),
            docs,
            lookup,
            postings,
            repo_root: metadata
                .build
                .as_ref()
                .map(|build| resolve_repo_path(Path::new(&build.repo_root), ".")),
            tokenizer_mode: metadata.tokenizer,
            tokenizer_options: TokenizerOptions {
                min_sparse_len: metadata.min_sparse_len,
                max_sparse_len: metadata.max_sparse_len,
            },
        })
    }

    pub fn search(&self, config: &QueryConfig) -> Result<SearchResults> {
        self.search_with_filter(config, None)
    }

    pub fn build_settings(&self) -> Option<&IndexBuildSettings> {
        self.metadata.build.as_ref()
    }

    pub fn tokenizer_mode(&self) -> crate::config::TokenizerMode {
        self.tokenizer_mode
    }

    pub fn tokenizer_options(&self) -> &TokenizerOptions {
        &self.tokenizer_options
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn ensure_query_scope_supported(
        &self,
        include_hidden: bool,
        no_ignore: bool,
        max_file_size: Option<u64>,
    ) -> Result<()> {
        let Some(build) = self.build_settings() else {
            if include_hidden || no_ignore || max_file_size.is_some() {
                return Err(AppError::InvalidIndex(
                    "index metadata is too old for search-scope overrides; rebuild the index"
                        .into(),
                ));
            }
            return Ok(());
        };

        if include_hidden && !build.include_hidden {
            return Err(AppError::InvalidIndex(
                "searching hidden files requires an index built with --hidden".into(),
            ));
        }
        if no_ignore && matches!(build.corpus_mode, crate::config::CorpusMode::RespectIgnore) {
            return Err(AppError::InvalidIndex(
                "searching ignored files requires an index built with --no-ignore".into(),
            ));
        }
        if max_file_size.is_some_and(|value| value > build.max_file_size) {
            return Err(AppError::InvalidIndex(format!(
                "search --max-filesize={} exceeds the index build limit of {} bytes",
                max_file_size.unwrap_or_default(),
                build.max_file_size,
            )));
        }
        Ok(())
    }

    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        let Some(build) = self.build_settings() else {
            return Ok(IndexWorktreeDiff::default());
        };

        let current_files = scan_repository(&BuildConfig {
            repo_path: PathBuf::from(&build.repo_root),
            index_path: self.index_path.clone(),
            tokenizer: self.metadata.tokenizer,
            corpus_mode: build.corpus_mode,
            include_hidden: build.include_hidden,
            max_file_size: build.max_file_size,
            min_sparse_len: self.metadata.min_sparse_len,
            max_sparse_len: self.metadata.max_sparse_len,
        })?;
        let current = current_files
            .into_iter()
            .filter_map(|file| match is_text_file(&file.path) {
                Ok(true) => Some(Ok(file)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|file| {
                (
                    file.path.to_string_lossy().into_owned(),
                    (file.size, file.mtime_nanos),
                )
            })
            .collect::<HashMap<_, _>>();
        let indexed_paths = self
            .docs
            .iter()
            .map(|doc| self.doc_display_path_ref(doc))
            .collect::<HashSet<_>>();

        let mut modified_files = Vec::new();
        let mut deleted_files = Vec::new();
        for doc in self.docs.iter() {
            let doc_path = self.doc_display_path_ref(doc);
            match current.get(&doc_path) {
                Some(&(size, mtime_nanos))
                    if size == doc.size() && mtime_nanos == doc.mtime_nanos() => {}
                Some(_) => modified_files.push(doc_path),
                None => deleted_files.push(doc_path),
            }
        }

        let mut modified_files = modified_files;
        modified_files.sort_unstable();
        let mut deleted_files = deleted_files;
        deleted_files.sort_unstable();
        let mut new_files = current
            .keys()
            .filter(|path| !indexed_paths.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        new_files.sort_unstable();

        Ok(IndexWorktreeDiff {
            modified_files,
            deleted_files,
            new_files,
        })
    }

    /// Returns the first detected human-readable stale reason, if any.
    ///
    /// This is a simplified explanation API. For structured freshness state,
    /// use `WorkspaceIndex::probe_freshness`.
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

    pub fn search_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        self.search_with_path_overrides(config, filter, None, None)
    }

    pub(crate) fn search_with_path_overrides(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        excluded_paths: Option<&HashSet<String>>,
    ) -> Result<SearchResults> {
        let profile = SearchProfile::enabled();
        let total_started = Instant::now();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());

        if let Some(results) = self.try_search_with_rg_scan_backend(
            config,
            &plan,
            filter,
            allowed_paths,
            excluded_paths,
        )? {
            profile.finish(
                total_started.elapsed(),
                results.candidate_docs,
                results.matched_lines,
            );
            return Ok(results);
        }

        if !config.has_context()
            && !matches!(config.search_mode, SearchMode::CountMatches)
            && config.max_count.is_none()
            && !config.fixed_strings
            && !config.word_regexp
            && !config.line_regexp
        {
            if let Some(multi_literal) = self.prepare_multi_literal_search(
                &plan,
                config.case_insensitive,
                config.top_k_tokens,
                filter,
                allowed_paths,
                &profile,
            )? {
                let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
                let candidate_ids = if filter.is_none() && allowed_paths.is_none() {
                    multi_literal.candidate_ids.clone()
                } else {
                    self.filter_candidate_ids_with_exclusions(
                        &multi_literal.candidate_ids,
                        filter,
                        allowed_paths,
                        excluded_paths,
                    )?
                };
                let match_started = Instant::now();
                let results = search_multi_literal(
                    &self.docs,
                    Some(candidate_ids.as_slice()),
                    &multi_literal.prepared,
                    config.search_mode,
                    &resolve_path,
                )?;
                profile.record_verify(match_started.elapsed());
                profile.finish(
                    total_started.elapsed(),
                    results.candidate_docs,
                    results.matched_lines,
                );
                return Ok(results);
            }
        }

        let preparation =
            self.prepare_indexed_search(config, &plan, filter, allowed_paths, &profile)?;
        let candidate_ids = self.filter_candidate_ids_with_exclusions(
            &preparation.candidate_ids,
            filter,
            allowed_paths,
            excluded_paths,
        )?;
        let candidate_doc_count = candidate_ids.len();

        let multiline_verifier = requires_multiline_verification(config)?;
        let match_started = Instant::now();
        let (matched_lines, matched_occurrences, searches_with_match, hits, bytes_searched) =
            match config.search_mode {
                SearchMode::CountOnly => {
                    let regex_started = Instant::now();
                    let verify_plan = CountVerifyPlan::compile(
                        config,
                        multiline_verifier,
                        preparation.line_prefilter.clone(),
                        CountVerifyKind::Lines,
                    )?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
                    let stats = verify_plan.verify_candidate_count_stats(
                        &candidate_ids,
                        self.docs.len(),
                        &resolve_path,
                    )?;
                    (
                        stats.matched_lines,
                        0,
                        stats.matched_docs,
                        Vec::new(),
                        self.sum_doc_sizes(&candidate_ids)?,
                    )
                }
                SearchMode::CountMatches => {
                    let regex_started = Instant::now();
                    let verify_plan = CountVerifyPlan::compile(
                        config,
                        multiline_verifier,
                        preparation.line_prefilter.clone(),
                        CountVerifyKind::Occurrences,
                    )?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
                    let stats = verify_plan.verify_candidate_count_stats(
                        &candidate_ids,
                        self.docs.len(),
                        &resolve_path,
                    )?;
                    (
                        stats.matched_lines,
                        stats.matched_occurrences,
                        stats.matched_docs,
                        Vec::new(),
                        self.sum_doc_sizes(&candidate_ids)?,
                    )
                }
                _ => {
                    let regex_started = Instant::now();
                    let matcher = build_matcher(config, multiline_verifier)?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
                    let mut hits = Vec::new();
                    let mut matched_lines = 0usize;
                    let mut matched_occurrences = 0usize;
                    let mut searches_with_match = 0usize;
                    let mut bytes_searched = 0u64;
                    let outcomes = verify_candidates(
                        &candidate_ids,
                        self.docs.len(),
                        &resolve_path,
                        &matcher,
                        preparation.line_prefilter.as_ref(),
                        multiline_verifier,
                        config.search_mode,
                        config.max_count,
                        config.before_context,
                        config.after_context,
                    )?;
                    for task in outcomes {
                        matched_lines += task.outcome.matched_lines;
                        matched_occurrences += task.outcome.matched_occurrences;
                        bytes_searched += task.outcome.bytes_searched;
                        if !task.outcome.matches.is_empty() {
                            searches_with_match += 1;
                            let doc = doc_by_id(&self.docs, task.doc_id)?;
                            hits.push(SearchHit {
                                path: self.doc_display_path_ref(doc),
                                matches: task.outcome.matches,
                                lines: task.outcome.lines,
                            });
                        }
                    }
                    (
                        matched_lines,
                        matched_occurrences,
                        searches_with_match,
                        hits,
                        bytes_searched,
                    )
                }
            };
        profile.record_verify(match_started.elapsed());
        profile.finish(total_started.elapsed(), candidate_doc_count, matched_lines);

        Ok(SearchResults {
            candidate_docs: candidate_doc_count,
            searches_with_match,
            bytes_searched,
            matched_lines,
            matched_occurrences,
            hits,
        })
    }

    pub fn candidate_paths(&self, config: &QueryConfig) -> Result<Vec<String>> {
        self.candidate_paths_with_filter(config, None)
    }

    pub fn candidate_paths_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        self.candidate_paths_with_allowed_paths(config, filter, None)
    }

    pub(crate) fn candidate_paths_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<String>> {
        Ok(self
            .candidate_docs_with_allowed_paths(config, filter, allowed_paths)?
            .into_iter()
            .map(|doc| self.doc_display_path(&doc))
            .collect())
    }

    pub(crate) fn candidate_docs_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<crate::index::format::DocMeta>> {
        let profile = SearchProfile::enabled();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());
        self.candidate_docs_for_plan_with_allowed_paths(config, &plan, filter, allowed_paths)
    }

    pub(crate) fn candidate_docs_for_plan_with_allowed_paths(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<crate::index::format::DocMeta>> {
        let candidate_ids = self.candidate_doc_ids_for_plan_with_allowed_paths(
            config,
            plan,
            filter,
            allowed_paths,
        )?;
        candidate_ids
            .into_iter()
            .map(|doc_id| Ok(doc_by_id(&self.docs, doc_id)?.to_owned()))
            .collect()
    }

    pub(crate) fn candidate_doc_ids_for_plan_with_allowed_paths(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        let profile = SearchProfile::enabled();
        let candidate_ids = if let Some(multi_literal) = self.prepare_multi_literal_search(
            plan,
            config.case_insensitive,
            config.top_k_tokens,
            filter,
            allowed_paths,
            &profile,
        )? {
            multi_literal.candidate_ids
        } else {
            self.prepare_indexed_search(config, plan, filter, allowed_paths, &profile)?
                .candidate_ids
        };
        self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)
    }

    #[cfg(test)]
    pub(crate) fn candidate_doc_ids_by_branch_for_plan_with_allowed_paths(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<Vec<u32>>> {
        let profile = SearchProfile::enabled();
        let candidate_started = Instant::now();

        if plan.fallback_to_scan {
            profile.record_candidates(candidate_started.elapsed());
            return Ok(vec![self.filter_candidate_ids(
                &self.all_doc_ids(),
                filter,
                allowed_paths,
            )?]);
        }

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let mut posting_cache = HashMap::new();
        let mut literal_scan_cache = HashMap::new();
        let mut branches = Vec::with_capacity(plan.branches.len());

        for branch in &plan.branches {
            let branch_candidate_ids = self
                .select_branch_candidates(
                    branch,
                    tokenizer.as_ref(),
                    config.top_k_tokens,
                    true,
                    filter,
                    allowed_paths,
                    &mut posting_cache,
                    &mut literal_scan_cache,
                )?
                .unwrap_or_else(|| self.all_doc_ids());
            branches.push(self.filter_candidate_ids(
                &branch_candidate_ids,
                filter,
                allowed_paths,
            )?);
        }

        profile.record_candidates(candidate_started.elapsed());
        Ok(branches)
    }

    pub(crate) fn try_candidate_doc_ids_by_branch_without_literal_scan_for_plan_with_allowed_paths(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Option<Vec<Vec<u32>>>> {
        let profile = SearchProfile::enabled();
        let candidate_started = Instant::now();

        if plan.fallback_to_scan {
            profile.record_candidates(candidate_started.elapsed());
            return Ok(Some(vec![self.filter_candidate_ids(
                &self.all_doc_ids(),
                filter,
                allowed_paths,
            )?]));
        }

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let mut posting_cache = HashMap::new();
        let mut literal_scan_cache = HashMap::new();
        let mut branches = Vec::with_capacity(plan.branches.len());

        for branch in &plan.branches {
            let Some(branch_candidate_ids) = self.select_branch_candidates(
                branch,
                tokenizer.as_ref(),
                config.top_k_tokens,
                false,
                filter,
                allowed_paths,
                &mut posting_cache,
                &mut literal_scan_cache,
            )?
            else {
                return Ok(None);
            };
            branches.push(self.filter_candidate_ids(
                &branch_candidate_ids,
                filter,
                allowed_paths,
            )?);
        }

        profile.record_candidates(candidate_started.elapsed());
        Ok(Some(branches))
    }

    pub fn count_matches_by_file(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileCount>> {
        self.count_matches_by_file_internal(config, filter, None, false)
    }

    pub fn count_matches_by_file_including_zero(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileCount>> {
        self.count_matches_by_file_internal(config, filter, None, true)
    }

    fn count_matches_by_file_internal(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        include_zero: bool,
    ) -> Result<Vec<FileCount>> {
        let profile = SearchProfile::enabled();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());
        let line_prefilter = query::LinePrefilter::compile(config, &plan);

        let candidate_ids = self
            .prepare_indexed_search(config, &plan, filter, allowed_paths, &profile)?
            .candidate_ids;
        let candidate_ids = self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)?;
        let multiline_verifier = requires_multiline_verification(config)?;
        let regex_started = Instant::now();
        let verify_plan = CountVerifyPlan::compile(
            config,
            multiline_verifier,
            line_prefilter,
            CountVerifyKind::Lines,
        )?;
        profile.record_regex_compile(regex_started.elapsed());
        let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
        let match_started = Instant::now();
        let counts = verify_plan.verify_candidate_counts_by_doc(
            &candidate_ids,
            self.docs.len(),
            &resolve_path,
        )?;
        profile.record_verify(match_started.elapsed());
        if include_zero {
            self.file_counts_from_doc_counts_including_zero(counts, filter, allowed_paths)
        } else {
            self.file_counts_from_doc_counts(counts)
        }
    }

    pub fn count_total_matches_by_file(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileMatchCount>> {
        self.count_total_matches_by_file_internal(config, filter, None, false)
    }

    pub fn count_total_matches_by_file_including_zero(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<FileMatchCount>> {
        self.count_total_matches_by_file_internal(config, filter, None, true)
    }

    fn count_total_matches_by_file_internal(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        include_zero: bool,
    ) -> Result<Vec<FileMatchCount>> {
        let profile = SearchProfile::enabled();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());

        let preparation =
            self.prepare_indexed_search(config, &plan, filter, allowed_paths, &profile)?;
        let candidate_ids = preparation.candidate_ids;
        let candidate_ids = self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)?;
        let multiline_verifier = requires_multiline_verification(config)?;
        let regex_started = Instant::now();
        let verify_plan = CountVerifyPlan::compile(
            config,
            multiline_verifier,
            preparation.line_prefilter,
            CountVerifyKind::Occurrences,
        )?;
        profile.record_regex_compile(regex_started.elapsed());
        let match_started = Instant::now();
        let resolve_path = |doc_id| self.resolve_doc_path_by_id(doc_id);
        let counts = verify_plan.verify_candidate_counts_by_doc(
            &candidate_ids,
            self.docs.len(),
            &resolve_path,
        )?;
        profile.record_verify(match_started.elapsed());

        let counts = counts
            .into_iter()
            .map(|task| {
                Ok(FileMatchCount {
                    path: self.doc_display_path_ref(doc_by_id(&self.docs, task.doc_id)?),
                    matched_occurrences: task.matched_occurrences,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if include_zero {
            self.file_match_counts_including_zero(counts, filter, allowed_paths)
        } else {
            Ok(counts
                .into_iter()
                .filter(FileMatchCount::has_match)
                .collect::<Vec<_>>())
        }
    }

    pub fn files_with_matches(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        self.files_with_matches_and_allowed_paths(config, filter, None)
    }

    pub(crate) fn files_with_matches_and_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<String>> {
        Ok(self
            .count_matches_by_file_internal(config, filter, allowed_paths, false)?
            .into_iter()
            .map(|count| count.path)
            .collect())
    }

    pub fn files_without_matches(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<String>> {
        self.files_without_matches_and_allowed_paths(config, filter, None)
    }

    pub(crate) fn files_without_matches_and_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<String>> {
        let matched = self
            .files_with_matches_and_allowed_paths(config, filter, allowed_paths)?
            .into_iter()
            .collect::<HashSet<_>>();
        Ok(self
            .indexed_paths_with_allowed_paths(filter, allowed_paths)
            .into_iter()
            .filter(|path| !matched.contains(path))
            .collect())
    }

    pub fn indexed_paths(&self, filter: Option<&PathFilter>) -> Vec<String> {
        self.indexed_paths_with_allowed_paths(filter, None)
    }

    pub fn indexed_doc_stats(&self, filter: Option<&PathFilter>) -> (usize, u64) {
        self.docs
            .iter()
            .filter(|doc| self.doc_matches_filters(*doc, filter, None))
            .fold((0usize, 0u64), |(count, bytes), doc| {
                (count + 1, bytes + doc.size())
            })
    }

    pub fn indexed_path_count(&self, filter: Option<&PathFilter>) -> usize {
        self.docs
            .iter()
            .filter(|doc| self.doc_matches_filters(*doc, filter, None))
            .count()
    }

    #[cfg(test)]
    pub(crate) fn doc_display_path_by_id(&self, doc_id: u32) -> Result<String> {
        Ok(self.doc_display_path_ref(doc_by_id(&self.docs, doc_id)?))
    }

    pub(crate) fn classify_dirty_path(&self, path: &str) -> Result<Option<DirtyPathKind>> {
        let indexed = self
            .docs
            .iter()
            .find(|doc| self.doc_display_path_ref(*doc) == path);

        match indexed {
            Some(doc) => match fs::metadata(path) {
                Ok(metadata) => {
                    if !metadata.is_file() || !is_text_file(Path::new(path))? {
                        return Ok(Some(DirtyPathKind::Deleted));
                    }
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| match u64::try_from(duration.as_nanos()) {
                            Ok(value) => value,
                            Err(_) => u64::MAX,
                        })
                        .unwrap_or_default();
                    if metadata.len() != doc.size() || modified != doc.mtime_nanos() {
                        Ok(Some(DirtyPathKind::Modified))
                    } else {
                        Ok(None)
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(Some(DirtyPathKind::Deleted))
                }
                Err(error) => Err(error.into()),
            },
            None => {
                let path = Path::new(path);
                if !path.exists() || !path.is_file() {
                    return Ok(None);
                }
                if is_text_file(path)? {
                    Ok(Some(DirtyPathKind::New))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) fn indexed_paths_with_allowed_paths(
        &self,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Vec<String> {
        self.docs
            .iter()
            .filter(|doc| self.doc_matches_filters(*doc, filter, allowed_paths))
            .map(|doc| self.doc_display_path_ref(doc))
            .collect()
    }

    fn doc_display_path_ref(&self, doc: DocMetaRef<'_>) -> String {
        self.doc_resolved_path_ref(doc)
            .to_string_lossy()
            .into_owned()
    }

    fn doc_resolved_path_ref(&self, doc: DocMetaRef<'_>) -> PathBuf {
        self.resolve_doc_path_str(doc.path())
    }

    fn resolve_doc_path_by_id(&self, doc_id: u32) -> Result<PathBuf> {
        Ok(self.doc_resolved_path_ref(doc_by_id(&self.docs, doc_id)?))
    }

    fn doc_display_path(&self, doc: &crate::index::format::DocMeta) -> String {
        self.doc_display_path_str(&doc.path)
    }

    fn doc_display_path_str(&self, path: &str) -> String {
        self.resolve_doc_path_str(path)
            .to_string_lossy()
            .into_owned()
    }

    fn resolve_doc_path_str(&self, path: &str) -> PathBuf {
        if Path::new(path).is_absolute() {
            return PathBuf::from(path);
        }

        if let Some(repo_root) = &self.repo_root {
            return resolve_repo_path(repo_root, path);
        }

        PathBuf::from(path)
    }

    fn prepare_indexed_search(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        profile: &SearchProfile,
    ) -> Result<SearchPreparation> {
        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let candidate_started = Instant::now();
        let candidate_ids = if plan.fallback_to_scan {
            self.all_doc_ids()
        } else {
            self.collect_candidates(
                &plan.branches,
                tokenizer.as_ref(),
                config.top_k_tokens,
                filter,
                allowed_paths,
            )?
        };
        profile.record_candidates(candidate_started.elapsed());
        Ok(SearchPreparation {
            candidate_ids,
            line_prefilter: query::LinePrefilter::compile(config, plan),
        })
    }

    fn try_search_with_rg_scan_backend(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        excluded_paths: Option<&HashSet<String>>,
    ) -> Result<Option<SearchResults>> {
        if !self.should_use_rg_scan_backend(config, plan, filter, allowed_paths, excluded_paths)? {
            return Ok(None);
        }
        self.search_counts_with_rg(config)
    }

    fn should_use_rg_scan_backend(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        excluded_paths: Option<&HashSet<String>>,
    ) -> Result<bool> {
        if !cfg!(target_os = "linux")
            || !matches!(
                config.search_mode,
                SearchMode::CountOnly | SearchMode::CountMatches
            )
            || config.has_context()
            || config.max_count.is_some()
            || config.multiline
            || config.dot_matches_new_line
            || filter.is_some()
            || allowed_paths.is_some()
            || excluded_paths.is_some_and(|paths| !paths.is_empty())
            || self.repo_root.is_none()
        {
            return Ok(false);
        }

        if plan.fallback_to_scan {
            return Ok(true);
        }

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        Ok(self
            .try_collect_candidates_without_literal_scan(
                &plan.branches,
                tokenizer.as_ref(),
                config.top_k_tokens,
                None,
                None,
            )?
            .is_none())
    }

    fn search_counts_with_rg(&self, config: &QueryConfig) -> Result<Option<SearchResults>> {
        let Some((matched_lines, searches_with_match)) = self.run_rg_count_query(config, false)?
        else {
            return Ok(None);
        };
        let matched_occurrences = if matches!(config.search_mode, SearchMode::CountMatches) {
            let Some((matched_occurrences, _)) = self.run_rg_count_query(config, true)? else {
                return Ok(None);
            };
            matched_occurrences
        } else {
            0
        };

        Ok(Some(SearchResults {
            candidate_docs: self.docs.len(),
            searches_with_match,
            bytes_searched: self.docs.iter().map(|doc| doc.size()).sum(),
            matched_lines,
            matched_occurrences,
            hits: Vec::new(),
        }))
    }

    fn run_rg_count_query(
        &self,
        config: &QueryConfig,
        count_matches: bool,
    ) -> Result<Option<(usize, usize)>> {
        let Some(repo_root) = self.repo_root.as_ref() else {
            return Ok(None);
        };
        let mut command = Command::new("rg");
        command
            .current_dir(repo_root)
            .arg("--with-filename")
            .arg("--color")
            .arg("never")
            .arg("--no-messages");
        if count_matches {
            command.arg("--count-matches");
        } else {
            command.arg("--count");
        }
        if config.case_insensitive {
            command.arg("--ignore-case");
        }
        if config.fixed_strings {
            command.arg("--fixed-strings");
            for pattern in &config.patterns {
                command.arg("-e").arg(pattern);
            }
        } else {
            if config.word_regexp {
                command.arg("--word-regexp");
            }
            if config.line_regexp {
                command.arg("--line-regexp");
            }
            command.arg("-e").arg(&config.regex_pattern);
        }

        if let Some(build) = self.build_settings() {
            command
                .arg("--max-filesize")
                .arg(build.max_file_size.to_string());
            if matches!(build.corpus_mode, crate::config::CorpusMode::NoIgnore) {
                command.arg("--no-ignore");
            }
            if build.include_hidden {
                command.arg("--hidden");
            }
        }

        command.arg("--");
        command.arg(".");

        let output = match command.output() {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(AppError::Protocol(format!(
                "rg scan backend failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let mut matched_total = 0usize;
        let mut searches_with_match = 0usize;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((_, count)) = line.rsplit_once(':') else {
                continue;
            };
            let count = count.parse::<usize>().map_err(|error| {
                AppError::Protocol(format!("failed to parse rg count output {line:?}: {error}"))
            })?;
            if count == 0 {
                continue;
            }
            matched_total += count;
            searches_with_match += 1;
        }

        Ok(Some((matched_total, searches_with_match)))
    }

    fn prepare_multi_literal_search(
        &self,
        plan: &crate::planner::QueryPlan,
        case_insensitive: bool,
        top_k_tokens: usize,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        profile: &SearchProfile,
    ) -> Result<Option<MultiLiteralPreparation>> {
        let Some(prepared) = plan
            .pure_literal_alternation
            .as_ref()
            .and_then(|alternation| prepare_multi_literal(alternation, case_insensitive))
        else {
            return Ok(None);
        };

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let candidate_started = Instant::now();
        let prefilter_ids = if plan.fallback_to_scan {
            None
        } else {
            Some(
                self.try_collect_candidates_without_literal_scan(
                    &plan.branches,
                    tokenizer.as_ref(),
                    top_k_tokens,
                    filter,
                    allowed_paths,
                )?
                .unwrap_or_else(|| {
                    self.collect_candidates(
                        &plan.branches,
                        tokenizer.as_ref(),
                        top_k_tokens,
                        filter,
                        allowed_paths,
                    )
                    .expect("fallback literal scan should succeed")
                }),
            )
        };
        let candidate_ids = collect_multi_literal_candidates(
            &self.docs,
            prefilter_ids.as_deref(),
            &prepared,
            &|doc_id| self.resolve_doc_path_by_id(doc_id),
        )?;
        profile.record_candidates(candidate_started.elapsed());

        Ok(Some(MultiLiteralPreparation {
            prepared,
            candidate_ids,
        }))
    }

    fn filter_candidate_ids(
        &self,
        candidate_ids: &[u32],
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        self.filter_candidate_ids_with_exclusions(candidate_ids, filter, allowed_paths, None)
    }

    fn filter_candidate_ids_with_exclusions(
        &self,
        candidate_ids: &[u32],
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        excluded_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        if filter.is_none() && allowed_paths.is_none() && excluded_paths.is_none() {
            return Ok(candidate_ids.to_vec());
        }

        Ok(candidate_ids
            .iter()
            .copied()
            .filter(|&doc_id| {
                let Ok(doc) = doc_by_id(&self.docs, doc_id) else {
                    return false;
                };
                self.doc_matches_scope(doc, filter, allowed_paths, excluded_paths)
            })
            .collect::<Vec<_>>())
    }

    fn file_counts_from_doc_counts(&self, counts: Vec<DocMatchCount>) -> Result<Vec<FileCount>> {
        counts
            .into_iter()
            .map(|count| {
                Ok(FileCount {
                    path: self.doc_display_path_ref(doc_by_id(&self.docs, count.doc_id)?),
                    matched_lines: count.matched_lines,
                })
            })
            .collect()
    }

    fn file_counts_from_doc_counts_including_zero(
        &self,
        counts: Vec<DocMatchCount>,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileCount>> {
        let matched = self.file_counts_from_doc_counts(counts)?;
        let matched = matched
            .into_iter()
            .map(|count| (count.path.clone(), count))
            .collect::<HashMap<_, _>>();
        Ok(self
            .indexed_paths_with_allowed_paths(filter, allowed_paths)
            .into_iter()
            .map(|path| {
                matched.get(&path).cloned().unwrap_or(FileCount {
                    path,
                    matched_lines: 0,
                })
            })
            .collect())
    }

    fn file_match_counts_including_zero(
        &self,
        counts: Vec<FileMatchCount>,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileMatchCount>> {
        let matched = counts
            .into_iter()
            .map(|count| (count.path.clone(), count))
            .collect::<HashMap<_, _>>();
        Ok(self
            .indexed_paths_with_allowed_paths(filter, allowed_paths)
            .into_iter()
            .map(|path| {
                matched.get(&path).cloned().unwrap_or(FileMatchCount {
                    path,
                    matched_occurrences: 0,
                })
            })
            .collect())
    }

    fn doc_matches_filters(
        &self,
        doc: DocMetaRef<'_>,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> bool {
        self.doc_matches_scope(doc, filter, allowed_paths, None)
    }

    fn doc_matches_scope(
        &self,
        doc: DocMetaRef<'_>,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        excluded_paths: Option<&HashSet<String>>,
    ) -> bool {
        let display_path = self.doc_display_path_ref(doc);
        if filter.is_some_and(|filter| !filter.matches_file(Path::new(&display_path))) {
            return false;
        }
        if allowed_paths.is_some_and(|allowed_paths| !allowed_paths.contains(&display_path)) {
            return false;
        }
        excluded_paths.is_none_or(|excluded_paths| !excluded_paths.contains(&display_path))
    }

    pub(crate) fn sum_doc_sizes(&self, candidate_ids: &[u32]) -> Result<u64> {
        let mut total = 0u64;
        for &doc_id in candidate_ids {
            total += doc_by_id(&self.docs, doc_id)?.size();
        }
        Ok(total)
    }

    fn collect_candidates(
        &self,
        branches: &[QueryBranch],
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        self.collect_candidates_with_literal_scan(
            branches,
            tokenizer,
            top_k,
            true,
            filter,
            allowed_paths,
        )
    }

    fn try_collect_candidates_without_literal_scan(
        &self,
        branches: &[QueryBranch],
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Option<Vec<u32>>> {
        let mut all_candidates = Vec::new();
        let mut posting_cache: HashMap<u64, Arc<[u32]>> = HashMap::new();
        let mut literal_scan_cache = HashMap::new();

        for branch in branches {
            let Some(branch_candidates) = self.select_branch_candidates(
                branch,
                tokenizer,
                top_k,
                false,
                filter,
                allowed_paths,
                &mut posting_cache,
                &mut literal_scan_cache,
            )?
            else {
                return Ok(None);
            };
            all_candidates = union_sorted(&all_candidates, &branch_candidates);
        }

        Ok(Some(all_candidates))
    }

    fn collect_candidates_with_literal_scan(
        &self,
        branches: &[QueryBranch],
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
        allow_literal_scan: bool,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        let mut all_candidates = Vec::new();
        let mut posting_cache: HashMap<u64, Arc<[u32]>> = HashMap::new();
        let mut literal_scan_cache = HashMap::new();

        for branch in branches {
            let Some(branch_candidates) = self.select_branch_candidates(
                branch,
                tokenizer,
                top_k,
                allow_literal_scan,
                filter,
                allowed_paths,
                &mut posting_cache,
                &mut literal_scan_cache,
            )?
            else {
                return Ok(self.all_doc_ids());
            };
            all_candidates = union_sorted(&all_candidates, &branch_candidates);
        }

        Ok(all_candidates)
    }
    fn select_branch_candidates(
        &self,
        branch: &QueryBranch,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
        allow_literal_scan: bool,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        posting_cache: &mut HashMap<u64, Arc<[u32]>>,
        literal_scan_cache: &mut HashMap<String, Vec<u32>>,
    ) -> Result<Option<Vec<u32>>> {
        let mut per_literal = Vec::new();
        let mut scanned_literals = Vec::new();

        for literal in &branch.literals {
            let query_literal = fold_query_literal(literal);
            let mut covering_hashes = Vec::new();
            tokenizer.collect_query_token_hashes(&query_literal, &mut covering_hashes);
            let literal_candidates =
                self.collect_literal_candidates(tokenizer, &query_literal, covering_hashes);
            match literal_candidates {
                LiteralCandidates::Impossible => return Ok(Some(Vec::new())),
                LiteralCandidates::Unavailable => {
                    scanned_literals.push(query_literal);
                }
                LiteralCandidates::Available(candidates) => per_literal.push(candidates),
            }
        }

        per_literal.sort_unstable_by(|left, right| {
            left.first()
                .map(|candidate| candidate.selection_rank())
                .cmp(&right.first().map(|candidate| candidate.selection_rank()))
        });

        if per_literal.is_empty() && scanned_literals.is_empty() {
            return Ok(None);
        }

        if !allow_literal_scan && !scanned_literals.is_empty() {
            return Ok(None);
        }

        let mut selected_hashes = HashSet::new();
        let mut selected_count = 0usize;
        let budget = self.branch_token_budget(top_k, per_literal.len());
        let candidate_limit = self.branch_candidate_limit();
        let mut branch_candidates: Option<Vec<u32>> = None;

        // Seed the branch with one strong gram per literal so concatenated
        // literals still contribute independent evidence before we tighten.
        for candidates in &per_literal {
            let Some(candidate) = next_unused_candidate(candidates, &selected_hashes) else {
                continue;
            };
            let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
            branch_candidates = Some(match branch_candidates.take() {
                Some(existing) => intersect_sorted(&existing, &docs),
                None => docs.to_vec(),
            });
            selected_hashes.insert(candidate.token_hash);
            selected_count += 1;

            if branch_candidates.as_ref().is_some_and(Vec::is_empty) {
                return Ok(Some(Vec::new()));
            }
        }

        while selected_count < budget
            && branch_candidates
                .as_ref()
                .is_some_and(|docs| docs.len() > candidate_limit)
        {
            let Some(current_candidates) = branch_candidates.as_ref() else {
                break;
            };
            let mut best_choice: Option<(TokenCandidate, usize)> = None;

            'candidate_search: for candidates in &per_literal {
                let mut probed = 0usize;
                for candidate in candidates {
                    if selected_hashes.contains(&candidate.token_hash) {
                        continue;
                    }
                    if probed >= CANDIDATE_PROBE_WINDOW {
                        break;
                    }
                    probed += 1;

                    let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
                    let reduced_len_bound = best_choice.as_ref().map_or(
                        current_candidates.len().saturating_add(1),
                        |(_, best_len)| best_len.saturating_add(1),
                    );
                    let reduced_len =
                        intersect_sorted_len_bounded(current_candidates, &docs, reduced_len_bound);
                    let replace = match &best_choice {
                        Some((best_candidate, best_reduced_len)) => {
                            reduced_len < *best_reduced_len
                                || (reduced_len == *best_reduced_len
                                    && candidate.selection_rank() < best_candidate.selection_rank())
                        }
                        None => true,
                    };
                    if replace {
                        best_choice = Some((*candidate, reduced_len));
                        if reduced_len == 0 {
                            break 'candidate_search;
                        }
                    }
                }
            }

            let Some((candidate, reduced_len)) = best_choice else {
                break;
            };
            if reduced_len >= current_candidates.len() {
                break;
            }

            let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
            let reduced = intersect_sorted(current_candidates, &docs);
            selected_hashes.insert(candidate.token_hash);
            branch_candidates = Some(reduced);
            selected_count += 1;
        }

        if branch_candidates.is_none()
            && !scanned_literals.is_empty()
            && scanned_literals
                .iter()
                .all(|literal| self.should_defer_unindexed_literal_scan(literal))
        {
            return Ok(None);
        }

        for literal in scanned_literals {
            let docs = self.scan_literal_docs(
                &literal,
                filter,
                allowed_paths,
                branch_candidates.as_deref(),
                literal_scan_cache,
            );
            branch_candidates = Some(match branch_candidates.take() {
                Some(existing) => intersect_sorted(&existing, &docs),
                None => docs,
            });
            if branch_candidates.as_ref().is_some_and(Vec::is_empty) {
                return Ok(Some(Vec::new()));
            }
        }

        let Some(branch_candidates) = branch_candidates else {
            return Ok(None);
        };

        Ok(Some(branch_candidates))
    }

    fn collect_literal_candidates(
        &self,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        query_literal: &str,
        mut covering_hashes: Vec<u64>,
    ) -> LiteralCandidates {
        covering_hashes.sort_unstable();
        covering_hashes.dedup();

        let should_use_document_fallbacks =
            matches!(self.tokenizer_mode, crate::config::TokenizerMode::Trigram);
        let tolerate_missing_covering_hashes = matches!(
            self.tokenizer_mode,
            crate::config::TokenizerMode::SparseNgram
        );
        let mut fallback_hashes = Vec::new();
        if should_use_document_fallbacks {
            tokenizer.collect_document_token_hashes(query_literal, &mut fallback_hashes);
            fallback_hashes.sort_unstable();
            fallback_hashes.dedup();
        }

        let mut selected = Vec::new();
        let mut seen_hashes = HashSet::new();

        for token_hash in covering_hashes {
            if !seen_hashes.insert(token_hash) {
                continue;
            }
            let Some(entry) = self.lookup.find(token_hash) else {
                if tolerate_missing_covering_hashes {
                    continue;
                }
                return LiteralCandidates::Impossible;
            };
            if entry.offset == u64::MAX {
                continue;
            }
            selected.push(TokenCandidate {
                token_hash,
                doc_freq: entry.doc_freq,
                source: CandidateSource::Covering,
                high_freq: entry.is_skipped_high_freq(),
            });
        }

        if should_use_document_fallbacks && selected.len() < 2 {
            for token_hash in fallback_hashes {
                if !seen_hashes.insert(token_hash) {
                    continue;
                }
                let Some(entry) = self.lookup.find(token_hash) else {
                    continue;
                };
                if entry.offset == u64::MAX {
                    continue;
                }
                selected.push(TokenCandidate {
                    token_hash,
                    doc_freq: entry.doc_freq,
                    source: CandidateSource::Fallback,
                    high_freq: entry.is_skipped_high_freq(),
                });
            }
        }

        selected.sort_unstable_by(|left, right| {
            left.selection_rank()
                .cmp(&right.selection_rank())
                .then_with(|| left.doc_freq.cmp(&right.doc_freq))
        });
        prune_high_frequency_candidates(&mut selected);

        if selected.is_empty() {
            LiteralCandidates::Unavailable
        } else {
            LiteralCandidates::Available(selected)
        }
    }

    fn read_posting_docs(
        &self,
        token_hash: u64,
        posting_cache: &mut HashMap<u64, Arc<[u32]>>,
    ) -> Result<Arc<[u32]>> {
        if let Some(cached) = posting_cache.get(&token_hash) {
            return Ok(Arc::clone(cached));
        }

        let docs = match self.lookup.find(token_hash) {
            Some(entry) if entry.offset != u64::MAX => {
                Arc::<[u32]>::from(self.postings.decode(entry)?)
            }
            Some(_) | None => Arc::<[u32]>::from(Vec::<u32>::new()),
        };
        posting_cache.insert(token_hash, Arc::clone(&docs));
        Ok(docs)
    }

    fn branch_token_budget(&self, top_k: usize, literal_count: usize) -> usize {
        let base = top_k.max(literal_count.max(1));
        match self.tokenizer_mode {
            crate::config::TokenizerMode::Trigram => base,
            crate::config::TokenizerMode::SparseNgram => {
                base.max(literal_count.saturating_mul(3)).max(12)
            }
        }
    }

    fn branch_candidate_limit(&self) -> usize {
        let doc_count = self.docs.len();
        if doc_count <= 512 {
            (doc_count / 4).max(8)
        } else if doc_count <= 8_192 {
            (doc_count / 8).max(64)
        } else {
            (doc_count / 16).max(512)
        }
    }

    fn should_defer_unindexed_literal_scan(&self, literal: &str) -> bool {
        matches!(
            self.tokenizer_mode,
            crate::config::TokenizerMode::SparseNgram
        ) && literal.chars().count() < self.tokenizer_options.min_sparse_len
    }

    fn scan_literal_docs(
        &self,
        literal: &str,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        candidate_ids: Option<&[u32]>,
        literal_scan_cache: &mut HashMap<String, Vec<u32>>,
    ) -> Vec<u32> {
        if let Some(candidate_ids) = candidate_ids {
            return self.scan_literal_docs_in_candidates(
                literal,
                filter,
                allowed_paths,
                candidate_ids,
            );
        }

        if let Some(cached) = literal_scan_cache.get(literal) {
            return cached.clone();
        }

        self.scan_literal_docs_batch(
            &[literal.to_string()],
            filter,
            allowed_paths,
            literal_scan_cache,
        );
        literal_scan_cache.get(literal).cloned().unwrap_or_default()
    }

    fn scan_literal_docs_in_candidates(
        &self,
        literal: &str,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        candidate_ids: &[u32],
    ) -> Vec<u32> {
        if literal.is_ascii() {
            let matcher = AhoCorasick::builder()
                .match_kind(MatchKind::Standard)
                .ascii_case_insensitive(true)
                .build([literal])
                .expect("ascii literal matcher should build");
            let mut docs = Vec::new();

            for &doc_id in candidate_ids {
                let Ok(doc) = doc_by_id(&self.docs, doc_id) else {
                    continue;
                };
                if !self.doc_matches_filters(doc, filter, allowed_paths) {
                    continue;
                }
                let path = self.doc_resolved_path_ref(doc);
                let Some(bytes) = map_doc_bytes(&path, doc.size()) else {
                    continue;
                };
                if matcher.find(&bytes).is_some() {
                    docs.push(doc_id);
                }
            }

            return docs;
        }

        let mut docs = Vec::new();
        for &doc_id in candidate_ids {
            let Ok(doc) = doc_by_id(&self.docs, doc_id) else {
                continue;
            };
            if !self.doc_matches_filters(doc, filter, allowed_paths) {
                continue;
            }
            let path = self.doc_resolved_path_ref(doc);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let folded = if text.is_ascii() {
                text.to_ascii_lowercase()
            } else {
                text.to_lowercase()
            };
            if folded.contains(literal) {
                docs.push(doc_id);
            }
        }

        docs
    }

    fn scan_literal_docs_batch(
        &self,
        literals: &[String],
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
        literal_scan_cache: &mut HashMap<String, Vec<u32>>,
    ) {
        let missing = literals
            .iter()
            .filter(|literal| !literal_scan_cache.contains_key(literal.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return;
        }

        let (ascii_literals, unicode_literals): (Vec<_>, Vec<_>) =
            missing.into_iter().partition(|literal| literal.is_ascii());

        if !ascii_literals.is_empty() {
            let matchers = ascii_literals
                .iter()
                .map(|literal| {
                    AhoCorasick::builder()
                        .match_kind(MatchKind::Standard)
                        .ascii_case_insensitive(true)
                        .build([literal.as_str()])
                        .expect("ascii literal matcher should build")
                })
                .collect::<Vec<_>>();
            let mut docs_by_literal = vec![Vec::new(); ascii_literals.len()];

            for doc in self
                .docs
                .iter()
                .filter(|doc| self.doc_matches_filters(*doc, filter, allowed_paths))
            {
                let path = self.doc_resolved_path_ref(doc);
                let Some(bytes) = map_doc_bytes(&path, doc.size()) else {
                    continue;
                };
                for (index, matcher) in matchers.iter().enumerate() {
                    if matcher.find(&bytes).is_some() {
                        docs_by_literal[index].push(doc.doc_id());
                    }
                }
            }

            for (literal, docs) in ascii_literals.into_iter().zip(docs_by_literal) {
                literal_scan_cache.insert(literal, docs);
            }
        }

        if !unicode_literals.is_empty() {
            let mut docs_by_literal = vec![Vec::new(); unicode_literals.len()];
            for doc in self
                .docs
                .iter()
                .filter(|doc| self.doc_matches_filters(*doc, filter, allowed_paths))
            {
                let path = self.doc_resolved_path_ref(doc);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let folded = if text.is_ascii() {
                    text.to_ascii_lowercase()
                } else {
                    text.to_lowercase()
                };
                for (index, literal) in unicode_literals.iter().enumerate() {
                    if folded.contains(literal) {
                        docs_by_literal[index].push(doc.doc_id());
                    }
                }
            }

            for (literal, docs) in unicode_literals.into_iter().zip(docs_by_literal) {
                literal_scan_cache.insert(literal, docs);
            }
        }
    }

    fn all_doc_ids(&self) -> Vec<u32> {
        self.docs.iter().map(|doc| doc.doc_id()).collect()
    }
}

fn map_doc_bytes(path: &Path, size: u64) -> Option<Mmap> {
    if size == 0 {
        return None;
    }
    let file = File::open(path).ok()?;
    unsafe { Mmap::map(&file).ok() }
}

#[derive(Debug, Clone, Copy)]
struct TokenCandidate {
    token_hash: u64,
    doc_freq: u32,
    source: CandidateSource,
    high_freq: bool,
}

impl TokenCandidate {
    fn selection_rank(self) -> (u8, u32, u8) {
        let freq_rank = u8::from(self.high_freq);
        (freq_rank, self.doc_freq, self.source.rank())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateSource {
    Covering,
    Fallback,
}

enum LiteralCandidates {
    Impossible,
    Unavailable,
    Available(Vec<TokenCandidate>),
}

impl CandidateSource {
    fn rank(self) -> u8 {
        match self {
            Self::Covering => 0,
            Self::Fallback => 1,
        }
    }
}

fn next_unused_candidate<'a>(
    candidates: &'a [TokenCandidate],
    used_hashes: &HashSet<u64>,
) -> Option<&'a TokenCandidate> {
    candidates
        .iter()
        .find(|candidate| !used_hashes.contains(&candidate.token_hash))
}

fn intersect_sorted(left: &[u32], right: &[u32]) -> Vec<u32> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let (small, large) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if large.len() >= small.len().saturating_mul(8) {
        return intersect_sorted_galloping(small, large);
    }

    let mut result = Vec::with_capacity(left.len().min(right.len()));
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                result.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    result
}

fn intersect_sorted_len_bounded(left: &[u32], right: &[u32], bound: usize) -> usize {
    if left.is_empty() || right.is_empty() || bound == 0 {
        return 0;
    }
    let (small, large) = if left.len() <= right.len() {
        (left, right)
    } else {
        (right, left)
    };
    if large.len() >= small.len().saturating_mul(8) {
        return intersect_sorted_len_bounded_galloping(small, large, bound);
    }

    let mut matched = 0usize;
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                matched += 1;
                if matched >= bound {
                    return matched;
                }
                left_index += 1;
                right_index += 1;
            }
        }
    }

    matched
}

fn intersect_sorted_galloping(small: &[u32], large: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(small.len());
    let mut large_start = 0usize;

    for &value in small {
        let next = lower_bound(&large[large_start..], value);
        large_start += next;
        if large_start >= large.len() {
            break;
        }
        if large[large_start] == value {
            result.push(value);
            large_start += 1;
        }
    }

    result
}

fn intersect_sorted_len_bounded_galloping(small: &[u32], large: &[u32], bound: usize) -> usize {
    let mut matched = 0usize;
    let mut large_start = 0usize;

    for &value in small {
        let next = lower_bound(&large[large_start..], value);
        large_start += next;
        if large_start >= large.len() {
            break;
        }
        if large[large_start] == value {
            matched += 1;
            if matched >= bound {
                return matched;
            }
            large_start += 1;
        }
    }

    matched
}

fn union_sorted(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(left.len() + right.len());
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => {
                result.push(left[left_index]);
                left_index += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(right[right_index]);
                right_index += 1;
            }
            std::cmp::Ordering::Equal => {
                result.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    result.extend_from_slice(&left[left_index..]);
    result.extend_from_slice(&right[right_index..]);
    result
}

fn lower_bound(values: &[u32], needle: u32) -> usize {
    let mut left = 0usize;
    let mut right = values.len();
    while left < right {
        let mid = left + (right - left) / 2;
        if values[mid] < needle {
            left = mid + 1;
        } else {
            right = mid;
        }
    }
    left
}

fn prune_high_frequency_candidates(candidates: &mut Vec<TokenCandidate>) {
    if candidates.iter().any(|candidate| !candidate.high_freq) {
        candidates.retain(|candidate| !candidate.high_freq);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{
        intersect_sorted, intersect_sorted_len_bounded, lower_bound,
        prune_high_frequency_candidates, union_sorted, CandidateSource, LiteralPrefilter,
        TokenCandidate,
    };
    use crate::planner::{QueryBranch, QueryPlan};

    #[test]
    fn galloping_intersection_matches_linear_result() {
        let small = vec![3, 50, 120, 9_999];
        let large = (0..20_000).step_by(3).collect::<Vec<u32>>();
        assert_eq!(intersect_sorted(&small, &large), vec![3, 120, 9_999]);
    }

    #[test]
    fn bounded_intersection_len_matches_full_len_when_under_bound() {
        let left = vec![1, 3, 5, 7, 9];
        let right = vec![0, 3, 4, 5, 8, 9];
        assert_eq!(
            intersect_sorted_len_bounded(&left, &right, usize::MAX),
            intersect_sorted(&left, &right).len()
        );
    }

    #[test]
    fn bounded_intersection_len_stops_at_bound() {
        let left = (0..10_000).step_by(2).collect::<Vec<u32>>();
        let right = (0..10_000).collect::<Vec<u32>>();
        assert_eq!(intersect_sorted_len_bounded(&left, &right, 3), 3);
    }

    #[test]
    fn union_sorted_preserves_order_and_uniqueness() {
        assert_eq!(
            union_sorted(&[1, 3, 5], &[2, 3, 4, 5, 6]),
            vec![1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn lower_bound_finds_first_not_less_than() {
        assert_eq!(lower_bound(&[2, 4, 6, 8], 1), 0);
        assert_eq!(lower_bound(&[2, 4, 6, 8], 6), 2);
        assert_eq!(lower_bound(&[2, 4, 6, 8], 7), 3);
        assert_eq!(lower_bound(&[2, 4, 6, 8], 9), 4);
    }

    #[test]
    fn high_frequency_candidates_are_dropped_when_selective_ones_exist() {
        let mut candidates = vec![
            TokenCandidate {
                token_hash: 1,
                doc_freq: 10,
                source: CandidateSource::Covering,
                high_freq: false,
            },
            TokenCandidate {
                token_hash: 2,
                doc_freq: 1_000,
                source: CandidateSource::Fallback,
                high_freq: true,
            },
        ];
        prune_high_frequency_candidates(&mut candidates);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].token_hash, 1);
    }

    #[test]
    fn literal_prefilter_requires_all_literals_in_a_branch() {
        let prefilter = LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: vec!["foo".into(), "bar".into()],
                }],
                fallback_to_scan: false,
                pure_literal_alternation: None,
            },
            false,
        )
        .expect("test should succeed");

        assert!(prefilter.text_may_match("prefix foo ... bar suffix"));
        assert!(!prefilter.text_may_match("prefix foo only"));
    }

    #[test]
    fn literal_prefilter_allows_any_branch_to_match() {
        let prefilter = LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![
                    QueryBranch {
                        literals: vec!["alpha".into()],
                    },
                    QueryBranch {
                        literals: vec!["beta".into(), "gamma".into()],
                    },
                ],
                fallback_to_scan: false,
                pure_literal_alternation: None,
            },
            false,
        )
        .expect("test should succeed");

        assert!(prefilter.text_may_match("beta then gamma"));
        assert!(prefilter.text_may_match("alpha"));
        assert!(!prefilter.text_may_match("beta only"));
    }

    #[test]
    fn literal_prefilter_supports_case_insensitive_matching() {
        let prefilter = LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: vec!["PM".into(), "RESUME".into()],
                }],
                fallback_to_scan: false,
                pure_literal_alternation: None,
            },
            true,
        )
        .expect("test should succeed");

        assert!(prefilter.text_may_match("prefix pm ... resume suffix"));
        assert!(prefilter.text_may_match("prefix Pm ... Resume suffix"));
        assert!(!prefilter.text_may_match("prefix pm only suffix"));
    }

    #[test]
    fn literal_prefilter_finds_first_candidate_line() {
        let prefilter = LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: vec!["foo".into(), "bar".into()],
                }],
                fallback_to_scan: false,
                pure_literal_alternation: None,
            },
            false,
        )
        .expect("test should succeed");

        let haystack = b"alpha only\nfoo only\nfoo and bar\nbar only\n";
        assert_eq!(prefilter.find_candidate_line(haystack, b'\n'), Some(20));
    }

    #[test]
    fn literal_prefilter_matches_case_insensitive_bytes() {
        let prefilter = LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: vec!["PM".into(), "RESUME".into()],
                }],
                fallback_to_scan: false,
                pure_literal_alternation: None,
            },
            true,
        )
        .expect("test should succeed");

        assert!(prefilter.line_may_match_bytes(b"prefix Pm ... Resume suffix\n"));
        assert!(!prefilter.line_may_match_bytes(b"prefix Resume only\n"));
    }

    #[test]
    fn literal_prefilter_is_disabled_when_query_falls_back_to_scan() {
        assert!(LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: Vec::new()
                }],
                fallback_to_scan: true,
                pure_literal_alternation: None,
            },
            false,
        )
        .is_none());
    }
}
