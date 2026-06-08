use flashgrep::{advanced::PathFilter, advanced::PathFilterArgs, QueryConfig, SearchMode};

use crate::common::{count_matches_query, count_query, query, TestRepo};

#[test]
fn candidate_paths_exports_prefiltered_docs() {
    let repo = TestRepo::new();
    let source = repo.write(
        "service.rs",
        "fn load() { let value = fuzzy_file_search(); }\n",
    );
    repo.write("other.rs", "fn noop() { let value = 42; }\n");

    repo.build_sparse();

    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");

    let candidates = repo
        .engine()
        .candidate_paths(&query("fuzzy_file_search"), None)
        .expect("test should succeed");

    assert_eq!(candidates, vec![source.to_string_lossy().to_string()]);
}

#[test]
fn build_respects_gitignore_by_default() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "ignored.rs\n");
    repo.write("ignored.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write("tracked.rs", "const NAME: &str = \"OTHER\";\n");

    repo.build();
    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");

    let results = repo
        .engine()
        .search(
            &count_query("PM_RESUME"),
            flashgrep::SearchPolicy::IndexOnly,
        )
        .expect("test should succeed");

    assert_eq!(results.results.candidate_docs, 0);
    assert_eq!(results.results.matched_lines, 0);
}

#[test]
fn build_can_disable_ignore_rules() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "ignored.rs\n");
    let ignored = repo.write("ignored.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build_with(flashgrep::BuildConfig {
        corpus_mode: flashgrep::CorpusMode::NoIgnore,
        ..repo.trigram_build_config()
    });

    let engine = flashgrep::SearchEngine::new(flashgrep::BuildConfig {
        corpus_mode: flashgrep::CorpusMode::NoIgnore,
        ..repo.trigram_build_config()
    });
    let query = count_query("PM_RESUME");
    let results = engine
        .search(&query, flashgrep::SearchPolicy::IndexOnly)
        .expect("test should succeed")
        .results;

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 1);
    assert!(results.line_matches.is_empty());
    assert_eq!(
        engine
            .candidate_paths(&query, None)
            .expect("test should succeed"),
        vec![ignored.to_string_lossy().to_string()]
    );
}

#[test]
fn indexed_search_includes_non_utf8_text_files() {
    let repo = TestRepo::new();
    let utf8 = repo.write("utf8.txt", "Content-Security-Policy\n");
    let latin1 = repo.write_bytes("latin1.txt", b"Content-Security-Policy\xff\n");

    let engine = flashgrep::SearchEngine::new(repo.sparse_build_config());
    engine.build_base_snapshot().expect("test should succeed");

    let query = count_query("Content-Security-Policy");
    let results = engine
        .search(&query, flashgrep::SearchPolicy::IndexOnly)
        .expect("test should succeed")
        .results;
    assert_eq!(results.candidate_docs, 2);
    assert_eq!(results.matched_lines, 2);

    let candidates = engine
        .candidate_paths(&query, None)
        .expect("test should succeed");
    assert_eq!(
        candidates,
        vec![
            latin1.to_string_lossy().to_string(),
            utf8.to_string_lossy().to_string()
        ]
    );
}

#[test]
fn build_skips_hidden_files_by_default() {
    let repo = TestRepo::new();
    repo.write(".secret.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();
    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");

    let counts = repo
        .engine()
        .count_matches_by_file_including_zero(&count_query("PM_RESUME"), None)
        .expect("test should succeed");
    let counts = counts
        .into_iter()
        .filter(|count| count.matched_lines > 0)
        .collect::<Vec<_>>();

    assert!(counts.is_empty());
}

#[test]
fn build_can_include_hidden_files() {
    let repo = TestRepo::new();
    let hidden = repo.write(".secret.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build_with(flashgrep::BuildConfig {
        include_hidden: true,
        ..repo.trigram_build_config()
    });
    let engine = flashgrep::SearchEngine::new(flashgrep::BuildConfig {
        include_hidden: true,
        ..repo.trigram_build_config()
    });

    let counts = engine
        .count_matches_by_file_including_zero(&count_query("PM_RESUME"), None)
        .expect("test should succeed");
    let counts = counts
        .into_iter()
        .filter(|count| count.matched_lines > 0)
        .collect::<Vec<_>>();

    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].path, hidden.to_string_lossy());
    assert_eq!(counts[0].matched_lines, 1);
}

