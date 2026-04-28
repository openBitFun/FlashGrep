use std::{fs, path::Path};

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
}
