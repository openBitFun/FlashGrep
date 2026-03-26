use std::{path::Path, process::Output};

use crate::common::{json_values, normalize_json_messages, sort_lines, stdout_string, TestRepo};

pub(crate) fn run_cli(repo: &TestRepo, args: &[&str], search_root: &Path) -> Output {
    repo.cli()
        .args(args)
        .arg(search_root)
        .output()
        .expect("test should succeed")
}

pub(crate) fn run_rg(repo: &TestRepo, args: &[&str], search_root: &Path) -> Output {
    repo.rg()
        .arg("--color=never")
        .args(args)
        .arg(search_root)
        .output()
        .expect("test should succeed")
}

pub(crate) fn assert_cli_matches_rg(repo: &TestRepo, args: &[&str], search_root: &Path) {
    let bitfun = run_cli(repo, args, search_root);
    let rg = run_rg(repo, args, search_root);

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(stdout_string(&bitfun.stdout), stdout_string(&rg.stdout));
}

pub(crate) fn assert_cli_matches_rg_sorted(repo: &TestRepo, args: &[&str], search_root: &Path) {
    let bitfun = run_cli(repo, args, search_root);
    let rg = run_rg(repo, args, search_root);

    assert_eq!(bitfun.status.code(), rg.status.code());
    assert_eq!(
        sort_lines(&stdout_string(&bitfun.stdout)),
        sort_lines(&stdout_string(&rg.stdout))
    );
}

pub(crate) fn assert_cli_json_matches_rg(repo: &TestRepo, args: &[&str], search_root: &Path) {
    let bitfun = repo
        .cli()
        .arg("--json")
        .args(args)
        .arg(search_root)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--json")
        .arg("--color=never")
        .args(args)
        .arg(search_root)
        .output()
        .expect("test should succeed");

    assert_eq!(bitfun.status.code(), rg.status.code());
    let mut bitfun_json = json_values(&bitfun.stdout);
    let mut rg_json = json_values(&rg.stdout);
    normalize_json_messages(&mut bitfun_json);
    normalize_json_messages(&mut rg_json);
    assert_eq!(bitfun_json, rg_json);
}

pub(crate) fn normalized_stats_output(output: &[u8]) -> String {
    stdout_string(output)
        .lines()
        .map(|line| {
            if line.ends_with("seconds spent searching") {
                "<search-time>".to_string()
            } else if line.ends_with("seconds total") {
                "<total-time>".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
