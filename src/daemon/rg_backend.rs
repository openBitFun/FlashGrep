use std::{
    env,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    thread::JoinHandle,
};

use serde_json::Value;

use crate::{
    config::{BuildConfig, CorpusMode, QueryConfig},
    error::{AppError, Result},
    path_filter::{PathFilter, PathFilterArgs},
    path_utils::normalize_lexical_path,
    search::{FileCount, FileMatchCount, LineMatch, SearchMode, SearchResults},
};

pub(super) fn run_rg_search(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Result<SearchResults> {
    if query.effective_global_max_results() == Some(0) {
        return Ok(empty_search_results());
    }
    match query.search_mode {
        SearchMode::FilesWithMatches => run_rg_files_with_matches(build_config, query, scope),
        SearchMode::LineMatches => run_rg_line_matches(build_config, query, scope),
        SearchMode::CountOnly => run_rg_count_search(build_config, query, scope, false),
        SearchMode::CountMatches => run_rg_count_search(build_config, query, scope, true),
    }
}

pub(super) fn run_rg_glob(
    build_config: &BuildConfig,
    scope: &PathFilterArgs,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    let mut command = build_rg_files_command(build_config);
    command.arg("--files");
    append_rg_scope_roots(&mut command, build_config, scope);
    let command_debug = format!("{command:?}");

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::Protocol(format!(
            "rg glob failed with status {} (command: {}): {}",
            output.status,
            command_debug,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let mut paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let candidate = Path::new(line);
            let absolute = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                build_config.repo_path.join(candidate)
            };
            let absolute = normalize_lexical_path(&absolute);
            if filter.is_some_and(|active| !active.matches_file(&absolute)) {
                return None;
            }
            let metadata = std::fs::metadata(&absolute).ok()?;
            if !metadata.is_file() || metadata.len() > build_config.max_file_size {
                return None;
            }
            Some(absolute.to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    paths.sort_unstable();
    Ok(paths)
}

fn run_rg_files_with_matches(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Result<SearchResults> {
    let counts = run_rg_count_search(build_config, query, scope, false)?;
    Ok(SearchResults {
        matched_paths: counts
            .file_counts
            .iter()
            .map(|count| count.path.clone())
            .collect(),
        file_counts: Vec::new(),
        ..counts
    })
}

fn run_rg_count_search(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
    count_matches: bool,
) -> Result<SearchResults> {
    let (matched_lines, searches_with_match, file_counts) =
        run_rg_count_command(build_config, query, scope, false)?;
    let (matched_occurrences, file_match_counts) = if count_matches {
        let (matched_occurrences, _, file_counts) =
            run_rg_count_command(build_config, query, scope, true)?;
        (
            matched_occurrences,
            file_counts
                .into_iter()
                .map(|count| FileMatchCount {
                    path: count.path,
                    matched_occurrences: count.matched_lines,
                })
                .collect(),
        )
    } else {
        (0, Vec::new())
    };

    Ok(SearchResults {
        candidate_docs: searches_with_match,
        searches_with_match,
        bytes_searched: 0,
        matched_lines,
        matched_occurrences,
        matched_paths: Vec::new(),
        line_matches: Vec::new(),
        line_match_files: Vec::new(),
        file_counts,
        file_match_counts,
    })
}

fn run_rg_line_matches(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Result<SearchResults> {
    let mut command = build_rg_base_command(build_config, query, scope);
    command.arg("--json");
    if let Some(max_count) = query.max_count {
        command.arg("-m").arg(max_count.to_string());
    }
    append_rg_scope_roots(&mut command, build_config, scope);
    let command_debug = format!("{command:?}");
    let mut process = StreamingRgProcess::spawn(&mut command)?;
    let max_results = query.effective_global_max_results().unwrap_or(usize::MAX);
    let mut line_matches = Vec::<LineMatch>::new();
    let mut matched_occurrences = 0usize;
    let mut stopped_early = false;

    for line in process.stdout_mut().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&line).map_err(|error| {
            AppError::Protocol(format!("failed to decode rg json event: {error}"))
        })?;
        if event.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let data = event.get("data").unwrap_or(&Value::Null);
        let Some(path) = rg_text_field(data.get("path")) else {
            continue;
        };
        let Some(line_number) = rg_line_number(data) else {
            continue;
        };
        let path = normalize_lexical_path(&build_config.repo_path.join(path))
            .to_string_lossy()
            .into_owned();
        line_matches.push(LineMatch { path, line_number });
        matched_occurrences += data
            .get("submatches")
            .and_then(Value::as_array)
            .map_or(1, Vec::len);
        if line_matches.len() >= max_results {
            stopped_early = true;
            break;
        }
    }
    process.finish(stopped_early, &command_debug)?;

    let searches_with_match = line_matches
        .iter()
        .map(|matched| matched.path.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Ok(SearchResults {
        candidate_docs: searches_with_match,
        searches_with_match,
        bytes_searched: 0,
        matched_lines: line_matches.len(),
        matched_occurrences,
        matched_paths: Vec::new(),
        line_matches,
        line_match_files: Vec::new(),
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
    })
}

fn run_rg_count_command(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
    count_matches: bool,
) -> Result<(usize, usize, Vec<FileCount>)> {
    let mut command = build_rg_base_command(build_config, query, scope);
    if count_matches {
        command.arg("--count-matches");
    } else {
        command.arg("--count");
    }
    if let Some(max_count) = query.max_count {
        command.arg("-m").arg(max_count.to_string());
    }
    append_rg_scope_roots(&mut command, build_config, scope);
    let command_debug = format!("{command:?}");
    let mut process = StreamingRgProcess::spawn(&mut command)?;
    let max_results = query.effective_global_max_results().unwrap_or(usize::MAX);
    let mut matched_total = 0usize;
    let mut searches_with_match = 0usize;
    let mut file_counts = Vec::new();
    let mut stopped_early = false;
    for line in process.stdout_mut().lines() {
        let line = line?;
        let Some((path, count)) = line.rsplit_once(':') else {
            continue;
        };
        let count = count.parse::<usize>().map_err(|error| {
            AppError::Protocol(format!("failed to parse rg count output {line:?}: {error}"))
        })?;
        if count == 0 {
            continue;
        }
        if file_counts.len() >= max_results {
            break;
        }
        matched_total += count;
        searches_with_match += 1;
        file_counts.push(FileCount {
            path: normalize_lexical_path(&build_config.repo_path.join(path))
                .to_string_lossy()
                .into_owned(),
            matched_lines: count,
        });
        if file_counts.len() >= max_results {
            stopped_early = true;
            break;
        }
    }
    process.finish(stopped_early, &command_debug)?;
    Ok((matched_total, searches_with_match, file_counts))
}

struct StreamingRgProcess {
    child: Option<Child>,
    stdout: BufReader<ChildStdout>,
    stderr_handle: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
}

impl StreamingRgProcess {
    fn spawn(command: &mut Command) -> Result<Self> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Protocol("rg fallback stdout pipe is missing".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::Protocol("rg fallback stderr pipe is missing".into()))?;
        let stderr_handle = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes)?;
            Ok(bytes)
        });
        Ok(Self {
            child: Some(child),
            stdout: BufReader::new(stdout),
            stderr_handle: Some(stderr_handle),
        })
    }

    fn stdout_mut(&mut self) -> &mut BufReader<ChildStdout> {
        &mut self.stdout
    }

    fn finish(&mut self, stopped_early: bool, command_debug: &str) -> Result<()> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| AppError::Protocol("rg fallback process already finished".into()))?;
        if stopped_early {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let stderr = self.join_stderr()?;
        if !stopped_early && !status.success() && status.code() != Some(1) {
            return Err(AppError::Protocol(format!(
                "rg fallback failed with status {status} (command: {command_debug}): {}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        Ok(())
    }

    fn join_stderr(&mut self) -> Result<Vec<u8>> {
        self.stderr_handle
            .take()
            .ok_or_else(|| AppError::Protocol("rg fallback stderr reader is missing".into()))?
            .join()
            .map_err(|_| AppError::Protocol("rg fallback stderr reader panicked".into()))?
            .map_err(AppError::from)
    }
}

impl Drop for StreamingRgProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }
}

fn empty_search_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        matched_paths: Vec::new(),
        line_matches: Vec::new(),
        line_match_files: Vec::new(),
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
    }
}

