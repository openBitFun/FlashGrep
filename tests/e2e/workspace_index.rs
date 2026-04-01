use std::{
    cell::Cell,
    collections::BTreeMap,
    fs,
    path::Path,
    rc::Rc,
    thread,
    time::{Duration, SystemTime},
};

use codgrep::{
    advanced::{
        json::{JsonData, JsonSearchReport},
        BaseSnapshotKind, IndexProgressPhase,
    },
    build_index_with_options,
    experimental::index_format::{read_doc_terms_file, read_docs_file},
    AppError, IndexBuildOptions, QueryConfig, SearchMode, WorkspaceIndex, WorkspaceIndexOptions,
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

    panic!(
        "mtime did not advance after rewriting {} from {:?}",
        path.display(),
        SystemTime::now()
    );
}

fn overlay_terms_by_path(
    repo: &TestRepo,
    base_snapshot_key: &str,
    overlay_key: &str,
) -> BTreeMap<String, Vec<u64>> {
    let overlay_dir = repo
        .index
        .join("overlays")
        .join(base_snapshot_key)
        .join("generations")
        .join(overlay_key);
    let (_, docs) = read_docs_file(&overlay_dir.join("docs.bin")).expect("test should succeed");
    let doc_terms =
        read_doc_terms_file(&overlay_dir.join("doc_terms.bin")).expect("test should succeed");
    docs.into_iter()
        .zip(doc_terms)
        .map(|(doc, token_hashes)| (doc.path, token_hashes))
        .collect()
}

fn overlay_doc_ids_by_path(
    repo: &TestRepo,
    base_snapshot_key: &str,
    overlay_key: &str,
) -> BTreeMap<String, u32> {
    let overlay_dir = repo
        .index
        .join("overlays")
        .join(base_snapshot_key)
        .join("generations")
        .join(overlay_key);
    let (_, docs) = read_docs_file(&overlay_dir.join("docs.bin")).expect("test should succeed");
    docs.into_iter().map(|doc| (doc.path, doc.doc_id)).collect()
}

fn overlay_generations(repo: &TestRepo, base_snapshot_key: &str) -> Vec<String> {
    let generations_dir = repo
        .index
        .join("overlays")
        .join(base_snapshot_key)
        .join("generations");
    let mut generations = fs::read_dir(generations_dir)
        .expect("test should succeed")
        .map(|entry| {
            entry
                .expect("test should succeed")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    generations.sort_unstable();
    generations
}

fn hit_paths(results: codgrep::SearchResults) -> Vec<String> {
    results.hits.into_iter().map(|hit| hit.path).collect()
}

fn json_text(data: &JsonData) -> &str {
    match data {
        JsonData::Text { text } => text,
        JsonData::Bytes { .. } => panic!("test expected utf-8 json payload"),
    }
}

#[test]
fn build_index_with_progress_emits_index_build_phases() {
    let repo = TestRepo::new();
    repo.write("src/main.rs", "fn main() { println!(\"hello\"); }\n");
    repo.write("src/lib.rs", "pub const NAME: &str = \"progress\";\n");

    let mut events = Vec::new();
    {
        let mut on_progress = |event| events.push(event);
        build_index_with_options(
            &repo.trigram_build_config(),
            IndexBuildOptions::new().with_progress(&mut on_progress),
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
    assert_eq!(
        events.last().map(|event| event.phase),
        Some(IndexProgressPhase::Finalizing)
    );
    assert!(events.last().is_some_and(|event| event.is_complete()));
}

#[test]
fn workspace_refresh_overlay_with_progress_emits_overlay_phases() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");
    repo.write("added.rs", "const NAME: &str = \"DIRTY_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let mut events = Vec::new();
    let overlay = workspace
        .refresh_overlay_with_progress(|event| events.push(event))
        .expect("test should succeed");

    assert!(overlay.is_some());
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::RefreshingOverlay));
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Scanning));
    assert!(events
        .iter()
        .any(|event| event.phase == IndexProgressPhase::Tokenizing));
    assert_eq!(
        events.last().map(|event| event.phase),
        Some(IndexProgressPhase::RefreshingOverlay)
    );
    assert!(events.last().is_some_and(|event| event.is_complete()));
}

