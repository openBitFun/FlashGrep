use std::path::Path;

use crate::{
    json_output::{JsonData, JsonMessage, JsonSearchReport},
    search::{FileCount, FileMatchCount},
};

pub(super) fn print_output_line(
    path: &str,
    line: usize,
    column: Option<usize>,
    snippet: &str,
    is_context: bool,
    print_path: bool,
    print_line_number: bool,
    print_column: bool,
    bytes_printed: &mut u64,
) {
    let mut fields = Vec::new();
    if print_path {
        fields.push(path.to_string());
    }
    if print_line_number {
        fields.push(line.to_string());
    }
    if print_column {
        if let Some(column) = column {
            fields.push(column.to_string());
        }
    }
    let separator = if is_context { '-' } else { ':' };
    let line = if fields.is_empty() {
        snippet.to_string()
    } else {
        format!(
            "{}{separator}{snippet}",
            fields.join(&separator.to_string())
        )
    };
    *bytes_printed += line.len() as u64 + 1;
    println!("{line}");
}

pub(super) fn print_paths(
    paths: &[String],
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for path in paths {
        println!(
            "{}",
            format_output_path(
                path,
                current_dir,
                display_relative_paths,
                display_dot_prefix
            )
        );
    }
}

pub(super) fn print_counts(
    counts: &[FileCount],
    print_path: bool,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for count in counts {
        if print_path {
            println!(
                "{}:{}",
                format_output_path(
                    &count.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix
                ),
                count.matched_lines
            );
        } else {
            println!("{}", count.matched_lines);
        }
    }
}

pub(super) fn print_total_counts(
    counts: &[FileMatchCount],
    print_path: bool,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for count in counts {
        if print_path {
            println!(
                "{}:{}",
                format_output_path(
                    &count.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix
                ),
                count.matched_occurrences
            );
        } else {
            println!("{}", count.matched_occurrences);
        }
    }
}

pub(super) struct SearchStats {
    pub(super) matches: usize,
    pub(super) matched_lines: usize,
    pub(super) searches_with_match: usize,
    pub(super) searches: usize,
    pub(super) bytes_printed: u64,
    pub(super) bytes_searched: u64,
}

pub(super) fn print_stats(
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
) {
    println!();
    println!("{} matches", stats.matches);
    println!("{} matched lines", stats.matched_lines);
    println!("{} files contained matches", stats.searches_with_match);
    println!("{} files searched", stats.searches);
    println!("{} bytes printed", stats.bytes_printed);
    println!("{} bytes searched", stats.bytes_searched);
    println!(
        "{:.6} seconds spent searching",
        search_elapsed.as_secs_f64()
    );
    println!("{:.6} seconds total", total_elapsed.as_secs_f64());
}

pub(super) fn format_output_path(
    path: &str,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) -> String {
    if !display_relative_paths {
        return path.to_string();
    }
    let candidate = Path::new(path);
    let Ok(relative) = candidate.strip_prefix(current_dir) else {
        return path.to_string();
    };
    if relative.as_os_str().is_empty() {
        return if display_dot_prefix {
            ".".to_string()
        } else {
            String::new()
        };
    }
    let relative = relative.display().to_string();
    if display_dot_prefix {
        format!("./{relative}")
    } else {
        relative
    }
}

pub(super) fn rewrite_json_paths(
    report: &mut JsonSearchReport,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    for file in &mut report.files {
        if let JsonData::Text { text } = &mut file.path {
            *text = format_output_path(
                text,
                current_dir,
                display_relative_paths,
                display_dot_prefix,
            );
        }
        for message in &mut file.messages {
            match message {
                JsonMessage::Begin(begin) => rewrite_json_data_path(
                    &mut begin.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::End(end) => rewrite_json_data_path(
                    &mut end.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Match(matched) => rewrite_json_data_path(
                    &mut matched.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Context(context) => rewrite_json_data_path(
                    &mut context.path,
                    current_dir,
                    display_relative_paths,
                    display_dot_prefix,
                ),
                JsonMessage::Summary(_) => {}
            }
        }
    }
}

fn rewrite_json_data_path(
    path: &mut Option<JsonData>,
    current_dir: &Path,
    display_relative_paths: bool,
    display_dot_prefix: bool,
) {
    if let Some(JsonData::Text { text }) = path {
        *text = format_output_path(
            text,
            current_dir,
            display_relative_paths,
            display_dot_prefix,
        );
    }
}
