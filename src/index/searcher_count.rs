use std::{
    collections::HashSet,
    convert::Infallible,
    env,
    fs::File,
    io::{Read, Result as IoResult},
    path::{Path, PathBuf},
};

use grep_matcher::{LineMatchKind, Match, Matcher, NoError};
use grep_regex::{RegexCaptures, RegexMatcher};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use memchr::{memchr, memrchr};
use memmap2::Mmap;
use rayon::prelude::*;
use regex_automata::{
    meta::{Cache as MetaCache, Regex as MetaRegex},
    util::syntax,
    Input as MetaInput,
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
};

use super::{
    query::LiteralPrefilter,
    shared::{
        build_line_starts, build_regex_matcher, line_index_for_offset, trim_line_terminator_bytes,
        validate_doc_id, validate_doc_ids,
    },
};

const MAX_LOOK_AHEAD: usize = 128;
const VERIFY_PARALLEL_THRESHOLD: usize = 512;
const VERIFY_SMALL_FILE_READ_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CountKind {
    Lines,
    Occurrences,
}

#[derive(Debug)]
pub(crate) enum VerifyPlan {
    CountOnlyLineMeta(CountOnlyLineMetaPlan),
    SearcherLine(SearcherLinePlan),
    SearcherMultiline(SearcherMultilinePlan),
}

impl VerifyPlan {
    pub(super) fn compile(
        config: &QueryConfig,
        multiline_verifier: bool,
        literal_prefilter: Option<LiteralPrefilter>,
        count_kind: CountKind,
    ) -> Result<Self> {
        if multiline_verifier {
            return Ok(Self::SearcherMultiline(SearcherMultilinePlan {
                matcher: build_regex_matcher(config, true)?,
                count_kind,
                max_count: config.max_count,
            }));
        }
        if config.word_regexp || config.line_regexp {
            return Self::compile_searcher_line(config, literal_prefilter, count_kind);
        }

        match env::var("BITFUN_COUNT_VERIFY_STRATEGY").ok().as_deref() {
            Some("meta") if matches!(count_kind, CountKind::Lines) => {
                match CountOnlyLineMetaPlan::compile(config) {
                    Ok(plan) => Ok(Self::CountOnlyLineMeta(plan)),
                    Err(_) => Self::compile_searcher_line(config, literal_prefilter, count_kind),
                }
            }
            Some("meta") => Self::compile_searcher_line(config, literal_prefilter, count_kind),
            Some("searcher" | "searcher-line" | "path") => {
                Self::compile_searcher_line(config, literal_prefilter, count_kind)
            }
            None => Self::compile_searcher_line(config, literal_prefilter, count_kind),
            Some(other) => Err(AppError::InvalidPattern(format!(
                "unknown BITFUN_COUNT_VERIFY_STRATEGY value: {other}"
            ))),
        }
    }

    pub(crate) fn compile_without_prefilter(
        config: &QueryConfig,
        multiline_verifier: bool,
        count_kind: CountKind,
    ) -> Result<Self> {
        Self::compile(config, multiline_verifier, None, count_kind)
    }

    pub(super) fn compile_searcher_line(
        config: &QueryConfig,
        literal_prefilter: Option<LiteralPrefilter>,
        count_kind: CountKind,
    ) -> Result<Self> {
        Ok(Self::SearcherLine(SearcherLinePlan {
            matcher: build_regex_matcher(config, false)?,
            literal_prefilter,
            count_kind,
            max_count: config.max_count,
        }))
    }

    pub(crate) fn verify_candidate_count_stats<F>(
        &self,
        candidate_ids: &[u32],
        doc_count: usize,
        resolve_path: &F,
    ) -> Result<CountStats>
    where
        F: Fn(u32) -> Result<PathBuf> + Sync,
    {
        validate_doc_ids(doc_count, candidate_ids)?;
        if should_parallel_verify(candidate_ids.len()) {
            let chunk_size = verify_chunk_size(candidate_ids.len());
            candidate_ids
                .par_chunks(chunk_size)
                .map_init(
                    || self.create_runtime(),
                    |runtime, chunk| runtime.verify_chunk(chunk, doc_count, resolve_path),
                )
                .try_reduce(CountStats::default, |left, right| Ok(left + right))
        } else {
            let mut runtime = self.create_runtime();
            runtime.verify_chunk(candidate_ids, doc_count, resolve_path)
        }
    }