#[test]
fn build_index_with_progress_and_cancel_stops_before_writing_index() {
    let repo = TestRepo::new();
    repo.write("src/main.rs", "fn main() { println!(\"hello\"); }\n");
    repo.write("src/lib.rs", "pub const NAME: &str = \"progress\";\n");

    let mut on_progress = |_| {};
    let mut should_cancel = || true;
    let result = codgrep::build_index_with_options(
        &repo.trigram_build_config(),
        IndexBuildOptions::new()
            .with_progress(&mut on_progress)
            .with_cancel(&mut should_cancel),
    );

    assert!(matches!(result, Err(AppError::Cancelled)));
    assert!(!repo.index.exists());
}

#[test]
fn workspace_refresh_overlay_with_progress_and_cancel_returns_cancelled() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let result = workspace.refresh_overlay_with_progress_and_cancel(|_| {}, || true);

    assert!(matches!(result, Err(AppError::Cancelled)));
}

#[test]
fn build_index_with_progress_and_cancel_can_stop_after_progress_starts_in_mock_git_repo() {
    let repo = TestRepo::new();
    repo.seed_mock_git_repo(6, 24);

    let should_cancel = Rc::new(Cell::new(false));
    let cancel_in_progress = Rc::clone(&should_cancel);
    let cancel_for_check = Rc::clone(&should_cancel);

    let mut on_progress = move |event: codgrep::advanced::IndexProgress| {
        if event.phase == IndexProgressPhase::Tokenizing && event.processed >= 8 {
            cancel_in_progress.set(true);
        }
    };
    let mut should_cancel = move || cancel_for_check.get();
    let result = codgrep::build_index_with_options(
        &repo.trigram_build_config(),
        IndexBuildOptions::new()
            .with_progress(&mut on_progress)
            .with_cancel(&mut should_cancel),
    );

    assert!(matches!(result, Err(AppError::Cancelled)));
    assert!(!repo.index.join("CURRENT").exists());
}

#[test]
fn workspace_refresh_overlay_with_progress_and_cancel_keeps_previous_overlay_active() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_DIRTY\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let first_overlay = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let base = workspace
        .status()
        .expect("test should succeed")
        .base
        .expect("base should exist");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_DIRTY\";\n");

    let should_cancel = Rc::new(Cell::new(false));
    let cancel_in_progress = Rc::clone(&should_cancel);
    let cancel_for_check = Rc::clone(&should_cancel);
    let result = workspace.refresh_overlay_with_progress_and_cancel(
        move |event| {
            if event.phase == IndexProgressPhase::Tokenizing && event.processed >= 1 {
                cancel_in_progress.set(true);
            }
        },
        move || cancel_for_check.get(),
    );

    assert!(matches!(result, Err(AppError::Cancelled)));

    let current_overlay = workspace
        .status()
        .expect("test should succeed")
        .overlay
        .expect("previous overlay should remain active");
    assert_eq!(current_overlay.overlay_key, first_overlay.overlay_key);

    let current_pointer = fs::read_to_string(
        repo.index
            .join("overlays")
            .join(&base.snapshot_key)
            .join("CURRENT"),
    )
    .expect("test should succeed");
    assert_eq!(current_pointer.trim(), first_overlay.overlay_key);
}

#[test]
fn workspace_index_searches_latest_worktree_when_base_is_stale() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let new_file = repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let results = workspace
        .search(&query("PM_RESUME"))
        .expect("test should succeed");

    let paths = results
        .hits
        .into_iter()
        .map(|hit| hit.path)
        .collect::<Vec<_>>();
    assert!(paths.contains(&tracked.to_string_lossy().into_owned()));
    assert!(paths.contains(&new_file.to_string_lossy().into_owned()));
}

#[test]
fn workspace_index_refreshes_overlay_manifest() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.build();
    repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let overlay = workspace.refresh_overlay().expect("test should succeed");
    let overlay = overlay.expect("dirty worktree should produce overlay");
    assert_eq!(overlay.new_files, 1);

    let status = workspace.status().expect("test should succeed");
    assert!(status.overlay.is_some());
    assert!(
        status
            .dirty_files
            .expect("dirty files should exist")
            .new_files
            .len()
            >= 1
    );
}

#[test]
fn workspace_index_refresh_overlay_returns_none_when_worktree_is_clean() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let overlay = workspace.refresh_overlay().expect("test should succeed");
    assert!(overlay.is_none());

    let status = workspace.status().expect("test should succeed");
    assert!(status.overlay.is_none());
    assert!(status
        .dirty_files
        .expect("dirty files should exist")
        .is_empty());
}

