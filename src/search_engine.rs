use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet},
    fs::File,
    io::ErrorKind,
    path::Path,
    sync::Arc,
};

use aho_corasick::{AhoCorasick, MatchKind};
use memmap2::Mmap;
use rayon::prelude::*;

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::{is_text_file, scan_paths, RepositoryFile, ScanOptions},
    index::{
        build_index_with_options,
        format::DocMeta,
        rebuild_index_with_options,
        searcher::{
            verify::{
                build_matcher, requires_multiline_verification,
                verify_candidate_sources_with_resolver, VerifyCandidateSource,
            },
            CountVerifyKind, CountVerifyPlan, IndexSearcher, IndexWorktreeDiff, LinePrefilter,
        },
        IndexBuildOptions,
    },
    path_filter::PathFilter,
    planner::plan,
    query_preflight::{evaluate_index_query_preflight, preflight_enabled},
    search::{FileMatch, MatchLocation, SearchHit, SearchLine, SearchResults},
    tokenizer::{create, TokenizerOptions},
};

const PARALLEL_DOC_THRESHOLD: usize = 16;
const PARALLEL_BYTE_THRESHOLD: u64 = 8 * 1024 * 1024;

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

#[derive(Debug, Clone)]
pub(crate) struct SearchDocument {
    pub(crate) logical_path: String,
    pub(crate) size: u64,
    pub(crate) mtime_nanos: u64,
    pub(crate) source: SearchDocumentSource,
}

#[derive(Debug, Clone)]
pub(crate) enum SearchDocumentSource {
    Path(std::path::PathBuf),
    LoadedBytes(Arc<[u8]>),
}

impl SearchDocument {
    pub(crate) fn from_repository_file(file: &RepositoryFile) -> Self {
        Self {
            logical_path: file.path.to_string_lossy().into_owned(),
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            source: SearchDocumentSource::Path(file.path.clone()),
        }
    }

