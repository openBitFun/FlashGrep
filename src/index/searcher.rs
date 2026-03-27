use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::scan_repository,
    index::format::{
        read_docs_file, IndexBuildSettings, IndexLayout, IndexMetadata, LookupTable, PostingsData,
    },
    json_output::{JsonData, JsonDuration, JsonSearchFile, JsonSearchReport, JsonStats},
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
#[path = "searcher_rg.rs"]
mod rg;
#[path = "searcher_shared.rs"]
mod shared;
#[path = "searcher_verify.rs"]
pub(crate) mod verify;

use count::{DocMatchCount, VerifyPlan as CountVerifyPlan};
use multi_literal::{
    collect_candidates as collect_multi_literal_candidates, prepare as prepare_multi_literal,
    search as search_multi_literal,
};
use query::{fold_query_literal, LiteralPrefilter, SearchPreparation};
use shared::doc_by_id;
use verify::{
    build_matcher, requires_multiline_verification, verify_candidates, verify_candidates_json,
    SearchProfile,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexWorktreeDiff {
    pub modified_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub new_files: Vec<String>,
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

pub struct IndexSearcher {
    index_path: PathBuf,
    metadata: IndexMetadata,
    docs: Vec<crate::index::format::DocMeta>,
    lookup: LookupTable,
    postings: PostingsData,
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
        let (metadata, docs) = read_docs_file(&layout.docs_path)?;
        let lookup = LookupTable::open(&layout.lookup_path)?;
        let postings = PostingsData::open(&layout.postings_path)?;

        Ok(Self {
            index_path,
            metadata: metadata.clone(),
            docs,
            lookup,
            postings,
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

    pub fn search_json(&self, config: &QueryConfig) -> Result<JsonSearchReport> {
        self.search_json_with_filter(config, None)
    }

    pub fn build_settings(&self) -> Option<&IndexBuildSettings> {
        self.metadata.build.as_ref()
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
            .map(|doc| doc.path.as_str())
            .collect::<HashSet<_>>();

        let mut modified_files = Vec::new();
        let mut deleted_files = Vec::new();
        for doc in &self.docs {
            match current.get(&doc.path) {
                Some(&(size, mtime_nanos))
                    if size == doc.size && mtime_nanos == doc.mtime_nanos => {}
                Some(_) => modified_files.push(doc.path.clone()),
                None => deleted_files.push(doc.path.clone()),
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

    pub fn search_json_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<JsonSearchReport> {
        self.search_json_with_allowed_paths(config, filter, None)
    }

    pub(crate) fn search_json_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<JsonSearchReport> {
        let total_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        let preparation = self.prepare_indexed_search(config, &plan, &SearchProfile::disabled())?;
        let candidate_ids =
            self.filter_candidate_ids(&preparation.candidate_ids, filter, allowed_paths)?;
        let multiline_verifier = requires_multiline_verification(config)?;
        let matcher = build_matcher(config, multiline_verifier)?;
        let outcomes = verify_candidates_json(
            &candidate_ids,
            &self.docs,
            &matcher,
            multiline_verifier,
            config.search_mode,
            config.max_count,
            config.before_context,
            config.after_context,
        )?;

        let mut files = Vec::new();
        let mut searches_with_match = 0u64;
        let mut bytes_searched = 0u64;
        let mut matched_lines = 0u64;
        let mut matches = 0u64;

        for task in outcomes {
            let doc = doc_by_id(&self.docs, task.doc_id)?;
            bytes_searched += task.outcome.bytes_searched;
            if task.outcome.matched_occurrences == 0 {
                continue;
            }

            searches_with_match += 1;
            matched_lines += task.outcome.matched_lines as u64;
            matches += task.outcome.matched_occurrences as u64;
            let path = JsonData::from_string(doc.path.clone());
            let messages = task
                .outcome
                .messages
                .into_iter()
                .map(|mut message| {
                    match &mut message {
                        crate::json_output::JsonMessage::Match(matched) => {
                            matched.path = Some(path.clone());
                        }
                        crate::json_output::JsonMessage::Context(context) => {
                            context.path = Some(path.clone());
                        }
                        _ => {}
                    }
                    message
                })
                .collect::<Vec<_>>();

            files.push(JsonSearchFile {
                path,
                messages,
                binary_offset: task.outcome.binary_offset,
                stats: JsonStats {
                    elapsed: JsonDuration::from_duration(task.elapsed),
                    searches: 1,
                    searches_with_match: 1,
                    bytes_searched: task.outcome.bytes_searched,
                    bytes_printed: 0,
                    matched_lines: task.outcome.matched_lines as u64,
                    matches: task.outcome.matched_occurrences as u64,
                },
            });
        }

        Ok(JsonSearchReport {
            files,
            summary: JsonStats {
                elapsed: JsonDuration::from_duration(total_started.elapsed()),
                searches: candidate_ids.len() as u64,
                searches_with_match,
                bytes_searched,
                bytes_printed: 0,
                matched_lines,
                matches,
            },
            elapsed_total: JsonDuration::from_duration(total_started.elapsed()),
        })
    }

    pub fn search_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<SearchResults> {
        self.search_with_allowed_paths(config, filter, None)
    }

    pub(crate) fn search_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<SearchResults> {
        let profile = SearchProfile::enabled();
        let total_started = Instant::now();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());
        let use_rg_verify = rg::enabled(config);

        if !config.has_context()
            && !matches!(config.search_mode, SearchMode::CountMatches)
            && config.max_count.is_none()
            && !config.fixed_strings
            && !config.word_regexp
            && !config.line_regexp
        {
            if let Some(multi_literal) =
                self.prepare_multi_literal_search(&plan, config.case_insensitive, &profile)?
            {
                let candidate_ids =
                    self.filter_candidate_ids(&multi_literal.candidate_ids, filter, allowed_paths)?;
                if use_rg_verify {
                    let match_started = Instant::now();
                    let matched_lines = rg::count_matches(config, &self.docs, &candidate_ids)?;
                    profile.record_verify(match_started.elapsed());
                    profile.finish(total_started.elapsed(), candidate_ids.len(), matched_lines);
                    return Ok(SearchResults {
                        candidate_docs: candidate_ids.len(),
                        searches_with_match: usize::from(matched_lines > 0),
                        bytes_searched: self.sum_doc_sizes(&candidate_ids)?,
                        matched_lines,
                        matched_occurrences: 0,
                        hits: Vec::new(),
                    });
                }
                let match_started = Instant::now();
                let results = if matches!(config.search_mode, SearchMode::CountOnly) {
                    let regex_started = Instant::now();
                    let verify_plan = CountVerifyPlan::compile_searcher_line(config, None)?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let stats =
                        verify_plan.verify_candidate_count_stats(&candidate_ids, &self.docs)?;
                    SearchResults {
                        candidate_docs: candidate_ids.len(),
                        searches_with_match: stats.matched_docs,
                        bytes_searched: self.sum_doc_sizes(&candidate_ids)?,
                        matched_lines: stats.matched_lines,
                        matched_occurrences: 0,
                        hits: Vec::new(),
                    }
                } else {
                    search_multi_literal(
                        &self.docs,
                        Some(&candidate_ids),
                        &multi_literal.prepared,
                        config.search_mode,
                    )?
                };
                profile.record_verify(match_started.elapsed());
                profile.finish(
                    total_started.elapsed(),
                    results.candidate_docs,
                    results.matched_lines,
                );
                return Ok(results);
            }
        }

        let preparation = self.prepare_indexed_search(config, &plan, &profile)?;
        let candidate_ids =
            self.filter_candidate_ids(&preparation.candidate_ids, filter, allowed_paths)?;
        let candidate_doc_count = candidate_ids.len();

        if use_rg_verify {
            let match_started = Instant::now();
            let matched_lines = rg::count_matches(config, &self.docs, &candidate_ids)?;
            profile.record_verify(match_started.elapsed());
            profile.finish(total_started.elapsed(), candidate_doc_count, matched_lines);
            return Ok(SearchResults {
                candidate_docs: candidate_doc_count,
                searches_with_match: usize::from(matched_lines > 0),
                bytes_searched: self.sum_doc_sizes(&candidate_ids)?,
                matched_lines,
                matched_occurrences: 0,
                hits: Vec::new(),
            });
        }

        let multiline_verifier = requires_multiline_verification(config)?;
        let match_started = Instant::now();
        let (matched_lines, matched_occurrences, searches_with_match, hits, bytes_searched) =
            match config.search_mode {
                SearchMode::CountOnly => {
                    let regex_started = Instant::now();
                    let verify_plan = CountVerifyPlan::compile(
                        config,
                        multiline_verifier,
                        preparation.literal_prefilter.clone(),
                    )?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let stats =
                        verify_plan.verify_candidate_count_stats(&candidate_ids, &self.docs)?;
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
                    let matcher = build_matcher(config, multiline_verifier)?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let mut matched_lines = 0usize;
                    let mut matched_occurrences = 0usize;
                    let mut searches_with_match = 0usize;
                    let mut bytes_searched = 0u64;
                    let outcomes = verify_candidates(
                        &candidate_ids,
                        &self.docs,
                        &matcher,
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
                        if task.outcome.matched_occurrences > 0 {
                            searches_with_match += 1;
                        }
                    }
                    (
                        matched_lines,
                        matched_occurrences,
                        searches_with_match,
                        Vec::new(),
                        bytes_searched,
                    )
                }
                _ => {
                    let regex_started = Instant::now();
                    let matcher = build_matcher(config, multiline_verifier)?;
                    profile.record_regex_compile(regex_started.elapsed());
                    let mut hits = Vec::new();
                    let mut matched_lines = 0usize;
                    let mut matched_occurrences = 0usize;
                    let mut searches_with_match = 0usize;
                    let mut bytes_searched = 0u64;
                    let outcomes = verify_candidates(
                        &candidate_ids,
                        &self.docs,
                        &matcher,
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
                                path: doc.path.clone(),
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
        let profile = SearchProfile::enabled();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());

        let candidate_ids = if let Some(multi_literal) =
            self.prepare_multi_literal_search(&plan, config.case_insensitive, &profile)?
        {
            collect_multi_literal_candidates(
                &self.docs,
                Some(&multi_literal.candidate_ids),
                &multi_literal.prepared,
            )?
        } else {
            self.prepare_indexed_search(config, &plan, &profile)?
                .candidate_ids
        };
        let candidate_ids = self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)?;

        candidate_ids
            .into_iter()
            .map(|doc_id| Ok(doc_by_id(&self.docs, doc_id)?.path.clone()))
            .collect()
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

    pub(crate) fn count_matches_by_file_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileCount>> {
        self.count_matches_by_file_internal(config, filter, allowed_paths, false)
    }

    pub(crate) fn count_matches_by_file_including_zero_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileCount>> {
        self.count_matches_by_file_internal(config, filter, allowed_paths, true)
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

        let candidate_ids = if let Some(multi_literal) =
            self.prepare_multi_literal_search(&plan, config.case_insensitive, &profile)?
        {
            multi_literal.candidate_ids
        } else {
            self.prepare_indexed_search(config, &plan, &profile)?
                .candidate_ids
        };
        let candidate_ids = self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)?;
        let multiline_verifier = requires_multiline_verification(config)?;
        let verify_plan = CountVerifyPlan::compile(config, multiline_verifier, None)?;
        let counts = verify_plan.verify_candidate_counts_by_doc(&candidate_ids, &self.docs)?;
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

    pub(crate) fn count_total_matches_by_file_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileMatchCount>> {
        self.count_total_matches_by_file_internal(config, filter, allowed_paths, false)
    }

    pub(crate) fn count_total_matches_by_file_including_zero_with_allowed_paths(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<FileMatchCount>> {
        self.count_total_matches_by_file_internal(config, filter, allowed_paths, true)
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

        let candidate_ids = self
            .prepare_indexed_search(config, &plan, &profile)?
            .candidate_ids;
        let candidate_ids = self.filter_candidate_ids(&candidate_ids, filter, allowed_paths)?;
        let multiline_verifier = requires_multiline_verification(config)?;
        let regex_started = Instant::now();
        let matcher = build_matcher(config, multiline_verifier)?;
        profile.record_regex_compile(regex_started.elapsed());
        let match_started = Instant::now();
        let outcomes = verify_candidates(
            &candidate_ids,
            &self.docs,
            &matcher,
            multiline_verifier,
            SearchMode::CountMatches,
            config.max_count,
            0,
            0,
        )?;
        profile.record_verify(match_started.elapsed());

        let counts = outcomes
            .into_iter()
            .map(|task| {
                Ok(FileMatchCount {
                    path: doc_by_id(&self.docs, task.doc_id)?.path.clone(),
                    matched_occurrences: task.outcome.matched_occurrences,
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
            .filter(|doc| self.doc_matches_filters(doc, filter, None))
            .fold((0usize, 0u64), |(count, bytes), doc| {
                (count + 1, bytes + doc.size)
            })
    }

    pub(crate) fn indexed_paths_with_allowed_paths(
        &self,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Vec<String> {
        self.docs
            .iter()
            .filter(|doc| self.doc_matches_filters(doc, filter, allowed_paths))
            .map(|doc| doc.path.clone())
            .collect()
    }

    fn prepare_indexed_search(
        &self,
        config: &QueryConfig,
        plan: &crate::planner::QueryPlan,
        profile: &SearchProfile,
    ) -> Result<SearchPreparation> {
        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let candidate_started = Instant::now();
        let candidate_ids = if plan.fallback_to_scan {
            self.all_doc_ids()
        } else {
            self.collect_candidates(&plan.branches, tokenizer.as_ref(), config.top_k_tokens)?
        };
        profile.record_candidates(candidate_started.elapsed());
        Ok(SearchPreparation {
            candidate_ids,
            literal_prefilter: LiteralPrefilter::new(plan, config.case_insensitive),
        })
    }

    fn prepare_multi_literal_search(
        &self,
        plan: &crate::planner::QueryPlan,
        case_insensitive: bool,
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
        let prefilter_ids =
            self.collect_multi_literal_prefilter_candidates(&prepared, tokenizer.as_ref())?;
        profile.record_candidates(candidate_started.elapsed());

        Ok(Some(MultiLiteralPreparation {
            prepared,
            candidate_ids: prefilter_ids.unwrap_or_else(|| self.all_doc_ids()),
        }))
    }

    fn filter_candidate_ids(
        &self,
        candidate_ids: &[u32],
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Result<Vec<u32>> {
        if filter.is_none() && allowed_paths.is_none() {
            return Ok(candidate_ids.to_vec());
        }

        Ok(candidate_ids
            .iter()
            .copied()
            .filter(|&doc_id| {
                let Ok(doc) = doc_by_id(&self.docs, doc_id) else {
                    return false;
                };
                self.doc_matches_filters(doc, filter, allowed_paths)
            })
            .collect::<Vec<_>>())
    }

    fn file_counts_from_doc_counts(&self, counts: Vec<DocMatchCount>) -> Result<Vec<FileCount>> {
        counts
            .into_iter()
            .map(|count| {
                Ok(FileCount {
                    path: doc_by_id(&self.docs, count.doc_id)?.path.clone(),
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
        doc: &crate::index::format::DocMeta,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> bool {
        if filter.is_some_and(|filter| !filter.matches_file(Path::new(&doc.path))) {
            return false;
        }
        allowed_paths.is_none_or(|allowed_paths| allowed_paths.contains(&doc.path))
    }

    fn sum_doc_sizes(&self, candidate_ids: &[u32]) -> Result<u64> {
        let mut total = 0u64;
        for &doc_id in candidate_ids {
            total += doc_by_id(&self.docs, doc_id)?.size;
        }
        Ok(total)
    }

    fn collect_multi_literal_prefilter_candidates(
        &self,
        prepared: &multi_literal::PreparedMultiLiteral,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
    ) -> Result<Option<Vec<u32>>> {
        let mut posting_cache = HashMap::new();
        let mut any_available = false;
        let mut union = Vec::new();

        for literal in &prepared.literals {
            let query_literal = fold_query_literal(literal);
            let mut covering_hashes = Vec::new();
            tokenizer.collect_query_token_hashes(&query_literal, &mut covering_hashes);
            match self.collect_literal_candidates(tokenizer, &query_literal, covering_hashes) {
                LiteralCandidates::Impossible => {}
                LiteralCandidates::Unavailable => return Ok(None),
                LiteralCandidates::Available(candidates) => {
                    let Some(candidate) = candidates.first() else {
                        continue;
                    };
                    let docs = self.read_posting_docs(candidate.token_hash, &mut posting_cache)?;
                    union = union_sorted(&union, &docs);
                    any_available = true;
                }
            }
        }

        if any_available {
            Ok(Some(union))
        } else {
            Ok(Some(Vec::new()))
        }
    }

    fn collect_candidates(
        &self,
        branches: &[QueryBranch],
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
    ) -> Result<Vec<u32>> {
        let mut all_candidates = Vec::new();
        let mut posting_cache = HashMap::new();
        let mut literal_scan_cache = HashMap::new();

        for branch in branches {
            let Some(branch_candidates) = self.select_branch_candidates(
                branch,
                tokenizer,
                top_k,
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
        posting_cache: &mut HashMap<u64, Vec<u32>>,
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
                LiteralCandidates::Unavailable => scanned_literals.push(query_literal),
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
                None => docs,
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
            let mut best_choice: Option<(TokenCandidate, Vec<u32>)> = None;

            for candidates in &per_literal {
                for candidate in candidates {
                    if selected_hashes.contains(&candidate.token_hash) {
                        continue;
                    }

                    let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
                    let reduced = intersect_sorted(current_candidates, &docs);
                    let replace = match &best_choice {
                        Some((best_candidate, best_reduced)) => {
                            reduced.len() < best_reduced.len()
                                || (reduced.len() == best_reduced.len()
                                    && candidate.selection_rank() < best_candidate.selection_rank())
                        }
                        None => true,
                    };
                    if replace {
                        best_choice = Some((*candidate, reduced));
                    }
                }
            }

            let Some((candidate, reduced)) = best_choice else {
                break;
            };
            if reduced.len() >= current_candidates.len() {
                break;
            }

            selected_hashes.insert(candidate.token_hash);
            branch_candidates = Some(reduced);
            selected_count += 1;
        }

        for literal in scanned_literals {
            let docs = self.scan_literal_docs(&literal, literal_scan_cache);
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

        let mut fallback_hashes = Vec::new();
        tokenizer.collect_document_token_hashes(query_literal, &mut fallback_hashes);
        fallback_hashes.sort_unstable();
        fallback_hashes.dedup();

        let mut selected = Vec::new();
        let mut seen_hashes = HashSet::new();

        for token_hash in covering_hashes {
            if !seen_hashes.insert(token_hash) {
                continue;
            }
            let Some(entry) = self.lookup.find(token_hash) else {
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

        if selected.len() < 2 {
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
        posting_cache: &mut HashMap<u64, Vec<u32>>,
    ) -> Result<Vec<u32>> {
        if let Some(cached) = posting_cache.get(&token_hash) {
            return Ok(cached.clone());
        }

        let docs = match self.lookup.find(token_hash) {
            Some(entry) if entry.offset != u64::MAX => self.postings.decode(entry)?,
            Some(_) | None => Vec::new(),
        };
        posting_cache.insert(token_hash, docs.clone());
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

    fn scan_literal_docs(
        &self,
        literal: &str,
        literal_scan_cache: &mut HashMap<String, Vec<u32>>,
    ) -> Vec<u32> {
        if let Some(cached) = literal_scan_cache.get(literal) {
            return cached.clone();
        }

        let mut docs = Vec::new();
        for doc in &self.docs {
            let path = std::path::PathBuf::from(&doc.path);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let folded = if text.is_ascii() {
                text.to_ascii_lowercase()
            } else {
                text.to_lowercase()
            };
            if folded.contains(literal) {
                docs.push(doc.doc_id);
            }
        }

        literal_scan_cache.insert(literal.to_string(), docs.clone());
        docs
    }

    fn all_doc_ids(&self) -> Vec<u32> {
        self.docs.iter().map(|doc| doc.doc_id).collect()
    }
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
        intersect_sorted, lower_bound, prune_high_frequency_candidates, union_sorted,
        CandidateSource, LiteralPrefilter, TokenCandidate,
    };
    use crate::planner::{QueryBranch, QueryPlan};

    #[test]
    fn galloping_intersection_matches_linear_result() {
        let small = vec![3, 50, 120, 9_999];
        let large = (0..20_000).step_by(3).collect::<Vec<u32>>();
        assert_eq!(intersect_sorted(&small, &large), vec![3, 120, 9_999]);
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