#[test]
fn workspace_index_reuses_overlay_cache_for_same_dirty_state() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let base = workspace
        .status()
        .expect("test should succeed")
        .base
        .expect("base should exist");
    let docs_path = repo
        .index
        .join("overlays")
        .join(&base.snapshot_key)
        .join("generations")
        .join(&first.overlay_key)
        .join("docs.bin");
    let docs_mtime_before = fs::metadata(&docs_path)
        .expect("test should succeed")
        .modified()
        .expect("test should succeed");

    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let docs_mtime_after = fs::metadata(&docs_path)
        .expect("test should succeed")
        .modified()
        .expect("test should succeed");

    assert_eq!(first.overlay_key, second.overlay_key);
    assert_eq!(docs_mtime_before, docs_mtime_after);
}

#[test]
fn workspace_index_keeps_multiple_overlay_caches_for_different_dirty_states() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");
    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    assert_ne!(first.overlay_key, second.overlay_key);
    assert!(repo
        .index
        .join("overlays")
        .join(&base.snapshot_key)
        .join("generations")
        .join(&first.overlay_key)
        .exists());
    assert!(repo
        .index
        .join("overlays")
        .join(&base.snapshot_key)
        .join("generations")
        .join(&second.overlay_key)
        .exists());
}

#[test]
fn workspace_index_auto_prunes_overlay_caches_to_configured_limit() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: Some(2),
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");
    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"THIRD_NAME\";\n");
    let third = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let generations = overlay_generations(&repo, &base.snapshot_key);
    assert_eq!(generations.len(), 2);
    assert!(generations.contains(&second.overlay_key));
    assert!(generations.contains(&third.overlay_key));
    assert!(!generations.contains(&first.overlay_key));
}

#[test]
fn workspace_index_manual_prune_overlay_cache_removes_stale_generations() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");
    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"THIRD_NAME\";\n");
    let third = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let removed = workspace
        .prune_overlay_cache(1)
        .expect("test should succeed");
    let generations = overlay_generations(&repo, &base.snapshot_key);

    assert_eq!(removed, 2);
    assert_eq!(generations, vec![third.overlay_key.clone()]);
    assert!(!generations.contains(&first.overlay_key));
    assert!(!generations.contains(&second.overlay_key));
}

#[test]
fn workspace_index_reuses_cached_overlay_doc_terms_for_unchanged_dirty_files() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let new_file = repo.write("new.rs", "const NAME: &str = \"NEW_NAME\";\n");
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");
    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let first_terms = overlay_terms_by_path(&repo, &base.snapshot_key, &first.overlay_key);

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let second_terms = overlay_terms_by_path(&repo, &base.snapshot_key, &second.overlay_key);

    let new_path = new_file
        .strip_prefix(&repo.repo)
        .expect("test should succeed")
        .to_string_lossy()
        .into_owned();
    let tracked_path = tracked
        .strip_prefix(&repo.repo)
        .expect("test should succeed")
        .to_string_lossy()
        .into_owned();

    assert_eq!(first_terms[&new_path], second_terms[&new_path]);
    assert_ne!(first_terms[&tracked_path], second_terms[&tracked_path]);
}

#[test]
fn workspace_index_keeps_overlay_doc_ids_stable_for_unchanged_dirty_files() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");

    let new_file = repo.write("new.rs", "const NAME: &str = \"NEW_NAME\";\n");
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");
    let first = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let first_ids = overlay_doc_ids_by_path(&repo, &base.snapshot_key, &first.overlay_key);

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    let second = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let second_ids = overlay_doc_ids_by_path(&repo, &base.snapshot_key, &second.overlay_key);

    let new_path = new_file
        .strip_prefix(&repo.repo)
        .expect("test should succeed")
        .to_string_lossy()
        .into_owned();
    let tracked_path = tracked
        .strip_prefix(&repo.repo)
        .expect("test should succeed")
        .to_string_lossy()
        .into_owned();

    assert_eq!(first_ids[&new_path], second_ids[&new_path]);
    assert_eq!(first_ids[&tracked_path], second_ids[&tracked_path]);
}

