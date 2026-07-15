use super::*;

pub(super) fn query_request_to_runtime_config(query: &QueryRequest) -> QueryConfig {
    query.to_config()
}

pub(super) fn matches_filters(doc: &IndexDocument, query: &QueryRequest) -> bool {
    if !matches_key_filters(&doc.key, query) {
        return false;
    }
    if !query.filters.languages.is_empty()
        && !query
            .filters
            .languages
            .iter()
            .any(|language| doc.language.as_deref() == Some(language.as_str()))
    {
        return false;
    }
    true
}

pub(super) fn document_matches_query_plan(
    doc: &IndexDocument,
    plan: &QueryPlan,
    tokenizer: &PostingTokenizerConfig,
) -> bool {
    if plan.fallback_to_scan || plan.candidate_branches.is_empty() {
        return true;
    }

    if !document_matches_literal_sequence_plan(doc, plan) {
        return false;
    }

    let terms = collect_document_posting_terms(&doc.source_bytes, tokenizer)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    plan.candidate_branches.iter().any(|branch| {
        branch.grams.is_empty() || branch.grams.iter().all(|gram| terms.contains(gram))
    })
}

pub(super) fn matches_key_filters(key: &DocKey, query: &QueryRequest) -> bool {
    if query
        .filters
        .repo
        .as_ref()
        .is_some_and(|repo| repo != &key.repo_id)
    {
        return false;
    }
    if !query.filters.path_prefixes.is_empty()
        && !query
            .filters
            .path_prefixes
            .iter()
            .any(|prefix| path_matches_prefix(&key.logical_path, prefix))
    {
        return false;
    }
    query.filters.matches_logical_path(&key.logical_path)
}

pub(super) fn empty_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        matched_paths: Vec::new(),
        line_matches: Vec::new(),
        line_match_files: Vec::new(),
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
    }
}

pub(super) fn document_matches_literal_sequence_plan(
    doc: &IndexDocument,
    plan: &QueryPlan,
) -> bool {
    let requires_case_fold = plan.request.case_insensitive;
    let text = String::from_utf8_lossy(&doc.source_bytes);
    let folded_storage;
    let haystack = if requires_case_fold {
        folded_storage = if text.is_ascii() {
            text.to_ascii_lowercase()
        } else {
            text.to_lowercase()
        };
        folded_storage.as_str()
    } else {
        text.as_ref()
    };

    plan.candidate_branches.iter().any(|branch| {
        branch.ordered_literals.is_empty()
            || literals_match_in_order(haystack, &branch.ordered_literals)
    })
}

fn literals_match_in_order(haystack: &str, literals: &[String]) -> bool {
    let mut search_from = 0usize;
    for literal in literals {
        let Some(offset) = haystack[search_from..].find(literal) else {
            return false;
        };
        search_from += offset + literal.len();
    }
    true
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(std::path::MAIN_SEPARATOR))
}