    pub(crate) fn from_loaded_bytes(
        logical_path: String,
        size: u64,
        mtime_nanos: u64,
        bytes: Arc<[u8]>,
    ) -> Self {
        Self {
            logical_path,
            size,
            mtime_nanos,
            source: SearchDocumentSource::LoadedBytes(bytes),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SearchDocumentIndex {
    documents: Vec<Option<SearchDocument>>,
    folded_text: Vec<Option<Arc<str>>>,
    token_hashes: Vec<Vec<u64>>,
    postings: HashMap<u64, Vec<u32>>,
    path_to_doc_id: HashMap<String, u32>,
    unindexed_doc_ids: BTreeSet<u32>,
    free_doc_ids: Vec<u32>,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
}

impl SearchDocumentIndex {
    pub(crate) fn build(
        tokenizer_mode: crate::config::TokenizerMode,
        tokenizer_options: TokenizerOptions,
        documents: Vec<SearchDocument>,
    ) -> Self {
        let mut index = Self {
            documents: Vec::new(),
            folded_text: Vec::new(),
            token_hashes: Vec::new(),
            postings: HashMap::new(),
            path_to_doc_id: HashMap::new(),
            unindexed_doc_ids: BTreeSet::new(),
            free_doc_ids: Vec::new(),
            tokenizer_mode,
            tokenizer_options,
        };

        for document in documents {
            index.upsert_document(document);
        }

        index
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.path_to_doc_id.is_empty()
    }

    pub(crate) fn upsert_document(&mut self, document: SearchDocument) {
        let doc_id = self
            .path_to_doc_id
            .get(&document.logical_path)
            .copied()
            .unwrap_or_else(|| self.allocate_doc_id());
        self.remove_doc_contents(doc_id);
        self.path_to_doc_id.insert(document.logical_path.clone(), doc_id);

        let (folded_text, token_hashes) = self.index_document_contents(&document);
        let slot = usize::try_from(doc_id).expect("doc id should fit usize");
        self.documents[slot] = Some(document);
        self.folded_text[slot] = folded_text;
        self.token_hashes[slot] = token_hashes.clone();

        if self.folded_text[slot].is_some() {
            self.unindexed_doc_ids.remove(&doc_id);
            for token_hash in token_hashes {
                insert_sorted_unique(self.postings.entry(token_hash).or_default(), doc_id);
            }
        } else {
            self.unindexed_doc_ids.insert(doc_id);
        }
    }

    pub(crate) fn remove_document(&mut self, path: &str) {
        let Some(doc_id) = self.path_to_doc_id.remove(path) else {
            return;
        };
        self.remove_doc_contents(doc_id);
        self.free_doc_ids.push(doc_id);
    }

    fn search(&self, config: &QueryConfig, filter: Option<&PathFilter>) -> Result<SearchResults> {
        let candidate_ids = self.candidate_doc_ids(config, filter)?;
        let documents = self.documents_for_ids(&candidate_ids);
        search_documents(config, &documents)
    }

    fn candidate_doc_ids(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
    ) -> Result<Vec<u32>> {
        let plan = plan(&config.regex_pattern)?;
        if plan.fallback_to_scan {
            return Ok(self.filtered_doc_ids(filter));
        }

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let mut all_candidates = Vec::new();
        for branch in &plan.branches {
            let Some(branch_candidates) =
                self.select_branch_candidates(branch, tokenizer.as_ref())?
            else {
                return Ok(self.filtered_doc_ids(filter));
            };
            all_candidates = union_sorted_u32(&all_candidates, &branch_candidates);
        }
        let unindexed = self.unindexed_doc_ids.iter().copied().collect::<Vec<_>>();
        all_candidates = union_sorted_u32(&all_candidates, &unindexed);
        Ok(self.filter_candidate_ids(&all_candidates, filter))
    }

    fn select_branch_candidates(
        &self,
        branch: &crate::planner::QueryBranch,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
    ) -> Result<Option<Vec<u32>>> {
        let mut branch_candidates: Option<Vec<u32>> = None;

        for literal in &branch.literals {
            let query_literal = fold_for_search_index(literal);
            let literal_candidates =
                self.literal_candidate_doc_ids(tokenizer, &query_literal)?;
            branch_candidates = Some(match branch_candidates.take() {
                Some(existing) => intersect_sorted_u32(&existing, &literal_candidates),
                None => literal_candidates,
            });
            if branch_candidates.as_ref().is_some_and(Vec::is_empty) {
                return Ok(Some(Vec::new()));
            }
        }

        Ok(branch_candidates)
    }

    fn literal_candidate_doc_ids(
        &self,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        query_literal: &str,
    ) -> Result<Vec<u32>> {
        let mut covering_hashes = Vec::new();
        tokenizer.collect_query_token_hashes(query_literal, &mut covering_hashes);
        covering_hashes.sort_unstable();
        covering_hashes.dedup();

        let tolerate_missing_covering_hashes = matches!(
            self.tokenizer_mode,
            crate::config::TokenizerMode::SparseNgram
        );
        let mut selected_hashes = Vec::new();
        let mut seen_hashes = HashSet::new();

        for token_hash in covering_hashes {
            if !seen_hashes.insert(token_hash) {
                continue;
            }
            if self.postings.contains_key(&token_hash) {
                selected_hashes.push(token_hash);
            } else if !tolerate_missing_covering_hashes {
                return Ok(Vec::new());
            }
        }

        if matches!(self.tokenizer_mode, crate::config::TokenizerMode::Trigram)
            && selected_hashes.len() < 2
        {
            let mut fallback_hashes = Vec::new();
            tokenizer.collect_document_token_hashes(query_literal, &mut fallback_hashes);
            fallback_hashes.sort_unstable();
            fallback_hashes.dedup();
            for token_hash in fallback_hashes {
                if seen_hashes.insert(token_hash) && self.postings.contains_key(&token_hash) {
                    selected_hashes.push(token_hash);
                }
            }
        }

        if selected_hashes.is_empty() {
            return Ok(self.scan_literal_doc_ids(query_literal, None));
        }

        selected_hashes.sort_unstable_by_key(|token_hash| {
            self.postings
                .get(token_hash)
                .map_or(usize::MAX, std::vec::Vec::len)
        });

        let mut candidates: Option<Vec<u32>> = None;
        for token_hash in selected_hashes {
            let docs = self
                .postings
                .get(&token_hash)
                .cloned()
                .unwrap_or_default();
            candidates = Some(match candidates.take() {
                Some(existing) => intersect_sorted_u32(&existing, &docs),
                None => docs,
            });
            if candidates.as_ref().is_some_and(Vec::is_empty) {
                return Ok(Vec::new());
            }
        }

        let candidates = candidates.unwrap_or_default();
        Ok(self.scan_literal_doc_ids(query_literal, Some(&candidates)))
    }

    fn scan_literal_doc_ids(&self, literal: &str, candidate_ids: Option<&[u32]>) -> Vec<u32> {
        let iter: Box<dyn Iterator<Item = u32>> = match candidate_ids {
            Some(ids) => Box::new(ids.iter().copied()),
            None => Box::new(0..u32::try_from(self.documents.len()).unwrap_or_default()),
        };

        iter.filter(|doc_id| self.document_contains_literal(*doc_id, literal))
            .collect()
    }

    fn document_contains_literal(&self, doc_id: u32, literal: &str) -> bool {
        let index = match usize::try_from(doc_id) {
            Ok(value) => value,
            Err(_) => return false,
        };
        self.folded_text
            .get(index)
            .and_then(|text| text.as_deref())
            .is_some_and(|text| text.contains(literal))
    }

    fn filtered_doc_ids(&self, filter: Option<&PathFilter>) -> Vec<u32> {
        (0..self.documents.len())
            .filter_map(|index| {
                self.documents.get(index)?.as_ref()?;
                let doc_id = u32::try_from(index).ok()?;
                self.doc_matches_filter(doc_id, filter).then_some(doc_id)
            })
            .collect()
    }

    fn filter_candidate_ids(&self, candidate_ids: &[u32], filter: Option<&PathFilter>) -> Vec<u32> {
        if filter.is_none() {
            return candidate_ids.to_vec();
        }
        candidate_ids
            .iter()
            .copied()
            .filter(|doc_id| self.doc_matches_filter(*doc_id, filter))
            .collect()
    }

    fn doc_matches_filter(&self, doc_id: u32, filter: Option<&PathFilter>) -> bool {
        let Some(filter) = filter else {
            return true;
        };
        let index = match usize::try_from(doc_id) {
            Ok(value) => value,
            Err(_) => return false,
        };
        self.documents
            .get(index)
            .and_then(|document| document.as_ref())
            .is_some_and(|document| filter.matches_file(Path::new(&document.logical_path)))
    }

    fn allocate_doc_id(&mut self) -> u32 {
        if let Some(doc_id) = self.free_doc_ids.pop() {
            return doc_id;
        }

        let doc_id = u32::try_from(self.documents.len()).expect("document index exceeds u32 range");
        self.documents.push(None);
        self.folded_text.push(None);
        self.token_hashes.push(Vec::new());
        doc_id
    }

    fn index_document_contents(
        &self,
        document: &SearchDocument,
    ) -> (Option<Arc<str>>, Vec<u64>) {
        let Some(text) = folded_document_text(document) else {
            return (None, Vec::new());
        };

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let mut token_hashes = Vec::new();
        tokenizer.collect_document_token_hashes(&text, &mut token_hashes);
        token_hashes.sort_unstable();
        token_hashes.dedup();
        (Some(Arc::<str>::from(text)), token_hashes)
    }

    fn remove_doc_contents(&mut self, doc_id: u32) {
        let slot = match usize::try_from(doc_id) {
            Ok(value) => value,
            Err(_) => return,
        };
        if slot >= self.documents.len() {
            return;
        }

        for token_hash in self.token_hashes[slot].drain(..) {
            let remove_entry = if let Some(postings) = self.postings.get_mut(&token_hash) {
                remove_sorted(postings, doc_id);
                postings.is_empty()
            } else {
                false
            };
            if remove_entry {
                self.postings.remove(&token_hash);
            }
        }
        self.unindexed_doc_ids.remove(&doc_id);
        self.documents[slot] = None;
        self.folded_text[slot] = None;
    }

    fn documents_for_ids(&self, candidate_ids: &[u32]) -> Vec<SearchDocument> {
        candidate_ids
            .iter()
            .filter_map(|doc_id| usize::try_from(*doc_id).ok())
            .filter_map(|index| self.documents.get(index))
            .filter_map(|document| document.clone())
            .collect()
    }
}

/// Product-facing search facade.
///
/// Prefer this type for long-lived service/IDE/agent integrations.
/// For one-off scripts, crate-level `build_index*`/`rebuild_index*` helpers are
/// also available.
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

    pub fn build_index(&self) -> Result<usize> {
        self.build_index_with_options(IndexBuildOptions::default())
    }

    pub fn build_index_with_options(&self, options: IndexBuildOptions<'_>) -> Result<usize> {
        build_index_with_options(&self.build_config, options)
    }

    pub fn rebuild_index(&self) -> Result<usize> {
        self.rebuild_index_with_options(IndexBuildOptions::default())
    }

    pub fn rebuild_index_with_options(&self, options: IndexBuildOptions<'_>) -> Result<usize> {
        rebuild_index_with_options(&self.build_config, options)
    }

    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        self.open_searcher()?.diff_against_worktree()
    }

    /// Returns a simplified human-readable stale reason, if any.
    ///
    /// For structured freshness state, prefer `WorkspaceIndex::probe_freshness`.
    pub fn stale_reason(&self) -> Result<Option<String>> {
        self.open_searcher()?.stale_reason()
    }

    pub fn search(&self, config: &QueryConfig, policy: SearchPolicy) -> Result<SearchResponse> {
        self.search_with_filter(config, None, policy)
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
        match self.open_searcher() {
            Ok(searcher) => {
                let query_prefers_scan = if preflight_enabled() {
                    let preflight =
                        evaluate_index_query_preflight(&searcher, config, filter, None)?;
                    preflight.reason.requires_scan_backend()
                } else {
                    false
                };
                self.search_with_searcher(searcher, config, filter, policy, query_prefers_scan)
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                self.search_without_index(config, filter, policy)
            }
            Err(error) => Err(error),
        }
    }

    fn search_with_searcher(
        &self,
        searcher: IndexSearcher,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
        query_prefers_scan: bool,
    ) -> Result<SearchResponse> {
        if matches!(policy, SearchPolicy::FallbackToScan) && query_prefers_scan {
            let files = self.scan_repo_files(filter)?;
            return Ok(SearchResponse {
                backend: SearchBackend::Scan,
                results: search_scanned_files(config, &files)?,
            });
        }

        Ok(SearchResponse {
            backend: SearchBackend::Index,
            results: searcher.search_with_filter(config, filter)?,
        })
    }

    fn search_without_index(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
    ) -> Result<SearchResponse> {
        match policy {
            SearchPolicy::IndexOnly => Err(AppError::InvalidIndex(format!(
                "index does not exist at {}",
                self.build_config.index_path.display()
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

    fn open_searcher(&self) -> Result<IndexSearcher> {
        IndexSearcher::open(self.build_config.index_path.clone())
    }

    fn scan_repo_files(&self, filter: Option<&PathFilter>) -> Result<Vec<RepositoryFile>> {
        scan_text_files(scan_paths(
            std::slice::from_ref(&self.build_config.repo_path),
            Some(&self.build_config.index_path),
            scan_options(&self.build_config),
            filter,
        )?)
    }
}

pub(crate) fn search_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
) -> Result<SearchResults> {
    let documents = files
        .iter()
        .map(SearchDocument::from_repository_file)
        .collect::<Vec<_>>();
    search_documents(config, &documents)
}

pub(crate) fn search_documents(
    config: &QueryConfig,
    documents: &[SearchDocument],
) -> Result<SearchResults> {
    if let Some(results) = search_documents_multi_literal_fast(config, documents)? {
        return Ok(results);
    }

    let docs = repository_docs(documents)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let query_plan = plan(&config.regex_pattern)?;
    let line_prefilter = (!multiline_verifier)
        .then(|| LinePrefilter::compile(config, &query_plan))
        .flatten();
    match config.search_mode {
        crate::search::SearchMode::CountOnly | crate::search::SearchMode::CountMatches => {
            let verify_plan = CountVerifyPlan::compile(
                config,
                multiline_verifier,
                line_prefilter.clone(),
                if matches!(config.search_mode, crate::search::SearchMode::CountOnly) {
                    CountVerifyKind::Lines
                } else {
                    CountVerifyKind::Occurrences
                },
            )?;
            let resolve_source = |doc_id| scan_doc_source(documents, doc_id);
            let stats = verify_plan.verify_candidate_count_stats_with_sources(
                &candidate_ids,
                docs.len(),
                &resolve_source,
            )?;
            Ok(SearchResults {
                candidate_docs: docs.len(),
                searches_with_match: stats.matched_docs,
                bytes_searched: documents.iter().map(|document| document.size).sum(),
                matched_lines: stats.matched_lines,
                matched_occurrences: stats.matched_occurrences,
                hits: Vec::new(),
            })
        }
        _ => {
            let matcher = build_matcher(config, multiline_verifier)?;
            let resolve_source = |doc_id| scan_doc_source(documents, doc_id);
            let outcomes = verify_candidate_sources_with_resolver(
                &candidate_ids,
                docs.len(),
                &resolve_source,
                &matcher,
                line_prefilter.as_ref(),
                multiline_verifier,
                config.search_mode,
                config.max_count,
                config.before_context,
                config.after_context,
            )?;

            let mut hits = Vec::new();
            let mut matched_lines = 0usize;
            let mut matched_occurrences = 0usize;
            let mut searches_with_match = 0usize;
            let mut bytes_searched = 0u64;
            for task in outcomes {
                matched_lines += task.outcome.matched_lines;
                matched_occurrences += task.outcome.matched_occurrences;
                bytes_searched += task.outcome.bytes_searched;
                if !task.outcome.matches.is_empty() {
                    searches_with_match += 1;
                    let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
                        AppError::ValueOutOfRange(format!(
                            "doc id {} exceeds usize range",
                            task.doc_id
                        ))
                    })?];
                    hits.push(SearchHit {
                        path: doc.path.clone(),
                        matches: task.outcome.matches,
                        lines: task.outcome.lines,
                    });
                } else if task.outcome.matched_lines > 0 || task.outcome.matched_occurrences > 0 {
                    searches_with_match += 1;
                }
            }

            Ok(SearchResults {
                candidate_docs: docs.len(),
                searches_with_match,
                bytes_searched,
                matched_lines,
                matched_occurrences,
                hits,
            })
        }
    }
}

pub(crate) fn search_document_index(
    config: &QueryConfig,
    index: &SearchDocumentIndex,
    filter: Option<&PathFilter>,
) -> Result<SearchResults> {
    index.search(config, filter)
}

fn search_documents_multi_literal_fast(
    config: &QueryConfig,
    documents: &[SearchDocument],
) -> Result<Option<SearchResults>> {
    if config.has_context()
        || matches!(config.search_mode, crate::search::SearchMode::CountOnly)
        || matches!(config.search_mode, crate::search::SearchMode::CountMatches)
        || config.max_count.is_some()
        || config.fixed_strings
        || config.word_regexp
        || config.line_regexp
    {
        return Ok(None);
    }

    let plan = plan(&config.regex_pattern)?;
    let Some(alternation) = plan.pure_literal_alternation else {
        return Ok(None);
    };
    if config.case_insensitive
        && !alternation
            .literals
            .iter()
            .all(|literal| literal.is_ascii())
    {
        return Ok(None);
    }

    let matcher = AhoCorasick::builder()
        .match_kind(MatchKind::LeftmostFirst)
        .ascii_case_insensitive(config.case_insensitive)
        .build(&alternation.literals)
        .map_err(|error| AppError::InvalidPattern(error.to_string()))?;

    let outcomes = if should_parallel_scan_documents(documents) {
        documents
            .par_iter()
            .map(|document| scan_document_multi_literal(document, &matcher, config.search_mode))
            .collect::<Vec<_>>()
    } else {
        documents
            .iter()
            .map(|document| scan_document_multi_literal(document, &matcher, config.search_mode))
            .collect::<Vec<_>>()
    };

    let mut matched_lines = 0usize;
    let mut matched_occurrences = 0usize;
    let mut searches_with_match = 0usize;
    let mut hits = Vec::new();
    let mut bytes_searched = 0u64;

    for outcome in outcomes.into_iter().flatten() {
        matched_lines += outcome.matched_lines;
        matched_occurrences += outcome.matched_occurrences;
        bytes_searched += outcome.bytes_searched;
        if !outcome.matches.is_empty() {
            searches_with_match += 1;
            hits.push(SearchHit {
                path: outcome.path,
                matches: outcome.matches.clone(),
                lines: outcome.matches.into_iter().map(SearchLine::Match).collect(),
            });
        } else if outcome.matched_lines > 0 || outcome.matched_occurrences > 0 {
            searches_with_match += 1;
        }
    }

    Ok(Some(SearchResults {
        candidate_docs: documents.len(),
        searches_with_match,
        bytes_searched,
        matched_lines,
        matched_occurrences,
        hits,
    }))
}

#[derive(Debug)]
struct MultiLiteralDocumentOutcome {
    path: String,
    matched_lines: usize,
    matched_occurrences: usize,
    bytes_searched: u64,
    matches: Vec<FileMatch>,
}

fn should_parallel_scan_documents(documents: &[SearchDocument]) -> bool {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) <= 1 {
        return false;
    }

    documents.len() >= PARALLEL_DOC_THRESHOLD
        || documents.iter().map(|document| document.size).sum::<u64>() >= PARALLEL_BYTE_THRESHOLD
}

fn scan_document_multi_literal(
    document: &SearchDocument,
    matcher: &AhoCorasick,
    mode: crate::search::SearchMode,
) -> Option<MultiLiteralDocumentOutcome> {
    let bytes = document_bytes(document)?;
    let bytes_searched = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    match mode {
        crate::search::SearchMode::CountOnly => {
            let matched_lines = count_matched_lines(&bytes, matcher);
            if matched_lines == 0 {
                None
            } else {
                Some(MultiLiteralDocumentOutcome {
                    path: document.logical_path.clone(),
                    matched_lines,
                    matched_occurrences: 0,
                    bytes_searched,
                    matches: Vec::new(),
                })
            }
        }
        crate::search::SearchMode::FirstHitOnly | crate::search::SearchMode::MaterializeMatches => {
            let first_only = matches!(mode, crate::search::SearchMode::FirstHitOnly);
            let matches = collect_document_matches(&bytes, matcher, first_only);
            if matches.is_empty() {
                None
            } else {
                Some(MultiLiteralDocumentOutcome {
                    path: document.logical_path.clone(),
                    matched_lines: count_unique_match_lines(&matches),
                    matched_occurrences: matches.len(),
                    bytes_searched,
                    matches,
                })
            }
        }
        crate::search::SearchMode::CountMatches => None,
    }
}

fn document_bytes(document: &SearchDocument) -> Option<Cow<'_, [u8]>> {
    match &document.source {
        SearchDocumentSource::Path(path) => {
            if document.size == 0 {
                return None;
            }
            let file = File::open(path).ok()?;
            let mmap = unsafe { Mmap::map(&file).ok()? };
            Some(Cow::Owned(mmap.to_vec()))
        }
        SearchDocumentSource::LoadedBytes(bytes) => Some(Cow::Borrowed(bytes)),
    }
}

