#![allow(clippy::expect_used)]

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
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let service = repo.join("service.rs");
    let other = repo.join("other.rs");

    fs::write(
        &service,
        "fn load() {\n    let value = getUserById(id);\n}\n",
    )
    .expect("test should succeed");
    fs::write(&other, "fn noop() {\n    let value = 42;\n}\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "get(User|Account)ById".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, service.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].location.line, 2);
    assert_eq!(results.hits[0].matches[0].matched_text, "getUserById");
}

#[test]
fn case_insensitive_search_uses_folded_tokens() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("service.rs");
    fs::write(&source, "const NAME: &str = \"Pm_Resume\";\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "pm_resume".into(),
        case_insensitive: true,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "Pm_Resume");
}

#[test]
fn sparse_search_uses_indexed_tokenizer_options() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("service.rs");
    fs::write(&source, "const NAME: &str = \"abcdef\";\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 4,
        max_sparse_len: 4,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "abcdef".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "abcdef");
}

#[test]
fn impossible_literal_returns_no_candidates() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    fs::write(repo.join("service.rs"), "const NAME: &str = \"abcdef\";\n")
        .expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "zzzz".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 0);
    assert!(results.hits.is_empty());
}

#[test]
fn sparse_literal_query_avoids_full_scan_when_fallback_tokens_exist() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    fs::write(repo.join("match.rs"), "const NAME: &str = \"PM_RESUME\";\n")
        .expect("test should succeed");
    fs::write(repo.join("other1.rs"), "const NAME: &str = \"PM_RESET\";\n")
        .expect("test should succeed");
    fs::write(
        repo.join("other2.rs"),
        "const NAME: &str = \"PM_RESTORE\";\n",
    )
    .expect("test should succeed");
    fs::write(
        repo.join("other3.rs"),
        "const NAME: &str = \"HELLO_WORLD\";\n",
    )
    .expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 5,
        max_sparse_len: 16,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert!(results.candidate_docs < 4);
}

#[test]
fn pure_literal_alternation_uses_exact_multi_literal_scan() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let err_sys = repo.join("err.rs");
    let turn_off = repo.join("turn_off.rs");
    let link_rst = repo.join("link.rs");
    let cfg_evt = repo.join("cfg.rs");
    let other = repo.join("other.rs");

    fs::write(&err_sys, "const NAME: &str = \"ERR_SYS\";\n").expect("test should succeed");
    fs::write(&turn_off, "const NAME: &str = \"PME_TURN_OFF\";\n").expect("test should succeed");
    fs::write(&link_rst, "const NAME: &str = \"LINK_REQ_RST\";\n").expect("test should succeed");
    fs::write(&cfg_evt, "const NAME: &str = \"CFG_BME_EVT\";\n").expect("test should succeed");
    fs::write(&other, "const NAME: &str = \"UNRELATED\";\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 5,
        max_sparse_len: 16,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 4);
    assert_eq!(results.hits.len(), 4);
    assert!(results
        .hits
        .iter()
        .all(|hit| hit.path != other.to_string_lossy()));
}

#[test]
fn pure_literal_alternation_respects_leftmost_first_order() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("service.rs");
    fs::write(&source, "const NAME: &str = \"ab\";\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "a|ab".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].matches.len(), 1);
    assert_eq!(results.hits[0].matches[0].matched_text, "a");
}

#[test]
fn trigram_pure_literal_alternation_respects_case_sensitive_index_prefilter() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("service.rs");
    fs::write(
        &source,
        "const A: &str = \"ERR_SYS\";\nconst B: &str = \"CFG_BME_EVT\";\n",
    )
    .expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 2);
}

#[test]
fn candidate_paths_exports_prefiltered_docs() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("service.rs");
    let other = repo.join("other.rs");
    fs::write(&source, "fn load() { let value = fuzzy_file_search(); }\n")
        .expect("test should succeed");
    fs::write(&other, "fn noop() { let value = 42; }\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 5,
        max_sparse_len: 16,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "fuzzy_file_search".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };

    let candidates = searcher
        .candidate_paths(&query)
        .expect("test should succeed");
    assert_eq!(candidates, vec![source.to_string_lossy().to_string()]);
}

#[test]
fn build_respects_gitignore_by_default() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");
    fs::create_dir(repo.join(".git")).expect("test should succeed");

    fs::write(repo.join(".gitignore"), "ignored.rs\n").expect("test should succeed");
    fs::write(
        repo.join("ignored.rs"),
        "const NAME: &str = \"PM_RESUME\";\n",
    )
    .expect("test should succeed");
    fs::write(repo.join("tracked.rs"), "const NAME: &str = \"OTHER\";\n")
        .expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 0);
    assert_eq!(results.matched_lines, 0);
}

#[test]
fn build_can_disable_ignore_rules() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");
    fs::create_dir(repo.join(".git")).expect("test should succeed");

    fs::write(repo.join(".gitignore"), "ignored.rs\n").expect("test should succeed");
    let ignored = repo.join("ignored.rs");
    fs::write(&ignored, "const NAME: &str = \"PM_RESUME\";\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::NoIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "PM_RESUME".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 1);
    assert!(results.hits.is_empty());
    assert_eq!(
        searcher
            .candidate_paths(&query)
            .expect("test should succeed"),
        vec![ignored.to_string_lossy().to_string()]
    );
}

#[test]
fn multiline_verifier_handles_cross_line_matches() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("story.txt");
    fs::write(&source, "Sherlock\nHolmes\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "Sherlock.*Holmes".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: true,
        top_k_tokens: 6,
        search_mode: SearchMode::MaterializeMatches,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 2);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].matches.len(), 1);
    assert_eq!(results.hits[0].matches[0].location.line, 1);
    assert_eq!(results.hits[0].matches[0].matched_text, "Sherlock\nHolmes");
}

#[test]
fn default_search_does_not_span_lines_for_whitespace_classes() {
    let temp = tempdir().expect("test should succeed");
    let repo = temp.path().join("repo");
    let index = repo.join(".bitfun-index");
    fs::create_dir_all(&repo).expect("test should succeed");

    let source = repo.join("story.txt");
    fs::write(&source, "Sherlock\nHolmes\n").expect("test should succeed");

    let build = BuildConfig {
        repo_path: repo.clone(),
        index_path: index.clone(),
        tokenizer: TokenizerMode::Trigram,
        corpus_mode: CorpusMode::RespectIgnore,
        max_file_size: 1024 * 1024,
        min_sparse_len: 3,
        max_sparse_len: 32,
    };
    build_index(&build).expect("test should succeed");

    let searcher = IndexSearcher::open(index).expect("test should succeed");
    let query = QueryConfig {
        regex_pattern: "Sherlock\\s+Holmes".into(),
        case_insensitive: false,
        multiline: false,
        dot_matches_new_line: false,
        top_k_tokens: 6,
        search_mode: SearchMode::CountOnly,
    };
    let results = searcher.search(&query).expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 0);
    assert!(results.hits.is_empty());
}
