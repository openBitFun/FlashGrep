use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::{BuildConfig, CorpusMode, TokenizerMode},
    progress::IndexProgressPhase,
    search::{SearchLine, SearchResults},
};

use super::{
    protocol as proto,
    repo::{
        DirtyStats, QueryBackend, RepoPhase, RepoStatus, RepoTaskKind, RepoTaskState,
        RepoTaskStatus, SearchConsistency, WorkspaceOverlayStatus,
    },
};

pub(super) fn system_time_to_unix_secs(time: Option<SystemTime>) -> Option<u64> {
    time.and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

pub(super) fn convert_search_results(results: SearchResults) -> proto::SearchResults {
    proto::SearchResults {
        candidate_docs: results.candidate_docs,
        searches_with_match: results.searches_with_match,
        bytes_searched: results.bytes_searched,
        matched_lines: results.matched_lines,
        matched_occurrences: results.matched_occurrences,
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
        hits: results
            .hits
            .into_iter()
            .map(|hit| proto::SearchHit {
                path: hit.path,
                matches: hit
                    .matches
                    .into_iter()
                    .map(|matched| proto::FileMatch {
                        location: proto::MatchLocation {
                            line: matched.location.line,
                            column: matched.location.column,
                        },
                        snippet: matched.snippet,
                        matched_text: matched.matched_text,
                    })
                    .collect(),
                lines: hit
                    .lines
                    .into_iter()
                    .map(|line| match line {
                        SearchLine::Match(value) => proto::SearchLine::Match {
                            value: proto::FileMatch {
                                location: proto::MatchLocation {
                                    line: value.location.line,
                                    column: value.location.column,
                                },
                                snippet: value.snippet,
                                matched_text: value.matched_text,
                            },
                        },
                        SearchLine::Context(context) => proto::SearchLine::Context {
                            line_number: context.line_number,
                            snippet: context.snippet,
                        },
                        SearchLine::ContextBreak => proto::SearchLine::ContextBreak,
                    })
                    .collect(),
            })
            .collect(),
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
        QueryBackend::IndexedSnapshot => proto::SearchBackend::IndexedSnapshot,
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

pub(super) fn convert_consistency(consistency: SearchConsistency) -> proto::ConsistencyMode {
    match consistency {
        SearchConsistency::SnapshotOnly => proto::ConsistencyMode::SnapshotOnly,
        SearchConsistency::WorkspaceEventual => proto::ConsistencyMode::WorkspaceEventual,
        SearchConsistency::WorkspaceStrict => proto::ConsistencyMode::WorkspaceStrict,
    }
}

pub(super) fn build_config_from_open(
    repo_path: PathBuf,
    storage_root: Option<PathBuf>,
    tokenizer: proto::TokenizerModeConfig,
    corpus_mode: proto::CorpusModeConfig,
    include_hidden: bool,
    max_file_size: u64,
    min_sparse_len: usize,
    max_sparse_len: usize,
) -> BuildConfig {
    BuildConfig {
        storage_root: storage_root.unwrap_or_else(|| repo_path.join(".codgrep-index-engine")),
        repo_path,
        tokenizer: match tokenizer {
            proto::TokenizerModeConfig::Trigram => TokenizerMode::Trigram,
            proto::TokenizerModeConfig::SparseNgram => TokenizerMode::SparseNgram,
        },
        corpus_mode: match corpus_mode {
            proto::CorpusModeConfig::RespectIgnore => CorpusMode::RespectIgnore,
            proto::CorpusModeConfig::NoIgnore => CorpusMode::NoIgnore,
        },
        include_hidden,
        max_file_size,
        min_sparse_len,
        max_sparse_len,
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
        last_probe_unix_secs: system_time_to_unix_secs(status.last_probe_at),
        last_rebuild_unix_secs: system_time_to_unix_secs(status.last_rebuild_at),
        dirty_files: convert_dirty_stats(status.dirty_files),
        rebuild_recommended: status.rebuild_recommended,
        active_task_id: status.active_task_id,
        probe_healthy: status.probe_healthy,
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
