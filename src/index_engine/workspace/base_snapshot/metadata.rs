use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WorkspaceBaseSnapshotMetadataFile {
    pub(super) snapshot_key: String,
    #[serde(default)]
    pub(super) revision: Option<GitRevision>,
    #[serde(default)]
    pub(super) tokenizer: Option<String>,
    #[serde(default)]
    pub(super) doc_count: Option<usize>,
    #[serde(default)]
    pub(super) head_commit: Option<String>,
    #[serde(default)]
    pub(super) config_fingerprint: Option<String>,
    #[serde(default)]
    pub(super) repo_root: Option<String>,
    #[serde(default)]
    pub(super) shards: Vec<WorkspaceBaseSnapshotShardMetadataFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WorkspaceBaseSnapshotShardMetadataFile {
    pub(super) shard_id: u32,
    pub(super) fingerprint: String,
    pub(super) segment_id: String,
    pub(super) doc_count: usize,
    pub(super) size_bytes: u64,
}

pub(super) fn write_marker_for_root(index_root: &Path, snapshot_key: &str) -> Result<()> {
    fs::create_dir_all(index_root)?;
    fs::write(
        WorkspaceBaseSnapshotIndex::marker_path_for_root(index_root),
        format!("{snapshot_key}\n"),
    )?;
    Ok(())
}

pub(super) fn write_metadata_file_for_root(
    index_root: &Path,
    metadata: &WorkspaceBaseSnapshotMetadata,
) -> Result<()> {
    fs::create_dir_all(index_root)?;
    let encoded = serde_json::to_vec_pretty(&WorkspaceBaseSnapshotMetadataFile {
        snapshot_key: metadata.snapshot_key.clone(),
        revision: metadata.revision.clone(),
        tokenizer: metadata.tokenizer.map(|mode| mode.as_str().to_string()),
        doc_count: metadata.doc_count,
        head_commit: metadata.head_commit.clone(),
        config_fingerprint: metadata.config_fingerprint.clone(),
        repo_root: metadata.repo_root.clone(),
        shards: metadata
            .shards
            .iter()
            .map(|shard| WorkspaceBaseSnapshotShardMetadataFile {
                shard_id: shard.shard_id,
                fingerprint: shard.fingerprint.clone(),
                segment_id: shard.segment_id.clone(),
                doc_count: shard.doc_count,
                size_bytes: shard.size_bytes,
            })
            .collect(),
    })
    .map_err(|error| {
        AppError::Protocol(format!(
            "failed to encode base snapshot metadata for {}: {error}",
            index_root.display()
        ))
    })?;
    fs::write(
        WorkspaceBaseSnapshotIndex::metadata_path_for_root(index_root),
        encoded,
    )?;
    Ok(())
}

pub(super) fn parse_snapshot_revision(snapshot_key: &str) -> Option<GitRevision> {
    let suffix = snapshot_key.strip_prefix("base-git-")?;
    let (head_commit, _) = suffix.rsplit_once('-')?;
    (!head_commit.is_empty()).then(|| GitRevision(head_commit.to_string()))
}

pub(super) fn parse_tokenizer_mode(value: &str) -> Option<TokenizerMode> {
    match value {
        "trigram" => Some(TokenizerMode::Trigram),
        "sparse-ngram" => Some(TokenizerMode::SparseNgram),
        _ => None,
    }
}
