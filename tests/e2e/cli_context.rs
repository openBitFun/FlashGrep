use crate::{
    cli_support::assert_cli_matches_rg,
    common::{TestRepo, SHERLOCK},
};

#[test]
fn cli_ripgrep_misc_after_context_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-A", "1", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_after_context_line_numbers_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-A", "1", "-n", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_before_context_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-B", "1", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_before_context_line_numbers_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-B", "1", "-n", "Sherlock"], &source);
}

#[test]
fn cli_ripgrep_misc_context_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-C", "1", "world|attached"], &source);
}

#[test]
fn cli_ripgrep_misc_context_line_numbers_sherlock() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_matches_rg(&repo, &["-C", "1", "-n", "world|attached"], &source);
}

#[test]
fn cli_after_context_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "a\nfoo\nb\n\nfoo\nc\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "--column", "-H", "-A", "1", "foo"], &source);
}

#[test]
fn cli_before_context_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "a\nfoo\nb\n\nc\nfoo\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "-H", "-B", "1", "foo"], &source);
}

#[test]
fn cli_context_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "a\nfoo\nb\n\nfoo\nc\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "-H", "-C", "1", "foo"], &source);
}

#[test]
fn cli_context_with_only_matching_matches_ripgrep_output() {
    let repo = TestRepo::new();
    let source = repo.write("matches.txt", "a\nfoo foo\nb\n");

    repo.build();

    assert_cli_matches_rg(&repo, &["-n", "-H", "-A", "1", "-o", "foo"], &source);
}
