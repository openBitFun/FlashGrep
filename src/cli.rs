use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Write},
};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::{
    bench::{self, BenchConfig},
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    error::{AppError, Result},
    files::{scan_paths, ScanOptions},
    index::{build_index, IndexSearcher},
    json_output::{JsonData, JsonMessage},
    path_filter::{normalize_path, PathFilter, PathFilterArgs},
    search::{FileCount, SearchMode},
};

#[derive(Debug, Parser)]
#[command(name = "bitfun-grep")]
#[command(
    about = "Indexed recursive regex search with a ripgrep-like CLI",
    version
)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    #[command(flatten)]
    search: SearchArgs,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Build(BuildArgs),
    Bench(BenchArgs),
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    index: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum)]
    corpus_mode: Option<CorpusArg>,
    #[arg(long, default_value_t = false)]
    no_ignore: bool,
    #[arg(short = '.', long, default_value_t = false)]
    hidden: bool,
    #[arg(long, default_value_t = 2 * 1024 * 1024)]
    max_file_size: u64,
    #[arg(long, default_value_t = 5)]
    min_sparse_len: usize,
    #[arg(long, default_value_t = 16)]
    max_sparse_len: usize,
}

#[derive(Debug, Args)]
struct BenchArgs {
    #[arg(long)]
    suite_dir: PathBuf,
    #[arg(long)]
    filter: Option<String>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    corpus_mode: CorpusArg,
    #[arg(long, default_value_t = 1)]
    warmup_iter: usize,
    #[arg(long, default_value_t = 3)]
    bench_iter: usize,
    #[arg(long, default_value_t = 6)]
    top_k_tokens: usize,
    #[arg(long, default_value_t = false)]
    compare_rg: bool,
    #[arg(long, default_value_t = false)]
    rebuild: bool,
    #[arg(long)]
    raw_output: Option<PathBuf>,
}