#[test]
fn workspace_index_rebuilds_overlay_when_same_dirty_path_changes_again() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.build();
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_NAME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let first_results = workspace
        .search(&query("FIRST_NAME"))
        .expect("test should succeed");
    assert_eq!(first_results.hits.len(), 1);

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"SECOND_NAME\";\n");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let second_results = workspace
        .search(&query("SECOND_NAME"))
        .expect("test should succeed");
    assert_eq!(second_results.hits.len(), 1);

    let stale_results = workspace
        .search(&QueryConfig {
            regex_pattern: "FIRST_NAME".into(),
            patterns: vec!["FIRST_NAME".into()],
            search_mode: SearchMode::CountOnly,
            ..QueryConfig::default()
        })
        .expect("test should succeed");
    assert_eq!(stale_results.matched_lines, 0);
}

#[test]
fn workspace_index_exposes_git_snapshot_identity() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
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
fn workspace_index_exposes_repository_fallback_snapshot_identity() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    assert_eq!(base.snapshot_kind, BaseSnapshotKind::RepositoryFallback);
    assert!(base.head_commit.is_none());
    assert!(base.snapshot_key.starts_with("base-repo-"));
    assert!(base.config_fingerprint.is_some());
}

#[test]
fn workspace_snapshot_on_dirty_git_repo_tracks_modified_deleted_and_new_files() {
    let repo = TestRepo::new();
    repo.init_git();
    let modified = repo.write("modified.rs", "const NAME: &str = \"BASE_ONLY\";\n");
    repo.write("deleted.rs", "const NAME: &str = \"DELETED_ONLY\";\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(&modified, "const NAME: &str = \"DIRTY_ONLY\";\n");
    fs::remove_file(repo.path("deleted.rs")).expect("test should succeed");
    let new_file = repo.write("new.rs", "const NAME: &str = \"NEW_ONLY\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let snapshot = workspace.snapshot().expect("test should succeed");

    let dirty_paths = hit_paths(
        snapshot
            .search(&query("DIRTY_ONLY"))
            .expect("test should succeed"),
    );
    assert_eq!(dirty_paths, vec![modified.to_string_lossy().into_owned()]);

    let new_paths = hit_paths(
        snapshot
            .search(&query("NEW_ONLY"))
            .expect("test should succeed"),
    );
    assert_eq!(new_paths, vec![new_file.to_string_lossy().into_owned()]);

    let stale_base = snapshot
        .search(&count_query("BASE_ONLY"))
        .expect("test should succeed");
    assert_eq!(stale_base.matched_lines, 0);

    let deleted = snapshot
        .search(&count_query("DELETED_ONLY"))
        .expect("test should succeed");
    assert_eq!(deleted.matched_lines, 0);
}

#[test]
fn workspace_index_search_requires_explicit_refresh_for_dirty_git_worktree() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"AUTO_DIRTY\";\n");
    let new_file = repo.write("new.rs", "const NAME: &str = \"AUTO_DIRTY\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: true,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let stale = workspace
        .search(&count_query("AUTO_DIRTY"))
        .expect("test should succeed");
    assert_eq!(stale.matched_lines, 0);

    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let paths = hit_paths(
        workspace
            .search(&query("AUTO_DIRTY"))
            .expect("test should succeed"),
    );
    assert!(paths.contains(&tracked.to_string_lossy().into_owned()));
    assert!(paths.contains(&new_file.to_string_lossy().into_owned()));

    let status = workspace.status().expect("test should succeed");
    let overlay = status
        .overlay
        .expect("overlay should exist after auto refresh");
    assert_eq!(
        overlay.base_snapshot_key,
        status.base.expect("base should exist").snapshot_key
    );
    assert_eq!(overlay.modified_files, 1);
    assert_eq!(overlay.new_files, 1);
    assert_eq!(overlay.deleted_files, 0);
}

#[test]
fn workspace_snapshot_reuses_dirty_git_overlay_for_multiple_queries() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"FIRST_DIRTY\";\n");
    let new_file = repo.write("new.rs", "const NAME: &str = \"SECOND_DIRTY\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    let base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let snapshot = workspace.snapshot().expect("test should succeed");

    let first_paths = hit_paths(
        snapshot
            .search(&query("FIRST_DIRTY"))
            .expect("test should succeed"),
    );
    assert_eq!(first_paths, vec![tracked.to_string_lossy().into_owned()]);

    let second_paths = hit_paths(
        snapshot
            .search(&query("SECOND_DIRTY"))
            .expect("test should succeed"),
    );
    assert_eq!(second_paths, vec![new_file.to_string_lossy().into_owned()]);

    let stale = snapshot
        .search(&count_query("BASE_NAME"))
        .expect("test should succeed");
    assert_eq!(stale.matched_lines, 0);

    assert_eq!(overlay_generations(&repo, &base.snapshot_key).len(), 1);
}

