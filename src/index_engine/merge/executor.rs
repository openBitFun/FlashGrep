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

        let mut max_delete_seq_by_key = BTreeMap::<DocKey, u64>::new();
        for delete_segment in &source_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment.id.clone())?;
            for record in reader.records() {
                max_delete_seq_by_key
                    .entry(record.key.clone())
                    .and_modify(|seq_no| *seq_no = (*seq_no).max(record.seq_no))
                    .or_insert(record.seq_no);
            }
        }

        let mut effective_docs = BTreeMap::<DocKey, (u64, IndexDocument)>::new();
        for segment in &source_segments {
            let reader = self.segment_store.open_segment(segment.id.clone())?;
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                let document = reader.document(doc_id)?;
                if max_delete_seq_by_key
                    .get(&document.key)
                    .is_some_and(|delete_seq_no| *delete_seq_no >= segment.max_seq_no)
                {
                    continue;
                }
                let replace = effective_docs
                    .get(&document.key)
                    .is_none_or(|(seq_no, _)| *seq_no <= segment.max_seq_no);
                if replace {
                    effective_docs
                        .insert(document.key.clone(), (segment.max_seq_no, document.clone()));
                }
            }
        }

        let documents = effective_docs
            .into_values()
            .map(|(_, document)| document)
            .collect::<Vec<_>>();
        let mut output_segments = Vec::new();
        if !documents.is_empty() {
            let next_generation = manifest.generation + 1;
            let segment_id = SegmentId(format!(
                "seg-merge-{}-{next_generation:016}",
                manifest.manifest_id.0
            ));
            let min_seq_no = source_segments
                .iter()
                .map(|segment| segment.min_seq_no)
                .min()
                .unwrap_or_default();
            let max_seq_no = source_segments
                .iter()
                .map(|segment| segment.max_seq_no)
                .max()
                .unwrap_or_default();
            let segment_meta = SegmentMeta {
                segment_id: segment_id.clone(),
                min_seq_no,
                max_seq_no,
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
                min_seq_no,
                max_seq_no,
                doc_count: documents.len() as u32,
                size_bytes: documents
                    .iter()
                    .map(|document| document.source_bytes.len() as u64)
                    .sum(),
            });
        }

        let max_consumed_delete_seq = source_delete_segments
            .iter()
            .map(|segment| segment.max_seq_no)
            .max();
        let can_drop_consumed_deletes = max_consumed_delete_seq.is_some_and(|max_delete_seq| {
            manifest.active_segments.iter().all(|segment| {
                segment.max_seq_no > max_delete_seq || source_segment_ids.contains(&segment.id)
            })
        });
        let mut output_delete_segments = Vec::new();
        if !max_delete_seq_by_key.is_empty() && !can_drop_consumed_deletes {
            let records = max_delete_seq_by_key
                .into_iter()
                .map(|(key, seq_no)| crate::index_engine::delete::DeleteRecord { key, seq_no })
                .collect::<Vec<_>>();
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
            let id = DeleteSegmentId(format!(
                "del-merge-{}-{:016}",
                manifest.manifest_id.0,
                manifest.generation.saturating_add(1)
            ));
            let meta = crate::index_engine::segment::DeleteSegmentMeta {
                delete_segment_id: id.clone(),
                min_seq_no,
                max_seq_no,
                delete_count: records.len() as u32,
            };
            self.delete_store.write_delete_segment(
                crate::index_engine::segment::DeleteSegmentBuildOutput {
                    meta: meta.clone(),
                    records: records.clone(),
                },
            )?;
            output_delete_segments.push(DeleteSegmentRef {
                id,
                min_seq_no,
                max_seq_no,
                delete_count: meta.delete_count,
                size_bytes: records
                    .iter()
                    .map(|record| {
                        (record.key.repo_id.0.len() + record.key.logical_path.len() + 8) as u64
                    })
                    .sum(),
            });
        }

        Ok(MergeResult {
            output_segments,
            output_delete_segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::{
        config::PostingTokenizerConfig,
        delete::{DeleteRecord, FileDeleteStore},
        manifest::Manifest,
        segment::{DeleteSegmentBuildOutput, DeleteSegmentMeta, FileSegmentStore},
        DeleteSegmentId, ManifestId, RepoId,
    };

    #[test]
    fn delete_compaction_preserves_max_sequence_while_foundation_is_unmerged() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let segment_store = Arc::new(
            FileSegmentStore::open(temp.path(), PostingTokenizerConfig::default())
                .expect("segment store should open"),
        );
        let delete_store =
            Arc::new(FileDeleteStore::open(temp.path()).expect("delete store should open"));
        let key = DocKey {
            repo_id: RepoId("repo".into()),
            logical_path: "deleted.rs".into(),
        };
        let delete_refs = [2_u64, 3]
            .into_iter()
            .map(|seq_no| {
                let id = DeleteSegmentId(format!("delete-{seq_no}"));
                delete_store
                    .write_delete_segment(DeleteSegmentBuildOutput {
                        meta: DeleteSegmentMeta {
                            delete_segment_id: id.clone(),
                            min_seq_no: seq_no,
                            max_seq_no: seq_no,
                            delete_count: 1,
                        },
                        records: vec![DeleteRecord {
                            seq_no,
                            key: key.clone(),
                        }],
                    })
                    .expect("delete segment should write");
                DeleteSegmentRef {
                    id,
                    min_seq_no: seq_no,
                    max_seq_no: seq_no,
                    delete_count: 1,
                    size_bytes: 1,
                }
            })
            .collect::<Vec<_>>();
        let foundation = SegmentRef {
            id: SegmentId("foundation".into()),
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count: 1,
            size_bytes: 1,
        };
        let manifest = Manifest {
            manifest_id: ManifestId("parent".into()),
            generation: 1,
            active_segments: vec![foundation],
            active_delete_segments: delete_refs.clone(),
            ..Manifest::default()
        };
        let executor = StoreMergeExecutor::new(segment_store, delete_store.clone());
        let result = executor
            .execute(
                MergeTask {
                    source_segments: Vec::new(),
                    source_delete_segments: delete_refs
                        .iter()
                        .map(|segment| segment.id.clone())
                        .collect(),
                },
                &ManifestSnapshot::new(manifest),
            )
            .expect("delete compaction should succeed");
        assert_eq!(result.output_delete_segments.len(), 1);
        let output = &result.output_delete_segments[0];
        let reader = delete_store
            .open_delete_segment(output.id.clone())
            .expect("compacted delete should open");
        assert_eq!(reader.records().len(), 1);
        assert_eq!(reader.records()[0].key, key);
        assert_eq!(reader.records()[0].seq_no, 3);
    }
}
