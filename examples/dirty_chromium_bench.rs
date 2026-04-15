use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use codgrep::{
    build_index, BuildConfig, CorpusMode, QueryConfig, Result, SearchMode, TokenizerMode,
    WorkspaceIndex, WorkspaceIndexOptions,
    daemon::{
        protocol::{
            ConsistencyMode, EnsureRepoParams, PathScope, QuerySpec, RepoConfig, Request,
            SearchModeConfig, SearchParams,
        },
        DaemonClient, ManagedDaemonClient,
    },
};

const QUERY: &str = "ScopedAllowBlocking";
const BENCH_ITERS: usize = 5;
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
    modify_count: usize,
    add_count: usize,
    delete_count: usize,
    matched_lines: usize,
    backend: String,
    codgrep_open_ms: f64,
    codgrep_first_ms: f64,
    codgrep_steady_median_ms: f64,
    rg_median_ms: f64,
    codgrep_vs_rg_ratio: f64,
    profile_diff_ms: f64,
    profile_overlay_build_ms: f64,
    profile_base_search_ms: f64,
    profile_dirty_search_ms: f64,
    profile_merge_ms: f64,
    profile_total_ms: f64,
    profile_dirty_docs: usize,
    profile_shadowed_paths: usize,
    profile_base_candidate_docs: usize,
    profile_dirty_candidate_docs: usize,
}

#[derive(Debug)]
struct SearchMeasurement {
    matched_lines: usize,
    backend: String,
    elapsed_ms: f64,
}

#[derive(Debug)]
struct DaemonRepo {
    repo_id: String,
    client: DaemonClient,
}

#[derive(Debug)]
struct TempWorktree {
    detached: bool,
    source_repo: PathBuf,
    root: PathBuf,
    worktree_path: PathBuf,
}

