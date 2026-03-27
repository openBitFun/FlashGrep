use std::{
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

use memmap2::Mmap;

use crate::{
    config::{CorpusMode, TokenizerMode},
    error::{AppError, Result},
};

const DOCS_MAGIC: &[u8; 4] = b"BFGD";
const DOC_TERMS_MAGIC: &[u8; 4] = b"BFGT";
const LOOKUP_MAGIC: &[u8; 4] = b"BFGL";
const POSTINGS_MAGIC: &[u8; 4] = b"BFGP";
const DOCS_FORMAT_VERSION: u32 = 5;
const DOCS_FORMAT_VERSION_LEGACY: u32 = 4;
const FORMAT_VERSION: u32 = 4;
const LOOKUP_ENTRY_SIZE: usize = 28;
pub const LOOKUP_FLAG_SKIPPED_HIGH_FREQ: u32 = 1;
const CURRENT_FILE_NAME: &str = "CURRENT";
const GENERATIONS_DIR_NAME: &str = "generations";

#[derive(Debug, Clone)]
pub struct DocMeta {
    pub doc_id: u32,
    pub path: String,
    pub size: u64,
    pub mtime_nanos: u64,
}

#[derive(Debug, Clone)]
pub struct IndexMetadata {
    pub tokenizer: TokenizerMode,
    pub min_sparse_len: usize,
    pub max_sparse_len: usize,
    pub build: Option<IndexBuildSettings>,
}

#[derive(Debug, Clone)]
pub struct IndexBuildSettings {
    pub repo_root: String,
    pub corpus_mode: CorpusMode,
    pub include_hidden: bool,
    pub max_file_size: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LookupEntry {
    pub token_hash: u64,
    pub offset: u64,
    pub byte_len: u32,
    pub doc_freq: u32,
    pub flags: u32,
}

impl LookupEntry {
    pub fn is_skipped_high_freq(self) -> bool {
        (self.flags & LOOKUP_FLAG_SKIPPED_HIGH_FREQ) != 0
    }
}

#[derive(Debug)]
pub struct IndexLayout {
    pub root_path: PathBuf,
    pub data_path: PathBuf,
    pub docs_path: PathBuf,
    pub doc_terms_path: PathBuf,
    pub lookup_path: PathBuf,
    pub postings_path: PathBuf,
}

impl IndexLayout {
    pub fn new(root: &Path) -> Self {
        Self::at_data_path(root.to_path_buf(), root.to_path_buf())
    }

    pub fn resolve(root: &Path) -> Result<Self> {
        let legacy_docs = root.join("docs.bin");
        if legacy_docs.exists() {
            return Ok(Self::new(root));
        }

        let current_path = root.join(CURRENT_FILE_NAME);
        if !current_path.exists() {
            return Ok(Self::new(root));
        }

        let generation = fs::read_to_string(&current_path)?;
        let generation = generation.trim();
        if generation.is_empty() {
            return Err(AppError::InvalidIndex(
                "CURRENT points to an empty generation".into(),
            ));
        }

        Ok(Self::for_generation(root, generation))
    }

    pub fn for_generation(root: &Path, generation: &str) -> Self {
        Self::at_data_path(
            root.to_path_buf(),
            root.join(GENERATIONS_DIR_NAME).join(generation),
        )
    }

    pub fn current_path(root: &Path) -> PathBuf {
        root.join(CURRENT_FILE_NAME)
    }

    pub fn generations_dir(root: &Path) -> PathBuf {
        root.join(GENERATIONS_DIR_NAME)
    }

    fn at_data_path(root_path: PathBuf, data_path: PathBuf) -> Self {
        Self {
            root_path,
            docs_path: data_path.join("docs.bin"),
            doc_terms_path: data_path.join("doc_terms.bin"),
            lookup_path: data_path.join("lookup.bin"),
            postings_path: data_path.join("postings.bin"),
            data_path,
        }
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn activate_generation(root: &Path, generation: &str) -> Result<()> {
    ensure_dir(root)?;
    let current_path = IndexLayout::current_path(root);
    let temp_path = root.join(format!("{CURRENT_FILE_NAME}.tmp"));
    fs::write(&temp_path, format!("{generation}\n"))?;
    #[cfg(windows)]
    {
        if current_path.exists() {
            fs::remove_file(&current_path)?;
        }
    }
    fs::rename(temp_path, current_path)?;
    Ok(())
}

pub fn write_docs_file(path: &Path, metadata: IndexMetadata, docs: &[DocMeta]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(DOCS_MAGIC)?;
    write_u32(&mut file, DOCS_FORMAT_VERSION)?;
    file.write_all(&[metadata.tokenizer.to_byte()])?;
    write_u32(
        &mut file,
        usize_to_u32(metadata.min_sparse_len, "min_sparse_len")?,
    )?;
    write_u32(
        &mut file,
        usize_to_u32(metadata.max_sparse_len, "max_sparse_len")?,
    )?;
    let build = metadata.build.ok_or_else(|| {
        AppError::InvalidIndex("missing build metadata when writing docs file".into())
    })?;
    file.write_all(&[build.corpus_mode.to_byte()])?;
    file.write_all(&[u8::from(build.include_hidden)])?;
    write_u64(&mut file, build.max_file_size)?;
    let repo_root_bytes = build.repo_root.as_bytes();
    write_u32(
        &mut file,
        usize_to_u32(repo_root_bytes.len(), "repo root length")?,
    )?;
    file.write_all(repo_root_bytes)?;
    write_u32(&mut file, usize_to_u32(docs.len(), "document count")?)?;

    for doc in docs {
        write_u32(&mut file, doc.doc_id)?;
        write_u64(&mut file, doc.size)?;
        write_u64(&mut file, doc.mtime_nanos)?;
        let path_bytes = doc.path.as_bytes();
        write_u32(&mut file, usize_to_u32(path_bytes.len(), "path length")?)?;
        file.write_all(path_bytes)?;
    }

    Ok(())
}

pub fn read_docs_file(path: &Path) -> Result<(IndexMetadata, Vec<DocMeta>)> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let mut cursor = Cursor::new(&bytes);

    cursor.expect_magic(DOCS_MAGIC)?;
    let version = cursor.read_u32()?;
    if version != DOCS_FORMAT_VERSION && version != DOCS_FORMAT_VERSION_LEGACY {
        return Err(AppError::InvalidIndex(format!(
            "unsupported docs version: {version}"
        )));
    }

    let tokenizer = TokenizerMode::from_byte(cursor.read_byte()?)
        .ok_or_else(|| AppError::InvalidIndex("unknown tokenizer mode".into()))?;
    let min_sparse_len = u32_to_usize(cursor.read_u32()?, "min_sparse_len")?;
    let max_sparse_len = u32_to_usize(cursor.read_u32()?, "max_sparse_len")?;
    let build = if version == DOCS_FORMAT_VERSION {
        let corpus_mode = CorpusMode::from_byte(cursor.read_byte()?)
            .ok_or_else(|| AppError::InvalidIndex("unknown corpus mode".into()))?;
        let include_hidden = cursor.read_byte()? != 0;
        let max_file_size = cursor.read_u64()?;
        let repo_root_len = u32_to_usize(cursor.read_u32()?, "repo root length")?;
        let repo_root = cursor.read_string(repo_root_len)?;
        Some(IndexBuildSettings {
            repo_root,
            corpus_mode,
            include_hidden,
            max_file_size,
        })
    } else {
        None
    };
    let doc_count = u32_to_usize(cursor.read_u32()?, "document count")?;
    let mut docs = Vec::with_capacity(doc_count);

    for _ in 0..doc_count {
        let doc_id = cursor.read_u32()?;
        let size = cursor.read_u64()?;
        let mtime_nanos = cursor.read_u64()?;
        let path_len = u32_to_usize(cursor.read_u32()?, "path length")?;
        let path = cursor.read_string(path_len)?;

        docs.push(DocMeta {
            doc_id,
            path,
            size,
            mtime_nanos,
        });
    }

    Ok((
        IndexMetadata {
            tokenizer,
            min_sparse_len,
            max_sparse_len,
            build,
        },
        docs,
    ))
}

pub fn write_doc_terms_file(path: &Path, doc_terms: &[Vec<u64>]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(DOC_TERMS_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    write_u32(&mut file, usize_to_u32(doc_terms.len(), "doc term count")?)?;

    for token_hashes in doc_terms {
        write_u32(
            &mut file,
            usize_to_u32(token_hashes.len(), "document token hash count")?,
        )?;
        for &token_hash in token_hashes {
            write_u64(&mut file, token_hash)?;
        }
    }

    Ok(())
}

pub fn read_doc_terms_file(path: &Path) -> Result<Vec<Vec<u64>>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let mut cursor = Cursor::new(&bytes);

    cursor.expect_magic(DOC_TERMS_MAGIC)?;
    let version = cursor.read_u32()?;
    if version != FORMAT_VERSION {
        return Err(AppError::InvalidIndex(format!(
            "unsupported doc terms version: {version}"
        )));
    }

    let doc_count = u32_to_usize(cursor.read_u32()?, "doc term count")?;
    let mut doc_terms = Vec::with_capacity(doc_count);

    for _ in 0..doc_count {
        let token_count = u32_to_usize(cursor.read_u32()?, "document token hash count")?;
        let mut token_hashes = Vec::with_capacity(token_count);
        for _ in 0..token_count {
            token_hashes.push(cursor.read_u64()?);
        }
        doc_terms.push(token_hashes);
    }

    if cursor.position != bytes.len() {
        return Err(AppError::InvalidIndex(
            "doc terms file has trailing bytes".into(),
        ));
    }

    Ok(doc_terms)
}

pub fn write_postings_file(
    path: &Path,
    postings: &[(u64, Vec<u32>)],
    high_freq_doc_threshold: Option<usize>,
) -> Result<Vec<LookupEntry>> {
    let mut file = File::create(path)?;
    file.write_all(POSTINGS_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    write_u32(&mut file, usize_to_u32(postings.len(), "posting count")?)?;

    let mut lookup_entries = Vec::with_capacity(postings.len());
    for (token_hash, doc_ids) in postings {
        let doc_freq = usize_to_u32(doc_ids.len(), "posting doc frequency")?;
        let skipped_high_freq =
            high_freq_doc_threshold.is_some_and(|threshold| doc_ids.len() > threshold);
        let offset = file.stream_position()?;
        let encoded = encode_posting_list(doc_ids);
        file.write_all(&encoded)?;
        let flags = if skipped_high_freq {
            LOOKUP_FLAG_SKIPPED_HIGH_FREQ
        } else {
            0
        };
        lookup_entries.push(LookupEntry {
            token_hash: *token_hash,
            offset,
            byte_len: usize_to_u32(encoded.len(), "posting byte length")?,
            doc_freq,
            flags,
        });
    }

    Ok(lookup_entries)
}

pub fn write_lookup_file(path: &Path, entries: &[LookupEntry]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(LOOKUP_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    write_u64(
        &mut file,
        usize_to_u64(entries.len(), "lookup entry count")?,
    )?;

    for entry in entries {
        write_u64(&mut file, entry.token_hash)?;
        write_u64(&mut file, entry.offset)?;
        write_u32(&mut file, entry.byte_len)?;
        write_u32(&mut file, entry.doc_freq)?;
        write_u32(&mut file, entry.flags)?;
    }

    Ok(())
}

pub struct LookupTable {
    mmap: Mmap,
    entries_len: usize,
}

impl LookupTable {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < 16 {
            return Err(AppError::InvalidIndex("lookup file too small".into()));
        }
        if &mmap[..4] != LOOKUP_MAGIC {
            return Err(AppError::InvalidIndex("lookup magic mismatch".into()));
        }

        let version = read_u32_le(&mmap[4..8]);
        if version != FORMAT_VERSION {
            return Err(AppError::InvalidIndex(format!(
                "unsupported lookup version: {version}"
            )));
        }

        let entries_len = u64_to_usize(read_u64_le(&mmap[8..16]), "lookup entry count")?;
        let expected_len = 16 + entries_len * LOOKUP_ENTRY_SIZE;
        if mmap.len() < expected_len {
            return Err(AppError::InvalidIndex("lookup file truncated".into()));
        }

        Ok(Self { mmap, entries_len })
    }

    pub fn find(&self, token_hash: u64) -> Option<LookupEntry> {
        let mut left = 0usize;
        let mut right = self.entries_len;

        while left < right {
            let mid = left + (right - left) / 2;
            let entry = self.entry_at(mid);
            if entry.token_hash == token_hash {
                return Some(entry);
            }
            if entry.token_hash < token_hash {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        None
    }

    fn entry_at(&self, index: usize) -> LookupEntry {
        let start = 16 + index * LOOKUP_ENTRY_SIZE;
        let token_hash = read_u64_le(&self.mmap[start..start + 8]);
        let offset = read_u64_le(&self.mmap[start + 8..start + 16]);
        let byte_len = read_u32_le(&self.mmap[start + 16..start + 20]);
        let doc_freq = read_u32_le(&self.mmap[start + 20..start + 24]);
        let flags = read_u32_le(&self.mmap[start + 24..start + 28]);

        LookupEntry {
            token_hash,
            offset,
            byte_len,
            doc_freq,
            flags,
        }
    }
}

pub struct PostingsData {
    mmap: Mmap,
}

impl PostingsData {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < 12 {
            return Err(AppError::InvalidIndex("postings file too small".into()));
        }
        if &mmap[..4] != POSTINGS_MAGIC {
            return Err(AppError::InvalidIndex("postings magic mismatch".into()));
        }

        let version = read_u32_le(&mmap[4..8]);
        if version != FORMAT_VERSION {
            return Err(AppError::InvalidIndex(format!(
                "unsupported postings version: {version}"
            )));
        }

        Ok(Self { mmap })
    }

    pub fn decode(&self, entry: LookupEntry) -> Result<Vec<u32>> {
        let start = u64_to_usize(entry.offset, "posting offset")?;
        let end = start
            .checked_add(u32_to_usize(entry.byte_len, "posting byte length")?)
            .ok_or_else(|| AppError::InvalidIndex("posting length overflow".into()))?;
        if end > self.mmap.len() {
            return Err(AppError::InvalidIndex("posting slice out of range".into()));
        }
        decode_posting_list(
            &self.mmap[start..end],
            u32_to_usize(entry.doc_freq, "posting doc frequency")?,
        )
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn expect_magic(&mut self, magic: &[u8; 4]) -> Result<()> {
        let value = self.read_exact(4)?;
        if value != magic {
            return Err(AppError::InvalidIndex("magic mismatch".into()));
        }
        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(read_u32_le(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(read_u64_le(bytes))
    }

    fn read_string(&mut self, len: usize) -> Result<String> {
        let bytes = self.read_exact(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|source| AppError::InvalidUtf8 {
            path: PathBuf::from("<index>"),
            source,
        })
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.position + len > self.bytes.len() {
            return Err(AppError::TruncatedData);
        }
        let slice = &self.bytes[self.position..self.position + len];
        self.position += len;
        Ok(slice)
    }
}

fn write_u32(writer: &mut File, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(writer: &mut File, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn usize_to_u32(value: usize, context: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| AppError::ValueOutOfRange(format!("{context} exceeds u32 range")))
}

fn usize_to_u64(value: usize, context: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| AppError::ValueOutOfRange(format!("{context} exceeds u64 range")))
}

fn u32_to_usize(value: u32, context: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| AppError::ValueOutOfRange(format!("{context} exceeds usize range")))
}

fn u64_to_usize(value: u64, context: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| AppError::ValueOutOfRange(format!("{context} exceeds usize range")))
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes);
    u32::from_le_bytes(raw)
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

fn encode_posting_list(doc_ids: &[u32]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(doc_ids.len() * 2);
    let mut previous = 0u32;
    for &doc_id in doc_ids {
        let delta = doc_id.wrapping_sub(previous);
        write_varint(&mut encoded, delta);
        previous = doc_id;
    }
    encoded
}

fn decode_posting_list(bytes: &[u8], doc_freq: usize) -> Result<Vec<u32>> {
    let mut docs = Vec::with_capacity(doc_freq);
    let mut position = 0usize;
    let mut previous = 0u32;

    while position < bytes.len() {
        let delta = read_varint(bytes, &mut position)?;
        previous = previous
            .checked_add(delta)
            .ok_or_else(|| AppError::InvalidIndex("posting delta overflow".into()))?;
        docs.push(previous);
    }

    if docs.len() != doc_freq {
        return Err(AppError::InvalidIndex(format!(
            "posting doc freq mismatch: expected {doc_freq}, got {}",
            docs.len()
        )));
    }

    Ok(docs)
}

fn write_varint(buffer: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        buffer.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
    buffer.push(value as u8);
}

fn read_varint(bytes: &[u8], position: &mut usize) -> Result<u32> {
    let mut value = 0u32;
    let mut shift = 0u32;

    while *position < bytes.len() {
        let byte = bytes[*position];
        *position += 1;
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 32 {
            return Err(AppError::InvalidIndex("posting varint overflow".into()));
        }
    }

    Err(AppError::TruncatedData)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        activate_generation, decode_posting_list, encode_posting_list, read_doc_terms_file,
        write_doc_terms_file, IndexLayout,
    };

    #[test]
    fn postings_roundtrip_delta_varint() {
        let docs = vec![1, 7, 8, 15, 1_000, 65_535];
        let encoded = encode_posting_list(&docs);
        let decoded = decode_posting_list(&encoded, docs.len()).expect("test should succeed");
        assert_eq!(decoded, docs);
    }

    #[test]
    fn doc_terms_roundtrip() {
        let temp = tempdir().expect("test should succeed");
        let path = temp.path().join("doc_terms.bin");
        let doc_terms = vec![vec![1, 3, 5], vec![], vec![8, 13]];

        write_doc_terms_file(&path, &doc_terms).expect("test should succeed");
        let decoded = read_doc_terms_file(&path).expect("test should succeed");

        assert_eq!(decoded, doc_terms);
    }

    #[test]
    fn resolve_prefers_active_generation_when_current_exists() {
        let temp = tempdir().expect("test should succeed");
        let root = temp.path().join("index");
        let generation = "gen-test";
        let generation_layout = IndexLayout::for_generation(&root, generation);
        fs::create_dir_all(&generation_layout.data_path).expect("test should succeed");
        activate_generation(&root, generation).expect("test should succeed");

        let resolved = IndexLayout::resolve(&root).expect("test should succeed");
        assert_eq!(resolved.data_path, generation_layout.data_path);
    }
}
