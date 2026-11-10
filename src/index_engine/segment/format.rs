use std::{
    env,
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::Path,
    sync::Arc,
    time::Instant,
};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use zstd::bulk::{compress as zstd_compress, decompress as zstd_decompress};

use crate::index_engine::{
    delete::DeleteRecord,
    segment::{advise_mmap_random, advise_mmap_sequential},
    types::{DeleteSegmentId, DocKey, RepoId, SegmentId, SeqNo},
    writer::IndexDocument,
};
use crate::{fs_atomic::replace_file, AppError, Result};

pub(crate) const SEGMENT_META_FILE: &str = "SEGMENT_META.json";
pub(crate) const DOCS_FILE: &str = "docs.bin";
pub(crate) const PATH_MAP_FILE: &str = "path_map.bin";
pub(crate) const PATH_OFFSETS_FILE: &str = "path_offsets.bin";

const DOCS_MAGIC: &[u8; 8] = b"CGDOCS01";
const DOCS_VERSION: u32 = 4;
const PATH_MAP_MAGIC: &[u8; 8] = b"CGPATH01";
const SOURCE_ENCODING_RAW: u8 = 0;
const SOURCE_ENCODING_ZSTD: u8 = 1;
const SOURCE_ZSTD_LEVEL: i32 = 3;
const SOURCE_MIN_COMPRESS_BYTES: usize = 4 * 1024;
const PATH_MAP_HEADER_LEN: usize = 16;
const DOCS_HEADER_LEN: usize = 16;
const DOC_RECORD_LEN: usize = 72;
const PATH_OFFSETS_MAGIC: &[u8; 8] = b"CGPTOF01";
const PATH_OFFSETS_VERSION: u32 = 1;

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
    source_offset: u64,
    source_len: u32,
    size: u64,
    modified_at: u64,
    content_hash: [u8; 32],
    source_kind: SegmentDocSourceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SegmentDocSourceKind {
    Inline = 0,
    Worktree = 1,
    GitHeadBlob = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentDocRecord {
    pub path_index: u32,
    pub source_offset: u64,
    pub source_len: u32,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
    pub source_kind: SegmentDocSourceKind,
}

#[derive(Debug)]
pub struct FileSegmentDocTable {
    path_map_mmap: Mmap,
    docs_mmap: Mmap,
    doc_count: usize,
    path_entry_offsets: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodedSourceBytes<'a> {
    Raw(&'a [u8]),
    Zstd {
        compressed: &'a [u8],
        uncompressed_len: u32,
    },
}

impl EncodedSourceBytes<'_> {
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
                let decoded =
                    zstd_decompress(compressed, uncompressed_len as usize).map_err(|error| {
                        AppError::InvalidIndex(format!("inline source zstd decode failed: {error}"))
                    })?;
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
    let mut path_entry_offsets = Vec::with_capacity(documents.len());
    let source_data_start = DOCS_HEADER_LEN
        .checked_add(documents.len().checked_mul(DOC_RECORD_LEN).ok_or_else(|| {
            AppError::ValueOutOfRange("docs record region exceeds usize range".into())
        })?)
        .ok_or_else(|| {
            AppError::ValueOutOfRange("docs record region exceeds usize range".into())
        })?;
    let mut next_source_offset = u64::try_from(source_data_start)
        .map_err(|_| AppError::ValueOutOfRange("docs source offset exceeds u64 range".into()))?;
    let mut encoded_sources = Vec::new();

    let mut doc_records = Vec::with_capacity(documents.len());

    for (path_index, document) in documents.iter().enumerate() {
        path_entry_offsets.push(path_map.len() as u64);
        write_string(&mut path_map, &document.repo_id.0)?;
        write_string(&mut path_map, &document.logical_path)?;

        let source_len = write_source_bytes(&mut encoded_sources, &document.source_bytes)?;
        let source_len = u32::try_from(source_len).map_err(|_| {
            AppError::ValueOutOfRange("inline source length exceeds u32 range".into())
        })?;

        doc_records.push(DocRecord {
            path_index: path_index as u32,
            source_offset: next_source_offset,
            source_len,
            size: document.size,
            modified_at: document.modified_at,
            content_hash: document.content_hash,
            source_kind: SegmentDocSourceKind::Inline,
        });

        next_source_offset = next_source_offset
            .checked_add(u64::from(source_len))
            .ok_or_else(|| {
                AppError::ValueOutOfRange("docs source offset exceeds u64 range".into())
            })?;
    }

    let mut docs = Vec::new();
    write_header_with_version(&mut docs, DOCS_MAGIC, DOCS_VERSION)?;
    write_u32(&mut docs, doc_records.len() as u32)?;
    for record in doc_records {
        write_doc_record(&mut docs, &record)?;
    }
    docs.write_all(&encoded_sources)?;

    fs::write(dir.join(PATH_MAP_FILE), path_map)?;
    write_path_offsets_file(&dir.join(PATH_OFFSETS_FILE), &path_entry_offsets)?;
    fs::write(dir.join(DOCS_FILE), docs)?;
    Ok(())
}

fn load_or_create_path_offsets(
    dir: &Path,
    path_map_mmap: &Mmap,
    path_count: usize,
) -> Result<(Vec<usize>, &'static str)> {
    match read_path_offsets_file(&dir.join(PATH_OFFSETS_FILE), path_count) {
        Ok(offsets) => return Ok((offsets, "sidecar")),
        Err(error) if is_path_offsets_rebuildable_error(&error) => {}
        Err(error) => return Err(error),
    }

    advise_mmap_sequential(path_map_mmap);
    let mut path_map_cursor = Cursor::new(path_map_mmap.as_ref());
    read_header(&mut path_map_cursor, PATH_MAP_MAGIC)?;
    let scanned_path_count = read_u32(&mut path_map_cursor)? as usize;
    if scanned_path_count != path_count {
        return Err(AppError::InvalidIndex(format!(
            "path map count mismatch while rebuilding offsets: expected {path_count}, got {scanned_path_count}"
        )));
    }
    let mut offsets = Vec::with_capacity(path_count);
    for _ in 0..path_count {
        let entry_offset = usize::try_from(path_map_cursor.position())
            .map_err(|_| AppError::ValueOutOfRange("path map offset exceeds usize range".into()))?;
        offsets.push(entry_offset);
        skip_length_prefixed_string(&mut path_map_cursor, "repo id")?;
        skip_length_prefixed_string(&mut path_map_cursor, "logical path")?;
    }

    let encoded_offsets = offsets
        .iter()
        .map(|offset| {
            u64::try_from(*offset)
                .map_err(|_| AppError::ValueOutOfRange("path offset exceeds u64 range".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let _ = write_path_offsets_file(&dir.join(PATH_OFFSETS_FILE), &encoded_offsets);
    Ok((offsets, "scanned"))
}

fn read_path_offsets_file(path: &Path, expected_count: usize) -> Result<Vec<usize>> {
    let bytes = fs::read(path)?;
    if bytes.len() < 16 {
        return Err(AppError::InvalidIndex(format!(
            "path offsets file is truncated: {}",
            path.display()
        )));
    }
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != PATH_OFFSETS_MAGIC {
        return Err(AppError::InvalidIndex(format!(
            "path offsets magic mismatch: {}",
            path.display()
        )));
    }
    let version = read_u32(&mut cursor)?;
    if version != PATH_OFFSETS_VERSION {
        return Err(AppError::InvalidIndex(format!(
            "unsupported path offsets version {version}: {}",
            path.display()
        )));
    }
    let count = read_u32(&mut cursor)? as usize;
    if count != expected_count {
        return Err(AppError::InvalidIndex(format!(
            "path offsets count mismatch: expected {expected_count}, got {count}"
        )));
    }
    let expected_len = 16usize
        .checked_add(count.checked_mul(8).ok_or_else(|| {
            AppError::ValueOutOfRange("path offsets file size exceeds usize range".into())
        })?)
        .ok_or_else(|| {
            AppError::ValueOutOfRange("path offsets file size exceeds usize range".into())
        })?;
    if bytes.len() < expected_len {
        return Err(AppError::InvalidIndex(format!(
            "path offsets file is truncated: expected {expected_len} bytes, got {}",
            bytes.len()
        )));
    }
    let mut offsets = Vec::with_capacity(count);
    for _ in 0..count {
        let offset = read_u64(&mut cursor)?;
        offsets
            .push(usize::try_from(offset).map_err(|_| {
                AppError::ValueOutOfRange("path offset exceeds usize range".into())
            })?);
    }
    Ok(offsets)
}

pub(crate) fn write_path_offsets_file(path: &Path, offsets: &[u64]) -> Result<()> {
    let mut buffer = Vec::with_capacity(
        16usize
            .checked_add(offsets.len().checked_mul(8).ok_or_else(|| {
                AppError::ValueOutOfRange("path offsets file size exceeds usize range".into())
            })?)
            .ok_or_else(|| {
                AppError::ValueOutOfRange("path offsets file size exceeds usize range".into())
            })?,
    );
    buffer.write_all(PATH_OFFSETS_MAGIC)?;
    write_u32(&mut buffer, PATH_OFFSETS_VERSION)?;
    write_u32(
        &mut buffer,
        u32::try_from(offsets.len()).map_err(|_| {
            AppError::ValueOutOfRange("path offsets count exceeds u32 range".into())
        })?,
    )?;
    for offset in offsets {
        write_u64(&mut buffer, *offset)?;
    }

    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, &buffer)?;
    replace_file(&tmp_path, path)?;
    Ok(())
}

fn is_path_offsets_rebuildable_error(error: &AppError) -> bool {
    match error {
        AppError::InvalidIndex(_) => true,
        AppError::Io(io_error) => io_error.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

impl FileSegmentDocTable {
    pub fn open(dir: &Path) -> Result<Self> {
        let profile_open = env::var_os("FLASHGREP_PROFILE_OPEN_SEGMENT").is_some();

        let path_map_started = Instant::now();
        let path_map_file = File::open(dir.join(PATH_MAP_FILE))?;
        let path_map_mmap = unsafe { Mmap::map(&path_map_file)? };
        if path_map_mmap.len() < PATH_MAP_HEADER_LEN {
            return Err(AppError::InvalidIndex(format!(
                "path map file is truncated: expected at least {PATH_MAP_HEADER_LEN} bytes, got {}",
                path_map_mmap.len()
            )));
        }
        let mut path_map_cursor = Cursor::new(path_map_mmap.as_ref());
        read_header(&mut path_map_cursor, PATH_MAP_MAGIC)?;
        let path_count = read_u32(&mut path_map_cursor)? as usize;
        let (path_entry_offsets, path_offsets_source) =
            load_or_create_path_offsets(dir, &path_map_mmap, path_count)?;
        let path_map_scan_elapsed = path_map_started.elapsed();
        advise_mmap_random(&path_map_mmap);

        let docs_started = Instant::now();
        let docs_file = File::open(dir.join(DOCS_FILE))?;
        let docs_mmap = unsafe { Mmap::map(&docs_file)? };
        // Metadata and selected source records are queried sparsely. Sequential
        // readahead would pull unrelated embedded source pages into memory.
        advise_mmap_random(&docs_mmap);
        if docs_mmap.len() < DOCS_HEADER_LEN {
            return Err(AppError::InvalidIndex(format!(
                "docs file is truncated: expected at least {DOCS_HEADER_LEN} bytes, got {}",
                docs_mmap.len()
            )));
        }
        let mut docs_cursor = Cursor::new(docs_mmap.as_ref());
        read_docs_header(&mut docs_cursor)?;
        let doc_count = read_u32(&mut docs_cursor)? as usize;
        let docs_header_elapsed = docs_started.elapsed();
        if doc_count != path_count {
            return Err(AppError::InvalidIndex(format!(
                "segment file count mismatch: docs={doc_count}, paths={path_count}",
            )));
        }

        let expected_docs_len = DOCS_HEADER_LEN
            .checked_add(doc_count.checked_mul(DOC_RECORD_LEN).ok_or_else(|| {
                AppError::ValueOutOfRange("docs file size exceeds usize range".into())
            })?)
            .ok_or_else(|| {
                AppError::ValueOutOfRange("docs file size exceeds usize range".into())
            })?;
        if docs_mmap.len() < expected_docs_len {
            return Err(AppError::InvalidIndex(format!(
                "docs file is truncated: expected at least {expected_docs_len} bytes, got {}",
                docs_mmap.len()
            )));
        }

        if profile_open {
            eprintln!(
                "segment_doc_table dir={} path_offsets={} path_map_open_ms={:.3} docs_header_ms={:.3} doc_count={}",
                dir.display(),
                path_offsets_source,
                path_map_scan_elapsed.as_secs_f64() * 1000.0,
                docs_header_elapsed.as_secs_f64() * 1000.0,
                doc_count,
            );
        }

        Ok(Self {
            path_map_mmap,
            docs_mmap,
            doc_count,
            path_entry_offsets,
        })
    }

    pub fn len(&self) -> usize {
        self.doc_count
    }

    pub fn record(&self, doc_id: usize) -> Result<SegmentDocRecord> {
        if doc_id >= self.doc_count {
            return Err(AppError::InvalidIndex(format!(
                "segment doc {doc_id} is out of bounds"
            )));
        }
        let start = DOCS_HEADER_LEN
            .checked_add(doc_id.checked_mul(DOC_RECORD_LEN).ok_or_else(|| {
                AppError::ValueOutOfRange("docs offset exceeds usize range".into())
            })?)
            .ok_or_else(|| AppError::ValueOutOfRange("docs offset exceeds usize range".into()))?;
        let end = start
            .checked_add(DOC_RECORD_LEN)
            .ok_or_else(|| AppError::InvalidIndex("docs record slice overflow".into()))?;
        let bytes = self.docs_mmap.get(start..end).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} is out of range"))
        })?;

        let path_index = read_u32_from_slice(bytes, 0)?;
        let source_offset = read_u64_from_slice(bytes, 4)?;
        let source_len = read_u32_from_slice(bytes, 12)?;
        let size = read_u64_from_slice(bytes, 16)?;
        let modified_at = read_u64_from_slice(bytes, 24)?;
        let content_hash = read_fixed_32_from_slice(bytes, 32)?;
        let source_kind = read_source_kind_from_slice(bytes, 64)?;
        Ok(SegmentDocRecord {
            path_index,
            source_offset,
            source_len,
            size,
            modified_at,
            content_hash,
            source_kind,
        })
    }

    pub fn source_slice(&self, doc_id: usize) -> Result<&[u8]> {
        let record = self.record(doc_id)?;
        let start = usize::try_from(record.source_offset)
            .map_err(|_| AppError::ValueOutOfRange("source offset exceeds usize range".into()))?;
        let end = start
            .checked_add(record.source_len as usize)
            .ok_or_else(|| AppError::InvalidIndex("source slice overflow".into()))?;
        self.docs_mmap.get(start..end).ok_or_else(|| {
            AppError::InvalidIndex(format!("segment doc {doc_id} source is out of range"))
        })
    }

    pub fn doc_key(&self, doc_id: usize) -> Result<DocKey> {
        let record = self.record(doc_id)?;
        let (repo_id, logical_path) = self.path_entry(record.path_index as usize)?;
        Ok(DocKey {
            repo_id: RepoId(repo_id),
            logical_path,
        })
    }

    fn path_entry(&self, path_index: usize) -> Result<(String, String)> {
        let offset = *self.path_entry_offsets.get(path_index).ok_or_else(|| {
            AppError::InvalidIndex(format!("path index {path_index} is out of bounds"))
        })?;
        let slice = self.path_map_mmap.get(offset..).ok_or_else(|| {
            AppError::InvalidIndex(format!("path map offset {offset} is out of range"))
        })?;
        let mut cursor = Cursor::new(slice);
        let repo_id = read_string(&mut cursor)?;
        let logical_path = read_string(&mut cursor)?;
        Ok((repo_id, logical_path))
    }
}

pub(crate) fn read_index_document_from_inline_slice(
    slice: &[u8],
    key: &DocKey,
    record: SegmentDocRecord,
) -> Result<IndexDocument> {
    let source_bytes = read_encoded_source_bytes_view(slice)?.decode()?;

    Ok(IndexDocument {
        key: key.clone(),
        repo_id: key.repo_id.clone(),
        logical_path: key.logical_path.clone(),
        file_name: file_name_from_logical_path(&key.logical_path),
        language: language_from_logical_path(&key.logical_path),
        size: record.size,
        modified_at: record.modified_at,
        content_hash: record.content_hash,
        source_bytes,
    })
}

pub(crate) fn read_encoded_source_bytes_view(slice: &[u8]) -> Result<EncodedSourceBytes<'_>> {
    let mut reader = StoredSliceReader::new(slice);
    let encoding = reader.read_bytes(1)?[0];
    let source_len = reader.read_u32()?;
    let payload_len = reader.read_u32()? as usize;
    let payload = reader.read_bytes(payload_len)?;
    match encoding {
        SOURCE_ENCODING_RAW => Ok(EncodedSourceBytes::Raw(payload)),
        SOURCE_ENCODING_ZSTD => Ok(EncodedSourceBytes::Zstd {
            compressed: payload,
            uncompressed_len: source_len,
        }),
        value => Err(AppError::InvalidIndex(format!(
            "invalid inline source encoding: {value}"
        ))),
    }
}

fn write_header(buffer: &mut Vec<u8>, magic: &[u8; 8]) -> Result<()> {
    write_header_with_version(buffer, magic, 1)
}

fn write_header_with_version(buffer: &mut Vec<u8>, magic: &[u8; 8], version: u32) -> Result<()> {
    buffer.write_all(magic)?;
    write_u32(buffer, version)?;
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

fn read_docs_header(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != DOCS_MAGIC {
        return Err(AppError::InvalidIndex("segment file magic mismatch".into()));
    }
    let version = read_u32(cursor)?;
    if version != DOCS_VERSION {
        return Err(AppError::InvalidIndex(format!(
            "unsupported docs file version: {version}"
        )));
    }
    Ok(version)
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

pub(crate) fn write_source_bytes(writer: &mut impl Write, value: &[u8]) -> Result<u64> {
    let len = u32::try_from(value.len())
        .map_err(|_| AppError::ValueOutOfRange("byte slice length exceeds u32 range".into()))?;
    if let Some(compressed) = maybe_compress_source_bytes(value)? {
        writer.write_all(&[SOURCE_ENCODING_ZSTD])?;
        write_u32(writer, len)?;
        write_u32(
            writer,
            u32::try_from(compressed.len()).map_err(|_| {
                AppError::ValueOutOfRange("compressed byte slice length exceeds u32 range".into())
            })?,
        )?;
        writer.write_all(&compressed)?;
        return Ok(9 + compressed.len() as u64);
    }

    writer.write_all(&[SOURCE_ENCODING_RAW])?;
    write_u32(writer, len)?;
    write_u32(writer, len)?;
    writer.write_all(value)?;
    Ok(9 + u64::from(len))
}

fn maybe_compress_source_bytes(value: &[u8]) -> Result<Option<Vec<u8>>> {
    if value.len() < SOURCE_MIN_COMPRESS_BYTES {
        return Ok(None);
    }

    let compressed = zstd_compress(value, SOURCE_ZSTD_LEVEL).map_err(|error| {
        AppError::Protocol(format!("inline source zstd encode failed: {error}"))
    })?;
    if compressed.len() + 16 >= value.len() {
        return Ok(None);
    }
    Ok(Some(compressed))
}

fn write_doc_record(writer: &mut impl Write, record: &DocRecord) -> Result<()> {
    write_u32(writer, record.path_index)?;
    write_u64(writer, record.source_offset)?;
    write_u32(writer, record.source_len)?;
    write_u64(writer, record.size)?;
    write_u64(writer, record.modified_at)?;
    writer.write_all(&record.content_hash)?;
    writer.write_all(&[record.source_kind as u8])?;
    writer.write_all(&[0u8; 7])?;
    Ok(())
}

fn read_source_kind_from_slice(slice: &[u8], offset: usize) -> Result<SegmentDocSourceKind> {
    let raw = *slice
        .get(offset)
        .ok_or_else(|| AppError::InvalidIndex("source kind is out of range".into()))?;
    SegmentDocSourceKind::from_u8(raw)
}

impl SegmentDocSourceKind {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Inline),
            1 => Ok(Self::Worktree),
            2 => Ok(Self::GitHeadBlob),
            other => Err(AppError::InvalidIndex(format!(
                "invalid segment doc source kind: {other}"
            ))),
        }
    }
}

fn skip_length_prefixed_string(cursor: &mut Cursor<&[u8]>, field: &str) -> Result<()> {
    let len = read_u32(cursor)? as u64;
    let next = cursor
        .position()
        .checked_add(len)
        .ok_or_else(|| AppError::InvalidIndex(format!("{field} cursor overflow")))?;
    if next > cursor.get_ref().len() as u64 {
        return Err(AppError::InvalidIndex(format!(
            "{field} is out of range in path map"
        )));
    }
    cursor.set_position(next);
    Ok(())
}

fn read_u32_from_slice(slice: &[u8], offset: usize) -> Result<u32> {
    let bytes = slice
        .get(offset..offset + 4)
        .ok_or_else(|| AppError::InvalidIndex("u32 slice is out of range".into()))?;
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes);
    Ok(u32::from_le_bytes(raw))
}

fn read_u64_from_slice(slice: &[u8], offset: usize) -> Result<u64> {
    let bytes = slice
        .get(offset..offset + 8)
        .ok_or_else(|| AppError::InvalidIndex("u64 slice is out of range".into()))?;
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(raw))
}

fn read_fixed_32_from_slice(slice: &[u8], offset: usize) -> Result<[u8; 32]> {
    let bytes = slice
        .get(offset..offset + 32)
        .ok_or_else(|| AppError::InvalidIndex("fixed 32-byte slice is out of range".into()))?;
    let mut raw = [0u8; 32];
    raw.copy_from_slice(bytes);
    Ok(raw)
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

pub(crate) fn file_name_from_logical_path(logical_path: &str) -> String {
    Path::new(logical_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(logical_path)
        .to_string()
}

pub(crate) fn language_from_logical_path(logical_path: &str) -> Option<String> {
    Path::new(logical_path)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
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
