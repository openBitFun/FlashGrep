use std::{
    fs,
    io::{Cursor, Read, Write},
    path::Path,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use zstd::bulk::{compress as zstd_compress, decompress as zstd_decompress};

use crate::index_engine::{
    delete::DeleteRecord,
    types::{DeleteSegmentId, DocKey, SegmentId, SeqNo},
    writer::IndexDocument,
};
use crate::{AppError, Result};

pub(crate) const SEGMENT_META_FILE: &str = "SEGMENT_META.json";
pub(crate) const DOCS_FILE: &str = "docs.bin";
pub(crate) const STORED_FIELDS_FILE: &str = "stored_fields.bin";
pub(crate) const PATH_MAP_FILE: &str = "path_map.bin";

const DOCS_MAGIC: &[u8; 8] = b"CGDOCS01";
const STORED_FIELDS_MAGIC_V1: &[u8; 8] = b"CGSTRD01";
const STORED_FIELDS_MAGIC_V2: &[u8; 8] = b"CGSTRD02";
const PATH_MAP_MAGIC: &[u8; 8] = b"CGPATH01";
const STORED_SOURCE_ENCODING_RAW: u8 = 0;
const STORED_SOURCE_ENCODING_ZSTD: u8 = 1;
const STORED_SOURCE_ZSTD_LEVEL: i32 = 3;
const STORED_SOURCE_COMPRESSION_ENABLED: bool = false;
const STORED_SOURCE_MIN_COMPRESS_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub segment_id: SegmentId,
    pub min_seq_no: SeqNo,
    pub max_seq_no: SeqNo,
    pub doc_count: u32,
    pub live_doc_count: u32,
    pub created_by_merge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteSegmentMeta {
    pub delete_segment_id: DeleteSegmentId,
    pub min_seq_no: SeqNo,
    pub max_seq_no: SeqNo,
    pub delete_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentBuildOutput {
    pub meta: SegmentMeta,
    pub documents: Vec<IndexDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteSegmentBuildOutput {
    pub meta: DeleteSegmentMeta,
    pub records: Vec<DeleteRecord>,
}

#[derive(Debug, Clone, Copy)]
struct DocRecord {
    path_index: u32,
    stored_offset: u64,
    stored_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDocAddress {
    pub key: DocKey,
    pub stored_offset: u64,
    pub stored_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredDocumentView<'a> {
    pub file_name: &'a str,
    pub language: Option<&'a str>,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
    pub source_bytes: StoredSourceBytes<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredFieldsLayout {
    V1Raw,
    V2Compressed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredSourceBytes<'a> {
    Raw(&'a [u8]),
    Zstd {
        compressed: &'a [u8],
        uncompressed_len: u32,
    },
}

impl StoredSourceBytes<'_> {
    pub(crate) fn len(self) -> usize {
        match self {
            Self::Raw(bytes) => bytes.len(),
            Self::Zstd {
                uncompressed_len, ..
            } => uncompressed_len as usize,
        }
    }

    pub(crate) fn decode(self) -> Result<Arc<[u8]>> {
        match self {
            Self::Raw(bytes) => Ok(Arc::<[u8]>::from(bytes)),
            Self::Zstd {
                compressed,
                uncompressed_len,
            } => {
                let decoded = zstd_decompress(compressed, uncompressed_len as usize).map_err(
                    |error| {
                        AppError::InvalidIndex(format!(
                            "stored source zstd decode failed: {error}"
                        ))
                    },
                )?;
                Ok(Arc::<[u8]>::from(decoded))
            }
        }
    }
}

pub(crate) fn write_segment_meta(path: &Path, meta: &SegmentMeta) -> Result<()> {
    let encoded = serde_json::to_vec_pretty(meta)
        .map_err(|error| AppError::Protocol(format!("segment meta encode failed: {error}")))?;
    fs::write(path, encoded)?;
    Ok(())
}

pub(crate) fn read_segment_meta(path: &Path) -> Result<SegmentMeta> {
    serde_json::from_slice::<SegmentMeta>(&fs::read(path)?).map_err(|error| {
        AppError::InvalidIndex(format!(
            "failed to decode segment meta {}: {error}",
            path.display()
        ))
    })
}

pub(crate) fn write_segment_documents(dir: &Path, documents: &[IndexDocument]) -> Result<()> {
    let mut path_map = Vec::new();
    write_header(&mut path_map, PATH_MAP_MAGIC)?;
    write_u32(&mut path_map, documents.len() as u32)?;

    let mut stored_fields = Vec::new();
    write_header(&mut stored_fields, STORED_FIELDS_MAGIC_V2)?;
    write_u32(&mut stored_fields, documents.len() as u32)?;

    let mut doc_records = Vec::with_capacity(documents.len());

    for (path_index, document) in documents.iter().enumerate() {
        write_string(&mut path_map, &document.repo_id.0)?;
        write_string(&mut path_map, &document.logical_path)?;

        let stored_offset = stored_fields.len() as u64;
        write_string(&mut stored_fields, &document.file_name)?;
        write_optional_string(&mut stored_fields, document.language.as_deref())?;
        write_u64(&mut stored_fields, document.size)?;
        write_u64(&mut stored_fields, document.modified_at)?;
        stored_fields.write_all(&document.content_hash)?;
        write_source_bytes(&mut stored_fields, &document.source_bytes)?;
        let stored_len = (stored_fields.len() as u64)
            .checked_sub(stored_offset)
            .ok_or_else(|| AppError::InvalidIndex("stored fields length underflow".into()))?;
        let stored_len = u32::try_from(stored_len).map_err(|_| {
            AppError::ValueOutOfRange("stored fields length exceeds u32 range".into())
        })?;

        doc_records.push(DocRecord {
            path_index: path_index as u32,
            stored_offset,
            stored_len,
        });
    }

    let mut docs = Vec::new();
    write_header(&mut docs, DOCS_MAGIC)?;
    write_u32(&mut docs, doc_records.len() as u32)?;
    for record in doc_records {
        write_u32(&mut docs, record.path_index)?;
        write_u64(&mut docs, record.stored_offset)?;
        write_u32(&mut docs, record.stored_len)?;
    }

    fs::write(dir.join(PATH_MAP_FILE), path_map)?;
    fs::write(dir.join(STORED_FIELDS_FILE), stored_fields)?;
    fs::write(dir.join(DOCS_FILE), docs)?;
    Ok(())
}

pub(crate) fn read_segment_documents(dir: &Path) -> Result<Vec<IndexDocument>> {
    let paths = read_segment_paths(dir)?;

    let stored_fields_bytes = fs::read(dir.join(STORED_FIELDS_FILE))?;
    let mut stored_cursor = Cursor::new(stored_fields_bytes.as_slice());
    let stored_fields_layout = read_stored_fields_header(&mut stored_cursor)?;
    let stored_doc_count = read_u32(&mut stored_cursor)? as usize;

    let addresses = read_segment_doc_addresses(dir)?;
    let doc_count = addresses.len();
    if doc_count != stored_doc_count || doc_count != paths.len() {
        return Err(AppError::InvalidIndex(format!(
            "segment file count mismatch: docs={doc_count}, stored={stored_doc_count}, paths={}",
            paths.len()
        )));
    }

    let mut documents = Vec::with_capacity(doc_count);
    for address in addresses {
        let start = usize::try_from(address.stored_offset)
            .map_err(|_| AppError::ValueOutOfRange("stored offset exceeds usize range".into()))?;
        let end = start
            .checked_add(address.stored_len as usize)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice overflow".into()))?;
        let slice = stored_fields_bytes
            .get(start..end)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice out of range".into()))?;
        documents.push(read_index_document_from_stored_slice(
            slice,
            &address.key,
            stored_fields_layout,
        )?);
    }

    Ok(documents)
}

pub(crate) fn read_segment_doc_addresses(dir: &Path) -> Result<Vec<SegmentDocAddress>> {
    let paths = read_segment_paths(dir)?;
    let docs_bytes = fs::read(dir.join(DOCS_FILE))?;
    let mut docs_cursor = Cursor::new(docs_bytes.as_slice());
    read_header(&mut docs_cursor, DOCS_MAGIC)?;
    let doc_count = read_u32(&mut docs_cursor)? as usize;
    if doc_count != paths.len() {
        return Err(AppError::InvalidIndex(format!(
            "segment file count mismatch: docs={doc_count}, paths={}",
            paths.len()
        )));
    }

    let mut addresses = Vec::with_capacity(doc_count);
    for _ in 0..doc_count {
        let path_index = read_u32(&mut docs_cursor)? as usize;
        let stored_offset = read_u64(&mut docs_cursor)?;
        let stored_len = read_u32(&mut docs_cursor)?;
        let (repo_id, logical_path) = paths.get(path_index).cloned().ok_or_else(|| {
            AppError::InvalidIndex(format!("path index {path_index} is out of bounds"))
        })?;
        let repo_id = crate::index_engine::types::RepoId(repo_id);
        addresses.push(SegmentDocAddress {
            key: DocKey {
                repo_id,
                logical_path,
            },
            stored_offset,
            stored_len,
        });
    }
    Ok(addresses)
}

pub(crate) fn read_index_document_from_stored_slice(
    slice: &[u8],
    key: &DocKey,
    stored_fields_layout: StoredFieldsLayout,
) -> Result<IndexDocument> {
    let view = read_stored_document_view(slice, stored_fields_layout)?;

    Ok(IndexDocument {
        key: key.clone(),
        repo_id: key.repo_id.clone(),
        logical_path: key.logical_path.clone(),
        file_name: view.file_name.to_string(),
        language: view.language.map(str::to_string),
        size: view.size,
        modified_at: view.modified_at,
        content_hash: view.content_hash,
        source_bytes: view.source_bytes.decode()?,
    })
}

pub(crate) fn read_stored_document_view(
    slice: &[u8],
    stored_fields_layout: StoredFieldsLayout,
) -> Result<StoredDocumentView<'_>> {
    let mut reader = StoredSliceReader::new(slice);
    let file_name = reader.read_string()?;
    let language = reader.read_optional_string()?;
    let size = reader.read_u64()?;
    let modified_at = reader.read_u64()?;
    let content_hash_bytes = reader.read_bytes(32)?;
    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(content_hash_bytes);
    let source_bytes = match stored_fields_layout {
        StoredFieldsLayout::V1Raw => {
            let source_len = reader.read_u32()? as usize;
            StoredSourceBytes::Raw(reader.read_bytes(source_len)?)
        }
        StoredFieldsLayout::V2Compressed => {
            let encoding = reader.read_bytes(1)?[0];
            let source_len = reader.read_u32()?;
            let payload_len = reader.read_u32()? as usize;
            let payload = reader.read_bytes(payload_len)?;
            match encoding {
                STORED_SOURCE_ENCODING_RAW => StoredSourceBytes::Raw(payload),
                STORED_SOURCE_ENCODING_ZSTD => StoredSourceBytes::Zstd {
                    compressed: payload,
                    uncompressed_len: source_len,
                },
                value => {
                    return Err(AppError::InvalidIndex(format!(
                        "invalid stored source encoding: {value}"
                    )));
                }
            }
        }
    };
    Ok(StoredDocumentView {
        file_name,
        language,
        size,
        modified_at,
        content_hash,
        source_bytes,
    })
}

pub(crate) fn read_stored_fields_layout(bytes: &[u8]) -> Result<StoredFieldsLayout> {
    let mut cursor = Cursor::new(bytes);
    read_stored_fields_header(&mut cursor)
}

fn read_segment_paths(dir: &Path) -> Result<Vec<(String, String)>> {
    let path_map_bytes = fs::read(dir.join(PATH_MAP_FILE))?;
    let mut path_map = Cursor::new(path_map_bytes.as_slice());
    read_header(&mut path_map, PATH_MAP_MAGIC)?;
    let path_count = read_u32(&mut path_map)? as usize;
    let mut paths = Vec::with_capacity(path_count);
    for _ in 0..path_count {
        let repo_id = read_string(&mut path_map)?;
        let logical_path = read_string(&mut path_map)?;
        paths.push((repo_id, logical_path));
    }
    Ok(paths)
}

fn write_header(buffer: &mut Vec<u8>, magic: &[u8; 8]) -> Result<()> {
    buffer.write_all(magic)?;
    write_u32(buffer, 1)?;
    Ok(())
}

fn read_header(cursor: &mut Cursor<&[u8]>, expected_magic: &[u8; 8]) -> Result<()> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != expected_magic {
        return Err(AppError::InvalidIndex("segment file magic mismatch".into()));
    }
    let version = read_u32(cursor)?;
    if version != 1 {
        return Err(AppError::InvalidIndex(format!(
            "unsupported segment file version: {version}"
        )));
    }
    Ok(())
}

fn read_stored_fields_header(cursor: &mut Cursor<&[u8]>) -> Result<StoredFieldsLayout> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    let version = read_u32(cursor)?;
    if version != 1 {
        return Err(AppError::InvalidIndex(format!(
            "unsupported stored fields version: {version}"
        )));
    }
    match &magic {
        magic if magic == STORED_FIELDS_MAGIC_V1 => Ok(StoredFieldsLayout::V1Raw),
        magic if magic == STORED_FIELDS_MAGIC_V2 => Ok(StoredFieldsLayout::V2Compressed),
        _ => Err(AppError::InvalidIndex(
            "stored fields magic mismatch".into(),
        )),
    }
}

fn write_u32(writer: &mut impl Write, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_string(buffer: &mut Vec<u8>, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("string length exceeds u32 range".into()))?;
    write_u32(buffer, len)?;
    buffer.write_all(bytes)?;
    Ok(())
}

fn write_optional_string(buffer: &mut Vec<u8>, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            buffer.write_all(&[1])?;
            write_string(buffer, value)?;
        }
        None => buffer.write_all(&[0])?,
    }
    Ok(())
}