    pub(crate) fn verify_candidate_counts_by_doc<F>(
        &self,
        candidate_ids: &[u32],
        doc_count: usize,
        resolve_path: &F,
    ) -> Result<Vec<DocMatchCount>>
    where
        F: Fn(u32) -> Result<PathBuf> + Sync,
    {
        validate_doc_ids(doc_count, candidate_ids)?;
        if should_parallel_verify(candidate_ids.len()) {
            let results = candidate_ids
                .par_iter()
                .enumerate()
                .map_init(
                    || self.create_runtime(),
                    |runtime, (ordinal, &doc_id)| {
                        validate_doc_id(doc_count, doc_id)?;
                        let path = resolve_path(doc_id)?;
                        let stats = runtime.verify_path(&path)?;
                        Ok(stats.has_match().then_some(OrdinalDocMatchCount {
                            ordinal,
                            count: DocMatchCount {
                                doc_id,
                                matched_lines: stats.matched_lines,
                                matched_occurrences: stats.matched_occurrences,
                            },
                        }))
                    },
                )
                .collect::<Vec<Result<Option<OrdinalDocMatchCount>>>>();

            let mut counts = Vec::new();
            for result in results {
                if let Some(count) = result? {
                    counts.push(count);
                }
            }
            counts.sort_unstable_by_key(|count| count.ordinal);
            Ok(counts.into_iter().map(|count| count.count).collect())
        } else {
            let mut runtime = self.create_runtime();
            let mut counts = Vec::new();
            for &doc_id in candidate_ids {
                validate_doc_id(doc_count, doc_id)?;
                let path = resolve_path(doc_id)?;
                let stats = runtime.verify_path(&path)?;
                if stats.has_match() {
                    counts.push(DocMatchCount {
                        doc_id,
                        matched_lines: stats.matched_lines,
                        matched_occurrences: stats.matched_occurrences,
                    });
                }
            }
            Ok(counts)
        }
    }

    fn create_runtime(&self) -> VerifyRuntime<'_> {
        match self {
            Self::CountOnlyLineMeta(plan) => VerifyRuntime::CountOnlyLineMeta(Box::new(
                CountOnlyLineMetaRuntime::new(&plan.regex, plan.max_count),
            )),
            Self::SearcherLine(plan) => VerifyRuntime::SearcherLine {
                plan,
                runtime: SearcherRuntime::new(&plan.matcher, false, plan.max_count),
            },
            Self::SearcherMultiline(plan) => VerifyRuntime::SearcherMultiline {
                plan,
                runtime: SearcherRuntime::new(&plan.matcher, true, plan.max_count),
            },
        }
    }
}

#[derive(Debug)]
pub(crate) struct CountOnlyLineMetaPlan {
    regex: MetaRegex,
    max_count: Option<usize>,
}

impl CountOnlyLineMetaPlan {
    fn compile(config: &QueryConfig) -> Result<Self> {
        let regex = MetaRegex::builder()
            .syntax(
                syntax::Config::new()
                    .case_insensitive(config.case_insensitive)
                    .multi_line(true)
                    .dot_matches_new_line(config.dot_matches_new_line)
                    .unicode(true)
                    .octal(false),
            )
            .configure(MetaRegex::config().line_terminator(b'\n'))
            .build(&config.regex_pattern)
            .map_err(|error| AppError::InvalidPattern(error.to_string()))?;
        Ok(Self {
            regex,
            max_count: config.max_count,
        })
    }
}

#[derive(Debug)]
pub(crate) struct SearcherLinePlan {
    matcher: RegexMatcher,
    literal_prefilter: Option<LiteralPrefilter>,
    count_kind: CountKind,
    max_count: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct SearcherMultilinePlan {
    matcher: RegexMatcher,
    count_kind: CountKind,
    max_count: Option<usize>,
}

enum VerifyRuntime<'a> {
    CountOnlyLineMeta(Box<CountOnlyLineMetaRuntime<'a>>),
    SearcherLine {
        plan: &'a SearcherLinePlan,
        runtime: SearcherRuntime<'a>,
    },
    SearcherMultiline {
        plan: &'a SearcherMultilinePlan,
        runtime: SearcherRuntime<'a>,
    },
}

impl VerifyRuntime<'_> {
    fn verify_chunk<F>(
        &mut self,
        candidate_ids: &[u32],
        doc_count: usize,
        resolve_path: &F,
    ) -> Result<CountStats>
    where
        F: Fn(u32) -> Result<PathBuf> + Sync,
    {
        let mut stats = CountStats::default();
        for &doc_id in candidate_ids {
            validate_doc_id(doc_count, doc_id)?;
            let path = resolve_path(doc_id)?;
            let doc_stats = self.verify_path(&path)?;
            if doc_stats.has_match() {
                stats.matched_docs += 1;
                stats.matched_lines += doc_stats.matched_lines;
                stats.matched_occurrences += doc_stats.matched_occurrences;
            }
        }
        Ok(stats)
    }

