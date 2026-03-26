use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    error::{AppError, Result},
    index::{build_index, IndexSearcher},
    search::SearchMode,
};

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub suite_dir: PathBuf,
    pub filter: Option<String>,
    pub tokenizer: TokenizerMode,
    pub corpus_mode: CorpusMode,
    pub warmup_iter: usize,
    pub bench_iter: usize,
    pub top_k_tokens: usize,
    pub compare_rg: bool,
    pub rebuild: bool,
    pub raw_output: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub corpus_builds: Vec<CorpusBuildRecord>,
    pub summaries: Vec<BenchSummary>,
}

#[derive(Debug, Clone)]
pub struct CorpusBuildRecord {
    pub corpus: &'static str,
    pub tokenizer: TokenizerMode,
    pub docs_indexed: usize,
    pub duration_secs: f64,
}

#[derive(Debug, Clone)]
pub struct BenchSummary {
    pub name: &'static str,
    pub pattern: &'static str,
    pub target: &'static str,
    pub runners: Vec<RunnerSummary>,
}

#[derive(Debug, Clone)]
pub struct RunnerSummary {
    pub runner: String,
    pub mean_secs: f64,
    pub stddev_secs: f64,
    pub min_secs: f64,
    pub sample_count: usize,
    pub candidate_docs: Option<usize>,
    pub match_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Corpus {
    Linux,
    SubtitlesEn,
    SubtitlesRu,
}

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

#[derive(Debug, Clone, Copy)]
struct BenchmarkCase {
    name: &'static str,
    corpus: Corpus,
    pattern: &'static str,
    case_insensitive: bool,
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

pub fn run(config: &BenchConfig) -> Result<BenchReport> {
    let selected = select_benchmarks(config);
    if selected.is_empty() {
        return Err(AppError::InvalidPattern(
            "no benchmarks matched the provided filter".into(),
        ));
    }

    let mut raw_writer = match &config.raw_output {
        Some(path) => Some(RawWriter::create(path)?),
        None => None,
    };

    let corpora = selected
        .iter()
        .map(|case| case.corpus)
        .collect::<std::collections::BTreeSet<_>>();
    let mut index_paths = BTreeMap::new();
    let mut corpus_builds = Vec::new();

    for corpus in corpora {
        ensure_corpus_exists(corpus, &config.suite_dir)?;
        let repo_path = corpus.repo_path(&config.suite_dir);
        let index_path = config.suite_dir.join(".bitfun-bench").join(format!(
            "{}-{}-{}",
            corpus.label(),
            config.corpus_mode.as_str(),
            config.tokenizer.as_str()
        ));

        let should_build = config.rebuild || !index_path.join("lookup.bin").exists();
        if should_build {
            let started = Instant::now();
            let docs_indexed = build_index(&BuildConfig {
                repo_path,
                index_path: index_path.clone(),
                tokenizer: config.tokenizer,
                corpus_mode: config.corpus_mode,
                max_file_size: 512 * 1024 * 1024,
                min_sparse_len: 5,
                max_sparse_len: 16,
            })?;
            let duration_secs = started.elapsed().as_secs_f64();
            corpus_builds.push(CorpusBuildRecord {
                corpus: corpus.label(),
                tokenizer: config.tokenizer,
                docs_indexed,
                duration_secs,
            });
        }
        index_paths.insert(corpus, index_path);
    }

    let mut summaries = Vec::new();
    for benchmark in selected {
        let index_path = index_paths.get(&benchmark.corpus).ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "missing benchmark index for corpus {}",
                benchmark.corpus.label()
            ))
        })?;

        let bitfun = run_bitfun_benchmark(benchmark, index_path, config, raw_writer.as_mut())?;
        let mut runners = vec![bitfun];

        if config.compare_rg {
            runners.push(run_rg_benchmark(
                benchmark,
                &config.suite_dir,
                config,
                raw_writer.as_mut(),
            )?);
        }

        summaries.push(BenchSummary {
            name: benchmark.name,
            pattern: benchmark.pattern,
            target: benchmark.corpus.label(),
            runners,
        });
    }

    Ok(BenchReport {
        corpus_builds,
        summaries,
    })
}

pub fn print_report(report: &BenchReport) {
    for build in &report.corpus_builds {
        println!(
            "build {} [{}]: {:.3}s, docs={}",
            build.corpus,
            build.tokenizer.as_str(),
            build.duration_secs,
            build.docs_indexed
        );
    }
    if !report.corpus_builds.is_empty() && !report.summaries.is_empty() {
        println!();
    }

    for (index, summary) in report.summaries.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let header = format!(
            "{} (target: {}, pattern: {})",
            summary.name, summary.target, summary.pattern
        );
        println!("{header}");
        println!("{}", "-".repeat(header.len()));
        for runner in &summary.runners {
            println!(
                "{}  mean={:.3}s stddev={:.3}s min={:.3}s samples={} candidates={} matches={}",
                runner.runner,
                runner.mean_secs,
                runner.stddev_secs,
                runner.min_secs,
                runner.sample_count,
                runner
                    .candidate_docs
                    .map_or_else(|| "-".to_string(), |count| count.to_string()),
                runner.match_count
            );
        }
    }
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

