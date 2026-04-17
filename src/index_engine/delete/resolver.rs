use std::collections::HashMap;

use crate::index_engine::types::{DocKey, SegmentDocId, SegmentId, SeqNo};

pub trait VisibilityResolver: Send + Sync {
    fn is_live(
        &self,
        doc_key: &DocKey,
        doc_seq_no: SeqNo,
        segment_id: SegmentId,
        doc_id: SegmentDocId,
    ) -> bool;
}

#[derive(Debug, Default)]
pub struct MemoryVisibilityResolver {
    max_deleted_seq_by_key: HashMap<DocKey, SeqNo>,
}

impl MemoryVisibilityResolver {
    pub fn record_delete(&mut self, key: DocKey, seq_no: SeqNo) {
        self.max_deleted_seq_by_key
            .entry(key)
            .and_modify(|current| *current = (*current).max(seq_no))
            .or_insert(seq_no);
    }
}

impl VisibilityResolver for MemoryVisibilityResolver {
    fn is_live(
        &self,
        doc_key: &DocKey,
        doc_seq_no: SeqNo,
        _segment_id: SegmentId,
        _doc_id: SegmentDocId,
    ) -> bool {
        self.max_deleted_seq_by_key
            .get(doc_key)
            .is_none_or(|deleted_seq_no| *deleted_seq_no < doc_seq_no)
    }
}