    fn verify_path(&mut self, path: &Path) -> Result<DocCountStats> {
        match self {
            Self::CountOnlyLineMeta(runtime) => runtime.verify_path(path),
            Self::SearcherLine { plan, runtime } => runtime.verify_path_line(path, plan),
            Self::SearcherMultiline { plan, runtime } => runtime.verify_path_multiline(path, plan),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CountStats {
    pub(crate) matched_docs: usize,
    pub(crate) matched_lines: usize,
    pub(crate) matched_occurrences: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DocMatchCount {
    pub(crate) doc_id: u32,
    pub(crate) matched_lines: usize,
    pub(crate) matched_occurrences: usize,
}

#[derive(Debug, Clone)]
struct OrdinalDocMatchCount {
    ordinal: usize,
    count: DocMatchCount,
}

#[derive(Debug, Default, Clone, Copy)]
struct DocCountStats {
    matched_lines: usize,
    matched_occurrences: usize,
}

impl DocCountStats {
    fn has_match(self) -> bool {
        self.matched_lines > 0 || self.matched_occurrences > 0
    }
}

impl std::ops::Add for CountStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            matched_docs: self.matched_docs + rhs.matched_docs,
            matched_lines: self.matched_lines + rhs.matched_lines,
            matched_occurrences: self.matched_occurrences + rhs.matched_occurrences,
        }
    }
}

struct CountOnlyLineMetaRuntime<'a> {
    regex: &'a MetaRegex,
    max_count: Option<usize>,
    cache: MetaCache,
    scratch: Vec<u8>,
}

impl<'a> CountOnlyLineMetaRuntime<'a> {
    fn new(regex: &'a MetaRegex, max_count: Option<usize>) -> Self {
        Self {
            regex,
            max_count,
            cache: regex.create_cache(),
            scratch: Vec::new(),
        }
    }

    fn verify_path(&mut self, path: &Path) -> Result<DocCountStats> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(DocCountStats::default());
        };
        let visible = visible_bytes(bytes.as_ref());
        if visible.is_empty() {
            return Ok(DocCountStats::default());
        }
        Ok(DocCountStats {
            matched_lines: count_meta_matched_lines(
                visible,
                self.regex,
                self.max_count,
                &mut self.cache,
            ),
            matched_occurrences: 0,
        })
    }
}

struct SearcherRuntime<'a> {
    matcher: &'a RegexMatcher,
    searcher: Searcher,
    scratch: Vec<u8>,
}

impl<'a> SearcherRuntime<'a> {
    fn new(matcher: &'a RegexMatcher, multiline_verifier: bool, max_count: Option<usize>) -> Self {
        Self {
            matcher,
            searcher: build_count_searcher(multiline_verifier, max_count),
            scratch: Vec::new(),
        }
    }

    fn verify_path_line(&mut self, path: &Path, plan: &SearcherLinePlan) -> Result<DocCountStats> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(DocCountStats::default());
        };
        match plan.literal_prefilter.as_ref() {
            Some(prefilter) => {
                let matcher = LinePrefilterMatcher::new(self.matcher, prefilter);
                count_match_stats_by_line(
                    visible_bytes(bytes.as_ref()),
                    &matcher,
                    plan.max_count,
                    plan.count_kind,
                )
            }
            None => count_match_stats_by_line(
                visible_bytes(bytes.as_ref()),
                self.matcher,
                plan.max_count,
                plan.count_kind,
            ),
        }
    }

    fn verify_path_multiline(
        &mut self,
        path: &Path,
        plan: &SearcherMultilinePlan,
    ) -> Result<DocCountStats> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(DocCountStats::default());
        };
        let mut sink = CountSink::new(self.matcher, plan.count_kind);
        self.searcher
            .search_slice(self.matcher, bytes.as_ref(), &mut sink)?;
        Ok(sink.stats)
    }
}

fn build_count_searcher(multiline_verifier: bool, max_count: Option<usize>) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(false)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .multi_line(multiline_verifier);
    if let Some(limit) = max_count.and_then(|count| u64::try_from(count).ok()) {
        builder.max_matches(Some(limit));
    }
    builder.build()
}

fn should_parallel_verify(candidate_count: usize) -> bool {
    candidate_count >= VERIFY_PARALLEL_THRESHOLD
        && std::thread::available_parallelism().map_or(1, std::num::NonZero::get) > 1
}

