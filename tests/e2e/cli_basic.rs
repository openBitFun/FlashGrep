use crate::{
    cli_support::{
        assert_cli_matches_rg, assert_cli_matches_rg_sorted, normalized_stats_output, run_cli,
        run_rg,
    },
    common::{sort_lines, stderr_string, stdout_string, TestRepo, SHERLOCK},
};

#[test]
fn cli_files_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file", "");
    repo.write("dir/file", "");

    repo.build();

    let codgrep = repo
        .cli()
        .arg("--files")
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("--files")
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        crate::common::sort_lines(&stdout_string(&codgrep.stdout)),
        crate::common::sort_lines(&stdout_string(&rg.stdout))
    );
}

#[test]
fn cli_files_relative_root_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("dir/abc", "");
    repo.write("foo/abc", "");

    repo.build();

    let codgrep = repo
        .cli()
        .args(["--files", "foo"])
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["--files", "foo"])
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        crate::common::sort_lines(&stdout_string(&codgrep.stdout)),
        crate::common::sort_lines(&stdout_string(&rg.stdout))
    );
}

#[test]
fn cli_files_dot_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file", "");
    repo.write("dir/file", "");

    repo.build();

    let codgrep = repo
        .cli()
        .args(["--files", "."])
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["--files", "."])
        .current_dir(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        crate::common::sort_lines(&stdout_string(&codgrep.stdout)),
        crate::common::sort_lines(&stdout_string(&rg.stdout))
    );
}

#[test]
fn cli_type_list_prints_non_empty_output() {
    let repo = TestRepo::new();
    repo.build();

    let output = repo
        .cli()
        .arg("--type-list")
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    assert!(!stdout_string(&output.stdout).is_empty());
}

#[test]
fn cli_ripgrep_misc_single_file() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_dir() {
    let repo = TestRepo::new();
    repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["Sherlock"], &repo.repo);
}

#[test]
fn cli_ripgrep_misc_line_numbers() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_columns() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--column", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_with_filename() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-H", "Sherlock"], &source);
}

