use crate::{
    index_engine::types::{FieldId, RepoId},
    index_engine::{config::PostingTokenizerConfig, segment::collect_query_posting_terms},
    search::SearchMode,
};

#[derive(Debug, Clone, Default)]
pub struct QueryFilters {
    pub repo: Option<RepoId>,
    pub path_prefixes: Vec<String>,
    pub languages: Vec<String>,
    pub fields: Vec<FieldId>,
}

#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub pattern: String,
    pub mode: SearchMode,
    pub case_insensitive: bool,
    pub top_k: Option<usize>,
    pub filters: QueryFilters,
    pub prefer_worktree_for_git_head_blob: bool,
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
    use crate::{index_engine::config::PostingTokenizerConfig, search::SearchMode, TokenizerMode};

    fn request(pattern: &str) -> QueryRequest {
        QueryRequest {
            pattern: pattern.into(),
            mode: SearchMode::CountOnly,
            case_insensitive: false,
            top_k: None,
            filters: QueryFilters::default(),
            prefer_worktree_for_git_head_blob: false,
        }
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