fn verify_chunk_size(candidate_count: usize) -> usize {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let target_chunks = threads.saturating_mul(8).max(1);
    let chunk_size = candidate_count.div_ceil(target_chunks);
    chunk_size.max(64)
}

enum LoadedBytes<'a> {
    Mmap(Mmap),
    Scratch(&'a [u8]),
}

impl AsRef<[u8]> for LoadedBytes<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(mmap) => mmap,
            Self::Scratch(bytes) => bytes,
        }
    }
}

fn load_doc_bytes<'a>(path: &Path, scratch: &'a mut Vec<u8>) -> Result<Option<LoadedBytes<'a>>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    if len <= VERIFY_SMALL_FILE_READ_LIMIT {
        scratch.clear();
        file.read_to_end(scratch)?;
        return Ok(Some(LoadedBytes::Scratch(scratch.as_slice())));
    }
    Ok(Some(LoadedBytes::Mmap(unsafe { Mmap::map(&file)? })))
}

fn visible_bytes(bytes: &[u8]) -> &[u8] {
    if let Some(nul) = memchr(b'\0', bytes) {
        &bytes[..nul]
    } else {
        bytes
    }
}

fn count_meta_matched_lines(
    bytes: &[u8],
    regex: &MetaRegex,
    max_count: Option<usize>,
    cache: &mut MetaCache,
) -> usize {
    if matches!(max_count, Some(0)) {
        return 0;
    }
    let mut matched_lines = 0usize;
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let line_end =
            memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |idx| line_start + idx + 1);
        let line = trim_line_terminator_bytes(&bytes[line_start..line_end]);
        if !line.is_empty()
            && regex
                .search_with(
                    cache,
                    &MetaInput::new(bytes).span(line_start..line.len() + line_start),
                )
                .is_some()
        {
            matched_lines += 1;
            if max_count.is_some_and(|limit| matched_lines >= limit) {
                break;
            }
        }
        line_start = line_end;
    }

    matched_lines
}

fn count_match_stats_by_line<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
    count_kind: CountKind,
) -> Result<DocCountStats>
where
    M::Error: std::fmt::Display,
{
    if matches!(max_count, Some(0)) {
        return Ok(DocCountStats::default());
    }
    if is_line_by_line_fast(matcher) {
        count_match_stats_by_line_fast(bytes, matcher, max_count, count_kind)
    } else {
        count_match_stats_by_line_slow(bytes, matcher, max_count, count_kind)
    }
}

fn count_match_stats_by_line_fast<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
    count_kind: CountKind,
) -> Result<DocCountStats>
where
    M::Error: std::fmt::Display,
{
    let mut stats = DocCountStats::default();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let Some(candidate) = matcher
            .find_candidate_line(&bytes[pos..])
            .map_err(io_error_from_matcher)?
        else {
            break;
        };
        let candidate_offset = match candidate {
            LineMatchKind::Confirmed(offset) | LineMatchKind::Candidate(offset) => offset,
        };
        let (line_start, line_end) = locate_line(&bytes[pos..], candidate_offset);
        let absolute_start = pos + line_start;
        let absolute_end = pos + line_end;
        if absolute_start >= bytes.len() {
            break;
        }

        let line = trim_line_terminator_bytes(&bytes[absolute_start..absolute_end]);
        let line_stats = match count_kind {
            CountKind::Lines => {
                let is_match = match candidate {
                    LineMatchKind::Confirmed(_) => true,
                    LineMatchKind::Candidate(_) => {
                        matcher.is_match(line).map_err(io_error_from_matcher)?
                    }
                };
                if is_match {
                    DocCountStats {
                        matched_lines: 1,
                        matched_occurrences: 0,
                    }
                } else {
                    DocCountStats::default()
                }
            }
            CountKind::Occurrences => {
                let matched_occurrences = count_line_occurrences(line, matcher)?;
                if matched_occurrences > 0 {
                    DocCountStats {
                        matched_lines: 1,
                        matched_occurrences,
                    }
                } else {
                    DocCountStats::default()
                }
            }
        };
        stats.matched_lines += line_stats.matched_lines;
        stats.matched_occurrences += line_stats.matched_occurrences;
        if max_count.is_some_and(|limit| stats.matched_lines >= limit) {
            break;
        }
        pos = absolute_end;
    }

    Ok(stats)
}

