use std::{
    collections::HashSet,
    env, io,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    files::{resolve_repo_path, scan_paths, RepositoryFile, ScanOptions},
    index::IndexSearcher,
    path_filter::PathFilter,
    query_preflight::{evaluate_index_query_preflight, preflight_enabled},
    search::{FileCount, FileMatchCount, SearchResults},
    search_engine::{
        count_scanned_files, count_total_scanned_files, search_json_scanned_files,
        search_scanned_files,
    },
};

use super::request::ripgrep_is_available;
use super::SearchRequest;

pub(super) enum SearchExecution {
    Indexed {
        searcher: IndexSearcher,
        allowed_paths: Option<HashSet<String>>,
        paths: Vec<String>,
        path_count: usize,
    },
    Ripgrep {
        plan: RipgrepPlan,
        paths: Vec<String>,
    },
    Scan {
        files: Vec<RepositoryFile>,
    },
}

#[derive(Debug, Clone)]
pub(super) struct RipgrepPlan {
    roots: Vec<PathBuf>,
    include_hidden: bool,
    no_ignore: bool,
    globs: Vec<String>,
    iglobs: Vec<String>,
    type_add: Vec<String>,
    type_clear: Vec<String>,
    types: Vec<String>,
    type_not: Vec<String>,
    ignore_files: Vec<PathBuf>,
    max_depth: Option<usize>,
    max_file_size: Option<u64>,
}

impl SearchExecution {
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::Indexed { path_count, .. } => *path_count == 0,
            Self::Ripgrep { paths, .. } => paths.is_empty(),
            Self::Scan { files } => files.is_empty(),
        }
    }

    pub(super) fn paths(&self) -> Vec<String> {
        match self {
            Self::Indexed { paths, .. } => paths.clone(),
            Self::Ripgrep { paths, .. } => paths.clone(),
            Self::Scan { files } => files
                .iter()
                .map(|file| file.path.to_string_lossy().into_owned())
                .collect(),
        }
    }
}

pub(super) fn execute_search(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<SearchResults> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => searcher.search_with_allowed_paths(config, filter, allowed_paths.as_ref()),
        SearchExecution::Ripgrep { .. } => Err(AppError::InvalidIndex(
            "ripgrep-backed fallback only supports count-style queries".into(),
        )),
        SearchExecution::Scan { files } => search_scanned_files(config, files),
    }
}

pub(super) fn execute_has_match(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<bool> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => Ok(searcher
            .search_with_allowed_paths(config, filter, allowed_paths.as_ref())?
            .has_match()),
        SearchExecution::Ripgrep { plan, .. } => run_ripgrep_has_match(plan, config),
        SearchExecution::Scan { files } => Ok(search_scanned_files(config, files)?.has_match()),
    }
}

pub(super) fn execute_search_json(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<crate::json_output::JsonSearchReport> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => searcher.search_json_with_allowed_paths(config, filter, allowed_paths.as_ref()),
        SearchExecution::Ripgrep { .. } => Err(AppError::InvalidIndex(
            "ripgrep-backed fallback does not support json output".into(),
        )),
        SearchExecution::Scan { files } => search_json_scanned_files(config, files),
    }
}

pub(super) fn execute_count_matches(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
    include_zero: bool,
) -> Result<Vec<FileCount>> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => {
            if include_zero {
                searcher.count_matches_by_file_including_zero_with_allowed_paths(
                    config,
                    filter,
                    allowed_paths.as_ref(),
                )
            } else {
                searcher.count_matches_by_file_with_allowed_paths(
                    config,
                    filter,
                    allowed_paths.as_ref(),
                )
            }
        }
        SearchExecution::Ripgrep { plan, .. } => {
            if include_zero {
                return Err(AppError::InvalidIndex(
                    "ripgrep-backed fallback does not support --include-zero".into(),
                ));
            }
            run_ripgrep_file_counts(plan, config, RipgrepCountKind::Lines)
        }
        SearchExecution::Scan { files } => count_scanned_files(config, files, include_zero),
    }
}

pub(super) fn execute_count_total_matches(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
    include_zero: bool,
) -> Result<Vec<FileMatchCount>> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => {
            if include_zero {
                searcher.count_total_matches_by_file_including_zero_with_allowed_paths(
                    config,
                    filter,
                    allowed_paths.as_ref(),
                )
            } else {
                searcher.count_total_matches_by_file_with_allowed_paths(
                    config,
                    filter,
                    allowed_paths.as_ref(),
                )
            }
        }
        SearchExecution::Ripgrep { plan, .. } => {
            if include_zero {
                return Err(AppError::InvalidIndex(
                    "ripgrep-backed fallback does not support --include-zero".into(),
                ));
            }
            run_ripgrep_file_match_counts(plan, config)
        }
        SearchExecution::Scan { files } => count_total_scanned_files(config, files, include_zero),
    }
}

