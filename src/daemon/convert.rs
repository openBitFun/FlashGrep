use std::time::{SystemTime, UNIX_EPOCH};

use crate::{progress::IndexProgressPhase, search::SearchResults};

use super::{
    protocol as proto,
    repo::{
        DirtyStats, QueryBackend, RepoPhase, RepoStatus, RepoTaskKind, RepoTaskState,
        RepoTaskStatus, WorkspaceOverlayStatus,
    },
};

pub(super) fn system_time_to_unix_secs(time: Option<SystemTime>) -> Option<u64> {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

pub(super) fn convert_search_results(results: SearchResults) -> proto::SearchResults {
    let line_matches = if results.line_matches.is_empty() && !results.line_match_files.is_empty() {
        results
            .line_match_files
            .iter()
            .flat_map(|file| {
                file.line_numbers
                    .iter()
                    .map(|line_number| proto::LineMatch {
                        path: file.path.clone(),
                        line_number: *line_number,
                    })
            })
            .collect()
    } else {
        results
            .line_matches
            .into_iter()
            .map(|matched| proto::LineMatch {
                path: matched.path,
                line_number: matched.line_number,
            })
            .collect()
    };

    proto::SearchResults {
        candidate_docs: results.candidate_docs,
        searches_with_match: results.searches_with_match,
        bytes_searched: results.bytes_searched,
        matched_lines: results.matched_lines,
        matched_occurrences: results.matched_occurrences,
        matched_paths: results.matched_paths,
        line_matches,
        file_counts: results
            .file_counts
            .into_iter()
            .map(|count| proto::FileCount {
                path: count.path,
                matched_lines: count.matched_lines,
            })
            .collect(),
        file_match_counts: results
            .file_match_counts
            .into_iter()
            .map(|count| proto::FileMatchCount {
                path: count.path,
                matched_occurrences: count.matched_occurrences,
            })
            .collect(),
    }
}

pub(super) fn convert_grouped_line_match_results(
    results: SearchResults,
    global_limit: Option<usize>,
) -> proto::GroupedLineMatchResults {
    let limit_reached = line_match_limit_reached(&results, global_limit);
    let files = collect_grouped_line_match_files(results.line_match_files, results.line_matches);
    let summary = line_match_summary(&files);
    proto::GroupedLineMatchResults {
        candidate_docs: results.candidate_docs,
        searches_with_match: results.searches_with_match,
        bytes_searched: results.bytes_searched,
        matched_lines: results.matched_lines,
        matched_occurrences: results.matched_occurrences,
        limit_reached,
        summary,
        files,
    }
}

pub(super) fn convert_compact_line_match_results(
    results: SearchResults,
    global_limit: Option<usize>,
) -> proto::CompactLineMatchResults {
    let limit_reached = line_match_limit_reached(&results, global_limit);
    let files = collect_grouped_line_match_files(results.line_match_files, results.line_matches);
    let summary = line_match_summary(&files);
    proto::CompactLineMatchResults {
        candidate_docs: results.candidate_docs,
        searches_with_match: results.searches_with_match,
        bytes_searched: results.bytes_searched,
        matched_lines: results.matched_lines,
        matched_occurrences: results.matched_occurrences,
        limit_reached,
        summary,
        files,
    }
}

fn line_match_limit_reached(results: &SearchResults, global_limit: Option<usize>) -> bool {
    global_limit.is_some_and(|limit| results.line_match_count() >= limit)
}

fn collect_grouped_line_match_files(
    line_match_files: Vec<crate::search::LineMatchFile>,
    line_matches: Vec<crate::search::LineMatch>,
) -> Vec<(String, Vec<usize>)> {
    let mut files = line_match_files
        .into_iter()
        .map(|file| (file.path, file.line_numbers))
        .collect::<Vec<_>>();
    if files.is_empty() {
        for matched in line_matches {
            match files.last_mut() {
                Some((path, line_numbers)) if *path == matched.path => {
                    line_numbers.push(matched.line_number);
                }
                _ => files.push((matched.path, vec![matched.line_number])),
            }
        }
    }
    files
}

fn line_match_summary(files: &[(String, Vec<usize>)]) -> proto::LineMatchSummary {
    const TOP_FILE_LIMIT: usize = 10;

    let mut top_files = files
        .iter()
        .map(|(path, line_numbers)| proto::LineMatchFileSummary {
            path: path.clone(),
            matched_lines: line_numbers.len(),
        })
        .collect::<Vec<_>>();
    top_files.sort_unstable_by(|left, right| {
        right
            .matched_lines
            .cmp(&left.matched_lines)
            .then_with(|| left.path.cmp(&right.path))
    });
    top_files.truncate(TOP_FILE_LIMIT);

    proto::LineMatchSummary {
        files_with_matches: files.len(),
        top_files,
    }
}

pub(super) fn convert_task_status(task: RepoTaskStatus) -> proto::TaskStatus {
    proto::TaskStatus {
        task_id: task.task_id,
        workspace_id: task.repo_id,
        kind: convert_task_kind(task.kind),
        state: convert_task_state(task.state),
        phase: task.phase.map(convert_task_phase),
        message: task.message,
        processed: task.processed,
        total: task.total,
        started_unix_secs: task
            .started_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        updated_unix_secs: task
            .updated_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        finished_unix_secs: system_time_to_unix_secs(task.finished_at),
        cancellable: task.cancellable,
        error: task.error,
    }
}

pub(super) fn convert_task_kind(kind: RepoTaskKind) -> proto::TaskKind {
    match kind {
        RepoTaskKind::BuildBaseSnapshot => proto::TaskKind::BuildBaseSnapshot,
        RepoTaskKind::RebuildBaseSnapshot => proto::TaskKind::RebuildBaseSnapshot,
        RepoTaskKind::AdvanceBaseSnapshot => proto::TaskKind::AdvanceBaseSnapshot,
        RepoTaskKind::CompactBaseDeltas => proto::TaskKind::CompactBaseDeltas,
        RepoTaskKind::RefreshWorkspace => proto::TaskKind::RefreshWorkspace,
    }
}

pub(super) fn convert_task_state(state: RepoTaskState) -> proto::TaskState {
    match state {
        RepoTaskState::Queued => proto::TaskState::Queued,
        RepoTaskState::Running => proto::TaskState::Running,
        RepoTaskState::Completed => proto::TaskState::Completed,
        RepoTaskState::Failed => proto::TaskState::Failed,
        RepoTaskState::Cancelled => proto::TaskState::Cancelled,
    }
}

pub(super) fn convert_task_phase(phase: IndexProgressPhase) -> proto::TaskPhase {
    match phase {
        IndexProgressPhase::Scanning => proto::TaskPhase::Scanning,
        IndexProgressPhase::Tokenizing => proto::TaskPhase::Tokenizing,
        IndexProgressPhase::Writing => proto::TaskPhase::Writing,
        IndexProgressPhase::Finalizing => proto::TaskPhase::Finalizing,
        IndexProgressPhase::RefreshingOverlay => proto::TaskPhase::RefreshingOverlay,
    }
}

pub(super) fn convert_query_backend(backend: QueryBackend) -> proto::SearchBackend {
    match backend {
        QueryBackend::IndexedClean => proto::SearchBackend::IndexedClean,
        QueryBackend::IndexedWorkspaceView => proto::SearchBackend::IndexedWorkspaceView,
        QueryBackend::RgFallback => proto::SearchBackend::RgFallback,
        QueryBackend::ScanFallback => proto::SearchBackend::ScanFallback,
    }
}

pub(super) fn convert_repo_phase(phase: RepoPhase) -> proto::RepoPhase {
    match phase {
        RepoPhase::Opening => proto::RepoPhase::Opening,
        RepoPhase::MissingBaseSnapshot => proto::RepoPhase::MissingBaseSnapshot,
        RepoPhase::BuildingBaseSnapshot => proto::RepoPhase::BuildingBaseSnapshot,
        RepoPhase::ReadyClean => proto::RepoPhase::ReadyClean,
        RepoPhase::ReadyDirty => proto::RepoPhase::ReadyDirty,
        RepoPhase::RebuildingBaseSnapshot => proto::RepoPhase::RebuildingBaseSnapshot,
        RepoPhase::Degraded => proto::RepoPhase::Degraded,
    }
}

pub(super) fn convert_repo_status(status: RepoStatus) -> proto::RepoStatus {
    proto::RepoStatus {
        repo_id: status.repo_id,
        repo_path: status.repo_path.to_string_lossy().into_owned(),
        storage_root: status.storage_root.to_string_lossy().into_owned(),
        base_snapshot_root: status.base_snapshot_root.to_string_lossy().into_owned(),
        workspace_overlay_root: status.workspace_overlay_root.to_string_lossy().into_owned(),
        phase: convert_repo_phase(status.phase),
        snapshot_key: status.snapshot_key,
        base_head_commit: status.base_head_commit,
        workspace_head_commit: status.workspace_head_commit,
        overlay_head_commit: status.overlay_head_commit,
        overlay_base_manifest_id: status.overlay_base_manifest_id,
        base_manifest_id: status.base_manifest_id,
        base_delta_depth: status.base_delta_depth,
        base_delta_bytes: status.base_delta_bytes,
        base_advance_target_head: status.base_advance_target_head,
        cached_head_count: status.cached_head_count,
        base_compaction_recommended: status.base_compaction_recommended,
        last_probe_unix_secs: system_time_to_unix_secs(status.last_probe_at),
        last_rebuild_unix_secs: system_time_to_unix_secs(status.last_rebuild_at),
        dirty_files: convert_dirty_stats(status.dirty_files),
        active_task_id: status.active_task_id,
        probe_healthy: status.probe_healthy,
        workspace_probe_pending: status.workspace_probe_pending,
        last_error: status.last_error,
        overlay: status.overlay.map(convert_workspace_overlay_status),
    }
}

fn convert_dirty_stats(stats: DirtyStats) -> proto::DirtyFileStats {
    proto::DirtyFileStats {
        modified: stats.modified,
        deleted: stats.deleted,
        new: stats.new,
    }
}

fn convert_workspace_overlay_status(
    status: WorkspaceOverlayStatus,
) -> proto::WorkspaceOverlayStatus {
    proto::WorkspaceOverlayStatus {
        base_manifest_id: status.base_manifest_id,
        committed_seq_no: status.committed_seq_no,
        last_seq_no: status.last_seq_no,
        uncommitted_ops: status.uncommitted_ops,
        pending_docs: status.pending_docs,
        active_segments: status.active_segments,
        active_delete_segments: status.active_delete_segments,
        merge_requested: status.merge_requested,
        merge_running: status.merge_running,
        merge_attempts: status.merge_attempts,
        merge_completed: status.merge_completed,
        merge_failed: status.merge_failed,
        last_merge_error: status.last_merge_error,
    }
}