#[test]
fn workspace_index_switches_to_new_git_head_after_commit_and_clears_overlay() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("src/service.rs", "const NAME: &str = \"BASE_NAME\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");

    let first_base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    let first_head = repo.git_head();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_NAME\";\n");
    let dirty_overlay = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    assert_eq!(dirty_overlay.base_snapshot_key, first_base.snapshot_key);

    repo.commit_all("dirty becomes clean base");
    let second_head = repo.git_head();
    assert_ne!(first_head, second_head);

    let second_base = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    assert_eq!(
        second_base.head_commit.as_deref(),
        Some(second_head.as_str())
    );
    assert_ne!(first_base.snapshot_key, second_base.snapshot_key);

    let status = workspace.status().expect("test should succeed");
    assert_eq!(
        status
            .base
            .expect("base should exist")
            .head_commit
            .as_deref(),
        Some(second_head.as_str())
    );
    assert!(status
        .dirty_files
        .expect("dirty files should exist")
        .is_empty());
    assert!(workspace
        .refresh_overlay()
        .expect("test should succeed")
        .is_none());
    assert!(workspace
        .status()
        .expect("test should succeed")
        .overlay
        .is_none());

    let results = workspace
        .search(&query("DIRTY_NAME"))
        .expect("test should succeed");
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, tracked.to_string_lossy());
}

#[test]
fn workspace_snapshot_handles_complex_git_worktree_with_ignore_and_rename_like_changes() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(".gitignore", "target/\n*.log\n");
    repo.write(
        "src/api/user_service.rs",
        "pub fn load_user() { let key = \"USER_TOKEN\"; }\n",
    );
    repo.write(
        "src/api/obsolete.rs",
        "pub fn old_path() { let key = \"OBSOLETE_TOKEN\"; }\n",
    );
    repo.write("docs/notes.md", "Base notes only.\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(
        &repo.path("src/api/user_service.rs"),
        "pub fn load_account() { let key = \"ACCOUNT_TOKEN\"; }\n",
    );
    fs::remove_file(repo.path("src/api/obsolete.rs")).expect("test should succeed");
    let renamed = repo.write(
        "src/domain/account_service.rs",
        "pub fn renamed_path() { let key = \"RENAMED_TOKEN\"; }\n",
    );
    let tracked_doc = repo.write("docs/runbook.md", "Operator note: RUNBOOK_TOKEN\n");
    repo.write(
        "target/generated.rs",
        "const NAME: &str = \"IGNORED_TOKEN\";\n",
    );
    repo.write("debug.log", "IGNORED_TOKEN\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    let snapshot = workspace.snapshot().expect("test should succeed");

    let account_paths = hit_paths(
        snapshot
            .search(&query("ACCOUNT_TOKEN"))
            .expect("test should succeed"),
    );
    assert_eq!(
        account_paths,
        vec![repo
            .path("src/api/user_service.rs")
            .to_string_lossy()
            .into_owned()]
    );

    let renamed_paths = hit_paths(
        snapshot
            .search(&query("RENAMED_TOKEN"))
            .expect("test should succeed"),
    );
    assert_eq!(renamed_paths, vec![renamed.to_string_lossy().into_owned()]);

    let doc_paths = hit_paths(
        snapshot
            .search(&query("RUNBOOK_TOKEN"))
            .expect("test should succeed"),
    );
    assert_eq!(doc_paths, vec![tracked_doc.to_string_lossy().into_owned()]);

    let obsolete = snapshot
        .search(&count_query("OBSOLETE_TOKEN"))
        .expect("test should succeed");
    assert_eq!(obsolete.matched_lines, 0);

    let ignored = snapshot
        .search(&count_query("IGNORED_TOKEN"))
        .expect("test should succeed");
    assert_eq!(ignored.matched_lines, 0);
}