pub(super) fn execute_files_with_matches(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => searcher.files_with_matches_and_allowed_paths(config, filter, allowed_paths.as_ref()),
        SearchExecution::Ripgrep { plan, .. } => {
            run_ripgrep_paths(plan, config, RipgrepPathKind::WithMatches)
        }
        SearchExecution::Scan { files } => Ok(count_scanned_files(config, files, false)?
            .into_iter()
            .map(|count| count.path)
            .collect()),
    }
}

pub(super) fn execute_files_without_matches(
    execution: &SearchExecution,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<Vec<String>> {
    match execution {
        SearchExecution::Indexed {
            searcher,
            allowed_paths,
            ..
        } => {
            searcher.files_without_matches_and_allowed_paths(config, filter, allowed_paths.as_ref())
        }
        SearchExecution::Ripgrep { plan, .. } => {
            run_ripgrep_paths(plan, config, RipgrepPathKind::WithoutMatch)
        }
        SearchExecution::Scan { files } => Ok(count_scanned_files(config, files, true)?
            .into_iter()
            .filter(|count| !count.has_match())
            .map(|count| count.path)
            .collect()),
    }
}

#[derive(Debug, Clone, Copy)]
enum RipgrepCountKind {
    Lines,
    Occurrences,
}

#[derive(Debug, Clone, Copy)]
enum RipgrepPathKind {
    WithMatches,
    WithoutMatch,
}

fn build_ripgrep_command(plan: &RipgrepPlan, config: &QueryConfig) -> Command {
    let mut command = Command::new("rg");
    command.arg("--color=never");
    if config.fixed_strings {
        command.arg("-F");
    }
    if config.case_insensitive {
        command.arg("-i");
    }
    if config.word_regexp {
        command.arg("-w");
    }
    if config.line_regexp {
        command.arg("-x");
    }
    if plan.include_hidden {
        command.arg("--hidden");
    }
    if plan.no_ignore {
        command.arg("--no-ignore");
    }
    if let Some(max_depth) = plan.max_depth {
        command.arg("--max-depth").arg(max_depth.to_string());
    }
    if let Some(max_file_size) = plan.max_file_size {
        command.arg("--max-filesize").arg(max_file_size.to_string());
    }
    for glob in &plan.globs {
        command.arg("-g").arg(glob);
    }
    for iglob in &plan.iglobs {
        command.arg("--iglob").arg(iglob);
    }
    for definition in &plan.type_add {
        command.arg("--type-add").arg(definition);
    }
    for name in &plan.type_clear {
        command.arg("--type-clear").arg(name);
    }
    for name in &plan.types {
        command.arg("-t").arg(name);
    }
    for name in &plan.type_not {
        command.arg("-T").arg(name);
    }
    for ignore_file in &plan.ignore_files {
        command.arg("--ignore-file").arg(ignore_file);
    }
    command
}

fn append_ripgrep_query_and_roots(command: &mut Command, plan: &RipgrepPlan, config: &QueryConfig) {
    if config.patterns.is_empty() {
        command.arg("-e").arg(&config.regex_pattern);
    } else {
        for pattern in &config.patterns {
            command.arg("-e").arg(pattern);
        }
    }
    for root in &plan.roots {
        command.arg(root);
    }
}

fn run_ripgrep_has_match(plan: &RipgrepPlan, config: &QueryConfig) -> Result<bool> {
    let mut command = build_ripgrep_command(plan, config);
    command.arg("-q");
    append_ripgrep_query_and_roots(&mut command, plan, config);
    let status = command.status()?;
    if status.success() {
        Ok(true)
    } else if status.code() == Some(1) {
        Ok(false)
    } else {
        Err(AppError::InvalidIndex(format!(
            "rg fallback failed with status {status}"
        )))
    }
}

fn run_ripgrep_file_counts(
    plan: &RipgrepPlan,
    config: &QueryConfig,
    kind: RipgrepCountKind,
) -> Result<Vec<FileCount>> {
    run_ripgrep_counts(plan, config, kind)?
        .into_iter()
        .map(|(path, count)| {
            Ok(FileCount {
                path,
                matched_lines: count,
            })
        })
        .collect()
}

fn run_ripgrep_file_match_counts(
    plan: &RipgrepPlan,
    config: &QueryConfig,
) -> Result<Vec<FileMatchCount>> {
    run_ripgrep_counts(plan, config, RipgrepCountKind::Occurrences)?
        .into_iter()
        .map(|(path, count)| {
            Ok(FileMatchCount {
                path,
                matched_occurrences: count,
            })
        })
        .collect()
}

fn run_ripgrep_counts(
    plan: &RipgrepPlan,
    config: &QueryConfig,
    kind: RipgrepCountKind,
) -> Result<Vec<(String, usize)>> {
    let mut command = build_ripgrep_command(plan, config);
    command.arg(match kind {
        RipgrepCountKind::Lines => "--count",
        RipgrepCountKind::Occurrences => "--count-matches",
    });
    command.arg("--with-filename");
    append_ripgrep_query_and_roots(&mut command, plan, config);

    let output = command.output()?;
    if output.status.success() {
        parse_ripgrep_count_output(&output.stdout)
    } else if output.status.code() == Some(1) {
        Ok(Vec::new())
    } else {
        Err(AppError::InvalidIndex(format!(
            "rg fallback failed with status {}",
            output.status
        )))
    }
}

fn parse_ripgrep_count_output(stdout: &[u8]) -> Result<Vec<(String, usize)>> {
    let mut counts = Vec::new();
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let line = std::str::from_utf8(line).map_err(|error| {
            AppError::InvalidIndex(format!("rg count output was not utf-8: {error}"))
        })?;
        let Some((path, count)) = line.rsplit_once(':') else {
            return Err(AppError::InvalidIndex(format!(
                "failed to parse rg count output '{line}'"
            )));
        };
        let count = count.parse::<usize>().map_err(|error| {
            AppError::InvalidIndex(format!("failed to parse rg count output '{line}': {error}"))
        })?;
        counts.push((path.to_string(), count));
    }
    Ok(counts)
}

