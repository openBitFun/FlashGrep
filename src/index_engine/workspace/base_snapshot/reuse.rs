use super::*;

pub(super) struct ResolvedBaseSnapshotBuild {
    pub(super) snapshot_key: String,
    pub(super) head_commit: Option<String>,
    pub(super) config_fingerprint: String,
}

pub(super) struct ReusableBaseSnapshotShards {
    pub(super) index_root: PathBuf,
    pub(super) manifest_segments: HashMap<String, SegmentRef>,
    pub(super) shards_by_id: HashMap<u32, WorkspaceBaseSnapshotShardMetadata>,
}

struct LinkedReusableShard {
    segment_ref: SegmentRef,
    doc_count: usize,
}

pub(super) fn reusable_base_snapshot_shards(
    build_config: &BuildConfig,
) -> Result<Option<ReusableBaseSnapshotShards>> {
    let Some(metadata) = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(build_config)? else {
        return Ok(None);
    };
    if metadata.shards.is_empty()
        || metadata.tokenizer != Some(build_config.tokenizer)
        || metadata.config_fingerprint.as_deref()
            != Some(&base_snapshot_config_fingerprint(build_config))
        || metadata.repo_root.as_deref() != Some(&canonical_repo_root(&build_config.repo_path))
    {
        return Ok(None);
    }

    let index_root = WorkspaceBaseSnapshotIndex::index_root(build_config);
    let manifest = FileManifestStore::open(&index_root)?.load_current()?;
    let manifest_segments = manifest
        .active_segments
        .into_iter()
        .map(|segment| (segment.id.0.clone(), segment))
        .collect::<HashMap<_, _>>();
    let shards_by_id = metadata
        .shards
        .into_iter()
        .map(|shard| (shard.shard_id, shard))
        .collect::<HashMap<_, _>>();
    Ok(Some(ReusableBaseSnapshotShards {
        index_root,
        manifest_segments,
        shards_by_id,
    }))
}

pub(super) fn maybe_link_reusable_base_snapshot_shard(
    staging_root: &Path,
    shard_id: u32,
    fingerprint: &str,
    previous_snapshot: Option<&ReusableBaseSnapshotShards>,
) -> Result<Option<SegmentRefWithDocCount>> {
    let Some(previous_snapshot) = previous_snapshot else {
        return Ok(None);
    };
    let Some(previous_shard) = previous_snapshot.shards_by_id.get(&shard_id) else {
        return Ok(None);
    };
    if previous_shard.fingerprint != fingerprint {
        return Ok(None);
    }
    let Some(segment_ref) = previous_snapshot
        .manifest_segments
        .get(&previous_shard.segment_id)
        .cloned()
    else {
        return Ok(None);
    };
    let source_segment_dir = previous_snapshot
        .index_root
        .join("segments")
        .join(&previous_shard.segment_id);
    if !source_segment_dir.exists() {
        return Ok(None);
    }
    link_segment_dir_into_staging(
        &source_segment_dir,
        &staging_root
            .join("segments")
            .join(&previous_shard.segment_id),
    )?;
    Ok(Some(SegmentRefWithDocCount {
        segment_ref,
        doc_count: previous_shard.doc_count,
    }))
}

pub(super) struct SegmentRefWithDocCount {
    pub(super) segment_ref: SegmentRef,
    pub(super) doc_count: usize,
}

pub(super) fn partition_base_snapshot_shards(
    files: Vec<BaseSnapshotInputFile>,
) -> Vec<BaseSnapshotShardBuild> {
    let mut shards = BTreeMap::<u32, Vec<BaseSnapshotInputFile>>::new();
    for file in files {
        shards
            .entry(base_snapshot_shard_id(&file.logical_path))
            .or_default()
            .push(file);
    }

    shards
        .into_iter()
        .map(|(shard_id, mut shard_files)| {
            shard_files.sort_unstable_by(|left, right| left.logical_path.cmp(&right.logical_path));
            let fingerprint = base_snapshot_shard_fingerprint(&shard_files);
            BaseSnapshotShardBuild {
                shard_id,
                fingerprint,
                files: shard_files,
            }
        })
        .collect()
}

pub(super) fn base_snapshot_segment_id(shard_id: u32, fingerprint: &str) -> SegmentId {
    SegmentId(format!("seg-base-{shard_id:04}-{fingerprint}"))
}

