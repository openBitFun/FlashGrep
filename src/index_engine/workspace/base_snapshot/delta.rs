use std::{
    path::Path,
    sync::{atomic::AtomicU64, atomic::Ordering, Arc},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    files::resolve_repo_path,
    git_snapshot::{commit_diff_paths, resolve_git_head, GitBlobReader},
    index_engine::{
        config::PostingTokenizerConfig,
        delete::{DeleteRecord, DeleteStore, FileDeleteStore},
        manifest::{
            DeleteSegmentRef, FileManifestStore, ManifestSource, ManifestStore, SegmentRef,
        },
        segment::{
            DeleteSegmentBuildOutput, DeleteSegmentMeta, FileSegmentStore, SegmentBuildOutput,
            SegmentMeta, SegmentStore,
        },
        DeleteSegmentId, DocKey, IndexDocument, ManifestId, RepoId, SegmentId,
    },
};

use super::{now_unix_ms, BaseSnapshotBuildLock, WorkspaceBaseSnapshotIndex};

static NEXT_CANDIDATE_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaseAdvanceResult {
    pub from_head: String,
    pub to_head: String,
    pub added_paths: usize,
    pub modified_paths: usize,
    pub deleted_paths: usize,
    pub enumerated_paths: usize,
    pub filtered_paths: usize,
    pub git_blobs_read: usize,
    pub tokenized_documents: usize,
    pub deleted_documents: usize,
    pub live_segments_written: usize,
    pub delete_segments_written: usize,
    pub delta_bytes: u64,
    pub parent_manifest_id: ManifestId,
    pub manifest_id: ManifestId,
    pub cache_hit: bool,
}