#[derive(Debug, Args, Clone, Default)]
struct SearchArgs {
    #[arg(long)]
    index: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    files: bool,
    #[arg(long = "type-list", default_value_t = false)]
    type_list: bool,
    #[arg(long = "json", action = ArgAction::SetTrue, overrides_with = "no_json")]
    json: bool,
    #[arg(long = "no-json", action = ArgAction::SetTrue, overrides_with = "json")]
    no_json: bool,
    #[arg(
        short = 'e',
        long = "regexp",
        value_name = "PATTERN",
        allow_hyphen_values = true
    )]
    regexp: Vec<String>,
    #[arg(short = 'f', long = "file", value_name = "PATTERNFILE")]
    pattern_files: Vec<PathBuf>,
    #[arg(index = 1, value_name = "PATTERN")]
    pattern: Option<String>,
    #[arg(index = 2, value_name = "PATH")]
    paths: Vec<PathBuf>,
    #[arg(short = 'F', long = "fixed-strings", default_value_t = false)]
    fixed_strings: bool,
    #[arg(short = 'i', long = "ignore-case", action = ArgAction::Count, conflicts_with_all = ["case_sensitive", "smart_case"])]
    ignore_case: u8,
    #[arg(short = 's', long = "case-sensitive", action = ArgAction::Count, conflicts_with_all = ["ignore_case", "smart_case"])]
    case_sensitive: u8,
    #[arg(short = 'S', long = "smart-case", action = ArgAction::Count, conflicts_with_all = ["ignore_case", "case_sensitive"])]
    smart_case: u8,
    #[arg(short = 'U', long = "multiline", default_value_t = false)]
    multiline: bool,
    #[arg(long = "multiline-dotall", default_value_t = false)]
    multiline_dotall: bool,
    #[arg(short = 'w', long = "word-regexp", default_value_t = false)]
    word_regexp: bool,
    #[arg(short = 'x', long = "line-regexp", default_value_t = false)]
    line_regexp: bool,
    #[arg(
        short = 'l',
        long = "files-with-matches",
        conflicts_with_all = ["count", "files_without_match"],
        default_value_t = false
    )]
    files_with_matches: bool,
    #[arg(
        long = "files-without-match",
        conflicts_with_all = ["count", "files_with_matches"],
        default_value_t = false
    )]
    files_without_match: bool,
    #[arg(
        short = 'c',
        long = "count",
        conflicts_with_all = ["files_with_matches", "files_without_match"],
        default_value_t = false
    )]
    count: bool,
    #[arg(
        long = "count-matches",
        conflicts_with_all = ["files_with_matches", "files_without_match"],
        default_value_t = false
    )]
    count_matches: bool,
    #[arg(
        long = "include-zero",
        action = ArgAction::SetTrue,
        overrides_with = "no_include_zero"
    )]
    include_zero: bool,
    #[arg(
        long = "no-include-zero",
        action = ArgAction::SetTrue,
        overrides_with = "include_zero"
    )]
    no_include_zero: bool,
    #[arg(short = 'n', long = "line-number", action = ArgAction::SetTrue, overrides_with = "no_line_number")]
    line_number: bool,
    #[arg(short = 'N', long = "no-line-number", action = ArgAction::SetTrue, overrides_with = "line_number")]
    no_line_number: bool,
    #[arg(long, default_value_t = false)]
    column: bool,
    #[arg(short = 'o', long = "only-matching", default_value_t = false)]
    only_matching: bool,
    #[arg(short = 'A', long = "after-context")]
    after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    before_context: Option<usize>,
    #[arg(short = 'C', long = "context")]
    context: Option<usize>,
    #[arg(short = 'H', long = "with-filename", action = ArgAction::SetTrue, overrides_with = "no_filename")]
    with_filename: bool,
    #[arg(short = 'I', long = "no-filename", action = ArgAction::SetTrue, overrides_with = "with_filename")]
    no_filename: bool,
    #[arg(short = 'L', long = "follow", default_value_t = false)]
    follow: bool,
    #[arg(short = '.', long = "hidden", default_value_t = false)]
    hidden: bool,
    #[arg(long = "no-ignore", default_value_t = false)]
    no_ignore: bool,
    #[arg(long = "ignore-file", value_name = "PATH")]
    ignore_file: Vec<PathBuf>,
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    glob: Vec<String>,
    #[arg(long = "iglob", value_name = "GLOB")]
    iglob: Vec<String>,
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    file_type: Vec<String>,
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    file_type_not: Vec<String>,
    #[arg(long = "type-add", value_name = "TYPE_SPEC")]
    type_add: Vec<String>,
    #[arg(long = "type-clear", value_name = "TYPE")]
    type_clear: Vec<String>,
    #[arg(long = "max-depth")]
    max_depth: Option<usize>,
    #[arg(long = "max-filesize")]
    max_filesize: Option<u64>,
    #[arg(short = 'm', long = "max-count")]
    max_count: Option<usize>,
    #[arg(short = 'q', long = "quiet", default_value_t = false)]
    quiet: bool,
    #[arg(long = "stats", action = ArgAction::SetTrue, overrides_with = "no_stats")]
    stats: bool,
    #[arg(long = "no-stats", action = ArgAction::SetTrue, overrides_with = "stats")]
    no_stats: bool,
    #[arg(long, default_value_t = 6, hide = true)]
    top_k_tokens: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TokenizerArg {
    Trigram,
    SparseNgram,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CorpusArg {
    Ignore,
    NoIgnore,
}

impl From<TokenizerArg> for TokenizerMode {
    fn from(value: TokenizerArg) -> Self {
        match value {
            TokenizerArg::Trigram => TokenizerMode::Trigram,
            TokenizerArg::SparseNgram => TokenizerMode::SparseNgram,
        }
    }
}

impl From<CorpusArg> for CorpusMode {
    fn from(value: CorpusArg) -> Self {
        match value {
            CorpusArg::Ignore => CorpusMode::RespectIgnore,
            CorpusArg::NoIgnore => CorpusMode::NoIgnore,
        }
    }
}

pub fn run() -> Result<bool> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Build(args)) => run_build(args),
        Some(Commands::Bench(args)) => run_bench(BenchConfig {
            suite_dir: args.suite_dir,
            filter: args.filter,
            tokenizer: args.tokenizer.into(),
            corpus_mode: args.corpus_mode.into(),
            warmup_iter: args.warmup_iter,
            bench_iter: args.bench_iter,
            top_k_tokens: args.top_k_tokens,
            compare_rg: args.compare_rg,
            rebuild: args.rebuild,
            raw_output: args.raw_output,
        }),
        None => run_search(cli.search),
    }
}

