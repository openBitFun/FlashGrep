use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkFinish, SinkMatch};
use memmap2::Mmap;
use rayon::prelude::*;
use regex_syntax::{
    hir::{Class, Hir, HirKind},
    ParserBuilder as HirParserBuilder,
};

use crate::{
    config::QueryConfig,
    error::Result,
    search::LineMatch,
};

use super::query::LinePrefilter;
pub(crate) use super::shared::build_regex_matcher as build_matcher;
use super::shared::{
    build_line_starts, configure_verifier_searcher, line_index_for_offset,
    trim_line_terminator_bytes, validate_doc_id, validate_doc_ids, with_verify_path_bytes,
};

const MAX_LOOK_AHEAD: usize = 128;
const VERIFY_PARALLEL_THRESHOLD: usize = 128;

#[derive(Debug, Clone)]
pub(crate) enum VerifyCandidateSource {
    Path(PathBuf),
    Bytes(Arc<[u8]>),
    Mmap(Arc<Mmap>),
}

#[derive(Debug, Clone)]
pub(crate) struct VerifyPlan {
    matcher: RegexMatcher,
    line_prefilter: Option<LinePrefilter>,
    multiline_verifier: bool,
    max_count: Option<usize>,
}

impl VerifyPlan {
    pub(crate) fn compile(
        config: &QueryConfig,
        multiline_verifier: bool,
        line_prefilter: Option<LinePrefilter>,
    ) -> Result<Self> {
        Ok(Self {
            matcher: build_matcher(config, multiline_verifier)?,
            line_prefilter: (!multiline_verifier).then_some(line_prefilter).flatten(),
            multiline_verifier,
            max_count: config.max_count,
        })
    }

    pub(crate) fn create_runtime(&self) -> VerifyRuntime<'_> {
        VerifyRuntime {
            matcher: &self.matcher,
            line_prefilter: self.line_prefilter.as_ref(),
            verifier: build_verifier(self.multiline_verifier, self.max_count),
        }
    }
}

pub(crate) fn requires_multiline_verification(config: &QueryConfig) -> Result<bool> {
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

pub(crate) fn verify_candidate_sources_with_resolver<F>(
    candidate_ids: &[u32],
    doc_count: usize,
    resolve_source: &F,
    matcher: &RegexMatcher,
    line_prefilter: Option<&LinePrefilter>,
    multiline_verifier: bool,
    max_count: Option<usize>,
    global_max_results: Option<usize>,
) -> Result<Vec<SearchTaskOutcome>>
where
    F: Fn(u32) -> Result<VerifyCandidateSource> + Sync,
{
    validate_doc_ids(doc_count, candidate_ids)?;
    let mut outcomes =
        if global_max_results.is_none() && should_parallel_verify(candidate_ids.len()) {
            let results = candidate_ids
                .par_iter()
                .enumerate()
                .map_init(
                    || (matcher.clone(), build_verifier(multiline_verifier, max_count)),
                    |(matcher, verifier), (ordinal, &doc_id)| {
                        validate_doc_id(doc_count, doc_id)?;
                        let source = resolve_source(doc_id)?;
                        let outcome =
                            verify_source(&source, matcher, line_prefilter, verifier)?;
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
            let mut verifier = build_verifier(multiline_verifier, max_count);
            let mut outcomes = Vec::with_capacity(candidate_ids.len());
            let mut consumed_results = 0usize;
            for (ordinal, &doc_id) in candidate_ids.iter().enumerate() {
                validate_doc_id(doc_count, doc_id)?;
                let source = resolve_source(doc_id)?;
                let outcome = verify_source(&source, matcher, line_prefilter, &mut verifier)?;
                consumed_results += outcome.line_matches.len();
                outcomes.push(SearchTaskOutcome {
                    ordinal,
                    doc_id,
                    outcome,
                });
                if global_max_results.is_some_and(|limit| consumed_results >= limit) {
                    break;
                }
            }
            return Ok(outcomes);
        };
    outcomes.sort_unstable_by_key(|task| task.ordinal);
    Ok(outcomes)
}

pub(crate) struct VerifyOutcome {
    pub(crate) matched_lines: usize,
    pub(crate) matched_occurrences: usize,
    pub(crate) bytes_searched: u64,
    pub(crate) line_matches: Vec<LineMatch>,
}

pub(crate) struct SearchTaskOutcome {
    pub(crate) ordinal: usize,
    pub(crate) doc_id: u32,
    pub(crate) outcome: VerifyOutcome,
}

pub(crate) struct VerifyRuntime<'a> {
    matcher: &'a RegexMatcher,
    line_prefilter: Option<&'a LinePrefilter>,
    verifier: Searcher,
}

impl VerifyRuntime<'_> {
    pub(crate) fn verify_source(
        &mut self,
        source: &VerifyCandidateSource,
    ) -> Result<VerifyOutcome> {
        verify_source(
            source,
            self.matcher,
            self.line_prefilter,
            &mut self.verifier,
        )
    }
}

fn build_verifier(multiline_verifier: bool, max_count: Option<usize>) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder.line_number(true);
    configure_verifier_searcher(&mut builder, multiline_verifier);
    if let Some(limit) = max_count.and_then(|count| u64::try_from(count).ok()) {
        builder.max_matches(Some(limit));
    }
    builder.build()
}

fn verify_path(
    path: &Path,
    matcher: &RegexMatcher,
    line_prefilter: Option<&LinePrefilter>,
    verifier: &mut Searcher,
) -> Result<VerifyOutcome> {
    with_verify_path_bytes(path, |bytes| verify_slice(bytes, matcher, line_prefilter, verifier))
}

