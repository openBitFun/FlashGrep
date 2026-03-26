use std::{collections::HashSet, env, io, path::Path, time::Duration};

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rayon::prelude::*;
use regex_syntax::{
    hir::{Class, Hir, HirKind},
    ParserBuilder as HirParserBuilder,
};

use crate::{
    config::QueryConfig,
    error::Result,
    index::format::DocMeta,
    search::{FileMatch, MatchLocation, SearchMode},
};

pub(super) use super::shared::build_regex_matcher as build_matcher;
use super::shared::{
    build_line_starts, doc_by_id, line_index_for_offset, trim_line_terminator_bytes,
    validate_doc_ids,
};

const MAX_LOOK_AHEAD: usize = 128;
const VERIFY_PARALLEL_THRESHOLD: usize = 512;

pub(super) fn requires_multiline_verification(config: &QueryConfig) -> Result<bool> {
    if !config.multiline && !config.dot_matches_new_line {
        return Ok(false);
    }
    let mut parser = HirParserBuilder::new();
    parser
        .case_insensitive(config.case_insensitive)
        .multi_line(config.multiline)
        .dot_matches_new_line(config.dot_matches_new_line);
    let hir = parser
        .build()
        .parse(&config.regex_pattern)
        .map_err(|error| crate::error::AppError::InvalidPattern(error.to_string()))?;
    Ok(hir_can_match_line_terminator(&hir))
}

pub(super) fn verify_candidates(
    candidate_ids: &[u32],
    docs: &[DocMeta],
    matcher: &RegexMatcher,
    multiline_verifier: bool,
    mode: SearchMode,
) -> Result<Vec<SearchTaskOutcome>> {
    validate_doc_ids(docs, candidate_ids)?;
    let mut outcomes = if should_parallel_verify(candidate_ids.len()) {
        let results = candidate_ids
            .par_iter()
            .enumerate()
            .map_init(
                || (matcher.clone(), build_verifier(multiline_verifier, mode)),
                |(matcher, verifier), (ordinal, &doc_id)| {
                    let path = Path::new(doc_by_id(docs, doc_id)?.path.as_str());
                    let outcome = verify_path(path, matcher, verifier, mode)?;
                    Ok(SearchTaskOutcome {
                        ordinal,
                        doc_id,
                        outcome,
                    })
                },
            )
            .collect::<Vec<Result<SearchTaskOutcome>>>();

        let mut outcomes = Vec::with_capacity(results.len());
        for result in results {
            outcomes.push(result?);
        }
        outcomes
    } else {
        let mut verifier = build_verifier(multiline_verifier, mode);
        let mut outcomes = Vec::with_capacity(candidate_ids.len());
        for (ordinal, &doc_id) in candidate_ids.iter().enumerate() {
            let path = Path::new(doc_by_id(docs, doc_id)?.path.as_str());
            let outcome = verify_path(path, matcher, &mut verifier, mode)?;
            outcomes.push(SearchTaskOutcome {
                ordinal,
                doc_id,
                outcome,
            });
        }
        return Ok(outcomes);
    };
    outcomes.sort_unstable_by_key(|task| task.ordinal);
    Ok(outcomes)
}

pub(super) struct VerifyOutcome {
    pub(super) matched_lines: usize,
    pub(super) matches: Vec<FileMatch>,
}

pub(super) struct SearchTaskOutcome {
    pub(super) ordinal: usize,
    pub(super) doc_id: u32,
    pub(super) outcome: VerifyOutcome,
}

#[derive(Debug, Default)]
pub(super) struct SearchProfile {
    enabled: bool,
}

impl SearchProfile {
    pub(super) fn enabled() -> Self {
        Self {
            enabled: env::var_os("BITFUN_PROFILE_SEARCH").is_some(),
        }
    }