fn run_ripgrep_paths(
    plan: &RipgrepPlan,
    config: &QueryConfig,
    kind: RipgrepPathKind,
) -> Result<Vec<String>> {
    let mut command = build_ripgrep_command(plan, config);
    command.arg(match kind {
        RipgrepPathKind::WithMatches => "--files-with-matches",
        RipgrepPathKind::WithoutMatch => "--files-without-match",
    });
    append_ripgrep_query_and_roots(&mut command, plan, config);

    let output = command.output()?;
    if output.status.success() {
        parse_ripgrep_path_output(&output.stdout)
    } else if output.status.code() == Some(1) {
        Ok(Vec::new())
    } else {
        Err(AppError::InvalidIndex(format!(
            "rg fallback failed with status {}",
            output.status
        )))
    }
}

fn parse_ripgrep_path_output(stdout: &[u8]) -> Result<Vec<String>> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            std::str::from_utf8(line)
                .map(str::to_string)
                .map_err(|error| {
                    AppError::InvalidIndex(format!("rg path output was not utf-8: {error}"))
                })
        })
        .collect()
}

impl SearchRequest {
    pub(super) fn filter(&self) -> Option<&crate::path_filter::PathFilter> {
        Some(&self.path_filter)
    }

    fn ripgrep_fallback_plan(&self) -> RipgrepPlan {
        RipgrepPlan {
            roots: self.search_paths.clone(),
            include_hidden: self.include_hidden,
            no_ignore: self.no_ignore,
            globs: self.globs.clone(),
            iglobs: self.iglobs.clone(),
            type_add: self.type_add.clone(),
            type_clear: self.type_clear.clone(),
            types: self.types.clone(),
            type_not: self.type_not.clone(),
            ignore_files: self.ignore_files.clone(),
            max_depth: self.max_depth,
            max_file_size: self.max_file_size,
        }
    }

