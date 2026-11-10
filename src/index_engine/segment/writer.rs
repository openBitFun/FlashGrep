use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use crate::index_engine::segment::reader::{
    FileSegmentReader, MemorySegmentReader, SegmentReaderHandle,
};
use crate::{
    git_snapshot::GitBlobReaderPool,
    index_engine::{config::PostingTokenizerConfig, types::SegmentId},
    Result,
};

use super::{
    build_posting_lists, read_segment_meta, write_postings_file, write_segment_documents,
    write_segment_meta, write_term_dict_file, FileSegmentDocTable, PostingLists,
    SegmentBuildOutput, SegmentMeta, TermDictionary, SEGMENT_META_FILE,
};
const TERM_DICT_FILE: &str = "term_dict.bin";
const POSTINGS_FILE: &str = "postings.bin";

pub trait SegmentStore: Send + Sync {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta>;
    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle>;
    /// Ensure this immutable artifact is on disk before publishing a durable manifest.
    fn persist_segment(&self, _id: &SegmentId) -> Result<()> {
        Ok(())
    }
    fn remove_segment(&self, id: SegmentId) -> Result<()>;
}

#[derive(Default)]
pub struct MemorySegmentStore {
    segments: RwLock<HashMap<SegmentId, SegmentReaderHandle>>,
    tokenizer: PostingTokenizerConfig,
}

impl MemorySegmentStore {
    pub(crate) fn new(tokenizer: PostingTokenizerConfig) -> Self {
        Self {
            segments: RwLock::default(),
            tokenizer,
        }
    }
}

impl SegmentStore for MemorySegmentStore {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta> {
        // Tokenization is independent of existing readers. Acquire the store lock
        // only to publish the completed immutable segment.
        let postings = build_posting_lists(&build.documents, &self.tokenizer);
        let meta = build.meta.clone();
        let reader = Arc::new(MemorySegmentReader::new(
            build.meta,
            build.documents,
            PostingLists::from_eager(postings),
        ));
        self.segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(meta.segment_id.clone(), reader);
        Ok(meta)
    }

    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle> {
        let guard = self
            .segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(&id).cloned().ok_or_else(|| {
            crate::AppError::Protocol(format!("unknown in-memory segment: {}", id.0))
        })
    }

    fn remove_segment(&self, id: SegmentId) -> Result<()> {
        let mut guard = self
            .segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&id);
        Ok(())
    }
}

pub struct FileSegmentStore {
    segments_dir: PathBuf,
    tokenizer: PostingTokenizerConfig,
    repo_root: Option<PathBuf>,
    base_head_commit: Option<String>,
    git_blob_reader_pool: Option<Arc<GitBlobReaderPool>>,
    cache: RwLock<HashMap<SegmentId, SegmentReaderHandle>>,
    query_source_budget: Option<Arc<super::source_cache::SourceCacheBudget>>,
}

impl FileSegmentStore {
    pub fn open(index_root: &Path, tokenizer: PostingTokenizerConfig) -> Result<Self> {
        Self::open_with_repo_root(index_root, tokenizer, None, None)
    }

    pub fn open_with_repo_root(
        index_root: &Path,
        tokenizer: PostingTokenizerConfig,
        repo_root: Option<PathBuf>,
        base_head_commit: Option<String>,
    ) -> Result<Self> {
        let segments_dir = index_root.join("segments");
        fs::create_dir_all(&segments_dir)?;
        let git_blob_reader_pool = match (base_head_commit.as_ref(), repo_root.as_deref()) {
            (Some(_), Some(repo_root)) => Some(Arc::new(GitBlobReaderPool::new(repo_root))),
            _ => None,
        };
        Ok(Self {
            segments_dir,
            tokenizer,
            repo_root,
            base_head_commit,
            git_blob_reader_pool,
            cache: RwLock::new(HashMap::new()),
            query_source_budget: None,
        })
    }

    pub(crate) fn with_query_source_cache_bytes(mut self, bytes: usize) -> Self {
        self.query_source_budget =
            (bytes > 0).then(|| Arc::new(super::source_cache::SourceCacheBudget::new(bytes)));
        self
    }

    pub(super) fn write_prepared<V: AsRef<[u32]>>(
        &self,
        build: &SegmentBuildOutput,
        postings: &HashMap<u64, V>,
    ) -> Result<SegmentMeta> {
        let dir = self.segment_dir(&build.meta.segment_id);
        fs::create_dir_all(&dir)?;
        write_segment_documents(&dir, &build.documents)?;
        write_term_dict_file(&dir.join(TERM_DICT_FILE), postings)?;
        write_postings_file(&dir.join(POSTINGS_FILE), postings)?;
        write_segment_meta(&dir.join(SEGMENT_META_FILE), &build.meta)?;
        Ok(build.meta.clone())
    }

    fn segment_dir(&self, id: &SegmentId) -> PathBuf {
        self.segments_dir.join(&id.0)
    }
}

