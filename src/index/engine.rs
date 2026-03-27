use std::{io::ErrorKind, path::Path};

use crate::{
    config::{BuildConfig, QueryConfig},
    error::{AppError, Result},
    files::{read_text_file, scan_paths, RepositoryFile, ScanOptions},
    index::{
        build_index,
        format::DocMeta,
        searcher::{
            verify::{build_matcher, requires_multiline_verification, verify_candidates},
            IndexSearcher, IndexWorktreeDiff,
        },
    },
    json_output::{JsonDuration, JsonSearchFile, JsonSearchReport, JsonStats},
    path_filter::PathFilter,
    planner::plan,
    search::{FileCount, FileMatchCount, SearchHit, SearchResults},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPolicy {
    IndexOnly,
    FallbackToScan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBackend {
    Index,
    Scan,
}

#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub backend: SearchBackend,
    pub results: SearchResults,
}

#[derive(Debug, Clone)]
pub struct SearchEngine {
    build_config: BuildConfig,
}

impl SearchEngine {
    pub fn new(build_config: BuildConfig) -> Self {
        Self { build_config }
    }

    pub fn build_index(&self) -> Result<usize> {
        build_index(&self.build_config)
    }

    pub fn rebuild_index(&self) -> Result<usize> {
        build_index(&self.build_config)
    }

    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        self.open_searcher()?.diff_against_worktree()
    }

    pub fn stale_reason(&self) -> Result<Option<String>> {
        self.open_searcher()?.stale_reason()
    }

    pub fn search(&self, config: &QueryConfig, policy: SearchPolicy) -> Result<SearchOutcome> {
        self.search_with_filter(config, None, policy)
    }

    pub fn search_with_filter(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
    ) -> Result<SearchOutcome> {
        let query_prefers_scan = query_prefers_scan(config)?;
        match self.open_searcher() {
            Ok(searcher) => {
                self.search_with_searcher(searcher, config, filter, policy, query_prefers_scan)
            }
            Err(AppError::Io(error)) if error.kind() == ErrorKind::NotFound => {
                self.search_without_index(config, filter, policy)
            }
            Err(error) => Err(error),
        }
    }

    fn search_with_searcher(
        &self,
        searcher: IndexSearcher,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
        query_prefers_scan: bool,
    ) -> Result<SearchOutcome> {
        if matches!(policy, SearchPolicy::FallbackToScan) && query_prefers_scan {
            let files = self.scan_repo_files(filter)?;
            return Ok(SearchOutcome {
                backend: SearchBackend::Scan,
                results: search_scanned_files(config, &files)?,
            });
        }

        Ok(SearchOutcome {
            backend: SearchBackend::Index,
            results: searcher.search_with_filter(config, filter)?,
        })
    }

    fn search_without_index(
        &self,
        config: &QueryConfig,
        filter: Option<&PathFilter>,
        policy: SearchPolicy,
    ) -> Result<SearchOutcome> {
        match policy {
            SearchPolicy::IndexOnly => Err(AppError::InvalidIndex(format!(
                "index does not exist at {}",
                self.build_config.index_path.display()
            ))),
            SearchPolicy::FallbackToScan => {
                let files = self.scan_repo_files(filter)?;
                Ok(SearchOutcome {
                    backend: SearchBackend::Scan,
                    results: search_scanned_files(config, &files)?,
                })
            }
        }
    }

    fn open_searcher(&self) -> Result<IndexSearcher> {
        IndexSearcher::open(self.build_config.index_path.clone())
    }

    fn scan_repo_files(&self, filter: Option<&PathFilter>) -> Result<Vec<RepositoryFile>> {
        scan_text_files(scan_paths(
            std::slice::from_ref(&self.build_config.repo_path),
            Some(&self.build_config.index_path),
            scan_options(&self.build_config),
            filter,
        )?)
    }
}

pub(crate) fn search_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
) -> Result<SearchResults> {
    let docs = repository_docs(files)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let matcher = build_matcher(config, multiline_verifier)?;
    let outcomes = verify_candidates(
        &candidate_ids,
        &docs,
        &matcher,
        multiline_verifier,
        config.search_mode,
        config.max_count,
        config.before_context,
        config.after_context,
    )?;

    let mut hits = Vec::new();
    let mut matched_lines = 0usize;
    let mut matched_occurrences = 0usize;
    let mut searches_with_match = 0usize;
    let mut bytes_searched = 0u64;
    for task in outcomes {
        matched_lines += task.outcome.matched_lines;
        matched_occurrences += task.outcome.matched_occurrences;
        bytes_searched += task.outcome.bytes_searched;
        if !task.outcome.matches.is_empty() {
            searches_with_match += 1;
            let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
                AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
            })?];
            hits.push(SearchHit {
                path: doc.path.clone(),
                matches: task.outcome.matches,
                lines: task.outcome.lines,
            });
        } else if task.outcome.matched_lines > 0 || task.outcome.matched_occurrences > 0 {
            searches_with_match += 1;
        }
    }

    Ok(SearchResults {
        candidate_docs: docs.len(),
        searches_with_match,
        bytes_searched,
        matched_lines,
        matched_occurrences,
        hits,
    })
}

