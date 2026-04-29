use std::{collections::BTreeSet, collections::HashSet};

use crate::{
    config::BuildConfig,
    error::Result,
    index_engine::{
        query::{QueryFilters, QueryRequest},
        reader::IndexSnapshotReader,
        WorkspaceSearchView,
    },
    path_filter::PathFilter,
    search::SearchResults,
};

pub(super) fn search_workspace_view(
    search_view: &WorkspaceSearchView,
    base_snapshot: &IndexSnapshotReader,
    overlay_snapshot: Option<&IndexSnapshotReader>,
    query: &crate::QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<SearchResults> {
    let workspace_snapshot = search_view
        .open_workspace_query_snapshot(Some(base_snapshot.clone()), overlay_snapshot.cloned())?;
    workspace_snapshot.search(search_view, query, &QueryFilters::default(), filter, true)
}

pub(super) fn workspace_visible_paths(
    build_config: &BuildConfig,
    base_snapshot: &IndexSnapshotReader,
    overlay_snapshot: Option<&IndexSnapshotReader>,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    let shadowed_base_paths = shadowed_base_paths(overlay_snapshot)?;
    let mut paths = BTreeSet::new();

    for document in base_snapshot.visible_documents()?.into_values() {
        if shadowed_base_paths.contains(&document.logical_path) {
            continue;
        }
        push_visible_path(&mut paths, build_config, &document.logical_path, filter);
    }

    if let Some(overlay_snapshot) = overlay_snapshot {
        for document in overlay_snapshot.visible_documents()?.into_values() {
            push_visible_path(&mut paths, build_config, &document.logical_path, filter);
        }
    }

    Ok(paths.into_iter().collect())
}

pub(super) fn workspace_candidate_paths(
    build_config: &BuildConfig,
    query: &QueryRequest,
    base_snapshot: &IndexSnapshotReader,
    overlay_snapshot: Option<&IndexSnapshotReader>,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    let shadowed_base_paths = shadowed_base_paths(overlay_snapshot)?;
    let mut paths = BTreeSet::new();

    for logical_path in base_snapshot.candidate_paths(query)? {
        if shadowed_base_paths.contains(&logical_path) {
            continue;
        }
        push_visible_path(&mut paths, build_config, &logical_path, filter);
    }

    if let Some(overlay_snapshot) = overlay_snapshot {
        for logical_path in overlay_snapshot.candidate_paths(query)? {
            push_visible_path(&mut paths, build_config, &logical_path, filter);
        }
    }

    Ok(paths.into_iter().collect())
}

pub(crate) fn workspace_query_request(query: &crate::QueryConfig) -> QueryRequest {
    QueryRequest {
        pattern: query.regex_pattern.clone(),
        mode: query.search_mode,
        case_insensitive: query.case_insensitive,
        top_k: query.global_max_results,
        filters: QueryFilters::default(),
        prefer_worktree_for_git_head_blob: false,
    }
}

pub(super) fn query_with_remaining_global_limit(
    query: &crate::QueryConfig,
    current_results: &SearchResults,
) -> Option<crate::QueryConfig> {
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

fn shadowed_base_paths(overlay_snapshot: Option<&IndexSnapshotReader>) -> Result<HashSet<String>> {
    Ok(overlay_snapshot
        .map(IndexSnapshotReader::visible_keys)
        .transpose()?
        .map(|keys| {
            keys.live
                .into_iter()
                .chain(keys.deleted)
                .map(|key| key.logical_path)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default())
}

fn push_visible_path(
    paths: &mut BTreeSet<String>,
    build_config: &BuildConfig,
    logical_path: &str,
    filter: Option<&PathFilter>,
) {
    let absolute = build_config.repo_path.join(logical_path);
    if filter.is_some_and(|filter| !filter.matches_file(&absolute)) {
        return;
    }
    paths.insert(absolute.to_string_lossy().into_owned());
}
