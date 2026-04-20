use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::index_engine::segment::reader::FileSegmentReader;
use crate::{
    index_engine::{config::PostingTokenizerConfig, types::SegmentId},
    AppError, Result,
};

use super::{
    build_posting_lists, read_segment_doc_addresses, read_segment_meta, write_postings_file,
    write_segment_documents, write_segment_meta, write_term_dict_file, MemorySegmentReader,
    PostingLists, SegmentBuildOutput, SegmentMeta, SegmentReaderHandle, TermDictionary, DOCS_FILE,
    PATH_MAP_FILE, SEGMENT_META_FILE, STORED_FIELDS_FILE,
};
const TERM_DICT_FILE: &str = "term_dict.bin";
const POSTINGS_FILE: &str = "postings.bin";

pub trait SegmentStore: Send + Sync {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta>;
    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle>;
    fn remove_segment(&self, id: SegmentId) -> Result<()>;
}

#[derive(Default)]
pub struct MemorySegmentStore {
    segments: RwLock<HashMap<SegmentId, SegmentMeta>>,
}

impl SegmentStore for MemorySegmentStore {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta> {
        let mut guard = self
            .segments
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.insert(build.meta.segment_id.clone(), build.meta.clone());
        Ok(build.meta)
    }

    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle> {
        let guard = self
            .segments
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let meta = guard.get(&id).cloned().ok_or_else(|| {
            crate::AppError::Protocol(format!("unknown in-memory segment: {}", id.0))
        })?;
        Ok(std::sync::Arc::new(MemorySegmentReader::new(
            meta,
            Vec::new(),
            Default::default(),
        )))
    }

    fn remove_segment(&self, id: SegmentId) -> Result<()> {
        let mut guard = self
            .segments
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&id);
        Ok(())
    }
}

pub struct FileSegmentStore {
    segments_dir: PathBuf,
    tokenizer: PostingTokenizerConfig,
    repo_root: Option<PathBuf>,
    cache: RwLock<HashMap<SegmentId, SegmentReaderHandle>>,
}

impl FileSegmentStore {
    pub fn open(index_root: &Path, tokenizer: PostingTokenizerConfig) -> Result<Self> {
        Self::open_with_repo_root(index_root, tokenizer, None)
    }

    pub fn open_with_repo_root(
        index_root: &Path,
        tokenizer: PostingTokenizerConfig,
        repo_root: Option<PathBuf>,
    ) -> Result<Self> {
        let segments_dir = index_root.join("segments");
        fs::create_dir_all(&segments_dir)?;
        Ok(Self {
            segments_dir,
            tokenizer,
            repo_root,
            cache: RwLock::new(HashMap::new()),
        })
    }

    fn segment_dir(&self, id: &SegmentId) -> PathBuf {
        self.segments_dir.join(&id.0)
    }
}

impl SegmentStore for FileSegmentStore {
    fn write_segment(&self, build: SegmentBuildOutput) -> Result<SegmentMeta> {
        let segment_dir = self.segment_dir(&build.meta.segment_id);
        fs::create_dir_all(&segment_dir)?;

        write_segment_meta(&segment_dir.join(SEGMENT_META_FILE), &build.meta)?;
        write_segment_documents(&segment_dir, &build.documents)?;
        let posting_lists = build_posting_lists(&build.documents, &self.tokenizer);
        write_term_dict_file(&segment_dir.join(TERM_DICT_FILE), &posting_lists)?;
        write_postings_file(&segment_dir.join(POSTINGS_FILE), &posting_lists)?;

        Ok(build.meta)
    }

    fn open_segment(&self, id: SegmentId) -> Result<SegmentReaderHandle> {
        {
            let cache = self
                .cache
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(reader) = cache.get(&id) {
                return Ok(reader.clone());
            }
        }

        let segment_dir = self.segment_dir(&id);
        let meta = read_segment_meta(&segment_dir.join(SEGMENT_META_FILE))?;
        let doc_addresses = read_segment_doc_addresses(&segment_dir)?;
        let term_dict = TermDictionary::open(&segment_dir.join(TERM_DICT_FILE))?;
        let posting_lists = PostingLists::open_lazy(term_dict, &segment_dir.join(POSTINGS_FILE))?;
        let stored_fields_path = segment_dir.join(STORED_FIELDS_FILE);
        let reader = std::sync::Arc::new(FileSegmentReader::new(
            meta,
            doc_addresses,
            posting_lists,
            stored_fields_path.exists().then_some(stored_fields_path),
            self.repo_root.clone(),
        )?);
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.insert(id, reader.clone());
        Ok(reader)
    }

    fn remove_segment(&self, id: SegmentId) -> Result<()> {
        let segment_dir = self.segment_dir(&id);
        if segment_dir.exists() {
            for required in [
                SEGMENT_META_FILE,
                DOCS_FILE,
                PATH_MAP_FILE,
                TERM_DICT_FILE,
                POSTINGS_FILE,
            ] {
                let path = segment_dir.join(required);
                if !path.exists() {
                    return Err(AppError::InvalidIndex(format!(
                        "segment {} is missing {} before removal",
                        id.0, required
                    )));
                }
            }
            fs::remove_dir_all(segment_dir)?;
        }
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.remove(&id);
        Ok(())
    }
}
