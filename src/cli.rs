use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::{
    config::{CorpusMode, TokenizerMode},
    daemon::{
        protocol::{
            ConsistencyMode, EnsureRepoParams, GlobParams, OpenRepoParams, PathScope, QuerySpec,
            RefreshRepoParams, RepoConfig, RepoRef, Request, SearchModeConfig, SearchParams,
            DEFAULT_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
        },
        serve, DaemonClient, ManagedDaemonClient, ServerOptions,
    },
    error::{AppError, Result},
    index_engine::{EngineConfig, IndexDocument, IndexEngine, RepoId, ShardId},
    search::SearchMode,
    tools::bench::BenchQueryMode,
    tools::bench::{self, BenchCacheMode, BenchConfig},
};

#[derive(Debug, Parser)]
#[command(name = "flashgrep")]
#[command(about = "Repository search server and daemon tooling", version)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Build(BuildArgs),
    Bench(BenchArgs),
    Engine(EngineArgs),
    Serve(ServeArgs),
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
struct EngineArgs {
    #[command(subcommand)]
    command: EngineCommands,
}

#[derive(Debug, Subcommand)]
enum EngineCommands {
    Upsert(EngineUpsertArgs),
    Delete(EngineDeleteArgs),
    Refresh(EngineRepoArgs),
    Flush(EngineRepoArgs),
    Commit(EngineRepoArgs),
    Search(EngineSearchArgs),
    Merge(EngineMergeArgs),
    Vacuum(EngineRepoArgs),
    Status(EngineRepoArgs),
}

#[derive(Debug, Args, Clone)]
struct EngineRepoArgs {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    index_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct EngineUpsertArgs {
    #[command(flatten)]
    repo: EngineRepoArgs,
    #[arg(long)]
    path: PathBuf,
    #[arg(long)]
    language: Option<String>,
}

#[derive(Debug, Args)]
struct EngineDeleteArgs {
    #[command(flatten)]
    repo: EngineRepoArgs,
    #[arg(long)]
    path: PathBuf,
}

#[derive(Debug, Args)]
struct EngineSearchArgs {
    #[command(flatten)]
    repo: EngineRepoArgs,
    #[arg(index = 1, value_name = "PATTERN")]
    pattern: String,
    #[arg(short = 'i', long = "ignore-case", default_value_t = false)]
    ignore_case: bool,
    #[arg(long, default_value_t = false)]
    committed: bool,
}

#[derive(Debug, Args)]
struct EngineMergeArgs {
    #[command(flatten)]
    repo: EngineRepoArgs,
    #[arg(long, default_value_t = 1)]
    max_segments: usize,
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long = "storage-root")]
    storage_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum)]
    corpus_mode: Option<CorpusArg>,
    #[arg(long, default_value_t = false)]
    no_ignore: bool,
    #[arg(short = '.', long, default_value_t = false)]
    hidden: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_file_size: u64,
    #[arg(long, default_value_t = 3)]
    min_sparse_len: usize,
    #[arg(long, default_value_t = 8)]
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
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    compare_rg: bool,
    #[arg(long, default_value_t = false)]
    compare_worktree: bool,
    #[arg(long)]
    worktree_sample_files: Option<usize>,
    #[arg(long, default_value_t = 1)]
    worktree_dirty_files: usize,
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
    #[arg(long, default_value_t = false)]
    stdio: bool,
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
    Glob(DaemonGlobArgs),
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
    #[arg(long = "storage-root")]
    storage_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    corpus_mode: CorpusArg,
    #[arg(short = '.', long, default_value_t = false)]
    hidden: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_file_size: u64,
    #[arg(long, default_value_t = 3)]
    min_sparse_len: usize,
    #[arg(long, default_value_t = 8)]
    max_sparse_len: usize,
    #[arg(long, default_value_t = 256)]
    rebuild_dirty_threshold: usize,
    #[arg(
        long,
        default_value_t = DEFAULT_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS
    )]
    overlay_auto_checkpoint_max_uncommitted_ops: u64,
    #[arg(long, default_value_t = 2_000)]
    overlay_merge_min_delay_ms: u64,
    #[arg(long, default_value_t = 10_000)]
    overlay_merge_retry_delay_ms: u64,
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

#[derive(Debug, Args)]
struct DaemonGlobArgs {
    #[command(flatten)]
    repo: DaemonRepoArgs,
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
    #[arg(index = 1, value_name = "PATH")]
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
        Commands::Build(args) => run_build(args),
        Commands::Bench(args) => run_bench(BenchConfig {
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
            compare_rg: args.compare_rg,
            compare_worktree: args.compare_worktree,
            worktree_sample_files: args.worktree_sample_files,
            worktree_dirty_files: args.worktree_dirty_files,
            rebuild: args.rebuild,
            raw_output: args.raw_output,
            cold_hook: args.cold_hook,
        }),
        Commands::Engine(args) => run_engine(args),
        Commands::Serve(args) => run_serve(args),
        Commands::Daemon(args) => run_daemon(args),
    }
}

