use std::{env, path::Path, process::Command};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    index::format::DocMeta,
};

use super::shared::resolve_candidate_docs;

const RG_BATCH_ARG_BYTES: usize = 96 * 1024;

pub(super) fn enabled(config: &QueryConfig) -> bool {
    config.search_mode == crate::search::SearchMode::CountOnly
        && !config.multiline
        && !config.dot_matches_new_line
        && !config.has_context()
        && !config.fixed_strings
        && !config.word_regexp
        && !config.line_regexp
        && config.max_count.is_none()
        && env::var_os("BITFUN_EXPERIMENTAL_RG_VERIFY").is_some()
}

pub(super) fn count_matches(
    config: &QueryConfig,
    docs: &[DocMeta],
    candidate_ids: &[u32],
) -> Result<usize> {
    if candidate_ids.is_empty() {
        return Ok(0);
    }
    let candidate_docs = resolve_candidate_docs(docs, candidate_ids)?;

    let mut total = 0usize;
    let mut start = 0usize;
    while start < candidate_docs.len() {
        let end = next_batch_end(&candidate_docs, start);
        total += count_matches_in_batch(config, &candidate_docs[start..end])?;
        start = end;
    }
    Ok(total)
}

fn next_batch_end(docs: &[&DocMeta], start: usize) -> usize {
    let mut bytes = 0usize;
    let mut end = start;
    while end < docs.len() {
        let path = docs[end].path.as_bytes();
        let next = bytes + path.len() + 1;
        if end > start && next > RG_BATCH_ARG_BYTES {
            break;
        }
        bytes = next;
        end += 1;
    }
    end
}

fn count_matches_in_batch(config: &QueryConfig, docs: &[&DocMeta]) -> Result<usize> {
    let mut command = Command::new("rg");
    command.arg("--count");
    command.arg("--with-filename");
    command.arg("--no-messages");
    if config.case_insensitive {
        command.arg("-i");
    }
    command.arg(&config.regex_pattern);
    for doc in docs {
        command.arg(Path::new(&doc.path));
    }

    let output = command.output()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(AppError::InvalidIndex(format!(
            "rg verify failed with status {}",
            output.status
        )));
    }
    parse_rg_count_output(&output.stdout)
}

fn parse_rg_count_output(stdout: &[u8]) -> Result<usize> {
    let mut total = 0usize;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let line = std::str::from_utf8(line).map_err(|error| {
            AppError::InvalidIndex(format!("rg count output was not utf-8: {error}"))
        })?;
        let count = line
            .rsplit_once(':')
            .map_or(line, |(_, count)| count)
            .parse::<usize>()
            .map_err(|error| {
                AppError::InvalidIndex(format!("failed to parse rg count output '{line}': {error}"))
            })?;
        total += count;
    }
    Ok(total)
}
