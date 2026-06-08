#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    FilesWithMatches,
    LineMatches,
    CountOnly,
    CountMatches,
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub candidate_docs: usize,
    pub searches_with_match: usize,
    pub bytes_searched: u64,
    pub matched_lines: usize,
    pub matched_occurrences: usize,
    pub matched_paths: Vec<String>,
    pub line_matches: Vec<LineMatch>,
    pub line_match_files: Vec<LineMatchFile>,
    pub file_counts: Vec<FileCount>,
    pub file_match_counts: Vec<FileMatchCount>,
}

impl SearchResults {
    pub fn has_match(&self) -> bool {
        self.matched_lines > 0 || self.matched_occurrences > 0
    }

    pub fn result_units(&self, mode: SearchMode) -> usize {
        match mode {
            SearchMode::LineMatches => self.line_match_count(),
            SearchMode::FilesWithMatches | SearchMode::CountOnly | SearchMode::CountMatches => {
                self.searches_with_match
            }
        }
    }

    pub fn line_match_count(&self) -> usize {
        if self.line_match_files.is_empty() {
            self.line_matches.len()
        } else {
            self.line_match_files
                .iter()
                .map(|file| file.line_numbers.len())
                .sum()
        }
    }

    pub fn flattened_line_matches(&self) -> Vec<LineMatch> {
        if self.line_match_files.is_empty() {
            return self.line_matches.clone();
        }
        let mut matches = Vec::with_capacity(self.line_match_count());
        for file in &self.line_match_files {
            matches.extend(file.line_numbers.iter().map(|line_number| LineMatch {
                path: file.path.clone(),
                line_number: *line_number,
            }));
        }
        matches
    }
}

#[derive(Debug, Clone)]
pub struct LineMatch {
    pub path: String,
    pub line_number: usize,
}

#[derive(Debug, Clone)]
pub struct LineMatchFile {
    pub path: String,
    pub line_numbers: Vec<usize>,
}

pub(crate) fn append_line_match_file(
    output: &mut Vec<LineMatchFile>,
    path: String,
    matches: Vec<LineMatch>,
) {
    if matches.is_empty() {
        return;
    }
    output.push(LineMatchFile {
        path,
        line_numbers: matches
            .into_iter()
            .map(|matched| matched.line_number)
            .collect(),
    });
}

#[derive(Debug, Clone)]
pub struct FileCount {
    pub path: String,
    pub matched_lines: usize,
}

impl FileCount {
    pub fn has_match(&self) -> bool {
        self.matched_lines > 0
    }
}

#[derive(Debug, Clone)]
pub struct FileMatchCount {
    pub path: String,
    pub matched_occurrences: usize,
}

impl FileMatchCount {
    pub fn has_match(&self) -> bool {
        self.matched_occurrences > 0
    }
}