fn run_engine(args: EngineArgs) -> Result<bool> {
    match args.command {
        EngineCommands::Upsert(args) => {
            let repo_root = canonical_or_original(&args.repo.repo);
            let engine = open_index_engine(&args.repo)?;
            let document = load_engine_document(&repo_root, &args.path, args.language)?;
            let stamp = engine.writer().add_document(document)?;
            println!("upserted seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Delete(args) => {
            let repo_root = canonical_or_original(&args.repo.repo);
            let engine = open_index_engine(&args.repo)?;
            let logical_path = logical_path_from_repo(&repo_root, &args.path)?;
            let stamp = engine
                .writer()
                .delete_document(&crate::index_engine::DocKey {
                    repo_id: RepoId(repo_root.to_string_lossy().into_owned()),
                    logical_path,
                })?;
            println!("deleted seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Refresh(args) => {
            let engine = open_index_engine(&args)?;
            let stamp = engine.writer().refresh()?;
            println!("refreshed seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Flush(args) => {
            let engine = open_index_engine(&args)?;
            let result = engine.writer().flush()?;
            println!(
                "flushed max_seq_no={} segments={} delete_segments={}",
                result.max_seq_no,
                result.segment_ids.len(),
                result.delete_segment_ids.len()
            );
            Ok(true)
        }
        EngineCommands::Commit(args) => {
            let engine = open_index_engine(&args)?;
            let stamp = engine.writer().commit()?;
            println!("committed seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Search(args) => {
            let engine = open_index_engine(&args.repo)?;
            let query = crate::index_engine::query::QueryRequest {
                pattern: args.pattern,
                mode: SearchMode::MaterializeMatches,
                case_insensitive: args.ignore_case,
                top_k: None,
                filters: crate::index_engine::query::QueryFilters::default(),
                prefer_worktree_for_git_head_blob: false,
            };
            let reader = if args.committed {
                engine.reader().open_committed()?
            } else {
                engine.reader().open_snapshot()?
            };
            let results = reader.search(&query)?;
            print_engine_search_results(&results);
            Ok(true)
        }
        EngineCommands::Merge(args) => {
            let engine = open_index_engine(&args.repo)?;
            engine.admin().force_merge(args.max_segments)?;
            let manifest = engine.admin().manifest()?;
            println!(
                "merged generation={} active_segments={} active_delete_segments={}",
                manifest.generation,
                manifest.active_segments.len(),
                manifest.active_delete_segments.len()
            );
            Ok(true)
        }
        EngineCommands::Vacuum(args) => {
            let engine = open_index_engine(&args)?;
            let removed = engine.admin().vacuum()?;
            println!("vacuumed files={removed}");
            Ok(true)
        }
        EngineCommands::Status(args) => {
            let engine = open_index_engine(&args)?;
            let manifest = engine.admin().manifest()?;
            let stats = engine.writer().stats();
            println!(
                "generation={} committed_seq_no={} manifest_refreshed_seq_no={} writer_last_seq_no={} uncommitted_ops={} pending_docs={} active_segments={} active_delete_segments={}",
                manifest.generation,
                manifest.committed_seq_no,
                manifest.refreshed_seq_no,
                stats.last_seq_no,
                stats.last_seq_no.saturating_sub(manifest.committed_seq_no),
                stats.pending_docs,
                manifest.active_segments.len(),
                manifest.active_delete_segments.len()
            );
            Ok(true)
        }
    }
}

fn run_build(args: BuildArgs) -> Result<bool> {
    let corpus_mode = if args.no_ignore {
        CorpusMode::NoIgnore
    } else {
        args.corpus_mode.unwrap_or(CorpusArg::Ignore).into()
    };
    let params = EnsureRepoParams {
        repo_path: args.repo,
        storage_root: args.storage_root,
        config: RepoConfig {
            tokenizer: match args.tokenizer {
                TokenizerArg::Trigram => crate::daemon::protocol::TokenizerModeConfig::Trigram,
                TokenizerArg::SparseNgram => {
                    crate::daemon::protocol::TokenizerModeConfig::SparseNgram
                }
            },
            corpus_mode: match corpus_mode {
                CorpusMode::RespectIgnore => {
                    crate::daemon::protocol::CorpusModeConfig::RespectIgnore
                }
                CorpusMode::NoIgnore => crate::daemon::protocol::CorpusModeConfig::NoIgnore,
            },
            include_hidden: args.hidden,
            max_file_size: args.max_file_size,
            min_sparse_len: args.min_sparse_len,
            max_sparse_len: args.max_sparse_len,
        },
        refresh: crate::daemon::protocol::RefreshPolicyConfig::default(),
    };
    let ensured = ManagedDaemonClient::new().ensure_repo(params)?;
    match ensured.indexed_docs {
        Some(indexed_docs) => {
            println!(
                "ensured repo via daemon at {}: built base snapshot with {indexed_docs} docs",
                ensured.addr
            );
        }
        None => {
            println!(
                "ensured repo via daemon at {}: base snapshot already available",
                ensured.addr
            );
        }
    }
    Ok(true)
}

fn open_index_engine(args: &EngineRepoArgs) -> Result<IndexEngine> {
    let repo_root = canonical_or_original(&args.repo);
    let index_root = args
        .index_root
        .clone()
        .unwrap_or_else(|| repo_root.join(".flashgrep-index-engine"));
    IndexEngine::open(EngineConfig {
        shard_id: ShardId(repo_root.to_string_lossy().into_owned()),
        index_root,
        ..EngineConfig::default()
    })
}

fn load_engine_document(
    repo_root: &Path,
    path: &Path,
    language: Option<String>,
) -> Result<IndexDocument> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    let logical_path = logical_path_from_repo(repo_root, &absolute_path)?;
    let bytes = std::fs::read(&absolute_path)?;
    let metadata = std::fs::metadata(&absolute_path)?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let repo_id = RepoId(repo_root.to_string_lossy().into_owned());
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    let file_name = absolute_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Protocol(format!("invalid file name: {}", absolute_path.display()))
        })?
        .to_string();

    Ok(IndexDocument {
        key: crate::index_engine::DocKey {
            repo_id: repo_id.clone(),
            logical_path: logical_path.clone(),
        },
        repo_id,
        logical_path,
        file_name,
        language: language.or_else(|| infer_language(&absolute_path)),
        size: bytes.len() as u64,
        modified_at,
        content_hash,
        source_bytes: std::sync::Arc::<[u8]>::from(bytes),
    })
}

fn logical_path_from_repo(repo_root: &Path, path: &Path) -> Result<String> {
    let path = canonical_or_original(path);
    let repo_root = canonical_or_original(repo_root);
    let relative = path.strip_prefix(&repo_root).map_err(|_| {
        AppError::Protocol(format!(
            "path {} is not under repo root {}",
            path.display(),
            repo_root.display()
        ))
    })?;
    Ok(relative.to_string_lossy().into_owned())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn infer_language(path: &Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_ascii_lowercase())
}

fn print_engine_search_results(results: &crate::search::SearchResults) {
    println!(
        "candidate_docs={} searches_with_match={} matched_lines={} matched_occurrences={}",
        results.candidate_docs,
        results.searches_with_match,
        results.matched_lines,
        results.matched_occurrences
    );
    for hit in &results.hits {
        println!("file {}", hit.path);
        for file_match in &hit.matches {
            println!(
                "  {}:{} {}",
                file_match.location.line, file_match.location.column, file_match.matched_text
            );
        }
    }
}

fn run_serve(args: ServeArgs) -> Result<bool> {
    serve(ServerOptions {
        bind_addr: args.bind,
        state_file: args.state_file,
        stdio: args.stdio,
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
                storage_root: open.storage_root,
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
            print_daemon_status_response(&response)?;
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
            let response = client.send(Request::BuildBaseSnapshot {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Rebuild(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::RebuildBaseSnapshot {
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
                        global_max_results: None,
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
        DaemonCommands::Glob(args) => {
            let client = DaemonClient::new(args.repo.connect.addr);
            let response = client.glob(GlobParams {
                repo_id: args.repo.repo_id,
                scope: PathScope {
                    roots: args.paths,
                    globs: args.glob,
                    iglobs: args.iglob,
                    type_add: args.type_add,
                    type_clear: args.type_clear,
                    types: args.file_type,
                    type_not: args.file_type_not,
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
        storage_root: args.storage_root,
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
            overlay_auto_checkpoint_max_uncommitted_ops: args
                .overlay_auto_checkpoint_max_uncommitted_ops,
            overlay_merge_min_delay_ms: args.overlay_merge_min_delay_ms,
            overlay_merge_retry_delay_ms: args.overlay_merge_retry_delay_ms,
        },
    }
}

fn print_daemon_response(response: &crate::daemon::protocol::Response) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, response).map_err(|error| {
        AppError::Protocol(format!("failed to encode daemon response: {error}"))
    })?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn print_daemon_status_response(response: &crate::daemon::protocol::Response) -> Result<()> {
    match response {
        crate::daemon::protocol::Response::RepoStatus { status } => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            stdout.write_all(format_repo_status_human(status).as_bytes())?;
            Ok(())
        }
        _ => print_daemon_response(response),
    }
}

fn format_repo_status_human(status: &crate::daemon::protocol::RepoStatus) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "repo_id: {}", status.repo_id);
    let _ = writeln!(output, "repo_path: {}", status.repo_path);
    let _ = writeln!(output, "storage_root: {}", status.storage_root);
    let _ = writeln!(output, "base_snapshot_root: {}", status.base_snapshot_root);
    let _ = writeln!(
        output,
        "workspace_overlay_root: {}",
        status.workspace_overlay_root
    );
    let _ = writeln!(output, "phase: {:?}", status.phase);
    let _ = writeln!(
        output,
        "snapshot_key: {}",
        status.snapshot_key.as_deref().unwrap_or("-")
    );
    let _ = writeln!(
        output,
        "last_probe_unix_secs: {}",
        status
            .last_probe_unix_secs
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    let _ = writeln!(
        output,
        "last_rebuild_unix_secs: {}",
        status
            .last_rebuild_unix_secs
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    let _ = writeln!(
        output,
        "dirty_files: modified={} deleted={} new={}",
        status.dirty_files.modified, status.dirty_files.deleted, status.dirty_files.new
    );
    let _ = writeln!(
        output,
        "rebuild_recommended: {}",
        status.rebuild_recommended
    );
    let _ = writeln!(
        output,
        "active_task_id: {}",
        status.active_task_id.as_deref().unwrap_or("-")
    );
    let _ = writeln!(output, "probe_healthy: {}", status.probe_healthy);
    let _ = writeln!(
        output,
        "last_error: {}",
        status.last_error.as_deref().unwrap_or("-")
    );
    if let Some(overlay) = &status.overlay {
        let _ = writeln!(output, "overlay:");
        let _ = writeln!(output, "  committed_seq_no: {}", overlay.committed_seq_no);
        let _ = writeln!(output, "  last_seq_no: {}", overlay.last_seq_no);
        let _ = writeln!(output, "  uncommitted_ops: {}", overlay.uncommitted_ops);
        let _ = writeln!(output, "  pending_docs: {}", overlay.pending_docs);
        let _ = writeln!(output, "  active_segments: {}", overlay.active_segments);
        let _ = writeln!(
            output,
            "  active_delete_segments: {}",
            overlay.active_delete_segments
        );
        let _ = writeln!(output, "  merge_requested: {}", overlay.merge_requested);
        let _ = writeln!(output, "  merge_running: {}", overlay.merge_running);
        let _ = writeln!(output, "  merge_attempts: {}", overlay.merge_attempts);
        let _ = writeln!(output, "  merge_completed: {}", overlay.merge_completed);
        let _ = writeln!(output, "  merge_failed: {}", overlay.merge_failed);
        let _ = writeln!(
            output,
            "  last_merge_error: {}",
            overlay.last_merge_error.as_deref().unwrap_or("-")
        );
    }
    output
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

fn run_bench(config: BenchConfig) -> Result<bool> {
    if cfg!(debug_assertions) {
        eprintln!(
            "warning: flashgrep bench is running from a debug build; performance results will be pessimistic. use `cargo run --release -- bench ...` or `target/release/flashgrep bench ...` for representative numbers."
        );
    }
    let report = bench::run(&config)?;
    bench::print_report(&report);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::format_repo_status_human;

    #[test]
    fn format_repo_status_human_includes_overlay_section() {
        let status = crate::daemon::protocol::RepoStatus {
            repo_id: "/tmp/repo".into(),
            repo_path: "/tmp/repo".into(),
            storage_root: "/tmp/repo/.flashgrep-index-engine".into(),
            base_snapshot_root: "/tmp/repo/.flashgrep-index-engine/base-snapshot".into(),
            workspace_overlay_root: "/tmp/repo/.flashgrep-index-engine/workspace-overlay".into(),
            phase: crate::daemon::protocol::RepoPhase::ReadyDirty,
            snapshot_key: Some("snapshot-key".into()),
            last_probe_unix_secs: Some(1),
            last_rebuild_unix_secs: Some(2),
            dirty_files: crate::daemon::protocol::DirtyFileStats {
                modified: 1,
                deleted: 2,
                new: 3,
            },
            rebuild_recommended: false,
            active_task_id: None,
            probe_healthy: true,
            last_error: None,
            overlay: Some(crate::daemon::protocol::WorkspaceOverlayStatus {
                committed_seq_no: 4,
                last_seq_no: 5,
                uncommitted_ops: 1,
                pending_docs: 0,
                active_segments: 2,
                active_delete_segments: 1,
                merge_requested: true,
                merge_running: false,
                merge_attempts: 7,
                merge_completed: 6,
                merge_failed: 1,
                last_merge_error: Some("merge failed".into()),
            }),
        };

        let rendered = format_repo_status_human(&status);
        assert!(rendered.contains("overlay:"));
        assert!(rendered.contains("merge_attempts: 7"));
        assert!(rendered.contains("last_merge_error: merge failed"));
    }
}