impl WorkspaceBaseSnapshotIndex {
    pub(crate) fn advance_to_head(
        build_config: &BuildConfig,
        repo_id: &RepoId,
        target_head: &str,
        head_cache_entries: usize,
        should_cancel: &mut impl FnMut() -> bool,
    ) -> Result<BaseAdvanceResult> {
        let _build_lock = BaseSnapshotBuildLock::acquire(build_config, should_cancel)?;
        let index_root = Self::index_root(build_config);
        let manifest_store = FileManifestStore::open(&index_root)?;
        let parent = manifest_store.load_current()?;
        let ManifestSource::GitBase {
            head_commit: from_head,
            config_fingerprint,
            repo_root,
            document_count,
            foundation_segment_ids,
            delta_depth,
            delta_bytes,
            ..
        } = parent.source.clone().ok_or_else(|| {
            AppError::InvalidIndex("base manifest is missing Git source identity".into())
        })?
        else {
            return Err(AppError::InvalidIndex(
                "active manifest is not a Git base manifest".into(),
            ));
        };
        if from_head == target_head {
            return Ok(BaseAdvanceResult {
                from_head,
                to_head: target_head.to_string(),
                added_paths: 0,
                modified_paths: 0,
                deleted_paths: 0,
                enumerated_paths: 0,
                filtered_paths: 0,
                git_blobs_read: 0,
                tokenized_documents: 0,
                deleted_documents: 0,
                live_segments_written: 0,
                delete_segments_written: 0,
                delta_bytes: 0,
                parent_manifest_id: parent.manifest_id.clone(),
                manifest_id: parent.manifest_id,
                cache_hit: false,
            });
        }
        if let Some(manifest_id) =
            Self::activate_cached_head(build_config, target_head, head_cache_entries)?
        {
            return Ok(BaseAdvanceResult {
                from_head,
                to_head: target_head.to_string(),
                added_paths: 0,
                modified_paths: 0,
                deleted_paths: 0,
                enumerated_paths: 0,
                filtered_paths: 0,
                git_blobs_read: 0,
                tokenized_documents: 0,
                deleted_documents: 0,
                live_segments_written: 0,
                delete_segments_written: 0,
                delta_bytes: 0,
                parent_manifest_id: parent.manifest_id,
                manifest_id,
                cache_hit: true,
            });
        }

        let diff = commit_diff_paths(
            &build_config.repo_path,
            &from_head,
            target_head,
            should_cancel,
        )?;
        let added_paths = diff.added.len();
        let modified_paths = diff.modified.len();
        let deleted_paths = diff.deleted.len();
        let enumerated_paths = added_paths + modified_paths + deleted_paths;
        let git_blobs_read = added_paths + deleted_paths + modified_paths.saturating_mul(2);
        let blob_reader = GitBlobReader::new(&build_config.repo_path)?;

        let mut old_live_paths = Vec::new();
        for path in diff.deleted.iter().chain(diff.modified.iter()) {
            check_cancel(should_cancel)?;
            let bytes = blob_reader.read_blob(&from_head, path)?;
            if is_indexable_blob(build_config, path, &bytes) {
                old_live_paths.push(path.clone());
            }
        }

        let mut documents = Vec::new();
        for path in diff.added.iter().chain(diff.modified.iter()) {
            check_cancel(should_cancel)?;
            let bytes = blob_reader.read_blob(target_head, path)?;
            if let Some(document) = index_document_from_blob(build_config, repo_id, path, bytes) {
                documents.push(document);
            }
        }
        documents.sort_by(|left, right| left.key.cmp(&right.key));
        let filtered_paths = added_paths
            .saturating_add(modified_paths)
            .saturating_sub(documents.len());
        old_live_paths.sort_unstable();
        old_live_paths.dedup();

        let delete_seq = parent.committed_seq_no.saturating_add(1);
        let add_seq = delete_seq.saturating_add(u64::from(!old_live_paths.is_empty()));
        let transition_id = format!("{}-{}-{}", from_head, target_head, next_candidate_nonce());
        let segment_store =
            FileSegmentStore::open(&index_root, PostingTokenizerConfig::from(build_config))?;
        let delete_store = FileDeleteStore::open(&index_root)?;

        let live_segment = write_live_delta(&segment_store, &transition_id, add_seq, documents)?;
        let delete_segment = write_delete_delta(
            &delete_store,
            repo_id,
            &transition_id,
            delete_seq,
            old_live_paths,
        )?;

        let mut active_segments = parent.active_segments.clone();
        if let Some(segment) = live_segment.clone() {
            active_segments.push(segment);
        }
        let mut active_delete_segments = parent.active_delete_segments.clone();
        if let Some(segment) = delete_segment.clone() {
            active_delete_segments.push(segment);
        }
        let written_delta_bytes = live_segment
            .as_ref()
            .map_or(0, |segment| segment.size_bytes)
            .saturating_add(
                delete_segment
                    .as_ref()
                    .map_or(0, |segment| segment.size_bytes),
            );
        let tokenized_documents = live_segment
            .as_ref()
            .map_or(0, |segment| segment.doc_count as usize);
        let deleted_documents = delete_segment
            .as_ref()
            .map_or(0, |segment| segment.delete_count as usize);
        let next_document_count = document_count
            .saturating_sub(deleted_documents)
            .saturating_add(tokenized_documents);
        let manifest_id = ManifestId(format!("manifest-base-{transition_id}"));
        let candidate = crate::index_engine::manifest::Manifest {
            manifest_id: manifest_id.clone(),
            generation: parent.generation.saturating_add(1),
            created_at_unix_ms: now_unix_ms(),
            committed_seq_no: add_seq.max(delete_seq),
            refreshed_seq_no: add_seq.max(delete_seq),
            writer_epoch: parent.writer_epoch,
            schema_fingerprint: parent.schema_fingerprint.clone(),
            active_segments,
            active_delete_segments,
            source: Some(ManifestSource::GitBase {
                head_commit: target_head.to_string(),
                config_fingerprint,
                repo_root,
                document_count: next_document_count,
                parent_manifest_id: Some(parent.manifest_id.clone()),
                foundation_segment_ids,
                delta_depth: delta_depth.saturating_add(1),
                delta_bytes: delta_bytes.saturating_add(written_delta_bytes),
            }),
        };
        manifest_store.write_candidate(&candidate)?;
        check_cancel(should_cancel)?;
        if resolve_git_head(&build_config.repo_path, should_cancel)?.as_deref() != Some(target_head)
        {
            Self::record_manifest_in_cache(build_config, &candidate, head_cache_entries)?;
            return Err(AppError::Cancelled);
        }
        if !manifest_store.activate_candidate_if_current(&parent.manifest_id, &manifest_id)? {
            return Err(AppError::Protocol(format!(
                "base manifest changed while advancing {} to {}",
                from_head, target_head
            )));
        }
        Self::record_manifest_in_cache(build_config, &candidate, head_cache_entries)?;

        Ok(BaseAdvanceResult {
            from_head,
            to_head: target_head.to_string(),
            added_paths,
            modified_paths,
            deleted_paths,
            enumerated_paths,
            filtered_paths,
            git_blobs_read,
            tokenized_documents,
            deleted_documents,
            live_segments_written: usize::from(live_segment.is_some()),
            delete_segments_written: usize::from(delete_segment.is_some()),
            delta_bytes: written_delta_bytes,
            parent_manifest_id: parent.manifest_id,
            manifest_id,
            cache_hit: false,
        })
    }
}

