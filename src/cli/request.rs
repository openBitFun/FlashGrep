use std::{
    fs::File,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::OnceLock,
};

use crate::{
    config::QueryConfig,
    error::{AppError, Result},
    path_filter::{normalize_path, PathFilter, PathFilterArgs},
    search::SearchMode,
};

use super::SearchArgs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RipgrepCaseMode {
    Default,
    Ignore,
    Sensitive,
    Smart,
}

#[derive(Debug)]
pub(super) struct SearchRequest {
    pub(super) index_path: PathBuf,
    pub(super) current_dir: PathBuf,
    pub(super) raw_search_paths: Vec<PathBuf>,
    pub(super) search_paths: Vec<PathBuf>,
    pub(super) used_default_search_path: bool,
    pub(super) path_filter: PathFilter,
    pub(super) config: Option<QueryConfig>,
    pub(super) files: bool,
    pub(super) type_list: bool,
    pub(super) json: bool,
    pub(super) files_with_matches: bool,
    pub(super) files_without_match: bool,
    pub(super) count: bool,
    pub(super) count_matches: bool,
    pub(super) quiet: bool,
    pub(super) include_zero: bool,
    pub(super) only_matching: bool,
    pub(super) stats: bool,
    pub(super) print_path: bool,
    pub(super) print_line_number: bool,
    pub(super) print_column: bool,
    pub(super) display_relative_paths: bool,
    pub(super) display_dot_prefix: bool,
    fixed_strings: bool,
    multiline: bool,
    multiline_dotall: bool,
    word_regexp: bool,
    line_regexp: bool,
    with_filename: bool,
    no_filename: bool,
    line_number: bool,
    no_line_number: bool,
    before_context: usize,
    after_context: usize,
    case_mode: RipgrepCaseMode,
    pub(super) include_hidden: bool,
    pub(super) no_ignore: bool,
    pub(super) globs: Vec<String>,
    pub(super) iglobs: Vec<String>,
    pub(super) type_add: Vec<String>,
    pub(super) type_clear: Vec<String>,
    pub(super) types: Vec<String>,
    pub(super) type_not: Vec<String>,
    pub(super) ignore_files: Vec<PathBuf>,
    pub(super) max_depth: Option<usize>,
    pub(super) max_file_size: Option<u64>,
    max_count: Option<usize>,
}

