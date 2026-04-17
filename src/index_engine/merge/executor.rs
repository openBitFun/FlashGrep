use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::{
    index_engine::{
        delete::DeleteStore,
        manifest::{DeleteSegmentRef, ManifestSnapshot, SegmentRef},
        segment::{SegmentBuildOutput, SegmentMeta, SegmentStore},
        types::{DeleteSegmentId, DocKey, SegmentId},
        writer::IndexDocument,
    },
    Result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTask {
    pub source_segments: Vec<SegmentId>,
    pub source_delete_segments: Vec<DeleteSegmentId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    pub output_segments: Vec<SegmentRef>,
    pub output_delete_segments: Vec<DeleteSegmentRef>,
}

pub trait MergeExecutor: Send + Sync {
    fn execute(&self, task: MergeTask, snapshot: &ManifestSnapshot) -> Result<MergeResult>;
}

#[derive(Debug, Default)]
pub struct NoopMergeExecutor;

impl MergeExecutor for NoopMergeExecutor {
    fn execute(&self, _task: MergeTask, _snapshot: &ManifestSnapshot) -> Result<MergeResult> {
        Ok(MergeResult {
            output_segments: Vec::new(),
            output_delete_segments: Vec::new(),
        })
    }
}

pub struct StoreMergeExecutor {
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
}

impl StoreMergeExecutor {
    pub fn new(segment_store: Arc<dyn SegmentStore>, delete_store: Arc<dyn DeleteStore>) -> Self {
        Self {
            segment_store,
            delete_store,
        }
    }
}

impl MergeExecutor for StoreMergeExecutor {
    fn execute(&self, task: MergeTask, snapshot: &ManifestSnapshot) -> Result<MergeResult> {
        let manifest = snapshot.manifest();
        let source_segment_ids = task
            .source_segments
            .into_iter()
            .collect::<BTreeSet<SegmentId>>();
        let source_delete_segment_ids = task
            .source_delete_segments
            .into_iter()
            .collect::<BTreeSet<DeleteSegmentId>>();
        let source_segments = manifest
            .active_segments
            .iter()
            .filter(|segment| source_segment_ids.contains(&segment.id))
            .cloned()
            .collect::<Vec<_>>();
        let source_delete_segments = manifest
            .active_delete_segments
            .iter()
            .filter(|segment| source_delete_segment_ids.contains(&segment.id))
            .cloned()
            .collect::<Vec<_>>();

        let mut deleted_keys = BTreeSet::<DocKey>::new();
        for delete_segment in &source_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment.id.clone())?;
            for record in reader.records() {
                deleted_keys.insert(record.key.clone());
            }
        }

        let mut effective_docs = BTreeMap::<DocKey, IndexDocument>::new();
        for segment in &source_segments {
            let reader = self.segment_store.open_segment(segment.id.clone())?;
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                let document = reader.document(doc_id)?;
                if deleted_keys.contains(&document.key) {
                    continue;
                }
                effective_docs.insert(document.key.clone(), document.clone());
            }
        }

        let documents = effective_docs.into_values().collect::<Vec<_>>();
        let mut output_segments = Vec::new();
        if !documents.is_empty() {
            let next_generation = manifest.generation + 1;
            let segment_id = SegmentId(format!("seg-{next_generation:016}-merge"));
            let segment_meta = SegmentMeta {
                segment_id: segment_id.clone(),
                min_seq_no: 0,
                max_seq_no: manifest.committed_seq_no,
                doc_count: documents.len() as u32,
                live_doc_count: documents.len() as u32,
                created_by_merge: true,
            };
            self.segment_store.write_segment(SegmentBuildOutput {
                meta: segment_meta,
                documents: documents.clone(),
            })?;
            output_segments.push(SegmentRef {
                id: segment_id,
                min_seq_no: 0,
                max_seq_no: manifest.committed_seq_no,
                doc_count: documents.len() as u32,
                size_bytes: documents
                    .iter()
                    .map(|document| document.source_bytes.len() as u64)
                    .sum(),
            });
        }

        Ok(MergeResult {
            output_segments,
            output_delete_segments: Vec::new(),
        })
    }
}
