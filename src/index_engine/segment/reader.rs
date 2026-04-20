use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Instant,
};

use memmap2::Mmap;

use crate::{
    files::resolve_repo_path,
    index_engine::{
        query::{QueryFilters, QueryPlan},
        segment::{
            advise_mmap_sequential, read_index_document_from_stored_slice,
            read_stored_document_view, read_stored_fields_layout, FileSegmentDocTable,
            PostingLists, SegmentDocRecord, StoredFieldsLayout,
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
    fn doc_metadata(&self, _doc_id: SegmentDocId) -> Result<StoredDocMetadata<'_>>;
    fn stored_doc(&self, _doc_id: SegmentDocId) -> Result<StoredDocRef<'_>>;
    fn verify_source(&self, _doc_id: SegmentDocId) -> Result<SegmentVerifySource>;
}

pub type SegmentReaderHandle = Arc<dyn SegmentReader>;

#[derive(Debug, Clone)]
pub struct StoredDocRef<'a> {
    pub key: &'a DocKey,
    pub source_bytes: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub enum SegmentVerifySource {
    Path(PathBuf),
    Bytes(Arc<[u8]>),
}

#[derive(Debug, Clone, Copy)]
pub struct StoredDocMetadata<'a> {
    pub key: &'a DocKey,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct StoredDocMetadataValue {
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

    fn doc_metadata(&self, doc_id: SegmentDocId) -> Result<StoredDocMetadata<'_>> {
        let doc = self.documents.get(doc_id as usize).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        Ok(StoredDocMetadata {
            key: &doc.key,
            size: doc.size,
            modified_at: doc.modified_at,
            content_hash: doc.content_hash,
        })
    }

    fn stored_doc(&self, _doc_id: SegmentDocId) -> Result<StoredDocRef<'_>> {
        let doc = self.documents.get(_doc_id as usize).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {_doc_id} is out of bounds"))
        })?;
        Ok(StoredDocRef {
            key: &doc.key,
            source_bytes: Arc::clone(&doc.source_bytes),
        })
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
    stored_fields_path: Option<PathBuf>,
    stored_fields_mmap: Option<Mmap>,
    stored_fields_layout: Option<StoredFieldsLayout>,
    repo_root: Option<PathBuf>,
    decoded_keys: LazyDocCache<DocKey>,
    decoded_source_bytes: LazyDocCache<Arc<[u8]>>,
    decoded_documents: LazyDocCache<IndexDocument>,
    decoded_metadata: LazyDocCache<StoredDocMetadataValue>,
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
        stored_fields_path: Option<PathBuf>,
        repo_root: Option<PathBuf>,
    ) -> Result<Self> {
        let profile_open = env::var_os("CODGREP_PROFILE_OPEN_SEGMENT").is_some();
        let stored_fields_started = Instant::now();
        let (stored_fields_mmap, stored_fields_layout) = if let Some(path) = &stored_fields_path {
            let stored_fields_file = File::open(path)?;
            let stored_fields_mmap = unsafe { Mmap::map(&stored_fields_file)? };
            advise_mmap_sequential(&stored_fields_mmap);
            let stored_fields_layout = read_stored_fields_layout(stored_fields_mmap.as_ref())?;
            (Some(stored_fields_mmap), Some(stored_fields_layout))
        } else {
            (None, None)
        };
        let stored_fields_elapsed = stored_fields_started.elapsed();
        let path_doc_ids_started = Instant::now();
        let decoded_keys = LazyDocCache::new(doc_table.len());
        let decoded_source_bytes = LazyDocCache::new(doc_table.len());
        let decoded_documents = LazyDocCache::new(doc_table.len());
        let decoded_metadata = LazyDocCache::new(doc_table.len());
        if profile_open {
            eprintln!(
                "segment_reader_new id={} stored_fields_ms={:.3} cache_init_ms={:.3} doc_count={}",
                meta.segment_id.0,
                stored_fields_elapsed.as_secs_f64() * 1000.0,
                path_doc_ids_started.elapsed().as_secs_f64() * 1000.0,
                doc_table.len(),
            );
        }
        Ok(Self {
            meta,
            doc_table,
            posting_lists,
            path_doc_ids: OnceLock::new(),
            stored_fields_path,
            stored_fields_mmap,
            stored_fields_layout,
            repo_root,
            decoded_keys,
            decoded_source_bytes,
            decoded_documents,
            decoded_metadata,
        })
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
        if env::var_os("CODGREP_PROFILE_OPEN_SEGMENT").is_some() {
            eprintln!(
                "segment_path_doc_ids id={} build_ms={:.3} doc_count={}",
                self.meta.segment_id.0,
                started.elapsed().as_secs_f64() * 1000.0,
                self.doc_table.len(),
            );
        }
        Ok(path_doc_ids)
    }

    fn stored_slice(&self, doc_id: usize) -> Result<&[u8]> {
        let stored_fields_mmap = self.stored_fields_mmap.as_ref().ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} does not have stored fields",
                self.meta.segment_id.0
            ))
        })?;
        let record = self.doc_record(doc_id)?;
        let start = usize::try_from(record.stored_offset)
            .map_err(|_| AppError::ValueOutOfRange("stored offset exceeds usize range".into()))?;
        let end = start
            .checked_add(record.stored_len as usize)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice overflow".into()))?;
        stored_fields_mmap.get(start..end).ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "stored field slice out of range for {}",
                self.segment_source_label()
            ))
        })
    }

    fn doc_record(&self, doc_id: usize) -> Result<SegmentDocRecord> {
        self.doc_table.record(doc_id)
    }

    fn load_document(&self, doc_id: usize) -> Result<IndexDocument> {
        let record = self.doc_record(doc_id)?;
        let key = self.doc_key(doc_id as SegmentDocId)?;
        if record.stored_len > 0 {
            if let Some(stored_fields_layout) = self.stored_fields_layout {
                return read_index_document_from_stored_slice(
                    self.stored_slice(doc_id)?,
                    key,
                    stored_fields_layout,
                );
            }
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

    fn load_metadata(&self, doc_id: usize) -> Result<StoredDocMetadataValue> {
        let record = self.doc_record(doc_id)?;
        if record.metadata_embedded {
            return Ok(StoredDocMetadataValue {
                size: record.size,
                modified_at: record.modified_at,
                content_hash: record.content_hash,
            });
        }
        let stored_fields_layout = self.stored_fields_layout.ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} is missing stored metadata",
                self.meta.segment_id.0
            ))
        })?;
        let document = read_stored_document_view(self.stored_slice(doc_id)?, stored_fields_layout)?;
        Ok(StoredDocMetadataValue {
            size: document.size,
            modified_at: document.modified_at,
            content_hash: document.content_hash,
        })
    }

    fn read_source_bytes_from_repo(&self, key: &DocKey) -> Result<Arc<[u8]>> {
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

    fn read_source_bytes_from_store(&self, doc_id: usize) -> Result<Arc<[u8]>> {
        let stored_fields_layout = self.stored_fields_layout.ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} does not have stored fields",
                self.meta.segment_id.0
            ))
        })?;
        let document = read_stored_document_view(self.stored_slice(doc_id)?, stored_fields_layout)?;
        document.source_bytes.decode()
    }

    fn load_source_bytes(&self, doc_id: usize) -> Result<Arc<[u8]>> {
        if let Some(_stored_fields_layout) = self.stored_fields_layout {
            if self.doc_record(doc_id)?.stored_len > 0 {
                return self.read_source_bytes_from_store(doc_id);
            }
        }
        let key = self.doc_key(doc_id as SegmentDocId)?;
        self.read_source_bytes_from_repo(key)
    }

    fn source_bytes(&self, doc_id: SegmentDocId) -> Result<Arc<[u8]>> {
        let doc_id = doc_id as usize;
        let cache = self.decoded_source_bytes.get(doc_id)?;
        if let Some(bytes) = cache.get() {
            return Ok(Arc::clone(bytes));
        }
        let bytes = self.load_source_bytes(doc_id)?;
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
        self.stored_fields_path
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| {
                self.repo_root
                    .as_ref()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| self.meta.segment_id.0.clone())
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

    fn doc_metadata(&self, doc_id: SegmentDocId) -> Result<StoredDocMetadata<'_>> {
        let doc_id = doc_id as usize;
        let key = self.doc_key(doc_id as SegmentDocId)?;
        let cache = self.decoded_metadata.get(doc_id)?;
        if let Some(metadata) = cache.get() {
            return Ok(StoredDocMetadata {
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
            return Ok(StoredDocMetadata {
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
        Ok(StoredDocMetadata {
            key,
            size: metadata.size,
            modified_at: metadata.modified_at,
            content_hash: metadata.content_hash,
        })
    }

    fn stored_doc(&self, doc_id: SegmentDocId) -> Result<StoredDocRef<'_>> {
        let key = self.doc_key(doc_id)?;
        let source_bytes = self.source_bytes(doc_id)?;
        Ok(StoredDocRef { key, source_bytes })
    }

    fn verify_source(&self, doc_id: SegmentDocId) -> Result<SegmentVerifySource> {
        Ok(SegmentVerifySource::Bytes(self.source_bytes(doc_id)?))
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

fn file_name_from_logical_path(logical_path: &str) -> String {
    Path::new(logical_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(logical_path)
        .to_string()
}

fn language_from_logical_path(logical_path: &str) -> Option<String> {
    Path::new(logical_path)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
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
