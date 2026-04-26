use std::{
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use crate::error::{AppError, Result};

use super::{
    cache::prepare_sample,
    configure_rg_command, finish_daemon_run,
    report::{build_summary, RawSample, RawWriter},
    run_daemon_search_count, start_bench_daemon, BenchCachePaths, BenchConfig, BenchmarkCase,
    BitfunOutcome, CacheRunner, RgSearchScope, RunnerSummary,
};

pub(super) fn run_codgrep_benchmark(
    benchmark: BenchmarkCase,
    storage_root: &Path,
    suite_dir: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let target = benchmark.corpus.search_target(suite_dir);
    let daemon = start_bench_daemon(&benchmark.corpus.repo_path(suite_dir), storage_root, config)?;
    let result = (|| {
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
    })();
    finish_daemon_run(daemon, result)
}

pub(super) fn run_rg_benchmark(
    benchmark: BenchmarkCase,
    scope: &RgSearchScope,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Rg, cache_paths, config)?;
        let _ = run_rg_once(benchmark, scope, config)?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Rg, cache_paths, config)?;
        let started = Instant::now();
        let matches = run_rg_once(benchmark, scope, config)?;
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
    storage_root: &Path,
    cache_paths: Option<&BenchCachePaths>,
    build_overlay_each_sample: bool,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let daemon = (!build_overlay_each_sample)
        .then(|| start_bench_daemon(repo_path, storage_root, config))
        .transpose()?;
    let result = (|| {
        let mut durations = Vec::with_capacity(config.bench_iter);
        let mut candidate_docs = None;
        let mut match_count = 0;

        for _ in 0..config.warmup_iter {
            prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
            let _ = run_worktree_once(
                daemon.as_ref(),
                repo_path,
                storage_root,
                benchmark,
                build_overlay_each_sample,
                config,
            )?;
        }

        for iter in 0..config.bench_iter {
            prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
            let started = Instant::now();
            let outcome = run_worktree_once(
                daemon.as_ref(),
                repo_path,
                storage_root,
                benchmark,
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
    })();
    match daemon {
        Some(daemon) => finish_daemon_run(daemon, result),
        None => result,
    }
}

fn run_rg_once(
    benchmark: BenchmarkCase,
    scope: &RgSearchScope,
    config: &BenchConfig,
) -> Result<usize> {
    let mut command = Command::new("rg");
    configure_rg_command(&mut command, config);
    if benchmark.case_insensitive {
        command.arg("-i");
    }
    if let Some(current_dir) = scope.current_dir.as_ref() {
        command.current_dir(current_dir);
    }
    if let Some(ignore_file) = scope.ignore_file.as_ref() {
        command.arg("--ignore-file");
        command.arg(ignore_file);
    }
    command.arg(benchmark.pattern);
    command.arg(&scope.target);
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

fn run_worktree_query(
    daemon: &super::BenchDaemon,
    benchmark: BenchmarkCase,
    repo_path: &Path,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let target = relocate_worktree_target(
        &benchmark.corpus.repo_path(&config.suite_dir),
        &benchmark.corpus.search_target(&config.suite_dir),
        repo_path,
    )?;
    run_daemon_search_count(
        daemon,
        repo_path,
        benchmark.pattern,
        benchmark.case_insensitive,
        config,
        vec![target],
    )
}

fn relocate_worktree_target(
    source_repo: &Path,
    source_target: &Path,
    fixture_repo: &Path,
) -> Result<std::path::PathBuf> {
    if source_target == source_repo {
        return Ok(fixture_repo.to_path_buf());
    }

    if let Ok(relative) = source_target.strip_prefix(source_repo) {
        return Ok(fixture_repo.join(relative));
    }

    Err(AppError::InvalidIndex(format!(
        "failed to relocate worktree benchmark target {} into fixture {}",
        source_target.display(),
        fixture_repo.display()
    )))
}

fn run_worktree_once(
    daemon: Option<&super::BenchDaemon>,
    repo_path: &Path,
    storage_root: &Path,
    benchmark: BenchmarkCase,
    build_overlay_each_sample: bool,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    if build_overlay_each_sample {
        let daemon = start_bench_daemon(repo_path, storage_root, config)?;
        let result = run_worktree_query(&daemon, benchmark, repo_path, config);
        finish_daemon_run(daemon, result)
    } else {
        let daemon = daemon.ok_or_else(|| {
            AppError::Protocol("missing cached daemon for dirty worktree benchmark".into())
        })?;
        run_worktree_query(daemon, benchmark, repo_path, config)
    }
}
