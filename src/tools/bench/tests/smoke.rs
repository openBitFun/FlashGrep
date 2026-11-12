use super::*;

#[test]
fn bench_runner_smoke_test() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path();
    write_standard_suite(suite_dir);

    let report = run(&BenchConfig {
        suite_dir: suite_dir.to_path_buf(),
        filter: Some("linux_literal".into()),
        custom_repo: None,
        custom_patterns: Vec::new(),
        custom_name: None,
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 1,
        top_k_tokens: 4,
        compare_rg: false,
        compare_worktree: false,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: None,
        cold_hook: None,
    })
    .expect("test should succeed");

    assert_eq!(report.summaries.len(), 2);
    assert!(report
        .summaries
        .iter()
        .all(|summary| summary.runners[0].sample_count == 1));
}

#[test]
fn parse_rg_count_output_supports_single_and_multi_file_formats() {
    assert_eq!(
        parse_rg_count_output(b"3\n").expect("test should succeed"),
        3
    );
    assert_eq!(
        parse_rg_count_output(b"/tmp/a.rs:2\n/tmp/b.rs:5\n").expect("test should succeed"),
        7
    );
}

#[test]
fn benchmark_scope_roots_uses_repo_relative_paths_for_daemon_queries() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("test should succeed");
    fs::write(repo.join("src").join("lib.rs"), "PM_RESUME\n").expect("test should succeed");

    let repo_roots = benchmark_scope_roots(&repo, vec![repo.clone()]);
    assert!(repo_roots.is_empty());

    let nested = benchmark_scope_roots(&repo, vec![repo.join("src").join("lib.rs")]);
    assert_eq!(nested, vec![std::path::PathBuf::from("src/lib.rs")]);
}

#[cfg(target_os = "linux")]
#[test]
fn cold_bench_runs_hook_before_each_iteration() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path();
    fs::create_dir_all(suite_dir.join("linux")).expect("test should succeed");
    fs::create_dir_all(suite_dir.join("subtitles")).expect("test should succeed");

    fs::write(
        suite_dir.join("linux").join("kernel.c"),
        "PM_RESUME\npm_resume\n",
    )
    .expect("test should succeed");
    fs::write(
        suite_dir.join("subtitles").join("en.sample.txt"),
        "Sherlock Holmes met John Watson.\n",
    )
    .expect("test should succeed");
    fs::write(
        suite_dir.join("subtitles").join("ru.txt"),
        "Шерлок Холмс встретил Джон Уотсон.\n",
    )
    .expect("test should succeed");
    ensure_git_repo_with_head(&suite_dir.join("linux"));
    ensure_git_repo_with_head(&suite_dir.join("subtitles"));

    let hook_path = suite_dir.join("cold-hook-count.txt");
    let hook = format!("printf x >> {}", hook_path.display());

    let report = run(&BenchConfig {
        suite_dir: suite_dir.to_path_buf(),
        filter: Some("linux_literal_casei".into()),
        custom_repo: None,
        custom_patterns: Vec::new(),
        custom_name: None,
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Cold,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 1,
        bench_iter: 2,
        top_k_tokens: 4,
        compare_rg: false,
        compare_worktree: false,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: None,
        cold_hook: Some(hook),
    })
    .expect("test should succeed");

    assert_eq!(report.summaries.len(), 1);
    let hook_count = fs::read_to_string(hook_path).expect("test should succeed");
    assert_eq!(hook_count, "xxx");
}

#[test]
fn bench_report_labels_worktree_runner_modes() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path();
    write_standard_suite(suite_dir);

    let report = run(&BenchConfig {
        suite_dir: suite_dir.to_path_buf(),
        filter: Some("linux_literal".into()),
        custom_repo: None,
        custom_patterns: Vec::new(),
        custom_name: None,
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 1,
        top_k_tokens: 4,
        compare_rg: false,
        compare_worktree: true,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: None,
        cold_hook: None,
    })
    .expect("test should succeed");

    let rendered = render_report(&report);
    let base = rendered
        .find("flashgrep [daemon-steady-state]")
        .expect("test should succeed");
    let worktree_build = rendered
        .find("flashgrep_worktree_build [dirty-first-query]")
        .expect("test should succeed");
    let worktree_cached = rendered
        .find("flashgrep_worktree [dirty-cached-query]")
        .expect("test should succeed");

    assert!(base < worktree_build);
    assert!(worktree_build < worktree_cached);
}

#[test]
fn bench_raw_output_includes_runner_family_and_mode() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path();
    write_standard_suite(suite_dir);

    let raw_output = suite_dir.join("bench.csv");
    let _ = run(&BenchConfig {
        suite_dir: suite_dir.to_path_buf(),
        filter: Some("linux_literal".into()),
        custom_repo: None,
        custom_patterns: Vec::new(),
        custom_name: None,
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 1,
        top_k_tokens: 4,
        compare_rg: false,
        compare_worktree: true,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: Some(raw_output.clone()),
        cold_hook: None,
    })
    .expect("test should succeed");

    let csv = fs::read_to_string(raw_output).expect("test should succeed");
    let lines = csv.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0],
        "benchmark,corpus,runner,runner_family,runner_mode,iteration,duration_secs,candidate_docs,match_count"
    );
    assert!(lines
        .iter()
        .any(|line| line.contains(",flashgrep,flashgrep,daemon_steady_state,")));
    assert!(lines
        .iter()
        .any(|line| line.contains(",flashgrep_worktree_build,flashgrep,dirty_first_query,")));
    assert!(lines
        .iter()
        .any(|line| line.contains(",flashgrep_worktree,flashgrep,dirty_cached_query,")));
}