fn verify_slice(
    bytes: &[u8],
    matcher: &RegexMatcher,
    line_prefilter: Option<&LinePrefilter>,
    verifier: &mut Searcher,
) -> Result<VerifyOutcome> {
    let mut sink = VerifySink::new(matcher);
    if let Some(prefilter) = line_prefilter {
        let wrapped = LinePrefilterMatcher::new(matcher, prefilter);
        verifier.search_slice(&wrapped, bytes, &mut sink)?;
    } else {
        verifier.search_slice(matcher, bytes, &mut sink)?;
    }
    Ok(sink.finish())
}

fn verify_source(
    source: &VerifyCandidateSource,
    matcher: &RegexMatcher,
    line_prefilter: Option<&LinePrefilter>,
    verifier: &mut Searcher,
) -> Result<VerifyOutcome> {
    match source {
        VerifyCandidateSource::Path(path) => verify_path(path, matcher, line_prefilter, verifier),
        VerifyCandidateSource::Bytes(bytes) => verify_slice(bytes, matcher, line_prefilter, verifier),
        VerifyCandidateSource::Mmap(mmap) => verify_slice(mmap.as_ref(), matcher, line_prefilter, verifier),
    }
}

struct LinePrefilterMatcher<'a> {
    inner: &'a RegexMatcher,
    prefilter: &'a LinePrefilter,
}

impl<'a> LinePrefilterMatcher<'a> {
    fn new(inner: &'a RegexMatcher, prefilter: &'a LinePrefilter) -> Self {
        Self { inner, prefilter }
    }
}

impl Matcher for LinePrefilterMatcher<'_> {
    type Captures = grep_regex::RegexCaptures;
    type Error = grep_matcher::NoError;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> std::result::Result<Option<grep_matcher::Match>, Self::Error> {
        self.inner.find_at(haystack, at)
    }

    fn new_captures(&self) -> std::result::Result<Self::Captures, Self::Error> {
        self.inner.new_captures()
    }

    fn capture_count(&self) -> usize {
        self.inner.capture_count()
    }

    fn capture_index(&self, name: &str) -> Option<usize> {
        self.inner.capture_index(name)
    }

    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> std::result::Result<std::result::Result<(), E>, Self::Error>
    where
        F: FnMut(grep_matcher::Match) -> std::result::Result<bool, E>,
    {
        self.inner.try_find_iter(haystack, matched)
    }

    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
    ) -> std::result::Result<bool, Self::Error> {
        self.inner.captures_at(haystack, at, caps)
    }

    fn shortest_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> std::result::Result<Option<usize>, Self::Error> {
        self.inner.shortest_match_at(haystack, at)
    }

    fn non_matching_bytes(&self) -> Option<&grep_matcher::ByteSet> {
        self.inner.non_matching_bytes()
    }

    fn line_terminator(&self) -> Option<grep_matcher::LineTerminator> {
        self.inner.line_terminator()
    }

    fn find_candidate_line(
        &self,
        haystack: &[u8],
    ) -> std::result::Result<Option<grep_matcher::LineMatchKind>, Self::Error> {
        Ok(self
            .prefilter
            .find_candidate_line(haystack, b'\n')
            .map(grep_matcher::LineMatchKind::Candidate))
    }
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

struct VerifySink<'m> {
    matcher: &'m RegexMatcher,
    matched_lines: usize,
    matched_occurrences: usize,
    bytes_searched: u64,
    line_matches: Vec<LineMatch>,
}

impl<'m> VerifySink<'m> {
    fn new(matcher: &'m RegexMatcher) -> Self {
        Self {
            matcher,
            matched_lines: 0,
            matched_occurrences: 0,
            bytes_searched: 0,
            line_matches: Vec::new(),
        }
    }

    fn finish(self) -> VerifyOutcome {
        VerifyOutcome {
            matched_lines: self.matched_lines,
            matched_occurrences: self.matched_occurrences,
            bytes_searched: self.bytes_searched,
            line_matches: self.line_matches,
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
        let buffer = mat.buffer();
        let range = mat.bytes_range_in_buffer();
        let line_starts = build_line_starts(buffer);
        let base_line = buffer_base_line_number(mat, &line_starts);
        let mut matched_occurrences = 0usize;
        find_iter_at_in_context(searcher, self.matcher, buffer, range.clone(), |_| {
            matched_occurrences += 1;
            true
        })
        .map_err(io_error_from_matcher)?;
        self.matched_occurrences += matched_occurrences.max(1);
        self.matched_lines += mat.lines().count().max(1);
        let line_match = build_line_match(buffer, range.start, &line_starts, base_line);
        if self
            .line_matches
            .last()
            .is_none_or(|previous| previous.line_number != line_match.line_number || previous.line_text != line_match.line_text)
        {
            self.line_matches.push(line_match);
        }
        Ok(true)
    }

    fn finish(
        &mut self,
        _searcher: &Searcher,
        finish: &SinkFinish,
    ) -> std::result::Result<(), Self::Error> {
        self.bytes_searched = finish.byte_count();
        Ok(())
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

fn build_line_match(
    bytes: &[u8],
    start: usize,
    line_starts: &[usize],
    base_line: usize,
) -> LineMatch {
    let line_index = line_index_for_offset(start, line_starts);
    let line_start = line_starts[line_index];
    let line_end = line_starts
        .get(line_index + 1)
        .copied()
        .unwrap_or(bytes.len());
    let snippet = trim_line_terminator_bytes(&bytes[line_start..line_end]);

    LineMatch {
        path: String::new(),
        line_number: base_line + line_index,
        line_text: String::from_utf8_lossy(snippet).into_owned(),
    }
}

fn io_error_from_matcher<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::other(error.to_string())
}