fn count_match_stats_by_line_slow<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
    count_kind: CountKind,
) -> Result<DocCountStats>
where
    M::Error: std::fmt::Display,
{
    let mut stats = DocCountStats::default();
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let line_end =
            memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |idx| line_start + idx + 1);
        let line = trim_line_terminator_bytes(&bytes[line_start..line_end]);
        let line_stats = match count_kind {
            CountKind::Lines => {
                if matcher
                    .shortest_match(line)
                    .map_err(io_error_from_matcher)?
                    .is_some()
                {
                    DocCountStats {
                        matched_lines: 1,
                        matched_occurrences: 0,
                    }
                } else {
                    DocCountStats::default()
                }
            }
            CountKind::Occurrences => {
                let matched_occurrences = count_line_occurrences(line, matcher)?;
                if matched_occurrences > 0 {
                    DocCountStats {
                        matched_lines: 1,
                        matched_occurrences,
                    }
                } else {
                    DocCountStats::default()
                }
            }
        };
        stats.matched_lines += line_stats.matched_lines;
        stats.matched_occurrences += line_stats.matched_occurrences;
        if max_count.is_some_and(|limit| stats.matched_lines >= limit) {
            break;
        }
        line_start = line_end;
    }

    Ok(stats)
}

fn count_line_occurrences<M: Matcher>(line: &[u8], matcher: &M) -> Result<usize>
where
    M::Error: std::fmt::Display,
{
    let mut matched_occurrences = 0usize;
    let result = matcher
        .try_find_iter(line, |_| -> std::result::Result<bool, Infallible> {
            matched_occurrences += 1;
            Ok(true)
        })
        .map_err(io_error_from_matcher)?;
    match result {
        Ok(()) => Ok(matched_occurrences),
        Err(never) => match never {},
    }
}

fn locate_line(haystack: &[u8], offset: usize) -> (usize, usize) {
    let start = memrchr(b'\n', &haystack[..offset]).map_or(0, |idx| idx + 1);
    let end = memchr(b'\n', &haystack[offset..]).map_or(haystack.len(), |idx| offset + idx + 1);
    (start, end)
}

fn is_line_by_line_fast<M: Matcher>(matcher: &M) -> bool {
    if matcher
        .line_terminator()
        .is_some_and(|line_term| line_term.as_byte() == b'\n')
    {
        return true;
    }
    matcher
        .non_matching_bytes()
        .is_some_and(|non_matching| non_matching.contains(b'\n'))
}

fn io_error_from_matcher<E: std::fmt::Display>(error: E) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

struct CountSink<'m> {
    matcher: &'m RegexMatcher,
    count_kind: CountKind,
    stats: DocCountStats,
}

impl<'m> CountSink<'m> {
    fn new(matcher: &'m RegexMatcher, count_kind: CountKind) -> Self {
        Self {
            matcher,
            count_kind,
            stats: DocCountStats::default(),
        }
    }
}

impl Sink for CountSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, searcher: &Searcher, mat: &SinkMatch<'_>) -> IoResult<bool> {
        match self.count_kind {
            CountKind::Lines => {
                if searcher.multi_line() {
                    self.stats.matched_lines += mat.lines().count().max(1);
                } else {
                    self.stats.matched_lines += 1;
                }
            }
            CountKind::Occurrences => {
                let buffer = mat.buffer();
                let range = mat.bytes_range_in_buffer();
                let line_starts = build_line_starts(buffer);
                let mut seen_lines = HashSet::new();
                let mut matched_occurrences = 0usize;
                find_iter_at_in_context(searcher, self.matcher, buffer, range, |matched| {
                    record_line_span(&mut seen_lines, &line_starts, matched);
                    matched_occurrences += 1;
                    true
                })?;
                self.stats.matched_lines += seen_lines.len();
                self.stats.matched_occurrences += matched_occurrences;
            }
        }
        Ok(true)
    }
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
) -> IoResult<()>
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

struct LinePrefilterMatcher<'a> {
    inner: &'a RegexMatcher,
    prefilter: &'a LiteralPrefilter,
}

impl<'a> LinePrefilterMatcher<'a> {
    fn new(inner: &'a RegexMatcher, prefilter: &'a LiteralPrefilter) -> Self {
        Self { inner, prefilter }
    }
}

impl Matcher for LinePrefilterMatcher<'_> {
    type Captures = RegexCaptures;
    type Error = NoError;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> std::result::Result<Option<Match>, Self::Error> {
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
        F: FnMut(Match) -> std::result::Result<bool, E>,
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
    ) -> std::result::Result<Option<LineMatchKind>, Self::Error> {
        Ok(self
            .prefilter
            .find_candidate_line(haystack, b'\n')
            .map(LineMatchKind::Candidate))
    }
}
