use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    thread,
    time::{Duration, SystemTime},
};

use bitfun_grep::{
    index::format::{read_doc_terms_file, read_docs_file, IndexLayout},
    BuildConfig, QueryConfig, SearchMode, TokenizerMode,
};

use crate::common::{count_query, query, TestRepo};

fn rewrite_with_fresh_mtime(path: &Path, contents: &str) {
    let before = fs::metadata(path)
        .expect("test should succeed")
        .modified()
        .expect("test should succeed");

    for _ in 0..20 {
        thread::sleep(Duration::from_millis(10));
        fs::write(path, contents).expect("test should succeed");

        let after = fs::metadata(path)
            .expect("test should succeed")
            .modified()
            .expect("test should succeed");
        if after > before {
            return;
        }
    }

    panic!(
        "mtime did not advance after rewriting {} from {:?}",
        path.display(),
        SystemTime::now()
    );
}

#[test]
fn build_config_defaults_to_sparse_ngram() {
    assert_eq!(BuildConfig::default().tokenizer, TokenizerMode::SparseNgram);
}

#[test]
fn build_and_search_repo() {
    let repo = TestRepo::new();
    let service = repo.write(
        "service.rs",
        "fn load() {\n    let value = getUserById(id);\n}\n",
    );
    repo.write("other.rs", "fn noop() {\n    let value = 42;\n}\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&query("get(User|Account)ById"))
        .expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, service.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].location.line, 2);
    assert_eq!(results.hits[0].matches[0].matched_text, "getUserById");
}

#[test]
fn incremental_build_detects_same_size_rewrite_and_reuses_unchanged_doc_terms() {
    let repo = TestRepo::new();
    let stable = repo.write("stable.rs", "const NAME: &str = \"BETA\";\n");
    let rewritten = repo.write("rewritten.rs", "const NAME: &str = \"ALPHA\";\n");

    let build = repo.trigram_build_config();
    repo.build_with(build.clone());

    let layout = IndexLayout::resolve(&repo.index).expect("test should succeed");
    let (_, docs_before) = read_docs_file(&layout.docs_path).expect("test should succeed");
    let terms_before = read_doc_terms_file(&layout.doc_terms_path).expect("test should succeed");
    let before = docs_before
        .into_iter()
        .zip(terms_before)
        .map(|(doc, token_hashes)| (doc.path, token_hashes))
        .collect::<BTreeMap<_, _>>();

    rewrite_with_fresh_mtime(&rewritten, "const NAME: &str = \"OMEGA\";\n");
    repo.build_with(build);

    let searcher = repo.searcher();
    let alpha = searcher
        .search(&count_query("ALPHA"))
        .expect("test should succeed");
    assert_eq!(alpha.candidate_docs, 0);
    assert_eq!(alpha.matched_lines, 0);

    let omega = searcher
        .search(&count_query("OMEGA"))
        .expect("test should succeed");
    assert_eq!(omega.candidate_docs, 1);
    assert_eq!(omega.matched_lines, 1);

    let layout = IndexLayout::resolve(&repo.index).expect("test should succeed");
    let (_, docs_after) = read_docs_file(&layout.docs_path).expect("test should succeed");
    let terms_after = read_doc_terms_file(&layout.doc_terms_path).expect("test should succeed");
    let after = docs_after
        .into_iter()
        .zip(terms_after)
        .map(|(doc, token_hashes)| (doc.path, token_hashes))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        before[stable.to_string_lossy().as_ref()],
        after[stable.to_string_lossy().as_ref()]
    );
    assert_ne!(
        before[rewritten.to_string_lossy().as_ref()],
        after[rewritten.to_string_lossy().as_ref()]
    );
    assert_eq!(after.len(), 2);
}

#[test]
fn build_writes_current_generation_pointer() {
    let repo = TestRepo::new();
    repo.write("service.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    let current_path = IndexLayout::current_path(&repo.index);
    let generation = std::fs::read_to_string(&current_path).expect("test should succeed");
    let generation = generation.trim();
    assert!(!generation.is_empty());

    let layout = IndexLayout::resolve(&repo.index).expect("test should succeed");
    assert_ne!(layout.data_path, repo.index);
    assert!(layout.docs_path.exists());
    assert_eq!(
        layout.data_path,
        IndexLayout::generations_dir(&repo.index).join(generation)
    );
}

#[test]
fn case_insensitive_search_uses_folded_tokens() {
    let repo = TestRepo::new();
    let source = repo.write("service.rs", "const NAME: &str = \"Pm_Resume\";\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&QueryConfig {
            case_insensitive: true,
            ..query("pm_resume")
        })
        .expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "Pm_Resume");
}