pub(crate) fn count_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
    include_zero: bool,
) -> Result<Vec<FileCount>> {
    let docs = repository_docs(files)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let matcher = build_matcher(config, multiline_verifier)?;
    let outcomes = verify_candidates(
        &candidate_ids,
        &docs,
        &matcher,
        multiline_verifier,
        config.search_mode,
        config.max_count,
        config.before_context,
        config.after_context,
    )?;
    let mut counts = Vec::with_capacity(outcomes.len());
    for task in outcomes {
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        counts.push(FileCount {
            path: doc.path.clone(),
            matched_lines: task.outcome.matched_lines,
        });
    }
    if include_zero {
        Ok(counts)
    } else {
        Ok(counts.into_iter().filter(FileCount::has_match).collect())
    }
}

pub(crate) fn count_total_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
    include_zero: bool,
) -> Result<Vec<FileMatchCount>> {
    let docs = repository_docs(files)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let matcher = build_matcher(config, multiline_verifier)?;
    let outcomes = verify_candidates(
        &candidate_ids,
        &docs,
        &matcher,
        multiline_verifier,
        crate::search::SearchMode::CountMatches,
        config.max_count,
        0,
        0,
    )?;
    let mut counts = Vec::with_capacity(outcomes.len());
    for task in outcomes {
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        counts.push(FileMatchCount {
            path: doc.path.clone(),
            matched_occurrences: task.outcome.matched_occurrences,
        });
    }
    if include_zero {
        Ok(counts)
    } else {
        Ok(counts
            .into_iter()
            .filter(FileMatchCount::has_match)
            .collect())
    }
}

pub(crate) fn search_json_scanned_files(
    config: &QueryConfig,
    files: &[RepositoryFile],
) -> Result<JsonSearchReport> {
    let total_started = std::time::Instant::now();
    let docs = repository_docs(files)?;
    let candidate_ids = docs.iter().map(|doc| doc.doc_id).collect::<Vec<_>>();
    let multiline_verifier = requires_multiline_verification(config)?;
    let matcher = build_matcher(config, multiline_verifier)?;
    let outcomes = super::searcher::verify::verify_candidates_json(
        &candidate_ids,
        &docs,
        &matcher,
        multiline_verifier,
        config.search_mode,
        config.max_count,
        config.before_context,
        config.after_context,
    )?;

    let mut files = Vec::new();
    let mut searches_with_match = 0u64;
    let mut bytes_searched = 0u64;
    let mut matched_lines = 0u64;
    let mut matches = 0u64;

    for task in outcomes {
        let doc = &docs[usize::try_from(task.doc_id).map_err(|_| {
            AppError::ValueOutOfRange(format!("doc id {} exceeds usize range", task.doc_id))
        })?];
        bytes_searched += task.outcome.bytes_searched;
        if task.outcome.matched_occurrences == 0 {
            continue;
        }

        searches_with_match += 1;
        matched_lines += task.outcome.matched_lines as u64;
        matches += task.outcome.matched_occurrences as u64;
        files.push(JsonSearchFile {
            path: crate::json_output::JsonData::from_string(doc.path.clone()),
            messages: task.outcome.messages,
            binary_offset: task.outcome.binary_offset,
            stats: JsonStats {
                elapsed: JsonDuration::from_duration(task.elapsed),
                searches: 1,
                searches_with_match: 1,
                bytes_searched: task.outcome.bytes_searched,
                bytes_printed: 0,
                matched_lines: task.outcome.matched_lines as u64,
                matches: task.outcome.matched_occurrences as u64,
            },
        });
    }

    Ok(JsonSearchReport {
        files,
        summary: JsonStats {
            elapsed: JsonDuration::from_duration(total_started.elapsed()),
            searches: candidate_ids.len() as u64,
            searches_with_match,
            bytes_searched,
            bytes_printed: 0,
            matched_lines,
            matches,
        },
        elapsed_total: JsonDuration::from_duration(total_started.elapsed()),
    })
}

fn repository_docs(files: &[RepositoryFile]) -> Result<Vec<DocMeta>> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let doc_id = u32::try_from(index).map_err(|_| {
                AppError::ValueOutOfRange(format!("scanned document id {index} exceeds u32 range"))
            })?;
            Ok(DocMeta {
                doc_id,
                path: file.path.to_string_lossy().into_owned(),
                size: file.size,
                mtime_nanos: file.mtime_nanos,
            })
        })
        .collect()
}

fn scan_options(config: &BuildConfig) -> ScanOptions {
    ScanOptions {
        respect_ignore: matches!(config.corpus_mode, crate::config::CorpusMode::RespectIgnore),
        include_hidden: config.include_hidden,
        max_file_size: config.max_file_size,
        max_depth: None,
        ignore_files: Vec::new(),
    }
}

fn scan_text_files(files: Vec<RepositoryFile>) -> Result<Vec<RepositoryFile>> {
    let mut text_files = Vec::with_capacity(files.len());
    for file in files {
        if read_text_file(Path::new(&file.path))?.is_some() {
            text_files.push(file);
        }
    }
    Ok(text_files)
}

pub(crate) fn query_prefers_scan(config: &QueryConfig) -> Result<bool> {
    Ok(plan(&config.regex_pattern)?.fallback_to_scan)
}
