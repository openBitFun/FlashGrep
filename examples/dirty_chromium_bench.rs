use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flashgrep::{
    daemon::{
        protocol::{
            EnsureRepoParams, PathScope, QuerySpec, RepoConfig, RepoRef, Request, SearchModeConfig,
            SearchParams,
        },
        DaemonClient, ManagedDaemonClient,
    },
    BuildConfig, CorpusMode, Result, SearchEngine, TokenizerMode,
};

const DEFAULT_QUERY: &str = "ScopedAllowBlockingForTesting";
const DEFAULT_BENCH_ITERS: usize = 5;
const DEFAULT_SCENARIO_SAMPLES: usize = 3;
const DEFAULT_SIZES: [usize; 5] = [1, 10, 30, 100, 300];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BenchMode {
    Search,
    Detect,
}

#[derive(Clone, Debug)]
struct Scenario {
    name: String,
    modify_count: usize,
    add_count: usize,
    delete_count: usize,
}

#[derive(Debug)]
struct ScenarioResult {
    scenario: String,
    edited_files: usize,
    samples: usize,
    rg_matched_lines_mean: f64,
    flashgrep_matched_lines_mean: f64,
    missing_lines_mean: f64,
    recall_ratio_mean: f64,
    settle_backend: String,
    detection_ms_mean: f64,
    detection_ms_stddev: f64,
    settle_ms_mean: f64,
    settle_ms_stddev: f64,
    flashgrep_query_ms_mean: f64,
    flashgrep_query_ms_stddev: f64,
    rg_median_ms_mean: f64,
    rg_median_ms_stddev: f64,
    status_dirty_paths_mean: f64,
}

#[derive(Debug)]
struct ScenarioSampleResult {
    scenario: String,
    sample_index: usize,
    edited_files: usize,
    rg_matched_lines: usize,
    flashgrep_matched_lines: usize,
    missing_lines: usize,
    recall_ratio: f64,
    settle_backend: String,
    detection_ms: f64,
    settle_ms: f64,
    flashgrep_query_ms: f64,
    rg_median_ms: f64,
    status_dirty_paths: usize,
}

#[derive(Debug)]
struct DetectionResult {
    scenario: String,
    sample_index: usize,
    modify_count: usize,
    add_count: usize,
    delete_count: usize,
    expected_dirty_paths: usize,
    detected_dirty_paths: usize,
    detection_ms: f64,
}

#[derive(Clone, Debug)]
struct SearchMeasurement {
    matched_lines: usize,
    backend: String,
    elapsed_ms: f64,
}

#[derive(Clone, Debug)]
struct CandidateProfile {
    relative_path: PathBuf,
    matched_lines: usize,
    byte_len: usize,
}

#[derive(Clone, Debug)]
struct BaselineFile {
    path: PathBuf,
    contents: Vec<u8>,
}

#[derive(Debug)]
struct DaemonRepo {
    repo_id: String,
    client: DaemonClient,
}

#[derive(Debug)]
struct BenchWorkspace {
    cleanup_detached_worktree: bool,
    source_repo: Option<PathBuf>,
    cleanup_root: bool,
    root: Option<PathBuf>,
    worktree_path: PathBuf,
    storage_root: PathBuf,
}

