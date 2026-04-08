use std::{
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    search::SearchMode,
    WorkspaceIndex, WorkspaceIndexOptions,
};

use super::{
    bench_build_config,
    cache::prepare_sample,
    configure_rg_command,
    report::{build_summary, RawSample, RawWriter},
    run_daemon_search_count, start_bench_daemon, BenchCachePaths, BenchConfig, BenchmarkCase,
    BitfunOutcome, CacheRunner, RunnerSummary,
};

pub(super) fn run_codgrep_benchmark(
    benchmark: BenchmarkCase,
    index_path: &Path,
    suite_dir: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let target = benchmark.corpus.search_target(suite_dir);
    let daemon = start_bench_daemon(&benchmark.corpus.repo_path(suite_dir), index_path, config)?;
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let _ = run_codgrep_daemon_once(&daemon, benchmark, &target, config)?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let started = Instant::now();
        let matches = run_codgrep_daemon_once(&daemon, benchmark, &target, config)?;
        let duration_secs = started.elapsed().as_secs_f64();
        match_count = matches.match_count;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "codgrep",
                iteration: iter,
                duration_secs,
                candidate_docs: Some(matches.candidate_docs),
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        "codgrep".into(),
        durations,
        None,
        match_count,
    ))
}

pub(super) fn run_rg_benchmark(
    benchmark: BenchmarkCase,
    suite_dir: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let target = benchmark.corpus.search_target(suite_dir);
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Rg, cache_paths, config)?;
        let _ = run_rg_once(benchmark, &target, config)?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Rg, cache_paths, config)?;
        let started = Instant::now();
        let matches = run_rg_once(benchmark, &target, config)?;
        let duration_secs = started.elapsed().as_secs_f64();
        match_count = matches;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "rg",
                iteration: iter,
                duration_secs,
                candidate_docs: None,
                match_count,
            })?;
        }
    }

    Ok(build_summary("rg".into(), durations, None, match_count))
}

pub(super) fn run_worktree_benchmark(
    benchmark: BenchmarkCase,
    repo_path: &Path,
    index_path: &Path,
    cache_paths: Option<&BenchCachePaths>,
    build_overlay_each_sample: bool,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let workspace = open_workspace(repo_path, index_path, config)?;
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut candidate_docs = None;
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let _ = run_worktree_once(
            &workspace,
            index_path,
            benchmark.pattern,
            benchmark.case_insensitive,
            build_overlay_each_sample,
            config,
        )?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let started = Instant::now();
        let outcome = run_worktree_once(
            &workspace,
            index_path,
            benchmark.pattern,
            benchmark.case_insensitive,
            build_overlay_each_sample,
            config,
        )?;
        let duration_secs = started.elapsed().as_secs_f64();
        candidate_docs = Some(outcome.candidate_docs);
        match_count = outcome.match_count;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: if build_overlay_each_sample {
                    "codgrep_worktree_build"
                } else {
                    "codgrep_worktree"
                },
                iteration: iter,
                duration_secs,
                candidate_docs,
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        if build_overlay_each_sample {
            "codgrep_worktree_build".into()
        } else {
            "codgrep_worktree".into()
        },
        durations,
        candidate_docs,
        match_count,
    ))
}

pub(super) fn run_workspace_benchmark(
    benchmark: BenchmarkCase,
    index_path: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let workspace = open_workspace(
        &benchmark.corpus.repo_path(&config.suite_dir),
        index_path,
        config,
    )?;
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut candidate_docs = None;
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let _ = run_workspace_once(
            &workspace,
            benchmark.pattern,
            benchmark.case_insensitive,
            config,
        )?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let started = Instant::now();
        let outcome = run_workspace_once(
            &workspace,
            benchmark.pattern,
            benchmark.case_insensitive,
            config,
        )?;
        let duration_secs = started.elapsed().as_secs_f64();
        candidate_docs = Some(outcome.candidate_docs);
        match_count = outcome.match_count;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "codgrep_workspace",
                iteration: iter,
                duration_secs,
                candidate_docs,
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        "codgrep_workspace".into(),
        durations,
        candidate_docs,
        match_count,
    ))
}

fn run_rg_once(benchmark: BenchmarkCase, target: &Path, config: &BenchConfig) -> Result<usize> {
    let mut command = Command::new("rg");
    configure_rg_command(&mut command, config);
    if benchmark.case_insensitive {
        command.arg("-i");
    }
    command.arg(benchmark.pattern);
    command.arg(target);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::InvalidIndex(format!(
            "rg failed for benchmark {} with status {}",
            benchmark.name, output.status
        )));
    }
    super::parse_rg_count_output(&output.stdout)
}

fn run_codgrep_daemon_once(
    daemon: &super::BenchDaemon,
    benchmark: BenchmarkCase,
    target: &Path,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    run_daemon_search_count(
        daemon,
        &benchmark.corpus.repo_path(&config.suite_dir),
        benchmark.pattern,
        benchmark.case_insensitive,
        config,
        vec![target.to_path_buf()],
    )
}

fn run_snapshot_once(
    snapshot: &crate::workspace::WorkspaceSnapshot,
    pattern: &str,
    case_insensitive: bool,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let results = snapshot.search(&QueryConfig {
        regex_pattern: pattern.to_string(),
        patterns: vec![pattern.to_string()],
        case_insensitive,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: config.top_k_tokens,
        search_mode: SearchMode::CountOnly,
        ..QueryConfig::default()
    })?;
    Ok(BitfunOutcome {
        candidate_docs: results.candidate_docs,
        match_count: results.matched_lines,
    })
}

fn open_workspace(
    repo_path: &Path,
    index_path: &Path,
    config: &BenchConfig,
) -> Result<WorkspaceIndex> {
    WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: bench_build_config(repo_path, index_path, config),
    })
}

fn run_workspace_once(
    workspace: &WorkspaceIndex,
    pattern: &str,
    case_insensitive: bool,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let snapshot = workspace.snapshot()?;
    run_snapshot_once(&snapshot, pattern, case_insensitive, config)
}

fn run_worktree_once(
    workspace: &WorkspaceIndex,
    _index_path: &Path,
    pattern: &str,
    case_insensitive: bool,
    _build_overlay_each_sample: bool,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let snapshot = workspace.snapshot()?;
    run_snapshot_once(&snapshot, pattern, case_insensitive, config)
}
