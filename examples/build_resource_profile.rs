use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use codgrep::{
    experimental::{IndexProgress, IndexProgressPhase},
    BaseSnapshotBuildOptions, BuildConfig, CorpusMode, Result, SearchEngine, TokenizerMode,
};

const DEFAULT_MAX_FILE_SIZE: u64 = 512 * 1024 * 1024;
const DEFAULT_SAMPLE_MS: u64 = 1000;

#[derive(Clone, Debug)]
struct SharedProgress {
    phase: String,
    processed: usize,
    total: Option<usize>,
    message: String,
}

#[derive(Debug)]
struct SampleRow {
    elapsed_secs: f64,
    cpu_pct: f64,
    rss_mb: f64,
    index_size_mb: f64,
    phase: String,
    processed: usize,
    total: Option<usize>,
    message: String,
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let repo_path = args.next().map(PathBuf::from).ok_or_else(|| {
        codgrep::AppError::Protocol(
            "usage: cargo run --release --example build_resource_profile -- <repo_path> <storage_root> <csv_output>"
                .into(),
        )
    })?;
    let storage_root = args.next().map(PathBuf::from).ok_or_else(|| {
        codgrep::AppError::Protocol(
            "usage: cargo run --release --example build_resource_profile -- <repo_path> <storage_root> <csv_output>"
                .into(),
        )
    })?;
    let csv_output = args.next().map(PathBuf::from).ok_or_else(|| {
        codgrep::AppError::Protocol(
            "usage: cargo run --release --example build_resource_profile -- <repo_path> <storage_root> <csv_output>"
                .into(),
        )
    })?;

    let repo_path = fs::canonicalize(repo_path)?;
    if storage_root.exists() {
        fs::remove_dir_all(&storage_root)?;
    }
    if let Some(parent) = csv_output.parent() {
        fs::create_dir_all(parent)?;
    }

    let sample_interval =
        Duration::from_millis(read_env_u64("CODGREP_PROFILE_SAMPLE_MS", DEFAULT_SAMPLE_MS));
    let max_file_size = read_env_u64("CODGREP_PROFILE_MAX_FILE_SIZE", DEFAULT_MAX_FILE_SIZE);

    let build_config = BuildConfig {
        repo_path: repo_path.clone(),
        storage_root: storage_root.clone(),
        tokenizer: TokenizerMode::SparseNgram,
        corpus_mode: CorpusMode::RespectIgnore,
        include_hidden: false,
        max_file_size,
        min_sparse_len: 3,
        max_sparse_len: 8,
    };
    let engine = SearchEngine::new(build_config);

    let shared_progress = Arc::new(Mutex::new(SharedProgress {
        phase: "starting".to_string(),
        processed: 0,
        total: None,
        message: "Preparing base snapshot build".to_string(),
    }));
    let build_done = Arc::new(AtomicBool::new(false));

    let sampler_progress = Arc::clone(&shared_progress);
    let sampler_done = Arc::clone(&build_done);
    let sampler_storage_root = storage_root.clone();
    let sampler_started = Instant::now();
    let pid = std::process::id();

    let sampler = thread::spawn(move || -> Result<Vec<SampleRow>> {
        let mut rows = Vec::new();
        loop {
            rows.push(sample_once(
                pid,
                sampler_started.elapsed(),
                &sampler_storage_root,
                &sampler_progress,
            )?);
            if sampler_done.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(sample_interval);
        }
        Ok(rows)
    });

    let build_started = Instant::now();
    let mut on_progress = {
        let shared_progress = Arc::clone(&shared_progress);
        move |event: IndexProgress| {
            if let Ok(mut state) = shared_progress.lock() {
                *state = SharedProgress {
                    phase: format_phase(event.phase).to_string(),
                    processed: event.processed,
                    total: event.total,
                    message: event.message,
                };
            }
        }
    };
    let indexed_docs = engine.build_base_snapshot_with_options(
        BaseSnapshotBuildOptions::new().with_progress(&mut on_progress),
    )?;
    let build_secs = build_started.elapsed().as_secs_f64();

    if let Ok(mut state) = shared_progress.lock() {
        state.phase = "complete".to_string();
        state.processed = indexed_docs;
        state.total = Some(indexed_docs);
        state.message = format!("Built base snapshot with {indexed_docs} indexed documents");
    }
    build_done.store(true, Ordering::Release);