impl TempWorktree {
    fn new(source_repo: &Path, in_place: bool) -> Result<Self> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_secs();
        let root = env::temp_dir().join(format!(
            "codgrep-dirty-bench-{timestamp}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root)?;
        let (detached, worktree_path) = if in_place {
            (false, source_repo.to_path_buf())
        } else {
            let worktree_path = root.join("repo");
            run_git(
                source_repo,
                ["worktree", "add", "--detach", worktree_path.to_string_lossy().as_ref(), "HEAD"],
            )?;
            (true, worktree_path)
        };
        Ok(Self {
            detached,
            source_repo: source_repo.to_path_buf(),
            root,
            worktree_path,
        })
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index")
    }

    fn reset_clean(&self, candidates: &[PathBuf]) -> Result<()> {
        restore_paths(&self.worktree_path, candidates)?;
        let added = self.worktree_path.join("codgrep_bench_added");
        if added.exists() {
            fs::remove_dir_all(added)?;
        }
        Ok(())
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        if self.detached {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.source_repo)
                .args(["worktree", "remove", "--force"])
                .arg(&self.worktree_path)
                .status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn main() -> Result<()> {
    let source_repo = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Users/user/workspace/chromium-src"));
    let source_repo = fs::canonicalize(source_repo)?;
    let in_place = env::var("CODGREP_BENCH_IN_PLACE").ok().as_deref() == Some("1");
    let worktree = TempWorktree::new(&source_repo, in_place)?;
    let index_path = worktree.index_path();

    println!("source_repo={}", source_repo.display());
    println!("worktree={}", worktree.worktree_path.display());
    println!("index_path={}", index_path.display());
    println!("in_place={in_place}");
    println!("query={QUERY}");
    println!("sizes={:?}", SIZES);

    let existing_index = source_repo.join(".codgrep-index");
    let build_started = Instant::now();
    let index_prepare_mode = if existing_index.exists() {
        eprintln!(
            "cloning existing index from {}",
            existing_index.display()
        );
        clone_existing_index(&existing_index, &index_path)?;
        "cloned_existing"
    } else {
        eprintln!("building fresh index for {}", worktree.worktree_path.display());
        build_index(&BuildConfig {
            repo_path: worktree.worktree_path.clone(),
            index_path: index_path.clone(),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 8,
        })?;
        "built_fresh"
    };
    let build_elapsed_ms = elapsed_ms(build_started.elapsed().as_secs_f64());

    let candidate_files = git_grep_files(&worktree.worktree_path, QUERY)?;
    if candidate_files.len() < 600 {
        return Err(codgrep::AppError::InvalidIndex(format!(
            "expected at least 600 tracked files for query {QUERY}, found {}",
            candidate_files.len()
        )));
    }
    println!("candidate_files={}", candidate_files.len());
    println!("index_prepare_mode={index_prepare_mode}");
    println!("index_prepare_ms={build_elapsed_ms:.2}");
    println!();

    let scenario_filter = env::var("CODGREP_BENCH_SCENARIOS").ok();
    let scenarios = filter_scenarios(build_scenarios(), scenario_filter.as_deref());
    let cg_path = codgrep_executable()?;
    let mut results = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        eprintln!("running scenario {}", scenario.name);
        worktree.reset_clean(&candidate_files)?;
        apply_scenario(&worktree.worktree_path, &scenario, &candidate_files)?;

        let profile = profile_workspace_query(&worktree.worktree_path, &index_path)?;
        let (repo, open_ms) = open_codgrep_repo(&cg_path, &worktree.worktree_path, &index_path)?;
        let first = run_codgrep_query(&repo)?;
        let mut steady = Vec::with_capacity(BENCH_ITERS);
        for _ in 0..BENCH_ITERS {
            steady.push(run_codgrep_query(&repo)?);
        }
        shutdown_daemon(&repo)?;

        let mut rg_times = Vec::with_capacity(BENCH_ITERS);
        let mut rg_count = None;
        for _ in 0..BENCH_ITERS {
            let (count, elapsed_ms) = run_rg_query(&worktree.worktree_path)?;
            rg_times.push(elapsed_ms);
            rg_count.get_or_insert(count);
        }

        let rg_count = rg_count.expect("rg should run at least once");
        if first.matched_lines != rg_count {
            return Err(codgrep::AppError::InvalidIndex(format!(
                "count mismatch for {}: codgrep={} rg={}",
                scenario.name, first.matched_lines, rg_count
            )));
        }
        for measurement in &steady {
            if measurement.matched_lines != rg_count {
                return Err(codgrep::AppError::InvalidIndex(format!(
                    "steady count mismatch for {}: codgrep={} rg={}",
                    scenario.name, measurement.matched_lines, rg_count
                )));
            }
        }

        let codgrep_steady = median_ms(steady.iter().map(|m| m.elapsed_ms).collect());
        let rg_median = median_ms(rg_times);
        let ratio = codgrep_steady / rg_median;

        results.push(ScenarioResult {
            scenario: scenario.name,
            modify_count: scenario.modify_count,
            add_count: scenario.add_count,
            delete_count: scenario.delete_count,
            matched_lines: rg_count,
            backend: first.backend,
            codgrep_open_ms: open_ms,
            codgrep_first_ms: first.elapsed_ms,
            codgrep_steady_median_ms: codgrep_steady,
            rg_median_ms: rg_median,
            codgrep_vs_rg_ratio: ratio,
            profile_diff_ms: elapsed_ms(profile.timings.diff.as_secs_f64()),
            profile_overlay_build_ms: elapsed_ms(profile.timings.overlay_build.as_secs_f64()),
            profile_base_search_ms: elapsed_ms(profile.timings.base_search.as_secs_f64()),
            profile_dirty_search_ms: elapsed_ms(profile.timings.dirty_search.as_secs_f64()),
            profile_merge_ms: elapsed_ms(profile.timings.merge.as_secs_f64()),
            profile_total_ms: elapsed_ms(profile.timings.total.as_secs_f64()),
            profile_dirty_docs: profile.dirty_documents,
            profile_shadowed_paths: profile.shadowed_base_paths,
            profile_base_candidate_docs: profile.base_candidate_docs,
            profile_dirty_candidate_docs: profile.dirty_candidate_docs,
        });
    }

    worktree.reset_clean(&candidate_files)?;

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

fn apply_scenario(repo_path: &Path, scenario: &Scenario, candidates: &[PathBuf]) -> Result<()> {
    let mut cursor = 0usize;

    for index in 0..scenario.modify_count {
        let path = &candidates[cursor];
        cursor += 1;
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        writeln!(file, "\n// CODGREP_DIRTY_BENCH_MOD_{index}")?;
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
                    "// synthetic dirty benchmark file\nvoid bench_{index}() {{ /* {QUERY} */ }}\n"
                ),
            )?;
        }
    }

    Ok(())
}

