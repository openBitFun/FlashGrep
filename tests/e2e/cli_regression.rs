use crate::{
    cli_support::{assert_cli_matches_rg, normalized_stats_output, run_cli, run_rg},
    common::{stdout_string, TestRepo, SHERLOCK},
};

#[test]
fn cli_ripgrep_regression_r65_gitignore_directory_ignored() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "a/\n");
    repo.write("a/foo", "xyz\n");
    repo.write("a/bar", "xyz\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["xyz"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r67_gitignore_whitelist_dir() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "/*\n!/dir\n");
    let source = repo.write("dir/bar", "test\n");
    repo.write("foo/bar", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &source);
}

#[test]
fn cli_ripgrep_regression_r90_gitignore_whitelist_hidden_file() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    let source = repo.write(".foo", "test\n");
    repo.write(".gitignore", "!.foo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &source);
}

#[test]
fn cli_ripgrep_regression_r127_gitignore_nested_path() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "foo/sherlock\n");
    repo.write("foo/sherlock", SHERLOCK);
    let source = repo.write("foo/watson", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["Sherlock"], &source);
}

#[test]
fn cli_ripgrep_regression_r128_line_numbers_with_vertical_tab() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "01234567\u{b}\n\u{b}\n\u{b}\n\u{b}\nx");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "x"], &source);
}

#[test]
fn cli_ripgrep_regression_r156_no_line_number_complex_regex() {
    let repo = TestRepo::new();
    let source = repo.write(
        "testcase.txt",
        r#"#parse('widgets/foo_bar_macros.vm')
#parse ( 'widgets/mobile/foo_bar_macros.vm' )
#parse ("widgets/foobarhiddenformfields.vm")
#parse ( "widgets/foo_bar_legal.vm" )
#include( 'widgets/foo_bar_tips.vm' )
#include('widgets/mobile/foo_bar_macros.vm')
#include ("widgets/mobile/foo_bar_resetpw.vm")
#parse('widgets/foo-bar-macros.vm')
#parse ( 'widgets/mobile/foo-bar-macros.vm' )
#parse ("widgets/foo-bar-hiddenformfields.vm")
#parse ( "widgets/foo-bar-legal.vm" )
#include( 'widgets/foo-bar-tips.vm' )
#include('widgets/mobile/foo-bar-macros.vm')
#include ("widgets/mobile/foo-bar-resetpw.vm")
"#,
    );

    repo.build();

    assert_cli_matches_rg(
        &repo,
        &[
            "-N",
            r#"#(?:parse|include)\s*\(\s*(?:"|')[./A-Za-z_-]+(?:"|')"#,
        ],
        &source,
    );
}

#[test]
fn cli_ripgrep_regression_r451_only_matching_as_in_issue() {
    let repo = TestRepo::new();
    let source = repo.write("digits.txt", "1 2 3\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--only-matching", r"[0-9]+"], &source);
}

#[test]
fn cli_ripgrep_regression_r451_only_matching() {
    let repo = TestRepo::new();
    let source = repo.write("digits.txt", "1 2 3\n123\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--only-matching", "--column", r"[0-9]"], &source);
}

#[test]
fn cli_ripgrep_regression_r483_quiet_files_glob_non_matching_exit_code() {
    let repo = TestRepo::new();
    repo.write("file.rs", "");

    repo.build();

    let bitfun = repo
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

    assert_eq!(bitfun.status.code(), rg.status.code());
}

#[test]
fn cli_ripgrep_regression_r493() {
    let repo = TestRepo::new();
    let source = repo.write("input.txt", "peshwaship 're seminomata\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-o", r"\b 're \b"], &source);
}

#[test]
fn cli_ripgrep_regression_r506_word_not_parenthesized() {
    let repo = TestRepo::new();
    let source = repo.write("wb.txt", "min minimum amin\nmax maximum amax\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-w", "-o", "min|max"], &source);
}

#[test]
fn cli_ripgrep_regression_r553_switch_repeated_case_insensitive() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-i", "sherlock"], &source);
    assert_cli_matches_rg(&repo, &["-i", "-i", "sherlock"], &source);
}

#[test]
fn cli_ripgrep_regression_r553_context_flag_override() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-C", "1", "world|attached"], &source);
    assert_cli_matches_rg(&repo, &["-C", "0", "world|attached"], &source);
    assert_cli_matches_rg(&repo, &["-B", "1", "-C", "0", "Sherlock"], &source);
    assert_cli_matches_rg(&repo, &["-A", "1", "-C", "0", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_regression_r568_leading_hyphen_option_args() {
    let repo = TestRepo::new();
    let source = repo.write("file", "foo bar -baz\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-e-baz", "-e", "-baz"], &source);
}

#[test]
fn cli_ripgrep_regression_r900_empty_pattern_file_errors() {
    let repo = TestRepo::new();
    let patterns = repo.write("pat", "");
    repo.write("sherlock", SHERLOCK);

    repo.build();

    let pattern_path = patterns.to_string_lossy().into_owned();
    let bitfun = run_cli(
        &repo,
        &["-f", pattern_path.as_str()],
        &repo.path("sherlock"),
    );
    let rg = run_rg(
        &repo,
        &["-f", pattern_path.as_str()],
        &repo.path("sherlock"),
    );

    assert_eq!(bitfun.status.code(), rg.status.code());
}

#[test]
fn cli_ripgrep_regression_r1064() {
    let repo = TestRepo::new();
    let source = repo.write("input", "abc");

    repo.build();

    assert_cli_matches_rg(&repo, &["a(.*c)"], &source);
}

#[test]
fn cli_ripgrep_regression_r1098_gitignore_double_star_literal() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "a**b\n");
    repo.write("afoob", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r1130_files_modes_with_explicit_file() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["--files-with-matches", "test"], &source);
    assert_cli_matches_rg(&repo, &["--files-without-match", "nada"], &source);
}

#[test]
fn cli_ripgrep_regression_r829_2747_files_respect_anchored_ignore_with_cwd() {
    let repo = TestRepo::new();
    repo.create_dir("a/c/b");
    repo.create_dir("a/src/f/b");
    repo.write("a/c/b/foo", "");
    repo.write("a/src/f/b/foo", "");
    repo.write(".ignore", "/a/*/b\n");

    repo.build();

    let bitfun = repo
        .cli()
        .arg("--files")
        .current_dir(repo.path("a/src"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("--files")
        .current_dir(repo.path("a/src"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r184_ignore_dotfile_when_cwd_changes() {
    let repo = TestRepo::new();
    repo.write(".gitignore", ".*\n");
    repo.write("foo/bar/baz", "test\n");

    repo.build();

    let bitfun = repo
        .cli()
        .arg("test")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("test")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r199_smart_case_regex() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "tEsT");

    repo.build();

    assert_cli_matches_rg(&repo, &["--smart-case", r"\btest\b"], &source);
}

#[test]
fn cli_ripgrep_regression_r206_glob_after_pattern() {
    let repo = TestRepo::new();
    let source = repo.write("foo/bar.txt", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test", "-g", "*.txt"], &source);
}

#[test]
fn cli_ripgrep_regression_r229_smart_case_uppercase_class_is_case_sensitive() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "economie");

    repo.build();

    assert_cli_matches_rg(&repo, &["-S", "[E]conomie"], &source);
}

#[test]
fn cli_ripgrep_regression_r251_unicode_ignore_case() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "привет\nПривет\nПрИвЕт");

    repo.build();

    assert_cli_matches_rg(&repo, &["-i", "привет"], &source);
}

#[test]
fn cli_ripgrep_regression_r270_leading_hyphen_pattern_with_split_e() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "-test");

    repo.build();

    assert_cli_matches_rg(&repo, &["-e", "-test"], &source);
}

