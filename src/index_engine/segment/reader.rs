use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

use memmap2::Mmap;

use crate::{
    files::resolve_repo_path,
    git_snapshot::GitBlobReaderPool,
    index_engine::{
        query::{QueryFilters, QueryPlan},
        segment::{
            file_name_from_logical_path, language_from_logical_path,
            read_encoded_source_bytes_view, read_index_document_from_inline_slice,
            FileSegmentDocTable, PostingLists, SegmentDocRecord, SegmentDocSourceKind,
        },
        types::{DocKey, SegmentDocId},
        writer::IndexDocument,
    },
    AppError, Result,
};

use super::SegmentMeta;

const DOC_CACHE_CHUNK_SIZE: usize = 256;

pub trait SegmentReader: Send + Sync {
    fn meta(&self) -> &SegmentMeta;
    fn doc_count(&self) -> usize;
    fn collect_doc_ids_for_filters(&self, filters: &QueryFilters) -> Result<Vec<SegmentDocId>>;
    fn collect_candidates(&self, _plan: &QueryPlan, _field: u16) -> Result<Vec<SegmentDocId>>;
    fn document(&self, doc_id: SegmentDocId) -> Result<&IndexDocument>;
    fn doc_key(&self, _doc_id: SegmentDocId) -> Result<&DocKey>;
    fn doc_metadata(&self, _doc_id: SegmentDocId) -> Result<SegmentDocMetadata<'_>>;
    fn doc_source_kind(&self, _doc_id: SegmentDocId) -> Result<SegmentDocSourceKind>;
    fn verify_source(&self, _doc_id: SegmentDocId) -> Result<SegmentVerifySource>;
    fn verify_source_for_query(
        &self,
        doc_id: SegmentDocId,
        _prefer_worktree_for_git_head_blob: bool,
    ) -> Result<SegmentVerifySource> {
        self.verify_source(doc_id)
    }

    #[cfg(test)]
    fn git_blob_reader_pool_identity(&self) -> Option<usize> {
        None
    }
}

pub type SegmentReaderHandle = Arc<dyn SegmentReader>;

#[derive(Debug, Clone)]
pub enum SegmentVerifySource {
    Bytes(Arc<[u8]>),
    Mmap(Arc<Mmap>),
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub struct SegmentDocMetadata<'a> {
    pub key: &'a DocKey,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct SegmentDocMetadataValue {
    size: u64,
    modified_at: u64,
    content_hash: [u8; 32],
}

#[derive(Debug)]
pub struct MemorySegmentReader {
    meta: SegmentMeta,
    documents: Vec<IndexDocument>,
    posting_lists: PostingLists,
    path_doc_ids: BTreeMap<String, Vec<SegmentDocId>>,
}

impl MemorySegmentReader {
    pub fn new(
        meta: SegmentMeta,
        documents: Vec<IndexDocument>,
        posting_lists: PostingLists,
    ) -> Self {
        let path_doc_ids = build_path_doc_ids(
            documents
                .iter()
                .enumerate()
                .map(|(doc_id, doc)| (doc.logical_path.as_str(), doc_id as SegmentDocId)),
        );
        Self {
            meta,
            documents,
            posting_lists,
            path_doc_ids,
        }
    }
}

impl SegmentReader for MemorySegmentReader {
    fn meta(&self) -> &SegmentMeta {
        &self.meta
    }

    fn doc_count(&self) -> usize {
        self.documents.len()
    }

    fn collect_doc_ids_for_filters(&self, filters: &QueryFilters) -> Result<Vec<SegmentDocId>> {
        Ok(collect_doc_ids_for_filters_impl(
            self.documents.len() as SegmentDocId,
            &self.path_doc_ids,
            &filters.path_prefixes,
        ))
    }