fn count_matched_lines(bytes: &[u8], matcher: &AhoCorasick) -> usize {
    let mut matched_lines = 0usize;
    let mut last_match_end = 0usize;

    for matched in matcher.find_iter(bytes) {
        if matched_lines == 0 || bytes[last_match_end..matched.start()].contains(&b'\n') {
            matched_lines += 1;
        }
        last_match_end = matched.end();
    }

    matched_lines
}

fn collect_document_matches(
    bytes: &[u8],
    matcher: &AhoCorasick,
    first_only: bool,
) -> Vec<FileMatch> {
    let line_starts = build_line_starts(bytes);
    let mut matches = Vec::new();

    for matched in matcher.find_iter(bytes) {
        matches.push(build_document_file_match(
            bytes,
            matched.start(),
            matched.end(),
            &line_starts,
        ));
        if first_only {
            break;
        }
    }

    matches
}

fn count_unique_match_lines(matches: &[FileMatch]) -> usize {
    use std::collections::HashSet;

    matches
        .iter()
        .map(|matched| matched.location.line)
        .collect::<HashSet<_>>()
        .len()
}

fn build_document_file_match(
    bytes: &[u8],
    start: usize,
    end: usize,
    line_starts: &[usize],
) -> FileMatch {
    let line_index = line_index_for_offset(start, line_starts);
    let line_start = line_starts[line_index];
    let line_end = line_starts
        .get(line_index + 1)
        .copied()
        .unwrap_or(bytes.len());
    let snippet = trim_line_terminator_bytes(&bytes[line_start..line_end]);

    FileMatch {
        location: MatchLocation {
            line: line_index + 1,
            column: start.saturating_sub(line_start) + 1,
        },
        snippet: String::from_utf8_lossy(snippet).into_owned(),
        matched_text: String::from_utf8_lossy(&bytes[start..end]).into_owned(),
    }
}