impl BenchWorkspace {
    fn new(source_repo: &Path, in_place: bool) -> Result<Self> {
        if let Some(worktree_path) = configured_path("FLASHGREP_BENCH_WORKTREE")? {
            let storage_root =
                configured_path("FLASHGREP_BENCH_STORAGE_ROOT")?.ok_or_else(|| {
                    flashgrep::AppError::InvalidIndex(
                    "FLASHGREP_BENCH_STORAGE_ROOT is required when FLASHGREP_BENCH_WORKTREE is set"
                        .into(),
                )
                })?;
            return Ok(Self {
                cleanup_detached_worktree: false,
                source_repo: None,
                cleanup_root: false,
                root: None,
                worktree_path,
                storage_root,
            });
        }

        if in_place {
            let storage_root = configured_path("FLASHGREP_BENCH_STORAGE_ROOT")?
                .unwrap_or_else(|| source_repo.join(".flashgrep-dirty-bench-index-engine"));
            return Ok(Self {
                cleanup_detached_worktree: false,
                source_repo: None,
                cleanup_root: false,
                root: None,
                worktree_path: source_repo.to_path_buf(),
                storage_root,
            });
        }

        if let Some(root) = configured_path("FLASHGREP_BENCH_ROOT")? {
            fs::create_dir_all(&root)?;
            let worktree_path = root.join("repo");
            if !worktree_path.exists() {
                run_git(
                    source_repo,
                    [
                        "worktree",
                        "add",
                        "--detach",
                        worktree_path.to_string_lossy().as_ref(),
                        "HEAD",
                    ],
                )?;
            }
            let storage_root = configured_path("FLASHGREP_BENCH_STORAGE_ROOT")?
                .unwrap_or_else(|| root.join("index"));
            return Ok(Self {
                cleanup_detached_worktree: false,
                source_repo: Some(source_repo.to_path_buf()),
                cleanup_root: false,
                root: Some(root),
                worktree_path,
                storage_root,
            });
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let root = env::temp_dir().join(format!(
            "flashgrep-dirty-bench-{timestamp}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root)?;
        let worktree_path = root.join("repo");
        run_git(
            source_repo,
            [
                "worktree",
                "add",
                "--detach",
                worktree_path.to_string_lossy().as_ref(),
                "HEAD",
            ],
        )?;
        Ok(Self {
            cleanup_detached_worktree: true,
            source_repo: Some(source_repo.to_path_buf()),
            cleanup_root: true,
            root: Some(root.clone()),
            worktree_path,
            storage_root: root.join("index"),
        })
    }

    fn reset_start_clean(&self, baseline_files: &[BaselineFile]) -> Result<()> {
        if let Some(source_repo) = self.source_repo.as_ref() {
            if *source_repo != self.worktree_path {
                run_git(&self.worktree_path, ["reset", "--hard", "HEAD"])?;
                let added_root = self.worktree_path.join("flashgrep_bench_added");
                if added_root.exists() {
                    fs::remove_dir_all(added_root)?;
                }
                return Ok(());
            }
        }
        restore_baseline_files(baseline_files)?;
        let added_root = self.worktree_path.join("flashgrep_bench_added");
        if added_root.exists() {
            fs::remove_dir_all(added_root)?;
        }
        Ok(())
    }
}

impl Drop for BenchWorkspace {
    fn drop(&mut self) {
        if self.cleanup_detached_worktree {
            if let Some(source_repo) = self.source_repo.as_ref() {
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(source_repo)
                    .args(["worktree", "remove", "--force"])
                    .arg(&self.worktree_path)
                    .status();
            }
        }
        if self.cleanup_root {
            if let Some(root) = self.root.as_ref() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let source_repo = args.next().map_or_else(
        || PathBuf::from("/Users/user/workspace/chromium-src"),
        PathBuf::from,
    );
    let query = args
        .next()
        .or_else(|| env::var("FLASHGREP_DIRTY_BENCH_QUERY").ok())
        .unwrap_or_else(|| DEFAULT_QUERY.to_string());
    let source_repo = fs::canonicalize(source_repo)?;
    let in_place = env::var("FLASHGREP_BENCH_IN_PLACE").ok().as_deref() == Some("1");
    let bench_mode = bench_mode();
    let bench_iters = bench_iters();
    let scenario_samples = scenario_samples();
    let sizes = scenario_sizes();
    let max_size = sizes.iter().copied().max().unwrap_or_default();
    let worktree = BenchWorkspace::new(&source_repo, in_place)?;
    let storage_root = worktree.storage_root.clone();

    println!("source_repo={}", source_repo.display());
    println!("worktree={}", worktree.worktree_path.display());
    println!("storage_root={}", storage_root.display());
    println!("in_place={in_place}");
    println!("bench_mode={bench_mode:?}");
    println!("query={query}");
    println!("sizes={sizes:?}");
    println!("bench_iters={bench_iters}");
    println!("scenario_samples={scenario_samples}");

    let build_started = Instant::now();
    let build_config = BuildConfig {
        git_mode: Default::default(),
        repo_path: worktree.worktree_path.clone(),
        storage_root: storage_root.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size: 512 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let prepare_mode =
        if flashgrep::index_engine::WorkspaceBaseSnapshotIndex::read_snapshot_metadata(
            &build_config,
        )?
        .is_some()
        {
            eprintln!(
                "reusing existing base snapshot for {}",
                worktree.worktree_path.display()
            );
            "reused_base_snapshot"
        } else {
            eprintln!(
                "building base snapshot for {}",
                worktree.worktree_path.display()
            );
            SearchEngine::new(build_config.clone()).build_base_snapshot()?;
            "built_base_snapshot"
        };
    let build_elapsed_ms = elapsed_ms(build_started.elapsed().as_secs_f64());

    let candidate_relative_paths = git_grep_files(&source_repo, &query)?;
    if candidate_relative_paths.len() < max_size {
        return Err(flashgrep::AppError::InvalidIndex(format!(
            "expected at least {max_size} tracked files for query {query}, found {}",
            candidate_relative_paths.len()
        )));
    }
    let selected_relative_paths =
        select_controlled_candidate_paths(&source_repo, &candidate_relative_paths, &query)?;
    let candidate_files = selected_relative_paths
        .iter()
        .map(|relative| worktree.worktree_path.join(relative))
        .collect::<Vec<_>>();
    println!("candidate_files_total={}", candidate_relative_paths.len());
    println!("candidate_files_selected={}", candidate_files.len());
    println!("index_prepare_mode={prepare_mode}");
    println!("index_prepare_ms={build_elapsed_ms:.2}");
    println!();
    let baseline_files = load_baseline_files(
        &source_repo,
        &worktree.worktree_path,
        &selected_relative_paths,
    )?;
    worktree.reset_start_clean(&baseline_files)?;

    let scenario_filter = env::var("FLASHGREP_BENCH_SCENARIOS").ok();
    let scenarios = filter_scenarios(build_scenarios(&sizes), scenario_filter.as_deref());
    let cg_path = flashgrep_executable()?;
    let mut sample_results = Vec::new();
    let mut results = Vec::with_capacity(scenarios.len());
    let mut detection_results = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        let mut scenario_samples_results = Vec::with_capacity(scenario_samples);
        for sample_index in 0..scenario_samples {
            eprintln!(
                "running scenario {} sample {}/{}",
                scenario.name,
                sample_index + 1,
                scenario_samples
            );
            worktree.reset_start_clean(&baseline_files)?;
            let scenario_candidates = select_scenario_candidates(
                &candidate_files,
                scenario.modify_count + scenario.delete_count,
                sample_index,
                scenario_samples,
            )?;
            let repo = open_flashgrep_repo(&cg_path, &worktree.worktree_path, &storage_root)?;
            wait_for_dirty_paths(&repo, 0, Duration::from_secs(180))?;
            apply_scenario(
                &worktree.worktree_path,
                &scenario,
                &scenario_candidates,
                &query,
            )?;
            let expected_dirty_paths =
                scenario.modify_count + scenario.add_count + scenario.delete_count;

            if bench_mode == BenchMode::Detect {
                let detection_ms =
                    wait_for_dirty_paths(&repo, expected_dirty_paths, Duration::from_secs(180))?;
                let detected_dirty_paths = repo_dirty_paths(&repo)?;
                detection_results.push(DetectionResult {
                    scenario: scenario.name.clone(),
                    sample_index,
                    modify_count: scenario.modify_count,
                    add_count: scenario.add_count,
                    delete_count: scenario.delete_count,
                    expected_dirty_paths,
                    detected_dirty_paths,
                    detection_ms,
                });
                shutdown_daemon(&repo)?;
                continue;
            }

            let mut rg_times = Vec::with_capacity(bench_iters);
            let mut rg_count = None;
            for _ in 0..bench_iters {
                let (count, elapsed_ms) = run_rg_query(&worktree.worktree_path, &query)?;
                rg_times.push(elapsed_ms);
                rg_count.get_or_insert(count);
            }

            let rg_count = rg_count.ok_or_else(|| {
                flashgrep::AppError::ValueOutOfRange(
                    "benchmark iterations must be greater than zero".into(),
                )
            })?;
            let rg_median = median_ms(rg_times);
            let detection_ms =
                wait_for_dirty_paths(&repo, expected_dirty_paths, Duration::from_secs(180))?;
            let query_measurement = run_flashgrep_query(&repo, &query)?;
            let settle_measurement = query_measurement.clone();
            let settle_ms = detection_ms;
            let status_dirty_paths = repo_dirty_paths(&repo)?;
            let missing_lines = rg_count.saturating_sub(query_measurement.matched_lines);
            let recall_ratio = if rg_count == 0 {
                1.0
            } else {
                query_measurement.matched_lines as f64 / rg_count as f64
            };

            let sample_result = ScenarioSampleResult {
                scenario: scenario.name.clone(),
                sample_index,
                edited_files: expected_dirty_paths,
                rg_matched_lines: rg_count,
                flashgrep_matched_lines: query_measurement.matched_lines,
                missing_lines,
                recall_ratio,
                settle_backend: settle_measurement.backend,
                detection_ms,
                settle_ms,
                flashgrep_query_ms: query_measurement.elapsed_ms,
                rg_median_ms: rg_median,
                status_dirty_paths,
            };
            scenario_samples_results.push(sample_result);
            shutdown_daemon(&repo)?;
        }
        if bench_mode == BenchMode::Search {
            sample_results.extend(scenario_samples_results.iter().map(clone_sample_result));
            results.push(aggregate_scenario_results(
                &scenario.name,
                scenario.modify_count + scenario.add_count + scenario.delete_count,
                &scenario_samples_results,
            ));
        }
    }

    worktree.reset_start_clean(&baseline_files)?;
    match bench_mode {
        BenchMode::Search => print_results(build_elapsed_ms, &sample_results, &results),
        BenchMode::Detect => print_detection_results(build_elapsed_ms, &detection_results),
    }
    Ok(())
}

fn build_scenarios(sizes: &[usize]) -> Vec<Scenario> {
    sizes
        .iter()
        .copied()
        .map(|size| Scenario {
            name: format!("modify-{size}"),
            modify_count: size,
            add_count: 0,
            delete_count: 0,
        })
        .collect()
}

fn filter_scenarios(scenarios: Vec<Scenario>, filter: Option<&str>) -> Vec<Scenario> {
    let Some(filter) = filter else {
        return scenarios;
    };
    let wanted = filter
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    scenarios
        .into_iter()
        .filter(|scenario| wanted.contains(scenario.name.as_str()))
        .collect()
}

fn apply_scenario(
    repo_path: &Path,
    scenario: &Scenario,
    candidates: &[PathBuf],
    query: &str,
) -> Result<()> {
    let mut cursor = 0usize;

    for index in 0..scenario.modify_count {
        let path = &candidates[cursor];
        cursor += 1;
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        writeln!(file, "\n// FLASHGREP_DIRTY_BENCH_MOD_{index}: {query}")?;
    }

    for _ in 0..scenario.delete_count {
        let path = &candidates[cursor];
        cursor += 1;
        fs::remove_file(path)?;
    }

    if scenario.add_count > 0 {
        let root = repo_path.join("flashgrep_bench_added").join(&scenario.name);
        fs::create_dir_all(&root)?;
        for index in 0..scenario.add_count {
            let path = root.join(format!("added_{index:04}.cc"));
            fs::write(
                path,
                format!(
                    "// synthetic dirty benchmark file\nvoid bench_{index}() {{ /* {query} */ }}\n"
                ),
            )?;
        }
    }

    Ok(())
}

fn clone_sample_result(result: &ScenarioSampleResult) -> ScenarioSampleResult {
    ScenarioSampleResult {
        scenario: result.scenario.clone(),
        sample_index: result.sample_index,
        edited_files: result.edited_files,
        rg_matched_lines: result.rg_matched_lines,
        flashgrep_matched_lines: result.flashgrep_matched_lines,
        missing_lines: result.missing_lines,
        recall_ratio: result.recall_ratio,
        settle_backend: result.settle_backend.clone(),
        detection_ms: result.detection_ms,
        settle_ms: result.settle_ms,
        flashgrep_query_ms: result.flashgrep_query_ms,
        rg_median_ms: result.rg_median_ms,
        status_dirty_paths: result.status_dirty_paths,
    }
}

fn aggregate_scenario_results(
    scenario: &str,
    edited_files: usize,
    samples: &[ScenarioSampleResult],
) -> ScenarioResult {
    let settle_backend = samples
        .iter()
        .map(|sample| sample.settle_backend.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("|");
    ScenarioResult {
        scenario: scenario.to_string(),
        edited_files,
        samples: samples.len(),
        rg_matched_lines_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.rg_matched_lines as f64)
                .collect::<Vec<_>>(),
        ),
        flashgrep_matched_lines_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.flashgrep_matched_lines as f64)
                .collect::<Vec<_>>(),
        ),
        missing_lines_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.missing_lines as f64)
                .collect::<Vec<_>>(),
        ),
        recall_ratio_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.recall_ratio)
                .collect::<Vec<_>>(),
        ),
        settle_backend,
        detection_ms_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.detection_ms)
                .collect::<Vec<_>>(),
        ),
        detection_ms_stddev: stddev(
            &samples
                .iter()
                .map(|sample| sample.detection_ms)
                .collect::<Vec<_>>(),
        ),
        settle_ms_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.settle_ms)
                .collect::<Vec<_>>(),
        ),
        settle_ms_stddev: stddev(
            &samples
                .iter()
                .map(|sample| sample.settle_ms)
                .collect::<Vec<_>>(),
        ),
        flashgrep_query_ms_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.flashgrep_query_ms)
                .collect::<Vec<_>>(),
        ),
        flashgrep_query_ms_stddev: stddev(
            &samples
                .iter()
                .map(|sample| sample.flashgrep_query_ms)
                .collect::<Vec<_>>(),
        ),
        rg_median_ms_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.rg_median_ms)
                .collect::<Vec<_>>(),
        ),
        rg_median_ms_stddev: stddev(
            &samples
                .iter()
                .map(|sample| sample.rg_median_ms)
                .collect::<Vec<_>>(),
        ),
        status_dirty_paths_mean: mean(
            &samples
                .iter()
                .map(|sample| sample.status_dirty_paths as f64)
                .collect::<Vec<_>>(),
        ),
    }
}

