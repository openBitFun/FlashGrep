use std::path::{Path, PathBuf};

use crate::{
    config::QueryConfig,
    index_engine::types::{FieldId, RepoId},
    index_engine::{config::PostingTokenizerConfig, segment::collect_query_posting_terms},
    path_filter::PathFilter,
    search::SearchMode,
};

#[derive(Debug, Clone, Default)]
pub struct QueryFilters {
    pub repo: Option<RepoId>,
    pub path_prefixes: Vec<String>,
    pub languages: Vec<String>,
    pub fields: Vec<FieldId>,
    path_filter: Option<QueryPathFilter>,
}

#[derive(Debug, Clone)]
struct QueryPathFilter {
    repo_root: PathBuf,
    filter: PathFilter,
}

impl QueryFilters {
    pub(crate) fn set_path_filter(&mut self, repo_root: PathBuf, filter: PathFilter) {
        self.path_filter = Some(QueryPathFilter { repo_root, filter });
    }

    pub(crate) fn matches_logical_path(&self, logical_path: &str) -> bool {
        self.path_filter.as_ref().is_none_or(|path_filter| {
            path_filter
                .filter
                .matches_file(&path_filter.repo_root.join(Path::new(logical_path)))
        })
    }
}

#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub pattern: String,
    pub patterns: Vec<String>,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub dot_matches_new_line: bool,
    pub fixed_strings: bool,
    pub word_regexp: bool,
    pub line_regexp: bool,
    pub top_k_tokens: usize,
    pub max_count: Option<usize>,
    pub top_k: Option<usize>,
    pub mode: SearchMode,
    pub filters: QueryFilters,
    pub prefer_worktree_for_git_head_blob: bool,
}

impl QueryRequest {
    pub fn from_config(
        config: &QueryConfig,
        filters: QueryFilters,
        prefer_worktree_for_git_head_blob: bool,
    ) -> Self {
        Self {
            pattern: config.regex_pattern.clone(),
            patterns: config.patterns.clone(),
            case_insensitive: config.case_insensitive,
            multiline: config.multiline,
            dot_matches_new_line: config.dot_matches_new_line,
            fixed_strings: config.fixed_strings,
            word_regexp: config.word_regexp,
            line_regexp: config.line_regexp,
            top_k_tokens: config.top_k_tokens,
            max_count: config.max_count,
            top_k: config.global_max_results,
            mode: config.search_mode,
            filters,
            prefer_worktree_for_git_head_blob,
        }
    }