    pub(super) fn prepare_listing_execution(&self) -> Result<SearchExecution> {
        let prepare_started = Instant::now();
        let searcher = match IndexSearcher::open(self.index_path.clone()) {
            Ok(searcher) => Some(searcher),
            Err(AppError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        trace_cli_profile("prepare_listing.open_index", prepare_started.elapsed());

        let Some(searcher) = searcher else {
            let scan_started = Instant::now();
            let files = self.scan_requested_files(None)?;
            trace_cli_profile("prepare_listing.scan_files", scan_started.elapsed());
            trace_cli_profile("prepare_listing.total", prepare_started.elapsed());
            return Ok(SearchExecution::Scan { files });
        };

        let scope_started = Instant::now();
        match searcher.ensure_query_scope_supported(
            self.include_hidden,
            self.no_ignore,
            self.max_file_size,
        ) {
            Ok(()) => {
                trace_cli_profile("prepare_listing.ensure_scope", scope_started.elapsed());
                let indexed_max_file_size = searcher
                    .build_settings()
                    .map(|settings| settings.max_file_size);
                if self.requires_filesystem_scope_resolution(indexed_max_file_size) {
                    let scan_started = Instant::now();
                    let files = self.scan_requested_files(indexed_max_file_size)?;
                    trace_cli_profile("prepare_listing.scan_files", scan_started.elapsed());
                    let allowed_paths = files
                        .iter()
                        .map(|file| file.path.to_string_lossy().into_owned())
                        .collect();
                    trace_cli_profile("prepare_listing.total", prepare_started.elapsed());
                    return Ok(SearchExecution::Indexed {
                        searcher,
                        allowed_paths: Some(allowed_paths),
                        path_count: files.len(),
                        paths: files
                            .iter()
                            .map(|file| file.path.to_string_lossy().into_owned())
                            .collect(),
                    });
                }

                let paths_started = Instant::now();
                let paths = searcher.indexed_paths(self.filter());
                trace_cli_profile("prepare_listing.indexed_paths", paths_started.elapsed());
                trace_cli_profile("prepare_listing.total", prepare_started.elapsed());
                Ok(SearchExecution::Indexed {
                    path_count: paths.len(),
                    paths,
                    searcher,
                    allowed_paths: None,
                })
            }
            Err(AppError::InvalidIndex(_)) => {
                trace_cli_profile("prepare_listing.ensure_scope", scope_started.elapsed());
                let scan_started = Instant::now();
                let files = self.scan_requested_files(None)?;
                trace_cli_profile("prepare_listing.scan_files", scan_started.elapsed());
                trace_cli_profile("prepare_listing.total", prepare_started.elapsed());
                Ok(SearchExecution::Scan { files })
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn prepare_query_execution(&self, config: &QueryConfig) -> Result<SearchExecution> {
        let prepare_started = Instant::now();
        let ripgrep_plan = ripgrep_is_available().then(|| self.ripgrep_fallback_plan());
        let open_started = Instant::now();
        let searcher = match IndexSearcher::open(self.index_path.clone()) {
            Ok(searcher) => Some(searcher),
            Err(AppError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        trace_cli_profile("prepare_query.open_index", open_started.elapsed());

        let Some(searcher) = searcher else {
            if let Some(plan) = ripgrep_plan.clone() {
                trace_ripgrep_fallback(&config.regex_pattern, true, "index does not exist");
                trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                return Ok(ripgrep_execution(plan));
            }
            let scan_started = Instant::now();
            let files = self.scan_requested_files(None)?;
            trace_cli_profile("prepare_query.scan_files", scan_started.elapsed());
            trace_cli_profile("prepare_query.total", prepare_started.elapsed());
            return Ok(SearchExecution::Scan { files });
        };

        let scope_started = Instant::now();
        match searcher.ensure_query_scope_supported(
            self.include_hidden,
            self.no_ignore,
            self.max_file_size,
        ) {
            Ok(()) => {
                trace_cli_profile("prepare_query.ensure_scope", scope_started.elapsed());
                let indexed_max_file_size = searcher
                    .build_settings()
                    .map(|settings| settings.max_file_size);
                if self.requires_filesystem_scope_resolution(indexed_max_file_size) {
                    if let Some(plan) = ripgrep_plan.clone() {
                        trace_ripgrep_fallback(
                            &config.regex_pattern,
                            true,
                            "filesystem scope resolution required",
                        );
                        trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                        return Ok(ripgrep_execution(plan));
                    }
                    let scan_started = Instant::now();
                    let files = self.scan_requested_files(indexed_max_file_size)?;
                    trace_cli_profile("prepare_query.scan_files", scan_started.elapsed());
                    trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                    return Ok(SearchExecution::Scan { files });
                }

                if preflight_enabled() {
                    let preflight_started = Instant::now();
                    let preflight =
                        evaluate_index_query_preflight(&searcher, config, self.filter(), None)?;
                    trace_cli_profile("prepare_query.preflight", preflight_started.elapsed());
                    if !preflight.reason.keep_indexed() {
                        if let Some(plan) = ripgrep_plan.clone() {
                            trace_ripgrep_fallback(&config.regex_pattern, true, &preflight.detail);
                            trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                            return Ok(ripgrep_execution(plan));
                        }
                        trace_ripgrep_fallback(
                            &config.regex_pattern,
                            false,
                            &format!("{}; rg unavailable", preflight.detail),
                        );
                        if preflight.reason.requires_scan_backend() {
                            let scan_started = Instant::now();
                            let files = self.scan_requested_files(indexed_max_file_size)?;
                            trace_cli_profile("prepare_query.scan_files", scan_started.elapsed());
                            trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                            return Ok(SearchExecution::Scan { files });
                        }
                    } else {
                        trace_ripgrep_fallback(&config.regex_pattern, false, &preflight.detail);
                    }
                }

                let path_count_started = Instant::now();
                let path_count = self.indexed_query_path_count(&searcher);
                trace_cli_profile("prepare_query.path_count", path_count_started.elapsed());
                trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                Ok(SearchExecution::Indexed {
                    path_count,
                    paths: Vec::new(),
                    searcher,
                    allowed_paths: None,
                })
            }
            Err(AppError::InvalidIndex(reason)) => {
                trace_cli_profile("prepare_query.ensure_scope", scope_started.elapsed());
                if let Some(plan) = ripgrep_plan {
                    trace_ripgrep_fallback(&config.regex_pattern, true, &reason);
                    trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                    Ok(ripgrep_execution(plan))
                } else {
                    let scan_started = Instant::now();
                    let files = self.scan_requested_files(None)?;
                    trace_cli_profile("prepare_query.scan_files", scan_started.elapsed());
                    trace_cli_profile("prepare_query.total", prepare_started.elapsed());
                    Ok(SearchExecution::Scan { files })
                }
            }
            Err(error) => Err(error),
        }
    }

    fn scan_requested_files(
        &self,
        default_max_file_size: Option<u64>,
    ) -> Result<Vec<RepositoryFile>> {
        scan_paths(
            &self.search_paths,
            Some(&self.index_path),
            ScanOptions {
                respect_ignore: !self.no_ignore,
                include_hidden: self.include_hidden,
                max_file_size: self
                    .max_file_size
                    .or(default_max_file_size)
                    .unwrap_or(u64::MAX),
                max_depth: self.max_depth,
                ignore_files: self.ignore_files.clone(),
            },
            Some(&self.path_filter),
        )
    }

    fn requires_filesystem_scope_resolution(&self, indexed_max_file_size: Option<u64>) -> bool {
        if !self.ignore_files.is_empty() || self.max_depth.is_some() {
            return true;
        }

        self.max_file_size
            .zip(indexed_max_file_size)
            .is_some_and(|(requested, indexed)| requested < indexed)
    }

    fn indexed_query_path_count(&self, searcher: &IndexSearcher) -> usize {
        if self.uses_full_index_scope(searcher) {
            return searcher.doc_count();
        }
        searcher.indexed_path_count(self.filter())
    }

    fn uses_full_index_scope(&self, searcher: &IndexSearcher) -> bool {
        if !self.globs.is_empty()
            || !self.iglobs.is_empty()
            || !self.type_add.is_empty()
            || !self.type_clear.is_empty()
            || !self.types.is_empty()
            || !self.type_not.is_empty()
        {
            return false;
        }

        if self.search_paths.len() != 1 {
            return false;
        }

        let Some(build) = searcher.build_settings() else {
            return false;
        };
        self.search_paths[0] == resolve_repo_path(std::path::Path::new(&build.repo_root), ".")
    }
}

fn ripgrep_execution(plan: RipgrepPlan) -> SearchExecution {
    let paths = plan
        .roots
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    SearchExecution::Ripgrep { plan, paths }
}

fn trace_ripgrep_fallback(pattern: &str, use_ripgrep: bool, reason: &str) {
    if env::var_os("BITFUN_TRACE_FALLBACK").is_none() {
        return;
    }

    eprintln!(
        "fallback rg={} pattern={:?} reason={}",
        use_ripgrep, pattern, reason
    );
}

fn trace_cli_profile(phase: &str, duration: Duration) {
    if env::var_os("BITFUN_PROFILE_CLI").is_none() {
        return;
    }

    eprintln!(
        "cli_profile phase={phase} secs={:.6}",
        duration.as_secs_f64()
    );
}
