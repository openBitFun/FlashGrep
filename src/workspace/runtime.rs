use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    time::UNIX_EPOCH,
};

use crate::{
    config::BuildConfig,
    error::Result,
    files::{is_corpus_text_file, is_text_file, scan_paths, scan_repository, ScanOptions},
    index_engine::{reader::SnapshotPathCatalog, workspace::WorkspaceOverlaySnapshotState, RepoId},
    path_filter::PathFilter,
    search::SearchResults,
    search_engine::{SearchDocument, SearchDocumentSource},
    worktree_diff::{diff_against_snapshot_catalog, IndexWorktreeDiff},
};

pub(super) fn scan_text_repository_files(
    build_config: &BuildConfig,
    filter: Option<&PathFilter>,
) -> Result<Vec<SearchDocument>> {
    let mut files = Vec::new();
    let scanned = if filter.is_some() {
        scan_paths(
            std::slice::from_ref(&build_config.repo_path),
            Some(&build_config.storage_root),
            ScanOptions {
                respect_ignore: matches!(
                    build_config.corpus_mode,
                    crate::config::CorpusMode::RespectIgnore
                ),
                include_hidden: build_config.include_hidden,
                max_file_size: build_config.max_file_size,
                max_depth: None,
                ignore_files: Vec::new(),
            },
            filter,
        )?
    } else {
        scan_repository(build_config)?
    };
    for file in scanned {
        if !is_text_file(&file.path)? {
            continue;
        }
        files.push(SearchDocument::from_repository_file(&file));
    }
    Ok(files)
}

pub(super) fn build_overlay_snapshot_state(
    repo_id: &RepoId,
    build_config: &BuildConfig,
    diff: &IndexWorktreeDiff,
) -> Result<WorkspaceOverlaySnapshotState> {
    let documents = resolve_dirty_documents(existing_dirty_paths(diff), None)?
        .into_iter()
        .map(|document| (document.logical_path.clone(), document))
        .collect::<HashMap<_, _>>();
    let mut deleted_paths = diff.deleted_files.iter().cloned().collect::<HashSet<_>>();
    for path in &diff.modified_files {
        if !documents.contains_key(path) {
            deleted_paths.insert(path.clone());
        }
    }
    WorkspaceOverlaySnapshotState::from_paths(
        repo_id,
        &build_config.repo_path,
        &documents,
        &deleted_paths,
    )
}

pub(super) fn merge_search_results(
    mut base: SearchResults,
    mut dirty: SearchResults,
) -> SearchResults {
    base.candidate_docs += dirty.candidate_docs;
    base.searches_with_match += dirty.searches_with_match;
    base.bytes_searched += dirty.bytes_searched;
    base.matched_lines += dirty.matched_lines;
    base.matched_occurrences += dirty.matched_occurrences;
    base.matched_paths.append(&mut dirty.matched_paths);
    base.line_matches.append(&mut dirty.line_matches);
    base.line_match_files.append(&mut dirty.line_match_files);
    base.file_counts.append(&mut dirty.file_counts);
    base.file_match_counts.append(&mut dirty.file_match_counts);
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
    base
}

pub(super) fn diff_against_base_snapshot(
    build_config: &BuildConfig,
    catalog: &SnapshotPathCatalog,
) -> Result<IndexWorktreeDiff> {
    diff_against_snapshot_catalog(build_config, catalog)
}

pub(super) fn reconcile_dirty_paths_against_current_diff(
    dirty: &IndexWorktreeDiff,
    current: &IndexWorktreeDiff,
) -> IndexWorktreeDiff {
    let current_modified = current
        .modified_files
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let current_deleted = current
        .deleted_files
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let current_new = current.new_files.iter().cloned().collect::<HashSet<_>>();

    let mut reconciled = IndexWorktreeDiff::default();
    for path in dirty
        .modified_files
        .iter()
        .chain(dirty.deleted_files.iter())
        .chain(dirty.new_files.iter())
    {
        if current_modified.contains(path) {
            reconciled.modified_files.push(path.clone());
        } else if current_deleted.contains(path) {
            reconciled.deleted_files.push(path.clone());
        } else if current_new.contains(path) {
            reconciled.new_files.push(path.clone());
        }
    }
    reconciled.modified_files.sort_unstable();
    reconciled.deleted_files.sort_unstable();
    reconciled.new_files.sort_unstable();
    reconciled.modified_files.dedup();
    reconciled.deleted_files.dedup();
    reconciled.new_files.dedup();
    reconciled
}

fn existing_dirty_paths(diff: &IndexWorktreeDiff) -> impl Iterator<Item = String> + '_ {
    diff.modified_files
        .iter()
        .chain(diff.new_files.iter())
        .cloned()
}

fn search_document_from_path(logical_path: String, path: PathBuf) -> Result<SearchDocument> {
    let metadata = fs::metadata(&path)?;
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default();

    Ok(SearchDocument {
        logical_path,
        size: metadata.len(),
        mtime_nanos,
        source: SearchDocumentSource::Path(path),
    })
}

fn resolve_dirty_documents<I>(paths: I, filter: Option<&PathFilter>) -> Result<Vec<SearchDocument>>
where
    I: IntoIterator<Item = String>,
{
    let mut documents = Vec::new();
    for logical_path in paths {
        let path = PathBuf::from(&logical_path);
        if filter.is_some_and(|active| !active.matches_file(&path)) {
            continue;
        }
        if !path.exists() || !is_corpus_text_file(&path)? {
            continue;
        }
        documents.push(search_document_from_path(logical_path, path)?);
    }
    Ok(documents)
}