fn run_bitfun_benchmark(
    benchmark: BenchmarkCase,
    index_path: &Path,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut candidate_docs = None;
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        let _ = run_bitfun_once(benchmark, index_path, config)?;
    }

    for iter in 0..config.bench_iter {
        let started = Instant::now();
        let outcome = run_bitfun_once(benchmark, index_path, config)?;
        let duration_secs = started.elapsed().as_secs_f64();
        candidate_docs = Some(outcome.candidate_docs);
        match_count = outcome.match_count;
        durations.push(duration_secs);

        if let Some(writer) = raw_writer.as_deref_mut() {
            writer.write_sample(RawSample {
                benchmark: benchmark.name,
                corpus: benchmark.corpus.label(),
                runner: "bitfun_grep",
                iteration: iter,
                duration_secs,
                candidate_docs,
                match_count,
            })?;
        }
    }

    Ok(build_summary(
        "bitfun_grep".into(),
        durations,
        candidate_docs,
        match_count,
    ))
}

fn run_bitfun_once(
    benchmark: BenchmarkCase,
    index_path: &Path,
    config: &BenchConfig,
) -> Result<BitfunOutcome> {
    let searcher = IndexSearcher::open(index_path.to_path_buf())?;
    let results = searcher.search(&QueryConfig {
        regex_pattern: benchmark.pattern.to_string(),
        case_insensitive: benchmark.case_insensitive,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: config.top_k_tokens,
        search_mode: SearchMode::CountOnly,
    })?;
    Ok(BitfunOutcome {
        candidate_docs: results.candidate_docs,
        match_count: results.matched_lines,
    })
}

fn run_rg_benchmark(
    benchmark: BenchmarkCase,
    suite_dir: &Path,
    config: &BenchConfig,
    mut raw_writer: Option<&mut RawWriter>,
) -> Result<RunnerSummary> {
    let target = benchmark.corpus.search_target(suite_dir);
    let mut durations = Vec::with_capacity(config.bench_iter);
    let mut match_count = 0;

    for _ in 0..config.warmup_iter {
        let _ = run_rg_once(benchmark, &target, config)?;
    }

    for iter in 0..config.bench_iter {
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
    parse_rg_count_output(&output.stdout)
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

fn build_summary(
    runner: String,
    durations: Vec<f64>,
    candidate_docs: Option<usize>,
    match_count: usize,
) -> RunnerSummary {
    let mean_secs = mean(&durations);
    let stddev_secs = stddev(&durations, mean_secs);
    let min_secs = durations
        .iter()
        .copied()
        .min_by(f64::total_cmp)
        .unwrap_or(0.0);

    RunnerSummary {
        runner,
        mean_secs,
        stddev_secs,
        min_secs,
        sample_count: durations.len(),
        candidate_docs,
        match_count,
    }
}

struct BitfunOutcome {
    candidate_docs: usize,
    match_count: usize,
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn stddev(values: &[f64], mean: f64) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / (values.len() as f64 - 1.0);
    variance.sqrt()
}

struct RawWriter {
    writer: BufWriter<File>,
}

impl RawWriter {
    fn create(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "benchmark,corpus,runner,iteration,duration_secs,candidate_docs,match_count"
        )?;
        Ok(Self { writer })
    }

    fn write_sample(&mut self, sample: RawSample<'_>) -> Result<()> {
        writeln!(
            self.writer,
            "{},{},{},{},{:.9},{},{}",
            sample.benchmark,
            sample.corpus,
            sample.runner,
            sample.iteration,
            sample.duration_secs,
            sample
                .candidate_docs
                .map(|count| count.to_string())
                .unwrap_or_default(),
            sample.match_count
        )?;
        self.writer.flush()?;
        Ok(())
    }
}

struct RawSample<'a> {
    benchmark: &'a str,
    corpus: &'a str,
    runner: &'a str,
    iteration: usize,
    duration_secs: f64,
    candidate_docs: Option<usize>,
    match_count: usize,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{parse_rg_count_output, run, BenchConfig};
    use crate::config::{CorpusMode, TokenizerMode};

    #[test]
    fn bench_runner_smoke_test() {
        let temp = tempdir().expect("test should succeed");
        let suite_dir = temp.path();
        fs::create_dir_all(suite_dir.join("linux")).expect("test should succeed");
        fs::create_dir_all(suite_dir.join("subtitles")).expect("test should succeed");

        fs::write(
            suite_dir.join("linux").join("kernel.c"),
            "PM_RESUME\nERR_SYS\nabc Ah\nΩHolmesΩ\n",
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

        let report = run(&BenchConfig {
            suite_dir: suite_dir.to_path_buf(),
            filter: Some("linux_literal".into()),
            tokenizer: TokenizerMode::Trigram,
            corpus_mode: CorpusMode::RespectIgnore,
            warmup_iter: 0,
            bench_iter: 1,
            top_k_tokens: 4,
            compare_rg: false,
            rebuild: true,
            raw_output: None,
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
}