impl SearchRequest {
    pub(super) fn from_args(args: SearchArgs) -> Result<Self> {
        if args.files && args.type_list {
            return Err(AppError::InvalidPattern(
                "--files and --type-list cannot be used together".into(),
            ));
        }
        if args.files
            && (args.count
                || args.count_matches
                || args.files_with_matches
                || args.files_without_match)
        {
            return Err(AppError::InvalidPattern(
                "--files cannot be combined with -c/--count, --count-matches, -l/--files-with-matches or -L/--files-without-match".into(),
            ));
        }
        if (args.files || args.type_list) && !args.regexp.is_empty() {
            return Err(AppError::InvalidPattern(
                "-e/--regexp is not supported with --files or --type-list".into(),
            ));
        }
        if (args.files || args.type_list) && !args.pattern_files.is_empty() {
            return Err(AppError::InvalidPattern(
                "-f/--file is not supported with --files or --type-list".into(),
            ));
        }
        let json = args.json && !args.no_json;
        let stats = (args.stats && !args.no_stats) || json;
        if json
            && (args.files
                || args.type_list
                || args.files_with_matches
                || args.files_without_match
                || args.count
                || args.count_matches)
        {
            return Err(AppError::InvalidPattern(
                "--json cannot be combined with --files, --type-list, -l/--files-with-matches, -L/--files-without-match, -c/--count or --count-matches".into(),
            ));
        }
        if args.multiline_dotall && !args.multiline {
            return Err(AppError::InvalidPattern(
                "--multiline-dotall requires --multiline".into(),
            ));
        }
        if args.follow {
            return Err(AppError::InvalidPattern(
                "--follow is not supported yet for indexed search".into(),
            ));
        }
        let used_default_search_path = uses_default_search_path(&args);
        let paths = collect_search_paths(&args)?;
        let current_dir = std::env::current_dir()?;
        let normalized_paths = paths
            .iter()
            .map(|path| normalize_path(path, &current_dir))
            .collect::<Vec<_>>();
        let path_filter = PathFilter::new(
            PathFilterArgs {
                roots: normalized_paths.clone(),
                globs: args.glob.clone(),
                iglobs: args.iglob.clone(),
                type_add: args.type_add.clone(),
                type_clear: args.type_clear.clone(),
                types: args.file_type.clone(),
                type_not: args.file_type_not.clone(),
            },
            &current_dir,
        )?;
        let index_path = args
            .index
            .clone()
            .unwrap_or_else(|| infer_index_path(&normalized_paths, &current_dir));
        let print_column = args.column;
        let print_line_number = print_column || (args.line_number && !args.no_line_number);
        let print_path = if args.no_filename {
            false
        } else if args.with_filename {
            true
        } else {
            default_print_path(&normalized_paths)
        };
        let display_relative_paths =
            used_default_search_path || paths.iter().all(|path| !path.is_absolute());
        let display_dot_prefix =
            !used_default_search_path && paths.len() == 1 && paths[0] == Path::new(".");
        let (before_context, after_context) = resolve_context_mode(&args);
        let config = if args.files || args.type_list {
            None
        } else {
            let patterns = collect_patterns(&args)?;
            let pattern = combine_patterns(&patterns, args.fixed_strings);
            let case_insensitive = resolve_case_mode(&patterns, &args);
            let search_mode = resolve_search_mode(&args, stats)?;
            Some(QueryConfig {
                regex_pattern: pattern,
                patterns,
                case_insensitive,
                multiline: args.multiline,
                dot_matches_new_line: args.multiline_dotall,
                fixed_strings: args.fixed_strings,
                word_regexp: args.word_regexp,
                line_regexp: args.line_regexp,
                before_context,
                after_context,
                top_k_tokens: args.top_k_tokens,
                max_count: args.max_count,
                search_mode,
            })
        };

        let ignore_files = args
            .ignore_file
            .iter()
            .map(|path| normalize_path(path, &current_dir))
            .collect();

        Ok(Self {
            index_path,
            current_dir,
            raw_search_paths: paths,
            search_paths: normalized_paths,
            used_default_search_path,
            path_filter,
            config,
            files: args.files,
            type_list: args.type_list,
            json,
            files_with_matches: args.files_with_matches,
            files_without_match: args.files_without_match,
            count: args.count,
            count_matches: args.count_matches,
            quiet: args.quiet,
            include_zero: args.include_zero && !args.no_include_zero,
            only_matching: args.only_matching,
            stats,
            print_path,
            print_line_number,
            print_column,
            display_relative_paths,
            display_dot_prefix,
            fixed_strings: args.fixed_strings,
            multiline: args.multiline,
            multiline_dotall: args.multiline_dotall,
            word_regexp: args.word_regexp,
            line_regexp: args.line_regexp,
            with_filename: args.with_filename,
            no_filename: args.no_filename,
            line_number: args.line_number,
            no_line_number: args.no_line_number,
            before_context,
            after_context,
            case_mode: resolve_ripgrep_case_mode(&args),
            include_hidden: args.hidden,
            no_ignore: args.no_ignore,
            globs: args.glob,
            iglobs: args.iglob,
            type_add: args.type_add,
            type_clear: args.type_clear,
            types: args.file_type,
            type_not: args.file_type_not,
            ignore_files,
            max_depth: args.max_depth,
            max_file_size: args.max_filesize,
            max_count: args.max_count,
        })
    }

    pub(super) fn run_ripgrep_files(&self) -> Result<Output> {
        let mut command = self.build_ripgrep_base_command();
        command.arg("--files");
        self.append_ripgrep_roots(&mut command);
        Ok(command.output()?)
    }