    pub(crate) fn to_config(&self) -> QueryConfig {
        QueryConfig {
            regex_pattern: self.pattern.clone(),
            patterns: self.patterns.clone(),
            case_insensitive: self.case_insensitive,
            multiline: self.multiline,
            dot_matches_new_line: self.dot_matches_new_line,
            fixed_strings: self.fixed_strings,
            word_regexp: self.word_regexp,
            line_regexp: self.line_regexp,
            top_k_tokens: self.top_k_tokens,
            max_count: self.max_count,
            global_max_results: self.top_k,
            search_mode: self.mode,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub request: QueryRequest,
    pub candidate_branches: Vec<CandidateBranch>,
    pub fallback_to_scan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateBranch {
    pub grams: Vec<u64>,
    pub ordered_literals: Vec<String>,
}

impl QueryPlan {
    pub fn build(request: QueryRequest, tokenizer: &PostingTokenizerConfig) -> Self {
        let (candidate_branches, fallback_to_scan) =
            build_candidate_branches(&request.pattern, tokenizer, request.case_insensitive);
        Self {
            request,
            candidate_branches,
            fallback_to_scan,
        }
    }
}

fn build_candidate_branches(
    pattern: &str,
    tokenizer: &PostingTokenizerConfig,
    case_insensitive: bool,
) -> (Vec<CandidateBranch>, bool) {
    let regex_plan = match crate::planner::plan(pattern) {
        Ok(plan) => plan,
        Err(_) => return (Vec::new(), true),
    };
    if regex_plan.fallback_to_scan {
        return (Vec::new(), true);
    }

    let candidate_branches = regex_plan
        .branches
        .into_iter()
        .map(|branch| CandidateBranch {
            grams: collect_branch_grams(&branch.literals, tokenizer),
            ordered_literals: collect_branch_literals(&branch.literals, case_insensitive),
        })
        .collect::<Vec<_>>();

    (candidate_branches, false)
}

fn collect_branch_grams(literals: &[String], tokenizer: &PostingTokenizerConfig) -> Vec<u64> {
    let mut grams = literals
        .iter()
        .flat_map(|literal| collect_query_posting_terms(literal, tokenizer))
        .collect::<Vec<_>>();
    grams.sort_unstable();
    grams.dedup();
    grams
}

fn collect_branch_literals(literals: &[String], case_insensitive: bool) -> Vec<String> {
    literals
        .iter()
        .map(|literal| {
            if case_insensitive {
                if literal.is_ascii() {
                    literal.to_ascii_lowercase()
                } else {
                    literal.to_lowercase()
                }
            } else {
                literal.clone()
            }
        })
        .filter(|literal| !literal.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{QueryFilters, QueryPlan, QueryRequest};
    use crate::{
        index_engine::config::PostingTokenizerConfig, search::SearchMode, QueryConfig,
        TokenizerMode,
    };

    fn request(pattern: &str) -> QueryRequest {
        QueryRequest::from_config(
            &QueryConfig {
                regex_pattern: pattern.into(),
                search_mode: SearchMode::CountOnly,
                ..QueryConfig::default()
            },
            QueryFilters::default(),
            false,
        )
    }

    #[test]
    fn query_request_round_trips_match_semantics() {
        let config = QueryConfig {
            regex_pattern: "escaped".into(),
            patterns: vec!["raw".into(), "second".into()],
            case_insensitive: true,
            multiline: true,
            dot_matches_new_line: true,
            fixed_strings: true,
            word_regexp: true,
            line_regexp: true,
            top_k_tokens: 9,
            max_count: Some(3),
            global_max_results: Some(7),
            search_mode: SearchMode::CountMatches,
        };

        let restored =
            QueryRequest::from_config(&config, QueryFilters::default(), true).to_config();

        assert_eq!(restored.regex_pattern, config.regex_pattern);
        assert_eq!(restored.patterns, config.patterns);
        assert_eq!(restored.case_insensitive, config.case_insensitive);
        assert_eq!(restored.multiline, config.multiline);
        assert_eq!(restored.dot_matches_new_line, config.dot_matches_new_line);
        assert_eq!(restored.fixed_strings, config.fixed_strings);
        assert_eq!(restored.word_regexp, config.word_regexp);
        assert_eq!(restored.line_regexp, config.line_regexp);
        assert_eq!(restored.top_k_tokens, config.top_k_tokens);
        assert_eq!(restored.max_count, config.max_count);
        assert_eq!(restored.global_max_results, config.global_max_results);
        assert_eq!(restored.search_mode, config.search_mode);
    }

    fn trigram_tokenizer() -> PostingTokenizerConfig {
        PostingTokenizerConfig {
            mode: TokenizerMode::Trigram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    fn sparse_tokenizer() -> PostingTokenizerConfig {
        PostingTokenizerConfig {
            mode: TokenizerMode::SparseNgram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    #[test]
    fn alternation_keeps_branch_specific_candidate_grams() {
        let plan = QueryPlan::build(request("DenseMap|SmallVector"), &trigram_tokenizer());
        assert!(!plan.fallback_to_scan);
        assert_eq!(plan.candidate_branches.len(), 2);
        assert!(plan.candidate_branches[0].grams.contains(&0x0070616d));
        assert!(plan.candidate_branches[1].grams.contains(&0x00636576));
    }

    #[test]
    fn short_literal_branch_stays_unpruned_without_forcing_scan() {
        let plan = QueryPlan::build(request("ab|longliteral"), &trigram_tokenizer());
        assert!(!plan.fallback_to_scan);
        assert_eq!(plan.candidate_branches[0].grams, Vec::<u64>::new());
        assert!(!plan.candidate_branches[1].grams.is_empty());
    }

    #[test]
    fn punctuated_literal_contributes_posting_grams() {
        let plan = QueryPlan::build(request("PreservedAnalyses::all"), &trigram_tokenizer());
        assert_eq!(plan.candidate_branches.len(), 1);
        assert!(plan.candidate_branches[0].grams.contains(&0x00613a3a));
    }

    #[test]
    fn sparse_query_uses_covering_tokens() {
        let plan = QueryPlan::build(request("getUserById"), &sparse_tokenizer());
        assert!(!plan.fallback_to_scan);
        assert_eq!(plan.candidate_branches.len(), 1);
        assert!(!plan.candidate_branches[0].grams.is_empty());
        assert!(plan.candidate_branches[0].grams.len() < "getUserById".len().saturating_sub(2));
    }

    #[test]
    fn ordered_literals_preserve_branch_sequence() {
        let plan = QueryPlan::build(request("foo.*bar|baz.*qux"), &trigram_tokenizer());
        assert!(!plan.fallback_to_scan);
        assert_eq!(plan.candidate_branches.len(), 2);
        assert_eq!(
            plan.candidate_branches[0].ordered_literals,
            vec!["foo", "bar"]
        );
        assert_eq!(
            plan.candidate_branches[1].ordered_literals,
            vec!["baz", "qux"]
        );
    }

    #[test]
    fn ordered_literals_fold_case_for_case_insensitive_queries() {
        let mut request = request("Foo.*BAR");
        request.case_insensitive = true;
        let plan = QueryPlan::build(request, &trigram_tokenizer());
        assert!(!plan.fallback_to_scan);
        assert_eq!(plan.candidate_branches.len(), 1);
        assert_eq!(
            plan.candidate_branches[0].ordered_literals,
            vec!["foo", "bar"]
        );
    }
}