fn write_live_delta(
    store: &FileSegmentStore,
    transition_id: &str,
    seq_no: u64,
    documents: Vec<IndexDocument>,
) -> Result<Option<SegmentRef>> {
    if documents.is_empty() {
        return Ok(None);
    }
    let id = SegmentId(format!("seg-delta-{transition_id}"));
    let size_bytes = documents
        .iter()
        .map(|document| document.source_bytes.len() as u64)
        .sum();
    let meta = SegmentMeta {
        segment_id: id.clone(),
        min_seq_no: seq_no,
        max_seq_no: seq_no,
        doc_count: documents.len() as u32,
        live_doc_count: documents.len() as u32,
        created_by_merge: false,
    };
    store.write_segment(SegmentBuildOutput {
        meta: meta.clone(),
        documents,
    })?;
    Ok(Some(SegmentRef {
        id,
        min_seq_no: seq_no,
        max_seq_no: seq_no,
        doc_count: meta.doc_count,
        size_bytes,
    }))
}

fn write_delete_delta(
    store: &FileDeleteStore,
    repo_id: &RepoId,
    transition_id: &str,
    seq_no: u64,
    paths: Vec<String>,
) -> Result<Option<DeleteSegmentRef>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let id = DeleteSegmentId(format!("del-delta-{transition_id}"));
    let records = paths
        .into_iter()
        .map(|logical_path| DeleteRecord {
            seq_no,
            key: DocKey {
                repo_id: repo_id.clone(),
                logical_path,
            },
        })
        .collect::<Vec<_>>();
    let size_bytes = records
        .iter()
        .map(|record| (record.key.repo_id.0.len() + record.key.logical_path.len() + 8) as u64)
        .sum();
    let meta = DeleteSegmentMeta {
        delete_segment_id: id.clone(),
        min_seq_no: seq_no,
        max_seq_no: seq_no,
        delete_count: records.len() as u32,
    };
    store.write_delete_segment(DeleteSegmentBuildOutput {
        meta: meta.clone(),
        records,
    })?;
    Ok(Some(DeleteSegmentRef {
        id,
        min_seq_no: seq_no,
        max_seq_no: seq_no,
        delete_count: meta.delete_count,
        size_bytes,
    }))
}

fn index_document_from_blob(
    build_config: &BuildConfig,
    repo_id: &RepoId,
    logical_path: &str,
    bytes: Vec<u8>,
) -> Option<IndexDocument> {
    if !is_indexable_blob(build_config, logical_path, &bytes) {
        return None;
    }
    let path = resolve_repo_path(&build_config.repo_path, logical_path);
    let mut content_hash = [0u8; 32];
    content_hash[..4].copy_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
    Some(IndexDocument {
        key: DocKey {
            repo_id: repo_id.clone(),
            logical_path: logical_path.to_string(),
        },
        repo_id: repo_id.clone(),
        logical_path: logical_path.to_string(),
        file_name: path.file_name()?.to_string_lossy().into_owned(),
        language: path
            .extension()
            .map(|extension| extension.to_string_lossy().to_ascii_lowercase()),
        size: bytes.len() as u64,
        modified_at: 0,
        content_hash,
        source_bytes: Arc::from(bytes),
    })
}

fn is_indexable_blob(build_config: &BuildConfig, logical_path: &str, bytes: &[u8]) -> bool {
    bytes.len() as u64 <= build_config.max_file_size
        && (build_config.include_hidden || !path_is_hidden(Path::new(logical_path)))
        && !bytes[..bytes.len().min(8 * 1024)].contains(&0)
}

