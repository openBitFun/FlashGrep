use super::*;

#[derive(Clone)]
pub(super) struct BaseSnapshotInputFile {
    pub(super) path: PathBuf,
    pub(super) logical_path: String,
    pub(super) size: u64,
    pub(super) mtime_nanos: u64,
    pub(super) source: BaseSnapshotFileSource,
    pub(super) fingerprint: BaseSnapshotFileFingerprint,
}

#[derive(Clone)]
pub(super) enum BaseSnapshotFileSource {
    Worktree,
    GitObject { object_id: String },
}

#[derive(Clone)]
pub(super) enum BaseSnapshotFileFingerprint {
    Worktree { mtime_nanos: u64 },
    GitObject { object_id: String, size: u64 },
}

pub(super) struct PreparedBaseDocument {
    pub(super) document: IndexDocument,
    pub(super) posting_terms: Vec<u64>,
    pub(super) source_kind: SegmentDocSourceKind,
}

pub(super) fn base_snapshot_input_files_from_repository_files(
    files: &[RepositoryFile],
    repo_root: &Path,
) -> Vec<BaseSnapshotInputFile> {
    files
        .iter()
        .map(|file| BaseSnapshotInputFile {
            path: file.path.clone(),
            logical_path: repo_relative_path(&file.path, repo_root),
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            source: BaseSnapshotFileSource::Worktree,
            fingerprint: BaseSnapshotFileFingerprint::Worktree {
                mtime_nanos: file.mtime_nanos,
            },
        })
        .collect()
}

pub(super) fn build_head_snapshot_input_files(
    build_config: &BuildConfig,
    head_commit: &str,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<BaseSnapshotInputFile>> {
    let repo_root = &build_config.repo_path;
    let started = std::time::Instant::now();
    let tree_started = std::time::Instant::now();
    let entries = head_tracked_entries(repo_root, head_commit, should_cancel)?;
    let tracked_entries = entries.len();
    let tree_ms = tree_started.elapsed().as_millis();
    let mut files = Vec::new();
    let classify_started = std::time::Instant::now();

    for entry in entries {
        let path = crate::files::resolve_repo_path(repo_root, &entry.logical_path);
        if !crate::files::workspace_path_is_eligible(
            repo_root,
            &path,
            Some(&build_config.storage_root),
            build_config.include_hidden,
        ) || entry.size > build_config.max_file_size
        {
            continue;
        }

        if !matches!(entry.mode.as_str(), "100644" | "100755") {
            continue;
        }
        files.push(BaseSnapshotInputFile {
            path,
            logical_path: entry.logical_path,
            size: entry.size,
            mtime_nanos: 0,
            source: BaseSnapshotFileSource::GitObject {
                object_id: entry.object_id.clone(),
            },
            fingerprint: BaseSnapshotFileFingerprint::GitObject {
                object_id: entry.object_id,
                size: entry.size,
            },
        });
    }

    eprintln!(
        "[flashgrep][base-metrics] stage=enumerate_inputs elapsed_ms={} git_tree_ms={} classify_ms={} tracked_entries={} selected_files={} source_bytes={}",
        started.elapsed().as_millis(),
        tree_ms,
        classify_started.elapsed().as_millis(),
        tracked_entries,
        files.len(),
        files.iter().map(|file| file.size).sum::<u64>(),
    );

    Ok(files)
}

pub(super) fn prepare_base_document(
    repo_id: &RepoId,
    repo_root: &Path,
    file: &BaseSnapshotInputFile,
    tokenizer: &PostingTokenizerConfig,
    git_blob_reader: Option<&GitBlobReader>,
) -> Result<Option<PreparedBaseDocument>> {
    let Some(document) =
        repository_file_to_index_document(repo_id, repo_root, file, git_blob_reader)?
    else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&document.source_bytes);
    let mut posting_terms = collect_document_posting_terms_from_text(text.as_ref(), tokenizer);
    posting_terms.sort_unstable();
    posting_terms.dedup();
    Ok(Some(PreparedBaseDocument {
        document,
        posting_terms,
        source_kind: SegmentDocSourceKind::Inline,
    }))
}

pub(super) fn should_parallel_prepare_base_documents(files: &[BaseSnapshotInputFile]) -> bool {
    if std::thread::available_parallelism().map_or(1, std::num::NonZero::get) <= 1 {
        return false;
    }

    files.len() >= BASE_SNAPSHOT_PREPARE_PARALLEL_DOC_THRESHOLD
        || files.iter().map(|file| file.size).sum::<u64>()
            >= BASE_SNAPSHOT_PREPARE_PARALLEL_BYTE_THRESHOLD
}

fn read_base_snapshot_file_source_bytes(
    file: &BaseSnapshotInputFile,
    git_blob_reader: Option<&GitBlobReader>,
) -> Result<Vec<u8>> {
    match &file.source {
        BaseSnapshotFileSource::Worktree => Ok(fs::read(&file.path)?),
        BaseSnapshotFileSource::GitObject { object_id } => git_blob_reader
            .ok_or_else(|| {
                AppError::InvalidIndex(
                    "Git-backed base input is missing its shard-local batch reader".into(),
                )
            })?
            .read_object(object_id),
    }
}

fn repository_file_to_index_document(
    repo_id: &RepoId,
    _repo_root: &Path,
    file: &BaseSnapshotInputFile,
    git_blob_reader: Option<&GitBlobReader>,
) -> Result<Option<IndexDocument>> {
    let logical_path = file.logical_path.clone();
    let key = DocKey {
        repo_id: repo_id.clone(),
        logical_path: logical_path.clone(),
    };
    let source_bytes =
        Arc::<[u8]>::from(read_base_snapshot_file_source_bytes(file, git_blob_reader)?);
    if has_binary_source_bytes(&source_bytes) {
        return Ok(None);
    }
    let mut content_hash = [0u8; 32];
    let crc = crc32fast::hash(&source_bytes);
    content_hash[..4].copy_from_slice(&crc.to_le_bytes());
    let file_name = file
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::Protocol(format!("invalid file name: {}", file.path.display())))?
        .to_string();

    Ok(Some(IndexDocument {
        key,
        repo_id: repo_id.clone(),
        logical_path,
        file_name,
        language: file
            .path
            .extension()
            .map(|ext| ext.to_string_lossy().to_ascii_lowercase()),
        size: file.size,
        modified_at: file.mtime_nanos,
        content_hash,
        source_bytes,
    }))
}

fn has_binary_source_bytes(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].contains(&0)
}