    fn collect_candidates(&self, plan: &QueryPlan, _field: u16) -> Result<Vec<SegmentDocId>> {
        let candidate_ids = collect_candidate_ids(
            &self.posting_lists,
            plan,
            self.documents.len() as SegmentDocId,
        )?;
        Ok(filter_candidate_ids_by_path_prefixes(
            candidate_ids,
            &self.path_doc_ids,
            &plan.request.filters.path_prefixes,
        ))
    }

    fn doc_key(&self, _doc_id: SegmentDocId) -> Result<&DocKey> {
        self.documents
            .get(_doc_id as usize)
            .map(|doc| &doc.key)
            .ok_or_else(|| {
                AppError::InvalidIndex(format!("segment doc {_doc_id} is out of bounds"))
            })
    }

    fn document(&self, doc_id: SegmentDocId) -> Result<&IndexDocument> {
        self.documents
            .get(doc_id as usize)
            .ok_or_else(|| AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds")))
    }

    fn doc_metadata(&self, doc_id: SegmentDocId) -> Result<SegmentDocMetadata<'_>> {
        let doc = self.documents.get(doc_id as usize).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        Ok(SegmentDocMetadata {
            key: &doc.key,
            size: doc.size,
            modified_at: doc.modified_at,
            content_hash: doc.content_hash,
        })
    }

    fn doc_source_kind(&self, _doc_id: SegmentDocId) -> Result<SegmentDocSourceKind> {
        Ok(SegmentDocSourceKind::Inline)
    }

    fn verify_source(&self, doc_id: SegmentDocId) -> Result<SegmentVerifySource> {
        let doc = self.documents.get(doc_id as usize).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        Ok(SegmentVerifySource::Bytes(Arc::clone(&doc.source_bytes)))
    }
}

#[derive(Debug)]
pub struct FileSegmentReader {
    meta: SegmentMeta,
    doc_table: FileSegmentDocTable,
    posting_lists: PostingLists,
    path_doc_ids: OnceLock<BTreeMap<String, Vec<SegmentDocId>>>,
    repo_root: Option<PathBuf>,
    base_head_commit: Option<String>,
    git_blob_reader_pool: Option<Arc<GitBlobReaderPool>>,
    decoded_keys: LazyDocCache<DocKey>,
    decoded_source_bytes: LazyDocCache<Arc<[u8]>>,
    query_source_bytes: LazyDocCache<Arc<[u8]>>,
    query_source_budget: Option<Arc<super::source_cache::SourceCacheBudget>>,
    query_cached_bytes: AtomicUsize,
    decoded_documents: LazyDocCache<IndexDocument>,
    decoded_metadata: LazyDocCache<SegmentDocMetadataValue>,
}

#[derive(Debug)]
struct LazyDocCache<T> {
    len: usize,
    chunk_size: usize,
    chunks: Vec<OnceLock<Box<[OnceLock<T>]>>>,
}

impl FileSegmentReader {
    pub fn new(
        meta: SegmentMeta,
        doc_table: FileSegmentDocTable,
        posting_lists: PostingLists,
        repo_root: Option<PathBuf>,
        base_head_commit: Option<String>,
        git_blob_reader_pool: Option<Arc<GitBlobReaderPool>>,
    ) -> Self {
        let profile_open = env::var_os("FLASHGREP_PROFILE_OPEN_SEGMENT").is_some();
        let reader_init_started = Instant::now();
        let decoded_keys = LazyDocCache::new(doc_table.len());
        let decoded_source_bytes = LazyDocCache::new(doc_table.len());
        let query_source_bytes = LazyDocCache::new(doc_table.len());
        let decoded_documents = LazyDocCache::new(doc_table.len());
        let decoded_metadata = LazyDocCache::new(doc_table.len());
        if profile_open {
            eprintln!(
                "segment_reader_new id={} init_ms={:.3} doc_count={}",
                meta.segment_id.0,
                reader_init_started.elapsed().as_secs_f64() * 1000.0,
                doc_table.len(),
            );
        }
        Self {
            meta,
            doc_table,
            posting_lists,
            path_doc_ids: OnceLock::new(),
            repo_root,
            base_head_commit,
            git_blob_reader_pool,
            decoded_keys,
            decoded_source_bytes,
            query_source_bytes,
            query_source_budget: None,
            query_cached_bytes: AtomicUsize::new(0),
            decoded_documents,
            decoded_metadata,
        }
    }

