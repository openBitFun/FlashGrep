use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

mod execution;
mod output;
mod request;

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    daemon::{
        protocol::{
            ConsistencyMode, OpenRepoParams, PathScope, QuerySpec, RefreshRepoParams, RepoConfig,
            RepoRef, Request, SearchModeConfig, SearchParams,
        },
        serve, DaemonClient, ServerOptions,
    },
    error::{AppError, Result},
    index::build_index,
    search::FileCount,
    tools::bench::BenchQueryMode,
    tools::bench::{self, BenchCacheMode, BenchConfig},
};
use execution::{
    execute_count_matches, execute_count_total_matches, execute_files_with_matches,
    execute_files_without_matches, execute_has_match, execute_search, execute_search_json,
    SearchExecution,
};
use output::{
    format_output_path, print_counts, print_output_line, print_paths, print_stats,
    print_total_counts, rewrite_json_paths, SearchStats,
};
use request::SearchRequest;

#[cfg(test)]
use request::{collect_search_paths, default_print_path, infer_index_path, resolve_context_mode};

#[derive(Debug, Parser)]
#[command(name = "cg")]
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
    Serve(ServeArgs),
    Daemon(DaemonArgs),
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
    #[arg(long)]
    repo: Option<PathBuf>,
    #[arg(long = "pattern")]
    patterns: Vec<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(short = 'i', long, default_value_t = false)]
    case_insensitive: bool,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    corpus_mode: CorpusArg,
    #[arg(long, value_enum, default_value = "warm")]
    cache_mode: BenchCacheArg,
    #[arg(long, value_enum, default_value = "trace")]
    query_mode: BenchQueryArg,
    #[arg(long, default_value_t = 1)]
    warmup_iter: usize,
    #[arg(long, default_value_t = 3)]
    bench_iter: usize,
    #[arg(long, default_value_t = 6)]
    top_k_tokens: usize,
    #[arg(long = "compare-index", alias = "compare-cli", default_value_t = false)]
    compare_index: bool,
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    compare_rg: bool,
    #[arg(long, default_value_t = false)]
    compare_worktree: bool,
    #[arg(long, default_value_t = false)]
    compare_workspace: bool,
    #[arg(long)]
    worktree_sample_files: Option<usize>,
    #[arg(long, default_value_t = false)]
    rebuild: bool,
    #[arg(long)]
    raw_output: Option<PathBuf>,
    #[arg(long)]
    cold_hook: Option<String>,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:4597")]
    bind: String,
    #[arg(long)]
    state_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[command(subcommand)]
    command: DaemonCommands,
}

#[derive(Debug, Subcommand)]
enum DaemonCommands {
    Open(DaemonOpenArgs),
    Ensure(DaemonOpenArgs),
    Status(DaemonRepoArgs),
    Refresh(DaemonRefreshArgs),
    Build(DaemonRepoArgs),
    Rebuild(DaemonRepoArgs),
    Search(DaemonSearchArgs),
    Close(DaemonRepoArgs),
    Shutdown(DaemonConnectArgs),
}

#[derive(Debug, Args, Clone)]
struct DaemonConnectArgs {
    #[arg(long, default_value = "127.0.0.1:4597")]
    addr: String,
}

#[derive(Debug, Args)]
struct DaemonRepoArgs {
    #[command(flatten)]
    connect: DaemonConnectArgs,
    #[arg(long)]
    repo_id: String,
}

#[derive(Debug, Args)]
struct DaemonRefreshArgs {
    #[command(flatten)]
    repo: DaemonRepoArgs,
    #[arg(long, default_value_t = false)]
    force: bool,
}

