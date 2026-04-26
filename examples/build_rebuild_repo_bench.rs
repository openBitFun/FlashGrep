use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};

use codgrep::{BuildConfig, CorpusMode, Result, SearchEngine, TokenizerMode};

const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;
const DEFAULT_REPOS: [&str; 4] = [
    "/Users/user/workspace/blue/BitFun",
    "/Users/user/workspace/ClickHouse",
    "/Users/user/workspace/llvm-project",
    "/Users/user/workspace/chromium-src",
];

fn main() -> Result<()> {
    let repos = if env::args().len() > 1 {
        env::args().skip(1).map(PathBuf::from).collect::<Vec<_>>()
    } else {
        DEFAULT_REPOS.iter().map(PathBuf::from).collect::<Vec<_>>()
    };

    let bench_root = env::var("CODGREP_BUILD_BENCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("tmp/build-rebuild-bench"));
    fs::create_dir_all(&bench_root)?;

    println!(
        "repo_name,repo_path,build_docs,build_secs,build_storage_bytes,rebuild_docs,rebuild_secs,rebuild_storage_bytes"
    );

    for repo in repos {
        let repo = fs::canonicalize(repo)?;
        let repo_name = repo
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repo")
            .replace(',', "_");
        let storage_root = bench_root.join(&repo_name);
        if storage_root.exists() {
            fs::remove_dir_all(&storage_root)?;
        }

        let build_config = BuildConfig {
            repo_path: repo.clone(),
            storage_root: storage_root.clone(),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::RespectIgnore,
            include_hidden: false,
            max_file_size: MAX_FILE_SIZE,
            min_sparse_len: 3,
            max_sparse_len: 8,
        };

        let engine = SearchEngine::new(build_config);

        let build_started = Instant::now();
        let build_docs = engine.build_base_snapshot()?;
        let build_secs = build_started.elapsed().as_secs_f64();
        let build_storage_bytes = dir_size_bytes(&storage_root)?;

        let rebuild_started = Instant::now();
        let rebuild_docs = engine.rebuild_base_snapshot()?;
        let rebuild_secs = rebuild_started.elapsed().as_secs_f64();
        let rebuild_storage_bytes = dir_size_bytes(&storage_root)?;

        println!(
            "{repo_name},{},{build_docs},{build_secs:.3},{build_storage_bytes},{rebuild_docs},{rebuild_secs:.3},{rebuild_storage_bytes}",
            repo.display()
        );
    }

    Ok(())
}

fn dir_size_bytes(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}
