use std::{
    env,
    fs::File,
    io::{Read, Result as IoResult},
    path::Path,
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
    index::format::DocMeta,
};

use super::{
    query::LiteralPrefilter,
    shared::{build_regex_matcher, doc_by_id, trim_line_terminator_bytes, validate_doc_ids},
};

const VERIFY_PARALLEL_THRESHOLD: usize = 512;
const VERIFY_SMALL_FILE_READ_LIMIT: u64 = 64 * 1024;

#[derive(Debug)]
pub(super) enum VerifyPlan {
    CountOnlyLineMeta(CountOnlyLineMetaPlan),
    SearcherLine(SearcherLinePlan),
    SearcherMultiline(SearcherMultilinePlan),
}

impl VerifyPlan {
    pub(super) fn compile(
        config: &QueryConfig,
        multiline_verifier: bool,
        literal_prefilter: Option<LiteralPrefilter>,
    ) -> Result<Self> {
        if multiline_verifier {
            return Ok(Self::SearcherMultiline(SearcherMultilinePlan {
                matcher: build_regex_matcher(config, true)?,
                max_count: config.max_count,
            }));
        }
        if config.word_regexp || config.line_regexp {
            return Self::compile_searcher_line(config, literal_prefilter);
        }

        match env::var("BITFUN_COUNT_VERIFY_STRATEGY").ok().as_deref() {
            Some("meta") => match CountOnlyLineMetaPlan::compile(config) {
                Ok(plan) => Ok(Self::CountOnlyLineMeta(plan)),
                Err(_) => Self::compile_searcher_line(config, literal_prefilter),
            },
            Some("searcher" | "searcher-line" | "path") => {
                Self::compile_searcher_line(config, literal_prefilter)
            }
            None => Self::compile_searcher_line(config, literal_prefilter),
            Some(other) => Err(AppError::InvalidPattern(format!(
                "unknown BITFUN_COUNT_VERIFY_STRATEGY value: {other}"
            ))),
        }
    }

    pub(super) fn compile_searcher_line(
        config: &QueryConfig,
        literal_prefilter: Option<LiteralPrefilter>,
    ) -> Result<Self> {
        Ok(Self::SearcherLine(SearcherLinePlan {
            matcher: build_regex_matcher(config, false)?,
            literal_prefilter,
            max_count: config.max_count,
        }))
    }

    pub(super) fn verify_candidate_count_stats(
        &self,
        candidate_ids: &[u32],
        docs: &[DocMeta],
    ) -> Result<CountStats> {
        validate_doc_ids(docs, candidate_ids)?;
        if should_parallel_verify(candidate_ids.len()) {
            let chunk_size = verify_chunk_size(candidate_ids.len());
            candidate_ids
                .par_chunks(chunk_size)
                .map_init(
                    || self.create_runtime(),
                    |runtime, chunk| runtime.verify_chunk(chunk, docs),
                )
                .try_reduce(CountStats::default, |left, right| Ok(left + right))
        } else {
            let mut runtime = self.create_runtime();
            runtime.verify_chunk(candidate_ids, docs)
        }
    }

