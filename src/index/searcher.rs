use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use regex::RegexBuilder;

use crate::{
    config::QueryConfig,
    error::Result,
    index::format::{read_docs_file, read_posting_list, IndexLayout, LookupTable},
    planner::{plan, QueryBranch},
    search::{FileMatch, MatchLocation, SearchHit, SearchResults},
    tokenizer::{create, TokenizerOptions},
};

pub struct IndexSearcher {
    docs: Vec<crate::index::format::DocMeta>,
    lookup: LookupTable,
    postings_path: PathBuf,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
}

impl IndexSearcher {
    pub fn open(index_path: PathBuf) -> Result<Self> {
        let layout = IndexLayout::new(&index_path);
        let (metadata, docs) = read_docs_file(&layout.docs_path)?;
        let lookup = LookupTable::open(&layout.lookup_path)?;

        Ok(Self {
            docs,
            lookup,
            postings_path: layout.postings_path,
            tokenizer_mode: metadata.tokenizer,
            tokenizer_options: TokenizerOptions {
                min_sparse_len: metadata.min_sparse_len,
                max_sparse_len: metadata.max_sparse_len,
            },
        })
    }

    pub fn search(&self, config: &QueryConfig) -> Result<SearchResults> {
        let profile = SearchProfile::enabled();
        let total_started = Instant::now();

        let candidate_ids = self.collect_candidate_ids(config, &profile)?;
        let candidate_doc_count = candidate_ids.len();

        let regex_started = Instant::now();
        let regex = RegexBuilder::new(&config.regex_pattern)
            .case_insensitive(config.case_insensitive)
            .multi_line(config.multiline)
            .dot_matches_new_line(config.dot_matches_new_line)
            .build()?;
        profile.record_regex_compile(regex_started.elapsed());

        let mut hits = Vec::new();
        let mut read_duration = Duration::ZERO;
        let mut match_duration = Duration::ZERO;
        for doc_id in candidate_ids {
            let doc = &self.docs[doc_id as usize];
            let path = PathBuf::from(&doc.path);
            let read_started = Instant::now();
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            read_duration += read_started.elapsed();
            let match_started = Instant::now();
            let matches = collect_matches(&text, &regex);
            match_duration += match_started.elapsed();
            if !matches.is_empty() {
                hits.push(SearchHit {
                    path: path.to_string_lossy().into_owned(),
                    matches,
                });
            }
        }
        profile.record_verify(read_duration, match_duration);
        profile.finish(
            total_started.elapsed(),
            candidate_doc_count,
            hits.iter().map(|hit| hit.matches.len()).sum(),
        );

        Ok(SearchResults {
            candidate_docs: candidate_doc_count,
            hits,
        })
    }

    pub fn candidate_paths(&self, config: &QueryConfig) -> Result<Vec<String>> {
        let profile = SearchProfile::enabled();
        let candidate_ids = self.collect_candidate_ids(config, &profile)?;
        Ok(candidate_ids
            .into_iter()
            .map(|doc_id| self.docs[doc_id as usize].path.clone())
            .collect())
    }

    fn collect_candidate_ids(
        &self,
        config: &QueryConfig,
        profile: &SearchProfile,
    ) -> Result<BTreeSet<u32>> {
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());

