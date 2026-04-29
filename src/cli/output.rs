use std::fmt::Write as _;
use std::io::Write;

use crate::{
    daemon::protocol::{RepoStatus, Response},
    error::{AppError, Result},
    search::SearchResults,
};

pub(super) fn print_engine_search_results(results: &SearchResults) {
    println!(
        "candidate_docs={} searches_with_match={} matched_lines={} matched_occurrences={}",
        results.candidate_docs,
        results.searches_with_match,
        results.matched_lines,
        results.matched_occurrences
    );
    for hit in &results.hits {
        println!("file {}", hit.path);
        for file_match in &hit.matches {
            println!(
                "  {}:{} {}",
                file_match.location.line, file_match.location.column, file_match.matched_text
            );
        }
    }
}

pub(super) fn print_daemon_response(response: &Response) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, response).map_err(|error| {
        AppError::Protocol(format!("failed to encode daemon response: {error}"))
    })?;
    stdout.write_all(b"\n")?;
    Ok(())
}

pub(super) fn print_daemon_status_response(response: &Response) -> Result<()> {
    match response {
        Response::RepoStatus { status } => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(format_repo_status_human(status).as_bytes())?;
            Ok(())
        }
        _ => print_daemon_response(response),
    }
}

fn format_repo_status_human(status: &RepoStatus) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "repo_id: {}", status.repo_id);
    let _ = writeln!(output, "repo_path: {}", status.repo_path);
    let _ = writeln!(output, "storage_root: {}", status.storage_root);
    let _ = writeln!(output, "base_snapshot_root: {}", status.base_snapshot_root);
    let _ = writeln!(
        output,
        "workspace_overlay_root: {}",
        status.workspace_overlay_root
    );
    let _ = writeln!(output, "phase: {:?}", status.phase);
    let _ = writeln!(
        output,
        "snapshot_key: {}",
        status.snapshot_key.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "last_probe_unix_secs: {}",
        status
            .last_probe_unix_secs
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    let _ = writeln!(
        output,
        "last_rebuild_unix_secs: {}",
        status
            .last_rebuild_unix_secs
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    let _ = writeln!(
        output,
        "dirty_files: modified={} deleted={} new={}",
        status.dirty_files.modified, status.dirty_files.deleted, status.dirty_files.new
    );
    let _ = writeln!(
        output,
        "rebuild_recommended: {}",
        status.rebuild_recommended
    );
    let _ = writeln!(
        output,
        "active_task_id: {}",
        status.active_task_id.as_deref().unwrap_or("-")
    );
    let _ = writeln!(output, "probe_healthy: {}", status.probe_healthy);
    let _ = writeln!(
        output,
        "last_error: {}",
        status.last_error.as_deref().unwrap_or("-")
    );
    if let Some(overlay) = &status.overlay {
        let _ = writeln!(output, "overlay:");
        let _ = writeln!(output, "  committed_seq_no: {}", overlay.committed_seq_no);
        let _ = writeln!(output, "  last_seq_no: {}", overlay.last_seq_no);
        let _ = writeln!(output, "  uncommitted_ops: {}", overlay.uncommitted_ops);
        let _ = writeln!(output, "  pending_docs: {}", overlay.pending_docs);
        let _ = writeln!(output, "  active_segments: {}", overlay.active_segments);
        let _ = writeln!(
            output,
            "  active_delete_segments: {}",
            overlay.active_delete_segments
        );
        let _ = writeln!(output, "  merge_requested: {}", overlay.merge_requested);
        let _ = writeln!(output, "  merge_running: {}", overlay.merge_running);
        let _ = writeln!(output, "  merge_attempts: {}", overlay.merge_attempts);
        let _ = writeln!(output, "  merge_completed: {}", overlay.merge_completed);
        let _ = writeln!(output, "  merge_failed: {}", overlay.merge_failed);
        let _ = writeln!(
            output,
            "  last_merge_error: {}",
            overlay.last_merge_error.as_deref().unwrap_or("-")
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::format_repo_status_human;

    #[test]
    fn format_repo_status_human_includes_overlay_section() {
        let status = crate::daemon::protocol::RepoStatus {
            repo_id: "/tmp/repo".into(),
            repo_path: "/tmp/repo".into(),
            storage_root: "/tmp/repo/.flashgrep-index-engine".into(),
            base_snapshot_root: "/tmp/repo/.flashgrep-index-engine/base-snapshot".into(),
            workspace_overlay_root: "/tmp/repo/.flashgrep-index-engine/workspace-overlay".into(),
            phase: crate::daemon::protocol::RepoPhase::ReadyDirty,
            snapshot_key: Some("snapshot-key".into()),
            last_probe_unix_secs: Some(1),
            last_rebuild_unix_secs: Some(2),
            dirty_files: crate::daemon::protocol::DirtyFileStats {
                modified: 1,
                deleted: 2,
                new: 3,
            },
            rebuild_recommended: false,
            active_task_id: None,
            probe_healthy: true,
            last_error: None,
            overlay: Some(crate::daemon::protocol::WorkspaceOverlayStatus {
                committed_seq_no: 4,
                last_seq_no: 5,
                uncommitted_ops: 1,
                pending_docs: 0,
                active_segments: 2,
                active_delete_segments: 1,
                merge_requested: true,
                merge_running: false,
                merge_attempts: 7,
                merge_completed: 6,
                merge_failed: 1,
                last_merge_error: Some("merge failed".into()),
            }),
        };

        let rendered = format_repo_status_human(&status);
        assert!(rendered.contains("overlay:"));
        assert!(rendered.contains("merge_attempts: 7"));
        assert!(rendered.contains("last_merge_error: merge failed"));
    }
}
