//! NDJSON experiment process. One request/response per line; not daemon protocol.
#![allow(deprecated)]
use clap::Parser;
use flashgrep::{
    experiments::watch::{Discovery, Options, OverlayStorage, WatchWorkspace},
    BuildConfig, QueryConfig, SearchMode,
};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
#[derive(Parser)]
struct Args {
    root: PathBuf,
    #[arg(long)]
    storage: PathBuf,
    #[arg(long)]
    git_base_storage: Option<PathBuf>,
    #[arg(long,default_value="memory",value_parser=["memory","disk"])]
    overlay: String,
    #[arg(long,default_value="watcher",value_parser=["watcher","git"])]
    discovery: String,
    #[arg(long, default_value_t = 60)]
    reconcile_secs: u64,
    #[arg(long, default_value_t = 16384)]
    event_capacity: usize,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    let start = Instant::now();
    let workspace = WatchWorkspace::open(Options {
        build: BuildConfig {
            repo_path: a.root,
            storage_root: a.storage,
            ..BuildConfig::default()
        },
        storage: if a.overlay == "memory" {
            OverlayStorage::Memory
        } else {
            OverlayStorage::Disk
        },
        discovery: if a.discovery == "git" {
            Discovery::Git
        } else {
            Discovery::Watcher
        },
        git_base_storage: a.git_base_storage,
        reconcile_interval: Duration::from_secs(a.reconcile_secs.max(1)),
        event_capacity: a.event_capacity,
    })?;
    println!(
        "{}",
        serde_json::json!({"ready":true,"startup_ms":start.elapsed().as_secs_f64()*1000.,"event_capacity":a.event_capacity})
    );
    for line in io::stdin().lock().lines() {
        let req: serde_json::Value = serde_json::from_str(&line?)?;
        let start = Instant::now();
        let result = match req["op"].as_str() {
            Some("search") => match workspace
                .search_with_glob(req["pattern"].as_str().unwrap_or(""), req["glob"].as_str())
            {
                Ok(paths) => {
                    serde_json::json!({"paths":paths,"ms":start.elapsed().as_secs_f64()*1000.})
                }
                Err(e) => serde_json::json!({"error":e.to_string()}),
            },
            Some("query_bench") => execute_query(&workspace, &req)?,
            Some("query_parallel") => {
                let workers = req["workers"].as_u64().unwrap_or(4).clamp(1, 16) as usize;
                let repeats = req["repeats"].as_u64().unwrap_or(10).clamp(1, 1000) as usize;
                let barrier = std::sync::Barrier::new(workers);
                let samples = std::thread::scope(|scope| {
                    let handles: Vec<_> = (0..workers)
                        .map(|_| {
                            let workspace = &workspace;
                            let req = &req;
                            let barrier = &barrier;
                            scope.spawn(move || {
                                barrier.wait();
                                (0..repeats)
                                    .map(|_| {
                                        execute_query(workspace, req).map_err(|e| e.to_string())
                                    })
                                    .collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .flat_map(|h| h.join().expect("query worker panicked"))
                        .collect::<std::result::Result<Vec<_>, _>>()
                })
                .map_err(std::io::Error::other)?;
                serde_json::json!({"samples":samples,"wall_ms":start.elapsed().as_secs_f64()*1000.})
            }
            Some("reconcile") => {
                workspace.reconcile();
                serde_json::json!({"ok":true})
            }
            Some("quit") => break,
            _ => serde_json::to_value(workspace.status())?,
        };
        println!("{result}");
        io::stdout().flush()?;
    }
    Ok(())
}

fn execute_query(
    workspace: &WatchWorkspace,
    req: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let start = Instant::now();
    let pattern = req["pattern"].as_str().unwrap_or("");
    let query = QueryConfig {
        regex_pattern: pattern.into(),
        patterns: vec![pattern.into()],
        fixed_strings: !req["regex"].as_bool().unwrap_or(false),
        global_max_results: req["unlimited"].as_bool().unwrap_or(false).then_some(0),
        search_mode: if req["lines"].as_bool().unwrap_or(false) {
            SearchMode::LineMatches
        } else {
            SearchMode::FilesWithMatches
        },
        ..QueryConfig::default()
    };
    let result = workspace.search_query(&query, req["glob"].as_str())?;
    let ms = start.elapsed().as_secs_f64() * 1000.;
    let mut paths = result.matched_paths.clone();
    paths.sort();
    let mut lines: Vec<_> = result
        .flattened_line_matches()
        .into_iter()
        .map(|m| (m.path, m.line_number))
        .collect();
    lines.sort();
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    paths.hash(&mut hash);
    lines.hash(&mut hash);
    let mut out = serde_json::json!({"ms":ms,"signature":format!("{:016x}",hash.finish()),
                    "files":result.searches_with_match,"line_count":lines.len(),
                    "candidate_docs":result.candidate_docs,"bytes_searched":result.bytes_searched});
    if req["include_results"].as_bool().unwrap_or(false) {
        out["paths"] = serde_json::json!(paths);
        out["lines"] = serde_json::json!(lines);
    }
    Ok(out)
}
