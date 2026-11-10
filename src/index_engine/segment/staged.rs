//! Query-visible immutable memory segments, spilled before durable publication.
//! The budget covers store-owned payload/posting estimates, not reader leases or RSS.
use super::{
    build_posting_lists,
    reader::{MemorySegmentReader, SegmentReaderHandle},
    writer::{FileSegmentStore, SegmentStore},
    PostingLists, SegmentBuildOutput, SegmentMeta,
};
use crate::{
    index_engine::{config::PostingTokenizerConfig, types::SegmentId},
    Result,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

struct Prepared {
    build: SegmentBuildOutput,
    postings: Arc<HashMap<u64, Arc<[u32]>>>,
    reader: SegmentReaderHandle,
    bytes: usize,
}
pub(crate) struct StagedSegmentStore {
    disk: FileSegmentStore,
    tokenizer: PostingTokenizerConfig,
    limit: usize,
    staged: RwLock<HashMap<SegmentId, Arc<Prepared>>>,
    persistence: Mutex<()>,
}
impl StagedSegmentStore {
    pub(crate) fn new(
        disk: FileSegmentStore,
        tokenizer: PostingTokenizerConfig,
        limit: usize,
    ) -> Self {
        Self {
            disk,
            tokenizer,
            limit,
            staged: RwLock::default(),
            persistence: Mutex::new(()),
        }
    }
}
impl SegmentStore for StagedSegmentStore {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta> {
        let postings: Arc<HashMap<u64, Arc<[u32]>>> = Arc::new(
            build_posting_lists(&build.documents, &self.tokenizer)
                .into_iter()
                .map(|(k, v)| (k, Arc::from(v)))
                .collect(),
        );
        let bytes = build
            .documents
            .iter()
            .map(|d| d.source_bytes.len() + d.logical_path.len() * 3 + 256)
            .sum::<usize>()
            + postings.values().map(|p| p.len() * 4 + 64).sum::<usize>();
        // A storm applies backpressure instead of accumulating an unlimited RAM index.
        loop {
            let oldest = {
                let staged = self
                    .staged
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if staged
                    .values()
                    .map(|p| p.bytes)
                    .sum::<usize>()
                    .saturating_add(bytes)
                    <= self.limit
                {
                    None
                } else {
                    staged
                        .values()
                        .min_by_key(|p| p.build.meta.max_seq_no)
                        .map(|p| p.build.meta.segment_id.clone())
                }
            };
            let Some(id) = oldest else { break };
            self.persist_segment(&id)?;
        }
        if bytes > self.limit || build.meta.created_by_merge {
            return self.disk.write_prepared(&build, &postings);
        }
        let meta = build.meta.clone();
        let reader = Arc::new(MemorySegmentReader::new(
            meta.clone(),
            build.documents.clone(),
            PostingLists::from_shared(postings.clone()),
        ));
        self.staged
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                meta.segment_id.clone(),
                Arc::new(Prepared {
                    build,
                    postings,
                    reader,
                    bytes,
                }),
            );
        Ok(meta)
    }
    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle> {
        if let Some(p) = self
            .staged
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
        {
            return Ok(p.reader.clone());
        }
        self.disk.open_segment(id)
    }
    fn persist_segment(&self, id: &SegmentId) -> Result<()> {
        let _serial = self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prepared = self
            .staged
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        if let Some(p) = prepared {
            self.disk.write_prepared(&p.build, &p.postings)?;
            self.staged
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        Ok(())
    }
    fn remove_segment(&self, id: SegmentId) -> Result<()> {
        let _serial = self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.staged
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        self.disk.remove_segment(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::{
        types::{DocKey, RepoId},
        writer::IndexDocument,
    };
    #[test]
    fn memory_budget_spills_old_segments_without_invalidating_held_reader() {
        let temp = tempfile::tempdir().unwrap();
        let tokenizer = PostingTokenizerConfig::default();
        let store = StagedSegmentStore::new(
            FileSegmentStore::open(temp.path(), tokenizer.clone()).unwrap(),
            tokenizer,
            32768,
        );
        let payload: Arc<[u8]> = Arc::from(b"MEMORY_BUDGET_CONTENT\n".repeat(128));
        let build = |i| SegmentBuildOutput {
            meta: SegmentMeta {
                segment_id: SegmentId(format!("s{i}")),
                min_seq_no: i,
                max_seq_no: i,
                doc_count: 1,
                live_doc_count: 1,
                created_by_merge: false,
            },
            documents: vec![IndexDocument {
                key: DocKey {
                    repo_id: RepoId("repo".into()),
                    logical_path: format!("a{i}.rs"),
                },
                repo_id: RepoId("repo".into()),
                logical_path: format!("a{i}.rs"),
                file_name: format!("a{i}.rs"),
                language: None,
                size: payload.len() as u64,
                modified_at: 0,
                content_hash: [0; 32],
                source_bytes: payload.clone(),
            }],
        };
        store.write_segment(build(0)).unwrap();
        assert!(!temp.path().join("segments/s0").exists());
        let held = store.open_segment(SegmentId("s0".into())).unwrap();
        for i in 1..40 {
            store.write_segment(build(i)).unwrap();
        }
        assert!(temp.path().join("segments/s0").is_dir());
        assert!(
            store
                .staged
                .read()
                .unwrap()
                .values()
                .map(|p| p.bytes)
                .sum::<usize>()
                <= 32768
        );
        assert_eq!(held.document(0).unwrap().source_bytes, payload);
        let disk_reader = store.open_segment(SegmentId("s0".into())).unwrap();
        assert_eq!(disk_reader.document(0).unwrap().source_bytes, payload);
        store.remove_segment(SegmentId("s0".into())).unwrap();
        assert_eq!(held.document(0).unwrap().source_bytes, payload);
    }
}