fn open_flashgrep_repo(
    cg_path: &Path,
    repo_path: &Path,
    storage_root: &Path,
) -> Result<DaemonRepo> {
    let client = ManagedDaemonClient::new()
        .with_daemon_program(OsString::from(cg_path))
        .with_start_timeout(std::time::Duration::from_secs(120));
    let ensured = client.ensure_repo(EnsureRepoParams {
        repo_path: repo_path.to_path_buf(),
        storage_root: Some(storage_root.to_path_buf()),
        config: RepoConfig::default(),
        refresh: Default::default(),
    })?;
    Ok(DaemonRepo {
        repo_id: ensured.repo_id,
        client: DaemonClient::new(ensured.addr),
    })
}

fn run_flashgrep_query(repo: &DaemonRepo, query: &str) -> Result<SearchMeasurement> {
    let started = Instant::now();
    let outcome = repo.client.send(Request::Search {
        params: SearchParams {
            repo_id: repo.repo_id.clone(),
            query: count_only_query(query),
            scope: PathScope::default(),
        },
    })?;
    let elapsed_ms = elapsed_ms(started.elapsed().as_secs_f64());

    match outcome {
        flashgrep::daemon::protocol::Response::SearchCompleted {
            backend, results, ..
        } => Ok(SearchMeasurement {
            matched_lines: results.matched_lines,
            backend: format!("{backend:?}"),
            elapsed_ms,
        }),
        other => Err(flashgrep::AppError::Protocol(format!(
            "unexpected search response: {other:?}"
        ))),
    }
}

