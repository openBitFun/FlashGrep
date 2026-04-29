use std::{collections::HashSet, path::Path};

use crate::{path_filter::PathFilter, search::SearchResults, QueryConfig};

pub(super) fn query_with_remaining_global_limit(
    query: &QueryConfig,
    current_results: &SearchResults,
) -> Option<QueryConfig> {
    let Some(limit) = query.effective_global_max_results() else {
        return Some(query.clone());
    };
    let consumed = current_results.result_units(query.search_mode);
    if consumed >= limit {
        return None;
    }

    let mut next_query = query.clone();
    next_query.global_max_results = Some(limit - consumed);
    Some(next_query)
}

pub(super) fn merge_search_results(
    mut base: SearchResults,
    mut overlay: SearchResults,
) -> SearchResults {
    let overlay_paths = search_result_paths(&overlay);
    let base_paths = search_result_paths(&base);
    let overlapping_paths = base_paths.intersection(&overlay_paths).count();

    base.candidate_docs = base
        .candidate_docs
        .saturating_add(overlay.candidate_docs)
        .saturating_sub(overlapping_paths);
    base.bytes_searched += overlay.bytes_searched;
    base.matched_lines += overlay.matched_lines;
    base.matched_occurrences += overlay.matched_occurrences;
    base.hits
        .retain(|hit| !overlay_paths.contains(hit.path.as_str()));
    base.file_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.file_match_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.file_counts.append(&mut overlay.file_counts);
    base.file_match_counts
        .append(&mut overlay.file_match_counts);
    base.hits.append(&mut overlay.hits);
    base.hits
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    base.file_counts
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    base.file_match_counts
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    recompute_result_totals(&mut base);
    base
}

pub(super) fn empty_search_results() -> SearchResults {
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

pub(super) fn filter_search_results(results: &mut SearchResults, filter: &PathFilter) {
    results
        .hits
        .retain(|hit| filter.matches_file(Path::new(&hit.path)));
    results
        .file_counts
        .retain(|count| filter.matches_file(Path::new(&count.path)));
    results
        .file_match_counts
        .retain(|count| filter.matches_file(Path::new(&count.path)));

    results.candidate_docs = unique_result_paths(results).len();
    results.bytes_searched = 0;
    recompute_result_totals(results);
}

pub(super) fn exclude_search_results(
    results: &mut SearchResults,
    excluded_paths: &HashSet<String>,
) {
    results
        .hits
        .retain(|hit| !excluded_paths.contains(&hit.path));
    results
        .file_counts
        .retain(|count| !excluded_paths.contains(&count.path));
    results
        .file_match_counts
        .retain(|count| !excluded_paths.contains(&count.path));

    results.candidate_docs = unique_result_paths(results).len();
    recompute_result_totals(results);
}

pub(super) fn absolutize_search_results(results: &mut SearchResults, repo_root: &Path) {
    for hit in &mut results.hits {
        hit.path = absolutize_path(&hit.path, repo_root);
    }
    for count in &mut results.file_counts {
        count.path = absolutize_path(&count.path, repo_root);
    }
    for count in &mut results.file_match_counts {
        count.path = absolutize_path(&count.path, repo_root);
    }
}

pub(super) fn absolutize_path(path: &str, repo_root: &Path) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_string_lossy().into_owned()
    } else {
        repo_root.join(path).to_string_lossy().into_owned()
    }
}

fn search_result_paths(results: &SearchResults) -> HashSet<&str> {
    results
        .hits
        .iter()
        .map(|hit| hit.path.as_str())
        .chain(results.file_counts.iter().map(|count| count.path.as_str()))
        .chain(
            results
                .file_match_counts
                .iter()
                .map(|count| count.path.as_str()),
        )
        .collect()
}

fn unique_result_paths(results: &SearchResults) -> HashSet<&str> {
    search_result_paths(results)
}

fn recompute_result_totals(results: &mut SearchResults) {
    let matched_lines_from_hits = results
        .hits
        .iter()
        .map(|hit| hit.matches.len())
        .sum::<usize>();
    let matched_occurrences_from_hits = results
        .hits
        .iter()
        .map(|hit| hit.matches.len())
        .sum::<usize>();

    results.searches_with_match = if !results.hits.is_empty() {
        results.hits.len()
    } else if !results.file_counts.is_empty() {
        results.file_counts.len()
    } else {
        results.file_match_counts.len()
    };
    results.matched_lines = if !results.file_counts.is_empty() {
        results
            .file_counts
            .iter()
            .map(|count| count.matched_lines)
            .sum()
    } else if !results.hits.is_empty() {
        matched_lines_from_hits
    } else if !results.file_match_counts.is_empty() {
        results.matched_lines
    } else {
        0
    };
    results.matched_occurrences = if !results.file_match_counts.is_empty() {
        results
            .file_match_counts
            .iter()
            .map(|count| count.matched_occurrences)
            .sum()
    } else if !results.hits.is_empty() {
        matched_occurrences_from_hits
    } else {
        0
    };
}