fn run_build(args: BuildArgs) -> Result<bool> {
    let corpus_mode = if args.no_ignore {
        CorpusMode::NoIgnore
    } else {
        args.corpus_mode.unwrap_or(CorpusArg::Ignore).into()
    };
    let index_path = args
        .index
        .unwrap_or_else(|| args.repo.join(".bitfun-index"));
    let config = BuildConfig {
        repo_path: args.repo,
        index_path,
        tokenizer: args.tokenizer.into(),
        corpus_mode,
        include_hidden: args.hidden,
        max_file_size: args.max_file_size,
        min_sparse_len: args.min_sparse_len,
        max_sparse_len: args.max_sparse_len,
    };
    let doc_count = build_index(&config)?;
    println!(
        "indexed {doc_count} docs with {}",
        config.tokenizer.as_str()
    );
    Ok(true)
}

fn run_search(args: SearchArgs) -> Result<bool> {
    let process_started = std::time::Instant::now();
    let request = SearchRequest::from_args(args)?;
    if request.type_list {
        let defs = request.path_filter.type_definitions();
        if !request.quiet {
            for def in defs {
                println!("{}: {}", def.name(), def.globs().join(", "));
            }
        }
        return Ok(!defs.is_empty());
    }

    let searcher = IndexSearcher::open(request.index_path.clone())?;
    let allowed_paths = request.collect_allowed_paths(&searcher)?;

    if request.files {
        let paths = searcher.indexed_paths_with_allowed_paths(None, Some(&allowed_paths));
        if !request.quiet {
            print_paths(
                &paths,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        return Ok(!paths.is_empty());
    }

    let Some(config) = request.config.as_ref() else {
        return Err(AppError::InvalidPattern(
            "a PATTERN or -e/--regexp is required".into(),
        ));
    };

    if request.quiet && !request.files_without_match && !request.stats {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        return Ok(searcher
            .search_with_allowed_paths(config, Some(&request.path_filter), Some(&allowed_paths))?
            .has_match());
    }

    if request.json {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let mut report = searcher.search_json_with_allowed_paths(
            config,
            Some(&request.path_filter),
            Some(&allowed_paths),
        )?;
        rewrite_json_paths(
            &mut report,
            &request.current_dir,
            request.display_relative_paths,
            request.display_dot_prefix,
        );
        if request.quiet {
            let mut stdout = std::io::stdout().lock();
            if let Some(summary) = report.messages().into_iter().last() {
                stdout.write_all(summary.to_line().as_bytes())?;
                stdout.write_all(b"\n")?;
            }
        } else {
            let mut stdout = std::io::stdout().lock();
            report.write(&mut stdout)?;
        }
        return Ok(report.has_match());
    }

    if request.files_with_matches {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let paths = searcher.files_with_matches_and_allowed_paths(
            config,
            Some(&request.path_filter),
            Some(&allowed_paths),
        )?;
        if !request.quiet {
            print_paths(
                &paths,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        return Ok(!paths.is_empty());
    }

    if request.files_without_match {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let paths = searcher.files_without_matches_and_allowed_paths(
            config,
            Some(&request.path_filter),
            Some(&allowed_paths),
        )?;
        if !request.quiet {
            print_paths(
                &paths,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        return Ok(!paths.is_empty());
    }

    if request.count {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = std::time::Instant::now();
        let counts = if request.only_matching {
            if request.include_zero {
                searcher
                    .count_total_matches_by_file_including_zero_with_allowed_paths(
                        config,
                        Some(&request.path_filter),
                        Some(&allowed_paths),
                    )?
                    .into_iter()
                    .map(|count| FileCount {
                        path: count.path,
                        matched_lines: count.matched_occurrences,
                    })
                    .collect::<Vec<_>>()
            } else {
                searcher
                    .count_total_matches_by_file_with_allowed_paths(
                        config,
                        Some(&request.path_filter),
                        Some(&allowed_paths),
                    )?
                    .into_iter()
                    .map(|count| FileCount {
                        path: count.path,
                        matched_lines: count.matched_occurrences,
                    })
                    .collect::<Vec<_>>()
            }
        } else if request.include_zero {
            searcher.count_matches_by_file_including_zero_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?
        } else {
            searcher.count_matches_by_file_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?
        };
        let search_elapsed = search_started.elapsed();
        if !request.quiet {
            print_counts(
                &counts,
                request.print_path,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        if request.stats {
            let summary = searcher.search_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?;
            print_stats(
                SearchStats {
                    matches: if request.only_matching {
                        summary.matched_occurrences
                    } else {
                        summary.matched_lines
                    },
                    matched_lines: summary.matched_lines,
                    searches_with_match: summary.searches_with_match,
                    searches: summary.candidate_docs,
                    bytes_printed: 0,
                    bytes_searched: summary.bytes_searched,
                },
                search_elapsed,
                process_started.elapsed(),
            );
        }
        return Ok(counts.iter().any(FileCount::has_match));
    }

    if request.count_matches {
        if request.used_default_search_path && allowed_paths.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = std::time::Instant::now();
        let counts = if request.include_zero {
            searcher.count_total_matches_by_file_including_zero_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?
        } else {
            searcher.count_total_matches_by_file_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?
        };
        let search_elapsed = search_started.elapsed();
        if !request.quiet {
            print_total_counts(
                &counts,
                request.print_path,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        if request.stats {
            let summary = searcher.search_with_allowed_paths(
                config,
                Some(&request.path_filter),
                Some(&allowed_paths),
            )?;
            print_stats(
                SearchStats {
                    matches: summary.matched_occurrences,
                    matched_lines: summary.matched_lines,
                    searches_with_match: summary.searches_with_match,
                    searches: summary.candidate_docs,
                    bytes_printed: 0,
                    bytes_searched: summary.bytes_searched,
                },
                search_elapsed,
                process_started.elapsed(),
            );
        }
        return Ok(counts.iter().any(crate::search::FileMatchCount::has_match));
    }

    if request.used_default_search_path && allowed_paths.is_empty() {
        return Err(AppError::NoFilesSearched);
    }
    let search_started = std::time::Instant::now();
    let results = searcher.search_with_allowed_paths(
        config,
        Some(&request.path_filter),
        Some(&allowed_paths),
    )?;
    let search_elapsed = search_started.elapsed();
    let mut bytes_printed = 0u64;
    if !request.quiet {
        for hit in &results.hits {
            let mut last_match_line = None;
            for line in &hit.lines {
                match line {
                    crate::search::SearchLine::Match(matched) => {
                        if !request.only_matching && last_match_line == Some(matched.location.line)
                        {
                            continue;
                        }
                        print_output_line(
                            &format_output_path(
                                &hit.path,
                                &request.current_dir,
                                request.display_relative_paths,
                                request.display_dot_prefix,
                            ),
                            matched.location.line,
                            Some(matched.location.column),
                            if request.only_matching {
                                &matched.matched_text
                            } else {
                                &matched.snippet
                            },
                            false,
                            request.print_path,
                            request.print_line_number,
                            request.print_column,
                            &mut bytes_printed,
                        );
                        last_match_line = Some(matched.location.line);
                    }
                    crate::search::SearchLine::Context(context) => {
                        print_output_line(
                            &format_output_path(
                                &hit.path,
                                &request.current_dir,
                                request.display_relative_paths,
                                request.display_dot_prefix,
                            ),
                            context.line_number,
                            None,
                            &context.snippet,
                            true,
                            request.print_path,
                            request.print_line_number,
                            false,
                            &mut bytes_printed,
                        );
                    }
                    crate::search::SearchLine::ContextBreak => {
                        bytes_printed += 3;
                        println!("--");
                    }
                }
            }
        }
    }
    if request.stats {
        print_stats(
            SearchStats {
                matches: results.matched_occurrences,
                matched_lines: results.matched_lines,
                searches_with_match: results.searches_with_match,
                searches: results.candidate_docs,
                bytes_printed,
                bytes_searched: results.bytes_searched,
            },
            search_elapsed,
            process_started.elapsed(),
        );
    }
    Ok(results.has_match())
}

fn run_bench(config: BenchConfig) -> Result<bool> {
    let report = bench::run(&config)?;
    bench::print_report(&report);
    Ok(true)
}

fn print_output_line(
    path: &str,
    line: usize,
    column: Option<usize>,
    snippet: &str,
    is_context: bool,
    print_path: bool,
    print_line_number: bool,
    print_column: bool,
    bytes_printed: &mut u64,
) {
    let mut fields = Vec::new();
    if print_path {
        fields.push(path.to_string());
    }
    if print_line_number {
        fields.push(line.to_string());
    }
    if print_column {
        if let Some(column) = column {
            fields.push(column.to_string());
        }
    }
    let separator = if is_context { '-' } else { ':' };
    let line = if fields.is_empty() {
        snippet.to_string()
    } else {
        format!(
            "{}{separator}{snippet}",
            fields.join(&separator.to_string())
        )
    };
    *bytes_printed += line.len() as u64 + 1;
    println!("{line}");
}

fn print_paths(
    paths: &[String],
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for path in paths {
        println!(
            "{}",
            format_output_path(
                path,
                current_dir,
                display_relative_paths,
                display_dot_prefix
            )
        );
    }
}

fn print_counts(
    counts: &[FileCount],
    print_path: bool,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for count in counts {
        if print_path {
            println!(
                "{}:{}",
                format_output_path(
                    &count.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix
                ),
                count.matched_lines
            );
        } else {
            println!("{}", count.matched_lines);
        }
    }
}

fn print_total_counts(
    counts: &[crate::search::FileMatchCount],
    print_path: bool,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for count in counts {
        if print_path {
            println!(
                "{}:{}",
                format_output_path(
                    &count.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix
                ),
                count.matched_occurrences
            );
        } else {
            println!("{}", count.matched_occurrences);
        }
    }
}

struct SearchStats {
    matches: usize,
    matched_lines: usize,
    searches_with_match: usize,
    searches: usize,
    bytes_printed: u64,
    bytes_searched: u64,
}

fn print_stats(
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
) {
    println!();
    println!("{} matches", stats.matches);
    println!("{} matched lines", stats.matched_lines);
    println!("{} files contained matches", stats.searches_with_match);
    println!("{} files searched", stats.searches);
    println!("{} bytes printed", stats.bytes_printed);
    println!("{} bytes searched", stats.bytes_searched);
    println!(
        "{:.6} seconds spent searching",
        search_elapsed.as_secs_f64()
    );
    println!("{:.6} seconds total", total_elapsed.as_secs_f64());
}

#[derive(Debug)]
struct SearchRequest {
    index_path: PathBuf,
    current_dir: PathBuf,
    search_paths: Vec<PathBuf>,
    used_default_search_path: bool,
    path_filter: PathFilter,
    config: Option<QueryConfig>,
    files: bool,
    type_list: bool,
    json: bool,
    files_with_matches: bool,
    files_without_match: bool,
    count: bool,
    count_matches: bool,
    quiet: bool,
    include_zero: bool,
    only_matching: bool,
    stats: bool,
    print_path: bool,
    print_line_number: bool,
    print_column: bool,
    display_relative_paths: bool,
    display_dot_prefix: bool,
    include_hidden: bool,
    no_ignore: bool,
    ignore_files: Vec<PathBuf>,
    max_depth: Option<usize>,
    max_file_size: Option<u64>,
}

impl SearchRequest {
    fn from_args(args: SearchArgs) -> Result<Self> {
        if args.files && args.type_list {
            return Err(AppError::InvalidPattern(
                "--files and --type-list cannot be used together".into(),
            ));
        }
        if args.files
            && (args.count
                || args.count_matches
                || args.files_with_matches
                || args.files_without_match)
        {
            return Err(AppError::InvalidPattern(
                "--files cannot be combined with -c/--count, --count-matches, -l/--files-with-matches or -L/--files-without-match".into(),
            ));
        }
        if (args.files || args.type_list) && !args.regexp.is_empty() {
            return Err(AppError::InvalidPattern(
                "-e/--regexp is not supported with --files or --type-list".into(),
            ));
        }
        if (args.files || args.type_list) && !args.pattern_files.is_empty() {
            return Err(AppError::InvalidPattern(
                "-f/--file is not supported with --files or --type-list".into(),
            ));
        }
        let json = args.json && !args.no_json;
        let stats = (args.stats && !args.no_stats) || json;
        if json
            && (args.files
                || args.type_list
                || args.files_with_matches
                || args.files_without_match
                || args.count
                || args.count_matches)
        {
            return Err(AppError::InvalidPattern(
                "--json cannot be combined with --files, --type-list, -l/--files-with-matches, -L/--files-without-match, -c/--count or --count-matches".into(),
            ));
        }
        if args.multiline_dotall && !args.multiline {
            return Err(AppError::InvalidPattern(
                "--multiline-dotall requires --multiline".into(),
            ));
        }
        if args.follow {
            return Err(AppError::InvalidPattern(
                "--follow is not supported yet for indexed search".into(),
            ));
        }
        let used_default_search_path = uses_default_search_path(&args);
        let paths = collect_search_paths(&args)?;
        let current_dir = std::env::current_dir()?;
        let normalized_paths = paths
            .iter()
            .map(|path| normalize_path(path, &current_dir))
            .collect::<Vec<_>>();
        let path_filter = PathFilter::new(
            PathFilterArgs {
                roots: normalized_paths.clone(),
                globs: args.glob.clone(),
                iglobs: args.iglob.clone(),
                type_add: args.type_add.clone(),
                type_clear: args.type_clear.clone(),
                types: args.file_type.clone(),
                type_not: args.file_type_not.clone(),
            },
            &current_dir,
        )?;
        let index_path = args
            .index
            .clone()
            .unwrap_or_else(|| infer_index_path(&normalized_paths, &current_dir));
        let print_column = args.column;
        let print_line_number = print_column || (args.line_number && !args.no_line_number);
        let print_path = if args.no_filename {
            false
        } else if args.with_filename {
            true
        } else {
            default_print_path(&normalized_paths)
        };
        let display_relative_paths =
            used_default_search_path || paths.iter().all(|path| !path.is_absolute());
        let display_dot_prefix =
            !used_default_search_path && paths.len() == 1 && paths[0] == Path::new(".");
        let config = if args.files || args.type_list {
            None
        } else {
            let patterns = collect_patterns(&args)?;
            let pattern = combine_patterns(&patterns, args.fixed_strings);
            let case_insensitive = resolve_case_mode(&patterns, &args);
            let search_mode = resolve_search_mode(&args, stats)?;
            let (before_context, after_context) = resolve_context_mode(&args);
            Some(QueryConfig {
                regex_pattern: pattern,
                patterns,
                case_insensitive,
                multiline: args.multiline,
                dot_matches_new_line: args.multiline_dotall,
                fixed_strings: args.fixed_strings,
                word_regexp: args.word_regexp,
                line_regexp: args.line_regexp,
                before_context,
                after_context,
                top_k_tokens: args.top_k_tokens,
                max_count: args.max_count,
                search_mode,
            })
        };

        let ignore_files = args
            .ignore_file
            .iter()
            .map(|path| normalize_path(path, &current_dir))
            .collect();

        Ok(Self {
            index_path,
            current_dir,
            search_paths: normalized_paths,
            used_default_search_path,
            path_filter,
            config,
            files: args.files,
            type_list: args.type_list,
            json,
            files_with_matches: args.files_with_matches,
            files_without_match: args.files_without_match,
            count: args.count,
            count_matches: args.count_matches,
            quiet: args.quiet,
            include_zero: args.include_zero && !args.no_include_zero,
            only_matching: args.only_matching,
            stats,
            print_path,
            print_line_number,
            print_column,
            display_relative_paths,
            display_dot_prefix,
            include_hidden: args.hidden,
            no_ignore: args.no_ignore,
            ignore_files,
            max_depth: args.max_depth,
            max_file_size: args.max_filesize,
        })
    }
}

impl SearchRequest {
    fn collect_allowed_paths(&self, searcher: &IndexSearcher) -> Result<HashSet<String>> {
        searcher.ensure_query_scope_supported(
            self.include_hidden,
            self.no_ignore,
            self.max_file_size,
        )?;
        if let Some(reason) = searcher.stale_reason()? {
            return Err(AppError::InvalidIndex(format!(
                "index is stale: {reason}. Rebuild it with `bitfun-grep build --repo {}`",
                infer_repo_root_for_message(searcher).display()
            )));
        }

        let default_max_file_size = self.max_file_size.or_else(|| {
            searcher
                .build_settings()
                .map(|settings| settings.max_file_size)
        });
        let scanned = scan_paths(
            &self.search_paths,
            Some(&self.index_path),
            ScanOptions {
                respect_ignore: !self.no_ignore,
                include_hidden: self.include_hidden,
                max_file_size: default_max_file_size.unwrap_or(u64::MAX),
                max_depth: self.max_depth,
                ignore_files: self.ignore_files.clone(),
            },
            Some(&self.path_filter),
        )?;

        Ok(scanned
            .into_iter()
            .map(|file| file.path.to_string_lossy().into_owned())
            .collect())
    }
}

fn infer_repo_root_for_message(searcher: &IndexSearcher) -> PathBuf {
    searcher
        .build_settings()
        .map(|settings| PathBuf::from(&settings.repo_root))
        .unwrap_or_else(|| PathBuf::from("<repo>"))
}

fn collect_patterns(args: &SearchArgs) -> Result<Vec<String>> {
    let mut patterns = args.regexp.clone();
    for path in &args.pattern_files {
        patterns.extend(read_patterns_from_file(path)?);
    }
    if patterns.is_empty() {
        let Some(pattern) = args.pattern.clone() else {
            return Err(AppError::InvalidPattern(
                "a PATTERN, -e/--regexp or -f/--file is required".into(),
            ));
        };
        patterns.push(pattern);
    }
    Ok(patterns)
}

fn combine_patterns(patterns: &[String], fixed_strings: bool) -> String {
    if patterns.len() == 1 {
        if fixed_strings {
            return regex_syntax::escape(&patterns[0]);
        }
        return patterns[0].clone();
    }
    patterns
        .iter()
        .map(|pattern| {
            if fixed_strings {
                format!("(?:{})", regex_syntax::escape(pattern))
            } else {
                format!("(?:{pattern})")
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn collect_search_paths(args: &SearchArgs) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if args.files || args.type_list || !args.regexp.is_empty() || !args.pattern_files.is_empty() {
        if let Some(value) = &args.pattern {
            paths.push(PathBuf::from(value));
        }
    }
    paths.extend(args.paths.iter().cloned());
    if paths.is_empty() {
        paths.push(std::env::current_dir()?);
    }
    Ok(paths)
}

fn uses_default_search_path(args: &SearchArgs) -> bool {
    let mut paths = 0usize;
    if (args.files || args.type_list || !args.regexp.is_empty() || !args.pattern_files.is_empty())
        && args.pattern.is_some()
    {
        paths += 1;
    }
    paths += args.paths.len();
    paths == 0
}

fn read_patterns_from_file(path: &Path) -> Result<Vec<String>> {
    if path == Path::new("-") {
        let stdin = io::stdin();
        return stdin
            .lock()
            .lines()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
    }

    let file = File::open(path)?;
    BufReader::new(file)
        .lines()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn resolve_case_mode(patterns: &[String], args: &SearchArgs) -> bool {
    if args.case_sensitive > 0 {
        return false;
    }
    if args.ignore_case > 0 {
        return true;
    }
    args.smart_case > 0
        && patterns.iter().any(|pattern| !pattern.is_empty())
        && !patterns
            .iter()
            .flat_map(|pattern| pattern.chars())
            .any(char::is_uppercase)
}

fn resolve_search_mode(args: &SearchArgs, stats: bool) -> Result<SearchMode> {
    if args.quiet && !stats {
        return Ok(SearchMode::FirstHitOnly);
    }
    if args.files_with_matches {
        return Ok(SearchMode::FirstHitOnly);
    }
    if args.count_matches {
        return Ok(SearchMode::CountMatches);
    }
    if args.count {
        return Ok(if args.only_matching {
            SearchMode::CountMatches
        } else {
            SearchMode::CountOnly
        });
    }
    Ok(SearchMode::MaterializeMatches)
}

fn resolve_context_mode(args: &SearchArgs) -> (usize, usize) {
    let context = args.context.unwrap_or(0);
    (
        args.before_context.unwrap_or(context),
        args.after_context.unwrap_or(context),
    )
}

fn default_print_path(paths: &[PathBuf]) -> bool {
    if paths.len() != 1 {
        return true;
    }
    !paths[0].is_file()
}

fn format_output_path(
    path: &str,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) -> String {
    if !display_relative_paths {
        return path.to_string();
    }
    let candidate = Path::new(path);
    let Ok(relative) = candidate.strip_prefix(current_dir) else {
        return path.to_string();
    };
    if relative.as_os_str().is_empty() {
        return if display_dot_prefix {
            ".".to_string()
        } else {
            String::new()
        };
    }
    let relative = relative.display().to_string();
    if display_dot_prefix {
        format!("./{relative}")
    } else {
        relative
    }
}

fn rewrite_json_paths(
    report: &mut crate::json_output::JsonSearchReport,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for file in &mut report.files {
        if let JsonData::Text { text } = &mut file.path {
            *text = format_output_path(
                text,
                current_dir,
                display_relative_paths,
                display_dot_prefix,
            );
        }
        for message in &mut file.messages {
            match message {
                JsonMessage::Begin(begin) => rewrite_json_data_path(
                    &mut begin.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::End(end) => rewrite_json_data_path(
                    &mut end.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Match(matched) => rewrite_json_data_path(
                    &mut matched.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Context(context) => rewrite_json_data_path(
                    &mut context.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Summary(_) => {}
            }
        }
    }
}

fn rewrite_json_data_path(
    path: &mut Option<JsonData>,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    if let Some(JsonData::Text { text }) = path {
        *text = format_output_path(
            text,
            current_dir,
            display_relative_paths,
            display_dot_prefix,
        );
    }
}

fn infer_index_path(paths: &[PathBuf], current_dir: &Path) -> PathBuf {
    let root = common_search_root(paths).unwrap_or_else(|| current_dir.to_path_buf());
    root.join(".bitfun-index")
}

fn common_search_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut roots = paths
        .iter()
        .map(|path| {
            if path.is_file() {
                path.parent().unwrap_or(path).to_path_buf()
            } else {
                path.to_path_buf()
            }
        })
        .collect::<Vec<_>>();
    let mut common = roots.pop()?;
    for root in roots {
        common = common_prefix(&common, &root)?;
    }
    Some(common)
}

fn common_prefix(left: &Path, right: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for (left_component, right_component) in left.components().zip(right.components()) {
        if left_component != right_component {
            break;
        }
        prefix.push(left_component.as_os_str());
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
