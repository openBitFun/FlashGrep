use std::fs;

use bitfun_grep::{
    config::{BuildConfig, CorpusMode, QueryConfig, TokenizerMode},
    index::{build_index, IndexSearcher},
    SearchMode,
};
use tempfile::tempdir;

#[test]
fn build_config_defaults_to_sparse_ngram() {
    assert_eq!(BuildConfig::default().tokenizer, TokenizerMode::SparseNgram);
}

#[test]
fn build_and_search_repo() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    let service = repo.join("service.rs");
    let other = repo.join("other.rs");

    fs::write(
        &service,
        "fn load() {\n    let value = getUserById(id);\n}\n",
    )
    .unwrap();
    fs::write(&other, "fn noop() {\n    let value = 42;\n}\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "get(User|Account)ById".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, service.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].location.line, 2);
    assert_eq!(results.hits[0].matches[0].matched_text, "getUserById");
}

#[test]
fn case_insensitive_search_uses_folded_tokens() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    let source = repo.join("service.rs");
    fs::write(&source, "const NAME: &str = \"Pm_Resume\";\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "pm_resume".into(),
        case_insensitive: true,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "Pm_Resume");
}

#[test]
fn sparse_search_uses_indexed_tokenizer_options() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    let source = repo.join("service.rs");
    fs::write(&source, "const NAME: &str = \"abcdef\";\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 4,
        max_sparse_len: 4,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "abcdef".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "abcdef");
}

#[test]
fn impossible_literal_returns_no_candidates() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    fs::write(repo.join("service.rs"), "const NAME: &str = \"abcdef\";\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "zzzz".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.candidate_docs, 0);
    assert!(results.hits.is_empty());
}

#[test]
fn sparse_literal_query_avoids_full_scan_when_fallback_tokens_exist() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    fs::write(repo.join("match.rs"), "const NAME: &str = \"PM_RESUME\";\n").unwrap();
    fs::write(repo.join("other1.rs"), "const NAME: &str = \"PM_RESET\";\n").unwrap();
    fs::write(
        repo.join("other2.rs"),
        "const NAME: &str = \"PM_RESTORE\";\n",
    )
    .unwrap();
    fs::write(
        repo.join("other3.rs"),
        "const NAME: &str = \"HELLO_WORLD\";\n",
    )
    .unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 5,
        max_sparse_len: 16,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.hits.len(), 1);
    assert!(results.candidate_docs < 4);
}

#[test]
fn candidate_paths_exports_prefiltered_docs() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    let source = repo.join("service.rs");
    let other = repo.join("other.rs");
    fs::write(&source, "fn load() { let value = fuzzy_file_search(); }\n").unwrap();
    fs::write(&other, "fn noop() { let value = 42; }\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 5,
        max_sparse_len: 16,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "fuzzy_file_search".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };

    let candidates = searcher.candidate_paths(&query).unwrap();
    assert_eq!(candidates, vec![source.to_string_lossy().to_string()]);
}

#[test]
fn build_respects_gitignore_by_default() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir(repo.join(".git")).unwrap();

    fs::write(repo.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(
        repo.join("ignored.rs"),
        "const NAME: &str = \"PM_RESUME\";\n",
    )
    .unwrap();
    fs::write(repo.join("tracked.rs"), "const NAME: &str = \"OTHER\";\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.candidate_docs, 0);
    assert_eq!(results.matched_lines, 0);
}

#[test]
fn build_can_disable_ignore_rules() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir(repo.join(".git")).unwrap();

    fs::write(repo.join(".gitignore"), "ignored.rs\n").unwrap();
    let ignored = repo.join("ignored.rs");
    fs::write(&ignored, "const NAME: &str = \"PM_RESUME\";\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::NoIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 1);
    assert!(results.hits.is_empty());
    assert_eq!(
        searcher.candidate_paths(&query).unwrap(),
        vec![ignored.to_string_lossy().to_string()]
    );
}

#[test]
fn multiline_verifier_handles_cross_line_matches() {
    let temp = tempdir().unwrap();
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).unwrap();

    let source = repo.join("story.txt");
    fs::write(&source, "Sherlock\nHolmes\n").unwrap();

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).unwrap();

    let searcher = IndexSearcher::open(index).unwrap();
    let query = QueryConfig {
        regex_pattern: "Sherlock.*Holmes".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: true,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).unwrap();

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 2);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].matches.len(), 1);
    assert_eq!(results.hits[0].matches[0].location.line, 1);
    assert_eq!(results.hits[0].matches[0].matched_text, "Sherlock\nHolmes");
}