#[test]
fn cli_returns_ripgrep_style_exit_codes() {
    let repo = TestRepo::new();
    repo.write("match.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    let matched = repo
        .cli()
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    assert!(matched.status.success());

    let missed = repo
        .cli()
        .arg("DOES_NOT_EXIST")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    assert_eq!(missed.status.code(), Some(1));
}

#[test]
fn cli_supports_ripgrep_files_without_match_mode() {
    let repo = TestRepo::new();
    repo.write("matched.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let missed = repo.write("missed.rs", "const NAME: &str = \"OTHER\";\n");

    repo.build();

    let output = repo
        .cli()
        .arg("--files-without-match")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    assert_eq!(
        stdout_string(&output.stdout),
        format!("{}\n", missed.to_string_lossy())
    );
}

#[test]
fn cli_reserves_l_for_follow_like_ripgrep() {
    let repo = TestRepo::new();
    repo.write("matched.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    let output = repo
        .cli()
        .arg("-L")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_string(&output.stderr).contains("--follow is not supported yet"));
}

#[test]
fn cli_supports_count_matches_mode() {
    let repo = TestRepo::new();
    let source = repo.write("matches.rs", "PM_RESUME PM_RESUME\nPM_RESUME\n");

    repo.build();

    let output = repo
        .cli()
        .arg("--count-matches")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    assert_eq!(
        stdout_string(&output.stdout),
        format!("{}:3\n", source.to_string_lossy())
    );
}

#[test]
fn cli_sparse_short_literal_count_falls_back_to_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/first.td", "defm Foo : Something;\n");
    repo.write(
        "defs/second.td",
        "defm Bar : Something;\ndefm Baz : Something;\n",
    );
    repo.write("src/other.rs", "const WORD: &str = \"defm\";\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-F", "-c", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["-F", "-c", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        sort_lines(&stdout_string(&codgrep.stdout)),
        sort_lines(&stdout_string(&rg.stdout))
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_sparse_short_literal_standard_search_matches_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");
    repo.write("defs/missed.td", "def Missing;\n");
    repo.write("src/ignored.rs", "const WORD: &str = \"defm\";\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-F", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["-F", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        sort_lines(&stdout_string(&codgrep.stdout)),
        sort_lines(&stdout_string(&rg.stdout))
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_sparse_short_literal_files_with_matches_fall_back_to_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");
    repo.write("defs/missed.td", "def Missing;\n");
    repo.write("src/matched.rs", "const WORD: &str = \"defm\";\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-F", "-l", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["-F", "-l", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        sort_lines(&stdout_string(&codgrep.stdout)),
        sort_lines(&stdout_string(&rg.stdout))
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_sparse_short_literal_files_without_match_fall_back_to_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");
    repo.write("defs/missed.td", "def Missing;\n");
    repo.write("src/ignored.rs", "const WORD: &str = \"defm\";\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-F", "--files-without-match", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["-F", "--files-without-match", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        sort_lines(&stdout_string(&codgrep.stdout)),
        sort_lines(&stdout_string(&rg.stdout))
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_sparse_short_literal_quiet_falls_back_to_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");

    repo.build_sparse();

    let matched = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-q", "-F", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg_matched = repo
        .rg()
        .arg("--color=never")
        .args(["-q", "-F", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(matched.status.code(), rg_matched.status.code());
    assert!(stderr_string(&matched.stderr).contains("fallback rg=true"));

    let missed = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-q", "-F", "none"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg_missed = repo
        .rg()
        .arg("--color=never")
        .args(["-q", "-F", "none"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(missed.status.code(), rg_missed.status.code());
    assert!(stderr_string(&missed.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_supports_include_zero_for_summary_modes() {
    let repo = TestRepo::new();
    let matched = repo.write("matched.rs", "PM_RESUME PM_RESUME\n");
    let missed = repo.write("missed.rs", "OTHER\n");

    repo.build();

    let count_output = repo
        .cli()
        .arg("-c")
        .arg("--include-zero")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    assert!(count_output.status.success());
    assert_eq!(
        stdout_string(&count_output.stdout),
        format!(
            "{}:1\n{}:0\n",
            matched.to_string_lossy(),
            missed.to_string_lossy()
        )
    );

    let total_output = repo
        .cli()
        .arg("--count-matches")
        .arg("--include-zero")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    assert!(total_output.status.success());
    assert_eq!(
        stdout_string(&total_output.stdout),
        format!(
            "{}:2\n{}:0\n",
            matched.to_string_lossy(),
            missed.to_string_lossy()
        )
    );
}

#[test]
fn cli_ripgrep_misc_quiet() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    let output = repo
        .cli()
        .args(["-q", "Sherlock"])
        .arg(&source)
        .output()
        .expect("test should succeed");
    assert!(output.stdout.is_empty());
    assert!(output.status.success());
}

#[test]
fn cli_no_filename_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--no-filename", "Sherlock"], &source);
}

#[test]
fn cli_fixed_strings_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("query.txt", "a+b?\na+b?\na-b\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-F", "a+b?"], &source);
}

#[test]
fn cli_hidden_matches_ripgrep_output_when_index_supports_it() {
    let repo = TestRepo::new();
    repo.write(".hidden.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build_with(codgrep::BuildConfig {
        include_hidden: true,
        ..repo.trigram_build_config()
    });

    assert_cli_matches_rg(&repo, &["--hidden", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_no_ignore_matches_ripgrep_output_when_index_supports_it() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "ignored.rs\n");
    repo.write("ignored.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build_with(codgrep::BuildConfig {
        corpus_mode: codgrep::CorpusMode::NoIgnore,
        ..repo.trigram_build_config()
    });

    assert_cli_matches_rg(&repo, &["--no-ignore", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_hidden_falls_back_to_scan_when_index_is_not_hidden_capable() {
    let repo = TestRepo::new();
    repo.write(".hidden.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--hidden", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_ignore_file_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write("ignored.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let ignore = repo.write("extra.ignore", "ignored.rs\n");

    repo.build();

    let ignore_path = ignore.to_string_lossy().into_owned();
    assert_cli_matches_rg(
        &repo,
        &["--ignore-file", ignore_path.as_str(), "PM_RESUME"],
        &repo.repo,
    );
}

#[test]
fn cli_no_ignore_still_respects_ignore_file() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "gitignored.rs\n");
    repo.write("gitignored.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    let ignore = repo.write("extra.ignore", "tracked.rs\n");

    repo.build_with(codgrep::BuildConfig {
        corpus_mode: codgrep::CorpusMode::NoIgnore,
        ..repo.trigram_build_config()
    });

    let ignore_path = ignore.to_string_lossy().into_owned();
    assert_cli_matches_rg(
        &repo,
        &[
            "--no-ignore",
            "--ignore-file",
            ignore_path.as_str(),
            "PM_RESUME",
        ],
        &repo.repo,
    );
}

#[test]
fn cli_no_ignore_falls_back_to_scan_when_index_is_not_no_ignore_capable() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "ignored.rs\n");
    repo.write("ignored.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--no-ignore", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_max_depth_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("top.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write("nested/deep.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();

    assert_cli_matches_rg_sorted(&repo, &["--max-depth", "1", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_max_filesize_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("small.rs", "PM_RESUME\n");
    repo.write("large.rs", "PM_RESUME\nIGNORED\n");

    repo.build();

    assert_cli_matches_rg_sorted(&repo, &["--max-filesize", "10", "PM_RESUME"], &repo.repo);
}

#[test]
fn cli_uses_index_without_checking_freshness() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    repo.build();
    repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let output = repo
        .cli()
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_string(&output.stdout);
    assert!(stdout.contains("tracked.rs"));
    assert!(!stdout.contains("new.rs"));
}

#[test]
fn cli_count_uses_index_without_checking_freshness() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.build();
    repo.write("new.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let output = repo
        .cli()
        .args(["--count", "PM_RESUME"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_string(&output.stdout);
    assert!(stdout.contains("tracked.rs:1"));
    assert!(!stdout.contains("new.rs"));
}

#[test]
fn cli_falls_back_to_scan_when_index_is_missing() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "const NAME: &str = \"PM_RESUME\";\n");

    let output = repo
        .cli()
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout_string(&output.stdout).contains("tracked.rs"));
}

#[test]
fn cli_smart_case_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-S", "sherlock"], &source);
}

#[test]
fn cli_word_regexp_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("tokens.txt", "foo\nfoobar\nbar foo baz\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-w", "foo"], &source);
}

#[test]
fn cli_ripgrep_misc_word_period() {
    let repo = TestRepo::new();
    let source = repo.write("haystack", "...");

    repo.build();

    assert_cli_matches_rg(&repo, &["-o", "-w", "."], &source);
}

#[test]
fn cli_line_regexp_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("lines.txt", "foo\nfoo bar\nbar foo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-x", "foo"], &source);
}

#[test]
fn cli_only_matching_with_line_and_column_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-o", "--column", "-n", "Sherlock"], &source);
}

#[test]
fn cli_only_matching_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "foo foo\nbar\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-o", "foo"], &source);
}

#[test]
fn cli_max_count_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "foo\nfoo foo\nfoo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-m", "2", "foo"], &source);
}

#[test]
fn cli_ripgrep_feature_f159_max_count_zero() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "foo\nfoo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-m", "0", "foo"], &source);
}

