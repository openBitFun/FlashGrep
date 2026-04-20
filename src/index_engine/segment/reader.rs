use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use memmap2::Mmap;

use crate::{
    files::resolve_repo_path,
    index_engine::{
        query::{QueryFilters, QueryPlan},
        segment::{
            advise_mmap_sequential, read_index_document_from_stored_slice,
            read_stored_document_view, read_stored_fields_layout, PostingLists, SegmentDocAddress,
            StoredFieldsLayout,
        },
        types::{DocKey, SegmentDocId},
        writer::IndexDocument,
    },
    AppError, Result,
};

use super::SegmentMeta;

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
    doc_addresses: Vec<SegmentDocAddress>,
    posting_lists: PostingLists,
    path_doc_ids: BTreeMap<String, Vec<SegmentDocId>>,
    stored_fields_path: Option<PathBuf>,
    stored_fields_mmap: Option<Mmap>,
    stored_fields_layout: Option<StoredFieldsLayout>,
    repo_root: Option<PathBuf>,
    decoded_documents: Vec<OnceLock<IndexDocument>>,
    decoded_metadata: Vec<OnceLock<StoredDocMetadataValue>>,
}

impl FileSegmentReader {
    pub fn new(
        meta: SegmentMeta,
        doc_addresses: Vec<SegmentDocAddress>,
        posting_lists: PostingLists,
        stored_fields_path: Option<PathBuf>,
        repo_root: Option<PathBuf>,
    ) -> Result<Self> {
        let (stored_fields_mmap, stored_fields_layout) = if let Some(path) = &stored_fields_path {
            let stored_fields_file = File::open(path)?;
            let stored_fields_mmap = unsafe { Mmap::map(&stored_fields_file)? };
            advise_mmap_sequential(&stored_fields_mmap);
            let stored_fields_layout = read_stored_fields_layout(stored_fields_mmap.as_ref())?;
            (Some(stored_fields_mmap), Some(stored_fields_layout))
        } else {
            (None, None)
        };
        let decoded_documents = (0..doc_addresses.len())
            .map(|_| OnceLock::new())
            .collect::<Vec<_>>();
        let decoded_metadata = (0..doc_addresses.len())
            .map(|_| OnceLock::new())
            .collect::<Vec<_>>();
        let path_doc_ids = build_path_doc_ids(
            doc_addresses
                .iter()
                .enumerate()
                .map(|(doc_id, doc)| (doc.key.logical_path.as_str(), doc_id as SegmentDocId)),
        );
        Ok(Self {
            meta,
            doc_addresses,
            posting_lists,
            path_doc_ids,
            stored_fields_path,
            stored_fields_mmap,
            stored_fields_layout,
            repo_root,
            decoded_documents,
            decoded_metadata,
        })
    }

    fn stored_slice(&self, doc_id: usize) -> Result<&[u8]> {
        let stored_fields_mmap = self.stored_fields_mmap.as_ref().ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "segment {} does not have stored fields",
                self.meta.segment_id.0
            ))
        })?;
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        let start = usize::try_from(address.stored_offset)
            .map_err(|_| AppError::ValueOutOfRange("stored offset exceeds usize range".into()))?;
        let end = start
            .checked_add(address.stored_len as usize)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice overflow".into()))?;
        stored_fields_mmap.get(start..end).ok_or_else(|| {
            AppError::InvalidIndex(format!(
                "stored field slice out of range for {}",
                self.segment_source_label()
            ))
        })
    }

    fn load_document(&self, doc_id: usize) -> Result<IndexDocument> {
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        if let Some(stored_fields_layout) = self.stored_fields_layout {
            return read_index_document_from_stored_slice(
                self.stored_slice(doc_id)?,
                &address.key,
                stored_fields_layout,
            );
        }

        Ok(IndexDocument {
            key: address.key.clone(),
            repo_id: address.key.repo_id.clone(),
            logical_path: address.key.logical_path.clone(),
            file_name: file_name_from_logical_path(&address.key.logical_path),
            language: language_from_logical_path(&address.key.logical_path),
            size: address.size,
            modified_at: address.modified_at,
            content_hash: address.content_hash,
            source_bytes: self.read_source_bytes_from_repo(address)?,
        })
    }

    fn load_metadata(&self, doc_id: usize) -> Result<StoredDocMetadataValue> {
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        if address.metadata_embedded {
            return Ok(StoredDocMetadataValue {
                size: address.size,
                modified_at: address.modified_at,
                content_hash: address.content_hash,
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

    fn read_source_bytes_from_repo(&self, address: &SegmentDocAddress) -> Result<Arc<[u8]>> {
        let path = self.repo_path_for_key(&address.key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Arc::<[u8]>::from(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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
        self.doc_addresses.len()
    }

    fn collect_doc_ids_for_filters(&self, filters: &QueryFilters) -> Result<Vec<SegmentDocId>> {
        Ok(collect_doc_ids_for_filters_impl(
            self.doc_addresses.len() as SegmentDocId,
            &self.path_doc_ids,
            &filters.path_prefixes,
        ))
    }

    fn collect_candidates(&self, plan: &QueryPlan, _field: u16) -> Result<Vec<SegmentDocId>> {
        let candidate_ids = collect_candidate_ids(
            &self.posting_lists,
            plan,
            self.doc_addresses.len() as SegmentDocId,
        )?;
        Ok(filter_candidate_ids_by_path_prefixes(
            candidate_ids,
            &self.path_doc_ids,
            &plan.request.filters.path_prefixes,
        ))
    }

    fn document(&self, doc_id: SegmentDocId) -> Result<&IndexDocument> {
        let doc_id = doc_id as usize;
        let slot = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        let _ = slot;
        let cache = self.decoded_documents.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
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
        self.doc_addresses
            .get(doc_id as usize)
            .map(|doc| &doc.key)
            .ok_or_else(|| AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds")))
    }

    fn doc_metadata(&self, doc_id: SegmentDocId) -> Result<StoredDocMetadata<'_>> {
        let doc_id = doc_id as usize;
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        let cache = self.decoded_metadata.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        if let Some(metadata) = cache.get() {
            return Ok(StoredDocMetadata {
                key: &address.key,
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
                key: &address.key,
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
            key: &address.key,
            size: metadata.size,
            modified_at: metadata.modified_at,
            content_hash: metadata.content_hash,
        })
    }

    fn stored_doc(&self, doc_id: SegmentDocId) -> Result<StoredDocRef<'_>> {
        let doc_id = doc_id as usize;
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        let source_bytes = if let Some(stored_fields_layout) = self.stored_fields_layout {
            let document =
                read_stored_document_view(self.stored_slice(doc_id)?, stored_fields_layout)?;
            document.source_bytes.decode()?
        } else {
            self.read_source_bytes_from_repo(address)?
        };
        Ok(StoredDocRef {
            key: &address.key,
            source_bytes,
        })
    }

    fn verify_source(&self, doc_id: SegmentDocId) -> Result<SegmentVerifySource> {
        let doc_id = doc_id as usize;
        let address = self.doc_addresses.get(doc_id).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of bounds"))
        })?;
        if self.stored_fields_layout.is_none() {
            let path = self.repo_path_for_key(&address.key)?;
            if !path.is_file() {
                return Ok(SegmentVerifySource::Bytes(Arc::<[u8]>::from(
                    Vec::<u8>::new(),
                )));
            }
            return Ok(SegmentVerifySource::Path(path));
        }
        Ok(SegmentVerifySource::Bytes(
            self.read_source_bytes_from_store(doc_id)?,
        ))
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
