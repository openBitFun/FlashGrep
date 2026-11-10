use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
};

use regex::RegexBuilder;
use walkdir::WalkDir;

use crate::{
    config::{BuildConfig, CorpusMode},
    error::{AppError, Result},
    files::{read_text_file, scan_paths, ScanOptions},
};

use super::{
    cache::{collect_cache_paths_for_target, sanitize_benchmark_name},
    ensure_bench_base_snapshot, BenchConfig, DirtyPattern, DirtyWorktreeFixture,
};

pub(super) fn prepare_dirty_worktree_fixture(
    source_path: &Path,
    suite_dir: &Path,
    fixture_label: &str,
    config: &BenchConfig,
    required_patterns: &[DirtyPattern],
) -> Result<DirtyWorktreeFixture> {
    let fixture_root = suite_dir
        .join(".flashgrep-bench-worktrees")
        .join("worktrees")
        .join(fixture_dir_name(
            fixture_label,
            config.worktree_sample_files,
            config.worktree_dirty_files,
        ));
    if fixture_root.exists() {
        fs::remove_dir_all(&fixture_root)?;
    }
    fs::create_dir_all(&fixture_root)?;

    let (repo_path, dirty_files) = if source_path.is_dir() {
        let target = fixture_root.join("repo");
        let dirty_files = if let Some(limit) = config.worktree_sample_files {
            copy_tree_sampled(source_path, &target, config, required_patterns, limit)?
        } else {
            copy_tree(source_path, &target)?;
            select_dirty_text_files(
                &target,
                &target.join("index"),
                config,
                required_patterns,
                config.worktree_dirty_files,
            )?
        };
        (target, dirty_files)
    } else {
        if config.worktree_dirty_files > 1 {
            return Err(AppError::InvalidIndex(format!(
                "single-file worktree benchmark corpus cannot materialize {} dirty files",
                config.worktree_dirty_files
            )));
        }
        let file_name = source_path.file_name().ok_or_else(|| {
            AppError::InvalidIndex("benchmark file corpus is missing a name".into())
        })?;
        let target = fixture_root.join(file_name);
        copy_file(source_path, &target)?;
        (target.clone(), vec![target])
    };
    if repo_path.is_dir() {
        ensure_git_repo_with_head(&repo_path)?;
    }
    let storage_root = fixture_root.join("index");
    let build_config = BuildConfig {
        git_mode: Default::default(),
        repo_path: repo_path.clone(),
        storage_root: storage_root.clone(),
        tokenizer: config.tokenizer,
        corpus_mode: config.corpus_mode,
        include_hidden: false,
        max_file_size: 512 * 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let _ = ensure_bench_base_snapshot(&build_config, true)?;

    for dirty_file in &dirty_files {
        let contents = read_text_file(dirty_file)?.ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "dirty worktree benchmark file is no longer text: {}",
                dirty_file.display()
            ))
        })?;
        rewrite_with_fresh_mtime(dirty_file, &contents)?;
    }

    let build_cache_paths = if config.cache_mode.is_cold() {
        Some(collect_cache_paths_for_target(
            &repo_path,
            &storage_root,
            config,
            fixture_label,
        )?)
    } else {
        None
    };

    let cache_paths = if config.cache_mode.is_cold() {
        Some(collect_cache_paths_for_target(
            &repo_path,
            &storage_root,
            config,
            fixture_label,
        )?)
    } else {
        None
    };

    Ok(DirtyWorktreeFixture {
        repo_path,
        storage_root,
        cache_paths,
        build_cache_paths,
    })
}

fn fixture_dir_name(
    fixture_label: &str,
    worktree_sample_files: Option<usize>,
    worktree_dirty_files: usize,
) -> String {
    let base = match worktree_sample_files {
        Some(limit) => format!("{}-sample-{limit}", sanitize_benchmark_name(fixture_label)),
        None => sanitize_benchmark_name(fixture_label),
    };
    format!("{base}-dirty-{worktree_dirty_files}")
}

fn copy_tree_sampled(
    source: &Path,
    target: &Path,
    config: &BenchConfig,
    required_patterns: &[DirtyPattern],
    limit: usize,
) -> Result<Vec<PathBuf>> {
    let selection = select_sampled_text_files(source, config, required_patterns, limit)?;
    for path in &selection.selected_files {
        let relative = path.strip_prefix(source).map_err(|error| {
            AppError::InvalidIndex(format!(
                "failed to compute sampled benchmark path for {}: {error}",
                path.display()
            ))
        })?;
        copy_file(path, &target.join(relative))?;
    }
    selection
        .dirty_files
        .into_iter()
        .map(|dirty_file| {
            let dirty_relative = dirty_file.strip_prefix(source).map_err(|error| {
                AppError::InvalidIndex(format!(
                    "failed to compute dirty benchmark path for {}: {error}",
                    dirty_file.display()
                ))
            })?;
            Ok(target.join(dirty_relative))
        })
        .collect()
}

struct SampledFixtureSelection {
    selected_files: Vec<PathBuf>,
    dirty_files: Vec<PathBuf>,
}

