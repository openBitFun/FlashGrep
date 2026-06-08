use aho_corasick::{AhoCorasick, MatchKind};
use regex_syntax::{
    hir::{Hir, HirKind},
    parse,
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    search::LineMatch,
};

#[derive(Debug, Clone)]
pub(crate) struct PureLiteralMatcher {
    matcher: AhoCorasick,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LiteralScanStats {
    pub(crate) matched_lines: usize,
    pub(crate) matched_occurrences: usize,
}

impl PureLiteralMatcher {
    pub(crate) fn compile(config: &QueryConfig, multiline_verifier: bool) -> Result<Option<Self>> {
        if multiline_verifier || config.word_regexp || config.line_regexp {
            return Ok(None);
        }

        let Some(literals) = collect_pure_literals(config)? else {
            return Ok(None);
        };
        if config.case_insensitive && !literals.iter().all(|literal| literal.is_ascii()) {
            return Ok(None);
        }

        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostFirst)
            .ascii_case_insensitive(config.case_insensitive)
            .build(&literals)
            .map_err(|error| AppError::InvalidPattern(error.to_string()))?;
        Ok(Some(Self { matcher }))
    }

    pub(crate) fn count(&self, bytes: &[u8]) -> LiteralScanStats {
        let mut matched_lines = 0usize;
        let mut matched_occurrences = 0usize;
        let mut current_line_start = 0usize;
        let mut current_line_end = next_line_end(bytes, current_line_start);
        let mut current_line_has_match = false;

        for matched in self.matcher.find_iter(bytes) {
            while matched.start() >= current_line_end && current_line_start < bytes.len() {
                current_line_start = current_line_end;
                current_line_end = next_line_end(bytes, current_line_start);
                current_line_has_match = false;
            }
            if !current_line_has_match {
                matched_lines += 1;
                current_line_has_match = true;
            }
            matched_occurrences += 1;
        }

        LiteralScanStats {
            matched_lines,
            matched_occurrences,
        }
    }

    pub(crate) fn line_matches(
        &self,
        bytes: &[u8],
        max_count: Option<usize>,
    ) -> LiteralLineScanStats {
        let max_count = max_count.unwrap_or(usize::MAX);
        if max_count == 0 {
            return LiteralLineScanStats::default();
        }

        let mut matched_lines = 0usize;
        let mut matched_occurrences = 0usize;
        let mut current_line_number = 1usize;
        let mut current_line_start = 0usize;
        let mut current_line_end = next_line_end(bytes, current_line_start);
        let mut current_line_has_match = false;
        let mut line_matches = Vec::new();

        for matched in self.matcher.find_iter(bytes) {
            while matched.start() >= current_line_end && current_line_start < bytes.len() {
                current_line_start = current_line_end;
                current_line_end = next_line_end(bytes, current_line_start);
                current_line_number += 1;
                current_line_has_match = false;
            }
            if !current_line_has_match {
                matched_lines += 1;
                current_line_has_match = true;
                if line_matches.len() < max_count {
                    line_matches.push(LineMatch {
                        path: String::new(),
                        line_number: current_line_number,
                    });
                }
            }
            matched_occurrences += 1;
            if line_matches.len() >= max_count {
                break;
            }
        }

        LiteralLineScanStats {
            matched_lines,
            matched_occurrences,
            bytes_searched: bytes.len() as u64,
            line_matches,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LiteralLineScanStats {
    pub(crate) matched_lines: usize,
    pub(crate) matched_occurrences: usize,
    pub(crate) bytes_searched: u64,
    pub(crate) line_matches: Vec<LineMatch>,
}

fn collect_pure_literals(config: &QueryConfig) -> Result<Option<Vec<String>>> {
    let patterns = if config.patterns.is_empty() {
        vec![config.regex_pattern.as_str()]
    } else {
        config.patterns.iter().map(String::as_str).collect()
    };

    let mut literals = Vec::new();
    for pattern in patterns {
        if config.fixed_strings {
            literals.push(pattern.to_owned());
            continue;
        }

        let Some(pattern_literals) = extract_pure_literals(pattern)? else {
            return Ok(None);
        };
        literals.extend(pattern_literals);
    }

    if literals.is_empty()
        || literals
            .iter()
            .any(|literal| literal.is_empty() || literal.contains(['\n', '\r']))
    {
        return Ok(None);
    }
    Ok(Some(literals))
}

fn extract_pure_literals(pattern: &str) -> Result<Option<Vec<String>>> {
    let hir = parse(pattern).map_err(|error| AppError::InvalidPattern(error.to_string()))?;
    let literals = match hir.kind() {
        HirKind::Alternation(branches) => branches
            .iter()
            .map(extract_pure_literal)
            .collect::<Option<Vec<_>>>(),
        _ => extract_pure_literal(&hir).map(|literal| vec![literal]),
    };
    Ok(literals)
}

fn extract_pure_literal(hir: &Hir) -> Option<String> {
    match hir.kind() {
        HirKind::Literal(literal) => Some(String::from_utf8_lossy(&literal.0).into_owned()),
        HirKind::Capture(capture) => extract_pure_literal(&capture.sub),
        HirKind::Concat(parts) => {
            let mut literal = String::new();
            for part in parts {
                literal.push_str(&extract_pure_literal(part)?);
            }
            Some(literal)
        }
        _ => None,
    }
}

fn next_line_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return bytes.len();
    }
    start + memchr::memchr(b'\n', &bytes[start..]).map_or(bytes.len() - start, |index| index + 1)
}

#[cfg(test)]
mod tests {
    use super::PureLiteralMatcher;
    use crate::{config::QueryConfig, search::SearchMode};

    fn literal_matcher(pattern: &str) -> PureLiteralMatcher {
        PureLiteralMatcher::compile(
            &QueryConfig {
                regex_pattern: pattern.into(),
                search_mode: SearchMode::CountOnly,
                ..QueryConfig::default()
            },
            false,
        )
        .expect("literal matcher should compile")
        .expect("literal matcher should exist")
    }

    #[test]
    fn count_occurrences_preserves_all_matches() {
        let matcher = literal_matcher("ui");
        let stats = matcher.count(b"ui ui ui\nalpha\nui beta ui\n");
        assert_eq!(stats.matched_lines, 2);
        assert_eq!(stats.matched_occurrences, 5);
    }

    #[test]
    fn line_matches_return_unique_matching_lines_until_limit() {
        let matcher = literal_matcher("ui");
        let stats = matcher.line_matches(b"ui ui ui\nalpha\nui beta ui\nui gamma\n", Some(2));
        assert_eq!(stats.matched_lines, 2);
        assert_eq!(stats.matched_occurrences, 4);
        assert_eq!(
            stats
                .line_matches
                .iter()
                .map(|matched| matched.line_number)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
    }
}
