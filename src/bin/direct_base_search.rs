use std::{env, path::PathBuf, time::Instant};

use codgrep::{
    index_engine::{query::QueryFilters, query::QueryRequest, EngineConfig, IndexEngine, ShardId},
    BuildConfig, CorpusMode, SearchMode, TokenizerMode,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> codgrep::Result<()> {
    let mut repo_path: Option<PathBuf> = None;
    let mut storage_root: Option<PathBuf> = None;
    let mut pattern: Option<String> = None;
    let mut ignore_case = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo_path = args.next().map(PathBuf::from),
            "--storage-root" => storage_root = args.next().map(PathBuf::from),
            "--pattern" => pattern = args.next(),
            "-i" | "--ignore-case" => ignore_case = true,
            other => {
                return Err(codgrep::AppError::Protocol(format!(
                    "unknown argument: {other}"
                )));
            }
        }
    }

    let repo_path = repo_path.ok_or_else(|| {
        codgrep::AppError::Protocol("usage: direct_base_search --repo <repo> --pattern <pattern> [--storage-root <root>] [-i]".into())
    })?;
    let storage_root = storage_root.unwrap_or_else(|| repo_path.join(".codgrep-index-engine"));
    let pattern = pattern.ok_or_else(|| {
        codgrep::AppError::Protocol("usage: direct_base_search --repo <repo> --pattern <pattern> [--storage-root <root>] [-i]".into())
    })?;

    let build_config = BuildConfig {
        repo_path,
        storage_root,
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size: 2 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let engine = IndexEngine::open(EngineConfig {
        shard_id: ShardId(format!(
            "{}#base-snapshot",
            build_config.repo_path.to_string_lossy()
        )),
        index_root: build_config.storage_root.join("base-snapshot"),
        tokenizer: (&build_config).into(),
        ..EngineConfig::default()
    })?;
    let snapshot = engine.reader().open_committed()?;
    let started = Instant::now();
    let results = snapshot.search(&QueryRequest {
        pattern: pattern.clone(),
        mode: SearchMode::CountOnly,
        case_insensitive: ignore_case,
        top_k: None,
        filters: QueryFilters::default(),
    })?;
    let elapsed = started.elapsed().as_secs_f64();

    println!("pattern={pattern}");
    println!("elapsed_secs={elapsed:.6}");
    println!("candidate_docs={}", results.candidate_docs);
    println!("searches_with_match={}", results.searches_with_match);
    println!("matched_lines={}", results.matched_lines);
    println!("matched_occurrences={}", results.matched_occurrences);
    Ok(())
}