fn select_sampled_text_files(
    repo_path: &Path,
    config: &BenchConfig,
    required_patterns: &[DirtyPattern],
    limit: usize,
) -> Result<SampledFixtureSelection> {
    let mut files = scan_paths(
        &[repo_path.to_path_buf()],
        None,
        ScanOptions {
            respect_ignore: matches!(config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: false,
            max_file_size: 512 * 1024 * 1024,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )?;
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));

    let mut text_files = Vec::new();
    for file in files {
        if read_text_file(&file.path)?.is_some() {
            text_files.push(file.path);
        }
    }
    if text_files.is_empty() {
        return Err(AppError::InvalidIndex(
            "no text file available for worktree benchmark".into(),
        ));
    }

    let required_files = required_match_files(&text_files, required_patterns)?;
    if required_files.len() > limit {
        return Err(AppError::InvalidIndex(format!(
            "worktree sample limit {limit} is too small for {} required benchmark files",
            required_files.len()
        )));
    }

    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for path in &required_files {
        if seen.insert(path.clone()) {
            selected.push(path.clone());
        }
    }
    for path in &text_files {
        if selected.len() >= limit {
            break;
        }
        if seen.insert(path.clone()) {
            selected.push(path.clone());
        }
    }

    let dirty_files = selected
        .iter()
        .take(config.worktree_dirty_files)
        .cloned()
        .collect::<Vec<_>>();
    if dirty_files.len() < config.worktree_dirty_files {
        return Err(AppError::InvalidIndex(format!(
            "failed to materialize {} dirty benchmark files from sampled fixture",
            config.worktree_dirty_files
        )));
    }
    Ok(SampledFixtureSelection {
        selected_files: selected,
        dirty_files,
    })
}

fn required_match_files(
    text_files: &[PathBuf],
    required_patterns: &[DirtyPattern],
) -> Result<Vec<PathBuf>> {
    let mut required = Vec::new();
    let mut seen = BTreeSet::new();
    for pattern in required_patterns {
        if let Some(path) = first_matching_text_file(text_files, pattern)? {
            if seen.insert(path.clone()) {
                required.push(path);
            }
        }
    }
    Ok(required)
}

fn first_matching_text_file(
    text_files: &[PathBuf],
    pattern: &DirtyPattern,
) -> Result<Option<PathBuf>> {
    let regex = RegexBuilder::new(&pattern.regex_pattern)
        .case_insensitive(pattern.case_insensitive)
        .multi_line(false)
        .dot_matches_new_line(false)
        .build()
        .map_err(|error| AppError::InvalidPattern(error.to_string()))?;
    for path in text_files {
        let Some(text) = read_text_file(path)? else {
            continue;
        };
        if regex.is_match(&text) {
            return Ok(Some(path.clone()));
        }
    }
    Ok(None)
}

fn select_dirty_text_files(
    repo_path: &Path,
    storage_root: &Path,
    config: &BenchConfig,
    required_patterns: &[DirtyPattern],
    dirty_file_count: usize,
) -> Result<Vec<PathBuf>> {
    let mut files = scan_paths(
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
    )?;
    files.sort_unstable_by(|left, right| left.path.cmp(&right.path));

    let mut text_files = Vec::new();
    for file in files {
        if read_text_file(&file.path)?.is_some() {
            text_files.push(file.path);
        }
    }
    if text_files.is_empty() {
        return Err(AppError::InvalidIndex(
            "no text file available for worktree benchmark".into(),
        ));
    }

    let required_files = required_match_files(&text_files, required_patterns)?;
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for path in required_files {
        if selected.len() >= dirty_file_count {
            break;
        }
        if seen.insert(path.clone()) {
            selected.push(path);
        }
    }
    for path in text_files {
        if selected.len() >= dirty_file_count {
            break;
        }
        if seen.insert(path.clone()) {
            selected.push(path);
        }
    }
    if selected.len() < dirty_file_count {
        return Err(AppError::InvalidIndex(format!(
            "failed to materialize {dirty_file_count} dirty benchmark files"
        )));
    }
    Ok(selected)
}

pub(super) fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;
    for entry in WalkDir::new(source) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            AppError::InvalidIndex(format!(
                "failed to compute relative benchmark path for {}: {error}",
                entry.path().display()
            ))
        })?;
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)?;
        } else {
            copy_path(entry.path(), &destination)?;
        }
    }
    Ok(())
}

fn copy_path(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let link_target = fs::read_link(source)?;
        let resolved = resolve_link_target(source, &link_target);
        if resolved.is_dir() {
            copy_tree(&resolved, target)?;
        } else {
            copy_file(&resolved, target)?;
        }
        return Ok(());
    }

    if metadata.is_file() {
        return copy_file(source, target);
    }

    Err(AppError::InvalidIndex(format!(
        "the source path is neither a regular file nor a symlink to a regular file: {}",
        source.display()
    )))
}

fn copy_file(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, target)?;
    Ok(())
}

fn resolve_link_target(source: &Path, link_target: &Path) -> PathBuf {
    if link_target.is_absolute() {
        link_target.to_path_buf()
    } else {
        source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(link_target)
    }
}

fn rewrite_with_fresh_mtime(path: &Path, contents: &str) -> Result<()> {
    let before = fs::metadata(path)?.modified()?;
    for _ in 0..20 {
        thread::sleep(std::time::Duration::from_millis(10));
        fs::write(path, contents)?;
        let after = fs::metadata(path)?.modified()?;
        if after > before {
            return Ok(());
        }
    }

    Err(AppError::InvalidIndex(format!(
        "mtime did not advance while rewriting benchmark file {}",
        path.display()
    )))
}

fn ensure_git_repo_with_head(repo_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        run_git(repo_path, &["init", "-q"])?;
        run_git(repo_path, &["config", "user.email", "test@example.com"])?;
        run_git(repo_path, &["config", "user.name", "Test User"])?;
    }

    let has_head = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success();
    if !has_head {
        run_git(repo_path, &["add", "."])?;
        run_git(repo_path, &["commit", "--allow-empty", "-qm", "initial"])?;
    }
    Ok(())
}

fn run_git(repo_path: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(AppError::InvalidIndex(format!(
        "git {} failed for {} with status {}",
        args.join(" "),
        repo_path.display(),
        status
    )))
}
