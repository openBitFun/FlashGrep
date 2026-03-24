#[derive(Debug, Clone)]
pub struct SearchResults {
    pub candidate_docs: usize,
    pub hits: Vec<SearchHit>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub matches: Vec<FileMatch>,
}

#[derive(Debug, Clone)]
pub struct FileMatch {
    pub location: MatchLocation,
    pub snippet: String,
    pub matched_text: String,
}

#[derive(Debug, Clone)]
pub struct MatchLocation {
    pub line: usize,
    pub column: usize,
}
