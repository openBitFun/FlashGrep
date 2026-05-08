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
    pub file_counts: Vec<FileCount>,
    pub file_match_counts: Vec<FileMatchCount>,
}

impl SearchResults {
    pub fn has_match(&self) -> bool {
        self.matched_lines > 0 || self.matched_occurrences > 0
    }

    pub fn result_units(&self, mode: SearchMode) -> usize {
        match mode {
            SearchMode::LineMatches => self.line_matches.len(),
            SearchMode::FilesWithMatches | SearchMode::CountOnly | SearchMode::CountMatches => {
                self.searches_with_match
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LineMatch {
    pub path: String,
    pub line_number: usize,
    pub line_text: String,
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
