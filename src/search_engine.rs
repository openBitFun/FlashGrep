use std::{borrow::Cow, fs::File, io::ErrorKind, path::Path};

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
                build_matcher, requires_multiline_verification, verify_candidate_sources,
                verify_candidate_sources_json,
            },
            CountVerifyKind, CountVerifyPlan, IndexSearcher, IndexWorktreeDiff,
        },
        IndexBuildOptions,
    },
    json_output::{JsonDuration, JsonSearchFile, JsonSearchReport, JsonStats},
    path_filter::PathFilter,
    planner::plan,
    query_preflight::evaluate_index_query_preflight,
    search::{
        FileCount, FileMatch, FileMatchCount, MatchLocation, SearchHit, SearchLine, SearchResults,
    },
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
                let preflight = evaluate_index_query_preflight(&searcher, config, filter, None)?;
                self.search_with_searcher(
                    searcher,
                    config,
                    filter,
                    policy,
                    preflight.reason.requires_scan_backend(),
                )
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
    match config.search_mode {
        crate::search::SearchMode::CountOnly | crate::search::SearchMode::CountMatches => {
            let verify_plan = CountVerifyPlan::compile_without_prefilter(
                config,
                multiline_verifier,
                if matches!(config.search_mode, crate::search::SearchMode::CountOnly) {
                    CountVerifyKind::Lines
                } else {
                    CountVerifyKind::Occurrences
                },
            )?;
            let resolve_path = |doc_id| scan_doc_path(documents, doc_id);
            let stats = verify_plan.verify_candidate_count_stats(
                &candidate_ids,
                docs.len(),
                &resolve_path,
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
            let resolve_path = |doc_id| scan_doc_path(documents, doc_id);
            let outcomes = verify_candidate_sources(
                &candidate_ids,
                docs.len(),
                &resolve_path,
                &matcher,
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

fn search_documents_multi_literal_fast(
    config: &QueryConfig,
    documents: &[SearchDocument],
) -> Result<Option<SearchResults>> {
    if config.has_context()
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
        .match_kind(
            if matches!(config.search_mode, crate::search::SearchMode::CountOnly) {
                MatchKind::Standard
            } else {
                MatchKind::LeftmostFirst
            },
        )
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

pub(crate) fn count_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
    include_zero: bool,
) -> Result<Vec<FileCount>> {
    let documents = files
        .iter()
        .map(SearchDocument::from_repository_file)
        .collect::<Vec<_>>();
    let docs = repository_docs(&documents)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let verify_plan = CountVerifyPlan::compile_without_prefilter(
        config,
        multiline_verifier,
        CountVerifyKind::Lines,
    )?;
    let resolve_path = |doc_id| scan_doc_path(&documents, doc_id);
    let outcomes =
        verify_plan.verify_candidate_counts_by_doc(&candidate_ids, docs.len(), &resolve_path)?;
    let mut counts = Vec::with_capacity(outcomes.len());
    for task in outcomes {
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        counts.push(FileCount {
            path: doc.path.clone(),
            matched_lines: task.matched_lines,
        });
    }
    if include_zero {
        Ok(counts)
    } else {
        Ok(counts.into_iter().filter(FileCount::has_match).collect())
    }
}

pub(crate) fn count_total_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
    include_zero: bool,
) -> Result<Vec<FileMatchCount>> {
    let documents = files
        .iter()
        .map(SearchDocument::from_repository_file)
        .collect::<Vec<_>>();
    let docs = repository_docs(&documents)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let verify_plan = CountVerifyPlan::compile_without_prefilter(
        config,
        multiline_verifier,
        CountVerifyKind::Occurrences,
    )?;
    let resolve_path = |doc_id| scan_doc_path(&documents, doc_id);
    let outcomes =
        verify_plan.verify_candidate_counts_by_doc(&candidate_ids, docs.len(), &resolve_path)?;
    let mut counts = Vec::with_capacity(outcomes.len());
    for task in outcomes {
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        counts.push(FileMatchCount {
            path: doc.path.clone(),
            matched_occurrences: task.matched_occurrences,
        });
    }
    if include_zero {
        Ok(counts)
    } else {
        Ok(counts
            .into_iter()
            .filter(FileMatchCount::has_match)
            .collect())
    }
}

pub(crate) fn search_json_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
) -> Result<JsonSearchReport> {
    let documents = files
        .iter()
        .map(SearchDocument::from_repository_file)
        .collect::<Vec<_>>();
    search_json_documents(config, &documents)
}

pub(crate) fn search_json_documents(
    config: &QueryConfig,
    documents: &[SearchDocument],
) -> Result<JsonSearchReport> {
    let total_started = std::time::Instant::now();
    let docs = repository_docs(documents)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let matcher = build_matcher(config, multiline_verifier)?;
    let resolve_path = |doc_id| scan_doc_path(documents, doc_id);
    let outcomes = verify_candidate_sources_json(
        &candidate_ids,
        docs.len(),
        &resolve_path,
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
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        bytes_searched += task.outcome.bytes_searched;
        if task.outcome.matched_occurrences == 0 {
            continue;
        }

        searches_with_match += 1;
        matched_lines += task.outcome.matched_lines as u64;
        matches += task.outcome.matched_occurrences as u64;
        files.push(JsonSearchFile {
            path: crate::json_output::JsonData::from_string(doc.path.clone()),
            messages: task.outcome.messages,
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

fn scan_doc_path(documents: &[SearchDocument], doc_id: u32) -> Result<std::path::PathBuf> {
    let index = usize::try_from(doc_id)
        .map_err(|_| AppError::ValueOutOfRange(format!("doc id {doc_id} exceeds usize range")))?;
    documents
        .get(index)
        .map(|document| match &document.source {
            SearchDocumentSource::Path(path) => path.clone(),
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
    // SearchEngine-specific fallback behavior is covered by e2e tests.
}