fn shutdown_daemon(repo: &DaemonRepo) -> Result<()> {
    match repo.client.send(Request::Shutdown)? {
        flashgrep::daemon::protocol::Response::ShutdownAck => Ok(()),
        other => Err(flashgrep::AppError::Protocol(format!(
            "unexpected shutdown response: {other:?}"
        ))),
    }
}

fn wait_for_dirty_paths(repo: &DaemonRepo, expected: usize, timeout: Duration) -> Result<f64> {
    let started = Instant::now();
    let poll_interval = Duration::from_millis(200);
    loop {
        let response = repo.client.send(Request::GetRepoStatus {
            params: RepoRef {
                repo_id: repo.repo_id.clone(),
            },
        })?;
        match response {
            flashgrep::daemon::protocol::Response::RepoStatus { status } => {
                let dirty_paths = status.dirty_files.modified
                    + status.dirty_files.deleted
                    + status.dirty_files.new;
                if dirty_paths == expected {
                    return Ok(elapsed_ms(started.elapsed().as_secs_f64()));
                }
                if started.elapsed() >= timeout {
                    return Err(flashgrep::AppError::Protocol(format!(
                        "timed out waiting for dirty path count {expected}; last observed {dirty_paths}"
                    )));
                }
            }
            other => {
                return Err(flashgrep::AppError::Protocol(format!(
                    "unexpected repo status response: {other:?}"
                )))
            }
        }
        thread::sleep(poll_interval);
    }
}