#[test]
fn sparse_search_uses_indexed_tokenizer_options() {
    let repo = TestRepo::new();
    let source = repo.write("service.rs", "const NAME: &str = \"abcdef\";\n");

    repo.build_with(BuildConfig {
        min_sparse_len: 4,
        max_sparse_len: 4,
        ..repo.sparse_build_config()
    });

    let results = repo
        .searcher()
        .search(&query("abcdef"))
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].path, source.to_string_lossy());
    assert_eq!(results.hits[0].matches[0].matched_text, "abcdef");
}

#[test]
fn impossible_literal_returns_no_candidates() {
    let repo = TestRepo::new();
    repo.write("service.rs", "const NAME: &str = \"abcdef\";\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&query("zzzz"))
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 0);
    assert!(results.hits.is_empty());
}

#[test]
fn sparse_literal_query_avoids_full_scan_when_fallback_tokens_exist() {
    let repo = TestRepo::new();
    repo.write("match.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write("other1.rs", "const NAME: &str = \"PM_RESET\";\n");
    repo.write("other2.rs", "const NAME: &str = \"PM_RESTORE\";\n");
    repo.write("other3.rs", "const NAME: &str = \"HELLO_WORLD\";\n");

    repo.build_sparse();

    let results = repo
        .searcher()
        .search(&query("PM_RESUME"))
        .expect("test should succeed");

    assert_eq!(results.hits.len(), 1);
    assert!(results.candidate_docs < 4);
}

#[test]
fn pure_literal_alternation_uses_exact_multi_literal_scan() {
    let repo = TestRepo::new();
    let other = repo.write("other.rs", "const NAME: &str = \"UNRELATED\";\n");
    repo.write("err.rs", "const NAME: &str = \"ERR_SYS\";\n");
    repo.write("turn_off.rs", "const NAME: &str = \"PME_TURN_OFF\";\n");
    repo.write("link.rs", "const NAME: &str = \"LINK_REQ_RST\";\n");
    repo.write("cfg.rs", "const NAME: &str = \"CFG_BME_EVT\";\n");

    repo.build_sparse();

    let results = repo
        .searcher()
        .search(&query("ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT"))
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 4);
    assert_eq!(results.hits.len(), 4);
    assert!(results
        .hits
        .iter()
        .all(|hit| hit.path != other.to_string_lossy()));
}

#[test]
fn pure_literal_alternation_respects_leftmost_first_order() {
    let repo = TestRepo::new();
    repo.write("service.rs", "const NAME: &str = \"ab\";\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&query("a|ab"))
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].matches.len(), 1);
    assert_eq!(results.hits[0].matches[0].matched_text, "a");
}

#[test]
fn trigram_pure_literal_alternation_respects_case_sensitive_index_prefilter() {
    let repo = TestRepo::new();
    repo.write(
        "service.rs",
        "const A: &str = \"ERR_SYS\";\nconst B: &str = \"CFG_BME_EVT\";\n",
    );

    repo.build();

    let results = repo
        .searcher()
        .search(&count_query(
            "ERR_SYS|PME_TURN_OFF|LINK_REQ_RST|CFG_BME_EVT",
        ))
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 2);
}

#[test]
fn multiline_verifier_handles_cross_line_matches() {
    let repo = TestRepo::new();
    repo.write("story.txt", "Sherlock\nHolmes\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&QueryConfig {
            dot_matches_new_line: true,
            ..query("Sherlock.*Holmes")
        })
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 2);
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].matches.len(), 1);
    assert_eq!(results.hits[0].matches[0].location.line, 1);
    assert_eq!(results.hits[0].matches[0].matched_text, "Sherlock\nHolmes");
}

#[test]
fn default_search_does_not_span_lines_for_whitespace_classes() {
    let repo = TestRepo::new();
    repo.write("story.txt", "Sherlock\nHolmes\n");

    repo.build();

    let results = repo
        .searcher()
        .search(&QueryConfig {
            search_mode: SearchMode::CountOnly,
            ..query("Sherlock\\s+Holmes")
        })
        .expect("test should succeed");

    assert_eq!(results.candidate_docs, 1);
    assert_eq!(results.matched_lines, 0);
    assert!(results.hits.is_empty());
}