#[test]
fn cli_pattern_file_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let patterns = repo.write("patterns.txt", "alpha\nbeta\n");
    let source = repo.write("matches.txt", "alpha\nbeta\ngamma\n");

    repo.build();

    let pattern_path = patterns.to_string_lossy().into_owned();
    assert_cli_matches_rg(
        &repo,
        &["-n", "--column", "-H", "-f", pattern_path.as_str()],
        &source,
    );
}

#[test]
fn cli_count_with_only_matching_behaves_like_ripgrep() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "foo foo\nfoo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-c", "-o", "-H", "foo"], &source);
}

#[test]
fn cli_ripgrep_misc_count() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--count", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_count_matches() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--count-matches", "the"], &source);
}

#[test]
fn cli_ripgrep_misc_count_matches_via_only() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--count", "--only-matching", "the"], &source);
}

#[test]
fn cli_ripgrep_misc_include_zero() {
    let repo = TestRepo::new();
    repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--count", "--include-zero", "nada"], &repo.repo);
}

#[test]
fn cli_ripgrep_misc_include_zero_override() {
    let repo = TestRepo::new();
    repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(
        &repo,
        &["--count", "--include-zero", "--no-include-zero", "nada"],
        &repo.repo,
    );
}

#[test]
fn cli_ripgrep_misc_files_with_matches() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["--files-with-matches", "Sherlock"], &source);
}