        let tokenizer = create(self.tokenizer_mode, self.tokenizer_options.clone());
        let candidate_started = Instant::now();
        let candidate_ids = if plan.fallback_to_scan {
            self.docs
                .iter()
                .map(|doc| doc.doc_id)
                .collect::<BTreeSet<_>>()
        } else {
            self.collect_candidates(&plan.branches, tokenizer.as_ref(), config.top_k_tokens)?
        };
        profile.record_candidates(candidate_started.elapsed());
        Ok(candidate_ids)
    }

    fn collect_candidates(
        &self,
        branches: &[QueryBranch],
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
    ) -> Result<BTreeSet<u32>> {
        let mut all_candidates = BTreeSet::new();
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
                for doc in &self.docs {
                    all_candidates.insert(doc.doc_id);
                }
                continue;
            };
            all_candidates.extend(branch_candidates);
        }

        Ok(all_candidates)
    }

    fn select_branch_candidates(
        &self,
        branch: &QueryBranch,
        tokenizer: &dyn crate::tokenizer::Tokenizer,
        top_k: usize,
        posting_cache: &mut HashMap<u64, BTreeSet<u32>>,
        literal_scan_cache: &mut HashMap<String, BTreeSet<u32>>,
    ) -> Result<Option<BTreeSet<u32>>> {
        let mut per_literal = Vec::new();
        let mut scanned_literals = Vec::new();

        for literal in &branch.literals {
            let query_literal = fold_query_literal(literal);
            let mut covering_hashes = Vec::new();
            tokenizer.collect_query_token_hashes(&query_literal, &mut covering_hashes);
            let literal_candidates =
                self.collect_literal_candidates(tokenizer, &query_literal, covering_hashes)?;
            match literal_candidates {
                LiteralCandidates::Impossible => return Ok(Some(BTreeSet::new())),
                LiteralCandidates::Unavailable => scanned_literals.push(query_literal),
                LiteralCandidates::Available(candidates) => per_literal.push(candidates),
            }
        }

        if per_literal.is_empty() && scanned_literals.is_empty() {
            return Ok(None);
        }

        let mut selected_hashes = HashSet::new();
        let mut selected_count = 0usize;
        let budget = self.branch_token_budget(top_k, per_literal.len());
        let candidate_limit = self.branch_candidate_limit();
        let mut branch_candidates: Option<BTreeSet<u32>> = None;

        // Seed the branch with one strong gram per literal so concatenated
        // literals still contribute independent evidence before we tighten.
        for candidates in &per_literal {
            let Some(candidate) = next_unused_candidate(candidates, &selected_hashes) else {
                continue;
            };
            let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
            branch_candidates = Some(match branch_candidates.take() {
                Some(existing) => existing.intersection(&docs).copied().collect(),
                None => docs,
            });
            selected_hashes.insert(candidate.token_hash);
            selected_count += 1;

            if branch_candidates.as_ref().is_some_and(BTreeSet::is_empty) {
                return Ok(Some(BTreeSet::new()));
            }
        }

        let mut branch_candidates = branch_candidates;

        while selected_count < budget
            && branch_candidates
                .as_ref()
                .is_some_and(|docs| docs.len() > candidate_limit)
        {
            let current_candidates = branch_candidates
                .as_ref()
                .expect("checked candidate set should exist");
            let mut best_choice: Option<(TokenCandidate, BTreeSet<u32>)> = None;

            for candidates in &per_literal {
                for candidate in candidates {
                    if selected_hashes.contains(&candidate.token_hash) {
                        continue;
                    }

                    let docs = self.read_posting_docs(candidate.token_hash, posting_cache)?;
                    let reduced = current_candidates
                        .intersection(&docs)
                        .copied()
                        .collect::<BTreeSet<_>>();
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
            let docs = self.scan_literal_docs(&literal, literal_scan_cache)?;
            branch_candidates = Some(match branch_candidates.take() {
                Some(existing) => existing.intersection(&docs).copied().collect(),
                None => docs,
            });
            if branch_candidates.as_ref().is_some_and(BTreeSet::is_empty) {
                return Ok(Some(BTreeSet::new()));
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
    ) -> Result<LiteralCandidates> {
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
                return Ok(LiteralCandidates::Impossible);
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

        if self.tokenizer_mode == crate::config::TokenizerMode::SparseNgram {
            let mut recovery_hashes = Vec::new();
            collect_sparse_recovery_hashes(
                query_literal,
                &self.tokenizer_options,
                &mut recovery_hashes,
            );
            for token_hash in recovery_hashes {
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
                    source: CandidateSource::Recovery,
                    high_freq: entry.is_skipped_high_freq(),
                });
            }
        }

        selected.sort_unstable_by(|left, right| {
            left.selection_rank()
                .cmp(&right.selection_rank())
                .then_with(|| left.doc_freq.cmp(&right.doc_freq))
        });

        if selected.is_empty() {
            Ok(LiteralCandidates::Unavailable)
        } else {
            Ok(LiteralCandidates::Available(selected))
        }
    }

    fn read_posting_docs(
        &self,
        token_hash: u64,
        posting_cache: &mut HashMap<u64, BTreeSet<u32>>,
    ) -> Result<BTreeSet<u32>> {
        if let Some(cached) = posting_cache.get(&token_hash) {
            return Ok(cached.clone());
        }

        let docs = match self.lookup.find(token_hash) {
            Some(entry) if entry.offset != u64::MAX => {
                read_posting_list(&self.postings_path, entry.offset)?
                    .into_iter()
                    .collect::<BTreeSet<_>>()
            }
            None => BTreeSet::new(),
            Some(_) => BTreeSet::new(),
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
        literal_scan_cache: &mut HashMap<String, BTreeSet<u32>>,
    ) -> Result<BTreeSet<u32>> {
        if let Some(cached) = literal_scan_cache.get(literal) {
            return Ok(cached.clone());
        }

        let mut docs = BTreeSet::new();
        for doc in &self.docs {
            let path = PathBuf::from(&doc.path);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let folded = if text.is_ascii() {
                text.to_ascii_lowercase()
            } else {
                text.to_lowercase()
            };
            if folded.contains(literal) {
                docs.insert(doc.doc_id);
            }
        }

        literal_scan_cache.insert(literal.to_string(), docs.clone());
        Ok(docs)
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
        let freq_rank = if self.high_freq { 1 } else { 0 };
        (freq_rank, self.doc_freq, self.source.rank())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateSource {
    Covering,
    Fallback,
    Recovery,
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
            Self::Recovery => 2,
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

fn fold_query_literal(literal: &str) -> String {
    if literal.is_ascii() {
        literal.to_ascii_lowercase()
    } else {
        literal.to_lowercase()
    }
}

fn collect_sparse_recovery_hashes(literal: &str, options: &TokenizerOptions, out: &mut Vec<u64>) {
    let mut char_offsets = literal
        .char_indices()
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    char_offsets.push(literal.len());
    let char_len = char_offsets.len().saturating_sub(1);
    if char_len < options.min_sparse_len {
        return;
    }

    let bytes = literal.as_bytes();
    for start_char in 0..char_len {
        let min_end = start_char + options.min_sparse_len;
        if min_end > char_len {
            break;
        }
        let max_end = (start_char + options.max_sparse_len).min(char_len);
        for end_char in min_end..=max_end {
            let start_byte = char_offsets[start_char];
            let end_byte = char_offsets[end_char];
            out.push(u64::from(crc32fast::hash(&bytes[start_byte..end_byte])));
        }
    }

    out.sort_unstable();
    out.dedup();
}

fn collect_matches(text: &str, regex: &regex::Regex) -> Vec<FileMatch> {
    let line_starts = build_line_starts(text);
    regex
        .find_iter(text)
        .map(|matched| {
            let location = offset_to_location(matched.start(), &line_starts, text.len());
            let line_text =
                text[location.line_start_offset..location.line_end_offset].trim_end_matches('\n');
            FileMatch {
                location: MatchLocation {
                    line: location.line,
                    column: location.column,
                },
                snippet: line_text.to_string(),
                matched_text: matched.as_str().to_string(),
            }
        })
        .collect()
}

#[derive(Debug, Default)]
struct SearchProfile {
    enabled: bool,
}

impl SearchProfile {
    fn enabled() -> Self {
        Self {
            enabled: env::var_os("BITFUN_PROFILE_SEARCH").is_some(),
            ..Self::default()
        }
    }

    fn record_plan(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile plan_secs={:.6}", duration.as_secs_f64());
        }
    }

    fn record_candidates(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile candidate_secs={:.6}", duration.as_secs_f64());
        }
    }

    fn record_regex_compile(&self, duration: Duration) {
        if self.enabled {
            eprintln!("profile regex_compile_secs={:.6}", duration.as_secs_f64());
        }
    }

    fn record_verify(&self, read_duration: Duration, match_duration: Duration) {
        if self.enabled {
            eprintln!(
                "profile verify_read_secs={:.6} verify_match_secs={:.6}",
                read_duration.as_secs_f64(),
                match_duration.as_secs_f64()
            );
        }
    }

    fn finish(&self, total_duration: Duration, candidate_docs: usize, match_count: usize) {
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

struct OffsetLocation {
    line: usize,
    column: usize,
    line_start_offset: usize,
    line_end_offset: usize,
}

fn build_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn offset_to_location(offset: usize, line_starts: &[usize], text_len: usize) -> OffsetLocation {
    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts[line_index];
    let line_end = line_starts.get(line_index + 1).copied().unwrap_or(text_len);

    OffsetLocation {
        line: line_index + 1,
        column: offset.saturating_sub(line_start) + 1,
        line_start_offset: line_start,
        line_end_offset: line_end,
    }
}
