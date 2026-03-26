use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    bench::{self, BenchConfig},
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    error::Result,
    index::{build_index, IndexSearcher},
    search::SearchMode,
};

#[derive(Debug, Parser)]
#[command(name = "bitfun-grep")]
#[command(about = "Local regex prefilter indexer for repository search", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Build {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        index: PathBuf,
        #[arg(long, value_enum, default_value = "sparse-ngram")]
        tokenizer: TokenizerArg,
        #[arg(long, value_enum, default_value = "ignore")]
        corpus_mode: CorpusArg,
        #[arg(long, default_value_t = 2 * 1024 * 1024)]
        max_file_size: u64,
        #[arg(long, default_value_t = 5)]
        min_sparse_len: usize,
        #[arg(long, default_value_t = 16)]
        max_sparse_len: usize,
    },
    Search {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        pattern: String,
        #[arg(long, default_value_t = false)]
        case_insensitive: bool,
        #[arg(long, default_value_t = false)]
        multiline: bool,
        #[arg(long, default_value_t = false)]
        dot_matches_new_line: bool,
        #[arg(long, default_value_t = 6)]
        top_k_tokens: usize,
        #[arg(long, value_enum, default_value = "materialize-matches")]
        search_mode: SearchModeArg,
    },
    Candidates {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        pattern: String,
        #[arg(long, default_value_t = false)]
        case_insensitive: bool,
        #[arg(long, default_value_t = false)]
        multiline: bool,
        #[arg(long, default_value_t = false)]
        dot_matches_new_line: bool,
        #[arg(long, default_value_t = 6)]
        top_k_tokens: usize,
    },
    Bench {
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
    },
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
enum SearchModeArg {
    CountOnly,
    FirstHitOnly,
    MaterializeMatches,
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

impl From<SearchModeArg> for SearchMode {
    fn from(value: SearchModeArg) -> Self {
        match value {
            SearchModeArg::CountOnly => SearchMode::CountOnly,
            SearchModeArg::FirstHitOnly => SearchMode::FirstHitOnly,
            SearchModeArg::MaterializeMatches => SearchMode::MaterializeMatches,
        }
    }
}

fn query_config(
    pattern: String,
    case_insensitive: bool,
    multiline: bool,
    dot_matches_new_line: bool,
    top_k_tokens: usize,
    search_mode: SearchMode,
) -> QueryConfig {
    QueryConfig {
        regex_pattern: pattern,
        case_insensitive,
        multiline,
        dot_matches_new_line,
        top_k_tokens,
        search_mode,
    }
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Build {
            repo,
            index,
            tokenizer,
            corpus_mode,
            max_file_size,
            min_sparse_len,
            max_sparse_len,
        } => run_build(
            repo,
            index,
            tokenizer,
            corpus_mode,
            max_file_size,
            min_sparse_len,
            max_sparse_len,
        ),
        Commands::Search {
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
            search_mode,
        } => run_search(
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
            search_mode.into(),
        ),
        Commands::Candidates {
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
        } => run_candidates(
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
        ),
        Commands::Bench {
            suite_dir,
            filter,
            tokenizer,
            corpus_mode,
            warmup_iter,
            bench_iter,
            top_k_tokens,
            compare_rg,
            rebuild,
            raw_output,
        } => run_bench(BenchConfig {
            suite_dir,
            filter,
            tokenizer: tokenizer.into(),
            corpus_mode: corpus_mode.into(),
            warmup_iter,
            bench_iter,
            top_k_tokens,
            compare_rg,
            rebuild,
            raw_output,
        }),
    }
}

fn run_build(
    repo: PathBuf,
    index: PathBuf,
    tokenizer: TokenizerArg,
    corpus_mode: CorpusArg,
    max_file_size: u64,
    min_sparse_len: usize,
    max_sparse_len: usize,
) -> Result<()> {
    let config = BuildConfig {
        repo_path: repo,
        index_path: index,
        tokenizer: tokenizer.into(),
        corpus_mode: corpus_mode.into(),
        max_file_size,
        min_sparse_len,
        max_sparse_len,
    };
    let doc_count = build_index(&config)?;
    println!(
        "indexed {doc_count} docs with {}",
        config.tokenizer.as_str()
    );
    Ok(())
}

fn run_search(
    index: PathBuf,
    pattern: String,
    case_insensitive: bool,
    multiline: bool,
    dot_matches_new_line: bool,
    top_k_tokens: usize,
    search_mode: SearchMode,
) -> Result<()> {
    let config = query_config(
        pattern,
        case_insensitive,
        multiline,
        dot_matches_new_line,
        top_k_tokens,
        search_mode,
    );
    let searcher = IndexSearcher::open(index)?;
    let results = searcher.search(&config)?;
    println!("candidate docs: {}", results.candidate_docs);
    println!("matched lines: {}", results.matched_lines);
    for hit in results.hits {
        println!("{}", hit.path);
        for matched in hit.matches {
            println!(
                "  {}:{} {}",
                matched.location.line, matched.location.column, matched.snippet
            );
        }
    }
    Ok(())
}

fn run_candidates(
    index: PathBuf,
    pattern: String,
    case_insensitive: bool,
    multiline: bool,
    dot_matches_new_line: bool,
    top_k_tokens: usize,
) -> Result<()> {
    let config = query_config(
        pattern,
        case_insensitive,
        multiline,
        dot_matches_new_line,
        top_k_tokens,
        SearchMode::MaterializeMatches,
    );
    let searcher = IndexSearcher::open(index)?;
    for path in searcher.candidate_paths(&config)? {
        println!("{path}");
    }
    Ok(())
}

fn run_bench(config: BenchConfig) -> Result<()> {
    let report = bench::run(&config)?;
    bench::print_report(&report);
    Ok(())
}