#[test]
fn cli_stats_matches_ripgrep_aggregate_lines() {
    let repo = TestRepo::new();
    let source = repo.write("sample.txt", "foo\nfoo bar\nbar\n");

    repo.build();

    let codgrep = run_cli(&repo, &["-H", "--stats", "foo"], &source);
    let rg = run_rg(&repo, &["-H", "--stats", "foo"], &source);

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        normalized_stats_output(&codgrep.stdout),
        normalized_stats_output(&rg.stdout)
    );
}

#[test]
fn cli_quiet_stats_matches_ripgrep_aggregate_lines() {
    let repo = TestRepo::new();
    let source = repo.write("sample.txt", "foo\nfoo\n");

    repo.build();

    let codgrep = run_cli(&repo, &["-q", "--stats", "foo"], &source);
    let rg = run_rg(&repo, &["-q", "--stats", "foo"], &source);

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        normalized_stats_output(&codgrep.stdout),
        normalized_stats_output(&rg.stdout)
    );
}

#[test]
fn cli_ripgrep_feature_f243_column_line() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "test");

    repo.build();

    assert_cli_matches_rg(&repo, &["--column", "test"], &source);
}

#[test]
fn cli_count_stats_matches_ripgrep_aggregate_lines() {
    let repo = TestRepo::new();
    let source = repo.write("sample.txt", "foo\nfoo\n");

    repo.build();

    let codgrep = run_cli(&repo, &["-c", "--stats", "-H", "foo"], &source);
    let rg = run_rg(&repo, &["-c", "--stats", "-H", "foo"], &source);

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        normalized_stats_output(&codgrep.stdout),
        normalized_stats_output(&rg.stdout)
    );
}

#[test]
fn cli_sparse_short_literal_stats_fall_back_to_ripgrep() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");
    repo.write("defs/other.td", "defm Other : Something;\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["-F", "--stats", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["-F", "--stats", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        normalized_stats_output(&codgrep.stdout),
        normalized_stats_output(&rg.stdout)
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}

