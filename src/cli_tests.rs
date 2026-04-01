use clap::Parser;

use super::{
    collect_search_paths, default_print_path, infer_index_path, resolve_context_mode, Cli,
    SearchArgs, SearchRequest,
};

#[test]
fn parse_root_search_request() {
    let cli = Cli::try_parse_from(["cg", "-n", "--column", "needle", "src"])
        .expect("test should succeed");

    assert!(cli.command.is_none());
    assert_eq!(cli.search.pattern.as_deref(), Some("needle"));
    assert_eq!(cli.search.paths, vec![std::path::PathBuf::from("src")]);
    assert!(cli.search.line_number);
    assert!(cli.search.column);
}

#[test]
fn regexp_turns_positional_pattern_into_path() {
    let args = SearchArgs {
        regexp: vec!["needle".into()],
        pattern: Some("src".into()),
        paths: vec![std::path::PathBuf::from("tests")],
        ..SearchArgs::default()
    };

    let paths = collect_search_paths(&args).expect("test should succeed");
    assert_eq!(
        paths,
        vec![
            std::path::PathBuf::from("src"),
            std::path::PathBuf::from("tests")
        ]
    );
}

#[test]
fn infer_index_path_uses_common_search_root() {
    let cwd = std::path::PathBuf::from("/repo");
    let paths = vec![
        std::path::PathBuf::from("/repo/src"),
        std::path::PathBuf::from("/repo/tests"),
    ];

    let index = infer_index_path(&paths, &cwd);
    assert_eq!(index, std::path::PathBuf::from("/repo/.codgrep-index"));
}

#[test]
fn single_file_search_omits_path_prefix_by_default() {
    assert!(!default_print_path(&[std::path::PathBuf::from(
        "src/main.rs"
    )]));
    assert!(default_print_path(&[std::path::PathBuf::from("src")]));
}

#[test]
fn files_mode_treats_positional_as_path() {
    let args = SearchArgs {
        files: true,
        pattern: Some("src".into()),
        ..SearchArgs::default()
    };

    let paths = collect_search_paths(&args).expect("test should succeed");
    assert_eq!(paths, vec![std::path::PathBuf::from("src")]);
}

#[test]
fn parse_files_mode_without_pattern_requirement() {
    let cli = Cli::try_parse_from(["cg", "--files", "src"]).expect("test should succeed");

    assert!(cli.command.is_none());
    assert!(cli.search.files);
    assert_eq!(cli.search.pattern.as_deref(), Some("src"));
}

#[test]
fn json_conflicts_with_summary_modes() {
    let args = SearchArgs {
        json: true,
        count: true,
        pattern: Some("needle".into()),
        ..SearchArgs::default()
    };

    let error = SearchRequest::from_args(args).expect_err("test should fail");
    assert!(error.to_string().contains("--json cannot be combined with"));
}

#[test]
fn no_json_reverts_to_standard_mode() {
    let args = SearchArgs {
        json: true,
        no_json: true,
        count: true,
        pattern: Some("needle".into()),
        ..SearchArgs::default()
    };

    let request = SearchRequest::from_args(args).expect("test should succeed");
    assert!(!request.json);
    assert!(request.count);
}

#[test]
fn before_and_after_context_override_context_flag() {
    let args = SearchArgs {
        context: Some(3),
        before_context: Some(1),
        after_context: Some(2),
        ..SearchArgs::default()
    };

    assert_eq!(resolve_context_mode(&args), (1, 2));
}

#[test]
fn case_mode_flags_conflict_like_ripgrep() {
    let error = Cli::try_parse_from(["cg", "--smart-case", "--case-sensitive", "test"])
        .expect_err("test should fail");
    assert!(error.to_string().contains("--case-sensitive"));

    let error = Cli::try_parse_from(["cg", "--ignore-case", "--case-sensitive", "test"])
        .expect_err("test should fail");
    assert!(error.to_string().contains("--case-sensitive"));
}