    pub(crate) fn with_query_source_budget(
        mut self,
        budget: Option<Arc<super::source_cache::SourceCacheBudget>>,
    ) -> Self {
        self.query_source_budget = budget;
        self
    }

    fn inline_query_source_bytes(&self, doc_id: usize) -> Result<Arc<[u8]>> {
        if let Some(bytes) = self.decoded_source_bytes.get(doc_id)?.get() {
            return Ok(Arc::clone(bytes));
        }
        let Some(budget) = self.query_source_budget.as_ref() else {
            return self.read_source_bytes_from_inline_segment(doc_id);
        };
        let cache = self.query_source_bytes.get(doc_id)?;
        if let Some(bytes) = cache.get() {
            return Ok(Arc::clone(bytes));
        }
        let bytes = self.read_source_bytes_from_inline_segment(doc_id)?;
        let size = bytes.len();
        if budget.reserve(size) {
            if cache.set(Arc::clone(&bytes)).is_ok() {
                self.query_cached_bytes.fetch_add(size, Ordering::Relaxed);
            } else {
                budget.release(size);
            }
        }
        Ok(cache.get().map(Arc::clone).unwrap_or(bytes))
    }

    fn path_doc_ids(&self) -> Result<&BTreeMap<String, Vec<SegmentDocId>>> {
        if let Some(path_doc_ids) = self.path_doc_ids.get() {
            return Ok(path_doc_ids);
        }

        let started = Instant::now();
        let mut entries = Vec::with_capacity(self.doc_table.len());
        for doc_id in 0..self.doc_table.len() {
            let key = self.doc_key(doc_id as SegmentDocId)?;
            entries.push((key.logical_path.as_str(), doc_id as SegmentDocId));
        }
        let path_doc_ids = build_path_doc_ids(entries);
        let _ = self.path_doc_ids.set(path_doc_ids);
        let path_doc_ids = self.path_doc_ids.get().ok_or_else(|| {
            AppError::Protocol(format!(
                "failed to initialize path doc ids for {}",
                self.segment_source_label()
            ))
        })?;
        if env::var_os("FLASHGREP_PROFILE_OPEN_SEGMENT").is_some() {
            eprintln!(
                "segment_path_doc_ids id={} build_ms={:.3} doc_count={}",
                self.meta.segment_id.0,
                started.elapsed().as_secs_f64() * 1000.0,
                self.doc_table.len(),
            );
        }
        Ok(path_doc_ids)
    }

    fn doc_record(&self, doc_id: usize) -> Result<SegmentDocRecord> {
        self.doc_table.record(doc_id)
    }

    fn load_document(&self, doc_id: usize) -> Result<IndexDocument> {
        let record = self.doc_record(doc_id)?;
        let key = self.doc_key(doc_id as SegmentDocId)?;
        if matches!(record.source_kind, SegmentDocSourceKind::Inline) {
            return read_index_document_from_inline_slice(
                self.doc_table.source_slice(doc_id)?,
                key,
                record,
            );
        }

        Ok(IndexDocument {
            key: key.clone(),
            repo_id: key.repo_id.clone(),
            logical_path: key.logical_path.clone(),
            file_name: file_name_from_logical_path(&key.logical_path),
            language: language_from_logical_path(&key.logical_path),
            size: record.size,
            modified_at: record.modified_at,
            content_hash: record.content_hash,
            source_bytes: self.source_bytes(doc_id as SegmentDocId)?,
        })
    }

    fn load_metadata(&self, doc_id: usize) -> Result<SegmentDocMetadataValue> {
        let record = self.doc_record(doc_id)?;
        Ok(SegmentDocMetadataValue {
            size: record.size,
            modified_at: record.modified_at,
            content_hash: record.content_hash,
        })
    }