#[test]
fn cli_max_depth_scope_resolution_falls_back_to_ripgrep() {
    let repo = TestRepo::new();
    let matched = repo.write("matched.rs", "const NAME: &str = \"PM_RESUME\";\n");
    repo.write(
        "nested/deeper/ignored.rs",
        "const NAME: &str = \"PM_RESUME\";\n",
    );

    repo.build();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .args(["--max-depth", "1", "PM_RESUME"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["--max-depth", "1", "PM_RESUME"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(stdout_string(&codgrep.stdout), stdout_string(&rg.stdout));
    assert_eq!(
        stdout_string(&codgrep.stdout),
        format!(
            "{}:const NAME: &str = \"PM_RESUME\";\n",
            matched.to_string_lossy()
        )
    );
    assert!(stderr_string(&codgrep.stderr).contains("fallback rg=true"));
    assert!(stderr_string(&codgrep.stderr).contains("filesystem scope resolution required"));
}

#[test]
fn cli_glob_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file.py", "Sherlock\n");
    let source = repo.write("file.rs", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-g", "*.rs", "Sherlock"], &source);
}

#[test]
fn cli_glob_negate_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("file.py", "Sherlock\n");
    repo.write("file.rs", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-g", "!*.rs", "Sherlock"], &source);
}

#[test]
fn cli_glob_case_sensitive_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file1.HTML", "Sherlock\n");
    let source = repo.write("file2.html", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--glob", "*.html", "Sherlock"], &source);
}

#[test]
fn cli_iglob_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("file.HTML", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--iglob", "*.html", "Sherlock"], &source);
}

#[test]
fn cli_type_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file.py", "Sherlock\n");
    let source = repo.write("file.rs", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-t", "rust", "Sherlock"], &source);
}

#[test]
fn cli_type_all_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("file.py", "Sherlock\n");
    repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-t", "all", "Sherlock"], &source);
}

#[test]
fn cli_type_not_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("file.py", "Sherlock\n");
    repo.write("file.rs", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-T", "rust", "Sherlock"], &source);
}

#[test]
fn cli_type_not_all_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file.py", "Sherlock\n");
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-T", "all", "Sherlock"], &source);
}

#[test]
fn cli_type_add_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("file.wat", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg(
        &repo,
        &["--type-add", "wat:*.wat", "-t", "wat", "Sherlock"],
        &source,
    );
}

#[test]
fn cli_type_add_compose_matches_ripgrep_output() {
    let repo = TestRepo::new();
    repo.write("file.py", "Sherlock\n");
    repo.write("file.rs", "Sherlock\n");
    repo.write("file.wat", "Sherlock\n");

    repo.build();

    assert_cli_matches_rg_sorted(
        &repo,
        &[
            "--type-add",
            "wat:*.wat",
            "--type-add",
            "combo:include:wat,py",
            "-t",
            "combo",
            "Sherlock",
        ],
        &repo.repo,
    );
}

#[test]
fn cli_type_clear_matches_ripgrep_error_status() {
    let repo = TestRepo::new();
    repo.write("file.py", "Sherlock\n");
    repo.write("file.rs", "Sherlock\n");

    repo.build();

    let codgrep = repo
        .cli()
        .args(["--type-clear", "rust", "-t", "rust", "Sherlock"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["--type-clear", "rust", "-t", "rust", "Sherlock"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert!(!codgrep.stderr.is_empty());
}

#[test]
fn cli_multiple_regexp_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "alpha\nbeta\ngamma\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-e", "alpha", "-e", "beta"], &source);
}

#[test]
fn cli_pattern_file_stdin_matches_ripgrep_output() {
    use std::process::Stdio;

    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "Sherlock\nHolmes\nWatson\n");

    repo.build();

    let mut codgrep = repo.cli();
    codgrep
        .arg("-f-")
        .arg(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut rg = repo.rg();
    rg.arg("--color=never")
        .arg("-f-")
        .arg(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());

    let mut codgrep_child = codgrep.spawn().expect("test should succeed");
    let mut rg_child = rg.spawn().expect("test should succeed");

    use std::io::Write;
    codgrep_child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"Sherlock\n")
        .expect("test should succeed");
    rg_child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"Sherlock\n")
        .expect("test should succeed");

    let codgrep_output = codgrep_child
        .wait_with_output()
        .expect("test should succeed");
    let rg_output = rg_child.wait_with_output().expect("test should succeed");

    assert_eq!(codgrep_output.status.code(), rg_output.status.code());
    assert_eq!(
        stdout_string(&codgrep_output.stdout),
        stdout_string(&rg_output.stdout)
    );
}

#[test]
fn cli_quiet_files_glob_matches_ripgrep_exit_code() {
    let repo = TestRepo::new();
    repo.write("file.py", "");

    repo.build();

    let codgrep = repo
        .cli()
        .args(["--quiet", "--files", "--glob", "*.py"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .args(["--quiet", "--files", "--glob", "*.py"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert!(codgrep.stdout.is_empty());
}