#[test]
fn workspace_index_keeps_overlay_caches_isolated_across_git_base_transitions() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("src/service.rs", "const NAME: &str = \"BASE_A\";\n");
    repo.commit_all("initial");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: Some(1),
    })
    .expect("test should succeed");

    let base_a = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_A\";\n");
    let overlay_a = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    assert_eq!(overlay_a.base_snapshot_key, base_a.snapshot_key);
    assert_eq!(
        overlay_generations(&repo, &base_a.snapshot_key),
        vec![overlay_a.overlay_key.clone()]
    );

    repo.commit_all("promote dirty state to new base");
    let base_b = workspace
        .ensure_base_snapshot()
        .expect("test should succeed");
    assert_ne!(base_a.snapshot_key, base_b.snapshot_key);
    assert_eq!(
        overlay_generations(&repo, &base_a.snapshot_key),
        vec![overlay_a.overlay_key.clone()]
    );

    let new_file = repo.write("src/new_feature.rs", "const NAME: &str = \"DIRTY_B\";\n");
    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_B\";\n");
    let overlay_b1 = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    assert_eq!(overlay_b1.base_snapshot_key, base_b.snapshot_key);

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"DIRTY_C\";\n");
    let overlay_b2 = workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");
    assert_eq!(overlay_b2.base_snapshot_key, base_b.snapshot_key);
    assert_ne!(overlay_b1.overlay_key, overlay_b2.overlay_key);

    assert_eq!(
        overlay_generations(&repo, &base_a.snapshot_key),
        vec![overlay_a.overlay_key.clone()]
    );
    assert_eq!(
        overlay_generations(&repo, &base_b.snapshot_key),
        vec![overlay_b2.overlay_key.clone()]
    );

    let dirty_c_paths = hit_paths(
        workspace
            .search(&query("DIRTY_C"))
            .expect("test should succeed"),
    );
    assert_eq!(dirty_c_paths, vec![tracked.to_string_lossy().into_owned()]);

    let dirty_b_paths = hit_paths(
        workspace
            .search(&query("DIRTY_B"))
            .expect("test should succeed"),
    );
    assert_eq!(dirty_b_paths, vec![new_file.to_string_lossy().into_owned()]);

    let stale_a = workspace
        .search(&count_query("DIRTY_A"))
        .expect("test should succeed");
    assert_eq!(stale_a.matched_lines, 0);
}

#[test]
fn workspace_search_json_reports_effective_dirty_git_view() {
    let repo = TestRepo::new();
    repo.init_git();
    let tracked = repo.write("src/tracked.rs", "const NAME: &str = \"BASE_JSON\";\n");
    repo.write("src/deleted.rs", "const NAME: &str = \"REMOVED_JSON\";\n");
    repo.commit_all("initial");
    repo.build();

    rewrite_with_fresh_mtime(&tracked, "const NAME: &str = \"LIVE_JSON\";\n");
    fs::remove_file(repo.path("src/deleted.rs")).expect("test should succeed");
    let new_file = repo.write("src/new.rs", "const NAME: &str = \"LIVE_JSON\";\n");

    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: repo.trigram_build_config(),
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
    })
    .expect("test should succeed");
    workspace
        .refresh_overlay()
        .expect("test should succeed")
        .expect("dirty worktree should produce overlay");

    let report = JsonSearchReport::from_search_results(
        &workspace
            .search(&query("LIVE_JSON"))
            .expect("test should succeed"),
    );
    let mut matched_paths = report
        .files
        .iter()
        .map(|file| json_text(&file.path).to_string())
        .collect::<Vec<_>>();
    matched_paths.sort_unstable();

    let mut expected_paths = vec![
        tracked.to_string_lossy().into_owned(),
        new_file.to_string_lossy().into_owned(),
    ];
    expected_paths.sort_unstable();

    assert!(report.has_match());
    assert_eq!(matched_paths, expected_paths);
    assert_eq!(report.summary.searches_with_match, 2);
    assert_eq!(report.summary.matched_lines, 2);
    assert_eq!(report.summary.matches, 2);

    let deleted = JsonSearchReport::from_search_results(
        &workspace
            .search(&query("REMOVED_JSON"))
            .expect("test should succeed"),
    );
    assert!(!deleted.has_match());
    assert!(deleted.files.is_empty());
    assert_eq!(deleted.summary.matched_lines, 0);
    assert_eq!(deleted.summary.matches, 0);

    let stale_base = JsonSearchReport::from_search_results(
        &workspace
            .search(&query("BASE_JSON"))
            .expect("test should succeed"),
    );
    assert!(!stale_base.has_match());
    assert!(stale_base.files.is_empty());
}
