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

use codgrep::{
    daemon::{
        protocol::{
            ConsistencyMode, EnsureRepoParams, PathScope, QuerySpec, RepoConfig, RepoRef, Request,
            SearchModeConfig, SearchParams,
        },
        DaemonClient, ManagedDaemonClient,
    },
    BuildConfig, CorpusMode, Result, SearchEngine, TokenizerMode,
};

const DEFAULT_QUERY: &str = "ScopedAllowBlockingForTesting";
const DEFAULT_BENCH_ITERS: usize = 5;
const DEFAULT_SEARCH_DELAYS_MS: [u64; 5] = [0, 200, 500, 1000, 3000];
const SIZES: [usize; 3] = [10, 100, 300];

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
    search_delay_ms: u64,
    modify_count: usize,
    add_count: usize,
    delete_count: usize,
    rg_matched_lines: usize,
    codgrep_matched_lines: usize,
    missing_lines: usize,
    recall_ratio: f64,
    backend: String,
    codgrep_search_ms: f64,
    rg_median_ms: f64,
    status_dirty_paths: usize,
}

#[derive(Debug)]
struct SearchMeasurement {
    matched_lines: usize,
    backend: String,
    elapsed_ms: f64,
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
        if let Some(worktree_path) = configured_path("CODGREP_BENCH_WORKTREE")? {
            let storage_root = configured_path("CODGREP_BENCH_STORAGE_ROOT")?.ok_or_else(|| {
                codgrep::AppError::InvalidIndex(
                    "CODGREP_BENCH_STORAGE_ROOT is required when CODGREP_BENCH_WORKTREE is set"
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
            let storage_root = configured_path("CODGREP_BENCH_STORAGE_ROOT")?
                .unwrap_or_else(|| source_repo.join(".codgrep-dirty-bench-index-engine"));
            return Ok(Self {
                cleanup_detached_worktree: false,
                source_repo: None,
                cleanup_root: false,
                root: None,
                worktree_path: source_repo.to_path_buf(),
                storage_root,
            });
        }

        if let Some(root) = configured_path("CODGREP_BENCH_ROOT")? {
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
            let storage_root = configured_path("CODGREP_BENCH_STORAGE_ROOT")?
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
            .expect("clock should be after unix epoch")
            .as_secs();
        let root = env::temp_dir().join(format!(
            "codgrep-dirty-bench-{timestamp}-{}",
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

    fn reset_clean(
        &self,
        previous: Option<&Scenario>,
        baseline_files: &[BaselineFile],
    ) -> Result<()> {
        let Some(previous) = previous else {
            return Ok(());
        };
        let restore_count = previous.modify_count + previous.delete_count;
        restore_baseline_files(&baseline_files[..restore_count])?;
        if previous.add_count > 0 {
            let added = self
                .worktree_path
                .join("codgrep_bench_added")
                .join(&previous.name);
            if added.exists() {
                fs::remove_dir_all(added)?;
            }
        }
        Ok(())
    }

    fn reset_start_clean(&self, baseline_files: &[BaselineFile]) -> Result<()> {
        if let Some(source_repo) = self.source_repo.as_ref() {
            if *source_repo != self.worktree_path {
                run_git(&self.worktree_path, ["reset", "--hard", "HEAD"])?;
                let added_root = self.worktree_path.join("codgrep_bench_added");
                if added_root.exists() {
                    fs::remove_dir_all(added_root)?;
                }
                return Ok(());
            }
        }
        restore_baseline_files(baseline_files)?;
        let added_root = self.worktree_path.join("codgrep_bench_added");
        if added_root.exists() {
            fs::remove_dir_all(added_root)?;
        }
        Ok(())
    }
}

impl Drop for BenchWorkspace {
    fn drop(&mut self) {
        if self.cleanup_detached_worktree {
            let _ = Command::new("git")
                .arg("-C")
                .arg(
                    self.source_repo
                        .as_ref()
                        .expect("cleanup_detached_worktree requires source_repo"),
                )
                .args(["worktree", "remove", "--force"])
                .arg(&self.worktree_path)
                .status();
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
    let source_repo = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/user/workspace/chromium-src"));
    let query = args
        .next()
        .or_else(|| env::var("CODGREP_DIRTY_BENCH_QUERY").ok())
        .unwrap_or_else(|| DEFAULT_QUERY.to_string());
    let source_repo = fs::canonicalize(source_repo)?;
    let in_place = env::var("CODGREP_BENCH_IN_PLACE").ok().as_deref() == Some("1");
    let bench_iters = bench_iters();
    let search_delays_ms = search_delays_ms();
    let worktree = BenchWorkspace::new(&source_repo, in_place)?;
    let storage_root = worktree.storage_root.clone();

    println!("source_repo={}", source_repo.display());
    println!("worktree={}", worktree.worktree_path.display());
    println!("storage_root={}", storage_root.display());
    println!("in_place={in_place}");
    println!("query={query}");
    println!("sizes={:?}", SIZES);
    println!("bench_iters={bench_iters}");
    println!("search_delays_ms={search_delays_ms:?}");

    let build_started = Instant::now();
    let build_config = BuildConfig {
        repo_path: worktree.worktree_path.clone(),
        storage_root: storage_root.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size: 512 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let prepare_mode = if codgrep::index_engine::WorkspaceBaseSnapshotIndex::read_snapshot_metadata(
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
    let candidate_files = candidate_relative_paths
        .iter()
        .map(|relative| worktree.worktree_path.join(relative))
        .collect::<Vec<_>>();
    if candidate_files.len() < 600 {
        return Err(codgrep::AppError::InvalidIndex(format!(
            "expected at least 600 tracked files for query {query}, found {}",
            candidate_files.len()
        )));
    }
    println!("candidate_files={}", candidate_files.len());
    println!("index_prepare_mode={prepare_mode}");
    println!("index_prepare_ms={build_elapsed_ms:.2}");
    println!();
    let baseline_files = load_baseline_files(
        &source_repo,
        &worktree.worktree_path,
        &candidate_relative_paths,
    )?;
    worktree.reset_start_clean(&baseline_files)?;

    let scenario_filter = env::var("CODGREP_BENCH_SCENARIOS").ok();
    let scenarios = filter_scenarios(build_scenarios(), scenario_filter.as_deref());
    let cg_path = codgrep_executable()?;
    let mut results = Vec::with_capacity(scenarios.len());
    let mut previous_scenario: Option<Scenario> = None;

    for scenario in scenarios {
        eprintln!("running scenario {}", scenario.name);
        let scenario_state = scenario.clone();
        worktree.reset_clean(previous_scenario.as_ref(), &baseline_files)?;
        let repo = open_codgrep_repo(&cg_path, &worktree.worktree_path, &storage_root)?;
        wait_for_dirty_paths(&repo, 0, Duration::from_secs(180))?;
        apply_scenario(&worktree.worktree_path, &scenario, &candidate_files, &query)?;
        let apply_completed_at = Instant::now();

        let mut rg_times = Vec::with_capacity(bench_iters);
        let mut rg_count = None;
        for _ in 0..bench_iters {
            let (count, elapsed_ms) = run_rg_query(&worktree.worktree_path, &query)?;
            rg_times.push(elapsed_ms);
            rg_count.get_or_insert(count);
        }

        let rg_count = rg_count.expect("rg should run at least once");
        let rg_median = median_ms(rg_times);

        for search_delay_ms in &search_delays_ms {
            sleep_until(apply_completed_at, Duration::from_millis(*search_delay_ms));
            let codgrep = run_codgrep_query(&repo, &query)?;
            let status_dirty_paths = repo_dirty_paths(&repo)?;
            let missing_lines = rg_count.saturating_sub(codgrep.matched_lines);
            let recall_ratio = if rg_count == 0 {
                1.0
            } else {
                codgrep.matched_lines as f64 / rg_count as f64
            };

            results.push(ScenarioResult {
                scenario: scenario.name.clone(),
                search_delay_ms: *search_delay_ms,
                modify_count: scenario.modify_count,
                add_count: scenario.add_count,
                delete_count: scenario.delete_count,
                rg_matched_lines: rg_count,
                codgrep_matched_lines: codgrep.matched_lines,
                missing_lines,
                recall_ratio,
                backend: codgrep.backend,
                codgrep_search_ms: codgrep.elapsed_ms,
                rg_median_ms: rg_median,
                status_dirty_paths,
            });
        }
        shutdown_daemon(&repo)?;
        previous_scenario = Some(scenario_state);
    }

    worktree.reset_clean(previous_scenario.as_ref(), &baseline_files)?;
    print_results(build_elapsed_ms, &results);
    Ok(())
}

fn build_scenarios() -> Vec<Scenario> {
    let mut scenarios = vec![Scenario {
        name: "clean".into(),
        modify_count: 0,
        add_count: 0,
        delete_count: 0,
    }];

    for size in SIZES {
        scenarios.push(Scenario {
            name: format!("modify-{size}"),
            modify_count: size,
            add_count: 0,
            delete_count: 0,
        });
        scenarios.push(Scenario {
            name: format!("add-{size}"),
            modify_count: 0,
            add_count: size,
            delete_count: 0,
        });
        scenarios.push(Scenario {
            name: format!("delete-{size}"),
            modify_count: 0,
            add_count: 0,
            delete_count: size,
        });
        scenarios.push(Scenario {
            name: format!("mixed-{size}"),
            modify_count: size / 3,
            add_count: size / 3,
            delete_count: size - 2 * (size / 3),
        });
    }

    scenarios
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
        writeln!(file, "\n// CODGREP_DIRTY_BENCH_MOD_{index}: {query}")?;
    }

    for _ in 0..scenario.delete_count {
        let path = &candidates[cursor];
        cursor += 1;
        fs::remove_file(path)?;
    }

    if scenario.add_count > 0 {
        let root = repo_path.join("codgrep_bench_added").join(&scenario.name);
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

fn open_codgrep_repo(cg_path: &Path, repo_path: &Path, storage_root: &Path) -> Result<DaemonRepo> {
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

fn run_codgrep_query(repo: &DaemonRepo, query: &str) -> Result<SearchMeasurement> {
    let started = Instant::now();
    let outcome = repo.client.send(Request::Search {
        params: SearchParams {
            repo_id: repo.repo_id.clone(),
            query: count_only_query(query),
            scope: PathScope::default(),
            consistency: ConsistencyMode::WorkspaceEventual,
            allow_scan_fallback: false,
        },
    })?;
    let elapsed_ms = elapsed_ms(started.elapsed().as_secs_f64());

    match outcome {
        codgrep::daemon::protocol::Response::SearchCompleted {
            backend, results, ..
        } => Ok(SearchMeasurement {
            matched_lines: results.matched_lines,
            backend: format!("{backend:?}"),
            elapsed_ms,
        }),
        other => Err(codgrep::AppError::Protocol(format!(
            "unexpected search response: {other:?}"
        ))),
    }
}

fn shutdown_daemon(repo: &DaemonRepo) -> Result<()> {
    match repo.client.send(Request::Shutdown)? {
        codgrep::daemon::protocol::Response::ShutdownAck => Ok(()),
        other => Err(codgrep::AppError::Protocol(format!(
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
            codgrep::daemon::protocol::Response::RepoStatus { status } => {
                let dirty_paths = status.dirty_files.modified
                    + status.dirty_files.deleted
                    + status.dirty_files.new;
                if dirty_paths == expected {
                    return Ok(elapsed_ms(started.elapsed().as_secs_f64()));
                }
                if started.elapsed() >= timeout {
                    return Err(codgrep::AppError::Protocol(format!(
                        "timed out waiting for dirty path count {expected}; last observed {dirty_paths}"
                    )));
                }
            }
            other => {
                return Err(codgrep::AppError::Protocol(format!(
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
        codgrep::daemon::protocol::Response::RepoStatus { status } => {
            Ok(status.dirty_files.modified + status.dirty_files.deleted + status.dirty_files.new)
        }
        other => Err(codgrep::AppError::Protocol(format!(
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
        return Err(codgrep::AppError::InvalidIndex(format!(
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
        pattern: pattern.clone(),
        patterns: vec![pattern],
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        fixed_strings: true,
        word_regexp: false,
        line_regexp: false,
        before_context: 0,
        after_context: 0,
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
                codgrep::AppError::InvalidIndex(format!(
                    "failed to parse rg count line {line:?}: {error}"
                ))
            })?;
        } else if !line.trim().is_empty() {
            total += line.trim().parse::<usize>().map_err(|error| {
                codgrep::AppError::InvalidIndex(format!(
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
        return Err(codgrep::AppError::InvalidIndex(format!(
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

fn run_git<const N: usize>(repo_path: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(codgrep::AppError::InvalidIndex(format!(
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

fn codgrep_executable() -> Result<PathBuf> {
    if let Ok(path) = env::var("CARGO_BIN_EXE_cg") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    let current = env::current_exe()?;
    let candidates = [
        current
            .parent()
            .map(|parent| parent.join("cg"))
            .unwrap_or_else(|| PathBuf::from("target/release/cg")),
        current
            .parent()
            .and_then(Path::parent)
            .map(|parent| parent.join("cg"))
            .unwrap_or_else(|| PathBuf::from("target/release/cg")),
        PathBuf::from("/Users/user/workspace/codgrep/target/release/cg"),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from("cg"))
}

fn median_ms(mut values: Vec<f64>) -> f64 {
    values.sort_by(|left, right| left.partial_cmp(right).expect("times should be finite"));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn elapsed_ms(seconds: f64) -> f64 {
    seconds * 1000.0
}

fn sleep_until(started_at: Instant, target_elapsed: Duration) {
    let elapsed = started_at.elapsed();
    if target_elapsed > elapsed {
        thread::sleep(target_elapsed - elapsed);
    }
}

fn bench_iters() -> usize {
    env::var("CODGREP_BENCH_ITERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_BENCH_ITERS)
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

fn search_delays_ms() -> Vec<u64> {
    let Some(value) = env::var("CODGREP_BENCH_SEARCH_DELAYS_MS").ok() else {
        return DEFAULT_SEARCH_DELAYS_MS.to_vec();
    };

    let parsed = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<u64>().ok())
        .collect::<Vec<_>>();
    if parsed.is_empty() {
        DEFAULT_SEARCH_DELAYS_MS.to_vec()
    } else {
        parsed
    }
}

fn print_results(base_snapshot_build_ms: f64, results: &[ScenarioResult]) {
    println!("RESULTS");
    println!("base_snapshot_build_ms={base_snapshot_build_ms:.2}");
    println!(
        "scenario,search_delay_ms,modify,add,delete,rg_matched_lines,codgrep_matched_lines,missing_lines,recall_ratio,backend,codgrep_search_ms,rg_median_ms,status_dirty_paths"
    );
    for result in results {
        println!(
            "{},{},{},{},{},{},{},{},{:.4},{},{:.2},{:.2},{}",
            result.scenario,
            result.search_delay_ms,
            result.modify_count,
            result.add_count,
            result.delete_count,
            result.rg_matched_lines,
            result.codgrep_matched_lines,
            result.missing_lines,
            result.recall_ratio,
            result.backend,
            result.codgrep_search_ms,
            result.rg_median_ms,
            result.status_dirty_paths
        );
    }
}
