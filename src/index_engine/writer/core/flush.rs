use crate::index_engine::{
    delete::DeleteRecord,
    manifest::{DeleteSegmentRef, SegmentRef},
    reader::state::SnapshotData,
    segment::{DeleteSegmentBuildOutput, DeleteSegmentMeta, SegmentBuildOutput, SegmentMeta},
    types::{DeleteSegmentId, FlushResult, SeqNo},
};

use super::{
    state::{artifact_nonce, build_snapshot},
    WriterState,
};

#[derive(Debug, Clone)]
pub(super) struct FlushPlan {
    pub(super) segment_build: Option<SegmentBuildOutput>,
    pub(super) segment_ref: Option<SegmentRef>,
    pub(super) delete_build: Option<DeleteSegmentBuildOutput>,
    pub(super) delete_segment_ref: Option<DeleteSegmentRef>,
    pub(super) max_seq_no: SeqNo,
}

#[derive(Debug, Clone)]
pub(super) struct AppliedFlush {
    pub(super) result: FlushResult,
    pub(super) snapshot: SnapshotData,
}

pub(super) fn build_flush_plan(state: &WriterState) -> Option<FlushPlan> {
    if state.pending_docs.is_empty() && state.pending_delete_seq_by_key.is_empty() {
        return None;
    }

    let generation = state.generation + 1;
    let next_segment_ordinal = state.active_segments.len() + state.pending_segments.len() + 1;
    let next_delete_ordinal =
        state.active_delete_segments.len() + state.pending_delete_segments.len() + 1;

    let mut live_docs = state
        .pending_docs
        .values()
        .map(|pending| pending.document.clone())
        .collect::<Vec<_>>();
    live_docs.sort_by(|left, right| left.key.cmp(&right.key));

    let segment = if live_docs.is_empty() {
        None
    } else {
        let segment_id = crate::index_engine::SegmentId(format!(
            "seg-{generation:016}-{next_segment_ordinal:016}-{}",
            artifact_nonce()
        ));
        let max_seq_no = live_docs
            .iter()
            .filter_map(|doc| {
                state
                    .pending_docs
                    .get(&doc.key)
                    .map(|pending| pending.seq_no)
            })
            .max()
            .unwrap_or_default();
        let min_seq_no = live_docs
            .iter()
            .filter_map(|doc| {
                state
                    .pending_docs
                    .get(&doc.key)
                    .map(|pending| pending.seq_no)
            })
            .min()
            .unwrap_or_default();
        let meta = SegmentMeta {
            segment_id: segment_id.clone(),
            min_seq_no,
            max_seq_no,
            doc_count: live_docs.len() as u32,
            live_doc_count: live_docs.len() as u32,
            created_by_merge: false,
        };
        let segment_ref = SegmentRef {
            applied_delete_seq_no: 0,
            id: segment_id,
            min_seq_no,
            max_seq_no,
            doc_count: meta.doc_count,
            size_bytes: live_docs
                .iter()
                .map(|doc| doc.source_bytes.len() as u64)
                .sum(),
        };
        Some((
            SegmentBuildOutput {
                meta,
                documents: live_docs,
            },
            segment_ref,
        ))
    };

    let delete_segment = build_delete_segment(state, generation, next_delete_ordinal);
    let max_seq_no = state
        .pending_docs
        .values()
        .map(|pending| pending.seq_no)
        .chain(state.pending_delete_seq_by_key.values().copied())
        .max()
        .unwrap_or_default();

    Some(FlushPlan {
        segment_build: segment.clone().map(|(build, _)| build),
        segment_ref: segment.map(|(_, segment_ref)| segment_ref),
        delete_build: delete_segment.clone().map(|(build, _)| build),
        delete_segment_ref: delete_segment.map(|(_, delete_ref)| delete_ref),
        max_seq_no,
    })
}

pub(super) fn apply_completed_flush(state: &mut WriterState, plan: &FlushPlan) -> AppliedFlush {
    state.last_flushed_seq_no = plan.max_seq_no;
    state
        .pending_docs
        .retain(|_, pending| pending.seq_no > plan.max_seq_no);
    state
        .pending_delete_seq_by_key
        .retain(|_, seq| *seq > plan.max_seq_no);
    state
        .pending_retractions
        .retain(|key| state.pending_delete_seq_by_key.contains_key(key));
    // Published readers own their source references. Once a version is in a
    // segment the writer must release its history copy, retaining only writes
    // that arrived after this flush plan was captured.
    state.history.retain(|_, entries| {
        entries.retain(|entry| entry.seq_no > plan.max_seq_no);
        !entries.is_empty()
    });
    if let Some(segment_ref) = plan.segment_ref.clone() {
        state.pending_segments.push(segment_ref);
    }
    if let Some(delete_segment_ref) = plan.delete_segment_ref.clone() {
        state.pending_delete_segments.push(delete_segment_ref);
    }
    state.generation += 1;
    let generation = state.generation;
    let snapshot = build_snapshot(state, generation, state.committed_seq_no, state.last_seq_no);

    AppliedFlush {
        result: FlushResult {
            segment_ids: plan
                .segment_ref
                .clone()
                .into_iter()
                .map(|segment| segment.id)
                .collect(),
            delete_segment_ids: plan
                .delete_segment_ref
                .clone()
                .into_iter()
                .map(|segment| segment.id)
                .collect(),
            max_seq_no: plan.max_seq_no,
        },
        snapshot,
    }
}

fn build_delete_segment(
    state: &WriterState,
    generation: u64,
    ordinal: usize,
) -> Option<(DeleteSegmentBuildOutput, DeleteSegmentRef)> {
    if state.pending_delete_seq_by_key.is_empty() {
        return None;
    }

    let mut records = state
        .pending_delete_seq_by_key
        .iter()
        .map(|(key, seq_no)| DeleteRecord {
            retract_to_base: state.pending_retractions.contains(key),
            seq_no: *seq_no,
            key: key.clone(),
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.seq_no
            .cmp(&right.seq_no)
            .then_with(|| left.key.cmp(&right.key))
    });

    let delete_segment_id = DeleteSegmentId(format!(
        "del-{generation:016}-{ordinal:016}-{}",
        artifact_nonce()
    ));
    let min_seq_no = records
        .iter()
        .map(|record| record.seq_no)
        .min()
        .unwrap_or_default();
    let max_seq_no = records
        .iter()
        .map(|record| record.seq_no)
        .max()
        .unwrap_or_default();
    let meta = DeleteSegmentMeta {
        delete_segment_id: delete_segment_id.clone(),
        min_seq_no,
        max_seq_no,
        delete_count: records.len() as u32,
    };
    let segment_ref = DeleteSegmentRef {
        id: delete_segment_id,
        min_seq_no,
        max_seq_no,
        delete_count: records.len() as u32,
        size_bytes: records
            .iter()
            .map(|record| delete_record_size_bytes(&record.key) as u64)
            .sum(),
    };

    Some((
        DeleteSegmentBuildOutput {
            meta,
            records: records.clone(),
        },
        segment_ref,
    ))
}

fn delete_record_size_bytes(key: &crate::index_engine::types::DocKey) -> usize {
    key.repo_id.0.len() + key.logical_path.len() + std::mem::size_of::<SeqNo>() + 1 + 8
}
