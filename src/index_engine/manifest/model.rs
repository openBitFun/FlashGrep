use serde::{Deserialize, Serialize};

use crate::index_engine::types::{DeleteSegmentId, ManifestId, SegmentId, SeqNo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_id: ManifestId,
    pub generation: u64,
    pub created_at_unix_ms: u64,
    pub committed_seq_no: SeqNo,
    pub refreshed_seq_no: SeqNo,
    pub writer_epoch: u64,
    pub schema_fingerprint: String,
    pub active_segments: Vec<SegmentRef>,
    pub active_delete_segments: Vec<DeleteSegmentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ManifestSource>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            manifest_id: ManifestId("manifest-0000000000000000".into()),
            generation: 0,
            created_at_unix_ms: 0,
            committed_seq_no: 0,
            refreshed_seq_no: 0,
            writer_epoch: 0,
            schema_fingerprint: "uninitialized".into(),
            active_segments: Vec::new(),
            active_delete_segments: Vec::new(),
            source: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestSource {
    GitBase {
        head_commit: String,
        config_fingerprint: String,
        repo_root: String,
        document_count: usize,
        parent_manifest_id: Option<ManifestId>,
        foundation_segment_ids: Vec<SegmentId>,
        delta_depth: u32,
        delta_bytes: u64,
    },
    WorkspaceOverlay {
        base_manifest_id: ManifestId,
        observed_head: String,
        probe_epoch: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    pub id: SegmentId,
    pub min_seq_no: SeqNo,
    pub max_seq_no: SeqNo,
    pub doc_count: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSegmentRef {
    pub id: DeleteSegmentId,
    pub min_seq_no: SeqNo,
    pub max_seq_no: SeqNo,
    pub delete_count: u32,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMeta {
    pub manifest_id: ManifestId,
    pub generation: u64,
}
