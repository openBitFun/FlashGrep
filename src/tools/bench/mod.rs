mod cache;
mod custom;
mod fixtures;
mod report;
mod standard;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, BufWriter, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::{
    base_snapshot_build::{BaseSnapshotBuildMode, BaseSnapshotBuildOptions},
    config::{BuildConfig, CorpusMode, TokenizerMode},
    daemon::{
        daemon_state_file_path,
        protocol::{
            EnsureRepoParams, PathScope, QuerySpec, RepoConfig, Request, RequestEnvelope, Response,
            ResponseEnvelope, SearchModeConfig, SearchParams,
        },
        DaemonClient,
    },
    error::{AppError, Result},
    files::{repo_relative_path, scan_repository},
    index_engine::{RepoId, WorkspaceBaseSnapshotIndex},
};

use self::{
    cache::collect_cache_paths,
    custom::run_custom,
    fixtures::prepare_dirty_worktree_fixture,
    report::{print_report as print_report_impl, RawWriter},
    standard::{run_flashgrep_benchmark, run_rg_benchmark, run_worktree_benchmark},
};

#[derive(Debug, Clone)]
pub(crate) struct BenchConfig {
    pub(crate) suite_dir: PathBuf,
    pub(crate) filter: Option<String>,
    pub(crate) custom_repo: Option<PathBuf>,
    pub(crate) custom_patterns: Vec<String>,
    pub(crate) custom_name: Option<String>,
    pub(crate) custom_case_insensitive: bool,
    pub(crate) tokenizer: TokenizerMode,
    pub(crate) corpus_mode: CorpusMode,
    pub(crate) cache_mode: BenchCacheMode,
    pub(crate) query_mode: BenchQueryMode,
    pub(crate) warmup_iter: usize,
    pub(crate) bench_iter: usize,
    pub(crate) top_k_tokens: usize,
    pub(crate) compare_rg: bool,
    pub(crate) compare_worktree: bool,
    pub(crate) worktree_sample_files: Option<usize>,
    pub(crate) worktree_dirty_files: usize,
    pub(crate) rebuild: bool,
    pub(crate) raw_output: Option<PathBuf>,
    pub(crate) cold_hook: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BenchCacheMode {
    Warm,
    Cold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BenchQueryMode {
    Same,
    Trace,
}

impl BenchCacheMode {
    fn is_cold(self) -> bool {
        matches!(self, Self::Cold)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BenchReport {
    corpus_builds: Vec<CorpusBuildRecord>,
    summaries: Vec<BenchSummary>,
}

#[derive(Debug, Clone)]
struct CorpusBuildRecord {
    corpus: String,
    tokenizer: TokenizerMode,
    docs_indexed: usize,
    duration_secs: f64,
}

#[derive(Debug, Clone)]
struct BenchSummary {
    name: String,
    pattern: String,
    target: String,
    runners: Vec<RunnerSummary>,
}

#[derive(Debug, Clone)]
struct RunnerSummary {
    runner: String,
    mean_secs: f64,
    stddev_secs: f64,
    min_secs: f64,
    sample_count: usize,
    candidate_docs: Option<usize>,
    match_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Corpus {
    Linux,
    SubtitlesEn,
    SubtitlesRu,
}

pub(super) const BENCH_MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;

impl Corpus {
    fn label(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::SubtitlesEn => "subtitles-en",
            Self::SubtitlesRu => "subtitles-ru",
        }
    }

    fn repo_path(self, suite_dir: &Path) -> PathBuf {
        match self {
            Self::Linux => suite_dir.join("linux"),
            Self::SubtitlesEn => suite_dir.join("subtitles").join("en.sample.txt"),
            Self::SubtitlesRu => suite_dir.join("subtitles").join("ru.txt"),
        }
    }

    fn search_target(self, suite_dir: &Path) -> PathBuf {
        self.repo_path(suite_dir)
    }
}

pub(super) fn bench_build_config(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> BuildConfig {
    BuildConfig {
        repo_path: repo_path.to_path_buf(),
        storage_root: storage_root.to_path_buf(),
        tokenizer: config.tokenizer,
        corpus_mode: config.corpus_mode,
        include_hidden: false,
        max_file_size: BENCH_MAX_FILE_SIZE,
        min_sparse_len: 3,
        max_sparse_len: 8,
    }
}

pub(super) fn configure_rg_command(command: &mut Command, config: &BenchConfig) {
    command.arg("--count");
    command.arg("--with-filename");
    command.arg("--max-filesize");
    command.arg(BENCH_MAX_FILE_SIZE.to_string());
    if matches!(config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
}

#[derive(Debug, Clone, Copy)]
struct BenchmarkCase {
    name: &'static str,
    corpus: Corpus,
    pattern: &'static str,
    case_insensitive: bool,
}

#[derive(Debug, Clone)]
struct CustomBenchmarkCase {
    name: String,
    target: String,
    repo_path: PathBuf,
    pattern: String,
    case_insensitive: bool,
}

#[derive(Debug, Clone)]
struct CustomBenchmarkTrace {
    name: String,
    target: String,
    repo_path: PathBuf,
    patterns: Vec<String>,
    case_insensitive: bool,
}

#[derive(Debug, Clone)]
struct DirtyPattern {
    regex_pattern: String,
    case_insensitive: bool,
}

impl CustomBenchmarkTrace {
    fn pattern_at(&self, iteration: usize) -> &str {
        &self.patterns[iteration % self.patterns.len()]
    }
}

#[derive(Debug, Clone)]
struct BenchCachePaths {
    corpus_files: Vec<PathBuf>,
    index_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BenchBaseSnapshotBuild {
    pub(super) indexed_docs: usize,
    pub(super) reused_existing: bool,
}

#[derive(Debug, Clone)]
struct DirtyWorktreeFixture {
    repo_path: PathBuf,
    storage_root: PathBuf,
    cache_paths: Option<BenchCachePaths>,
    build_cache_paths: Option<BenchCachePaths>,
}

#[derive(Debug, Clone)]
pub(super) struct RgSearchScope {
    current_dir: Option<PathBuf>,
    target: PathBuf,
    ignore_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum CacheRunner {
    Bitfun,
    Rg,
}

pub(super) struct BitfunOutcome {
    candidate_docs: usize,
    match_count: usize,
}

pub(super) struct BenchDaemon {
    child: Option<Child>,
    client: DaemonClient,
    addr: String,
    repo_id: String,
    state_file: PathBuf,
    shutdown_sent: bool,
}

const BENCHMARKS: &[BenchmarkCase] = &[
    BenchmarkCase {
        name: "linux_literal",
        corpus: Corpus::Linux,
        pattern: "PM_RESUME",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_literal_casei",
        corpus: Corpus::Linux,
        pattern: "PM_RESUME",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "linux_re_literal_suffix",
        corpus: Corpus::Linux,
        pattern: "[A-Z]+_RESUME",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_word",
        corpus: Corpus::Linux,
        pattern: "\\bPM_RESUME\\b",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_unicode_greek",
        corpus: Corpus::Linux,
        pattern: "\\p{Greek}",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_unicode_greek_casei",
        corpus: Corpus::Linux,
        pattern: "\\p{Greek}",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "linux_unicode_word",
        corpus: Corpus::Linux,
        pattern: "\\wAh",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_no_literal",
        corpus: Corpus::Linux,
        pattern: "\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_alternates",
        corpus: Corpus::Linux,
        pattern: "ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "linux_alternates_casei",
        corpus: Corpus::Linux,
        pattern: "ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "subtitles_en_literal",
        corpus: Corpus::SubtitlesEn,
        pattern: "Sherlock Holmes",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_en_literal_casei",
        corpus: Corpus::SubtitlesEn,
        pattern: "Sherlock Holmes",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "subtitles_en_literal_word",
        corpus: Corpus::SubtitlesEn,
        pattern: "\\bSherlock Holmes\\b",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_en_alternate",
        corpus: Corpus::SubtitlesEn,
        pattern: "Sherlock Holmes|John Watson|Irene Adler|Inspector Lestrade|Professor Moriarty",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_en_alternate_casei",
        corpus: Corpus::SubtitlesEn,
        pattern: "Sherlock Holmes|John Watson|Irene Adler|Inspector Lestrade|Professor Moriarty",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "subtitles_en_surrounding_words",
        corpus: Corpus::SubtitlesEn,
        pattern: "\\w+\\s+Holmes\\s+\\w+",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_en_no_literal",
        corpus: Corpus::SubtitlesEn,
        pattern: "\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_ru_literal",
        corpus: Corpus::SubtitlesRu,
        pattern: "Шерлок Холмс",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_ru_literal_casei",
        corpus: Corpus::SubtitlesRu,
        pattern: "Шерлок Холмс",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "subtitles_ru_literal_word",
        corpus: Corpus::SubtitlesRu,
        pattern: "\\bШерлок Холмс\\b",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_ru_alternate",
        corpus: Corpus::SubtitlesRu,
        pattern: "Шерлок Холмс|Джон Уотсон|Ирен Адлер|инспектор Лестрейд|профессор Мориарти",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_ru_alternate_casei",
        corpus: Corpus::SubtitlesRu,
        pattern: "Шерлок Холмс|Джон Уотсон|Ирен Адлер|инспектор Лестрейд|профессор Мориарти",
        case_insensitive: true,
    },
    BenchmarkCase {
        name: "subtitles_ru_surrounding_words",
        corpus: Corpus::SubtitlesRu,
        pattern: "\\w+\\s+Холмс\\s+\\w+",
        case_insensitive: false,
    },
    BenchmarkCase {
        name: "subtitles_ru_no_literal",
        corpus: Corpus::SubtitlesRu,
        pattern: "\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}\\s+\\w{5}",
        case_insensitive: false,
    },
];

pub(crate) fn run(config: &BenchConfig) -> Result<BenchReport> {
    validate_config(config)?;
    let mut raw_writer = match &config.raw_output {
        Some(path) => Some(RawWriter::create(path)?),
        None => None,
    };
    if config.custom_repo.is_some() {
        return run_custom(config, raw_writer.as_mut());
    }

    let selected = select_benchmarks(config);
    if selected.is_empty() {
        return Err(AppError::InvalidPattern(
            "no benchmarks matched the provided filter".into(),
        ));
    }
    let dirty_patterns_by_corpus = collect_dirty_patterns_by_corpus(&selected);

    let corpora = selected
        .iter()
        .map(|case| case.corpus)
        .collect::<std::collections::BTreeSet<_>>();
    let mut storage_roots = BTreeMap::new();
    let mut rg_scopes = BTreeMap::new();
    let mut cache_paths = BTreeMap::new();
    let mut dirty_worktree_fixtures = BTreeMap::new();
    let mut corpus_builds = Vec::new();

    for corpus in corpora {
        ensure_corpus_exists(corpus, &config.suite_dir)?;
        let repo_path = corpus.repo_path(&config.suite_dir);
        let storage_root = config.suite_dir.join(".flashgrep-bench").join(format!(
            "{}-{}-{}",
            corpus.label(),
            config.corpus_mode.as_str(),
            config.tokenizer.as_str()
        ));
        let build_config = bench_build_config(&repo_path, &storage_root, config);

        let started = std::time::Instant::now();
        let snapshot = ensure_bench_base_snapshot(&build_config, config.rebuild)?;
        if config.rebuild || !snapshot.reused_existing {
            corpus_builds.push(CorpusBuildRecord {
                corpus: corpus.label().to_string(),
                tokenizer: config.tokenizer,
                docs_indexed: snapshot.indexed_docs,
                duration_secs: started.elapsed().as_secs_f64(),
            });
        }
        rg_scopes.insert(
            corpus,
            prepare_rg_search_scope(&repo_path, &storage_root, config)?,
        );
        let cache_entry = if config.cache_mode.is_cold() {
            Some(collect_cache_paths(
                corpus,
                &repo_path,
                &storage_root,
                config,
            )?)
        } else {
            None
        };
        storage_roots.insert(corpus, storage_root.clone());
        if let Some(cache_entry) = cache_entry {
            cache_paths.insert(corpus, cache_entry);
        }

        if config.compare_worktree {
            let fixture_label = format!(
                "{}-{}-{}",
                corpus.label(),
                config.corpus_mode.as_str(),
                config.tokenizer.as_str()
            );
            let patterns = dirty_patterns_by_corpus
                .get(&corpus)
                .map_or_else(|| &[][..], Vec::as_slice);
            dirty_worktree_fixtures.insert(
                corpus,
                prepare_dirty_worktree_fixture(
                    &repo_path,
                    &config.suite_dir,
                    &fixture_label,
                    config,
                    patterns,
                )?,
            );
        }
    }

    let mut summaries = Vec::new();
    for benchmark in selected {
        let storage_root = storage_roots.get(&benchmark.corpus).ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "missing benchmark storage root for corpus {}",
                benchmark.corpus.label()
            ))
        })?;
        let cache_paths = cache_paths.get(&benchmark.corpus);
        let mut runners = vec![run_flashgrep_benchmark(
            benchmark,
            storage_root,
            &config.suite_dir,
            cache_paths,
            config,
            raw_writer.as_mut(),
        )?];

        if config.compare_rg {
            let rg_scope = rg_scopes.get(&benchmark.corpus).ok_or_else(|| {
                AppError::InvalidIndex(format!(
                    "missing rg scope for corpus {}",
                    benchmark.corpus.label()
                ))
            })?;
            runners.push(run_rg_benchmark(
                benchmark,
                rg_scope,
                cache_paths,
                config,
                raw_writer.as_mut(),
            )?);
        }

        if config.compare_worktree {
            let fixture = dirty_worktree_fixtures
                .get(&benchmark.corpus)
                .ok_or_else(|| {
                    AppError::InvalidIndex(format!(
                        "missing dirty worktree fixture for corpus {}",
                        benchmark.corpus.label()
                    ))
                })?;
            runners.push(run_worktree_benchmark(
                benchmark,
                &fixture.repo_path,
                &fixture.storage_root,
                fixture.build_cache_paths.as_ref(),
                true,
                config,
                raw_writer.as_mut(),
            )?);
            runners.push(run_worktree_benchmark(
                benchmark,
                &fixture.repo_path,
                &fixture.storage_root,
                fixture.cache_paths.as_ref(),
                false,
                config,
                raw_writer.as_mut(),
            )?);
        }

        summaries.push(BenchSummary {
            name: benchmark.name.to_string(),
            pattern: benchmark.pattern.to_string(),
            target: benchmark.corpus.label().to_string(),
            runners,
        });
    }

    Ok(BenchReport {
        corpus_builds,
        summaries,
    })
}

pub(crate) fn print_report(report: &BenchReport) {
    print_report_impl(report);
}

fn validate_config(config: &BenchConfig) -> Result<()> {
    if !config.cache_mode.is_cold() && config.cold_hook.is_some() {
        return Err(AppError::InvalidPattern(
            "--cold-hook requires --cache-mode cold".into(),
        ));
    }
    if config.custom_repo.is_some() && config.custom_patterns.is_empty() {
        return Err(AppError::InvalidPattern(
            "--repo requires at least one --pattern".into(),
        ));
    }
    if config.custom_repo.is_none() && !config.custom_patterns.is_empty() {
        return Err(AppError::InvalidPattern(
            "--pattern requires --repo for ad-hoc benchmarks".into(),
        ));
    }
    if matches!(config.worktree_sample_files, Some(0)) {
        return Err(AppError::InvalidPattern(
            "--worktree-sample-files must be greater than zero".into(),
        ));
    }
    if config.worktree_dirty_files == 0 {
        return Err(AppError::InvalidPattern(
            "--worktree-dirty-files must be greater than zero".into(),
        ));
    }
    if config.compare_worktree
        && config
            .worktree_sample_files
            .is_some_and(|limit| limit < config.worktree_dirty_files)
    {
        return Err(AppError::InvalidPattern(format!(
            "--worktree-sample-files ({}) must be >= --worktree-dirty-files ({})",
            config.worktree_sample_files.unwrap_or_default(),
            config.worktree_dirty_files
        )));
    }
    Ok(())
}

fn collect_dirty_patterns_by_corpus(
    selected: &[BenchmarkCase],
) -> BTreeMap<Corpus, Vec<DirtyPattern>> {
    let mut patterns = BTreeMap::<Corpus, Vec<DirtyPattern>>::new();
    for benchmark in selected {
        patterns
            .entry(benchmark.corpus)
            .or_default()
            .push(DirtyPattern {
                regex_pattern: benchmark.pattern.to_string(),
                case_insensitive: benchmark.case_insensitive,
            });
    }
    patterns
}

fn select_benchmarks(config: &BenchConfig) -> Vec<BenchmarkCase> {
    BENCHMARKS
        .iter()
        .copied()
        .filter(|case| match &config.filter {
            Some(filter) => case.name.contains(filter),
            None => true,
        })
        .collect()
}

fn ensure_corpus_exists(corpus: Corpus, suite_dir: &Path) -> Result<()> {
    let path = corpus.repo_path(suite_dir);
    if path.exists() {
        return Ok(());
    }
    Err(AppError::InvalidIndex(format!(
        "missing benchmark corpus for {} at {}",
        corpus.label(),
        path.display()
    )))
}

fn parse_rg_count_output(stdout: &[u8]) -> Result<usize> {
    let mut total = 0usize;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let line = std::str::from_utf8(line).map_err(|error| {
            AppError::InvalidIndex(format!("rg count output was not utf-8: {error}"))
        })?;
        let count = line
            .rsplit_once(':')
            .map_or(line, |(_, count)| count)
            .parse::<usize>()
            .map_err(|error| {
                AppError::InvalidIndex(format!("failed to parse rg count output '{line}': {error}"))
            })?;
        total += count;
    }
    Ok(total)
}

pub(super) fn prepare_rg_search_scope(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> Result<RgSearchScope> {
    if repo_path.is_dir() {
        return Ok(RgSearchScope {
            current_dir: Some(repo_path.to_path_buf()),
            target: PathBuf::from("."),
            ignore_file: prepare_rg_ignore_file(repo_path, storage_root, config)?,
        });
    }

    Ok(RgSearchScope {
        current_dir: None,
        target: repo_path.to_path_buf(),
        ignore_file: None,
    })
}

fn prepare_rg_ignore_file(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> Result<Option<PathBuf>> {
    let ignore_path = storage_root.join("bench-rg-exclude.ignore");
    if ignore_path.exists() {
        return Ok(Some(ignore_path));
    }

    let repo_root = fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let visible_paths = bench_visible_paths(repo_path, storage_root, config)?;
    let excluded = scan_repository(&bench_build_config(repo_path, storage_root, config))?
        .into_iter()
        .filter_map(|file| {
            let rel = canonical_repo_relative_path(&file.path, &repo_root);
            (!visible_paths.contains(&rel)).then_some(rel)
        })
        .collect::<Vec<_>>();
    if excluded.is_empty() {
        return Ok(None);
    }

    fs::create_dir_all(storage_root)?;
    let mut contents = String::new();
    for path in excluded {
        contents.push('/');
        contents.push_str(&escape_ignore_pattern(&path));
        contents.push('\n');
    }
    fs::write(&ignore_path, contents)?;
    Ok(Some(ignore_path))
}

fn bench_visible_paths(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> Result<HashSet<String>> {
    let build_config = bench_build_config(repo_path, storage_root, config);
    let snapshot = WorkspaceBaseSnapshotIndex::open_read_only(&build_config)?.open_committed()?;
    Ok(snapshot
        .visible_documents()?
        .into_keys()
        .map(|key| key.logical_path)
        .collect())
}

fn canonical_repo_relative_path(path: &Path, repo_root: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    repo_relative_path(&canonical, repo_root)
}

fn escape_ignore_pattern(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            '\\' | '*' | '?' | '[' | ']' | '{' | '}' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn flashgrep_executable() -> Result<PathBuf> {
    if let Ok(path) = env::var("CARGO_BIN_EXE_flashgrep") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let current = env::current_exe()?;
    if current
        .file_stem()
        .is_some_and(|stem| stem == std::ffi::OsStr::new("flashgrep"))
    {
        return Ok(current);
    }

    let candidates = [
        current.parent().map_or_else(
            || PathBuf::from("flashgrep"),
            |parent| parent.join("flashgrep"),
        ),
        current.parent().and_then(Path::parent).map_or_else(
            || PathBuf::from("flashgrep"),
            |parent| parent.join("flashgrep"),
        ),
    ];
    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(PathBuf::from("flashgrep"))
}

pub(super) fn start_bench_daemon(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> Result<BenchDaemon> {
    let ensure_params = EnsureRepoParams {
        repo_path: repo_path.to_path_buf(),
        storage_root: Some(storage_root.to_path_buf()),
        config: RepoConfig {
            tokenizer: match config.tokenizer {
                TokenizerMode::Trigram => crate::daemon::protocol::TokenizerModeConfig::Trigram,
                TokenizerMode::SparseNgram => {
                    crate::daemon::protocol::TokenizerModeConfig::SparseNgram
                }
            },
            corpus_mode: match config.corpus_mode {
                CorpusMode::RespectIgnore => {
                    crate::daemon::protocol::CorpusModeConfig::RespectIgnore
                }
                CorpusMode::NoIgnore => crate::daemon::protocol::CorpusModeConfig::NoIgnore,
            },
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            min_sparse_len: 3,
            max_sparse_len: 8,
        },
        refresh: Default::default(),
    };
    let state_file = daemon_state_file_path(&ensure_params)?;
    let daemon_program = flashgrep_executable()?;
    let mut child = spawn_bench_daemon_process(&daemon_program, &state_file)?;
    let (addr, repo_id) = wait_for_bench_daemon_ready(&mut child, &state_file, &ensure_params)?;
    Ok(BenchDaemon {
        child: Some(child),
        client: DaemonClient::new(addr.clone()),
        addr,
        repo_id,
        state_file,
        shutdown_sent: false,
    })
}

pub(super) fn finish_daemon_run<T>(daemon: BenchDaemon, result: Result<T>) -> Result<T> {
    let shutdown_result = daemon.shutdown();
    match (result, shutdown_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(()) | Err(_)) | (Ok(_), Err(error)) => Err(error),
    }
}

pub(super) fn run_daemon_search_count(
    daemon: &BenchDaemon,
    repo_path: &Path,
    pattern: &str,
    case_insensitive: bool,
    config: &BenchConfig,
    roots: Vec<PathBuf>,
) -> Result<BitfunOutcome> {
    let roots = benchmark_scope_roots(repo_path, roots);
    let response = daemon.client.search(SearchParams {
        repo_id: daemon.repo_id.clone(),
        query: QuerySpec {
            pattern: pattern.to_string(),
            patterns: vec![pattern.to_string()],
            case_insensitive,
            multiline: false,
            dot_matches_new_line: false,
            fixed_strings: false,
            word_regexp: false,
            line_regexp: false,
            before_context: 0,
            after_context: 0,
            top_k_tokens: config.top_k_tokens,
            max_count: None,
            global_max_results: None,
            search_mode: SearchModeConfig::CountOnly,
        },
        scope: PathScope {
            roots,
            globs: Vec::new(),
            iglobs: Vec::new(),
            type_add: Vec::new(),
            type_clear: Vec::new(),
            types: Vec::new(),
            type_not: Vec::new(),
        },
        allow_scan_fallback: false,
    })?;

    match response {
        crate::daemon::protocol::Response::SearchCompleted { results, .. } => Ok(BitfunOutcome {
            candidate_docs: results.candidate_docs,
            match_count: results.matched_lines,
        }),
        other => Err(AppError::Protocol(format!(
            "unexpected daemon bench response: {other:?}"
        ))),
    }
}

pub(super) fn benchmark_scope_roots(repo_path: &Path, roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let canonical_repo = std::fs::canonicalize(repo_path).ok();
    roots
        .into_iter()
        .filter_map(|root| normalize_scope_root(repo_path, canonical_repo.as_deref(), root))
        .collect()
}

pub(super) fn ensure_bench_base_snapshot(
    build_config: &BuildConfig,
    rebuild: bool,
) -> Result<BenchBaseSnapshotBuild> {
    let build_mode = if rebuild {
        BaseSnapshotBuildMode::ForceRebuildSnapshot
    } else {
        BaseSnapshotBuildMode::ReuseExistingSnapshot
    };
    let result = WorkspaceBaseSnapshotIndex::build_with_options(
        build_config,
        &RepoId(build_config.repo_path.to_string_lossy().into_owned()),
        BaseSnapshotBuildOptions::new().with_build_mode(build_mode),
    )?;
    Ok(BenchBaseSnapshotBuild {
        indexed_docs: result.indexed_docs,
        reused_existing: result.reused_existing,
    })
}

fn normalize_scope_root(
    repo_path: &Path,
    canonical_repo: Option<&Path>,
    root: PathBuf,
) -> Option<PathBuf> {
    if root == repo_path {
        return None;
    }
    if let Ok(relative) = root.strip_prefix(repo_path) {
        return (!relative.as_os_str().is_empty()).then(|| relative.to_path_buf());
    }

    let canonical_root = std::fs::canonicalize(&root).ok();
    if let (Some(canonical_repo), Some(canonical_root)) =
        (canonical_repo, canonical_root.as_deref())
    {
        if canonical_root == canonical_repo {
            return None;
        }
        if let Ok(relative) = canonical_root.strip_prefix(canonical_repo) {
            return (!relative.as_os_str().is_empty()).then(|| relative.to_path_buf());
        }
    }

    Some(root)
}

impl Drop for BenchDaemon {
    fn drop(&mut self) {
        let _ = self.request_shutdown();
        let _ = self.wait_for_exit();
    }
}

impl BenchDaemon {
    fn request_shutdown(&mut self) -> Result<()> {
        if self.shutdown_sent {
            return Ok(());
        }
        send_shutdown_request(&self.addr)?;
        self.shutdown_sent = true;
        Ok(())
    }

    fn shutdown(mut self) -> Result<()> {
        self.request_shutdown()?;
        self.wait_for_exit()
    }

    fn wait_for_exit(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.try_wait_child()? {
                self.finish_child_cleanup(status);
                return Ok(());
            }

            if !self.state_file.exists() {
                let status = self.wait_child_blocking()?;
                self.finish_child_cleanup(status);
                return Ok(());
            }

            if Instant::now() >= deadline {
                self.force_kill_child();
                return Ok(());
            }

            thread::sleep(Duration::from_millis(50));
        }
    }

    fn try_wait_child(&mut self) -> Result<Option<ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait().map_err(Into::into),
            None => Ok(Some(success_exit_status())),
        }
    }

    fn wait_child_blocking(&mut self) -> Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => child.wait().map_err(Into::into),
            None => Ok(success_exit_status()),
        }
    }

    fn finish_child_cleanup(&mut self, _status: ExitStatus) {
        self.child.take();
        let _ = fs::remove_file(&self.state_file);
    }

    fn force_kill_child(&mut self) {
        let Some(mut child) = self.child.take() else {
            let _ = fs::remove_file(&self.state_file);
            return;
        };
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(&self.state_file);
    }
}

fn spawn_bench_daemon_process(daemon_program: &Path, state_file: &Path) -> Result<Child> {
    if let Some(parent) = state_file.parent() {
        fs::create_dir_all(parent)?;
    }
    if state_file.exists() {
        fs::remove_file(state_file)?;
    }

    let mut command = Command::new(daemon_program);
    command
        .arg("serve")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--state-file")
        .arg(state_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map_err(Into::into)
}

fn wait_for_bench_daemon_ready(
    child: &mut Child,
    state_file: &Path,
    ensure_params: &EnsureRepoParams,
) -> Result<(String, String)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(AppError::Protocol(format!(
                "bench daemon exited before becoming ready: {status}"
            )));
        }

        if let Some(addr) = read_bench_daemon_addr(state_file)? {
            let client = DaemonClient::new(addr.clone()).with_io_timeout(Duration::from_secs(1));
            match client.ensure_repo(ensure_params.clone()) {
                Ok(Response::RepoEnsured { repo_id, .. }) => return Ok((addr, repo_id)),
                Ok(other) => {
                    return Err(AppError::Protocol(format!(
                        "unexpected ensure_repo response while starting bench daemon: {other:?}"
                    )));
                }
                Err(_) => {}
            }
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(state_file);
            return Err(AppError::Protocol(format!(
                "timed out waiting for bench daemon state file at {}",
                state_file.display()
            )));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn read_bench_daemon_addr(state_file: &Path) -> Result<Option<String>> {
    let contents = match fs::read_to_string(state_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|error| AppError::Protocol(format!("invalid bench daemon state file: {error}")))?;
    Ok(value
        .get("addr")
        .and_then(|addr| addr.as_str())
        .map(std::string::ToString::to_string))
}

fn success_exit_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
}

fn send_shutdown_request(addr: &str) -> Result<()> {
    let stream = match TcpStream::connect(addr) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request = RequestEnvelope {
        jsonrpc: "2.0".into(),
        id: Some(1),
        request: Request::Shutdown,
    };
    let mut writer = BufWriter::new(stream.try_clone()?);
    serde_json::to_writer(&mut writer, &request).map_err(|error| {
        AppError::Protocol(format!("failed to encode shutdown request: {error}"))
    })?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(()),
        Ok(_) => {
            let response: ResponseEnvelope = serde_json::from_str(&line).map_err(|error| {
                AppError::Protocol(format!("failed to decode shutdown response: {error}"))
            })?;
            match response.result {
                Some(Response::ShutdownAck) | None => Ok(()),
                Some(other) => Err(AppError::Protocol(format!(
                    "unexpected shutdown response: {other:?}"
                ))),
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NotConnected
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
