use std::{collections::HashSet, fs::File, path::Path};

use aho_corasick::{AhoCorasick, MatchKind};
use memmap2::Mmap;
use rayon::prelude::*;

use crate::{
    error::{AppError, Result},
    index::format::DocMeta,
    planner::PureLiteralAlternation,
    search::{FileMatch, MatchLocation, SearchHit, SearchLine, SearchMode, SearchResults},
};

use super::shared::{
    build_line_starts, line_index_for_offset, resolve_candidate_docs, trim_line_terminator_bytes,
};

const PARALLEL_DOC_THRESHOLD: usize = 16;
const PARALLEL_BYTE_THRESHOLD: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct PreparedMultiLiteral {
    pub(super) literals: Vec<String>,
    pub(super) case_insensitive: bool,
}

pub(super) fn prepare(
    alternation: &PureLiteralAlternation,
    case_insensitive: bool,
) -> Option<PreparedMultiLiteral> {
    if case_insensitive
        && !alternation
            .literals
            .iter()
            .all(|literal| literal.is_ascii())
    {
        return None;
    }
    Some(PreparedMultiLiteral {
        literals: alternation.literals.clone(),
        case_insensitive,
    })
}

pub(super) fn collect_candidates(
    docs: &[DocMeta],
    candidate_ids: Option<&[u32]>,
    prepared: &PreparedMultiLiteral,
) -> Result<Vec<u32>> {
    let matcher = build_fast_matcher(prepared)?;
    let candidates = if let Some(candidate_ids) = candidate_ids {
        let candidate_docs = resolve_candidate_docs(docs, candidate_ids)?;
        if should_parallel_scan(
            candidate_docs.len(),
            candidate_docs.iter().map(|doc| doc.size).sum(),
        ) {
            candidate_docs
                .par_iter()
                .map(|doc| scan_doc_id(doc, &matcher))
                .collect::<Vec<_>>()
        } else {
            candidate_docs
                .iter()
                .map(|doc| scan_doc_id(doc, &matcher))
                .collect::<Vec<_>>()
        }
    } else if should_parallel_scan(docs.len(), docs.iter().map(|doc| doc.size).sum()) {
        docs.par_iter()
            .map(|doc| scan_doc_id(doc, &matcher))
            .collect::<Vec<_>>()
    } else {
        docs.iter()
            .map(|doc| scan_doc_id(doc, &matcher))
            .collect::<Vec<_>>()
    };
    Ok(candidates.into_iter().flatten().collect())
}

pub(super) fn search(
    docs: &[DocMeta],
    candidate_ids: Option<&[u32]>,
    prepared: &PreparedMultiLiteral,
    mode: SearchMode,
) -> Result<SearchResults> {
    if matches!(mode, SearchMode::CountOnly) {
        return search_count_only(docs, candidate_ids, prepared);
    }

    let matcher = build_semantic_matcher(prepared)?;
    let outcomes = if let Some(candidate_ids) = candidate_ids {
        let candidate_docs = resolve_candidate_docs(docs, candidate_ids)?;
        if should_parallel_scan(
            candidate_docs.len(),
            candidate_docs.iter().map(|doc| doc.size).sum(),
        ) {
            candidate_docs
                .par_iter()
                .map(|doc| scan_doc(doc, &matcher, mode))
                .collect::<Vec<_>>()
        } else {
            candidate_docs
                .iter()
                .map(|doc| scan_doc(doc, &matcher, mode))
                .collect::<Vec<_>>()
        }
    } else if should_parallel_scan(docs.len(), docs.iter().map(|doc| doc.size).sum()) {
        docs.par_iter()
            .map(|doc| scan_doc(doc, &matcher, mode))
            .collect::<Vec<_>>()
    } else {
        docs.iter()
            .map(|doc| scan_doc(doc, &matcher, mode))
            .collect::<Vec<_>>()
    };

    let mut candidate_docs = 0usize;
    let mut matched_lines = 0usize;
    let mut hits = Vec::new();
    for outcome in outcomes.into_iter().flatten() {
        candidate_docs += 1;
        matched_lines += outcome.matched_lines;
        if !outcome.matches.is_empty() {
            hits.push(SearchHit {
                path: outcome.path,
                matches: outcome.matches.clone(),
                lines: outcome.matches.into_iter().map(SearchLine::Match).collect(),
            });
        }
    }

    Ok(SearchResults {
        candidate_docs,
        searches_with_match: candidate_docs,
        bytes_searched: 0,
        matched_lines,
        matched_occurrences: hits.iter().map(|hit| hit.matches.len()).sum(),
        hits,
    })
}

#[derive(Debug)]
struct Outcome {
    path: String,
    matched_lines: usize,
    matches: Vec<FileMatch>,
}

fn build_fast_matcher(prepared: &PreparedMultiLiteral) -> Result<FastMatcher> {
    build_matcher(prepared, MatchKind::Standard)
}

fn build_semantic_matcher(prepared: &PreparedMultiLiteral) -> Result<AhoCorasick> {
    build_matcher(prepared, MatchKind::LeftmostFirst)
}

