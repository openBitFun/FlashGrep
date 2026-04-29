use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::{
    config::{CorpusMode, TokenizerMode},
    daemon::protocol::{
        ConsistencyMode, EnsureRepoParams, GlobParams, OpenRepoParams, PathScope, QuerySpec,
        RepoConfig, SearchModeConfig, SearchParams,
        DEFAULT_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
    },
    tools::bench::BenchQueryMode,
    tools::bench::{BenchCacheMode, BenchConfig},
};

#[derive(Debug, Parser)]
#[command(name = "flashgrep")]
#[command(about = "Repository search server and daemon tooling", version)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Commands,
}

#[derive(Debug, Subcommand)]
pub(super) enum Commands {
    Build(BuildArgs),
    Bench(BenchArgs),
    Engine(EngineArgs),
    Serve(ServeArgs),
    Daemon(DaemonArgs),
}

#[derive(Debug, Args)]
pub(super) struct EngineArgs {
    #[command(subcommand)]
    pub(super) command: EngineCommands,
}

#[derive(Debug, Subcommand)]
pub(super) enum EngineCommands {
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
pub(super) struct EngineRepoArgs {
    #[arg(long)]
    pub(super) repo: PathBuf,
    #[arg(long)]
    pub(super) index_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct EngineUpsertArgs {
    #[command(flatten)]
    pub(super) repo: EngineRepoArgs,
    #[arg(long)]
    pub(super) path: PathBuf,
    #[arg(long)]
    pub(super) language: Option<String>,
}

#[derive(Debug, Args)]
pub(super) struct EngineDeleteArgs {
    #[command(flatten)]
    pub(super) repo: EngineRepoArgs,
    #[arg(long)]
    pub(super) path: PathBuf,
}

#[derive(Debug, Args)]
pub(super) struct EngineSearchArgs {
    #[command(flatten)]
    pub(super) repo: EngineRepoArgs,
    #[arg(index = 1, value_name = "PATTERN")]
    pub(super) pattern: String,
    #[arg(short = 'i', long = "ignore-case", default_value_t = false)]
    pub(super) ignore_case: bool,
    #[arg(long, default_value_t = false)]
    pub(super) committed: bool,
}

#[derive(Debug, Args)]
pub(super) struct EngineMergeArgs {
    #[command(flatten)]
    pub(super) repo: EngineRepoArgs,
    #[arg(long, default_value_t = 1)]
    pub(super) max_segments: usize,
}

#[derive(Debug, Args)]
pub(super) struct BuildArgs {
    #[arg(long)]
    pub(super) repo: PathBuf,
    #[arg(long = "storage-root")]
    pub(super) storage_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    pub(super) tokenizer: TokenizerArg,
    #[arg(long, value_enum)]
    pub(super) corpus_mode: Option<CorpusArg>,
    #[arg(long, default_value_t = false)]
    pub(super) no_ignore: bool,
    #[arg(short = '.', long, default_value_t = false)]
    pub(super) hidden: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub(super) max_file_size: u64,
    #[arg(long, default_value_t = 3)]
    pub(super) min_sparse_len: usize,
    #[arg(long, default_value_t = 8)]
    pub(super) max_sparse_len: usize,
}

impl BuildArgs {
    pub(super) fn into_ensure_params(self) -> EnsureRepoParams {
        let corpus_mode = if self.no_ignore {
            CorpusMode::NoIgnore
        } else {
            self.corpus_mode.unwrap_or(CorpusArg::Ignore).into()
        };
        EnsureRepoParams {
            repo_path: self.repo,
            storage_root: self.storage_root,
            config: RepoConfig {
                tokenizer: self.tokenizer.into(),
                corpus_mode: match corpus_mode {
                    CorpusMode::RespectIgnore => {
                        crate::daemon::protocol::CorpusModeConfig::RespectIgnore
                    }
                    CorpusMode::NoIgnore => crate::daemon::protocol::CorpusModeConfig::NoIgnore,
                },
                include_hidden: self.hidden,
                max_file_size: self.max_file_size,
                min_sparse_len: self.min_sparse_len,
                max_sparse_len: self.max_sparse_len,
            },
            refresh: crate::daemon::protocol::RefreshPolicyConfig::default(),
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct BenchArgs {
    #[arg(long)]
    pub(super) suite_dir: PathBuf,
    #[arg(long)]
    pub(super) filter: Option<String>,
    #[arg(long)]
    pub(super) repo: Option<PathBuf>,
    #[arg(long = "pattern")]
    pub(super) patterns: Vec<String>,
    #[arg(long)]
    pub(super) name: Option<String>,
    #[arg(short = 'i', long, default_value_t = false)]
    pub(super) case_insensitive: bool,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    pub(super) tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    pub(super) corpus_mode: CorpusArg,
    #[arg(long, value_enum, default_value = "warm")]
    pub(super) cache_mode: BenchCacheArg,
    #[arg(long, value_enum, default_value = "trace")]
    pub(super) query_mode: BenchQueryArg,
    #[arg(long, default_value_t = 1)]
    pub(super) warmup_iter: usize,
    #[arg(long, default_value_t = 3)]
    pub(super) bench_iter: usize,
    #[arg(long, default_value_t = 6)]
    pub(super) top_k_tokens: usize,
    #[arg(long, action = ArgAction::Set, default_value_t = true)]
    pub(super) compare_rg: bool,
    #[arg(long, default_value_t = false)]
    pub(super) compare_worktree: bool,
    #[arg(long)]
    pub(super) worktree_sample_files: Option<usize>,
    #[arg(long, default_value_t = 1)]
    pub(super) worktree_dirty_files: usize,
    #[arg(long, default_value_t = false)]
    pub(super) rebuild: bool,
    #[arg(long)]
    pub(super) raw_output: Option<PathBuf>,
    #[arg(long)]
    pub(super) cold_hook: Option<String>,
}

impl BenchArgs {
    pub(super) fn into_config(self) -> BenchConfig {
        BenchConfig {
            suite_dir: self.suite_dir,
            filter: self.filter,
            custom_repo: self.repo,
            custom_patterns: self.patterns,
            custom_name: self.name,
            custom_case_insensitive: self.case_insensitive,
            tokenizer: self.tokenizer.into(),
            corpus_mode: self.corpus_mode.into(),
            cache_mode: self.cache_mode.into(),
            query_mode: self.query_mode.into(),
            warmup_iter: self.warmup_iter,
            bench_iter: self.bench_iter,
            top_k_tokens: self.top_k_tokens,
            compare_rg: self.compare_rg,
            compare_worktree: self.compare_worktree,
            worktree_sample_files: self.worktree_sample_files,
            worktree_dirty_files: self.worktree_dirty_files,
            rebuild: self.rebuild,
            raw_output: self.raw_output,
            cold_hook: self.cold_hook,
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:4597")]
    pub(super) bind: String,
    #[arg(long)]
    pub(super) state_file: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    pub(super) stdio: bool,
}

#[derive(Debug, Args)]
pub(super) struct DaemonArgs {
    #[command(subcommand)]
    pub(super) command: DaemonCommands,
}

#[derive(Debug, Subcommand)]
pub(super) enum DaemonCommands {
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
pub(super) struct DaemonConnectArgs {
    #[arg(long, default_value = "127.0.0.1:4597")]
    pub(super) addr: String,
}

#[derive(Debug, Args)]
pub(super) struct DaemonRepoArgs {
    #[command(flatten)]
    pub(super) connect: DaemonConnectArgs,
    #[arg(long)]
    pub(super) repo_id: String,
}

#[derive(Debug, Args)]
pub(super) struct DaemonRefreshArgs {
    #[command(flatten)]
    pub(super) repo: DaemonRepoArgs,
    #[arg(long, default_value_t = false)]
    pub(super) force: bool,
}

#[derive(Debug, Args)]
pub(super) struct DaemonOpenArgs {
    #[command(flatten)]
    pub(super) connect: DaemonConnectArgs,
    #[arg(long)]
    pub(super) repo: PathBuf,
    #[arg(long = "storage-root")]
    pub(super) storage_root: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "sparse-ngram")]
    pub(super) tokenizer: TokenizerArg,
    #[arg(long, value_enum, default_value = "ignore")]
    pub(super) corpus_mode: CorpusArg,
    #[arg(short = '.', long, default_value_t = false)]
    pub(super) hidden: bool,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub(super) max_file_size: u64,
    #[arg(long, default_value_t = 3)]
    pub(super) min_sparse_len: usize,
    #[arg(long, default_value_t = 8)]
    pub(super) max_sparse_len: usize,
    #[arg(long, default_value_t = 256)]
    pub(super) rebuild_dirty_threshold: usize,
    #[arg(
        long,
        default_value_t = DEFAULT_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS
    )]
    pub(super) overlay_auto_checkpoint_max_uncommitted_ops: u64,
    #[arg(long, default_value_t = 2_000)]
    pub(super) overlay_merge_min_delay_ms: u64,
    #[arg(long, default_value_t = 10_000)]
    pub(super) overlay_merge_retry_delay_ms: u64,
}

impl DaemonOpenArgs {
    pub(super) fn addr(&self) -> String {
        self.connect.addr.clone()
    }

    pub(super) fn into_open_repo_params(self) -> OpenRepoParams {
        OpenRepoParams {
            repo_path: self.repo,
            storage_root: self.storage_root,
            config: RepoConfig {
                tokenizer: self.tokenizer.into(),
                corpus_mode: self.corpus_mode.into(),
                include_hidden: self.hidden,
                max_file_size: self.max_file_size,
                min_sparse_len: self.min_sparse_len,
                max_sparse_len: self.max_sparse_len,
            },
            refresh: crate::daemon::protocol::RefreshPolicyConfig {
                rebuild_dirty_threshold: self.rebuild_dirty_threshold,
                overlay_auto_checkpoint_max_uncommitted_ops: self
                    .overlay_auto_checkpoint_max_uncommitted_ops,
                overlay_merge_min_delay_ms: self.overlay_merge_min_delay_ms,
                overlay_merge_retry_delay_ms: self.overlay_merge_retry_delay_ms,
            },
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct DaemonSearchArgs {
    #[command(flatten)]
    pub(super) repo: DaemonRepoArgs,
    #[arg(index = 1, value_name = "PATTERN")]
    pub(super) pattern: String,
    #[arg(
        short = 'e',
        long = "regexp",
        value_name = "PATTERN",
        allow_hyphen_values = true
    )]
    pub(super) regexp: Vec<String>,
    #[arg(short = 'F', long = "fixed-strings", default_value_t = false)]
    pub(super) fixed_strings: bool,
    #[arg(short = 'i', long = "ignore-case", default_value_t = false)]
    pub(super) ignore_case: bool,
    #[arg(short = 'U', long = "multiline", default_value_t = false)]
    pub(super) multiline: bool,
    #[arg(long = "multiline-dotall", default_value_t = false)]
    pub(super) multiline_dotall: bool,
    #[arg(short = 'w', long = "word-regexp", default_value_t = false)]
    pub(super) word_regexp: bool,
    #[arg(short = 'x', long = "line-regexp", default_value_t = false)]
    pub(super) line_regexp: bool,
    #[arg(short = 'A', long = "after-context")]
    pub(super) after_context: Option<usize>,
    #[arg(short = 'B', long = "before-context")]
    pub(super) before_context: Option<usize>,
    #[arg(short = 'm', long = "max-count")]
    pub(super) max_count: Option<usize>,
    #[arg(short = 'q', long = "quiet", default_value_t = false)]
    pub(super) quiet: bool,
    #[arg(short = 'c', long = "count", default_value_t = false)]
    pub(super) count: bool,
    #[arg(long = "count-matches", default_value_t = false)]
    pub(super) count_matches: bool,
    #[arg(long, value_enum, default_value = "workspace-eventual")]
    pub(super) consistency: DaemonConsistencyArg,
    #[arg(long, default_value_t = false)]
    pub(super) allow_scan_fallback: bool,
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    pub(super) glob: Vec<String>,
    #[arg(long = "iglob", value_name = "GLOB")]
    pub(super) iglob: Vec<String>,
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub(super) file_type: Vec<String>,
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    pub(super) file_type_not: Vec<String>,
    #[arg(long = "type-add", value_name = "TYPE_SPEC")]
    pub(super) type_add: Vec<String>,
    #[arg(long = "type-clear", value_name = "TYPE")]
    pub(super) type_clear: Vec<String>,
    #[arg(index = 2, value_name = "PATH")]
    pub(super) paths: Vec<PathBuf>,
}

impl DaemonSearchArgs {
    pub(super) fn into_search_params(self) -> SearchParams {
        let Self {
            repo,
            pattern,
            regexp,
            fixed_strings,
            ignore_case,
            multiline,
            multiline_dotall,
            word_regexp,
            line_regexp,
            after_context,
            before_context,
            max_count,
            quiet,
            count,
            count_matches,
            consistency,
            allow_scan_fallback,
            glob,
            iglob,
            file_type,
            file_type_not,
            type_add,
            type_clear,
            paths,
        } = self;
        let mut patterns = vec![pattern.clone()];
        patterns.extend(regexp);
        SearchParams {
            repo_id: repo.repo_id,
            query: QuerySpec {
                pattern: combine_patterns(&patterns, fixed_strings),
                patterns,
                case_insensitive: ignore_case,
                multiline,
                dot_matches_new_line: multiline_dotall,
                fixed_strings,
                word_regexp,
                line_regexp,
                before_context: before_context.unwrap_or(0),
                after_context: after_context.unwrap_or(0),
                top_k_tokens: 6,
                max_count,
                global_max_results: None,
                search_mode: select_search_mode(quiet, count, count_matches),
            },
            scope: path_scope(
                paths,
                glob,
                iglob,
                type_add,
                type_clear,
                file_type,
                file_type_not,
            ),
            consistency: consistency.into(),
            allow_scan_fallback,
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct DaemonGlobArgs {
    #[command(flatten)]
    pub(super) repo: DaemonRepoArgs,
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    pub(super) glob: Vec<String>,
    #[arg(long = "iglob", value_name = "GLOB")]
    pub(super) iglob: Vec<String>,
    #[arg(short = 't', long = "type", value_name = "TYPE")]
    pub(super) file_type: Vec<String>,
    #[arg(short = 'T', long = "type-not", value_name = "TYPE")]
    pub(super) file_type_not: Vec<String>,
    #[arg(long = "type-add", value_name = "TYPE_SPEC")]
    pub(super) type_add: Vec<String>,
    #[arg(long = "type-clear", value_name = "TYPE")]
    pub(super) type_clear: Vec<String>,
    #[arg(index = 1, value_name = "PATH")]
    pub(super) paths: Vec<PathBuf>,
}

impl DaemonGlobArgs {
    pub(super) fn into_glob_params(self) -> GlobParams {
        GlobParams {
            repo_id: self.repo.repo_id,
            scope: path_scope(
                self.paths,
                self.glob,
                self.iglob,
                self.type_add,
                self.type_clear,
                self.file_type,
                self.file_type_not,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum BenchCacheArg {
    Warm,
    Cold,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum BenchQueryArg {
    Same,
    Trace,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum TokenizerArg {
    Trigram,
    SparseNgram,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum CorpusArg {
    Ignore,
    NoIgnore,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(super) enum DaemonConsistencyArg {
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

impl From<TokenizerArg> for crate::daemon::protocol::TokenizerModeConfig {
    fn from(value: TokenizerArg) -> Self {
        match value {
            TokenizerArg::Trigram => Self::Trigram,
            TokenizerArg::SparseNgram => Self::SparseNgram,
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

impl From<CorpusArg> for crate::daemon::protocol::CorpusModeConfig {
    fn from(value: CorpusArg) -> Self {
        match value {
            CorpusArg::Ignore => Self::RespectIgnore,
            CorpusArg::NoIgnore => Self::NoIgnore,
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

impl From<DaemonConsistencyArg> for ConsistencyMode {
    fn from(value: DaemonConsistencyArg) -> Self {
        match value {
            DaemonConsistencyArg::SnapshotOnly => ConsistencyMode::SnapshotOnly,
            DaemonConsistencyArg::WorkspaceEventual => ConsistencyMode::WorkspaceEventual,
            DaemonConsistencyArg::WorkspaceStrict => ConsistencyMode::WorkspaceStrict,
        }
    }
}

fn path_scope(
    roots: Vec<PathBuf>,
    globs: Vec<String>,
    iglobs: Vec<String>,
    type_add: Vec<String>,
    type_clear: Vec<String>,
    types: Vec<String>,
    type_not: Vec<String>,
) -> PathScope {
    PathScope {
        roots,
        globs,
        iglobs,
        type_add,
        type_clear,
        types,
        type_not,
    }
}

fn select_search_mode(quiet: bool, count: bool, count_matches: bool) -> SearchModeConfig {
    if quiet {
        SearchModeConfig::FirstHitOnly
    } else if count_matches {
        SearchModeConfig::CountMatches
    } else if count {
        SearchModeConfig::CountOnly
    } else {
        SearchModeConfig::MaterializeMatches
    }
}

fn combine_patterns(patterns: &[String], fixed_strings: bool) -> String {
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