    fn read_source_bytes_from_worktree(&self, key: &DocKey) -> Result<Arc<[u8]>> {
        let path = self.repo_path_for_key(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Arc::<[u8]>::from(bytes)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
                ) =>
            {
                Ok(Arc::<[u8]>::from(Vec::<u8>::new()))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read_source_bytes_from_git_blob(
        &self,
        key: &DocKey,
        head_commit: &str,
    ) -> Result<Arc<[u8]>> {
        let reader = self.git_blob_reader_pool.as_ref().ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} cannot read {} from git blob storage",
                self.meta.segment_id.0, key.logical_path
            ))
        })?;
        Ok(Arc::<[u8]>::from(
            reader.read_blob(head_commit, &key.logical_path)?,
        ))
    }

    fn read_source_bytes_from_inline_segment(&self, doc_id: usize) -> Result<Arc<[u8]>> {
        read_encoded_source_bytes_view(self.doc_table.source_slice(doc_id)?)?.decode()
    }

    fn load_source_bytes(&self, doc_id: usize) -> Result<Arc<[u8]>> {
        let record = self.doc_record(doc_id)?;
        let key = self.doc_key(doc_id as SegmentDocId)?;
        match record.source_kind {
            SegmentDocSourceKind::Inline => self.read_source_bytes_from_inline_segment(doc_id),
            SegmentDocSourceKind::Worktree => self.read_source_bytes_from_worktree(key),
            SegmentDocSourceKind::GitHeadBlob => {
                let head_commit = self.base_head_commit.as_deref().ok_or_else(|| {
                    AppError::InvalidIndex(format!(
                        "segment {} is missing base head commit for git-backed doc source",
                        self.meta.segment_id.0
                    ))
                })?;
                self.read_source_bytes_from_git_blob(key, head_commit)
            }
        }
    }

    fn source_bytes(&self, doc_id: SegmentDocId) -> Result<Arc<[u8]>> {
        let doc_id = doc_id as usize;
        let cache = self.decoded_source_bytes.get(doc_id)?;
        if let Some(bytes) = cache.get() {
            return Ok(Arc::clone(bytes));
        }
        let bytes = match self.query_source_bytes.get(doc_id)?.get() {
            Some(bytes) => Arc::clone(bytes),
            None => self.load_source_bytes(doc_id)?,
        };
        if cache.set(Arc::clone(&bytes)).is_err() {
            let bytes = cache.get().ok_or_else(|| {
                AppError::Protocol(format!(
                    "failed to initialize cached segment source {doc_id} from {}",
                    self.segment_source_label()
                ))
            })?;
            return Ok(Arc::clone(bytes));
        }
        let bytes = cache.get().ok_or_else(|| {
            AppError::Protocol(format!(
                "failed to read cached segment source {doc_id} from {}",
                self.segment_source_label()
            ))
        })?;
        Ok(Arc::clone(bytes))
    }

    fn repo_path_for_key(&self, key: &DocKey) -> Result<PathBuf> {
        let repo_root = self.repo_root.as_ref().ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} cannot resolve {} without repo root",
                self.meta.segment_id.0, key.logical_path
            ))
        })?;
        Ok(resolve_repo_path(repo_root, &key.logical_path))
    }

    fn segment_source_label(&self) -> String {
        self.repo_root.as_ref().map_or_else(
            || self.meta.segment_id.0.clone(),
            |path| path.display().to_string(),
        )
    }

    fn worktree_verify_source(&self, doc_id: SegmentDocId) -> Result<SegmentVerifySource> {
        let key = self.doc_key(doc_id as SegmentDocId)?;
        let path = self.repo_path_for_key(key)?;
        match File::open(&path) {
            Ok(_) => Ok(SegmentVerifySource::Path(path)),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
                ) =>
            {
                Ok(SegmentVerifySource::Bytes(Arc::<[u8]>::from(
                    Vec::<u8>::new(),
                )))
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl SegmentReader for FileSegmentReader {
    fn meta(&self) -> &SegmentMeta {
        &self.meta
    }

    fn doc_count(&self) -> usize {
        self.doc_table.len()
    }

    fn collect_doc_ids_for_filters(&self, filters: &QueryFilters) -> Result<Vec<SegmentDocId>> {
        if filters.path_prefixes.is_empty() {
            return Ok((0..self.doc_table.len() as SegmentDocId).collect());
        }
        Ok(collect_doc_ids_for_filters_impl(
            self.doc_table.len() as SegmentDocId,
            self.path_doc_ids()?,
            &filters.path_prefixes,
        ))
    }

    fn collect_candidates(&self, plan: &QueryPlan, _field: u16) -> Result<Vec<SegmentDocId>> {
        let candidate_ids = collect_candidate_ids(
            &self.posting_lists,
            plan,
            self.doc_table.len() as SegmentDocId,
        )?;
        if plan.request.filters.path_prefixes.is_empty() {
            return Ok(candidate_ids);
        }
        Ok(filter_candidate_ids_by_path_prefixes(
            candidate_ids,
            self.path_doc_ids()?,
            &plan.request.filters.path_prefixes,
        ))
    }

    fn document(&self, doc_id: SegmentDocId) -> Result<&IndexDocument> {
        let doc_id = doc_id as usize;
        let slot = self.decoded_documents.get(doc_id)?;
        let _ = slot;
        let cache = self.decoded_documents.get(doc_id)?;
        if let Some(document) = cache.get() {
            return Ok(document);
        }
        let document = self.load_document(doc_id)?;
        if cache.set(document).is_err() {
            return cache.get().ok_or_else(|| {
                AppError::Protocol(format!(
                    "failed to initialize cached segment doc {doc_id} from {}",
                    self.segment_source_label()
                ))
            });
        }
        cache.get().ok_or_else(|| {
            AppError::Protocol(format!(
                "failed to read cached segment doc {doc_id} from {}",
                self.segment_source_label()
            ))
        })
    }

    fn doc_key(&self, doc_id: SegmentDocId) -> Result<&DocKey> {
        let doc_id = doc_id as usize;
        let cache = self.decoded_keys.get(doc_id)?;
        if let Some(key) = cache.get() {
            return Ok(key);
        }
        let key = self.doc_table.doc_key(doc_id)?;
        if cache.set(key).is_err() {
            return cache.get().ok_or_else(|| {
                AppError::Protocol(format!(
                    "failed to initialize cached segment key {doc_id} from {}",
                    self.segment_source_label()
                ))
            });
        }
        cache.get().ok_or_else(|| {
            AppError::Protocol(format!(
                "failed to read cached segment key {doc_id} from {}",
                self.segment_source_label()
            ))
        })
    }

    fn doc_metadata(&self, doc_id: SegmentDocId) -> Result<SegmentDocMetadata<'_>> {
        let doc_id = doc_id as usize;
        let key = self.doc_key(doc_id as SegmentDocId)?;
        let cache = self.decoded_metadata.get(doc_id)?;
        if let Some(metadata) = cache.get() {
            return Ok(SegmentDocMetadata {
                key,
                size: metadata.size,
                modified_at: metadata.modified_at,
                content_hash: metadata.content_hash,
            });
        }
        let metadata = self.load_metadata(doc_id)?;
        if cache.set(metadata).is_err() {
            let metadata = cache.get().ok_or_else(|| {
                AppError::Protocol(format!(
                    "failed to initialize cached segment metadata {doc_id} from {}",
                    self.segment_source_label()
                ))
            })?;
            return Ok(SegmentDocMetadata {
                key,
                size: metadata.size,
                modified_at: metadata.modified_at,
                content_hash: metadata.content_hash,
            });
        }
        let metadata = cache.get().ok_or_else(|| {
            AppError::Protocol(format!(
                "failed to read cached segment metadata {doc_id} from {}",
                self.segment_source_label()
            ))
        })?;
        Ok(SegmentDocMetadata {
            key,
            size: metadata.size,
            modified_at: metadata.modified_at,
            content_hash: metadata.content_hash,
        })
    }

    fn doc_source_kind(&self, doc_id: SegmentDocId) -> Result<SegmentDocSourceKind> {
        Ok(self.doc_record(doc_id as usize)?.source_kind)
    }

    fn verify_source(&self, doc_id: SegmentDocId) -> Result<SegmentVerifySource> {
        match self.doc_record(doc_id as usize)?.source_kind {
            SegmentDocSourceKind::Worktree => self.worktree_verify_source(doc_id),
            // Base queries decode on demand. Overlay readers may retain payloads
            // within their shared admission budget, retiring them with segments.
            SegmentDocSourceKind::Inline => Ok(SegmentVerifySource::Bytes(
                self.inline_query_source_bytes(doc_id as usize)?,
            )),
            SegmentDocSourceKind::GitHeadBlob => {
                Ok(SegmentVerifySource::Bytes(self.source_bytes(doc_id)?))
            }
        }
    }

    fn verify_source_for_query(
        &self,
        doc_id: SegmentDocId,
        prefer_worktree_for_git_head_blob: bool,
    ) -> Result<SegmentVerifySource> {
        if !prefer_worktree_for_git_head_blob {
            return self.verify_source(doc_id);
        }

        let cache = self.decoded_source_bytes.get(doc_id as usize)?;
        if let Some(bytes) = cache.get() {
            return Ok(SegmentVerifySource::Bytes(Arc::clone(bytes)));
        }

        let record = self.doc_record(doc_id as usize)?;
        if matches!(record.source_kind, SegmentDocSourceKind::GitHeadBlob) {
            let key = self.doc_key(doc_id)?;
            return Ok(SegmentVerifySource::Path(self.repo_path_for_key(key)?));
        }

        self.verify_source(doc_id)
    }

    #[cfg(test)]
    fn git_blob_reader_pool_identity(&self) -> Option<usize> {
        self.git_blob_reader_pool
            .as_ref()
            .map(|reader| Arc::as_ptr(reader) as usize)
    }
}

impl Drop for FileSegmentReader {
    fn drop(&mut self) {
        if let Some(budget) = self.query_source_budget.as_ref() {
            budget.release(self.query_cached_bytes.load(Ordering::Relaxed));
        }
    }
}

impl<T> LazyDocCache<T> {
    fn new(len: usize) -> Self {
        let chunk_count = if len == 0 {
            0
        } else {
            len.div_ceil(DOC_CACHE_CHUNK_SIZE)
        };
        Self {
            len,
            chunk_size: DOC_CACHE_CHUNK_SIZE,
            chunks: (0..chunk_count).map(|_| OnceLock::new()).collect(),
        }
    }

    fn get(&self, doc_id: usize) -> Result<&OnceLock<T>> {
        if doc_id >= self.len {
            return Err(AppError::InvalidIndex(format!(
                "segment doc {doc_id} is out of bounds"
            )));
        }
        let chunk_index = doc_id / self.chunk_size;
        let chunk_offset = doc_id % self.chunk_size;
        let chunk_len = self.chunk_len(chunk_index);
        let chunk = self
            .chunks
            .get(chunk_index)
            .ok_or_else(|| {
                AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
            })?
            .get_or_init(|| {
                (0..chunk_len)
                    .map(|_| OnceLock::new())
                    .collect::<Vec<_>>()
                    .into_boxed_slice()
            });
        chunk
            .get(chunk_offset)
            .ok_or_else(|| AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds")))
    }

    fn chunk_len(&self, chunk_index: usize) -> usize {
        let chunk_start = chunk_index * self.chunk_size;
        self.len.saturating_sub(chunk_start).min(self.chunk_size)
    }
}

fn build_path_doc_ids<'a, I>(entries: I) -> BTreeMap<String, Vec<SegmentDocId>>
where
    I: IntoIterator<Item = (&'a str, SegmentDocId)>,
{
    let mut path_doc_ids = BTreeMap::<String, Vec<SegmentDocId>>::new();
    for (path, doc_id) in entries {
        path_doc_ids
            .entry(path.to_string())
            .or_default()
            .push(doc_id);
    }
    path_doc_ids
}

fn collect_doc_ids_for_filters_impl(
    doc_count: SegmentDocId,
    path_doc_ids: &BTreeMap<String, Vec<SegmentDocId>>,
    path_prefixes: &[String],
) -> Vec<SegmentDocId> {
    if path_prefixes.is_empty() {
        return (0..doc_count).collect();
    }
    allowed_doc_ids_for_prefixes(path_doc_ids, path_prefixes)
        .into_iter()
        .collect()
}

fn filter_candidate_ids_by_path_prefixes(
    candidate_ids: Vec<SegmentDocId>,
    path_doc_ids: &BTreeMap<String, Vec<SegmentDocId>>,
    path_prefixes: &[String],
) -> Vec<SegmentDocId> {
    if path_prefixes.is_empty() {
        return candidate_ids;
    }
    let allowed = allowed_doc_ids_for_prefixes(path_doc_ids, path_prefixes);
    if allowed.is_empty() {
        return Vec::new();
    }
    candidate_ids
        .into_iter()
        .filter(|doc_id| allowed.contains(doc_id))
        .collect()
}

fn collect_candidate_ids(
    posting_lists: &PostingLists,
    plan: &QueryPlan,
    doc_count: SegmentDocId,
) -> Result<Vec<SegmentDocId>> {
    if plan.fallback_to_scan || plan.candidate_branches.is_empty() {
        return Ok((0..doc_count).collect());
    }

    let mut union = BTreeSet::new();
    for branch in &plan.candidate_branches {
        if branch.grams.is_empty() {
            return Ok((0..doc_count).collect());
        }

        let mut posting_slices = Vec::with_capacity(branch.grams.len());
        let mut missing_gram = false;
        for gram in &branch.grams {
            let Some(doc_ids) = posting_lists.get(gram)? else {
                missing_gram = true;
                break;
            };
            posting_slices.push(doc_ids);
        }
        if missing_gram || posting_slices.is_empty() {
            continue;
        }

        posting_slices.sort_by_key(|doc_ids| doc_ids.len());
        let mut branch_ids = posting_slices[0].to_vec();
        for doc_ids in &posting_slices[1..] {
            branch_ids = intersect_sorted_doc_ids(&branch_ids, doc_ids);
            if branch_ids.is_empty() {
                break;
            }
        }
        union.extend(branch_ids);
    }

    Ok(union.into_iter().collect())
}

fn intersect_sorted_doc_ids(left: &[SegmentDocId], right: &[SegmentDocId]) -> Vec<SegmentDocId> {
    let mut intersection = Vec::new();
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    while left_index < left.len() && right_index < right.len() {
        match left[left_index].cmp(&right[right_index]) {
            std::cmp::Ordering::Less => left_index += 1,
            std::cmp::Ordering::Greater => right_index += 1,
            std::cmp::Ordering::Equal => {
                intersection.push(left[left_index]);
                left_index += 1;
                right_index += 1;
            }
        }
    }

    intersection
}

fn allowed_doc_ids_for_prefixes(
    path_doc_ids: &BTreeMap<String, Vec<SegmentDocId>>,
    path_prefixes: &[String],
) -> std::collections::BTreeSet<SegmentDocId> {
    let mut allowed = std::collections::BTreeSet::new();
    for prefix in path_prefixes {
        if let Some(doc_ids) = path_doc_ids.get(prefix) {
            allowed.extend(doc_ids.iter().copied());
        }
        for (path, doc_ids) in path_doc_ids.range(prefix.clone()..) {
            if !path_matches_prefix(path, prefix) {
                break;
            }
            allowed.extend(doc_ids.iter().copied());
        }
    }
    allowed
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(std::path::MAIN_SEPARATOR))
}
