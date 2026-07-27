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
    GitHeadBlob { head_commit: String },
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
    repo_root: &Path,
    head_commit: &str,
    include_hidden: bool,
    max_file_size: u64,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<BaseSnapshotInputFile>> {
    let started = std::time::Instant::now();
    let dirty_started = std::time::Instant::now();
    let dirty_paths = dirty_tracked_paths(repo_root, should_cancel)?;
    let dirty_ms = dirty_started.elapsed().as_millis();
    let tree_started = std::time::Instant::now();
    let entries = head_tracked_entries(repo_root, head_commit, should_cancel)?;
    let tracked_entries = entries.len();
    let tree_ms = tree_started.elapsed().as_millis();
    let mut files = Vec::new();
    let classify_started = std::time::Instant::now();

    for entry in entries {
        let path = crate::files::resolve_repo_path(repo_root, &entry.logical_path);
        if crate::files::is_workspace_internal_path(&path)
            || (!include_hidden && path_has_hidden_components(&path, repo_root))
            || entry.size > max_file_size
        {
            continue;
        }

        let worktree_metadata = fs::metadata(&path).ok();
        if entry.mode == "120000" {
            let Some(metadata) = worktree_metadata.filter(std::fs::Metadata::is_file) else {
                continue;
            };
            files.push(BaseSnapshotInputFile {
                path,
                logical_path: entry.logical_path,
                size: metadata.len(),
                mtime_nanos: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
                    .unwrap_or_default(),
                source: BaseSnapshotFileSource::Worktree,
                fingerprint: BaseSnapshotFileFingerprint::Worktree {
                    mtime_nanos: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
                        .unwrap_or_default(),
                },
            });
            continue;
        }
        let use_worktree = !dirty_paths.contains(&entry.logical_path)
            && worktree_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.is_file() && metadata.len() == entry.size);
        if let Some(metadata) = worktree_metadata.filter(|_| use_worktree) {
            files.push(BaseSnapshotInputFile {
                path,
                logical_path: entry.logical_path,
                size: metadata.len(),
                mtime_nanos: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
                    .unwrap_or_default(),
                source: BaseSnapshotFileSource::Worktree,
                fingerprint: BaseSnapshotFileFingerprint::GitObject {
                    object_id: entry.object_id,
                    size: metadata.len(),
                },
            });
        } else {
            files.push(BaseSnapshotInputFile {
                path,
                logical_path: entry.logical_path,
                size: entry.size,
                mtime_nanos: 0,
                source: BaseSnapshotFileSource::GitHeadBlob {
                    head_commit: head_commit.to_string(),
                },
                fingerprint: BaseSnapshotFileFingerprint::GitObject {
                    object_id: entry.object_id,
                    size: entry.size,
                },
            });
        }
    }

    eprintln!(
        "[flashgrep][base-metrics] stage=enumerate_inputs elapsed_ms={} dirty_ms={} git_tree_ms={} classify_ms={} dirty_paths={} tracked_entries={} selected_files={} source_bytes={}",
        started.elapsed().as_millis(),
        dirty_ms,
        tree_ms,
        classify_started.elapsed().as_millis(),
        dirty_paths.len(),
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
) -> Result<Option<PreparedBaseDocument>> {
    let Some(document) = repository_file_to_index_document(repo_id, repo_root, file)? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&document.source_bytes);
    let mut posting_terms = collect_document_posting_terms_from_text(text.as_ref(), tokenizer);
    posting_terms.sort_unstable();
    posting_terms.dedup();
    Ok(Some(PreparedBaseDocument {
        document,
        posting_terms,
        source_kind: segment_doc_source_kind(file),
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

fn read_base_snapshot_file_source_bytes(file: &BaseSnapshotInputFile) -> Result<Vec<u8>> {
    match &file.source {
        BaseSnapshotFileSource::Worktree => Ok(fs::read(&file.path)?),
        BaseSnapshotFileSource::GitHeadBlob { head_commit } => {
            let mut should_cancel = || false;
            read_git_blob(
                &infer_repo_root_from_logical_path(file),
                head_commit,
                &file.logical_path,
                &mut should_cancel,
            )
        }
    }
}

fn infer_repo_root_from_logical_path(file: &BaseSnapshotInputFile) -> PathBuf {
    let component_count = std::path::Path::new(&file.logical_path)
        .components()
        .count();
    let mut root = file.path.clone();
    for _ in 0..component_count {
        if let Some(parent) = root.parent() {
            root = parent.to_path_buf();
        }
    }
    root
}

fn path_has_hidden_components(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| match component {
        std::path::Component::Normal(value) => value.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn repository_file_to_index_document(
    repo_id: &RepoId,
    _repo_root: &Path,
    file: &BaseSnapshotInputFile,
) -> Result<Option<IndexDocument>> {
    let logical_path = file.logical_path.clone();
    let key = DocKey {
        repo_id: repo_id.clone(),
        logical_path: logical_path.clone(),
    };
    let source_bytes = Arc::<[u8]>::from(read_base_snapshot_file_source_bytes(file)?);
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

fn segment_doc_source_kind(file: &BaseSnapshotInputFile) -> SegmentDocSourceKind {
    match file.fingerprint {
        BaseSnapshotFileFingerprint::Worktree { .. } => SegmentDocSourceKind::Inline,
        BaseSnapshotFileFingerprint::GitObject { .. } => SegmentDocSourceKind::GitHeadBlob,
    }
}

fn has_binary_source_bytes(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].contains(&0)
}
