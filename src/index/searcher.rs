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

#[path = "searcher_query.rs"]
mod query;
#[path = "searcher_verify.rs"]
mod verify;

use query::{fold_query_literal, LiteralPrefilter, SearchPreparation};
use verify::{
    build_matcher, requires_multiline_verification, verify_candidate_match_counts,
    verify_candidates, SearchProfile,
};

pub struct IndexSearcher {
    docs: Vec<crate::index::format::DocMeta>,
    lookup: LookupTable,
    postings: PostingsData,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
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

        let preparation = self.prepare_search(config, &profile)?;
        let candidate_doc_count = preparation.candidate_ids.len();

        let multiline_verifier = requires_multiline_verification(config)?;
        let regex_started = Instant::now();
        let matcher = build_matcher(config, multiline_verifier)?;
        profile.record_regex_compile(regex_started.elapsed());

        let match_started = Instant::now();
        let (matched_lines, hits) = match config.search_mode {
            SearchMode::CountOnly => (
                verify_candidate_match_counts(
                    &preparation.candidate_ids,
                    &self.docs,
                    &matcher,
                    multiline_verifier,
                    preparation.literal_prefilter.as_ref(),
                )?,
                Vec::new(),
            ),
            _ => {
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
                        hits.push(SearchHit {
                            path: self.docs[task.doc_id as usize].path.clone(),
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
        let preparation = self.prepare_search(config, &profile)?;
        Ok(preparation
            .candidate_ids
            .into_iter()
            .map(|doc_id| self.docs[doc_id as usize].path.clone())
            .collect())
    }

    fn prepare_search(
        &self,
        config: &QueryConfig,
        profile: &SearchProfile,
    ) -> Result<SearchPreparation> {
        let plan_started = Instant::now();
        let plan = plan(&config.regex_pattern)?;
        profile.record_plan(plan_started.elapsed());

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
            literal_prefilter: LiteralPrefilter::new(&plan, config.case_insensitive),
        })
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
                self.collect_literal_candidates(tokenizer, &query_literal, covering_hashes)?;
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

        let mut branch_candidates = branch_candidates;

        while selected_count < budget
            && branch_candidates
                .as_ref()
                .is_some_and(|docs| docs.len() > candidate_limit)
        {
            let current_candidates = branch_candidates
                .as_ref()
                .expect("checked candidate set should exist");
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
            let docs = self.scan_literal_docs(&literal, literal_scan_cache)?;
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

        selected.sort_unstable_by(|left, right| {
            left.selection_rank()
                .cmp(&right.selection_rank())
                .then_with(|| left.doc_freq.cmp(&right.doc_freq))
        });
        prune_high_frequency_candidates(&mut selected);

        if selected.is_empty() {
            Ok(LiteralCandidates::Unavailable)
        } else {
            Ok(LiteralCandidates::Available(selected))
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
            None => Vec::new(),
            Some(_) => Vec::new(),
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
    ) -> Result<Vec<u32>> {
        if let Some(cached) = literal_scan_cache.get(literal) {
            return Ok(cached.clone());
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
        Ok(docs)
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
        let freq_rank = if self.high_freq { 1 } else { 0 };
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
            },
            false,
        )
        .unwrap();

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
            },
            false,
        )
        .unwrap();

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
            },
            true,
        )
        .unwrap();

        assert!(prefilter.text_may_match("prefix pm ... resume suffix"));
        assert!(prefilter.text_may_match("prefix Pm ... Resume suffix"));
        assert!(!prefilter.text_may_match("prefix pm only suffix"));
    }

    #[test]
    fn literal_prefilter_is_disabled_when_query_falls_back_to_scan() {
        assert!(LiteralPrefilter::new(
            &QueryPlan {
                branches: vec![QueryBranch {
                    literals: Vec::new()
                }],
                fallback_to_scan: true,
            },
            false,
        )
        .is_none());
    }
}