#[derive(Debug, Args)]
struct DaemonOpenArgs {
    #[command(flatten)]
    connect: DaemonConnectArgs,
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    index: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    corpus_mode: CorpusArg,
    #[arg(short = '.', long, default_value_t = false)]
    hidden: bool,
    #[arg(long, default_value_t = 2 * 1024 * 1024)]
    max_file_size: u64,
    #[arg(long, default_value_t = 5)]
    min_sparse_len: usize,
    #[arg(long, default_value_t = 16)]
    max_sparse_len: usize,
    #[arg(long, default_value_t = 256)]
    rebuild_dirty_threshold: usize,
}

#[derive(Debug, Args)]
struct DaemonSearchArgs {
    #[command(flatten)]
    repo: DaemonRepoArgs,
    #[arg(index = 1, value_name = "PATTERN")]
    pattern: String,
    #[arg(
        short = 'e',
        long = "regexp",
        value_name = "PATTERN",
        allow_hyphen_values = true
    )]
    regexp: Vec<String>,
    #[arg(short = 'F', long = "fixed-strings", default_value_t = false)]
    fixed_strings: bool,
    #[arg(short = 'i', long = "ignore-case", default_value_t = false)]
    ignore_case: bool,
    #[arg(short = 'U', long = "multiline", default_value_t = false)]
    multiline: bool,
    #[arg(long = "multiline-dotall", default_value_t = false)]
    multiline_dotall: bool,
    #[arg(short = 'w', long = "word-regexp", default_value_t = false)]
    word_regexp: bool,
    #[arg(short = 'x', long = "line-regexp", default_value_t = false)]
    line_regexp: bool,
    #[arg(short = 'A', long = "after-context")]
    after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    before_context: Option<usize>,
    #[arg(short = 'm', long = "max-count")]
    max_count: Option<usize>,
    #[arg(short = 'q', long = "quiet", default_value_t = false)]
    quiet: bool,
    #[arg(short = 'c', long = "count", default_value_t = false)]
    count: bool,
    #[arg(long = "count-matches", default_value_t = false)]
    count_matches: bool,
    #[arg(long, value_enum, default_value = "workspace-eventual")]
    consistency: DaemonConsistencyArg,
    #[arg(long, default_value_t = false)]
    allow_scan_fallback: bool,
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
    #[arg(index = 2, value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BenchCacheArg {
    Warm,
    Cold,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BenchQueryArg {
    Same,
    Trace,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DaemonConsistencyArg {
    SnapshotOnly,
    WorkspaceEventual,
    WorkspaceStrict,
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

impl From<BenchCacheArg> for BenchCacheMode {
    fn from(value: BenchCacheArg) -> Self {
        match value {
            BenchCacheArg::Warm => BenchCacheMode::Warm,
            BenchCacheArg::Cold => BenchCacheMode::Cold,
        }
    }
}

impl From<BenchQueryArg> for BenchQueryMode {
    fn from(value: BenchQueryArg) -> Self {
        match value {
            BenchQueryArg::Same => BenchQueryMode::Same,
            BenchQueryArg::Trace => BenchQueryMode::Trace,
        }
    }
}

pub(crate) fn run() -> Result<bool> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Build(args)) => run_build(args),
        Some(Commands::Bench(args)) => run_bench(BenchConfig {
            suite_dir: args.suite_dir,
            filter: args.filter,
            custom_repo: args.repo,
            custom_patterns: args.patterns,
            custom_name: args.name,
            custom_case_insensitive: args.case_insensitive,
            tokenizer: args.tokenizer.into(),
            corpus_mode: args.corpus_mode.into(),
            cache_mode: args.cache_mode.into(),
            query_mode: args.query_mode.into(),
            warmup_iter: args.warmup_iter,
            bench_iter: args.bench_iter,
            top_k_tokens: args.top_k_tokens,
            compare_index: args.compare_index,
            compare_rg: args.compare_rg,
            compare_worktree: args.compare_worktree,
            compare_workspace: args.compare_workspace,
            worktree_sample_files: args.worktree_sample_files,
            rebuild: args.rebuild,
            raw_output: args.raw_output,
            cold_hook: args.cold_hook,
        }),
        Some(Commands::Serve(args)) => run_serve(args),
        Some(Commands::Daemon(args)) => run_daemon(args),
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
        .unwrap_or_else(|| args.repo.join(".codgrep-index"));
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

fn run_serve(args: ServeArgs) -> Result<bool> {
    serve(ServerOptions {
        bind_addr: args.bind,
        state_file: args.state_file,
    })?;
    Ok(true)
}

fn run_daemon(args: DaemonArgs) -> Result<bool> {
    match args.command {
        DaemonCommands::Open(args) => {
            let addr = args.connect.addr.clone();
            let params = build_daemon_open_params(args);
            let client = DaemonClient::new(addr);
            let response = client.open_repo(params)?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Ensure(args) => {
            let addr = args.connect.addr.clone();
            let open = build_daemon_open_params(args);
            let client = DaemonClient::new(addr);
            let response = client.ensure_repo(crate::daemon::protocol::EnsureRepoParams {
                repo_path: open.repo_path,
                index_path: open.index_path,
                config: open.config,
                refresh: open.refresh,
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Status(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::GetRepoStatus {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Refresh(args) => {
            let client = DaemonClient::new(args.repo.connect.addr);
            let response = client.send(Request::RefreshRepo {
                params: RefreshRepoParams {
                    repo_id: args.repo.repo_id,
                    force: args.force,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Build(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::BuildIndex {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Rebuild(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::RebuildIndex {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Search(args) => {
            let client = DaemonClient::new(args.repo.connect.addr);
            let mut patterns = vec![args.pattern.clone()];
            patterns.extend(args.regexp);
            let combined_pattern = combine_daemon_patterns(&patterns, args.fixed_strings);
            let response = client.send(Request::Search {
                params: SearchParams {
                    repo_id: args.repo.repo_id,
                    query: QuerySpec {
                        pattern: combined_pattern,
                        patterns,
                        case_insensitive: args.ignore_case,
                        multiline: args.multiline,
                        dot_matches_new_line: args.multiline_dotall,
                        fixed_strings: args.fixed_strings,
                        word_regexp: args.word_regexp,
                        line_regexp: args.line_regexp,
                        before_context: args.before_context.unwrap_or(0),
                        after_context: args.after_context.unwrap_or(0),
                        top_k_tokens: 6,
                        max_count: args.max_count,
                        search_mode: if args.quiet {
                            SearchModeConfig::FirstHitOnly
                        } else if args.count_matches {
                            SearchModeConfig::CountMatches
                        } else if args.count {
                            SearchModeConfig::CountOnly
                        } else {
                            SearchModeConfig::MaterializeMatches
                        },
                    },
                    scope: PathScope {
                        roots: args.paths,
                        globs: args.glob,
                        iglobs: args.iglob,
                        type_add: args.type_add,
                        type_clear: args.type_clear,
                        types: args.file_type,
                        type_not: args.file_type_not,
                    },
                    consistency: match args.consistency {
                        DaemonConsistencyArg::SnapshotOnly => ConsistencyMode::SnapshotOnly,
                        DaemonConsistencyArg::WorkspaceEventual => {
                            ConsistencyMode::WorkspaceEventual
                        }
                        DaemonConsistencyArg::WorkspaceStrict => ConsistencyMode::WorkspaceStrict,
                    },
                    allow_scan_fallback: args.allow_scan_fallback,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Close(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::CloseRepo {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Shutdown(args) => {
            let client = DaemonClient::new(args.addr);
            let response = client.send(Request::Shutdown)?;
            print_daemon_response(&response)?;
            Ok(true)
        }
    }
}

fn build_daemon_open_params(args: DaemonOpenArgs) -> OpenRepoParams {
    OpenRepoParams {
        repo_path: args.repo,
        index_path: args.index,
        config: RepoConfig {
            tokenizer: match args.tokenizer {
                TokenizerArg::Trigram => crate::daemon::protocol::TokenizerModeConfig::Trigram,
                TokenizerArg::SparseNgram => {
                    crate::daemon::protocol::TokenizerModeConfig::SparseNgram
                }
            },
            corpus_mode: match args.corpus_mode {
                CorpusArg::Ignore => crate::daemon::protocol::CorpusModeConfig::RespectIgnore,
                CorpusArg::NoIgnore => crate::daemon::protocol::CorpusModeConfig::NoIgnore,
            },
            include_hidden: args.hidden,
            max_file_size: args.max_file_size,
            min_sparse_len: args.min_sparse_len,
            max_sparse_len: args.max_sparse_len,
        },
        refresh: crate::daemon::protocol::RefreshPolicyConfig {
            rebuild_dirty_threshold: args.rebuild_dirty_threshold,
        },
    }
}

fn print_daemon_response(response: &crate::daemon::protocol::Response) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, response).map_err(|error| {
        AppError::Protocol(format!("failed to encode daemon response: {error}"))
    })?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn combine_daemon_patterns(patterns: &[String], fixed_strings: bool) -> String {
    if patterns.len() == 1 {
        return if fixed_strings {
            regex::escape(&patterns[0])
        } else {
            patterns[0].clone()
        };
    }

    let branches = patterns
        .iter()
        .map(|pattern| {
            if fixed_strings {
                regex::escape(pattern)
            } else {
                pattern.clone()
            }
        })
        .collect::<Vec<_>>();
    format!("(?:{})", branches.join("|"))
}

fn run_search(args: SearchArgs) -> Result<bool> {
    let process_started = Instant::now();
    let cli_profile = CliProfile::enabled();

    let request_started = Instant::now();
    let request = SearchRequest::from_args(args)?;
    cli_profile.record("request_from_args", request_started.elapsed());
    if request.type_list {
        let defs = request.path_filter.type_definitions();
        if !request.quiet {
            for def in defs {
                println!("{}: {}", def.name(), def.globs().join(", "));
            }
        }
        return Ok(!defs.is_empty());
    }

    if request.files {
        match request.run_ripgrep_files() {
            Ok(output) => {
                if !request.quiet {
                    std::io::stdout().write_all(&output.stdout)?;
                }
                return if output.status.success() || output.status.code() == Some(1) {
                    Ok(output.status.success())
                } else {
                    Err(AppError::InvalidIndex(format!(
                        "rg file listing failed with status {}",
                        output.status
                    )))
                };
            }
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let execution = request.prepare_listing_execution()?;
        let paths = execution.paths();
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

    let execution_started = Instant::now();
    let execution = request.prepare_query_execution(config)?;
    cli_profile.record("resolve_execution", execution_started.elapsed());
    if matches!(execution, SearchExecution::Ripgrep { .. }) {
        let rg_started = Instant::now();
        let result = passthrough_ripgrep_query(&request, config);
        cli_profile.record("rg_execute", rg_started.elapsed());
        cli_profile.finish(process_started.elapsed());
        return result;
    }

    if request.quiet && !request.files_without_match && !request.stats {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let matched = execute_has_match(&execution, config, request.filter())?;
        cli_profile.record("search_execute", search_started.elapsed());
        cli_profile.finish(process_started.elapsed());
        return Ok(matched);
    }

    if request.json {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let mut report = execute_search_json(&execution, config, request.filter())?;
        cli_profile.record("search_execute", search_started.elapsed());
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
        cli_profile.finish(process_started.elapsed());
        return Ok(report.has_match());
    }

    if request.files_with_matches {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let paths = execute_files_with_matches(&execution, config, request.filter())?;
        cli_profile.record("search_execute", search_started.elapsed());
        if !request.quiet {
            print_paths(
                &paths,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        cli_profile.finish(process_started.elapsed());
        return Ok(!paths.is_empty());
    }

    if request.files_without_match {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let paths = execute_files_without_matches(&execution, config, request.filter())?;
        cli_profile.record("search_execute", search_started.elapsed());
        if !request.quiet {
            print_paths(
                &paths,
                &request.current_dir,
                request.display_relative_paths,
                request.display_dot_prefix,
            );
        }
        cli_profile.finish(process_started.elapsed());
        return Ok(!paths.is_empty());
    }

    if request.count {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let counts = if request.only_matching {
            if request.include_zero {
                execute_count_total_matches(&execution, config, request.filter(), true)?
                    .into_iter()
                    .map(|count| FileCount {
                        path: count.path,
                        matched_lines: count.matched_occurrences,
                    })
                    .collect::<Vec<_>>()
            } else {
                execute_count_total_matches(&execution, config, request.filter(), false)?
                    .into_iter()
                    .map(|count| FileCount {
                        path: count.path,
                        matched_lines: count.matched_occurrences,
                    })
                    .collect::<Vec<_>>()
            }
        } else if request.include_zero {
            execute_count_matches(&execution, config, request.filter(), true)?
        } else {
            execute_count_matches(&execution, config, request.filter(), false)?
        };
        let search_elapsed = search_started.elapsed();
        cli_profile.record("search_execute", search_elapsed);
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
            let summary_started = Instant::now();
            let summary = execute_search(&execution, config, request.filter())?;
            cli_profile.record("stats_summary", summary_started.elapsed());
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
        cli_profile.finish(process_started.elapsed());
        return Ok(counts.iter().any(FileCount::has_match));
    }

    if request.count_matches {
        if request.used_default_search_path && execution.is_empty() {
            return Err(AppError::NoFilesSearched);
        }
        let search_started = Instant::now();
        let counts = if request.include_zero {
            execute_count_total_matches(&execution, config, request.filter(), true)?
        } else {
            execute_count_total_matches(&execution, config, request.filter(), false)?
        };
        let search_elapsed = search_started.elapsed();
        cli_profile.record("search_execute", search_elapsed);
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
            let summary_started = Instant::now();
            let summary = execute_search(&execution, config, request.filter())?;
            cli_profile.record("stats_summary", summary_started.elapsed());
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
        cli_profile.finish(process_started.elapsed());
        return Ok(counts.iter().any(crate::search::FileMatchCount::has_match));
    }

    if request.used_default_search_path && execution.is_empty() {
        return Err(AppError::NoFilesSearched);
    }
    let search_started = Instant::now();
    let results = execute_search(&execution, config, request.filter())?;
    let search_elapsed = search_started.elapsed();
    cli_profile.record("search_execute", search_elapsed);
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
    cli_profile.finish(process_started.elapsed());
    Ok(results.has_match())
}

struct CliProfile {
    enabled: bool,
}

impl CliProfile {
    fn enabled() -> Self {
        Self {
            enabled: std::env::var_os("BITFUN_PROFILE_CLI").is_some(),
        }
    }

    fn record(&self, phase: &str, duration: Duration) {
        if self.enabled {
            eprintln!(
                "cli_profile phase={phase} secs={:.6}",
                duration.as_secs_f64()
            );
        }
    }

    fn finish(&self, total: Duration) {
        if self.enabled {
            eprintln!("cli_profile phase=total secs={:.6}", total.as_secs_f64());
        }
    }
}

fn run_bench(config: BenchConfig) -> Result<bool> {
    let report = bench::run(&config)?;
    bench::print_report(&report);
    Ok(true)
}

fn passthrough_ripgrep_query(request: &SearchRequest, config: &QueryConfig) -> Result<bool> {
    let output = request.run_ripgrep_query(config)?;
    if output.status.success() || output.status.code() == Some(1) {
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
        return Ok(output.status.success());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No files were searched") {
        return Err(AppError::NoFilesSearched);
    }
    if stderr.trim().is_empty() {
        return Err(AppError::InvalidIndex(format!(
            "rg fallback failed with status {}",
            output.status
        )));
    }
    Err(AppError::InvalidIndex(stderr.trim_end().to_string()))
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
