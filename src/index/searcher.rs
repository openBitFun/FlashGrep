use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use crate::{
    config::QueryConfig,
    error::Result,
    index::format::{read_docs_file, IndexLayout, LookupTable, PostingsData},
    planner::{plan, QueryBranch},
    search::{SearchHit, SearchMode, SearchResults},
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
mod verify;

use count::VerifyPlan as CountVerifyPlan;
use multi_literal::{
    collect_candidates as collect_multi_literal_candidates, prepare as prepare_multi_literal,
    search as search_multi_literal,
};
use query::{fold_query_literal, LiteralPrefilter, SearchPreparation};
use shared::doc_by_id;
use verify::{build_matcher, requires_multiline_verification, verify_candidates, SearchProfile};

pub struct IndexSearcher {
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
        let layout = IndexLayout::new(&index_path);
        let (metadata, docs) = read_docs_file(&layout.docs_path)?;
        let lookup = LookupTable::open(&layout.lookup_path)?;
        let postings = PostingsData::open(&layout.postings_path)?;

        Ok(Self {
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
        let profile = SearchProfile::enabled();
        let total_started = Instant::now();
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());
        let use_rg_verify = rg::enabled(config);

        if let Some(multi_literal) =
            self.prepare_multi_literal_search(&plan, config.case_insensitive, &profile)?
        {
            let candidate_ids = &multi_literal.candidate_ids;
            if use_rg_verify {
                let match_started = Instant::now();
                let matched_lines = rg::count_matches(config, &self.docs, candidate_ids)?;
                profile.record_verify(match_started.elapsed());
                profile.finish(total_started.elapsed(), candidate_ids.len(), matched_lines);
                return Ok(SearchResults {
                    candidate_docs: candidate_ids.len(),
                    matched_lines,
                    hits: Vec::new(),
                });
            }
            let match_started = Instant::now();
            let results = if matches!(config.search_mode, SearchMode::CountOnly) {
                let regex_started = Instant::now();
                let verify_plan = CountVerifyPlan::compile_searcher_line(config, None)?;
                profile.record_regex_compile(regex_started.elapsed());
                let stats = verify_plan.verify_candidate_count_stats(candidate_ids, &self.docs)?;
                SearchResults {
                    candidate_docs: stats.matched_docs,
                    matched_lines: stats.matched_lines,
                    hits: Vec::new(),
                }
            } else {
                search_multi_literal(
                    &self.docs,
                    Some(candidate_ids),
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

        let preparation = self.prepare_indexed_search(config, &plan, &profile)?;
        let candidate_doc_count = preparation.candidate_ids.len();

        if use_rg_verify {
            let match_started = Instant::now();
            let matched_lines = rg::count_matches(config, &self.docs, &preparation.candidate_ids)?;
            profile.record_verify(match_started.elapsed());
            profile.finish(total_started.elapsed(), candidate_doc_count, matched_lines);
            return Ok(SearchResults {
                candidate_docs: candidate_doc_count,
                matched_lines,
                hits: Vec::new(),
            });
        }

        let multiline_verifier = requires_multiline_verification(config)?;

        let match_started = Instant::now();
        let (matched_lines, hits) = match config.search_mode {
            SearchMode::CountOnly => {
                let regex_started = Instant::now();
                let verify_plan = CountVerifyPlan::compile(
                    config,
                    multiline_verifier,
                    preparation.literal_prefilter.clone(),
                )?;
                profile.record_regex_compile(regex_started.elapsed());
                (
                    verify_plan
                        .verify_candidate_match_counts(&preparation.candidate_ids, &self.docs)?,
                    Vec::new(),
                )
            }
            _ => {
                let regex_started = Instant::now();
                let matcher = build_matcher(config, multiline_verifier)?;
                profile.record_regex_compile(regex_started.elapsed());
                let mut hits = Vec::new();
                let mut matched_lines = 0usize;
                let outcomes = verify_candidates(
                    &preparation.candidate_ids,
                    &self.docs,
                    &matcher,
                    multiline_verifier,
                    config.search_mode,
                )?;
                for task in outcomes {
                    matched_lines += task.outcome.matched_lines;
                    if !task.outcome.matches.is_empty() {
                        let doc = doc_by_id(&self.docs, task.doc_id)?;
                        hits.push(SearchHit {
                            path: doc.path.clone(),
                            matches: task.outcome.matches,
                        });
                    }
                }
                (matched_lines, hits)
            }
        };
        profile.record_verify(match_started.elapsed());
        profile.finish(total_started.elapsed(), candidate_doc_count, matched_lines);

        Ok(SearchResults {
            candidate_docs: candidate_doc_count,
            matched_lines,
            hits,
        })
    }

    pub fn candidate_paths(&self, config: &QueryConfig) -> Result<Vec<String>> {
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

        candidate_ids
            .into_iter()
            .map(|doc_id| Ok(doc_by_id(&self.docs, doc_id)?.path.clone()))
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
