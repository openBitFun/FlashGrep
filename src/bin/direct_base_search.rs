use std::{env, path::PathBuf, time::Instant};

use codgrep::{
    index_engine::{query::QueryFilters, query::QueryRequest, WorkspaceBaseSnapshotIndex},
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
    let mut mode = SearchMode::CountOnly;
    let mut repeat = 1usize;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo_path = args.next().map(PathBuf::from),
            "--storage-root" => storage_root = args.next().map(PathBuf::from),
            "--pattern" => pattern = args.next(),
            "--repeat" => {
                let value = args.next().ok_or_else(|| {
                    codgrep::AppError::Protocol(
                        "usage: direct_base_search --repo <repo> --pattern <pattern> [--storage-root <root>] [--mode count|count-matches|first|materialize] [--repeat <n>] [-i]".into(),
                    )
                })?;
                repeat = value.parse::<usize>().map_err(|error| {
                    codgrep::AppError::Protocol(format!("invalid --repeat value {value}: {error}"))
                })?;
                if repeat == 0 {
                    return Err(codgrep::AppError::Protocol(
                        "--repeat must be greater than 0".into(),
                    ));
                }
            }
            "--mode" => {
                let value = args.next().ok_or_else(|| {
                    codgrep::AppError::Protocol(
                        "usage: direct_base_search --repo <repo> --pattern <pattern> [--storage-root <root>] [--mode count|count-matches|first|materialize] [--repeat <n>] [-i]".into(),
                    )
                })?;
                mode = match value.as_str() {
                    "count" => SearchMode::CountOnly,
                    "count-matches" => SearchMode::CountMatches,
                    "first" => SearchMode::FirstHitOnly,
                    "materialize" => SearchMode::MaterializeMatches,
                    other => {
                        return Err(codgrep::AppError::Protocol(format!(
                            "unknown mode: {other}"
                        )))
                    }
                };
            }
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
        codgrep::AppError::Protocol("usage: direct_base_search --repo <repo> --pattern <pattern> [--storage-root <root>] [--mode count|count-matches|first|materialize] [--repeat <n>] [-i]".into())
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
    let snapshot = WorkspaceBaseSnapshotIndex::open_read_only(&build_config)?.open_committed()?;
    let query = QueryRequest {
        pattern: pattern.clone(),
        mode,
        case_insensitive: ignore_case,
        top_k: None,
        filters: QueryFilters::default(),
    };
    let mut last_results = None;
    for iteration in 1..=repeat {
        let started = Instant::now();
        let results = snapshot.search(&query)?;
        let elapsed = started.elapsed().as_secs_f64();
        println!("iteration={iteration}");
        println!("elapsed_secs={elapsed:.6}");
        println!("candidate_docs={}", results.candidate_docs);
        println!("searches_with_match={}", results.searches_with_match);
        println!("matched_lines={}", results.matched_lines);
        println!("matched_occurrences={}", results.matched_occurrences);
        println!("hits={}", results.hits.len());
        last_results = Some(results);
    }
    println!("pattern={pattern}");
    println!("repeat={repeat}");
    if let Some(results) = last_results {
        println!("final_candidate_docs={}", results.candidate_docs);
        println!("final_searches_with_match={}", results.searches_with_match);
        println!("final_matched_lines={}", results.matched_lines);
        println!("final_matched_occurrences={}", results.matched_occurrences);
        println!("final_hits={}", results.hits.len());
    }
    Ok(())
}
