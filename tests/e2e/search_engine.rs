use flashgrep::{BuildConfig, SearchBackend, SearchEngine, SearchPolicy, TokenizerMode};

use crate::common::{count_matches_query, query, TestRepo};

#[test]
fn search_engine_uses_index_when_fresh() {
    let repo = TestRepo::new();
    let source = repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    engine.build_base_snapshot().expect("test should succeed");
    let outcome = engine
        .search(&query("PM_RESUME"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Index);
    let line_matches = outcome.results.flattened_line_matches();
    assert_eq!(line_matches.len(), 1);
    assert_eq!(line_matches[0].path, source.to_string_lossy());
}

#[test]
fn search_engine_uses_workspace_snapshot_view_when_worktree_is_dirty() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    engine.build_base_snapshot().expect("test should succeed");
    let new_file = repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let outcome = engine
        .search(&query("PM_RESUME"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Index);
    let line_matches = outcome.results.flattened_line_matches();
    assert_eq!(line_matches.len(), 2);
    let mut paths = line_matches
        .iter()
        .map(|matched| matched.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            new_file.to_string_lossy().into_owned(),
            tracked.to_string_lossy().into_owned()
        ]
    );
}

#[test]
fn search_engine_workspace_view_keeps_non_utf8_dirty_text_files() {
    let repo = TestRepo::new();
    let tracked = repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let engine = SearchEngine::new(repo.sparse_build_config());
    engine.build_base_snapshot().expect("test should succeed");
    let new_file = repo.write_bytes("latin1.txt", b"const NAME: &str = \"PM_RESUME\";\xff\n");
    let outcome = engine
        .search(&query("PM_RESUME"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Index);
    let line_matches = outcome.results.flattened_line_matches();
    assert_eq!(line_matches.len(), 2);
    let mut paths = line_matches
        .iter()
        .map(|matched| matched.path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            new_file.to_string_lossy().into_owned(),
            tracked.to_string_lossy().into_owned()
        ]
    );
}

#[test]
fn search_engine_scans_when_index_is_missing() {
    let repo = TestRepo::new();
    let source = repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    let outcome = engine
        .search(&query("PM_RESUME"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Scan);
    let line_matches = outcome.results.flattened_line_matches();
    assert_eq!(line_matches.len(), 1);
    assert_eq!(line_matches[0].path, source.to_string_lossy());
}

#[test]
fn search_engine_scans_count_matches_when_index_is_missing() {
    let repo = TestRepo::new();
    repo.write("service.rs", "PM_RESUME PM_RESUME\nPM_RESUME\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    let outcome = engine
        .search(
            &count_matches_query("PM_RESUME"),
            SearchPolicy::FallbackToScan,
        )
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Scan);
    assert_eq!(outcome.results.matched_lines, 2);
    assert_eq!(outcome.results.matched_occurrences, 3);
    assert!(outcome.results.flattened_line_matches().is_empty());
}

#[test]
fn search_engine_scan_count_matches_honors_max_count_by_matching_line() {
    let repo = TestRepo::new();
    repo.write("service.rs", "PM_RESUME PM_RESUME\nPM_RESUME\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    let mut query = count_matches_query("PM_RESUME");
    query.max_count = Some(1);

    let outcome = engine
        .search(&query, SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Scan);
    assert_eq!(outcome.results.matched_lines, 1);
    assert_eq!(outcome.results.matched_occurrences, 2);
    assert!(outcome.results.flattened_line_matches().is_empty());
}

#[test]
fn search_engine_reports_worktree_diff_separately() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    engine.build_base_snapshot().expect("test should succeed");
    let new_file = repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let diff = engine.diff_against_worktree().expect("test should succeed");

    assert!(diff
        .new_files
        .contains(&new_file.to_string_lossy().into_owned()));
    assert!(engine
        .stale_reason()
        .expect("test should succeed")
        .is_some());
}

#[test]
fn search_engine_uses_index_by_default_when_preflight_is_disabled() {
    let repo = TestRepo::new();
    repo.write("words.txt", "alpha beta gamma\n");

    let engine = SearchEngine::new(repo.trigram_build_config());
    engine.build_base_snapshot().expect("test should succeed");
    let outcome = engine
        .search(&query("beta"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");

    assert_eq!(outcome.backend, SearchBackend::Index);
    assert_eq!(outcome.results.flattened_line_matches().len(), 1);
}

#[test]
fn search_engine_keeps_relative_build_paths_fresh_after_reopen() {
    let repo = TestRepo::new();
    let source = repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let repo_root = repo
        .repo
        .parent()
        .expect("temp repo should have a parent")
        .to_path_buf();
    let storage_root = repo_root.join("idx");

    let build_engine = SearchEngine::new(BuildConfig {
        repo_path: repo.repo.clone(),
        storage_root: storage_root.clone(),
        tokenizer: TokenizerMode::Trigram,
        ..repo.trigram_build_config()
    });
    build_engine
        .build_base_snapshot()
        .expect("test should succeed");

    let engine = SearchEngine::new(BuildConfig {
        repo_path: repo.repo.clone(),
        storage_root,
        tokenizer: TokenizerMode::Trigram,
        ..repo.trigram_build_config()
    });
    let diff = engine.diff_against_worktree().expect("test should succeed");
    assert!(diff.is_empty());

    let outcome = engine
        .search(&query("PM_RESUME"), SearchPolicy::FallbackToScan)
        .expect("test should succeed");
    assert_eq!(outcome.backend, SearchBackend::Index);
    let line_matches = outcome.results.flattened_line_matches();
    assert_eq!(line_matches.len(), 1);
    assert_eq!(line_matches[0].path, source.to_string_lossy());
}