impl SegmentStore for FileSegmentStore {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta> {
        let postings = build_posting_lists(&build.documents, &self.tokenizer);
        self.write_prepared(&build, &postings)
    }

    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle> {
        {
            let cache = self
                .cache
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(reader) = cache.get(&id) {
                return Ok(reader.clone());
            }
        }

        let segment_dir = self.segment_dir(&id);
        let profile_open = env::var_os("FLASHGREP_PROFILE_OPEN_SEGMENT").is_some();
        let meta = read_segment_meta(&segment_dir.join(SEGMENT_META_FILE))?;
        let doc_addresses_started = Instant::now();
        let doc_table = FileSegmentDocTable::open(&segment_dir)?;
        let doc_addresses_elapsed = doc_addresses_started.elapsed();
        let term_dict_started = Instant::now();
        let term_dict = TermDictionary::open(&segment_dir.join(TERM_DICT_FILE))?;
        let term_dict_elapsed = term_dict_started.elapsed();
        let postings_started = Instant::now();
        let posting_lists = PostingLists::open_lazy(term_dict, &segment_dir.join(POSTINGS_FILE))?;
        let postings_elapsed = postings_started.elapsed();
        let reader_started = Instant::now();
        let reader = std::sync::Arc::new(
            FileSegmentReader::new(
                meta,
                doc_table,
                posting_lists,
                self.repo_root.clone(),
                self.base_head_commit.clone(),
                self.git_blob_reader_pool.clone(),
            )
            .with_query_source_budget(self.query_source_budget.clone()),
        );
        let reader_elapsed = reader_started.elapsed();
        if profile_open {
            eprintln!(
                "segment_open id={} doc_addresses_ms={:.3} term_dict_ms={:.3} postings_ms={:.3} reader_new_ms={:.3}",
                id.0,
                doc_addresses_elapsed.as_secs_f64() * 1000.0,
                term_dict_elapsed.as_secs_f64() * 1000.0,
                postings_elapsed.as_secs_f64() * 1000.0,
                reader_elapsed.as_secs_f64() * 1000.0,
            );
        }
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(id, reader.clone());
        Ok(reader)
    }

    fn remove_segment(&self, id: SegmentId) -> Result<()> {
        let segment_dir = self.segment_dir(&id);
        if segment_dir.exists() {
            fs::remove_dir_all(segment_dir)?;
        }
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_segment(id: &str) -> SegmentBuildOutput {
        SegmentBuildOutput {
            meta: SegmentMeta {
                segment_id: SegmentId(id.to_string()),
                min_seq_no: 0,
                max_seq_no: 0,
                doc_count: 0,
                live_doc_count: 0,
                created_by_merge: false,
            },
            documents: Vec::new(),
        }
    }

    fn payload_segment(id: &str, payload: &[u8]) -> SegmentBuildOutput {
        let mut build = empty_segment(id);
        build.meta.doc_count = 1;
        build.meta.live_doc_count = 1;
        build
            .documents
            .push(crate::index_engine::writer::IndexDocument {
                key: crate::index_engine::DocKey {
                    repo_id: crate::index_engine::RepoId("repo".into()),
                    logical_path: format!("{id}.txt"),
                },
                repo_id: crate::index_engine::RepoId("repo".into()),
                logical_path: format!("{id}.txt"),
                file_name: format!("{id}.txt"),
                language: None,
                size: payload.len() as u64,
                modified_at: 0,
                content_hash: [0; 32],
                source_bytes: Arc::from(payload),
            });
        build
    }

    fn source_bytes(reader: &SegmentReaderHandle) -> Arc<[u8]> {
        match reader.verify_source(0).unwrap() {
            super::super::reader::SegmentVerifySource::Bytes(bytes) => bytes,
            _ => panic!("expected immutable bytes"),
        }
    }

    #[test]
    fn query_cache_budget_is_shared_and_released_after_last_reader() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileSegmentStore::open(temp.path(), PostingTokenizerConfig::default())
            .unwrap()
            .with_query_source_cache_bytes(4);
        store.write_segment(payload_segment("a", b"aaaa")).unwrap();
        store.write_segment(payload_segment("b", b"bbbb")).unwrap();
        let a = store.open_segment(SegmentId("a".into())).unwrap();
        let b = store.open_segment(SegmentId("b".into())).unwrap();
        let budget = store.query_source_budget.as_ref().unwrap();
        assert!(Arc::ptr_eq(&source_bytes(&a), &source_bytes(&a)));
        assert_eq!(budget.used(), 4);
        assert!(!Arc::ptr_eq(&source_bytes(&b), &source_bytes(&b)));
        assert_eq!(budget.used(), 4);
        // Removing a store's cache reference must not invalidate an in-flight reader.
        store.remove_segment(SegmentId("a".into())).unwrap();
        assert_eq!(budget.used(), 4);
        assert_eq!(&*source_bytes(&a), b"aaaa");
        drop(a);
        assert_eq!(budget.used(), 0);
        assert!(Arc::ptr_eq(&source_bytes(&b), &source_bytes(&b)));
        assert_eq!(budget.used(), 4);
    }

    #[test]
    fn base_source_verification_does_not_retain_decoded_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileSegmentStore::open(temp.path(), PostingTokenizerConfig::default()).unwrap();
        let payload = b"immutable payload\n".repeat(1000);
        store
            .write_segment(payload_segment("base", &payload))
            .unwrap();
        let reader = store.open_segment(SegmentId("base".into())).unwrap();
        let first = source_bytes(&reader);
        let second = source_bytes(&reader);
        assert_eq!(&*first, payload.as_slice());
        assert_eq!(&*second, payload.as_slice());
        assert!(!Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn file_segment_readers_share_one_git_blob_reader_pool_per_store() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repo should create");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git should start");
        assert!(status.success());

        let index_root = temp.path().join("index");
        let store = FileSegmentStore::open_with_repo_root(
            &index_root,
            PostingTokenizerConfig::default(),
            Some(repo),
            Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        )
        .expect("segment store should open");
        for id in ["segment-a", "segment-b"] {
            store
                .write_segment(empty_segment(id))
                .expect("segment should write");
        }

        let first = store
            .open_segment(SegmentId("segment-a".to_string()))
            .expect("first segment should open")
            .git_blob_reader_pool_identity();
        let second = store
            .open_segment(SegmentId("segment-b".to_string()))
            .expect("second segment should open")
            .git_blob_reader_pool_identity();

        assert!(first.is_some());
        assert_eq!(first, second);
    }
}