fn build_matcher(prepared: &PreparedMultiLiteral, match_kind: MatchKind) -> Result<AhoCorasick> {
    AhoCorasick::builder()
        .match_kind(match_kind)
        .ascii_case_insensitive(prepared.case_insensitive)
        .build(&prepared.literals)
        .map_err(|error| AppError::InvalidPattern(error.to_string()))
}

fn search_count_only(
    docs: &[DocMeta],
    candidate_ids: Option<&[u32]>,
    prepared: &PreparedMultiLiteral,
) -> Result<SearchResults> {
    let matcher = build_fast_matcher(prepared)?;
    let (candidate_docs, matched_lines) = if let Some(candidate_ids) = candidate_ids {
        let candidate_docs = resolve_candidate_docs(docs, candidate_ids)?;
        if should_parallel_scan(
            candidate_docs.len(),
            candidate_docs.iter().map(|doc| doc.size).sum(),
        ) {
            candidate_docs
                .par_iter()
                .map(|doc| count_doc(doc, &matcher))
                .reduce(|| (0usize, 0usize), reduce_counts)
        } else {
            candidate_docs
                .iter()
                .map(|doc| count_doc(doc, &matcher))
                .fold((0usize, 0usize), reduce_counts)
        }
    } else if should_parallel_scan(docs.len(), docs.iter().map(|doc| doc.size).sum()) {
        docs.par_iter()
            .map(|doc| count_doc(doc, &matcher))
            .reduce(|| (0usize, 0usize), reduce_counts)
    } else {
        docs.iter()
            .map(|doc| count_doc(doc, &matcher))
            .fold((0usize, 0usize), reduce_counts)
    };

    Ok(SearchResults {
        candidate_docs,
        searches_with_match: candidate_docs,
        bytes_searched: 0,
        matched_lines,
        matched_occurrences: 0,
        hits: Vec::new(),
    })
}

fn should_parallel_scan(doc_count: usize, total_bytes: u64) -> bool {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) <= 1 {
        return false;
    }

    doc_count >= PARALLEL_DOC_THRESHOLD || total_bytes >= PARALLEL_BYTE_THRESHOLD
}

type FastMatcher = AhoCorasick;

fn scan_doc_id(doc: &DocMeta, matcher: &FastMatcher) -> Option<u32> {
    let bytes = map_doc_bytes(doc)?;
    matcher.find(&bytes).map(|_| doc.doc_id)
}

fn scan_doc(doc: &DocMeta, matcher: &AhoCorasick, mode: SearchMode) -> Option<Outcome> {
    let bytes = map_doc_bytes(doc)?;
    let outcome = match mode {
        SearchMode::CountOnly => unreachable!("count-only uses search_count_only"),
        SearchMode::CountMatches => unreachable!("count-matches uses regex verification"),
        SearchMode::FirstHitOnly => collect_matches(&doc.path, &bytes, matcher, true),
        SearchMode::MaterializeMatches => collect_matches(&doc.path, &bytes, matcher, false),
    };
    if outcome.matched_lines == 0 {
        None
    } else {
        Some(outcome)
    }
}

fn count_doc(doc: &DocMeta, matcher: &FastMatcher) -> (usize, usize) {
    let Some(bytes) = map_doc_bytes(doc) else {
        return (0, 0);
    };
    let matched_lines = count_matched_lines(&bytes, matcher);
    if matched_lines == 0 {
        (0, 0)
    } else {
        (1, matched_lines)
    }
}

fn reduce_counts(left: (usize, usize), right: (usize, usize)) -> (usize, usize) {
    (left.0 + right.0, left.1 + right.1)
}

fn map_doc_bytes(doc: &DocMeta) -> Option<Mmap> {
    if doc.size == 0 {
        return None;
    }
    let path = Path::new(&doc.path);
    let file = File::open(path).ok()?;
    unsafe { Mmap::map(&file).ok() }
}

fn count_matched_lines(bytes: &[u8], matcher: &FastMatcher) -> usize {
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

fn collect_matches(path: &str, bytes: &[u8], matcher: &AhoCorasick, first_only: bool) -> Outcome {
    let line_starts = build_line_starts(bytes);
    let mut seen_lines = HashSet::new();
    let mut matches = Vec::new();

    for matched in matcher.find_iter(bytes) {
        record_match_lines(
            &mut seen_lines,
            &line_starts,
            matched.start(),
            matched.end(),
        );
        matches.push(build_file_match(
            bytes,
            matched.start(),
            matched.end(),
            &line_starts,
        ));
        if first_only {
            break;
        }
    }

    Outcome {
        path: path.to_string(),
        matched_lines: seen_lines.len(),
        matches,
    }
}

fn record_match_lines(
    seen_lines: &mut HashSet<usize>,
    line_starts: &[usize],
    start: usize,
    end: usize,
) {
    let start_line = line_index_for_offset(start, line_starts);
    let end_offset = if start == end {
        start
    } else {
        end.saturating_sub(1)
    };
    let end_line = line_index_for_offset(end_offset, line_starts);
    for line in start_line..=end_line {
        seen_lines.insert(line);
    }
}

fn build_file_match(bytes: &[u8], start: usize, end: usize, line_starts: &[usize]) -> FileMatch {
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
