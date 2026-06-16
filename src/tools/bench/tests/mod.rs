use std::{fs, io::ErrorKind, net::TcpListener, path::Path, process::Command};

use tempfile::tempdir;

use super::{
    bench_build_config, benchmark_scope_roots, ensure_bench_base_snapshot, parse_rg_count_output,
    prepare_rg_search_scope, run, start_bench_daemon, BenchCacheMode, BenchConfig,
};
use super::{fixtures::prepare_dirty_worktree_fixture, DirtyPattern};
use crate::config::{CorpusMode, TokenizerMode};
use crate::tools::bench::report::render_report;

mod custom;
mod smoke;
mod worktree;

fn supports_tcp_daemon_tests() -> bool {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping TCP daemon bench test in restricted environment: {error}");
            false
        }
        Err(error) => panic!("failed to probe TCP daemon bench support: {error}"),
    }
}

fn write_standard_suite(suite_dir: &Path) {
    fs::create_dir_all(suite_dir.join("linux")).expect("test should succeed");
    fs::create_dir_all(suite_dir.join("subtitles")).expect("test should succeed");

    fs::write(
        suite_dir.join("linux").join("kernel.c"),
        "PM_RESUME\nERR_SYS\nabc Ah\nΩHolmesΩ\n",
    )
    .expect("test should succeed");
    fs::write(
        suite_dir.join("subtitles").join("en.sample.txt"),
        "Sherlock Holmes met John Watson.\n",
    )
    .expect("test should succeed");
    fs::write(
        suite_dir.join("subtitles").join("ru.txt"),
        "Шерлок Холмс встретил Джон Уотсон.\n",
    )
    .expect("test should succeed");

    ensure_git_repo_with_head(&suite_dir.join("linux"));
    ensure_git_repo_with_head(&suite_dir.join("subtitles"));
}

fn ensure_git_repo_with_head(repo_path: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .expect("git rev-parse should run");
    if !output.status.success() {
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["init", "-q"])
            .status()
            .expect("git init should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("git config should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("git config should succeed");
    }

    let has_head = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git rev-parse should run")
        .success();
    if !has_head {
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["add", "."])
            .status()
            .expect("git add should succeed");
        Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["commit", "--allow-empty", "-qm", "initial"])
            .status()
            .expect("git commit should succeed");
    }
}
