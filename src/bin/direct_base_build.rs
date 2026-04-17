use std::{env, path::PathBuf, time::Instant};

use codgrep::{BuildConfig, CorpusMode, SearchEngine, TokenizerMode};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> codgrep::Result<()> {
    let mut args = env::args_os();
    let _program = args.next();
    let repo_path = PathBuf::from(args.next().ok_or_else(|| {
        codgrep::AppError::Protocol(
            "usage: direct_base_build <repo_path> [storage_root]".to_string(),
        )
    })?);
    let storage_root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_path.join(".codgrep-index-engine"));

    let engine = SearchEngine::new(BuildConfig {
        repo_path,
        storage_root,
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size: 2 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    });

    let started = Instant::now();
    let indexed_docs = engine.rebuild_base_snapshot()?;
    println!(
        "rebuilt base snapshot with {indexed_docs} docs in {:.2?}",
        started.elapsed()
    );
    Ok(())
}
