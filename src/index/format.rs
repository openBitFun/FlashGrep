use std::{
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

use memmap2::Mmap;

use crate::{
    config::TokenizerMode,
    error::{AppError, Result},
};

const DOCS_MAGIC: &[u8; 4] = b"BFGD";
const LOOKUP_MAGIC: &[u8; 4] = b"BFGL";
const POSTINGS_MAGIC: &[u8; 4] = b"BFGP";
const FORMAT_VERSION: u32 = 3;
const LOOKUP_ENTRY_SIZE: usize = 24;
pub const LOOKUP_FLAG_SKIPPED_HIGH_FREQ: u32 = 1;

#[derive(Debug, Clone)]
pub struct DocMeta {
    pub doc_id: u32,
    pub path: String,
    pub size: u64,
    pub mtime_secs: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexMetadata {
    pub tokenizer: TokenizerMode,
    pub min_sparse_len: usize,
    pub max_sparse_len: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct LookupEntry {
    pub token_hash: u64,
    pub offset: u64,
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
    pub docs_path: PathBuf,
    pub lookup_path: PathBuf,
    pub postings_path: PathBuf,
}

impl IndexLayout {
    pub fn new(root: &Path) -> Self {
        Self {
            docs_path: root.join("docs.bin"),
            lookup_path: root.join("lookup.bin"),
            postings_path: root.join("postings.bin"),
        }
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn write_docs_file(path: &Path, metadata: IndexMetadata, docs: &[DocMeta]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(DOCS_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    file.write_all(&[metadata.tokenizer.to_byte()])?;
    write_u32(&mut file, metadata.min_sparse_len as u32)?;
    write_u32(&mut file, metadata.max_sparse_len as u32)?;
    write_u32(&mut file, docs.len() as u32)?;

    for doc in docs {
        write_u32(&mut file, doc.doc_id)?;
        write_u64(&mut file, doc.size)?;
        write_u64(&mut file, doc.mtime_secs)?;
        let path_bytes = doc.path.as_bytes();
        write_u32(&mut file, path_bytes.len() as u32)?;
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
    if version != FORMAT_VERSION {
        return Err(AppError::InvalidIndex(format!(
            "unsupported docs version: {version}"
        )));
    }

    let tokenizer = TokenizerMode::from_byte(cursor.read_byte()?)
        .ok_or_else(|| AppError::InvalidIndex("unknown tokenizer mode".into()))?;
    let min_sparse_len = cursor.read_u32()? as usize;
    let max_sparse_len = cursor.read_u32()? as usize;
    let doc_count = cursor.read_u32()? as usize;
    let mut docs = Vec::with_capacity(doc_count);

    for _ in 0..doc_count {
        let doc_id = cursor.read_u32()?;
        let size = cursor.read_u64()?;
        let mtime_secs = cursor.read_u64()?;
        let path_len = cursor.read_u32()? as usize;
        let path = cursor.read_string(path_len)?;

        docs.push(DocMeta {
            doc_id,
            path,
            size,
            mtime_secs,
        });
    }

    Ok((
        IndexMetadata {
            tokenizer,
            min_sparse_len,
            max_sparse_len,
        },
        docs,
    ))
}

pub fn write_postings_file(
    path: &Path,
    postings: &[(u64, Vec<u32>)],
    high_freq_doc_threshold: Option<usize>,
) -> Result<Vec<LookupEntry>> {
    let mut file = File::create(path)?;
    file.write_all(POSTINGS_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    write_u32(&mut file, postings.len() as u32)?;

    let mut lookup_entries = Vec::with_capacity(postings.len());
    for (token_hash, doc_ids) in postings {
        let doc_freq = doc_ids.len() as u32;
        let skipped_high_freq = high_freq_doc_threshold
            .map(|threshold| doc_ids.len() > threshold)
            .unwrap_or(false);
        let offset = file.stream_position()?;
        write_u32(&mut file, doc_freq)?;
        for doc_id in doc_ids {
            write_u32(&mut file, *doc_id)?;
        }
        let flags = if skipped_high_freq {
            LOOKUP_FLAG_SKIPPED_HIGH_FREQ
        } else {
            0
        };
        lookup_entries.push(LookupEntry {
            token_hash: *token_hash,
            offset,
            doc_freq,
            flags,
        });
    }

    Ok(lookup_entries)
}

pub fn read_posting_list(path: &Path, offset: u64) -> Result<Vec<u32>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if offset >= file_len {
        return Err(AppError::InvalidIndex(format!(
            "posting offset out of range: {offset}"
        )));
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;

    let mut count_bytes = [0u8; 4];
    file.read_exact(&mut count_bytes)?;
    let count = u32::from_le_bytes(count_bytes) as usize;
    let mut docs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut doc_bytes = [0u8; 4];
        file.read_exact(&mut doc_bytes)?;
        docs.push(u32::from_le_bytes(doc_bytes));
    }
    Ok(docs)
}

pub fn write_lookup_file(path: &Path, entries: &[LookupEntry]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(LOOKUP_MAGIC)?;
    write_u32(&mut file, FORMAT_VERSION)?;
    write_u64(&mut file, entries.len() as u64)?;

    for entry in entries {
        write_u64(&mut file, entry.token_hash)?;
        write_u64(&mut file, entry.offset)?;
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

        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(AppError::InvalidIndex(format!(
                "unsupported lookup version: {version}"
            )));
        }

        let entries_len = u64::from_le_bytes(mmap[8..16].try_into().unwrap()) as usize;
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
        let token_hash = u64::from_le_bytes(self.mmap[start..start + 8].try_into().unwrap());
        let offset = u64::from_le_bytes(self.mmap[start + 8..start + 16].try_into().unwrap());
        let doc_freq = u32::from_le_bytes(self.mmap[start + 16..start + 20].try_into().unwrap());
        let flags = u32::from_le_bytes(self.mmap[start + 20..start + 24].try_into().unwrap());

        LookupEntry {
            token_hash,
            offset,
            doc_freq,
            flags,
        }
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
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
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