fn repo_dirty_paths(repo: &DaemonRepo) -> Result<usize> {
    let response = repo.client.send(Request::GetRepoStatus {
        params: RepoRef {
            repo_id: repo.repo_id.clone(),
        },
    })?;
    match response {
        flashgrep::daemon::protocol::Response::RepoStatus { status } => {
            Ok(status.dirty_files.modified + status.dirty_files.deleted + status.dirty_files.new)
        }
        other => Err(flashgrep::AppError::Protocol(format!(
            "unexpected repo status response: {other:?}"
        ))),
    }
}

fn run_rg_query(repo_path: &Path, query: &str) -> Result<(usize, f64)> {
    let started = Instant::now();
    let output = Command::new("rg")
        .current_dir(repo_path)
        .args([
            "--count",
            "--with-filename",
            "--fixed-strings",
            "--max-filesize",
            "536870912",
            query,
            ".",
        ])
        .output()?;
    let elapsed_ms = elapsed_ms(started.elapsed().as_secs_f64());

    if !output.status.success() && output.status.code() != Some(1) {
        return Err(flashgrep::AppError::InvalidIndex(format!(
            "rg failed with status {}",
            output.status
        )));
    }

    let count = parse_rg_count_output(&output.stdout)?;
    Ok((count, elapsed_ms))
}

