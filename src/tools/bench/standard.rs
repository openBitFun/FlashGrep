use std::{
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig},
    error::{AppError, Result},
    index::IndexSearcher,
    search::SearchMode,
    WorkspaceIndex, WorkspaceIndexOptions,
};

use super::{
    cache::prepare_sample,
    report::{build_summary, RawSample, RawWriter},
    BenchCachePaths, BenchConfig, BenchmarkCase, BitfunOutcome, CacheRunner, RunnerSummary,
};

pub(super) fn run_codgrep_benchmark(
    benchmark: BenchmarkCase,
    index_path: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let searcher = IndexSearcher::open(index_path.to_path_buf())?;
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut candidate_docs = None;
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let _ = run_codgrep_once(&searcher, benchmark, config)?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let started = Instant::now();
        let outcome = run_codgrep_once(&searcher, benchmark, config)?;
        let duration_secs = started.elapsed().as_secs_f64();
        candidate_docs = Some(outcome.candidate_docs);
        match_count = outcome.match_count;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "codgrep",
                iteration: iter,
                duration_secs,
                candidate_docs,
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        "codgrep".into(),
        durations,
        candidate_docs,
        match_count,
    ))
}

pub(super) fn run_codgrep_cli_benchmark(
    benchmark: BenchmarkCase,
    index_path: &Path,
    suite_dir: &Path,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let target = benchmark.corpus.search_target(suite_dir);
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let _ = run_codgrep_cli_once(benchmark, index_path, &target, config)?;
    }

    for iter in 0..config.bench_iter {
        prepare_sample(CacheRunner::Bitfun, cache_paths, config)?;
        let started = Instant::now();
        let matches = run_codgrep_cli_once(benchmark, index_path, &target, config)?;
        let duration_secs = started.elapsed().as_secs_f64();
        match_count = matches;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "codgrep_cli",
                iteration: iter,
                duration_secs,
                candidate_docs: None,
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        "codgrep_cli".into(),
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

fn run_codgrep_once(
    searcher: &IndexSearcher,
    benchmark: BenchmarkCase,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let results = searcher.search(&QueryConfig {
        regex_pattern: benchmark.pattern.to_string(),
        patterns: vec![benchmark.pattern.to_string()],
        case_insensitive: benchmark.case_insensitive,
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

fn run_rg_once(benchmark: BenchmarkCase, target: &Path, config: &BenchConfig) -> Result<usize> {
    let mut command = Command::new("rg");
    command.arg("--count");
    command.arg("--with-filename");
    if matches!(config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
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

fn run_codgrep_cli_once(
    benchmark: BenchmarkCase,
    index_path: &Path,
    target: &Path,
    config: &BenchConfig,
) -> Result<usize> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("--index").arg(index_path);
    command.arg("-c");
    if matches!(config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
    if benchmark.case_insensitive {
        command.arg("-i");
    }
    command.arg("-e").arg(benchmark.pattern);
    command.arg(target);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::InvalidIndex(format!(
            "codgrep cli failed for benchmark {} with status {}",
            benchmark.name, output.status
        )));
    }
    super::parse_rg_count_output(&output.stdout)
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
        build_config: BuildConfig {
            repo_path: repo_path.to_path_buf(),
            index_path: index_path.to_path_buf(),
            tokenizer: config.tokenizer,
            corpus_mode: config.corpus_mode,
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            min_sparse_len: 5,
            max_sparse_len: 16,
        },
        auto_refresh_overlay: false,
        overlay_cache_limit: None,
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
    index_path: &Path,
    pattern: &str,
    case_insensitive: bool,
    build_overlay_each_sample: bool,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    if build_overlay_each_sample {
        reset_overlay_cache(index_path)?;
    }
    let _ = workspace.refresh_overlay()?;
    let snapshot = workspace.snapshot()?;
    run_snapshot_once(&snapshot, pattern, case_insensitive, config)
}

fn reset_overlay_cache(index_path: &Path) -> Result<()> {
    let overlays_dir = index_path.join("overlays");
    if overlays_dir.exists() {
        std::fs::remove_dir_all(&overlays_dir)?;
    }
    Ok(())
}
