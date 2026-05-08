mod args;
mod output;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use clap::Parser;

use crate::{
    daemon::{serve, DaemonClient, ManagedDaemonClient, ServerOptions},
    error::{AppError, Result},
    index_engine::{EngineConfig, IndexDocument, IndexEngine, RepoId, ShardId},
    search::SearchMode,
    tools::bench::{self},
};

use self::args::{
    BuildArgs, Cli, Commands, DaemonArgs, DaemonCommands, EngineArgs, EngineCommands,
    EngineRepoArgs, EngineSearchArgs,
};
use self::output::{
    print_daemon_response, print_daemon_status_response, print_engine_search_results,
};

pub(crate) fn run() -> Result<bool> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build(args) => run_build(args),
        Commands::Bench(args) => run_bench(args.into_config()),
        Commands::Engine(args) => run_engine(args),
        Commands::Serve(args) => run_serve(args),
        Commands::Daemon(args) => run_daemon(args),
    }
}

fn run_engine(args: EngineArgs) -> Result<bool> {
    match args.command {
        EngineCommands::Upsert(args) => {
            let repo_root = canonical_or_original(&args.repo.repo);
            let engine = open_index_engine(&args.repo)?;
            let document = load_engine_document(&repo_root, &args.path, args.language)?;
            let stamp = engine.writer().add_document(document)?;
            println!("upserted seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Delete(args) => {
            let repo_root = canonical_or_original(&args.repo.repo);
            let engine = open_index_engine(&args.repo)?;
            let logical_path = logical_path_from_repo(&repo_root, &args.path)?;
            let stamp = engine
                .writer()
                .delete_document(&crate::index_engine::DocKey {
                    repo_id: RepoId(repo_root.to_string_lossy().into_owned()),
                    logical_path,
                })?;
            println!("deleted seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Refresh(args) => {
            let engine = open_index_engine(&args)?;
            let stamp = engine.writer().refresh()?;
            println!("refreshed seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Flush(args) => {
            let engine = open_index_engine(&args)?;
            let result = engine.writer().flush()?;
            println!(
                "flushed max_seq_no={} segments={} delete_segments={}",
                result.max_seq_no,
                result.segment_ids.len(),
                result.delete_segment_ids.len()
            );
            Ok(true)
        }
        EngineCommands::Commit(args) => {
            let engine = open_index_engine(&args)?;
            let stamp = engine.writer().commit()?;
            println!("committed seq_no={}", stamp.seq_no);
            Ok(true)
        }
        EngineCommands::Search(args) => run_engine_search(args),
        EngineCommands::Merge(args) => {
            let engine = open_index_engine(&args.repo)?;
            engine.admin().force_merge(args.max_segments)?;
            let manifest = engine.admin().manifest()?;
            println!(
                "merged generation={} active_segments={} active_delete_segments={}",
                manifest.generation,
                manifest.active_segments.len(),
                manifest.active_delete_segments.len()
            );
            Ok(true)
        }
        EngineCommands::Vacuum(args) => {
            let engine = open_index_engine(&args)?;
            let removed = engine.admin().vacuum()?;
            println!("vacuumed files={removed}");
            Ok(true)
        }
        EngineCommands::Status(args) => {
            let engine = open_index_engine(&args)?;
            let manifest = engine.admin().manifest()?;
            let stats = engine.writer().stats();
            println!(
                "generation={} committed_seq_no={} manifest_refreshed_seq_no={} writer_last_seq_no={} uncommitted_ops={} pending_docs={} active_segments={} active_delete_segments={}",
                manifest.generation,
                manifest.committed_seq_no,
                manifest.refreshed_seq_no,
                stats.last_seq_no,
                stats.last_seq_no.saturating_sub(manifest.committed_seq_no),
                stats.pending_docs,
                manifest.active_segments.len(),
                manifest.active_delete_segments.len()
            );
            Ok(true)
        }
    }
}

fn run_engine_search(args: EngineSearchArgs) -> Result<bool> {
    let engine = open_index_engine(&args.repo)?;
    let query = crate::index_engine::query::QueryRequest {
        pattern: args.pattern,
        mode: SearchMode::LineMatches,
        case_insensitive: args.ignore_case,
        max_count: None,
        top_k: None,
        filters: crate::index_engine::query::QueryFilters::default(),
        prefer_worktree_for_git_head_blob: false,
    };
    let reader = if args.committed {
        engine.reader().open_committed()?
    } else {
        engine.reader().open_snapshot()?
    };
    let results = reader.search(&query)?;
    print_engine_search_results(&results);
    Ok(true)
}

fn run_build(args: BuildArgs) -> Result<bool> {
    let ensured = ManagedDaemonClient::new().ensure_repo(args.into_ensure_params())?;
    match ensured.indexed_docs {
        Some(indexed_docs) => {
            println!(
                "ensured repo via daemon at {}: built base snapshot with {indexed_docs} docs",
                ensured.addr
            );
        }
        None => {
            println!(
                "ensured repo via daemon at {}: base snapshot already available",
                ensured.addr
            );
        }
    }
    Ok(true)
}

fn open_index_engine(args: &EngineRepoArgs) -> Result<IndexEngine> {
    let repo_root = canonical_or_original(&args.repo);
    let index_root = args
        .index_root
        .clone()
        .unwrap_or_else(|| repo_root.join(".flashgrep-index-engine"));
    IndexEngine::open(EngineConfig {
        shard_id: ShardId(repo_root.to_string_lossy().into_owned()),
        index_root,
        ..EngineConfig::default()
    })
}

fn load_engine_document(
    repo_root: &Path,
    path: &Path,
    language: Option<String>,
) -> Result<IndexDocument> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    let logical_path = logical_path_from_repo(repo_root, &absolute_path)?;
    let bytes = std::fs::read(&absolute_path)?;
    let metadata = std::fs::metadata(&absolute_path)?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let repo_id = RepoId(repo_root.to_string_lossy().into_owned());
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    let file_name = absolute_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::Protocol(format!("invalid file name: {}", absolute_path.display()))
        })?
        .to_string();

    Ok(IndexDocument {
        key: crate::index_engine::DocKey {
            repo_id: repo_id.clone(),
            logical_path: logical_path.clone(),
        },
        repo_id,
        logical_path,
        file_name,
        language: language.or_else(|| infer_language(&absolute_path)),
        size: bytes.len() as u64,
        modified_at,
        content_hash,
        source_bytes: std::sync::Arc::<[u8]>::from(bytes),
    })
}