    pub(super) fn record_plan(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile plan_secs={:.6}", duration.as_secs_f64());
        }
    }

    pub(super) fn record_candidates(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile candidate_secs={:.6}", duration.as_secs_f64());
        }
    }

    pub(super) fn record_regex_compile(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile regex_compile_secs={:.6}", duration.as_secs_f64());
        }
    }

    pub(super) fn record_verify(&self, match_duration: Duration) {
        if self.enabled {
            eprintln!(
                "profile verify_read_secs={:.6} verify_match_secs={:.6}",
                0.0,
                match_duration.as_secs_f64()
            );
        }
    }

    pub(super) fn finish(
        &self,
        total_duration: Duration,
        candidate_docs: usize,
        match_count: usize,
    ) {
        if self.enabled {
            eprintln!(
                "profile total_secs={:.6} candidate_docs={} match_count={}",
                total_duration.as_secs_f64(),
                candidate_docs,
                match_count
            );
        }
    }
}

fn build_verifier(multiline_verifier: bool, mode: SearchMode) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(mode.materializes_matches())
        .binary_detection(BinaryDetection::quit(b'\0'))
        .multi_line(multiline_verifier);
    if matches!(mode, SearchMode::FirstHitOnly) {
        builder.max_matches(Some(1));
    }
    builder.build()
}

fn verify_path(
    path: &Path,
    matcher: &RegexMatcher,
    verifier: &mut Searcher,
    mode: SearchMode,
) -> Result<VerifyOutcome> {
    let mut sink = VerifySink::new(matcher, mode);
    verifier.search_path(matcher, path, &mut sink)?;
    Ok(sink.finish())
}

fn should_parallel_verify(candidate_count: usize) -> bool {
    candidate_count >= VERIFY_PARALLEL_THRESHOLD
        && std::thread::available_parallelism().map_or(1, std::num::NonZero::get) > 1
}

fn hir_can_match_line_terminator(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Literal(literal) => literal.0.contains(&b'\n'),
        HirKind::Class(class) => class_can_match_line_terminator(class),
        HirKind::Capture(capture) => hir_can_match_line_terminator(&capture.sub),
        HirKind::Repetition(repetition) => hir_can_match_line_terminator(&repetition.sub),
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => {
            hirs.iter().any(hir_can_match_line_terminator)
        }
    }
}

fn class_can_match_line_terminator(class: &Class) -> bool {
    match class {
        Class::Unicode(class) => class
            .ranges()
            .iter()
            .any(|range| range.start() <= '\n' && '\n' <= range.end()),
        Class::Bytes(class) => class
            .ranges()
            .iter()
            .any(|range| range.start() <= b'\n' && b'\n' <= range.end()),
    }
}

struct OffsetLocation {
    line: usize,
    column: usize,
    line_start_offset: usize,
    line_end_offset: usize,
}

fn offset_to_location(offset: usize, line_starts: &[usize], text_len: usize) -> OffsetLocation {
    let line_index = line_index_for_offset(offset, line_starts);
    let line_start = line_starts[line_index];
    let line_end = line_starts.get(line_index + 1).copied().unwrap_or(text_len);

    OffsetLocation {
        line: line_index + 1,
        column: offset.saturating_sub(line_start) + 1,
        line_start_offset: line_start,
        line_end_offset: line_end,
    }
}

struct VerifySink<'m> {
    matcher: &'m RegexMatcher,
    mode: SearchMode,
    matched_lines: usize,
    matches: Vec<FileMatch>,
}

impl<'m> VerifySink<'m> {
    fn new(matcher: &'m RegexMatcher, mode: SearchMode) -> Self {
        Self {
            matcher,
            mode,
            matched_lines: 0,
            matches: Vec::new(),
        }
    }

    fn finish(self) -> VerifyOutcome {
        VerifyOutcome {
            matched_lines: self.matched_lines,
            matches: self.matches,
        }
    }