fn count_only_query(pattern: impl Into<String>) -> QuerySpec {
    let pattern = pattern.into();
    QuerySpec {
        freshness: Default::default(),
        pattern: pattern.clone(),
        patterns: vec![pattern],
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        fixed_strings: true,
        word_regexp: false,
        line_regexp: false,
        top_k_tokens: 6,
        max_count: None,
        global_max_results: None,
        search_mode: SearchModeConfig::CountOnly,
    }
}

fn parse_rg_count_output(stdout: &[u8]) -> Result<usize> {
    let text = String::from_utf8_lossy(stdout);
    let mut total = 0usize;
    for line in text.lines() {
        if let Some((_, count)) = line.rsplit_once(':') {
            total += count.trim().parse::<usize>().map_err(|error| {
                flashgrep::AppError::InvalidIndex(format!(
                    "failed to parse rg count line {line:?}: {error}"
                ))
            })?;
        } else if !line.trim().is_empty() {
            total += line.trim().parse::<usize>().map_err(|error| {
                flashgrep::AppError::InvalidIndex(format!(
                    "failed to parse single-count rg output {line:?}: {error}"
                ))
            })?;
        }
    }
    Ok(total)
}

fn git_grep_files(repo_path: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["grep", "-l", "-z", pattern])
        .output()?;
    if !output.status.success() {
        return Err(flashgrep::AppError::InvalidIndex(format!(
            "git grep failed with status {}",
            output.status
        )));
    }

    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| PathBuf::from(String::from_utf8_lossy(chunk).into_owned()))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn select_controlled_candidate_paths(
    repo_path: &Path,
    relative_paths: &[PathBuf],
    query: &str,
) -> Result<Vec<PathBuf>> {
    let mut profiles = relative_paths
        .iter()
        .map(|relative_path| {
            let contents = fs::read(repo_path.join(relative_path))?;
            let matched_lines = String::from_utf8_lossy(&contents)
                .lines()
                .filter(|line| line.contains(query))
                .count();
            Ok(CandidateProfile {
                relative_path: relative_path.clone(),
                matched_lines,
                byte_len: contents.len(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    profiles.retain(|profile| profile.matched_lines > 0);

    let min_matched_lines = profiles
        .iter()
        .map(|profile| profile.matched_lines)
        .min()
        .unwrap_or(0);
    let preferred_count = profiles
        .iter()
        .filter(|profile| profile.matched_lines == min_matched_lines)
        .count();
    let median_size = candidate_median_size(
        profiles
            .iter()
            .filter(|profile| profile.matched_lines == min_matched_lines)
            .map(|profile| profile.byte_len)
            .collect::<Vec<_>>()
            .as_slice(),
    );
    profiles.sort_by(|left, right| {
        (
            left.matched_lines,
            left.byte_len.abs_diff(median_size),
            &left.relative_path,
        )
            .cmp(&(
                right.matched_lines,
                right.byte_len.abs_diff(median_size),
                &right.relative_path,
            ))
    });
    let selected = profiles
        .into_iter()
        .map(|profile| profile.relative_path)
        .collect::<Vec<_>>();
    eprintln!(
        "controlled candidate selection: min_matched_lines={min_matched_lines}, preferred_count={preferred_count}, selected={}",
        selected.len()
    );
    Ok(selected)
}

fn select_scenario_candidates(
    candidates: &[PathBuf],
    needed: usize,
    sample_index: usize,
    sample_count: usize,
) -> Result<Vec<PathBuf>> {
    if needed == 0 {
        return Ok(Vec::new());
    }
    if candidates.len() < needed {
        return Err(flashgrep::AppError::InvalidIndex(format!(
            "expected at least {needed} candidate files, found {}",
            candidates.len()
        )));
    }
    let stride = (candidates.len() / sample_count.max(1)).max(1);
    let start = (sample_index * stride) % candidates.len();
    Ok((0..needed)
        .map(|offset| candidates[(start + offset) % candidates.len()].clone())
        .collect())
}

fn candidate_median_size(sizes: &[usize]) -> usize {
    let mut sizes = sizes.to_vec();
    sizes.sort_unstable();
    sizes.get(sizes.len() / 2).copied().unwrap_or_default()
}

fn run_git<const N: usize>(repo_path: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(flashgrep::AppError::InvalidIndex(format!(
            "git {:?} failed in {}: {}",
            args,
            repo_path.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn load_baseline_files(
    source_repo: &Path,
    worktree_repo: &Path,
    relative_paths: &[PathBuf],
) -> Result<Vec<BaselineFile>> {
    let mut baseline = Vec::with_capacity(relative_paths.len());
    for relative_path in relative_paths {
        baseline.push(BaselineFile {
            path: worktree_repo.join(relative_path),
            contents: fs::read(source_repo.join(relative_path))?,
        });
    }
    Ok(baseline)
}

fn restore_baseline_files(files: &[BaselineFile]) -> Result<()> {
    for file in files {
        if let Some(parent) = file.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&file.path, &file.contents)?;
    }
    Ok(())
}

fn flashgrep_executable() -> Result<PathBuf> {
    if let Ok(path) = env::var("CARGO_BIN_EXE_flashgrep") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    let current = env::current_exe()?;
    let candidates = [
        current.parent().map_or_else(
            || PathBuf::from("target/release/flashgrep"),
            |parent| parent.join("flashgrep"),
        ),
        current.parent().and_then(Path::parent).map_or_else(
            || PathBuf::from("target/release/flashgrep"),
            |parent| parent.join("flashgrep"),
        ),
        PathBuf::from("/Users/user/workspace/flashgrep/target/release/flashgrep"),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from("flashgrep"))
}

fn median_ms(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn stddev(values: &[f64]) -> f64 {
    if values.len() <= 1 {
        return 0.0;
    }
    let mean = mean(values);
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}

fn elapsed_ms(seconds: f64) -> f64 {
    seconds * 1000.0
}

fn bench_iters() -> usize {
    env::var("FLASHGREP_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BENCH_ITERS)
}

fn scenario_samples() -> usize {
    env::var("FLASHGREP_BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SCENARIO_SAMPLES)
}

fn bench_mode() -> BenchMode {
    match env::var("FLASHGREP_BENCH_MODE")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("detect") => BenchMode::Detect,
        _ => BenchMode::Search,
    }
}

fn configured_path(name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.exists() {
        return Ok(Some(fs::canonicalize(path)?));
    }
    Ok(Some(path))
}

fn scenario_sizes() -> Vec<usize> {
    let Some(value) = env::var("FLASHGREP_BENCH_SIZES").ok() else {
        return DEFAULT_SIZES.to_vec();
    };

    let parsed = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .collect::<Vec<_>>();
    if parsed.is_empty() {
        DEFAULT_SIZES.to_vec()
    } else {
        parsed
    }
}

fn print_results(
    base_snapshot_build_ms: f64,
    sample_results: &[ScenarioSampleResult],
    results: &[ScenarioResult],
) {
    println!("RESULTS");
    println!("base_snapshot_build_ms={base_snapshot_build_ms:.2}");
    println!(
        "sample,scenario,edited_files,rg_matched_lines,flashgrep_matched_lines,missing_lines,recall_ratio,settle_backend,detection_ms,settle_ms,flashgrep_query_ms,rg_median_ms,status_dirty_paths"
    );
    for result in sample_results {
        println!(
            "{},{},{},{},{},{},{:.4},{},{:.2},{:.2},{:.2},{:.2},{}",
            result.sample_index + 1,
            result.scenario,
            result.edited_files,
            result.rg_matched_lines,
            result.flashgrep_matched_lines,
            result.missing_lines,
            result.recall_ratio,
            result.settle_backend,
            result.detection_ms,
            result.settle_ms,
            result.flashgrep_query_ms,
            result.rg_median_ms,
            result.status_dirty_paths
        );
    }
    println!("SUMMARY");
    println!(
        "scenario,edited_files,samples,rg_matched_lines_mean,flashgrep_matched_lines_mean,missing_lines_mean,recall_ratio_mean,settle_backend,detection_ms_mean,detection_ms_stddev,settle_ms_mean,settle_ms_stddev,flashgrep_query_ms_mean,flashgrep_query_ms_stddev,rg_median_ms_mean,rg_median_ms_stddev,status_dirty_paths_mean"
    );
    for result in results {
        println!(
            "{},{},{},{:.2},{:.2},{:.2},{:.4},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2}",
            result.scenario,
            result.edited_files,
            result.samples,
            result.rg_matched_lines_mean,
            result.flashgrep_matched_lines_mean,
            result.missing_lines_mean,
            result.recall_ratio_mean,
            result.settle_backend,
            result.detection_ms_mean,
            result.detection_ms_stddev,
            result.settle_ms_mean,
            result.settle_ms_stddev,
            result.flashgrep_query_ms_mean,
            result.flashgrep_query_ms_stddev,
            result.rg_median_ms_mean,
            result.rg_median_ms_stddev,
            result.status_dirty_paths_mean
        );
    }
}

fn print_detection_results(base_snapshot_build_ms: f64, results: &[DetectionResult]) {
    println!("RESULTS");
    println!("base_snapshot_build_ms={base_snapshot_build_ms:.2}");
    println!(
        "scenario,sample,modify,add,delete,expected_dirty_paths,detected_dirty_paths,detection_ms"
    );
    for result in results {
        println!(
            "{},{},{},{},{},{},{},{:.2}",
            result.scenario,
            result.sample_index + 1,
            result.modify_count,
            result.add_count,
            result.delete_count,
            result.expected_dirty_paths,
            result.detected_dirty_paths,
            result.detection_ms
        );
    }
}
