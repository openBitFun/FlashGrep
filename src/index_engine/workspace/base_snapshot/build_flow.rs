use super::*;

pub(super) fn build_sharded_base_snapshot(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    files: Vec<BaseSnapshotInputFile>,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
    on_progress: &mut impl FnMut(IndexProgress),
) -> Result<(usize, Vec<WorkspaceBaseSnapshotShardMetadata>)> {
    fs::create_dir_all(index_root)?;
    let _ = FileDeleteStore::open(index_root)?;
    let shard_builds = partition_base_snapshot_shards(files);
    let shard_total = shard_builds.len();
    let total_files = shard_builds
        .iter()
        .map(|shard| shard.files.len())
        .sum::<usize>();
    let tokenizing_progress_total = total_files.max(1);
    let shard_parallelism = base_snapshot_shard_build_parallelism(shard_total);
    report_progress(
        on_progress,
        IndexProgressPhase::Tokenizing,
        if total_files == 0 {
            "No base snapshot shards to build".into()
        } else {
            format!(
                "Tokenizing {total_files} base snapshot file(s) across {shard_total} shard(s) with {shard_parallelism} worker(s)"
            )
        },
        usize::from(total_files == 0),
        Some(tokenizing_progress_total),
    );
    let mut outcomes = Vec::with_capacity(shard_total);
    if shard_total > 0 {
        let (tx, rx) = mpsc::channel();
        let pending_shards = std::sync::Mutex::new(std::collections::VecDeque::from(shard_builds));
        let mut shard_processed = HashMap::<u32, usize>::new();
        let mut completed_shards = 0usize;
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..shard_parallelism {
                let tx = tx.clone();
                let pending_shards = &pending_shards;
                scope.spawn(move || loop {
                    let Some(shard) = pending_shards
                        .lock()
                        .expect("base snapshot shard queue lock poisoned")
                        .pop_front()
                    else {
                        break;
                    };
                    let shard_id = shard.shard_id;
                    let total_files = shard.files.len();
                    let progress_tx = tx.clone();
                    let report_shard_progress = move |processed_files| {
                        let _ = progress_tx.send(ShardBuildMessage::Progress {
                            shard_id,
                            processed_files,
                        });
                    };
                    let outcome = build_or_reuse_base_snapshot_shard(
                        index_root,
                        build_config,
                        repo_id,
                        repo_root,
                        shard,
                        previous_snapshot,
                        &report_shard_progress,
                    );
                    let _ = tx.send(ShardBuildMessage::Finished {
                        shard_id,
                        total_files,
                        outcome,
                    });
                });
            }
            drop(tx);

            while completed_shards < shard_total {
                match rx.recv().map_err(|_| {
                    AppError::Protocol(
                        "base snapshot shard worker exited before sending outcome".into(),
                    )
                })? {
                    ShardBuildMessage::Progress {
                        shard_id,
                        processed_files,
                    } => {
                        let current = shard_processed.entry(shard_id).or_default();
                        if processed_files <= *current {
                            continue;
                        }
                        *current = processed_files;
                        let global_processed = shard_processed.values().sum::<usize>();
                        report_progress(
                            on_progress,
                            IndexProgressPhase::Tokenizing,
                            format!(
                                "Tokenized {global_processed} of {total_files} base snapshot file(s) ({completed_shards} of {shard_total} shards complete)"
                            ),
                            global_processed,
                            Some(tokenizing_progress_total),
                        );
                    }
                    ShardBuildMessage::Finished {
                        shard_id,
                        total_files: shard_total_files,
                        outcome,
                    } => {
                        completed_shards += 1;
                        let current = shard_processed.entry(shard_id).or_default();
                        *current = (*current).max(shard_total_files);
                        let global_processed = shard_processed.values().sum::<usize>();
                        outcomes.push(outcome);
                        report_progress(
                            on_progress,
                            IndexProgressPhase::Tokenizing,
                            format!(
                                "Tokenized {global_processed} of {total_files} base snapshot file(s) ({completed_shards} of {shard_total} shards complete)"
                            ),
                            global_processed,
                            Some(tokenizing_progress_total),
                        );
                    }
                }
            }
            Ok(())
        })?;
    }

    let mut active_segments = Vec::new();
    let mut shard_metadata = Vec::new();
    let mut doc_count = 0usize;
    for outcome in outcomes {
        let outcome = outcome?;
        doc_count = doc_count.saturating_add(outcome.doc_count);
        if let Some(segment_ref) = outcome.segment_ref {
            active_segments.push(segment_ref);
        }
        if let Some(metadata) = outcome.shard_metadata {
            shard_metadata.push(metadata);
        }
    }

    active_segments.sort_unstable_by(|left, right| left.id.0.cmp(&right.id.0));
    report_progress(
        on_progress,
        IndexProgressPhase::Writing,
        format!(
            "Writing base snapshot shard manifest for {} segment(s)",
            active_segments.len()
        ),
        0,
        Some(3),
    );
    let committed_seq_no = u64::from(!active_segments.is_empty());
    let manifest_store = FileManifestStore::open(index_root)?;
    manifest_store.publish(&Manifest {
        manifest_id: ManifestId(BASE_SNAPSHOT_MANIFEST_ID.into()),
        generation: 1,
        created_at_unix_ms: now_unix_ms(),
        committed_seq_no,
        refreshed_seq_no: committed_seq_no,
        writer_epoch: 0,
        schema_fingerprint: SEGMENT_SCHEMA_FINGERPRINT.into(),
        active_segments,
        active_delete_segments: Vec::new(),
    })?;
    report_progress(
        on_progress,
        IndexProgressPhase::Writing,
        "Writing base snapshot metadata",
        1,
        Some(3),
    );

    shard_metadata.sort_unstable_by_key(|shard| shard.shard_id);
    Ok((doc_count, shard_metadata))
}