    let rows = sampler
        .join()
        .map_err(|_| codgrep::AppError::Protocol("resource sampler thread panicked".into()))??;
    write_csv(&csv_output, &rows)?;

    let peak_cpu_pct = rows.iter().map(|row| row.cpu_pct).fold(0.0_f64, f64::max);
    let peak_rss_mb = rows.iter().map(|row| row.rss_mb).fold(0.0_f64, f64::max);
    let peak_index_size_mb = rows
        .iter()
        .map(|row| row.index_size_mb)
        .fold(0.0_f64, f64::max);

    println!("repo_path={}", repo_path.display());
    println!("storage_root={}", storage_root.display());
    println!("csv_output={}", csv_output.display());
    println!("indexed_docs={indexed_docs}");
    println!("build_secs={build_secs:.3}");
    println!("samples={}", rows.len());
    println!("peak_cpu_pct={peak_cpu_pct:.1}");
    println!("peak_rss_mb={peak_rss_mb:.1}");
    println!("peak_index_size_mb={peak_index_size_mb:.1}");

    Ok(())
}

fn sample_once(
    pid: u32,
    elapsed: Duration,
    storage_root: &Path,
    shared_progress: &Arc<Mutex<SharedProgress>>,
) -> Result<SampleRow> {
    let (cpu_pct, rss_mb) = current_process_stats(pid)?;
    let index_size_mb = dir_size_bytes(storage_root)? as f64 / (1024.0 * 1024.0);
    let progress = shared_progress
        .lock()
        .map_err(|_| codgrep::AppError::Protocol("progress mutex poisoned".into()))?
        .clone();

    Ok(SampleRow {
        elapsed_secs: elapsed.as_secs_f64(),
        cpu_pct,
        rss_mb,
        index_size_mb,
        phase: progress.phase,
        processed: progress.processed,
        total: progress.total,
        message: progress.message,
    })
}

fn current_process_stats(pid: u32) -> Result<(f64, f64)> {
    let output = Command::new("ps")
        .args(["-o", "%cpu=", "-o", "rss=", "-p", &pid.to_string()])
        .output()?;
    if !output.status.success() {
        return Err(codgrep::AppError::Protocol(format!(
            "ps failed for pid {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| codgrep::AppError::Protocol(format!("ps returned no row for pid {pid}")))?;
    let mut columns = line.split_whitespace();
    let cpu_pct = columns
        .next()
        .ok_or_else(|| codgrep::AppError::Protocol("missing cpu column from ps".into()))?
        .parse::<f64>()
        .map_err(|error| {
            codgrep::AppError::Protocol(format!("invalid cpu value from ps: {error}"))
        })?;
    let rss_kb = columns
        .next()
        .ok_or_else(|| codgrep::AppError::Protocol("missing rss column from ps".into()))?
        .parse::<f64>()
        .map_err(|error| {
            codgrep::AppError::Protocol(format!("invalid rss value from ps: {error}"))
        })?;
    Ok((cpu_pct, rss_kb / 1024.0))
}

fn write_csv(output: &Path, rows: &[SampleRow]) -> Result<()> {
    let mut file = fs::File::create(output)?;
    writeln!(
        file,
        "elapsed_secs,cpu_pct,rss_mb,index_size_mb,phase,processed,total,message"
    )?;
    for row in rows {
        writeln!(
            file,
            "{:.3},{:.2},{:.2},{:.2},{},{},{},{}",
            row.elapsed_secs,
            row.cpu_pct,
            row.rss_mb,
            row.index_size_mb,
            csv_escape(&row.phase),
            row.processed,
            row.total.map(|value| value.to_string()).unwrap_or_default(),
            csv_escape(&row.message),
        )?;
    }
    Ok(())
}

fn csv_escape(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn dir_size_bytes(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn read_env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn format_phase(phase: IndexProgressPhase) -> &'static str {
    match phase {
        IndexProgressPhase::Scanning => "scanning",
        IndexProgressPhase::Tokenizing => "tokenizing",
        IndexProgressPhase::Writing => "writing",
        IndexProgressPhase::Finalizing => "finalizing",
        IndexProgressPhase::RefreshingOverlay => "refreshing_overlay",
    }
}