#[test]
fn search_can_be_restricted_to_selected_paths() {
    let repo = TestRepo::new();
    let src = repo.write("src/lib.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let tests = repo.write("tests/lib.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();
    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");

    let engine = repo.engine();
    let query = query("PM_RESUME");
    let cwd = std::env::current_dir().expect("test should succeed");

    let src_filter = PathFilter::new(
        PathFilterArgs {
            roots: vec![repo.path("src")],
            ..PathFilterArgs::default()
        },
        &cwd,
    )
    .expect("test should succeed");
    let results = engine
        .search_with_filter(
            &query,
            Some(&src_filter),
            flashgrep::SearchPolicy::IndexOnly,
        )
        .expect("test should succeed");
    assert_eq!(results.results.line_matches.len(), 1);
    assert_eq!(results.results.line_matches[0].path, src.to_string_lossy());

    let tests_filter = PathFilter::new(
        PathFilterArgs {
            roots: vec![repo.path("tests")],
            ..PathFilterArgs::default()
        },
        &cwd,
    )
    .expect("test should succeed");
    let counts = engine
        .count_matches_by_file_including_zero(
            &QueryConfig {
                search_mode: SearchMode::CountOnly,
                ..query.clone()
            },
            Some(&tests_filter),
        )
        .expect("test should succeed");
    let counts = counts
        .into_iter()
        .filter(|count| count.matched_lines > 0)
        .collect::<Vec<_>>();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].path, tests.to_string_lossy());
    assert_eq!(counts[0].matched_lines, 1);
}

#[test]
fn search_can_be_restricted_to_exact_file_path() {
    let repo = TestRepo::new();
    let source = repo.write("input.txt", "abc");
    repo.write("other.txt", "zzz");

    repo.build();

    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");
    let engine = repo.engine();
    let cwd = std::env::current_dir().expect("test should succeed");
    let file_filter = PathFilter::new(
        PathFilterArgs {
            roots: vec![source.clone()],
            ..PathFilterArgs::default()
        },
        &cwd,
    )
    .expect("test should succeed");

    assert_eq!(
        engine
            .visible_paths(Some(&file_filter))
            .expect("test should succeed"),
        vec![source.to_string_lossy()]
    );

    let literal = engine
        .search_with_filter(
            &query("abc"),
            Some(&file_filter),
            flashgrep::SearchPolicy::IndexOnly,
        )
        .expect("test should succeed");
    assert_eq!(literal.results.line_matches.len(), 1);
    assert_eq!(
        literal.results.line_matches[0].path,
        source.to_string_lossy()
    );
}

#[test]
fn files_without_matches_are_exposed_via_search_engine_api() {
    let repo = TestRepo::new();
    repo.write("matched.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let missed = repo.write("missed.rs", "const NAME: &str = \"OTHER\";\n");

    repo.build();

    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");
    let without_matches = repo
        .engine()
        .files_without_matches(&count_query("PM_RESUME"), None)
        .expect("test should succeed");

    assert_eq!(without_matches, vec![missed.to_string_lossy().to_string()]);
}

#[test]
fn count_total_matches_are_exposed_via_search_engine_api() {
    let repo = TestRepo::new();
    let source = repo.write("matches.rs", "PM_RESUME PM_RESUME\nPM_RESUME\n");

    repo.build();
    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");

    let query = count_matches_query("PM_RESUME");
    let engine = repo.engine();
    let results = engine
        .search(&query, flashgrep::SearchPolicy::IndexOnly)
        .expect("test should succeed")
        .results;
    assert_eq!(results.matched_lines, 2);
    assert_eq!(results.matched_occurrences, 3);
    assert!(results.line_matches.is_empty());

    let counts = engine
        .count_total_matches_by_file(&query, None)
        .expect("test should succeed");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].path, source.to_string_lossy());
    assert_eq!(counts[0].matched_occurrences, 3);
}

#[test]
fn include_zero_counts_are_exposed_via_search_engine_api() {
    let repo = TestRepo::new();
    let matched = repo.write("matched.rs", "PM_RESUME PM_RESUME\n");
    let missed = repo.write("missed.rs", "OTHER\n");

    repo.build();

    repo.engine()
        .build_base_snapshot()
        .expect("test should succeed");
    let engine = repo.engine();
    let count_query = count_query("PM_RESUME");
    let count_matches_query = count_matches_query("PM_RESUME");

    let line_counts = engine
        .count_matches_by_file_including_zero(&count_query, None)
        .expect("test should succeed");
    assert_eq!(line_counts.len(), 2);
    assert_eq!(line_counts[0].path, matched.to_string_lossy());
    assert_eq!(line_counts[0].matched_lines, 1);
    assert_eq!(line_counts[1].path, missed.to_string_lossy());
    assert_eq!(line_counts[1].matched_lines, 0);

    let total_counts = repo
        .engine()
        .count_total_matches_by_file_including_zero(&count_matches_query, None)
        .expect("test should succeed");
    assert_eq!(total_counts.len(), 2);
    assert_eq!(total_counts[0].path, matched.to_string_lossy());
    assert_eq!(total_counts[0].matched_occurrences, 2);
    assert_eq!(total_counts[1].path, missed.to_string_lossy());
    assert_eq!(total_counts[1].matched_occurrences, 0);
}
