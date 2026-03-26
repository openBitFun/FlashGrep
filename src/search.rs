#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    CountOnly,
    CountMatches,
    FirstHitOnly,
    MaterializeMatches,
}

impl SearchMode {
    pub fn materializes_matches(self) -> bool {
        matches!(self, Self::MaterializeMatches | Self::FirstHitOnly)
    }
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub candidate_docs: usize,
    pub searches_with_match: usize,
    pub bytes_searched: u64,
    pub matched_lines: usize,
    pub matched_occurrences: usize,
    pub hits: Vec<SearchHit>,
}

impl SearchResults {
    pub fn has_match(&self) -> bool {
        self.matched_lines > 0 || self.matched_occurrences > 0
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub matches: Vec<FileMatch>,
    pub lines: Vec<SearchLine>,
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

#[derive(Debug, Clone)]
pub struct FileMatch {
    pub location: MatchLocation,
    pub snippet: String,
    pub matched_text: String,
}

#[derive(Debug, Clone)]
pub struct FileContext {
    pub line_number: usize,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub enum SearchLine {
    Match(FileMatch),
    Context(FileContext),
    ContextBreak,
}

#[derive(Debug, Clone)]
pub struct MatchLocation {
    pub line: usize,
    pub column: usize,
}