#[test]
fn cli_ripgrep_regression_r279_quiet_stdout_empty() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "test");

    repo.build();

    let bitfun = run_cli(&repo, &["-q", "test"], &source);
    let rg = run_rg(&repo, &["-q", "test"], &source);

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert!(bitfun.stdout.is_empty());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r829_2778_files_relative_cwd_with_anchored_ignore() {
    let repo = TestRepo::new();
    repo.create_dir("parent/subdir");
    repo.write(".ignore", "/parent/*.txt\n");
    repo.write("parent/ignore-me.txt", "");
    repo.write("parent/subdir/dont-ignore-me.txt", "");

    repo.build();

    let bitfun = repo
        .cli()
        .arg("--files")
        .current_dir(repo.path("parent"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("--files")
        .current_dir(repo.path("parent"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r1334_crazy_literals() {
    let repo = TestRepo::new();
    let patterns = repo.write("patterns", &"1.208.0.0/12\n".repeat(40));
    let source = repo.write("corpus", "1.208.0.0/12\n");

    repo.build();

    let pattern_path = patterns.to_string_lossy().into_owned();
    assert_cli_matches_rg(&repo, &["-F", "-f", pattern_path.as_str()], &source);
}

#[test]
fn cli_ripgrep_regression_r1380_after_context_with_max_count() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "a\nb\nc\nd\ne\nd\ne\nd\ne\nd\ne\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-A", "2", "-m", "1", "d"], &source);
}

#[test]
fn cli_ripgrep_regression_r1537() {
    let repo = TestRepo::new();
    let source = repo.write("foo", "abc;de,fg");

    repo.build();

    assert_cli_matches_rg(&repo, &[";(.*,){1}"], &source);
}

#[test]
fn cli_ripgrep_regression_r2236_escaped_slash_in_ignore() {
    let repo = TestRepo::new();
    repo.write(".ignore", "foo\\/\n");
    repo.write("foo/bar", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r3067_gitignore_specificity() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "foobar/debug\n");
    repo.write("foobar/some/debug/flag", "baz\n");
    repo.write("foobar/debug/flag2", "baz\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["baz"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r3127_gitignore_allow_unclosed_class() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "[abc\n");
    repo.write("[abc", "");
    repo.write("test", "");

    repo.build();

    let bitfun = repo
        .cli()
        .arg("--files")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("--files")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r3127_glob_flag_not_allow_unclosed_class() {
    let repo = TestRepo::new();
    repo.write("[abc", "");
    repo.write("test", "");

    repo.build();

    let bitfun = run_cli(&repo, &["--files", "-g", "[abc"], &repo.repo);
    let rg = run_rg(&repo, &["--files", "-g", "[abc"], &repo.repo);

    assert_eq!(bitfun.status.code(), rg.status.code());
}

#[test]
fn cli_ripgrep_regression_r2944_incorrect_bytes_searched() {
    let repo = TestRepo::new();
    repo.write("haystack", "foo1\nfoo2\nfoo3\nfoo4\nfoo5\n");

    repo.build();

    let bitfun = run_cli(&repo, &["--stats", "-m2", "foo"], &repo.repo);
    let rg = run_rg(&repo, &["--stats", "-m2", "foo"], &repo.repo);

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(
        normalized_stats_output(&bitfun.stdout),
        normalized_stats_output(&rg.stdout)
    );
}

#[test]
fn cli_ripgrep_misc_ignore_git_parent_stop() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "sherlock\n");
    repo.create_dir("foo/.git");
    repo.write("foo/bar/sherlock", SHERLOCK);

    repo.build();

    let bitfun = repo
        .cli()
        .arg("Sherlock")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("Sherlock")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_misc_ignore_generic_file() {
    let repo = TestRepo::new();
    repo.write("sherlock", SHERLOCK);
    repo.write(".ignore", "sherlock\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["Sherlock"], &repo.repo);
}

#[test]
fn cli_ripgrep_misc_ignore_git_parent_reports_no_files_searched() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "sherlock\n");
    repo.write("foo/sherlock", SHERLOCK);

    repo.build();

    let bitfun = repo
        .cli()
        .arg("Sherlock")
        .current_dir(repo.path("foo"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("Sherlock")
        .current_dir(repo.path("foo"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
    assert_eq!(
        String::from_utf8_lossy(&bitfun.stderr),
        String::from_utf8_lossy(&rg.stderr)
    );
}

#[test]
fn cli_ripgrep_misc_ignore_git_parent_stop_file() {
    let repo = TestRepo::new();
    repo.write(".gitignore", "sherlock\n");
    repo.write("foo/.git", "");
    repo.write("foo/bar/sherlock", SHERLOCK);

    repo.build();

    let bitfun = repo
        .cli()
        .arg("Sherlock")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--color=never")
        .arg("Sherlock")
        .current_dir(repo.path("foo/bar"))
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

#[test]
fn cli_ripgrep_regression_r1163_bom_line_anchor() {
    let repo = TestRepo::new();
    let source = repo.write("bom.txt", "\u{FEFF}test123\ntest123");

    repo.build();

    assert_cli_matches_rg(&repo, &["^test123"], &source);
}

#[test]
fn cli_ripgrep_regression_r1173_gitignore_double_star_ignores_everything() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "**\n");
    repo.write("foo", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r1174_gitignore_deep_double_star_ignores_everything() {
    let repo = TestRepo::new();
    repo.create_dir(".git");
    repo.write(".gitignore", "**/**/*\n");
    repo.write("a/foo", "test\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["test"], &repo.repo);
}

#[test]
fn cli_ripgrep_regression_r1176_literal_file() {
    let repo = TestRepo::new();
    let patterns = repo.write("patterns", "foo(bar\n");
    let source = repo.write("test", "foo(bar");

    repo.build();

    let pattern_path = patterns.to_string_lossy().into_owned();
    assert_cli_matches_rg(&repo, &["-F", "-f", pattern_path.as_str()], &source);
}

#[test]
fn cli_ripgrep_regression_r1176_line_regex() {
    let repo = TestRepo::new();
    let patterns = repo.write("patterns", "foo\n");
    let source = repo.write("test", "foobar\nfoo\nbarfoo\n");

    repo.build();

    let pattern_path = patterns.to_string_lossy().into_owned();
    assert_cli_matches_rg(&repo, &["-x", "-f", pattern_path.as_str()], &source);
}

#[test]
fn cli_ripgrep_regression_r1319() {
    let repo = TestRepo::new();
    let source = repo.write(
        "input",
        "CCAGCTACTCGGGAGGCTGAGGCTGGAGGATCGCTTGAGTCCAGGAGTTC",
    );

    repo.build();

    assert_cli_matches_rg(&repo, &["TTGAGTCCAGGAG[ATCG]{2}C"], &source);
}

#[test]
fn cli_ripgrep_regression_r3108_files_without_match_quiet_exit() {
    let repo = TestRepo::new();
    let matched = repo.write("yes-match", "abc");
    let missed = repo.write("non-match", "xyz");

    repo.build();

    let quiet_no_match = repo
        .cli()
        .args(["-q", "abc"])
        .arg(&missed)
        .output()
        .expect("test should succeed");
    assert_eq!(quiet_no_match.status.code(), Some(1));

    let quiet_match = repo
        .cli()
        .args(["-q", "abc"])
        .arg(&matched)
        .output()
        .expect("test should succeed");
    assert_eq!(quiet_match.status.code(), Some(0));

    let files_without_match = repo
        .cli()
        .args(["--files-without-match", "abc"])
        .arg(&missed)
        .output()
        .expect("test should succeed");
    assert_eq!(files_without_match.status.code(), Some(0));
    assert_eq!(
        stdout_string(&files_without_match.stdout),
        format!("{}\n", missed.to_string_lossy())
    );

    let files_without_match_hit = repo
        .cli()
        .args(["--files-without-match", "abc"])
        .arg(&matched)
        .output()
        .expect("test should succeed");
    assert_eq!(files_without_match_hit.status.code(), Some(1));

    let quiet_files_without_match = repo
        .cli()
        .args(["--files-without-match", "-q", "abc"])
        .arg(&missed)
        .output()
        .expect("test should succeed");
    assert_eq!(quiet_files_without_match.status.code(), Some(0));
    assert!(quiet_files_without_match.stdout.is_empty());

    let quiet_files_without_match_hit = repo
        .cli()
        .args(["--files-without-match", "-q", "abc"])
        .arg(&matched)
        .output()
        .expect("test should succeed");
    assert_eq!(quiet_files_without_match_hit.status.code(), Some(1));
}
