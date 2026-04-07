use crate::{
    cli_support::assert_cli_json_matches_rg,
    common::{json_lines, json_values, normalize_json_messages, stderr_string, TestRepo, SHERLOCK},
};

fn sorted_normalized_json(output: &[u8]) -> Vec<String> {
    let mut values = json_values(output);
    normalize_json_messages(&mut values);
    let mut lines = values
        .into_iter()
        .map(|value| serde_json::to_string(&value).expect("json value should serialize"))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines
}

#[test]
fn cli_json_emits_ripgrep_style_message_sequence() {
    let repo = TestRepo::new();
    let source = repo.write("story.txt", "Sherlock Holmes\n");

    repo.build();

    let output = repo
        .cli()
        .arg("--json")
        .arg("Sherlock Holmes")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    let messages = json_lines(&output.stdout);
    assert_eq!(messages.len(), 4);

    assert!(messages[0].starts_with("{\"type\":\"begin\""));
    assert!(messages[0].contains(&format!(
        "\"path\":{{\"text\":\"{}\"}}",
        source.to_string_lossy()
    )));

    assert!(messages[1].starts_with("{\"type\":\"match\""));
    assert!(messages[1].contains(&format!(
        "\"path\":{{\"text\":\"{}\"}}",
        source.to_string_lossy()
    )));
    assert!(messages[1].contains("\"lines\":{\"text\":\"Sherlock Holmes\\n\"}"));
    assert!(messages[1].contains("\"line_number\":1"));
    assert!(messages[1].contains("\"absolute_offset\":0"));
    assert!(messages[1].contains(
        "\"submatches\":[{\"match\":{\"text\":\"Sherlock Holmes\"},\"start\":0,\"end\":15}]"
    ));

    assert!(messages[2].starts_with("{\"type\":\"end\""));
    assert!(messages[2].contains("\"binary_offset\":null"));
    assert!(messages[2].contains("\"searches\":1"));
    assert!(messages[2].contains("\"searches_with_match\":1"));
    assert!(messages[2].contains("\"matched_lines\":1"));
    assert!(messages[2].contains("\"matches\":1"));

    assert!(messages[3].starts_with("{\"type\":\"summary\""));
    assert!(messages[3].contains("\"searches\":1"));
    assert!(messages[3].contains("\"searches_with_match\":1"));
    assert!(messages[3].contains("\"matched_lines\":1"));
    assert!(messages[3].contains("\"matches\":1"));
}

#[test]
fn cli_json_emits_summary_only_when_no_match_exists() {
    let repo = TestRepo::new();
    repo.write("story.txt", "PM_RESUME\n");

    repo.build();

    let output = repo
        .cli()
        .arg("--json")
        .arg("PM_RESUME[A-Z]")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(output.status.code(), Some(1));
    let messages = json_lines(&output.stdout);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].starts_with("{\"type\":\"summary\""));
    assert!(messages[0].contains("\"searches\":1"));
    assert!(messages[0].contains("\"searches_with_match\":0"));
    assert!(messages[0].contains("\"matched_lines\":0"));
    assert!(messages[0].contains("\"matches\":0"));
}

#[test]
fn cli_ripgrep_json_basic() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_json_matches_rg(&repo, &["-B1", "Sherlock Holmes"], &source);
}

#[test]
fn cli_ripgrep_json_quiet_stats() {
    let repo = TestRepo::new();
    let source = repo.write("sherlock", SHERLOCK);

    repo.build();

    assert_cli_json_matches_rg(&repo, &["--quiet", "--stats", "Sherlock Holmes"], &source);
}

#[test]
fn cli_json_emits_context_messages() {
    let repo = TestRepo::new();
    let source = repo.write("story.txt", "a\nSherlock Holmes\nb\n");

    repo.build();

    let output = repo
        .cli()
        .args(["--json", "-n", "-A", "1", "Sherlock Holmes"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    let messages = json_lines(&output.stdout);
    assert_eq!(messages.len(), 5);

    assert!(messages[0].starts_with("{\"type\":\"begin\""));
    assert!(messages[1].starts_with("{\"type\":\"match\""));
    assert!(messages[1].contains(&format!(
        "\"path\":{{\"text\":\"{}\"}}",
        source.to_string_lossy()
    )));
    assert!(messages[2].starts_with("{\"type\":\"context\""));
    assert!(messages[2].contains(&format!(
        "\"path\":{{\"text\":\"{}\"}}",
        source.to_string_lossy()
    )));
    assert!(messages[2].contains("\"lines\":{\"text\":\"b\\n\"}"));
    assert!(messages[2].contains("\"line_number\":3"));
    assert!(messages[2].contains("\"absolute_offset\":18"));
    assert!(messages[2].contains("\"submatches\":[]"));
    assert!(messages[3].starts_with("{\"type\":\"end\""));
    assert!(messages[4].starts_with("{\"type\":\"summary\""));
}

#[test]
fn cli_json_uses_index_without_checking_freshness() {
    let repo = TestRepo::new();
    repo.write("tracked.rs", "PM_RESUME\n");
    repo.build();
    repo.write("new.rs", "PM_RESUME\n");

    let output = repo
        .cli()
        .arg("--json")
        .arg("PM_RESUME")
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert!(output.status.success());
    let mut messages = json_values(&output.stdout);
    normalize_json_messages(&mut messages);
    let lines = messages
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>();
    assert!(lines.iter().any(|line| line.contains("tracked.rs")));
    assert!(!lines.iter().any(|line| line.contains("new.rs")));
    assert!(lines
        .last()
        .is_some_and(|line| line.contains("\"searches\":1") && line.contains("\"matches\":1")));
}

#[test]
fn cli_sparse_short_literal_json_matches_ripgrep_via_index() {
    let repo = TestRepo::new();
    repo.write("defs/matched.td", "defm Match : Something;\n");
    repo.write("defs/other.td", "defm Other : Something;\n");

    repo.build_sparse();

    let codgrep = repo
        .cli()
        .env("BITFUN_TRACE_FALLBACK", "1")
        .env("BITFUN_ENABLE_PREFLIGHT", "1")
        .args(["--json", "-F", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");
    let rg = repo
        .rg()
        .arg("--json")
        .arg("--color=never")
        .args(["-F", "-g", "*.td", "defm"])
        .arg(&repo.repo)
        .output()
        .expect("test should succeed");

    assert_eq!(codgrep.status.code(), rg.status.code());
    assert_eq!(
        sorted_normalized_json(&codgrep.stdout),
        sorted_normalized_json(&rg.stdout)
    );
    assert!(!stderr_string(&codgrep.stderr).contains("fallback rg=true"));
}