fn path_is_hidden(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(value) => value.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn check_cancel(should_cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if should_cancel() {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn next_candidate_nonce() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT_CANDIDATE_NONCE.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp:032x}-{sequence:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        base_snapshot_build::BaseSnapshotBuildOptions,
        config::{CorpusMode, TokenizerMode},
    };
    use std::{fs, process::Command};
    use tempfile::TempDir;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("git should start");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn head(repo: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git should start");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("head should be utf8")
            .trim()
            .to_string()
    }

    fn config(temp: &TempDir, repo: &Path) -> BuildConfig {
        BuildConfig {
            repo_path: repo.to_path_buf(),
            storage_root: temp.path().join("storage"),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::NoIgnore,
            include_hidden: false,
            max_file_size: 64,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    fn initialize_repo(repo: &Path) {
        fs::create_dir_all(repo).expect("repo dir should succeed");
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "flashgrep@example.invalid"]);
        git(repo, &["config", "user.name", "Flashgrep Test"]);
    }

    #[test]
    fn advance_tokenizes_only_target_side_changed_corpus_and_orders_replacements() {
        let temp = TempDir::new().expect("tempdir should succeed");
        let repo = temp.path().join("repo");
        initialize_repo(&repo);
        fs::write(repo.join("live.rs"), "const VALUE: &str = \"OLD\";\n")
            .expect("write should succeed");
        fs::write(repo.join("delete.rs"), "const DELETE: &str = \"OLD\";\n")
            .expect("write should succeed");
        fs::write(repo.join(".hidden.rs"), "hidden old\n").expect("write should succeed");
        fs::write(repo.join("binary.dat"), b"old\0binary").expect("write should succeed");
        fs::write(repo.join("large.txt"), vec![b'x'; 65]).expect("write should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "base"]);

        let build_config = config(&temp, &repo);
        let repo_id = RepoId("repo".into());
        WorkspaceBaseSnapshotIndex::build_with_options(
            &build_config,
            &repo_id,
            BaseSnapshotBuildOptions::new().requiring_git_head(),
        )
        .expect("base build should succeed");
        let store = FileManifestStore::open(&WorkspaceBaseSnapshotIndex::index_root(&build_config))
            .expect("manifest store should open");
        let foundation_ids = store
            .load_current()
            .expect("initial manifest should load")
            .active_segments
            .into_iter()
            .map(|segment| segment.id)
            .collect::<Vec<_>>();

        fs::write(repo.join("live.rs"), "const VALUE: &str = \"NEW\";\n")
            .expect("rewrite should succeed");
        fs::remove_file(repo.join("delete.rs")).expect("delete should succeed");
        fs::write(repo.join("add.rs"), "const ADD: &str = \"NEW\";\n").expect("add should succeed");
        fs::write(repo.join(".hidden.rs"), "hidden new\n").expect("write should succeed");
        fs::write(repo.join("binary.dat"), b"new\0binary").expect("write should succeed");
        fs::write(repo.join("large.txt"), vec![b'y'; 66]).expect("write should succeed");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "target"]);
        let target = head(&repo);

        let result = WorkspaceBaseSnapshotIndex::advance_to_head(
            &build_config,
            &repo_id,
            &target,
            4,
            &mut || false,
        )
        .expect("advance should succeed");
        assert_eq!(result.enumerated_paths, 6);
        assert_eq!(
            (
                result.added_paths,
                result.modified_paths,
                result.deleted_paths
            ),
            (1, 4, 1)
        );
        assert_eq!(result.filtered_paths, 3);
        assert_eq!(result.git_blobs_read, 10);
        assert_eq!(result.tokenized_documents, 2);
        assert_eq!(result.deleted_documents, 2);
        assert_eq!(
            (result.live_segments_written, result.delete_segments_written),
            (1, 1)
        );

        let manifest = store.load_current().expect("current manifest should load");
        let live = manifest
            .active_segments
            .iter()
            .find(|segment| segment.id.0.starts_with("seg-delta-"))
            .expect("live delta should exist");
        let deletes = manifest
            .active_delete_segments
            .iter()
            .find(|segment| segment.id.0.starts_with("del-delta-"))
            .expect("delete delta should exist");
        assert!(deletes.max_seq_no < live.min_seq_no);
        assert!(matches!(
            manifest.source,
            Some(ManifestSource::GitBase {
                document_count: 2,
                delta_depth: 1,
                foundation_segment_ids,
                ..
            }) if foundation_segment_ids == foundation_ids
        ));
    }

    #[test]
    fn stale_target_candidate_is_cached_but_never_activated() {
        let temp = TempDir::new().expect("tempdir should succeed");
        let repo = temp.path().join("repo");
        initialize_repo(&repo);
        fs::write(repo.join("value.rs"), "const VALUE: &str = \"H0\";\n")
            .expect("write should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "h0"]);
        let h0 = head(&repo);
        let build_config = config(&temp, &repo);
        let repo_id = RepoId("repo".into());
        WorkspaceBaseSnapshotIndex::build_with_options(
            &build_config,
            &repo_id,
            BaseSnapshotBuildOptions::new().requiring_git_head(),
        )
        .expect("base build should succeed");

        git(&repo, &["checkout", "-q", "-b", "h1"]);
        fs::write(repo.join("value.rs"), "const VALUE: &str = \"H1\";\n")
            .expect("write should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "h1"]);
        let h1 = head(&repo);
        git(&repo, &["checkout", "-q", "-b", "h2", &h0]);
        fs::write(repo.join("value.rs"), "const VALUE: &str = \"H2\";\n")
            .expect("write should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "h2"]);
        let h2 = head(&repo);
        git(&repo, &["checkout", "-q", "h1"]);

        let manifests = WorkspaceBaseSnapshotIndex::index_root(&build_config).join("manifests");
        let initial_manifest_count = fs::read_dir(&manifests)
            .expect("manifest directory should exist")
            .count();
        let mut moved_to_h2 = false;
        let result = WorkspaceBaseSnapshotIndex::advance_to_head(
            &build_config,
            &repo_id,
            &h1,
            4,
            &mut || {
                let candidate_exists = fs::read_dir(&manifests)
                    .is_ok_and(|entries| entries.count() > initial_manifest_count);
                if candidate_exists && !moved_to_h2 {
                    git(&repo, &["checkout", "-q", "h2"]);
                    moved_to_h2 = true;
                }
                false
            },
        );
        assert!(matches!(result, Err(AppError::Cancelled)));
        assert!(moved_to_h2);
        let store = FileManifestStore::open(&WorkspaceBaseSnapshotIndex::index_root(&build_config))
            .expect("manifest store should open");
        let current = store.load_current().expect("current manifest should load");
        assert!(matches!(
            current.source,
            Some(ManifestSource::GitBase { head_commit, .. }) if head_commit == h0
        ));
        assert!(
            WorkspaceBaseSnapshotIndex::cached_manifest_ids(&build_config)
                .into_iter()
                .any(|id| store.load_manifest(&id).is_ok_and(|manifest| matches!(
                    manifest.source,
                    Some(ManifestSource::GitBase { head_commit, .. }) if head_commit == h1
                )))
        );
        assert_eq!(head(&repo), h2);
    }

    #[test]
    fn cancellation_after_candidate_write_keeps_parent_active() {
        let temp = TempDir::new().expect("tempdir should succeed");
        let repo = temp.path().join("repo");
        initialize_repo(&repo);
        fs::write(repo.join("value.rs"), "const VALUE: &str = \"PARENT\";\n")
            .expect("write should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "parent"]);
        let parent_head = head(&repo);
        let build_config = config(&temp, &repo);
        let repo_id = RepoId("repo".into());
        WorkspaceBaseSnapshotIndex::build_with_options(
            &build_config,
            &repo_id,
            BaseSnapshotBuildOptions::new().requiring_git_head(),
        )
        .expect("base build should succeed");
        fs::write(repo.join("value.rs"), "const VALUE: &str = \"TARGET\";\n")
            .expect("rewrite should succeed");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "target"]);
        let target = head(&repo);

        let manifests = WorkspaceBaseSnapshotIndex::index_root(&build_config).join("manifests");
        let initial_manifest_count = fs::read_dir(&manifests)
            .expect("manifest directory should exist")
            .count();
        let result = WorkspaceBaseSnapshotIndex::advance_to_head(
            &build_config,
            &repo_id,
            &target,
            4,
            &mut || {
                fs::read_dir(&manifests)
                    .is_ok_and(|entries| entries.count() > initial_manifest_count)
            },
        );
        assert!(matches!(result, Err(AppError::Cancelled)));
        let store = FileManifestStore::open(&WorkspaceBaseSnapshotIndex::index_root(&build_config))
            .expect("manifest store should open");
        assert!(matches!(
            store.load_current().expect("current should load").source,
            Some(ManifestSource::GitBase { head_commit, .. }) if head_commit == parent_head
        ));
    }
}
