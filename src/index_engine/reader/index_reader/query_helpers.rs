use super::*;

pub(super) fn query_request_to_runtime_config(query: &QueryRequest) -> QueryConfig {
    QueryConfig {
        regex_pattern: query.pattern.clone(),
        case_insensitive: query.case_insensitive,
        search_mode: query.mode,
        ..QueryConfig::default()
    }
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
    true
}

pub(super) fn accumulate_doc_search(
    doc: &IndexDocument,
    query: &QueryRequest,
    regex: &regex::Regex,
    results: &mut SearchResults,
) {
    accumulate_search_bytes(&doc.logical_path, &doc.source_bytes, query, regex, results);
}

pub(super) fn empty_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
        hits: Vec::new(),
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

fn accumulate_search_bytes(
    path: &str,
    source_bytes: &[u8],
    query: &QueryRequest,
    regex: &regex::Regex,
    results: &mut SearchResults,
) {
    let text = String::from_utf8_lossy(source_bytes);
    let mut doc_matched_lines = 0usize;
    let mut doc_matched_occurrences = 0usize;
    let mut doc_matches = Vec::new();
    let materializes_matches = query.mode.materializes_matches();
    let first_hit_only = matches!(query.mode, crate::search::SearchMode::FirstHitOnly);

    for (line_index, line) in text.lines().enumerate() {
        if materializes_matches {
            let matches = regex.find_iter(line).collect::<Vec<_>>();
            if matches.is_empty() {
                continue;
            }

            doc_matched_lines += 1;
            doc_matched_occurrences += matches.len();
            for matched in matches {
                doc_matches.push(FileMatch {
                    location: MatchLocation {
                        line: line_index + 1,
                        column: matched.start() + 1,
                    },
                    snippet: line.to_string(),
                    matched_text: matched.as_str().to_string(),
                });
                if first_hit_only {
                    break;
                }
            }
            if first_hit_only && !doc_matches.is_empty() {
                break;
            }
            continue;
        }

        let match_count = regex.find_iter(line).count();
        if match_count == 0 {
            continue;
        }

        doc_matched_lines += 1;
        doc_matched_occurrences += match_count;
    }

    if doc_matched_lines == 0 {
        return;
    }

    results.searches_with_match += 1;
    results.matched_lines += doc_matched_lines;
    results.matched_occurrences += doc_matched_occurrences;
    results.file_counts.push(FileCount {
        path: path.to_string(),
        matched_lines: doc_matched_lines,
    });
    results.file_match_counts.push(FileMatchCount {
        path: path.to_string(),
        matched_occurrences: doc_matched_occurrences,
    });

    if materializes_matches {
        let lines = doc_matches
            .iter()
            .cloned()
            .map(SearchLine::Match)
            .collect::<Vec<_>>();
        results.hits.push(SearchHit {
            path: path.to_string(),
            matches: doc_matches,
            lines,
        });
    }
}