fn build_rg_base_command(
    build_config: &BuildConfig,
    query: &QueryConfig,
    scope: &PathFilterArgs,
) -> Command {
    let mut command = build_rg_scope_command(build_config, scope);
    command.arg("--with-filename");

    if query.case_insensitive {
        command.arg("--ignore-case");
    }
    if query.multiline {
        command.arg("--multiline");
    }
    if query.dot_matches_new_line {
        command.arg("--multiline-dotall");
    }
    if query.fixed_strings {
        command.arg("--fixed-strings");
        for pattern in &query.patterns {
            command.arg("-e").arg(pattern);
        }
    } else {
        if query.word_regexp {
            command.arg("--word-regexp");
        }
        if query.line_regexp {
            command.arg("--line-regexp");
        }
        command.arg("-e").arg(&query.regex_pattern);
    }

    command
}

fn build_rg_scope_command(build_config: &BuildConfig, scope: &PathFilterArgs) -> Command {
    let mut command = Command::new("rg");
    command
        .current_dir(&build_config.repo_path)
        .arg("--color")
        .arg("never")
        .arg("--max-filesize")
        .arg(build_config.max_file_size.to_string());
    if !rg_diagnostics_enabled() {
        command.arg("--no-messages");
    }
    if matches!(build_config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
    if build_config.include_hidden {
        command.arg("--hidden");
    }

    for glob in &scope.globs {
        command.arg("-g").arg(glob);
    }
    for glob in &scope.iglobs {
        command.arg("--iglob").arg(glob);
    }
    for spec in &scope.type_add {
        command.arg("--type-add").arg(spec);
    }
    for name in &scope.type_clear {
        command.arg("--type-clear").arg(name);
    }
    for name in &scope.types {
        command.arg("-t").arg(name);
    }
    for name in &scope.type_not {
        command.arg("-T").arg(name);
    }
    if let Some(exclude_glob) =
        rg_storage_root_exclude_glob(&build_config.repo_path, &build_config.storage_root)
    {
        command.arg("-g").arg(format!("!{exclude_glob}"));
    }
    command
}

fn build_rg_files_command(build_config: &BuildConfig) -> Command {
    let mut command = Command::new("rg");
    command
        .current_dir(&build_config.repo_path)
        .arg("--color")
        .arg("never");
    if !rg_diagnostics_enabled() {
        command.arg("--no-messages");
    }
    if matches!(build_config.corpus_mode, CorpusMode::NoIgnore) {
        command.arg("--no-ignore");
    }
    if build_config.include_hidden {
        command.arg("--hidden");
    }
    if let Some(exclude_glob) =
        rg_storage_root_exclude_glob(&build_config.repo_path, &build_config.storage_root)
    {
        command.arg("-g").arg(format!("!{exclude_glob}"));
    }
    command
}

fn rg_diagnostics_enabled() -> bool {
    env::var_os("FLASHGREP_LOG_TRANSITION_FALLBACKS").is_some()
}

fn append_rg_scope_roots(
    command: &mut Command,
    build_config: &BuildConfig,
    scope: &PathFilterArgs,
) {
    command.arg("--");
    let roots = rg_scope_roots(&build_config.repo_path, &scope.roots);
    if roots.is_empty() {
        command.arg(".");
    } else {
        for root in roots {
            command.arg(root);
        }
    }
}

fn rg_scope_roots(repo_root: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    if roots.is_empty() {
        return Vec::new();
    }

    roots
        .iter()
        .map(|root| {
            if root.is_absolute() {
                if let Ok(relative) = root.strip_prefix(repo_root) {
                    if relative.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        relative.to_path_buf()
                    }
                } else {
                    root.clone()
                }
            } else if root.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                root.clone()
            }
        })
        .collect()
}

fn rg_storage_root_exclude_glob(repo_root: &Path, storage_root: &Path) -> Option<String> {
    let relative = storage_root.strip_prefix(repo_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    Some(format!(
        "{}/**",
        relative.to_string_lossy().replace('\\', "/")
    ))
}

fn rg_text_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|field| field.get("text"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn rg_line_number(data: &Value) -> Option<usize> {
    data.get("line_number")
        .and_then(Value::as_u64)
        .and_then(|line| usize::try_from(line).ok())
}

#[cfg(test)]
mod tests {
    use super::StreamingRgProcess;
    use std::io::BufRead;
    use std::process::Command;

    #[cfg(unix)]
    #[test]
    fn streaming_rg_process_can_stop_after_reaching_a_result_limit() {
        let mut command = Command::new("sh");
        command.args(["-c", "while :; do printf 'match\\n'; done"]);
        let mut process = StreamingRgProcess::spawn(&mut command).expect("process should spawn");
        let mut line = String::new();
        process
            .stdout_mut()
            .read_line(&mut line)
            .expect("line should read");
        assert_eq!(line, "match\n");
        process
            .finish(true, "test streaming command")
            .expect("early stop should succeed");
    }
}