fn open_codgrep_repo(
    cg_path: &Path,
    repo_path: &Path,
    index_path: &Path,
) -> Result<(DaemonRepo, f64)> {
    let client = ManagedDaemonClient::new()
        .with_daemon_program(OsString::from(cg_path))
        .with_start_timeout(std::time::Duration::from_secs(120));
    let started = Instant::now();
    let ensured = client.ensure_repo(EnsureRepoParams {
        repo_path: repo_path.to_path_buf(),
        index_path: Some(index_path.to_path_buf()),
        config: RepoConfig::default(),
        refresh: Default::default(),
    })?;
    Ok((
        DaemonRepo {
            repo_id: ensured.repo_id,
            client: DaemonClient::new(ensured.addr),
        },
        elapsed_ms(started.elapsed().as_secs_f64()),
    ))
}

fn run_codgrep_query(repo: &DaemonRepo) -> Result<SearchMeasurement> {
    let started = Instant::now();
    let outcome = repo.client.send(Request::Search {
        params: SearchParams {
            repo_id: repo.repo_id.clone(),
            query: count_only_query(QUERY),
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

fn run_rg_query(repo_path: &Path) -> Result<(usize, f64)> {
    let started = Instant::now();
    let output = Command::new("rg")
        .current_dir(repo_path)
        .args([
            "--count",
            "--with-filename",
            "--fixed-strings",
            "--max-filesize",
            "536870912",
            QUERY,
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

fn profile_workspace_query(
    repo_path: &Path,
    index_path: &Path,
) -> Result<codgrep::WorkspaceSearchProfile> {
    let workspace = WorkspaceIndex::open(WorkspaceIndexOptions {
        build_config: BuildConfig {
            repo_path: repo_path.to_path_buf(),
            index_path: index_path.to_path_buf(),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 8,
        },
    })?;
    workspace.profile_search_with_filter(&profile_query(), None)
}

fn profile_query() -> QueryConfig {
    QueryConfig {
        regex_pattern: QUERY.into(),
        patterns: vec![QUERY.into()],
        fixed_strings: true,
        search_mode: SearchMode::CountOnly,
        ..QueryConfig::default()
    }
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
        .map(|chunk| repo_path.join(String::from_utf8_lossy(chunk).as_ref()))
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

fn restore_paths(repo_path: &Path, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    const CHUNK_SIZE: usize = 128;
    for chunk in paths.chunks(CHUNK_SIZE) {
        let mut command = Command::new("git");
        command.arg("-C").arg(repo_path);
        command.args(["restore", "--source=HEAD", "--worktree", "--staged", "--"]);
        for path in chunk {
            command.arg(path);
        }
        let output = command.output()?;
        if !output.status.success() {
            return Err(codgrep::AppError::InvalidIndex(format!(
                "git restore failed in {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
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

fn clone_existing_index(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let clone_attempt = Command::new("cp")
        .args(["-cR"])
        .arg(source)
        .arg(target)
        .output()?;
    if clone_attempt.status.success() {
        return Ok(());
    }

    let copy_attempt = Command::new("cp")
        .args(["-R"])
        .arg(source)
        .arg(target)
        .output()?;
    if copy_attempt.status.success() {
        return Ok(());
    }

    Err(codgrep::AppError::InvalidIndex(format!(
        "failed to copy index from {} to {}: {}",
        source.display(),
        target.display(),
        String::from_utf8_lossy(&copy_attempt.stderr)
    )))
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

fn print_results(index_build_ms: f64, results: &[ScenarioResult]) {
    println!("RESULTS");
    println!("index_build_ms={index_build_ms:.2}");
    println!(
        "scenario,modify,add,delete,matched_lines,backend,codgrep_open_ms,codgrep_first_ms,codgrep_steady_median_ms,rg_median_ms,codgrep_vs_rg_ratio,profile_diff_ms,profile_overlay_build_ms,profile_base_search_ms,profile_dirty_search_ms,profile_merge_ms,profile_total_ms,profile_dirty_docs,profile_shadowed_paths,profile_base_candidate_docs,profile_dirty_candidate_docs"
    );
    for result in results {
        println!(
            "{},{},{},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{},{},{},{}",
            result.scenario,
            result.modify_count,
            result.add_count,
            result.delete_count,
            result.matched_lines,
            result.backend,
            result.codgrep_open_ms,
            result.codgrep_first_ms,
            result.codgrep_steady_median_ms,
            result.rg_median_ms,
            result.codgrep_vs_rg_ratio,
            result.profile_diff_ms,
            result.profile_overlay_build_ms,
            result.profile_base_search_ms,
            result.profile_dirty_search_ms,
            result.profile_merge_ms,
            result.profile_total_ms,
            result.profile_dirty_docs,
            result.profile_shadowed_paths,
            result.profile_base_candidate_docs,
            result.profile_dirty_candidate_docs
        );
    }
}
