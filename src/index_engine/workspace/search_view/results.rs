use std::{collections::HashSet, path::Path};

use crate::{search::SearchResults, QueryConfig};

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
    base.matched_paths
        .retain(|path| !overlay_paths.contains(path.as_str()));
    base.line_matches
        .retain(|matched| !overlay_paths.contains(matched.path.as_str()));
    base.line_match_files
        .retain(|file| !overlay_paths.contains(file.path.as_str()));
    base.file_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.file_match_counts
        .retain(|count| !overlay_paths.contains(count.path.as_str()));
    base.matched_paths.append(&mut overlay.matched_paths);
    base.line_matches.append(&mut overlay.line_matches);
    base.line_match_files.append(&mut overlay.line_match_files);
    base.file_counts.append(&mut overlay.file_counts);
    base.file_match_counts
        .append(&mut overlay.file_match_counts);
    base.matched_paths.sort_unstable();
    base.line_matches.sort_unstable_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line_number.cmp(&right.line_number))
    });
    base.line_match_files
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
        matched_paths: Vec::new(),
        line_matches: Vec::new(),
        line_match_files: Vec::new(),
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
    }
}

pub(super) fn exclude_search_results(
    results: &mut SearchResults,
    excluded_paths: &HashSet<String>,
) {
    results
        .matched_paths
        .retain(|path| !excluded_paths.contains(path));
    results
        .line_matches
        .retain(|matched| !excluded_paths.contains(&matched.path));
    results
        .line_match_files
        .retain(|file| !excluded_paths.contains(&file.path));
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
    for path in &mut results.matched_paths {
        *path = absolutize_path(path, repo_root);
    }
    for matched in &mut results.line_matches {
        matched.path = absolutize_path(&matched.path, repo_root);
    }
    for file in &mut results.line_match_files {
        file.path = absolutize_path(&file.path, repo_root);
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
        .matched_paths
        .iter()
        .map(String::as_str)
        .chain(
            results
                .line_matches
                .iter()
                .map(|matched| matched.path.as_str()),
        )
        .chain(
            results
                .line_match_files
                .iter()
                .map(|file| file.path.as_str()),
        )
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
    results.searches_with_match = if !results.matched_paths.is_empty() {
        results.matched_paths.len()
    } else if !results.line_matches.is_empty() {
        results
            .line_matches
            .iter()
            .map(|matched| matched.path.as_str())
            .collect::<HashSet<_>>()
            .len()
    } else if !results.line_match_files.is_empty() {
        results.line_match_files.len()
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
    } else if !results.file_match_counts.is_empty() {
        results.matched_lines
    } else if !results.line_matches.is_empty() {
        results.line_matches.len()
    } else if !results.line_match_files.is_empty() {
        results
            .line_match_files
            .iter()
            .map(|file| file.line_numbers.len())
            .sum()
    } else {
        0
    };
    results.matched_occurrences = if !results.file_match_counts.is_empty() {
        results
            .file_match_counts
            .iter()
            .map(|count| count.matched_occurrences)
            .sum()
    } else if !results.line_matches.is_empty() || !results.line_match_files.is_empty() {
        results.matched_occurrences
    } else {
        0
    };
}
