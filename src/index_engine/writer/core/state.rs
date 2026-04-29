use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::index_engine::{
    manifest::{DeleteSegmentRef, Manifest, SegmentRef},
    reader::state::{SnapshotData, VersionedDocument},
    types::{DocKey, SeqNo},
};

use super::IndexDocument;

#[derive(Debug, Clone)]
pub(super) struct PendingDocument {
    pub(super) seq_no: SeqNo,
    pub(super) document: IndexDocument,
}

#[derive(Debug, Default)]
pub(super) struct WriterState {
    pub(super) generation: u64,
    pub(super) committed_seq_no: SeqNo,
    pub(super) last_flushed_seq_no: SeqNo,
    pub(super) last_seq_no: SeqNo,
    pub(super) pending_docs: HashMap<DocKey, PendingDocument>,
    pub(super) pending_delete_seq_by_key: HashMap<DocKey, SeqNo>,
    pub(super) history: HashMap<DocKey, Vec<DocHistoryEntry>>,
    pub(super) active_segments: Vec<SegmentRef>,
    pub(super) active_delete_segments: Vec<DeleteSegmentRef>,
    pub(super) pending_segments: Vec<SegmentRef>,
    pub(super) pending_delete_segments: Vec<DeleteSegmentRef>,
}

#[derive(Debug, Clone)]
pub(super) struct DocHistoryEntry {
    pub(super) seq_no: SeqNo,
    pub(super) document: Option<IndexDocument>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum SnapshotKind {
    Committed,
    Refreshed,
}

pub(super) fn build_snapshot(
    state: &WriterState,
    generation: u64,
    committed_seq_no: SeqNo,
    refreshed_seq_no: SeqNo,
    snapshot_kind: SnapshotKind,
) -> SnapshotData {
    let mut docs = BTreeMap::new();
    let mut deleted_keys = BTreeSet::new();

    for (key, entries) in &state.history {
        let Some(latest) = latest_state_entry(entries, refreshed_seq_no) else {
            continue;
        };
        if latest.seq_no <= state.last_flushed_seq_no {
            continue;
        }
        if let Some(document) = latest.document.clone() {
            docs.insert(
                key.clone(),
                VersionedDocument {
                    seq_no: latest.seq_no,
                    document,
                },
            );
        } else {
            deleted_keys.insert(key.clone());
        }
    }

    SnapshotData {
        manifest: Manifest {
            manifest_id: next_manifest_id(generation),
            generation,
            created_at_unix_ms: now_unix_ms(),
            committed_seq_no,
            refreshed_seq_no,
            writer_epoch: 0,
            schema_fingerprint: "segment-publish-v2".into(),
            active_segments: visible_segment_refs(state, snapshot_kind),
            active_delete_segments: visible_delete_segment_refs(state, snapshot_kind),
        },
        docs,
        deleted_keys,
        ..SnapshotData::default()
    }
}

pub(super) fn next_manifest_id(generation: u64) -> crate::index_engine::types::ManifestId {
    crate::index_engine::types::ManifestId(format!("manifest-{generation:016}"))
}

pub(super) fn promote_pending_segments(state: &mut WriterState) {
    if !state.pending_segments.is_empty() {
        state.active_segments.append(&mut state.pending_segments);
    }
    if !state.pending_delete_segments.is_empty() {
        state
            .active_delete_segments
            .append(&mut state.pending_delete_segments);
    }
}

pub(super) fn apply_add(state: &mut WriterState, seq_no: SeqNo, doc: IndexDocument) {
    state.pending_delete_seq_by_key.remove(&doc.key);
    state.pending_docs.insert(
        doc.key.clone(),
        PendingDocument {
            seq_no,
            document: doc.clone(),
        },
    );
    state
        .history
        .entry(doc.key.clone())
        .or_default()
        .push(DocHistoryEntry {
            seq_no,
            document: Some(doc),
        });
    state.last_seq_no = state.last_seq_no.max(seq_no);
}

pub(super) fn apply_delete(state: &mut WriterState, seq_no: SeqNo, key: &DocKey) {
    state.pending_docs.remove(key);
    state.pending_delete_seq_by_key.insert(key.clone(), seq_no);
    state
        .history
        .entry(key.clone())
        .or_default()
        .push(DocHistoryEntry {
            seq_no,
            document: None,
        });
    state.last_seq_no = state.last_seq_no.max(seq_no);
}

pub(super) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
        .unwrap_or_default()
}

fn visible_segment_refs(state: &WriterState, snapshot_kind: SnapshotKind) -> Vec<SegmentRef> {
    let mut segments = state.active_segments.clone();
    if matches!(snapshot_kind, SnapshotKind::Refreshed) {
        segments.extend(state.pending_segments.clone());
    }
    segments
}

fn visible_delete_segment_refs(
    state: &WriterState,
    snapshot_kind: SnapshotKind,
) -> Vec<DeleteSegmentRef> {
    let mut segments = state.active_delete_segments.clone();
    if matches!(snapshot_kind, SnapshotKind::Refreshed) {
        segments.extend(state.pending_delete_segments.clone());
    }
    segments
}

fn latest_state_entry(entries: &[DocHistoryEntry], upper_bound: SeqNo) -> Option<&DocHistoryEntry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.seq_no <= upper_bound)
}