pub(super) fn resolve_base_snapshot_build(
    build_config: &BuildConfig,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<ResolvedBaseSnapshotBuild> {
    let config_fingerprint = base_snapshot_config_fingerprint(build_config);
    match resolve_git_head(&build_config.repo_path, should_cancel)? {
        Some(head_commit) => {
            let snapshot_key = format!("base-git-{head_commit}-{config_fingerprint}");
            Ok(ResolvedBaseSnapshotBuild {
                snapshot_key,
                head_commit: Some(head_commit),
                config_fingerprint,
            })
        }
        None => {
            let snapshot_key = format!(
                "base-repo-{}-{}",
                repo_snapshot_fingerprint(build_config)?,
                config_fingerprint
            );
            Ok(ResolvedBaseSnapshotBuild {
                snapshot_key,
                head_commit: None,
                config_fingerprint,
            })
        }
    }
}

pub(super) fn base_snapshot_config_fingerprint(build_config: &BuildConfig) -> String {
    format!(
        "{}-{}",
        build_config_fingerprint(build_config),
        SEGMENT_SCHEMA_FINGERPRINT
    )
}

pub(super) fn reusable_snapshot_metadata(
    build_config: &BuildConfig,
    resolved: &ResolvedBaseSnapshotBuild,
) -> Result<Option<WorkspaceBaseSnapshotMetadata>> {
    let Some(metadata) = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(build_config)? else {
        return Ok(None);
    };
    if metadata.snapshot_key != resolved.snapshot_key
        || metadata.tokenizer != Some(build_config.tokenizer)
        || metadata.config_fingerprint.as_deref() != Some(&resolved.config_fingerprint)
        || metadata.repo_root.as_deref() != Some(&canonical_repo_root(&build_config.repo_path))
        || metadata.doc_count.is_none()
        || metadata.head_commit != resolved.head_commit
    {
        return Ok(None);
    }
    let Some(index) =
        WorkspaceBaseSnapshotIndex::open_if_matching(build_config, &resolved.snapshot_key)?
    else {
        return Ok(None);
    };
    if index.open_committed().is_err() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

pub(super) fn base_snapshot_shard_id(logical_path: &str) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(logical_path.as_bytes());
    hasher.finalize() % BASE_SNAPSHOT_SHARD_COUNT as u32
}

fn base_snapshot_shard_fingerprint(files: &[BaseSnapshotInputFile]) -> String {
    let mut left = crc32fast::Hasher::new();
    let mut right = crc32fast::Hasher::new();
    left.update(b"base-shard-fingerprint-v1:left");
    right.update(b"base-shard-fingerprint-v1:right");
    for file in files {
        update_base_snapshot_shard_hasher(&mut left, file, b"L");
        update_base_snapshot_shard_hasher(&mut right, file, b"R");
    }
    format!("{:08x}{:08x}", left.finalize(), right.finalize())
}

fn update_base_snapshot_shard_hasher(
    hasher: &mut crc32fast::Hasher,
    file: &BaseSnapshotInputFile,
    kind_prefix: &[u8],
) {
    hasher.update(kind_prefix);
    hasher.update(file.logical_path.as_bytes());
    hasher.update(&[0]);
    hasher.update(&file.size.to_le_bytes());
    match &file.fingerprint {
        BaseSnapshotFileFingerprint::Worktree { mtime_nanos, .. } => {
            hasher.update(b"worktree");
            hasher.update(&mtime_nanos.to_le_bytes());
        }
        BaseSnapshotFileFingerprint::GitObject { object_id, size } => {
            hasher.update(b"git-object");
            hasher.update(object_id.as_bytes());
            hasher.update(&size.to_le_bytes());
        }
    }
}

fn link_segment_dir_into_staging(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    fs::create_dir_all(dest_dir)?;
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let source = entry.path();
        if entry.file_type()?.is_dir() {
            return Err(AppError::InvalidIndex(format!(
                "unexpected nested directory in base segment {}",
                source.display()
            )));
        }
        let destination = dest_dir.join(entry.file_name());
        match fs::hard_link(&source, &destination) {
            Ok(()) => {}
            Err(_) => {
                fs::copy(&source, &destination)?;
            }
        }
    }
    Ok(())
}

fn repo_snapshot_fingerprint(build_config: &BuildConfig) -> Result<String> {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(canonical_repo_root(&build_config.repo_path).as_bytes());
    let files = scan_paths(
        std::slice::from_ref(&build_config.repo_path),
        Some(&build_config.storage_root),
        ScanOptions {
            respect_ignore: matches!(build_config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: build_config.include_hidden,
            max_file_size: build_config.max_file_size,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )?;
    for file in files {
        hasher.update(repo_relative_path(&file.path, &build_config.repo_path).as_bytes());
        hasher.update(&file.size.to_le_bytes());
        hasher.update(&file.mtime_nanos.to_le_bytes());
    }
    Ok(format!("{:08x}", hasher.finalize()))
}