pub(crate) fn write_source_bytes(writer: &mut impl Write, value: &[u8]) -> Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| AppError::ValueOutOfRange("byte slice length exceeds u32 range".into()))?;
    if let Some(compressed) = maybe_compress_source_bytes(value)? {
        writer.write_all(&[STORED_SOURCE_ENCODING_ZSTD])?;
        write_u32(writer, len)?;
        write_u32(
            writer,
            u32::try_from(compressed.len()).map_err(|_| {
                AppError::ValueOutOfRange(
                    "compressed byte slice length exceeds u32 range".into(),
                )
            })?,
        )?;
        writer.write_all(&compressed)?;
        return Ok(());
    }

    writer.write_all(&[STORED_SOURCE_ENCODING_RAW])?;
    write_u32(writer, len)?;
    write_u32(writer, len)?;
    writer.write_all(value)?;
    Ok(())
}

fn maybe_compress_source_bytes(value: &[u8]) -> Result<Option<Vec<u8>>> {
    if !STORED_SOURCE_COMPRESSION_ENABLED {
        return Ok(None);
    }
    if value.len() < STORED_SOURCE_MIN_COMPRESS_BYTES {
        return Ok(None);
    }

    let compressed = zstd_compress(value, STORED_SOURCE_ZSTD_LEVEL)
        .map_err(|error| AppError::Protocol(format!("stored source zstd encode failed: {error}")))?;
    if compressed.len() + 16 >= value.len() {
        return Ok(None);
    }
    Ok(Some(compressed))
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut bytes = [0u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> Result<String> {
    let len = read_u32(cursor)? as usize;
    let mut bytes = vec![0u8; len];
    cursor.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|error| AppError::Protocol(format!("segment string decode failed: {error}")))
}

fn read_optional_string(cursor: &mut Cursor<&[u8]>) -> Result<Option<String>> {
    let mut tag = [0u8; 1];
    cursor.read_exact(&mut tag)?;
    match tag[0] {
        0 => Ok(None),
        1 => Ok(Some(read_string(cursor)?)),
        value => Err(AppError::InvalidIndex(format!(
            "invalid optional string tag: {value}"
        ))),
    }
}

struct StoredSliceReader<'a> {
    slice: &'a [u8],
    offset: usize,
}

impl<'a> StoredSliceReader<'a> {
    fn new(slice: &'a [u8]) -> Self {
        Self { slice, offset: 0 }
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        let mut raw = [0u8; 4];
        raw.copy_from_slice(bytes);
        Ok(u32::from_le_bytes(raw))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_bytes(8)?;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(raw))
    }

    fn read_string(&mut self) -> Result<&'a str> {
        let len = self.read_u32()? as usize;
        let bytes = self.read_bytes(len)?;
        std::str::from_utf8(bytes)
            .map_err(|error| AppError::Protocol(format!("segment string decode failed: {error}")))
    }

    fn read_optional_string(&mut self) -> Result<Option<&'a str>> {
        let tag = self.read_bytes(1)?[0];
        match tag {
            0 => Ok(None),
            1 => Ok(Some(self.read_string()?)),
            value => Err(AppError::InvalidIndex(format!(
                "invalid optional string tag: {value}"
            ))),
        }
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice overflow".into()))?;
        let bytes = self
            .slice
            .get(self.offset..end)
            .ok_or_else(|| AppError::InvalidIndex("stored field slice out of range".into()))?;
        self.offset = end;
        Ok(bytes)
    }
}