    fn record_match_group(&mut self, searcher: &Searcher, mat: &SinkMatch<'_>) -> io::Result<bool> {
        match self.mode {
            SearchMode::CountOnly => {
                if searcher.multi_line() {
                    self.matched_lines += mat.lines().count().max(1);
                } else {
                    self.matched_lines += 1;
                }
                Ok(true)
            }
            SearchMode::FirstHitOnly => {
                let buffer = mat.buffer();
                let range = mat.bytes_range_in_buffer();
                let line_starts = build_line_starts(buffer);
                let base_line = buffer_base_line_number(mat, &line_starts);
                let mut seen_lines = HashSet::new();
                let mut first_match = None;
                find_iter_at_in_context(searcher, self.matcher, buffer, range, |matched| {
                    record_line_span(&mut seen_lines, &line_starts, matched);
                    first_match = Some(matched);
                    false
                })?;
                let Some(matched) = first_match else {
                    return Ok(true);
                };
                self.matched_lines += seen_lines.len().max(1);
                self.matches
                    .push(build_file_match(buffer, matched, &line_starts, base_line));
                Ok(false)
            }
            SearchMode::MaterializeMatches => {
                let buffer = mat.buffer();
                let range = mat.bytes_range_in_buffer();
                let line_starts = build_line_starts(buffer);
                let base_line = buffer_base_line_number(mat, &line_starts);
                let mut seen_lines = HashSet::new();
                find_iter_at_in_context(searcher, self.matcher, buffer, range, |matched| {
                    record_line_span(&mut seen_lines, &line_starts, matched);
                    self.matches
                        .push(build_file_match(buffer, matched, &line_starts, base_line));
                    true
                })
                .map_err(io_error_from_matcher)?;
                if !seen_lines.is_empty() {
                    self.matched_lines += seen_lines.len();
                }
                Ok(true)
            }
        }
    }
}

impl Sink for VerifySink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        self.record_match_group(searcher, mat)
    }
}

fn buffer_base_line_number(mat: &SinkMatch<'_>, line_starts: &[usize]) -> usize {
    let range = mat.bytes_range_in_buffer();
    let range_start_line = line_index_for_offset(range.start, line_starts);
    mat.line_number().map_or(1, |line| {
        usize::try_from(line)
            .unwrap_or(usize::MAX)
            .saturating_sub(range_start_line)
    })
}

fn trim_line_terminator<'b>(
    searcher: &Searcher,
    buf: &'b [u8],
    line: &mut grep_matcher::Match,
) -> &'b [u8] {
    let line_term = searcher.line_terminator();
    if line_term.is_suffix(&buf[*line]) {
        let mut end = line.end() - 1;
        if line_term.is_crlf() && end > 0 && buf.get(end - 1) == Some(&b'\r') {
            end -= 1;
        }
        let orig_end = line.end();
        *line = line.with_end(end);
        &buf[end..orig_end]
    } else {
        &[]
    }
}

fn find_iter_at_in_context<F>(
    searcher: &Searcher,
    matcher: &RegexMatcher,
    mut bytes: &[u8],
    range: std::ops::Range<usize>,
    mut matched: F,
) -> io::Result<()>
where
    F: FnMut(grep_matcher::Match) -> bool,
{
    if searcher.multi_line_with_matcher(matcher) {
        if bytes[range.end..].len() >= MAX_LOOK_AHEAD {
            bytes = &bytes[..range.end + MAX_LOOK_AHEAD];
        }
    } else {
        let mut line = grep_matcher::Match::new(0, range.end);
        trim_line_terminator(searcher, bytes, &mut line);
        bytes = &bytes[..line.end()];
    }
    matcher
        .find_iter_at(bytes, range.start, |matched_range| {
            if matched_range.start() >= range.end {
                return false;
            }
            matched(matched_range)
        })
        .map_err(io_error_from_matcher)
}

fn build_file_match(
    bytes: &[u8],
    matched: grep_matcher::Match,
    line_starts: &[usize],
    base_line: usize,
) -> FileMatch {
    let location = offset_to_location(matched.start(), line_starts, bytes.len());
    let line =
        trim_line_terminator_bytes(&bytes[location.line_start_offset..location.line_end_offset]);

    FileMatch {
        location: MatchLocation {
            line: base_line + location.line - 1,
            column: location.column,
        },
        snippet: String::from_utf8_lossy(line).into_owned(),
        matched_text: String::from_utf8_lossy(&bytes[matched]).into_owned(),
    }
}

fn io_error_from_matcher<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::other(error.to_string())
}

fn record_line_span(
    seen_lines: &mut HashSet<usize>,
    line_starts: &[usize],
    matched: grep_matcher::Match,
) {
    let start = line_index_for_offset(matched.start(), line_starts);
    let end_offset = if matched.is_empty() {
        matched.start()
    } else {
        matched.end().saturating_sub(1)
    };
    let end = line_index_for_offset(end_offset, line_starts);
    for line in start..=end {
        seen_lines.insert(line);
    }
}