fn logical_path_from_repo(repo_root: &Path, path: &Path) -> Result<String> {
    let path = canonical_or_original(path);
    let repo_root = canonical_or_original(repo_root);
    let relative = path.strip_prefix(&repo_root).map_err(|_| {
        AppError::Protocol(format!(
            "path {} is not under repo root {}",
            path.display(),
            repo_root.display()
        ))
    })?;
    Ok(relative.to_string_lossy().into_owned())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn infer_language(path: &Path) -> Option<String> {
    Some(path.extension()?.to_string_lossy().to_ascii_lowercase())
}

fn run_serve(args: self::args::ServeArgs) -> Result<bool> {
    serve(ServerOptions {
        bind_addr: args.bind,
        state_file: args.state_file,
        stdio: args.stdio,
    })?;
    Ok(true)
}

fn run_daemon(args: DaemonArgs) -> Result<bool> {
    use crate::daemon::protocol::{RefreshRepoParams, RepoRef, Request};

    match args.command {
        DaemonCommands::Open(args) => {
            let addr = args.addr();
            let client = DaemonClient::new(addr);
            let response = client.open_repo(args.into_open_repo_params())?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Ensure(args) => {
            let addr = args.addr();
            let open = args.into_open_repo_params();
            let client = DaemonClient::new(addr);
            let response = client.ensure_repo(crate::daemon::protocol::EnsureRepoParams {
                repo_path: open.repo_path,
                storage_root: open.storage_root,
                config: open.config,
                refresh: open.refresh,
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Status(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::GetRepoStatus {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_status_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Refresh(args) => {
            let client = DaemonClient::new(args.repo.connect.addr);
            let response = client.send(Request::RefreshRepo {
                params: RefreshRepoParams {
                    repo_id: args.repo.repo_id,
                    force: args.force,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Build(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::BuildBaseSnapshot {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Rebuild(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::RebuildBaseSnapshot {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Search(args) => {
            let client = DaemonClient::new(args.repo.connect.addr.clone());
            let response = client.send(Request::Search {
                params: args.into_search_params(),
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Glob(args) => {
            let client = DaemonClient::new(args.repo.connect.addr.clone());
            let response = client.glob(args.into_glob_params())?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Close(args) => {
            let client = DaemonClient::new(args.connect.addr);
            let response = client.send(Request::CloseRepo {
                params: RepoRef {
                    repo_id: args.repo_id,
                },
            })?;
            print_daemon_response(&response)?;
            Ok(true)
        }
        DaemonCommands::Shutdown(args) => {
            let client = DaemonClient::new(args.addr);
            let response = client.send(Request::Shutdown)?;
            print_daemon_response(&response)?;
            Ok(true)
        }
    }
}

fn run_bench(config: crate::tools::bench::BenchConfig) -> Result<bool> {
    if cfg!(debug_assertions) {
        eprintln!(
            "warning: flashgrep bench is running from a debug build; performance results will be pessimistic. use `cargo run --release -- bench ...` or `target/release/flashgrep bench ...` for representative numbers."
        );
    }
    let report = bench::run(&config)?;
    bench::print_report(&report);
    Ok(true)
}
