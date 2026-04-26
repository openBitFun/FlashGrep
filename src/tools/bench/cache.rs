use std::{
    path::{Path, PathBuf},
    process::Command,
};

use walkdir::WalkDir;

use crate::{
    config::CorpusMode,
    error::{AppError, Result},
    files::{scan_paths, ScanOptions},
    index_engine::WorkspaceBaseSnapshotIndex,
};

use super::{bench_build_config, BenchCachePaths, BenchConfig, CacheRunner, Corpus};

pub(super) fn collect_cache_paths(
    corpus: Corpus,
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
) -> Result<BenchCachePaths> {
    collect_cache_paths_for_target(repo_path, storage_root, config, corpus.label())
}

pub(super) fn collect_cache_paths_for_target(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
    label: &str,
) -> Result<BenchCachePaths> {
    let corpus_files = scan_paths(
        &[repo_path.to_path_buf()],
        Some(storage_root),
        ScanOptions {
            respect_ignore: matches!(config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )?
    .into_iter()
    .map(|file| file.path)
    .collect::<Vec<_>>();
    if corpus_files.is_empty() {
        return Err(AppError::InvalidIndex(format!(
            "no benchmark files found for {}",
            label
        )));
    }

    let build_config = bench_build_config(repo_path, storage_root, config);
    let mut index_files = runtime_index_files(storage_root);
    let base_snapshot_root = WorkspaceBaseSnapshotIndex::index_root(&build_config);
    index_files.extend(runtime_index_files(&base_snapshot_root));
    index_files.sort();
    index_files.dedup();
    if index_files.is_empty() {
        return Err(AppError::InvalidIndex(format!(
            "no bench runtime index files found under {} or {}",
            storage_root.display(),
            base_snapshot_root.display()
        )));
    }

    Ok(BenchCachePaths {
        corpus_files,
        index_files,
    })
}

fn runtime_index_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect()
}

pub(super) fn sanitize_benchmark_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '-',
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "custom".into()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn prepare_sample(
    runner: CacheRunner,
    cache_paths: Option<&BenchCachePaths>,
    config: &BenchConfig,
) -> Result<()> {
    if !config.cache_mode.is_cold() {
        return Ok(());
    }
    let cache_paths = cache_paths
        .ok_or_else(|| AppError::InvalidIndex("missing cold benchmark cache paths".into()))?;

    match runner {
        CacheRunner::Bitfun => evict_file_cache(
            cache_paths
                .index_files
                .iter()
                .chain(cache_paths.corpus_files.iter()),
        )?,
        CacheRunner::Rg => evict_file_cache(cache_paths.corpus_files.iter())?,
    }

    if let Some(hook) = &config.cold_hook {
        run_cold_hook(hook)?;
    }

    Ok(())
}

fn evict_file_cache<'a, I>(paths: I) -> Result<()>
where
    I: IntoIterator<Item = &'a PathBuf>,
{
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        for path in paths {
            let file = std::fs::File::open(path)?;
            let status =
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
            if status != 0 {
                return Err(AppError::InvalidIndex(format!(
                    "failed to evict page cache for {}: {}",
                    path.display(),
                    std::io::Error::from_raw_os_error(status)
                )));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = paths;
        Err(AppError::InvalidIndex(
            "cold benchmark mode is only supported on Linux".into(),
        ))
    }
}

fn run_cold_hook(command: &str) -> Result<()> {
    #[cfg(target_family = "unix")]
    let mut process = {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    #[cfg(target_family = "windows")]
    let mut process = {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    };

    let status = process.status()?;
    if !status.success() {
        return Err(AppError::InvalidIndex(format!(
            "cold hook failed with status {status}"
        )));
    }
    Ok(())
}