    pub(super) fn verify_candidate_counts_by_doc(
        &self,
        candidate_ids: &[u32],
        docs: &[DocMeta],
    ) -> Result<Vec<DocMatchCount>> {
        validate_doc_ids(docs, candidate_ids)?;
        let mut runtime = self.create_runtime();
        let mut counts = Vec::new();
        for &doc_id in candidate_ids {
            let path = Path::new(doc_by_id(docs, doc_id)?.path.as_str());
            let matched_lines = runtime.verify_path(path)?;
            if matched_lines > 0 {
                counts.push(DocMatchCount {
                    doc_id,
                    matched_lines,
                });
            }
        }
        Ok(counts)
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
pub(super) struct CountOnlyLineMetaPlan {
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
pub(super) struct SearcherLinePlan {
    matcher: RegexMatcher,
    literal_prefilter: Option<LiteralPrefilter>,
    max_count: Option<usize>,
}

#[derive(Debug)]
pub(super) struct SearcherMultilinePlan {
    matcher: RegexMatcher,
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
    fn verify_chunk(&mut self, candidate_ids: &[u32], docs: &[DocMeta]) -> Result<CountStats> {
        let mut stats = CountStats::default();
        for &doc_id in candidate_ids {
            let path = Path::new(doc_by_id(docs, doc_id)?.path.as_str());
            let matched_lines = self.verify_path(path)?;
            if matched_lines > 0 {
                stats.matched_docs += 1;
                stats.matched_lines += matched_lines;
            }
        }
        Ok(stats)
    }

    fn verify_path(&mut self, path: &Path) -> Result<usize> {
        match self {
            Self::CountOnlyLineMeta(runtime) => runtime.verify_path(path),
            Self::SearcherLine { plan, runtime } => runtime.verify_path_line(path, plan),
            Self::SearcherMultiline { plan, runtime } => runtime.verify_path_multiline(path, plan),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct CountStats {
    pub(super) matched_docs: usize,
    pub(super) matched_lines: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DocMatchCount {
    pub(super) doc_id: u32,
    pub(super) matched_lines: usize,
}

impl std::ops::Add for CountStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self {
            matched_docs: self.matched_docs + rhs.matched_docs,
            matched_lines: self.matched_lines + rhs.matched_lines,
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

    fn verify_path(&mut self, path: &Path) -> Result<usize> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(0);
        };
        let visible = visible_bytes(bytes.as_ref());
        if visible.is_empty() {
            return Ok(0);
        }
        Ok(count_meta_matched_lines(
            visible,
            self.regex,
            self.max_count,
            &mut self.cache,
        ))
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

    fn verify_path_line(&mut self, path: &Path, plan: &SearcherLinePlan) -> Result<usize> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(0);
        };
        match plan.literal_prefilter.as_ref() {
            Some(prefilter) => {
                let matcher = LinePrefilterMatcher::new(self.matcher, prefilter);
                count_matches_by_line(visible_bytes(bytes.as_ref()), &matcher, plan.max_count)
            }
            None => {
                count_matches_by_line(visible_bytes(bytes.as_ref()), self.matcher, plan.max_count)
            }
        }
    }

    fn verify_path_multiline(
        &mut self,
        path: &Path,
        _plan: &SearcherMultilinePlan,
    ) -> Result<usize> {
        let Some(bytes) = load_doc_bytes(path, &mut self.scratch)? else {
            return Ok(0);
        };
        let mut sink = CountSink::default();
        self.searcher
            .search_slice(self.matcher, bytes.as_ref(), &mut sink)?;
        Ok(sink.matched_lines)
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

fn count_matches_by_line<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
) -> Result<usize>
where
    M::Error: std::fmt::Display,
{
    if matches!(max_count, Some(0)) {
        return Ok(0);
    }
    if is_line_by_line_fast(matcher) {
        count_matches_by_line_fast(bytes, matcher, max_count)
    } else {
        count_matches_by_line_slow(bytes, matcher, max_count)
    }
}

fn count_matches_by_line_fast<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
) -> Result<usize>
where
    M::Error: std::fmt::Display,
{
    let mut matched_lines = 0usize;
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

        let is_match = match candidate {
            LineMatchKind::Confirmed(_) => true,
            LineMatchKind::Candidate(_) => matcher
                .is_match(trim_line_terminator_bytes(
                    &bytes[absolute_start..absolute_end],
                ))
                .map_err(io_error_from_matcher)?,
        };
        if is_match {
            matched_lines += 1;
            if max_count.is_some_and(|limit| matched_lines >= limit) {
                break;
            }
        }
        pos = absolute_end;
    }

    Ok(matched_lines)
}

fn count_matches_by_line_slow<M: Matcher>(
    bytes: &[u8],
    matcher: &M,
    max_count: Option<usize>,
) -> Result<usize>
where
    M::Error: std::fmt::Display,
{
    let mut matched_lines = 0usize;
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let line_end =
            memchr(b'\n', &bytes[line_start..]).map_or(bytes.len(), |idx| line_start + idx + 1);
        if matcher
            .shortest_match(trim_line_terminator_bytes(&bytes[line_start..line_end]))
            .map_err(io_error_from_matcher)?
            .is_some()
        {
            matched_lines += 1;
            if max_count.is_some_and(|limit| matched_lines >= limit) {
                break;
            }
        }
        line_start = line_end;
    }

    Ok(matched_lines)
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

#[derive(Default)]
struct CountSink {
    matched_lines: usize,
}

impl Sink for CountSink {
    type Error = std::io::Error;

    fn matched(&mut self, searcher: &Searcher, mat: &SinkMatch<'_>) -> IoResult<bool> {
        if searcher.multi_line() {
            self.matched_lines += mat.lines().count().max(1);
        } else {
            self.matched_lines += 1;
        }
        Ok(true)
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