fn build_line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_index_for_offset(offset: usize, line_starts: &[usize]) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    }
}

fn trim_line_terminator_bytes(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn folded_document_text(document: &SearchDocument) -> Option<String> {
    match &document.source {
        SearchDocumentSource::Path(path) => std::fs::read_to_string(path)
            .ok()
            .map(|text| fold_for_search_index(&text)),
        SearchDocumentSource::LoadedBytes(bytes) => std::str::from_utf8(bytes)
            .ok()
            .map(fold_for_search_index),
    }
}

fn fold_for_search_index(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.to_lowercase()
    }
}

fn intersect_sorted_u32(left: &[u32], right: &[u32]) -> Vec<u32> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
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

fn union_sorted_u32(left: &[u32], right: &[u32]) -> Vec<u32> {
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

fn insert_sorted_unique(values: &mut Vec<u32>, needle: u32) {
    match values.binary_search(&needle) {
        Ok(_) => {}
        Err(index) => values.insert(index, needle),
    }
}

fn remove_sorted(values: &mut Vec<u32>, needle: u32) {
    if let Ok(index) = values.binary_search(&needle) {
        values.remove(index);
    }
}

fn repository_docs(documents: &[SearchDocument]) -> Result<Vec<DocMeta>> {
    documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let doc_id = u32::try_from(index).map_err(|_| {
                AppError::ValueOutOfRange(format!("scanned document id {index} exceeds u32 range"))
            })?;
            Ok(DocMeta {
                doc_id,
                path: document.logical_path.clone(),
                size: document.size,
                mtime_nanos: document.mtime_nanos,
            })
        })
        .collect()
}

