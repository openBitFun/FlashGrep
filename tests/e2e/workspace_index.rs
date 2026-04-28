use std::{cell::Cell, fs, path::Path, rc::Rc, thread, time::Duration};

use flashgrep::{
    advanced::{json::JsonSearchReport, BaseSnapshotKind},
    experimental::{IndexProgress, IndexProgressPhase},
    AppError, BaseSnapshotBuildOptions, WorkspaceIndex, WorkspaceIndexOptions,
};

use crate::common::{count_query, query, TestRepo};

fn rewrite_with_fresh_mtime(path: &Path, contents: &str) {
    let before = fs::metadata(path)
        .expect("test should succeed")
        .modified()
        .expect("test should succeed");

    for _ in 0..20 {
        thread::sleep(Duration::from_millis(10));
        fs::write(path, contents).expect("test should succeed");

        let after = fs::metadata(path)
            .expect("test should succeed")
            .modified()
            .expect("test should succeed");
        if after > before {
            return;
        }
    }

    panic!("mtime did not advance for {}", path.display());
}

fn hit_paths(workspace: &WorkspaceIndex, pattern: &str) -> Vec<String> {
    let mut paths = workspace
        .search(&query(pattern))
        .expect("test should succeed")
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn hit_paths_current_view(workspace: &WorkspaceIndex, pattern: &str) -> Vec<String> {
    let mut paths = workspace
        .search(&query(pattern))
        .expect("test should succeed")
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn build_base_snapshot_with_progress_emits_build_phases() {
    let repo = TestRepo::new();
    repo.write("src/main.rs", "fn main() { println!(\"hello\"); }\n");
    repo.write("src/lib.rs", "pub const NAME: &str = \"progress\";\n");

    let mut events = Vec::<IndexProgress>::new();
    {
        let mut on_progress = |event| events.push(event);
        WorkspaceIndex::open(WorkspaceIndexOptions {
            build_config: repo.trigram_build_config(),
        })
        .expect("test should succeed")
        .ensure_base_snapshot_with_options(
            BaseSnapshotBuildOptions::new().with_progress(&mut on_progress),
        )
        .expect("test should succeed");
    }

    assert!(!events.is_empty());
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Scanning));
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Tokenizing));
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Writing));
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Finalizing));
    assert!(events.iter().all(|event| !event.message.trim().is_empty()));
    assert!(events.iter().all(|event| {
        event
            .total
            .map(|total| total > 0 && event.processed <= total)
            .unwrap_or(true)
    }));
    assert!(events.iter().any(|event| {
        event.phase == IndexProgressPhase::Writing && event.processed == 3 && event.total == Some(3)
    }));
    assert_eq!(
        events.last().map(|event| event.phase),
        Some(IndexProgressPhase::Finalizing)
    );
}

#[test]
fn build_base_snapshot_with_progress_and_cancel_stops_before_writing_snapshot() {
    let repo = TestRepo::new();
    repo.write("src/main.rs", "fn main() { println!(\"hello\"); }\n");
    repo.write("src/lib.rs", "pub const NAME: &str = \"progress\";\n");

    let mut on_progress = |_| {};
    let mut should_cancel = || true;
    let result = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed")
    .ensure_base_snapshot_with_options(
        BaseSnapshotBuildOptions::new()
            .with_progress(&mut on_progress)
            .with_cancel(&mut should_cancel),
    );

    assert!(matches!(result, Err(AppError::Cancelled)));
    assert!(!repo.index.exists());
}

#[test]
fn build_base_snapshot_with_progress_reports_scanning_before_publish_in_mock_git_repo() {
    let repo = TestRepo::new();
    repo.seed_mock_git_repo(6, 24);

    let saw_scanning = Rc::new(Cell::new(false));
    let saw_scanning_in_progress = Rc::clone(&saw_scanning);

    let mut on_progress = move |event: IndexProgress| {
        if event.phase == IndexProgressPhase::Scanning {
            saw_scanning_in_progress.set(true);
        }
    };
    let mut should_cancel = || false;
    let result = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed")
    .ensure_base_snapshot_with_options(
        BaseSnapshotBuildOptions::new()
            .with_progress(&mut on_progress)
            .with_cancel(&mut should_cancel),
    );

    assert!(result.is_ok());
    assert!(saw_scanning.get());
}

#[test]
fn workspace_index_searches_latest_worktree_when_base_is_stale() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");
    let added = repo.write("new.rs", "const NAME: &str = \"DIRTY_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let paths = hit_paths(&workspace, "DIRTY_NAME");

    assert_eq!(
        paths,
        vec![
            added.to_string_lossy().into_owned(),
            tracked.to_string_lossy().into_owned()
        ]
    );

    let stale_base = workspace
        .search(&count_query("BASE_NAME"))
        .expect("test should succeed");
    assert_eq!(stale_base.matched_lines, 0);
}

#[test]
fn workspace_index_search_uses_current_workspace_view_without_manual_diff() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");
    let added = repo.write("new.rs", "const NAME: &str = \"DIRTY_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let paths = hit_paths_current_view(&workspace, "DIRTY_NAME");

    assert_eq!(
        paths,
        vec![
            added.to_string_lossy().into_owned(),
            tracked.to_string_lossy().into_owned()
        ]
    );
}

