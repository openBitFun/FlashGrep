use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{
    bench::{self, BenchConfig},
    config::{BuildConfig, QueryConfig, TokenizerMode},
    error::Result,
    index::{build_index, IndexSearcher},
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
        #[arg(long, value_enum, default_value = "trigram")]
        tokenizer: TokenizerArg,
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
        #[arg(long, value_enum, default_value = "trigram")]
        tokenizer: TokenizerArg,
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

impl From<TokenizerArg> for TokenizerMode {
    fn from(value: TokenizerArg) -> Self {
        match value {
            TokenizerArg::Trigram => TokenizerMode::Trigram,
            TokenizerArg::SparseNgram => TokenizerMode::SparseNgram,
        }
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build {
            repo,
            index,
            tokenizer,
            max_file_size,
            min_sparse_len,
            max_sparse_len,
        } => {
            let config = BuildConfig {
                repo_path: repo,
                index_path: index,
                tokenizer: tokenizer.into(),
                max_file_size,
                min_sparse_len,
                max_sparse_len,
            };
            let doc_count = build_index(&config)?;
            println!(
                "indexed {doc_count} docs with {}",
                config.tokenizer.as_str()
            );
        }
        Commands::Search {
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
        } => {
            let config = QueryConfig {
                regex_pattern: pattern,
                case_insensitive,
                multiline,
                dot_matches_new_line,
                top_k_tokens,
            };
            let searcher = IndexSearcher::open(index)?;
            let results = searcher.search(&config)?;
            println!("candidate docs: {}", results.candidate_docs);
            for hit in results.hits {
                println!("{}", hit.path);
                for matched in hit.matches {
                    println!(
                        "  {}:{} {}",
                        matched.location.line, matched.location.column, matched.snippet
                    );
                }
            }
        }
        Commands::Candidates {
            index,
            pattern,
            case_insensitive,
            multiline,
            dot_matches_new_line,
            top_k_tokens,
        } => {
            let config = QueryConfig {
                regex_pattern: pattern,
                case_insensitive,
                multiline,
                dot_matches_new_line,
                top_k_tokens,
            };
            let searcher = IndexSearcher::open(index)?;
            for path in searcher.candidate_paths(&config)? {
                println!("{path}");
            }
        }
        Commands::Bench {
            suite_dir,
            filter,
            tokenizer,
            warmup_iter,
            bench_iter,
            top_k_tokens,
            compare_rg,
            rebuild,
            raw_output,
        } => {
            let report = bench::run(&BenchConfig {
                suite_dir,
                filter,
                tokenizer: tokenizer.into(),
                warmup_iter,
                bench_iter,
                top_k_tokens,
                compare_rg,
                rebuild,
                raw_output,
            })?;
            bench::print_report(&report);
        }
    }

    Ok(())
}
