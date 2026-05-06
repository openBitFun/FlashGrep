use super::super::fixtures::copy_tree;
use super::*;

#[test]
fn bench_runner_supports_worktree_runners() {
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

    assert_eq!(report.summaries.len(), 2);
    let summary = report
        .summaries
        .iter()
        .find(|summary| summary.name == "linux_literal")
        .expect("test should succeed");
    assert_eq!(summary.runners.len(), 3);
    assert!(summary
        .runners
        .iter()
        .any(|runner| runner.runner == "flashgrep_worktree_build"));
    assert!(summary
        .runners
        .iter()
        .any(|runner| runner.runner == "flashgrep_worktree"));
}

#[cfg(unix)]
#[test]
fn copy_tree_dereferences_symlinked_files_for_worktree_fixtures() {
    use std::os::unix::fs::symlink;

    let temp = tempdir().expect("test should succeed");
    let source = temp.path().join("source");
    let target = temp.path().join("target");

    fs::create_dir_all(source.join("nested")).expect("test should succeed");
    fs::write(source.join("nested").join("real.txt"), "PM_RESUME\n").expect("test should succeed");
    symlink("nested/real.txt", source.join("linked.txt")).expect("test should succeed");

    copy_tree(&source, &target).expect("test should succeed");

    let copied = target.join("linked.txt");
    assert!(copied.is_file());
    assert_eq!(
        fs::read_to_string(copied).expect("test should succeed"),
        "PM_RESUME\n"
    );
}

#[test]
fn sampled_dirty_worktree_fixture_limits_files_but_keeps_required_matches() {
    let temp = tempdir().expect("test should succeed");
    let suite_dir = temp.path().join("suite");
    let repo_dir = temp.path().join("repo");

    fs::create_dir_all(&suite_dir).expect("test should succeed");
    fs::create_dir_all(repo_dir.join("drivers")).expect("test should succeed");
    fs::create_dir_all(repo_dir.join("kernel")).expect("test should succeed");
    fs::write(repo_dir.join("drivers").join("apm.c"), "PM_RESUME\n").expect("test should succeed");
    fs::write(repo_dir.join("kernel").join("ata.c"), "ERR_SYS\n").expect("test should succeed");
    fs::write(repo_dir.join("misc.txt"), "unrelated\n").expect("test should succeed");

    let fixture = prepare_dirty_worktree_fixture(
        &repo_dir,
        &suite_dir,
        "linux-small",
        &BenchConfig {
            suite_dir: suite_dir.clone(),
            filter: None,
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
            worktree_sample_files: Some(2),
            worktree_dirty_files: 2,
            rebuild: true,
            raw_output: None,
            cold_hook: None,
        },
        &[
            DirtyPattern {
                regex_pattern: "PM_RESUME".into(),
                case_insensitive: false,
            },
            DirtyPattern {
                regex_pattern: "ERR_SYS".into(),
                case_insensitive: false,
            },
        ],
    )
    .expect("test should succeed");

    let copied = walkdir::WalkDir::new(&fixture.repo_path)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            entry
                .path()
                .strip_prefix(&fixture.repo_path)
                .expect("test should succeed")
                .to_string_lossy()
                .into_owned()
        })
        .filter(|path| !path.starts_with(".flashgrep-index-engine/"))
        .map(|path| path.replace('\\', "/"))
        .collect::<Vec<_>>();

    assert_eq!(copied.len(), 2);
    assert!(copied.iter().any(|path| path == "drivers/apm.c"));
    assert!(copied.iter().any(|path| path == "kernel/ata.c"));
}

#[test]
fn bench_runner_supports_custom_worktree_build_runner() {
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
        custom_name: Some("flashgrep-worktree".into()),
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

    assert_eq!(report.summaries.len(), 1);
    let summary = &report.summaries[0];
    assert!(summary
        .runners
        .iter()
        .any(|runner| runner.runner == "flashgrep_worktree_build"));
    assert!(summary
        .runners
        .iter()
        .any(|runner| runner.runner == "flashgrep_worktree"));
}