#[test]
fn workspace_snapshot_freezes_materialized_overlay_snapshot() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_ONE\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    let snapshot = workspace.snapshot().expect("test should succeed");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_TWO\";\n");
    let added = repo.write("new.rs", "const NAME: &str = \"DIRTY_TWO\";\n");

    let snapshot_paths = snapshot
        .search(&query("DIRTY_ONE"))
        .expect("test should succeed")
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect::<Vec<_>>();
    let snapshot_new_paths = snapshot
        .search(&query("DIRTY_TWO"))
        .expect("test should succeed")
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect::<Vec<_>>();
    let workspace_paths = hit_paths_current_view(&workspace, "DIRTY_TWO");

    assert_eq!(snapshot_paths, vec![tracked.to_string_lossy().into_owned()]);
    assert!(snapshot_new_paths.is_empty());
    assert_eq!(
        workspace_paths,
        vec![
            added.to_string_lossy().into_owned(),
            tracked.to_string_lossy().into_owned()
        ]
    );
}

#[test]
fn workspace_index_hides_deleted_base_results() {
    let repo = TestRepo::new();
    repo.write("keep.rs", "const NAME: &str = \"TOKEN\";\n");
    let deleted = repo.write("deleted.rs", "const NAME: &str = \"TOKEN\";\n");
    repo.build();
    fs::remove_file(&deleted).expect("test should succeed");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let paths = hit_paths(&workspace, "TOKEN");

    assert_eq!(paths.len(), 1);
    assert!(!paths.contains(&deleted.to_string_lossy().into_owned()));
}

#[test]
fn workspace_status_reports_dirty_files_without_refresh_step() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY\";\n");
    repo.write("new.rs", "const NAME: &str = \"DIRTY\";\n");

    let status = workspace.status().expect("test should succeed");
    let dirty = status.dirty_files.expect("dirty diff should exist");
    assert_eq!(dirty.modified_files.len(), 1);
    assert_eq!(dirty.new_files.len(), 1);
}

#[test]
fn workspace_index_exposes_git_snapshot_identity() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");

    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    assert_eq!(base.snapshot_kind, BaseSnapshotKind::GitCommit);
    assert_eq!(base.head_commit.as_deref(), Some(repo.git_head().as_str()));
    assert!(base.snapshot_key.starts_with("base-git-"));
    assert!(base.config_fingerprint.is_some());
}

#[test]
fn workspace_index_git_base_snapshot_skips_untracked_files_when_head_matches_worktree() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"TRACKED\";\n");
    repo.commit_all("initial");
    repo.write("untracked.rs", "const NAME: &str = \"UNTRACKED\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");

    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    assert_eq!(base.doc_count, 1);

    let dirty = workspace
        .status()
        .expect("test should succeed")
        .dirty_files
        .expect("dirty diff should exist");
    assert_eq!(dirty.new_files.len(), 1);
}

#[test]
fn workspace_index_switches_to_new_git_head_after_commit() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");

    let first_base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    let first_head = repo.git_head();

    repo.write("tracked.rs", "const NAME: &str = \"NEXT_NAME\";\n");
    repo.commit_all("second");

    let second_base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    let second_head = repo.git_head();

    assert_ne!(first_head, second_head);
    assert_ne!(first_base.snapshot_key, second_base.snapshot_key);
    assert_eq!(
        second_base.head_commit.as_deref(),
        Some(second_head.as_str())
    );
}

#[test]
fn workspace_probe_freshness_if_due_reuses_cache_until_ttl_expires() {
    let repo = TestRepo::new();
    repo.write("base.rs", "const NAME: &str = \"BASE\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let first = workspace
        .probe_freshness_if_due(Duration::from_secs(60))
        .expect("test should succeed");
    assert!(!first.is_stale());

    repo.write("added.rs", "const NAME: &str = \"DIRTY\";\n");

    let cached = workspace
        .probe_freshness_if_due(Duration::from_secs(60))
        .expect("test should succeed");
    assert!(!cached.is_stale());

    let refreshed = workspace
        .probe_freshness_if_due(Duration::ZERO)
        .expect("test should succeed");
    assert!(refreshed.is_stale());
    assert_eq!(refreshed.new_files, 1);
}

#[test]
fn workspace_json_report_reflects_latest_worktree_view() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");
    let new_file = repo.write("new.rs", "const NAME: &str = \"DIRTY_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
    })
    .expect("test should succeed");
    workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    let report = JsonSearchReport::from_search_results(
        &workspace
            .search(&query("DIRTY_NAME"))
            .expect("test should succeed"),
    );
    assert!(report.has_match());
    assert_eq!(report.files.len(), 2);
    let paths = report
        .files
        .iter()
        .map(|file| match &file.path {
            flashgrep::advanced::json::JsonData::Text { text } => text.clone(),
            flashgrep::advanced::json::JsonData::Bytes { .. } => {
                panic!("test expected utf-8 path")
            }
        })
        .collect::<Vec<_>>();
    assert!(paths.contains(&tracked.to_string_lossy().into_owned()));
    assert!(paths.contains(&new_file.to_string_lossy().into_owned()));
}