    pub(super) fn run_ripgrep_query(&self, config: &QueryConfig) -> Result<Output> {
        let mut command = self.build_ripgrep_base_command();
        if self.fixed_strings {
            command.arg("-F");
        }
        match self.case_mode {
            RipgrepCaseMode::Default => {}
            RipgrepCaseMode::Ignore => {
                command.arg("-i");
            }
            RipgrepCaseMode::Sensitive => {
                command.arg("-s");
            }
            RipgrepCaseMode::Smart => {
                command.arg("-S");
            }
        }
        if self.multiline {
            command.arg("-U");
        }
        if self.multiline_dotall {
            command.arg("--multiline-dotall");
        }
        if self.word_regexp {
            command.arg("-w");
        }
        if self.line_regexp {
            command.arg("-x");
        }
        if self.files_with_matches {
            command.arg("--files-with-matches");
        }
        if self.files_without_match {
            command.arg("--files-without-match");
        }
        if self.count {
            command.arg("--count");
        }
        if self.count_matches {
            command.arg("--count-matches");
        }
        if self.include_zero {
            command.arg("--include-zero");
        }
        if self.only_matching {
            command.arg("--only-matching");
        }
        if self.before_context > 0 && self.before_context == self.after_context {
            command
                .arg("--context")
                .arg(self.before_context.to_string());
        } else {
            if self.before_context > 0 {
                command
                    .arg("--before-context")
                    .arg(self.before_context.to_string());
            }
            if self.after_context > 0 {
                command
                    .arg("--after-context")
                    .arg(self.after_context.to_string());
            }
        }
        if self.with_filename {
            command.arg("--with-filename");
        }
        if self.no_filename {
            command.arg("--no-filename");
        }
        if self.line_number {
            command.arg("--line-number");
        }
        if self.no_line_number {
            command.arg("--no-line-number");
        }
        if self.print_column {
            command.arg("--column");
        }
        if self.quiet {
            command.arg("--quiet");
        }
        if self.json {
            command.arg("--json");
        }
        if self.stats {
            command.arg("--stats");
        }
        if let Some(max_count) = self.max_count {
            command.arg("--max-count").arg(max_count.to_string());
        }
        if config.patterns.is_empty() {
            command.arg("-e").arg(&config.regex_pattern);
        } else {
            for pattern in &config.patterns {
                command.arg("-e").arg(pattern);
            }
        }
        self.append_ripgrep_roots(&mut command);
        Ok(command.output()?)
    }

    fn build_ripgrep_base_command(&self) -> Command {
        let mut command = Command::new("rg");
        command.current_dir(&self.current_dir);
        command.arg("--color=never");
        if self.include_hidden {
            command.arg("--hidden");
        }
        if self.no_ignore {
            command.arg("--no-ignore");
        }
        if let Some(max_depth) = self.max_depth {
            command.arg("--max-depth").arg(max_depth.to_string());
        }
        if let Some(max_file_size) = self.max_file_size {
            command.arg("--max-filesize").arg(max_file_size.to_string());
        }
        for glob in &self.globs {
            command.arg("-g").arg(glob);
        }
        for iglob in &self.iglobs {
            command.arg("--iglob").arg(iglob);
        }
        for definition in &self.type_add {
            command.arg("--type-add").arg(definition);
        }
        for name in &self.type_clear {
            command.arg("--type-clear").arg(name);
        }
        for name in &self.types {
            command.arg("-t").arg(name);
        }
        for name in &self.type_not {
            command.arg("-T").arg(name);
        }
        for ignore_file in &self.ignore_files {
            command.arg("--ignore-file").arg(ignore_file);
        }
        command
    }

    fn append_ripgrep_roots(&self, command: &mut Command) {
        if !self.used_default_search_path {
            for root in &self.raw_search_paths {
                command.arg(root);
            }
        }
    }
}

pub(super) fn ripgrep_is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| match Command::new("rg").arg("--version").output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    })
}

fn collect_patterns(args: &SearchArgs) -> Result<Vec<String>> {
    let mut patterns = args.regexp.clone();
    for path in &args.pattern_files {
        patterns.extend(read_patterns_from_file(path)?);
    }
    if patterns.is_empty() {
        let Some(pattern) = args.pattern.clone() else {
            return Err(AppError::InvalidPattern(
                "a PATTERN, -e/--regexp or -f/--file is required".into(),
            ));
        };
        patterns.push(pattern);
    }
    Ok(patterns)
}