fn scan_doc_source(documents: &[SearchDocument], doc_id: u32) -> Result<VerifyCandidateSource> {
    let index = usize::try_from(doc_id)
        .map_err(|_| AppError::ValueOutOfRange(format!("doc id {doc_id} exceeds usize range")))?;
    documents
        .get(index)
        .map(|document| match &document.source {
            SearchDocumentSource::Path(path) => VerifyCandidateSource::Path(path.clone()),
            SearchDocumentSource::LoadedBytes(bytes) => VerifyCandidateSource::Bytes(bytes.clone()),
        })
        .ok_or_else(|| {
            AppError::InvalidIndex(format!("doc id {doc_id} is missing from scan documents"))
        })
}

fn scan_options(config: &BuildConfig) -> ScanOptions {
    ScanOptions {
        respect_ignore: matches!(config.corpus_mode, crate::config::CorpusMode::RespectIgnore),
        include_hidden: config.include_hidden,
        max_file_size: config.max_file_size,
        max_depth: None,
        ignore_files: Vec::new(),
    }
}

fn scan_text_files(files: Vec<RepositoryFile>) -> Result<Vec<RepositoryFile>> {
    let mut text_files = Vec::with_capacity(files.len());
    for file in files {
        if is_text_file(Path::new(&file.path))? {
            text_files.push(file);
        }
    }
    Ok(text_files)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{search_document_index, search_documents, SearchDocument, SearchDocumentIndex};
    use crate::{
        config::{QueryConfig, TokenizerMode},
        search::SearchMode,
        tokenizer::TokenizerOptions,
    };

    fn materialize_fixed_query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: regex::escape(pattern),
            patterns: vec![pattern.to_string()],
            fixed_strings: true,
            search_mode: SearchMode::MaterializeMatches,
            ..QueryConfig::default()
        }
    }

    fn document_index(documents: Vec<SearchDocument>) -> SearchDocumentIndex {
        SearchDocumentIndex::build(TokenizerMode::Trigram, TokenizerOptions::default(), documents)
    }

    #[test]
    fn search_documents_matches_fixed_string_phrase_with_spaces_in_loaded_bytes() {
        let pattern = "VersionTuple watcher validation content gamma";
        let documents = vec![SearchDocument::from_loaded_bytes(
            "doc.txt".into(),
            pattern.len() as u64,
            0,
            Arc::<[u8]>::from(format!("prefix {pattern} suffix\n").into_bytes()),
        )];

        let results = search_documents(&materialize_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 1);
        assert_eq!(results.matched_occurrences, 1);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].matches.len(), 1);
        assert_eq!(results.hits[0].matches[0].matched_text, pattern);
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

        let results = search_documents(&materialize_fixed_query(pattern), &documents)
            .expect("search should succeed");

        assert_eq!(results.matched_lines, 1);
        assert_eq!(results.matched_occurrences, 1);
        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].matches[0].matched_text, pattern);
    }

    #[test]
    fn search_document_index_matches_fixed_string_phrase_with_spaces_in_loaded_bytes() {
        let pattern = "VersionTuple watcher validation content gamma";
        let documents = vec![
            SearchDocument::from_loaded_bytes(
                "miss.txt".into(),
                4,
                0,
                Arc::<[u8]>::from(b"miss\n".to_vec()),
            ),
            SearchDocument::from_loaded_bytes(
                "hit.txt".into(),
                pattern.len() as u64,
                0,
                Arc::<[u8]>::from(format!("prefix {pattern} suffix\n").into_bytes()),
            ),
        ];
        let index = document_index(documents.clone());

        let direct = search_documents(&materialize_fixed_query(pattern), &documents)
            .expect("direct search should succeed");
        let indexed = search_document_index(&materialize_fixed_query(pattern), &index, None)
            .expect("indexed search should succeed");

        assert_eq!(indexed.matched_lines, direct.matched_lines);
        assert_eq!(indexed.matched_occurrences, direct.matched_occurrences);
        assert_eq!(indexed.hits.len(), 1);
        assert_eq!(indexed.hits[0].path, "hit.txt");
        assert_eq!(indexed.candidate_docs, 1);
    }

    #[test]
    fn search_document_index_matches_fixed_single_token_in_loaded_bytes() {
        let pattern = "VERSIONTUPLE_WATCHER_VALIDATION_GAMMA_20260410";
        let documents = vec![
            SearchDocument::from_loaded_bytes(
                "miss.txt".into(),
                4,
                0,
                Arc::<[u8]>::from(b"miss\n".to_vec()),
            ),
            SearchDocument::from_loaded_bytes(
                "hit.txt".into(),
                pattern.len() as u64,
                0,
                Arc::<[u8]>::from(format!("prefix {pattern} suffix\n").into_bytes()),
            ),
        ];
        let index = document_index(documents.clone());

        let direct = search_documents(&materialize_fixed_query(pattern), &documents)
            .expect("direct search should succeed");
        let indexed = search_document_index(&materialize_fixed_query(pattern), &index, None)
            .expect("indexed search should succeed");

        assert_eq!(indexed.matched_lines, direct.matched_lines);
        assert_eq!(indexed.matched_occurrences, direct.matched_occurrences);
        assert_eq!(indexed.hits.len(), 1);
        assert_eq!(indexed.hits[0].path, "hit.txt");
        assert_eq!(indexed.candidate_docs, 1);
    }
}
