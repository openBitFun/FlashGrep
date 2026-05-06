use super::*;

#[test]
fn bench_runner_supports_custom_repo_patterns() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(&repo_dir).expect("test should succeed");
    fs::write(repo_dir.join("main.ts"), "const APP = 'BitFun';\n").expect("test should succeed");

    let report = run(&BenchConfig {
        suite_dir,
        filter: None,
        custom_repo: Some(repo_dir),
        custom_patterns: vec!["BitFun".into()],
        custom_name: Some("flashgrep-custom".into()),
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

    assert_eq!(report.summaries.len(), 1);
    let summary = &report.summaries[0];
    assert_eq!(summary.name, "flashgrep-custom");
    assert_eq!(summary.pattern, "BitFun");
    assert_eq!(summary.target, "flashgrep-custom");
    assert!(summary
        .runners
        .iter()
        .any(|runner| runner.runner == "flashgrep"));
}

#[test]
fn bench_daemon_shutdown_removes_state_file() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");
    let storage_root = suite_dir
        .join(".flashgrep-bench")
        .join("custom-runtime")
        .join("repo-repro-ignore-sparse-ngram");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(&repo_dir).expect("test should succeed");
    fs::write(repo_dir.join("main.ts"), "const APP = 'BitFun';\n").expect("test should succeed");

    let config = BenchConfig {
        suite_dir,
        filter: None,
        custom_repo: Some(repo_dir.clone()),
        custom_patterns: vec!["BitFun".into()],
        custom_name: Some("repro".into()),
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Same,
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
    };
    ensure_bench_base_snapshot(&bench_build_config(&repo_dir, &storage_root, &config), true)
        .expect("test should succeed");

    let daemon =
        start_bench_daemon(&repo_dir, &storage_root, &config).expect("test should succeed");
    let state_file = daemon.state_file.clone();
    assert!(state_file.exists());

    daemon.shutdown().expect("shutdown should succeed");
    assert!(!state_file.exists());
}

#[test]
fn bench_rg_runner_uses_snapshot_visible_scope_for_custom_repo() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(&repo_dir).expect("test should succeed");
    fs::write(repo_dir.join("main.ts"), "const APP = 'BitFun';\n").expect("test should succeed");
    fs::write(repo_dir.join("latin1.txt"), b"BitFun\xff\n").expect("test should succeed");

    let report = run(&BenchConfig {
        suite_dir,
        filter: None,
        custom_repo: Some(repo_dir),
        custom_patterns: vec!["BitFun".into()],
        custom_name: Some("flashgrep-custom-rg-scope".into()),
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 1,
        top_k_tokens: 4,
        compare_rg: true,
        compare_worktree: false,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: None,
        cold_hook: None,
    })
    .expect("test should succeed");

    let summary = &report.summaries[0];
    let flashgrep = summary
        .runners
        .iter()
        .find(|runner| runner.runner == "flashgrep")
        .expect("test should succeed");
    let rg = summary
        .runners
        .iter()
        .find(|runner| runner.runner == "rg")
        .expect("test should succeed");
    assert_eq!(flashgrep.match_count, 2);
    assert_eq!(rg.match_count, 2);
}

#[test]
fn prepare_rg_search_scope_keeps_snapshot_visible_non_utf8_files() {
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");
    let storage_root = temp.path().join("index");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(&repo_dir).expect("test should succeed");
    fs::write(repo_dir.join("main.ts"), "const APP = 'BitFun';\n").expect("test should succeed");
    fs::write(repo_dir.join("latin1.txt"), b"BitFun\xff\n").expect("test should succeed");

    let config = BenchConfig {
        suite_dir,
        filter: None,
        custom_repo: Some(repo_dir.clone()),
        custom_patterns: vec!["BitFun".into()],
        custom_name: Some("flashgrep-custom-rg-scope".into()),
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 1,
        top_k_tokens: 4,
        compare_rg: true,
        compare_worktree: false,
        worktree_sample_files: None,
        worktree_dirty_files: 1,
        rebuild: true,
        raw_output: None,
        cold_hook: None,
    };
    ensure_bench_base_snapshot(&bench_build_config(&repo_dir, &storage_root, &config), true)
        .expect("test should succeed");

    let scope =
        prepare_rg_search_scope(&repo_dir, &storage_root, &config).expect("test should succeed");
    assert!(scope.ignore_file.is_none());
}

#[test]
fn bench_runner_uses_trace_mode_for_multiple_custom_patterns() {
    if !supports_tcp_daemon_tests() {
        return;
    }
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(&repo_dir).expect("test should succeed");
    fs::write(
        repo_dir.join("main.ts"),
        "const APP = 'BitFun';\nconst TOOL = 'Search';\n",
    )
    .expect("test should succeed");

    let report = run(&BenchConfig {
        suite_dir,
        filter: None,
        custom_repo: Some(repo_dir),
        custom_patterns: vec!["BitFun".into(), "Search".into()],
        custom_name: Some("flashgrep-trace".into()),
        custom_case_insensitive: false,
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        cache_mode: BenchCacheMode::Warm,
        query_mode: super::super::BenchQueryMode::Trace,
        warmup_iter: 0,
        bench_iter: 2,
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

    assert_eq!(report.summaries.len(), 1);
    let summary = &report.summaries[0];
    assert_eq!(summary.name, "flashgrep-trace");
    assert_eq!(summary.pattern, "trace[2]");
    assert_eq!(summary.target, "flashgrep-trace");
    assert!(summary
        .runners
        .iter()
        .all(|runner| runner.sample_count == 2));
}