fn combine_patterns(patterns: &[String], fixed_strings: bool) -> String {
    if patterns.len() == 1 {
        if fixed_strings {
            return regex_syntax::escape(&patterns[0]);
        }
        return patterns[0].clone();
    }
    patterns
        .iter()
        .map(|pattern| {
            if fixed_strings {
                format!("(?:{})", regex_syntax::escape(pattern))
            } else {
                format!("(?:{pattern})")
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub(super) fn collect_search_paths(args: &SearchArgs) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if args.files || args.type_list || !args.regexp.is_empty() || !args.pattern_files.is_empty() {
        if let Some(value) = &args.pattern {
            paths.push(PathBuf::from(value));
        }
    }
    paths.extend(args.paths.iter().cloned());
    if paths.is_empty() {
        paths.push(std::env::current_dir()?);
    }
    Ok(paths)
}

fn uses_default_search_path(args: &SearchArgs) -> bool {
    let mut paths = 0usize;
    if (args.files || args.type_list || !args.regexp.is_empty() || !args.pattern_files.is_empty())
        && args.pattern.is_some()
    {
        paths += 1;
    }
    paths += args.paths.len();
    paths == 0
}

fn read_patterns_from_file(path: &Path) -> Result<Vec<String>> {
    if path == Path::new("-") {
        let stdin = io::stdin();
        return stdin
            .lock()
            .lines()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
    }

    let file = File::open(path)?;
    BufReader::new(file)
        .lines()
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn resolve_case_mode(patterns: &[String], args: &SearchArgs) -> bool {
    if args.case_sensitive > 0 {
        return false;
    }
    if args.ignore_case > 0 {
        return true;
    }
    args.smart_case > 0
        && patterns.iter().any(|pattern| !pattern.is_empty())
        && !patterns
            .iter()
            .flat_map(|pattern| pattern.chars())
            .any(char::is_uppercase)
}

fn resolve_ripgrep_case_mode(args: &SearchArgs) -> RipgrepCaseMode {
    if args.case_sensitive > 0 {
        return RipgrepCaseMode::Sensitive;
    }
    if args.ignore_case > 0 {
        return RipgrepCaseMode::Ignore;
    }
    if args.smart_case > 0 {
        return RipgrepCaseMode::Smart;
    }
    RipgrepCaseMode::Default
}

fn resolve_search_mode(args: &SearchArgs, stats: bool) -> Result<SearchMode> {
    if args.quiet && !stats {
        return Ok(SearchMode::FirstHitOnly);
    }
    if args.quiet && stats {
        return Ok(SearchMode::CountMatches);
    }
    if args.files_with_matches {
        return Ok(SearchMode::FirstHitOnly);
    }
    if args.count_matches {
        return Ok(SearchMode::CountMatches);
    }
    if args.count {
        return Ok(if args.only_matching {
            SearchMode::CountMatches
        } else {
            SearchMode::CountOnly
        });
    }
    Ok(SearchMode::MaterializeMatches)
}

pub(super) fn resolve_context_mode(args: &SearchArgs) -> (usize, usize) {
    let context = args.context.unwrap_or(0);
    (
        args.before_context.unwrap_or(context),
        args.after_context.unwrap_or(context),
    )
}

pub(super) fn default_print_path(paths: &[PathBuf]) -> bool {
    if paths.len() != 1 {
        return true;
    }
    !paths[0].is_file()
}

pub(super) fn infer_index_path(paths: &[PathBuf], current_dir: &Path) -> PathBuf {
    let root = common_search_root(paths).unwrap_or_else(|| current_dir.to_path_buf());
    root.join(".codgrep-index")
}

fn common_search_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut roots = paths
        .iter()
        .map(|path| {
            if path.is_file() {
                path.parent().unwrap_or(path).to_path_buf()
            } else {
                path.to_path_buf()
            }
        })
        .collect::<Vec<_>>();
    let mut common = roots.pop()?;
    for root in roots {
        common = common_prefix(&common, &root)?;
    }
    Some(common)
}

fn common_prefix(left: &Path, right: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for (left_component, right_component) in left.components().zip(right.components()) {
        if left_component != right_component {
            break;
        }
        prefix.push(left_component.as_os_str());
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}
