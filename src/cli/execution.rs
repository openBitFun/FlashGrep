use std::{collections::HashSet, env, io, path::PathBuf, process::Command};

use crate::{
    config::{QueryConfig, TokenizerMode},
    error::{AppError, Result},
    files::{scan_paths, RepositoryFile, ScanOptions},
    index::IndexSearcher,
    path_filter::PathFilter,
    planner::plan,
    search::{FileCount, FileMatchCount, SearchResults},
    search_engine::{
        count_scanned_files, count_total_scanned_files, query_prefers_scan,
        search_json_scanned_files, search_scanned_files,
    },
    tokenizer::create,
};

use super::SearchRequest;

pub(super) enum SearchExecution {
    Indexed {
        searcher: IndexSearcher,
        allowed_paths: Option<HashSet<String>>,
        paths: Vec<String>,
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
            Self::Indexed { paths, .. } => paths.is_empty(),
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
        SearchExecution::Ripgrep { .. } => Err(AppError::InvalidIndex(
            "ripgrep-backed fallback only supports count-style queries".into(),
        )),
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
        SearchExecution::Ripgrep { .. } => Err(AppError::InvalidIndex(
            "ripgrep-backed fallback only supports count-style queries".into(),
        )),
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
    let mut command = Command::new("rg");
    command.arg(match kind {
        RipgrepCountKind::Lines => "--count",
        RipgrepCountKind::Occurrences => "--count-matches",
    });
    command.arg("--with-filename");
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
        command.arg("-g").arg(iglob);
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
    command.arg(&config.regex_pattern);
    for root in &plan.roots {
        command.arg(root);
    }

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

impl SearchRequest {
    pub(super) fn filter(&self) -> Option<&crate::path_filter::PathFilter> {
        Some(&self.path_filter)
    }

    fn ripgrep_fallback_plan(&self, config: &QueryConfig) -> Option<RipgrepPlan> {
        if self.json
            || self.stats
            || self.files
            || self.type_list
            || self.files_with_matches
            || self.files_without_match
            || !(self.count || self.count_matches)
            || self.include_zero
            || config.multiline
            || config.dot_matches_new_line
            || config.has_context()
            || config.max_count.is_some()
        {
            return None;
        }

        Some(RipgrepPlan {
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
        })
    }

    pub(super) fn prepare_listing_execution(&self) -> Result<SearchExecution> {
        let searcher = match IndexSearcher::open(self.index_path.clone()) {
            Ok(searcher) => Some(searcher),
            Err(AppError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };

        let Some(searcher) = searcher else {
            return Ok(SearchExecution::Scan {
                files: self.scan_requested_files(None)?,
            });
        };

        match searcher.ensure_query_scope_supported(
            self.include_hidden,
            self.no_ignore,
            self.max_file_size,
        ) {
            Ok(()) => {
                let indexed_max_file_size = searcher
                    .build_settings()
                    .map(|settings| settings.max_file_size);
                if self.requires_filesystem_scope_resolution(indexed_max_file_size) {
                    let files = self.scan_requested_files(indexed_max_file_size)?;
                    let allowed_paths = files
                        .iter()
                        .map(|file| file.path.to_string_lossy().into_owned())
                        .collect();
                    return Ok(SearchExecution::Indexed {
                        searcher,
                        allowed_paths: Some(allowed_paths),
                        paths: files
                            .iter()
                            .map(|file| file.path.to_string_lossy().into_owned())
                            .collect(),
                    });
                }

                Ok(SearchExecution::Indexed {
                    paths: searcher.indexed_paths(self.filter()),
                    searcher,
                    allowed_paths: None,
                })
            }
            Err(AppError::InvalidIndex(_)) => Ok(SearchExecution::Scan {
                files: self.scan_requested_files(None)?,
            }),
            Err(error) => Err(error),
        }
    }

    pub(super) fn prepare_query_execution(&self, config: &QueryConfig) -> Result<SearchExecution> {
        let searcher = match IndexSearcher::open(self.index_path.clone()) {
            Ok(searcher) => Some(searcher),
            Err(AppError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };

        let Some(searcher) = searcher else {
            return Ok(SearchExecution::Scan {
                files: self.scan_requested_files(None)?,
            });
        };

        match searcher.ensure_query_scope_supported(
            self.include_hidden,
            self.no_ignore,
            self.max_file_size,
        ) {
            Ok(()) => {
                let ripgrep_plan = self.ripgrep_fallback_plan(config);
                let query_prefers_scan = query_prefers_scan(
                    config,
                    searcher.tokenizer_mode(),
                    searcher.tokenizer_options(),
                )?;
                let query_prefers_ripgrep = ripgrep_plan.is_some()
                    && query_prefers_ripgrep_fallback(&searcher, config, self.filter())?;
                if query_prefers_scan || query_prefers_ripgrep {
                    if let Some(plan) = ripgrep_plan {
                        let paths = plan
                            .roots
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect();
                        return Ok(SearchExecution::Ripgrep { plan, paths });
                    }
                    return Ok(SearchExecution::Scan {
                        files: self.scan_requested_files(
                            searcher
                                .build_settings()
                                .map(|settings| settings.max_file_size),
                        )?,
                    });
                }

                let indexed_max_file_size = searcher
                    .build_settings()
                    .map(|settings| settings.max_file_size);
                if self.requires_filesystem_scope_resolution(indexed_max_file_size) {
                    let files = self.scan_requested_files(indexed_max_file_size)?;
                    let allowed_paths = files
                        .iter()
                        .map(|file| file.path.to_string_lossy().into_owned())
                        .collect();
                    return Ok(SearchExecution::Indexed {
                        searcher,
                        allowed_paths: Some(allowed_paths),
                        paths: files
                            .iter()
                            .map(|file| file.path.to_string_lossy().into_owned())
                            .collect(),
                    });
                }

                Ok(SearchExecution::Indexed {
                    paths: searcher.indexed_paths(self.filter()),
                    searcher,
                    allowed_paths: None,
                })
            }
            Err(AppError::InvalidIndex(_)) => Ok(SearchExecution::Scan {
                files: self.scan_requested_files(None)?,
            }),
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
}

fn query_prefers_ripgrep_fallback(
    searcher: &IndexSearcher,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<bool> {
    match evaluate_ripgrep_fallback(searcher, config, filter)? {
        RipgrepFallbackDecision::UseRipgrep { reason } => {
            trace_ripgrep_fallback(&config.regex_pattern, true, &reason);
            Ok(true)
        }
        RipgrepFallbackDecision::KeepIndexed { reason } => {
            trace_ripgrep_fallback(&config.regex_pattern, false, &reason);
            Ok(false)
        }
    }
}

enum RipgrepFallbackDecision {
    UseRipgrep { reason: String },
    KeepIndexed { reason: String },
}

fn evaluate_preflight_candidate_thresholds(
    doc_count: usize,
    branch_candidates: &[Vec<u32>],
) -> RipgrepFallbackDecision {
    let branch_limit = preflight_branch_limit(doc_count);
    let union_limit = preflight_union_limit(doc_count);
    let mut union = HashSet::new();
    let branch_sizes = branch_candidates.iter().map(Vec::len).collect::<Vec<_>>();

    for (index, candidates) in branch_candidates.iter().enumerate() {
        if candidates.len() > branch_limit {
            return RipgrepFallbackDecision::UseRipgrep {
                reason: format!(
                    "branch {index} candidates {} exceeded limit {branch_limit}; branch_sizes={branch_sizes:?}",
                    candidates.len(),
                ),
            };
        }

        for &doc_id in candidates {
            union.insert(doc_id);
            if union.len() > union_limit {
                return RipgrepFallbackDecision::UseRipgrep {
                    reason: format!(
                        "union candidates {} exceeded limit {union_limit}; branch_sizes={branch_sizes:?}",
                        union.len(),
                    ),
                };
            }
        }
    }

    RipgrepFallbackDecision::KeepIndexed {
        reason: format!(
            "candidate preflight stayed within limits (branch<={branch_limit}, union<={union_limit}, branch_sizes={branch_sizes:?}, union_size={})",
            union.len()
        ),
    }
}

fn preflight_branch_limit(doc_count: usize) -> usize {
    (doc_count / 8).max(20_000)
}

fn preflight_union_limit(doc_count: usize) -> usize {
    // Be more aggressive on broad alternations. These still verify exactly,
    // so the only risk here is sending a few medium-width cases to rg earlier.
    (doc_count / 28).max(5_000)
}

fn evaluate_ripgrep_fallback(
    searcher: &IndexSearcher,
    config: &QueryConfig,
    filter: Option<&PathFilter>,
) -> Result<RipgrepFallbackDecision> {
    if searcher.tokenizer_mode() != TokenizerMode::SparseNgram {
        return Ok(RipgrepFallbackDecision::KeepIndexed {
            reason: "tokenizer is not sparse-ngram".into(),
        });
    }

    let plan = plan(&config.regex_pattern)?;
    if plan.fallback_to_scan {
        return Ok(RipgrepFallbackDecision::UseRipgrep {
            reason: "planner requested fallback_to_scan".into(),
        });
    }

    let Some(alternation) = plan.pure_literal_alternation.as_ref() else {
        return Ok(RipgrepFallbackDecision::KeepIndexed {
            reason: "query is not a pure literal alternation".into(),
        });
    };

    let tokenizer = create(
        searcher.tokenizer_mode(),
        searcher.tokenizer_options().clone(),
    );
    for literal in &alternation.literals {
        let folded = if config.case_insensitive {
            if literal.is_ascii() {
                literal.to_ascii_lowercase()
            } else {
                literal.to_lowercase()
            }
        } else {
            literal.clone()
        };
        let mut token_hashes = Vec::new();
        tokenizer.collect_query_token_hashes(&folded, &mut token_hashes);
        if token_hashes.is_empty() {
            return Ok(RipgrepFallbackDecision::UseRipgrep {
                reason: format!("literal '{literal}' produced no sparse query tokens"),
            });
        }
    }

    let Some(branch_candidates) = searcher
        .try_candidate_doc_ids_by_branch_without_literal_scan_for_plan_with_allowed_paths(
            config, &plan, filter, None,
        )?
    else {
        return Ok(RipgrepFallbackDecision::UseRipgrep {
            reason: "candidate preflight required literal scan".into(),
        });
    };

    Ok(evaluate_preflight_candidate_thresholds(
        searcher.doc_count(),
        &branch_candidates,
    ))
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

#[cfg(test)]
mod tests {
    use super::{evaluate_preflight_candidate_thresholds, RipgrepFallbackDecision};

    #[test]
    fn preflight_uses_ripgrep_when_branch_exceeds_limit() {
        let doc_count = 120_000;
        let branch_candidates = vec![vec![1; 7_089], vec![2; 6_461], vec![3; 40_796]];
        let decision = evaluate_preflight_candidate_thresholds(doc_count, &branch_candidates);
        match decision {
            RipgrepFallbackDecision::UseRipgrep { reason } => {
                assert!(reason.contains("branch 2 candidates 40796 exceeded limit 20000"));
                assert!(reason.contains("branch_sizes=[7089, 6461, 40796]"));
            }
            RipgrepFallbackDecision::KeepIndexed { reason } => {
                panic!("expected ripgrep fallback, got indexed: {reason}");
            }
        }
    }

    #[test]
    fn preflight_uses_ripgrep_when_union_exceeds_limit() {
        let doc_count = 120_000;
        let branch_candidates = vec![
            (0..8_000).collect::<Vec<u32>>(),
            (20_000..29_001).collect::<Vec<u32>>(),
            (40_000..47_000).collect::<Vec<u32>>(),
        ];
        let decision = evaluate_preflight_candidate_thresholds(doc_count, &branch_candidates);
        match decision {
            RipgrepFallbackDecision::UseRipgrep { reason } => {
                assert!(reason.contains("union candidates 5001 exceeded limit 5000"));
                assert!(reason.contains("branch_sizes=[8000, 9001, 7000]"));
            }
            RipgrepFallbackDecision::KeepIndexed { reason } => {
                panic!("expected ripgrep fallback, got indexed: {reason}");
            }
        }
    }

    #[test]
    fn preflight_uses_ripgrep_for_medium_union_after_tightening() {
        let doc_count = 120_000;
        let branch_candidates = vec![
            (0..2_311).collect::<Vec<u32>>(),
            (3_000..5_154).collect::<Vec<u32>>(),
            (6_000..8_917).collect::<Vec<u32>>(),
        ];
        let decision = evaluate_preflight_candidate_thresholds(doc_count, &branch_candidates);
        match decision {
            RipgrepFallbackDecision::UseRipgrep { reason } => {
                assert!(reason.contains("union candidates 5001 exceeded limit 5000"));
                assert!(reason.contains("branch_sizes=[2311, 2154, 2917]"));
            }
            RipgrepFallbackDecision::KeepIndexed { reason } => {
                panic!("expected ripgrep fallback, got indexed: {reason}");
            }
        }
    }

    #[test]
    fn preflight_keeps_indexed_for_narrower_union() {
        let doc_count = 120_000;
        let branch_candidates = vec![
            (0..2_182).collect::<Vec<u32>>(),
            (1_500..4_625).collect::<Vec<u32>>(),
            (20_000..20_003).collect::<Vec<u32>>(),
        ];
        let decision = evaluate_preflight_candidate_thresholds(doc_count, &branch_candidates);
        match decision {
            RipgrepFallbackDecision::KeepIndexed { reason } => {
                assert!(reason.contains("branch<=20000, union<=5000"));
                assert!(reason.contains("branch_sizes=[2182, 3125, 3]"));
                assert!(reason.contains("union_size=4628"));
            }
            RipgrepFallbackDecision::UseRipgrep { reason } => {
                panic!("expected indexed path, got fallback: {reason}");
            }
        }
    }
}
