use std::{fs::File, path::Path};

use aho_corasick::{AhoCorasick, MatchKind};
use memmap2::Mmap;
use rayon::prelude::*;

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::{is_text_file, RepositoryFile, ScanOptions},
    planner::plan,
    query_runtime::{
        build_matcher, requires_multiline_verification, verify_candidate_sources_with_resolver,
        CountVerifyKind, CountVerifyPlan, LinePrefilter, PureLiteralMatcher, VerifyCandidateSource,
    },
    search::{append_line_match_file, FileCount, FileMatchCount, SearchMode, SearchResults},
};

use super::document::{SearchDocument, SearchDocumentSource};

const PARALLEL_DOC_THRESHOLD: usize = 16;
const PARALLEL_BYTE_THRESHOLD: u64 = 8 * 1024 * 1024;

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
    let resolve_source = |doc_id| scan_doc_source(documents, doc_id);
    if matches!(config.search_mode, SearchMode::LineMatches) {
        let matcher = build_matcher(config, multiline_verifier)?;
        let literal_matcher = PureLiteralMatcher::compile(config, multiline_verifier)?;
        let outcomes = verify_candidate_sources_with_resolver(
            &candidate_ids,
            docs.len(),
            &resolve_source,
            &matcher,
            literal_matcher.as_ref(),
            line_prefilter.as_ref(),
            multiline_verifier,
            config.max_count,
            config.effective_global_max_results(),
        )?;
        let mut results = SearchResults {
            candidate_docs: docs.len(),
            searches_with_match: 0,
            bytes_searched: 0,
            matched_lines: 0,
            matched_occurrences: 0,
            matched_paths: Vec::new(),
            line_matches: Vec::new(),
            line_match_files: Vec::new(),
            file_counts: Vec::new(),
            file_match_counts: Vec::new(),
        };
        for task in outcomes {
            let path = scan_doc_meta(docs.as_slice(), task.doc_id)?.path.clone();
            results.bytes_searched += task.outcome.bytes_searched;
            results.matched_lines += task.outcome.matched_lines;
            results.matched_occurrences += task.outcome.matched_occurrences;
            if !task.outcome.line_matches.is_empty() {
                results.searches_with_match += 1;
                append_line_match_file(
                    &mut results.line_match_files,
                    path,
                    task.outcome.line_matches,
                );
            }
        }
        return Ok(results);
    }

    let verify_plan = CountVerifyPlan::compile(
        config,
        multiline_verifier,
        line_prefilter,
        count_kind_for_mode(config.search_mode),
    )?;
    let counts = verify_plan.verify_candidate_counts_by_doc_with_sources(
        &candidate_ids,
        docs.len(),
        &resolve_source,
        config.effective_global_max_results(),
    )?;

    build_results_from_counts(
        config.search_mode,
        docs.len(),
        documents.iter().map(|document| document.size).sum(),
        counts
            .into_iter()
            .map(|count| {
                Ok((
                    scan_doc_meta(docs.as_slice(), count.doc_id)?.path.clone(),
                    count.matched_lines,
                    count.matched_occurrences,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
    )
}

pub(crate) fn scan_options(config: &BuildConfig) -> ScanOptions {
    ScanOptions {
        respect_ignore: matches!(config.corpus_mode, crate::config::CorpusMode::RespectIgnore),
        include_hidden: config.include_hidden,
        max_file_size: config.max_file_size,
        max_depth: None,
        ignore_files: Vec::new(),
    }
}

pub(crate) fn scan_text_files(files: Vec<RepositoryFile>) -> Result<Vec<RepositoryFile>> {
    let mut text_files = Vec::with_capacity(files.len());
    for file in files {
        if is_text_file(Path::new(&file.path))? {
            text_files.push(file);
        }
    }
    Ok(text_files)
}

fn search_documents_multi_literal_fast(
    config: &QueryConfig,
    documents: &[SearchDocument],
) -> Result<Option<SearchResults>> {
    if matches!(config.search_mode, SearchMode::LineMatches) {
        return Ok(None);
    }
    if config.max_count.is_some()
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

    let outcomes = if config.effective_global_max_results().is_none()
        && should_parallel_scan_documents(documents)
    {
        documents
            .par_iter()
            .filter_map(|document| scan_document_multi_literal(document, &matcher))
            .collect::<Vec<_>>()
    } else {
        documents
            .iter()
            .filter_map(|document| scan_document_multi_literal(document, &matcher))
            .collect::<Vec<_>>()
    };

    let max_results = config.effective_global_max_results().unwrap_or(usize::MAX);
    let limited = outcomes.into_iter().take(max_results).collect::<Vec<_>>();
    Ok(Some(build_results_from_counts(
        config.search_mode,
        documents.len(),
        limited.iter().map(|outcome| outcome.bytes_searched).sum(),
        limited
            .into_iter()
            .map(|outcome| {
                (
                    outcome.path,
                    outcome.matched_lines,
                    outcome.matched_occurrences,
                )
            })
            .collect(),
    )?))
}

#[derive(Debug)]
struct MultiLiteralDocumentOutcome {
    path: String,
    matched_lines: usize,
    matched_occurrences: usize,
    bytes_searched: u64,
}

#[derive(Debug, Clone)]
struct ScannedDocMeta {
    doc_id: u32,
    path: String,
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
) -> Option<MultiLiteralDocumentOutcome> {
    let bytes = document_bytes(document)?;
    let (matched_lines, matched_occurrences) = count_matches(&bytes, matcher);
    if matched_occurrences == 0 {
        return None;
    }
    Some(MultiLiteralDocumentOutcome {
        path: document.logical_path.clone(),
        matched_lines,
        matched_occurrences,
        bytes_searched: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    })
}

fn document_bytes(document: &SearchDocument) -> Option<Vec<u8>> {
    match &document.source {
        SearchDocumentSource::Path(path) => {
            if document.size == 0 {
                return None;
            }
            let file = File::open(path).ok()?;
            let mmap = unsafe { Mmap::map(&file).ok()? };
            Some(mmap.to_vec())
        }
        SearchDocumentSource::LoadedBytes(bytes) => Some(bytes.to_vec()),
    }
}

fn count_matches(bytes: &[u8], matcher: &AhoCorasick) -> (usize, usize) {
    let mut matched_lines = 0usize;
    let mut matched_occurrences = 0usize;
    let mut last_match_end = 0usize;

    for matched in matcher.find_iter(bytes) {
        if matched_occurrences == 0 || bytes[last_match_end..matched.start()].contains(&b'\n') {
            matched_lines += 1;
        }
        matched_occurrences += 1;
        last_match_end = matched.end();
    }

    (matched_lines, matched_occurrences)
}

fn repository_docs(documents: &[SearchDocument]) -> Result<Vec<ScannedDocMeta>> {
    documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let doc_id = u32::try_from(index).map_err(|_| {
                AppError::ValueOutOfRange(format!("scanned document id {index} exceeds u32 range"))
            })?;
            Ok(ScannedDocMeta {
                doc_id,
                path: document.logical_path.clone(),
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

fn scan_doc_meta(documents: &[ScannedDocMeta], doc_id: u32) -> Result<&ScannedDocMeta> {
    let index = usize::try_from(doc_id)
        .map_err(|_| AppError::ValueOutOfRange(format!("doc id {doc_id} exceeds usize range")))?;
    documents.get(index).ok_or_else(|| {
        AppError::InvalidIndex(format!("doc id {doc_id} is missing from scan metadata"))
    })
}

fn count_kind_for_mode(mode: SearchMode) -> CountVerifyKind {
    match mode {
        SearchMode::FilesWithMatches | SearchMode::CountOnly => CountVerifyKind::Lines,
        SearchMode::CountMatches => CountVerifyKind::Occurrences,
        SearchMode::LineMatches => unreachable!("line matches use the lightweight verifier"),
    }
}

fn build_results_from_counts(
    mode: SearchMode,
    candidate_docs: usize,
    bytes_searched: u64,
    counts: Vec<(String, usize, usize)>,
) -> Result<SearchResults> {
    let searches_with_match = counts.len();
    let matched_lines = counts
        .iter()
        .map(|(_, matched_lines, _)| *matched_lines)
        .sum();
    let matched_occurrences = counts
        .iter()
        .map(|(_, _, matched_occurrences)| *matched_occurrences)
        .sum();

    let mut results = SearchResults {
        candidate_docs,
        searches_with_match,
        bytes_searched,
        matched_lines,
        matched_occurrences,
        matched_paths: Vec::new(),
        line_matches: Vec::new(),
        line_match_files: Vec::new(),
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
    };

    match mode {
        SearchMode::FilesWithMatches => {
            results.matched_paths = counts.into_iter().map(|(path, _, _)| path).collect();
        }
        SearchMode::LineMatches => unreachable!("line_matches are built separately"),
        SearchMode::CountOnly => {
            results.file_counts = counts
                .into_iter()
                .map(|(path, matched_lines, _)| FileCount {
                    path,
                    matched_lines,
                })
                .collect();
        }
        SearchMode::CountMatches => {
            results.file_match_counts = counts
                .into_iter()
                .map(|(path, _, matched_occurrences)| FileMatchCount {
                    path,
                    matched_occurrences,
                })
                .collect();
        }
    }

    if searches_with_match > candidate_docs {
        return Err(AppError::InvalidIndex(format!(
            "matched doc count {searches_with_match} exceeds candidate doc count {candidate_docs}"
        )));
    }

    Ok(results)
}