fn base_snapshot_shard_build_parallelism(shard_total: usize) -> usize {
    let configured = std::env::var("FLASHGREP_BASE_SNAPSHOT_SHARD_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    let available = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    base_snapshot_shard_build_parallelism_for(shard_total, configured, available)
}

pub(super) fn base_snapshot_shard_build_parallelism_for(
    shard_total: usize,
    configured: Option<usize>,
    available_parallelism: usize,
) -> usize {
    if shard_total == 0 {
        return 0;
    }
    configured
        .filter(|workers| *workers > 0)
        .unwrap_or_else(|| available_parallelism.max(1))
        .clamp(1, shard_total)
}

fn build_or_reuse_base_snapshot_shard(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    shard: BaseSnapshotShardBuild,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
    on_progress: &(impl Fn(usize) + Sync),
) -> Result<ShardBuildOutcome> {
    if let Some(reused) = maybe_link_reusable_base_snapshot_shard(
        index_root,
        shard.shard_id,
        &shard.fingerprint,
        previous_snapshot,
    )? {
        on_progress(shard.files.len());
        return Ok(ShardBuildOutcome {
            segment_ref: Some(reused.segment_ref.clone()),
            shard_metadata: Some(WorkspaceBaseSnapshotShardMetadata {
                shard_id: shard.shard_id,
                fingerprint: shard.fingerprint,
                segment_id: reused.segment_ref.id.0,
                doc_count: reused.doc_count,
                size_bytes: reused.segment_ref.size_bytes,
            }),
            doc_count: reused.doc_count,
        });
    }

    let segment_id = base_snapshot_segment_id(shard.shard_id, &shard.fingerprint);
    let Some(segment_ref) = build_base_snapshot_segment(
        index_root,
        build_config,
        repo_id,
        repo_root,
        &shard.files,
        segment_id,
        on_progress,
    )?
    else {
        return Ok(ShardBuildOutcome {
            segment_ref: None,
            shard_metadata: None,
            doc_count: 0,
        });
    };

    Ok(ShardBuildOutcome {
        doc_count: segment_ref.doc_count as usize,
        shard_metadata: Some(WorkspaceBaseSnapshotShardMetadata {
            shard_id: shard.shard_id,
            fingerprint: shard.fingerprint,
            segment_id: segment_ref.id.0.clone(),
            doc_count: segment_ref.doc_count as usize,
            size_bytes: segment_ref.size_bytes,
        }),
        segment_ref: Some(segment_ref),
    })
}

fn build_base_snapshot_segment(
    index_root: &Path,
    build_config: &BuildConfig,
    repo_id: &RepoId,
    repo_root: &Path,
    files: &[BaseSnapshotInputFile],
    segment_id: SegmentId,
    on_progress: &(impl Fn(usize) + Sync),
) -> Result<Option<SegmentRef>> {
    let mut builder = BulkBaseSegmentWriter::new(
        index_root,
        segment_id,
        PostingTokenizerConfig::from(build_config),
    )?;
    let processed_files = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for chunk in files.chunks(BASE_SNAPSHOT_PREPARE_CHUNK_DOCS) {
        let prepared = if should_parallel_prepare_base_documents(chunk) {
            let processed_files = Arc::clone(&processed_files);
            chunk
                .par_iter()
                .map(|file| {
                    let prepared =
                        prepare_base_document(repo_id, repo_root, file, &builder.tokenizer);
                    let processed = processed_files
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .saturating_add(1);
                    if processed % BASE_SNAPSHOT_PROGRESS_REPORT_DOCS == 0
                        || processed == files.len()
                    {
                        on_progress(processed);
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        } else {
            let processed_files = Arc::clone(&processed_files);
            chunk
                .iter()
                .map(|file| {
                    let prepared =
                        prepare_base_document(repo_id, repo_root, file, &builder.tokenizer);
                    let processed = processed_files
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        .saturating_add(1);
                    if processed % BASE_SNAPSHOT_PROGRESS_REPORT_DOCS == 0
                        || processed == files.len()
                    {
                        on_progress(processed);
                    }
                    prepared
                })
                .collect::<Vec<_>>()
        };

        for prepared_document in prepared {
            let Some(prepared_document) = prepared_document? else {
                continue;
            };
            builder.add_document(prepared_document)?;
        }
    }

    builder.finish()
}

pub(super) fn cleanup_unpublished_base_snapshot_builds(build_config: &BuildConfig) -> Result<()> {
    match fs::read_dir(WorkspaceBaseSnapshotIndex::storage_root(build_config)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.starts_with(BASE_SNAPSHOT_BUILD_STAGING_PREFIX)
                    || name.starts_with(BASE_SNAPSHOT_BUILD_STALE_PREFIX)
                {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = fs::remove_dir_all(path);
                    } else {
                        let _ = fs::remove_file(path);
                    }
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn next_staging_root(build_config: &BuildConfig) -> PathBuf {
    WorkspaceBaseSnapshotIndex::storage_root(build_config).join(format!(
        "{BASE_SNAPSHOT_BUILD_STAGING_PREFIX}{}-{}",
        std::process::id(),
        now_unix_nanos()
    ))
}

pub(super) fn publish_staged_base_snapshot(
    build_config: &BuildConfig,
    staged_root: &Path,
) -> Result<()> {
    let final_root = WorkspaceBaseSnapshotIndex::index_root(build_config);
    if !final_root.exists() {
        fs::rename(staged_root, &final_root)?;
        return Ok(());
    }

    merge_directory_contents(staged_root, &final_root)?;
    fs::remove_dir_all(staged_root)?;
    Ok(())
}

pub(super) fn report_progress(
    on_progress: &mut impl FnMut(IndexProgress),
    phase: IndexProgressPhase,
    message: impl Into<String>,
    processed: usize,
    total: Option<usize>,
) {
    on_progress(IndexProgress {
        phase,
        message: message.into(),
        processed,
        total,
    });
}

pub(super) fn check_cancel(should_cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if should_cancel() {
        return Err(AppError::Cancelled);
    }
    Ok(())
}

fn merge_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            if destination_path.exists() {
                merge_directory_contents(&source_path, &destination_path)?;
                fs::remove_dir_all(&source_path)?;
            } else {
                fs::rename(&source_path, &destination_path)?;
            }
            continue;
        }

        if destination_path.exists() {
            fs::remove_file(&destination_path)?;
        } else if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source_path, &destination_path)?;
        fs::remove_file(&source_path)?;
    }
    Ok(())
}
