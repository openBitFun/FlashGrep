use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
};

use memmap2::Mmap;

use crate::{
    config::TokenizerMode,
    index_engine::{
        config::PostingTokenizerConfig,
        segment::{advise_mmap_random, encoded_posting_len},
        writer::IndexDocument,
    },
    tokenizer::{create, TokenizerOptions},
    AppError, Result,
};

pub(crate) const TERM_DICT_MAGIC: &[u8; 8] = b"CGTERM01";
pub(crate) const TERM_DICT_VERSION: u32 = 5;
const POSTING_GRAM_LEN: usize = 3;
const TERM_COUNT_OFFSET: u64 = 12;
const TERM_DICT_BUCKET_COUNT: usize = 64;
const TERM_DICT_HEADER_LEN: usize = 16 + TERM_DICT_BUCKET_COUNT * 4;
const TERM_DICT_ENTRY_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermDictionaryStats {
    pub key_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermDictEntry {
    pub key: u64,
    pub postings_offset: u32,
}

#[derive(Debug)]
pub struct TermDictionary {
    mmap: Mmap,
    count: usize,
    bucket_ranges: [(usize, usize); TERM_DICT_BUCKET_COUNT],
}

pub(crate) fn build_posting_lists(
    documents: &[IndexDocument],
    tokenizer: &PostingTokenizerConfig,
) -> HashMap<u64, Vec<u32>> {
    let mut posting_lists = HashMap::<u64, Vec<u32>>::new();

    for (doc_id, document) in documents.iter().enumerate() {
        let mut unique_terms = collect_document_posting_terms(&document.source_bytes, tokenizer)
            .into_iter()
            .collect::<Vec<_>>();
        unique_terms.sort_unstable();
        unique_terms.dedup();
        for term in unique_terms {
            posting_lists.entry(term).or_default().push(doc_id as u32);
        }
    }

    posting_lists
}

pub(crate) fn collect_document_posting_terms(
    bytes: &[u8],
    tokenizer: &PostingTokenizerConfig,
) -> Vec<u64> {
    let text = String::from_utf8_lossy(bytes);
    collect_document_posting_terms_from_text(&text, tokenizer)
}

pub(crate) fn collect_document_posting_terms_from_text(
    text: &str,
    tokenizer: &PostingTokenizerConfig,
) -> Vec<u64> {
    match tokenizer.mode {
        TokenizerMode::Trigram => collect_posting_grams(text.as_bytes()),
        TokenizerMode::SparseNgram => collect_sparse_posting_terms_from_text(text, tokenizer, true),
    }
}

pub(crate) fn collect_query_posting_terms(
    literal: &str,
    tokenizer: &PostingTokenizerConfig,
) -> Vec<u64> {
    match tokenizer.mode {
        TokenizerMode::Trigram => extract_trigram_terms_from_text(&fold_tokenizer_text(literal)),
        TokenizerMode::SparseNgram => {
            collect_sparse_query_terms(&fold_tokenizer_text(literal), tokenizer)
        }
    }
}

pub(crate) fn write_term_dict_file(
    path: &Path,
    posting_lists: &HashMap<u64, Vec<u32>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    buffer.write_all(TERM_DICT_MAGIC)?;
    write_u32(&mut buffer, TERM_DICT_VERSION)?;
    let bucket_counts = posting_key_buckets(posting_lists)
        .iter()
        .map(Vec::len)
        .collect::<Vec<_>>();
    let key_count = bucket_counts.iter().sum::<usize>();
    write_u32(
        &mut buffer,
        u32::try_from(key_count).map_err(|_| {
            AppError::ValueOutOfRange("term dictionary key count exceeds u32 range".into())
        })?,
    )?;
    for bucket_count in bucket_counts {
        write_u32(
            &mut buffer,
            u32::try_from(bucket_count).map_err(|_| {
                AppError::ValueOutOfRange("term dictionary bucket size exceeds u32 range".into())
            })?,
        )?;
    }
    let mut postings_offset = 16u32;
    for key in ordered_posting_keys(posting_lists) {
        write_u64(&mut buffer, key)?;
        write_u32(&mut buffer, postings_offset)?;
        let doc_ids = posting_lists.get(&key).expect("posting key must exist");
        let posting_bytes = 4usize
            .checked_add(encoded_posting_len(doc_ids)?)
            .ok_or_else(|| AppError::ValueOutOfRange("posting size exceeds usize range".into()))?;
        let posting_bytes = u32::try_from(posting_bytes)
            .map_err(|_| AppError::ValueOutOfRange("posting size exceeds u32 range".into()))?;
        postings_offset = postings_offset
            .checked_add(posting_bytes)
            .ok_or_else(|| AppError::ValueOutOfRange("postings offset exceeds u32 range".into()))?;
    }
    fs::write(path, buffer)?;
    Ok(())
}

pub(crate) fn ordered_posting_keys(posting_lists: &HashMap<u64, Vec<u32>>) -> Vec<u64> {
    posting_key_buckets(posting_lists)
        .into_iter()
        .flatten()
        .collect()
}

pub(crate) struct TermDictStreamWriter {
    writer: BufWriter<File>,
    key_count: u32,
    bucket_counts: [u32; TERM_DICT_BUCKET_COUNT],
}

impl TermDictStreamWriter {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(TERM_DICT_MAGIC)?;
        write_u32_to(&mut writer, TERM_DICT_VERSION)?;
        write_u32_to(&mut writer, 0)?;
        for _ in 0..TERM_DICT_BUCKET_COUNT {
            write_u32_to(&mut writer, 0)?;
        }
        Ok(Self {
            writer,
            key_count: 0,
            bucket_counts: [0; TERM_DICT_BUCKET_COUNT],
        })
    }

    pub(crate) fn push_entry(&mut self, key: u64, postings_offset: u32) -> Result<()> {
        write_u64_to(&mut self.writer, key)?;
        write_u32_to(&mut self.writer, postings_offset)?;
        self.key_count = self.key_count.checked_add(1).ok_or_else(|| {
            AppError::ValueOutOfRange("term dictionary key count exceeds u32 range".into())
        })?;
        let bucket_id = term_dict_bucket_id(key);
        self.bucket_counts[bucket_id] =
            self.bucket_counts[bucket_id]
                .checked_add(1)
                .ok_or_else(|| {
                    AppError::ValueOutOfRange(
                        "term dictionary bucket size exceeds u32 range".into(),
                    )
                })?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(TERM_COUNT_OFFSET))?;
        write_u32_to(&mut self.writer, self.key_count)?;
        for bucket_count in self.bucket_counts {
            write_u32_to(&mut self.writer, bucket_count)?;
        }
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        Ok(())
    }
}

pub(crate) fn read_term_dict_file(path: &Path) -> Result<Vec<TermDictEntry>> {
    let bytes = fs::read(path)?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let (count, _) = verify_term_dict_cursor(&mut cursor, bytes.len())?;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        keys.push(TermDictEntry {
            key: read_u64(&mut cursor)?,
            postings_offset: read_u32(&mut cursor)?,
        });
    }
    Ok(keys)
}

impl TermDictionary {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        advise_mmap_random(&mmap);
        let (count, bucket_ranges) = verify_term_dict_bytes(&mmap)?;
        Ok(Self {
            mmap,
            count,
            bucket_ranges,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn postings_offset(&self, key: u64) -> Option<u32> {
        let (mut left, mut right) = self.bucket_ranges[term_dict_bucket_id(key)];
        while left < right {
            let mid = left + (right - left) / 2;
            let entry = self.entry(mid);
            match entry.key.cmp(&key) {
                std::cmp::Ordering::Less => left = mid + 1,
                std::cmp::Ordering::Greater => right = mid,
                std::cmp::Ordering::Equal => return Some(entry.postings_offset),
            }
        }
        None
    }

    fn entry(&self, index: usize) -> TermDictEntry {
        debug_assert!(index < self.count);
        let start = TERM_DICT_HEADER_LEN + index * TERM_DICT_ENTRY_LEN;
        let bytes = &self.mmap[start..start + TERM_DICT_ENTRY_LEN];
        let mut key = [0u8; 8];
        key.copy_from_slice(&bytes[..8]);
        let mut postings_offset = [0u8; 4];
        postings_offset.copy_from_slice(&bytes[8..12]);
        TermDictEntry {
            key: u64::from_le_bytes(key),
            postings_offset: u32::from_le_bytes(postings_offset),
        }
    }
}

pub(crate) fn collect_posting_grams(bytes: &[u8]) -> Vec<u64> {
    extract_trigram_terms_from_bytes(bytes)
}

fn collect_sparse_posting_terms_from_text(
    text: &str,
    tokenizer: &PostingTokenizerConfig,
    document_mode: bool,
) -> Vec<u64> {
    let folded = fold_tokenizer_text(text);
    let tokenizer = create(
        tokenizer.mode,
        TokenizerOptions {
            min_sparse_len: tokenizer.min_sparse_len,
            max_sparse_len: tokenizer.max_sparse_len,
        },
    );
    let mut hashes = Vec::new();
    if document_mode {
        tokenizer.collect_document_token_hashes(&folded, &mut hashes);
    } else {
        tokenizer.collect_query_token_hashes(&folded, &mut hashes);
    }
    hashes
}

fn collect_sparse_query_terms(literal: &str, tokenizer: &PostingTokenizerConfig) -> Vec<u64> {
    collect_sparse_posting_terms_from_text(literal, tokenizer, false)
}

fn fold_tokenizer_text(text: &str) -> String {
    if text.is_ascii() {
        text.to_ascii_lowercase()
    } else {
        text.to_lowercase()
    }
}

fn extract_trigram_terms_from_text(text: &str) -> Vec<u64> {
    extract_trigram_terms_from_bytes(text.as_bytes())
}

fn extract_trigram_terms_from_bytes(bytes: &[u8]) -> Vec<u64> {
    let folded = bytes
        .iter()
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if folded.len() < POSTING_GRAM_LEN {
        return Vec::new();
    }

    folded
        .windows(POSTING_GRAM_LEN)
        .map(hash_trigram_window)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        collect_document_posting_terms, collect_query_posting_terms, write_term_dict_file,
        TermDictionary,
    };
    use crate::{index_engine::config::PostingTokenizerConfig, TokenizerMode};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn sparse_tokenizer() -> PostingTokenizerConfig {
        PostingTokenizerConfig {
            mode: TokenizerMode::SparseNgram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    #[test]
    fn sparse_documents_emit_build_all_tokens_while_queries_use_covering_tokens() {
        let tokenizer = sparse_tokenizer();
        let document_terms = collect_document_posting_terms(b"getUserById", &tokenizer);
        let query_terms = collect_query_posting_terms("getUserById", &tokenizer);

        assert!(!document_terms.is_empty());
        assert!(!query_terms.is_empty());
        assert!(document_terms.len() > query_terms.len());
        for term in &query_terms {
            assert!(document_terms.contains(term));
        }
    }

    #[test]
    fn mmap_term_dictionary_binary_searches_offsets() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("term_dict.bin");
        let posting_lists = HashMap::from([
            (11u64, vec![1u32, 3u32]),
            (22u64, vec![2u32]),
            (33u64, vec![4u32, 5u32, 6u32]),
        ]);
        write_term_dict_file(&path, &posting_lists).expect("write term dict");

        let dict = TermDictionary::open(&path).expect("open term dict");
        assert_eq!(dict.len(), 3);
        assert_eq!(dict.postings_offset(11), Some(16));
        assert_eq!(dict.postings_offset(22), Some(23));
        assert_eq!(dict.postings_offset(33), Some(29));
        assert_eq!(dict.postings_offset(44), None);
    }
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) -> Result<()> {
    buffer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(buffer: &mut Vec<u8>, value: u64) -> Result<()> {
    buffer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u32_to(writer: &mut impl Write, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64_to(writer: &mut impl Write, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
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

fn verify_term_dict_bytes(
    bytes: &[u8],
) -> Result<(usize, [(usize, usize); TERM_DICT_BUCKET_COUNT])> {
    let mut cursor = Cursor::new(bytes);
    verify_term_dict_cursor(&mut cursor, bytes.len())
}

fn verify_term_dict_cursor(
    cursor: &mut Cursor<&[u8]>,
    byte_len: usize,
) -> Result<(usize, [(usize, usize); TERM_DICT_BUCKET_COUNT])> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != TERM_DICT_MAGIC {
        return Err(AppError::InvalidIndex("term dict magic mismatch".into()));
    }
    let version = read_u32(cursor)?;
    if version != TERM_DICT_VERSION {
        return Err(AppError::InvalidIndex(format!(
            "unsupported term dict version: {version}"
        )));
    }
    let count = read_u32(cursor)? as usize;
    let mut bucket_ranges = [(0usize, 0usize); TERM_DICT_BUCKET_COUNT];
    let mut cursor_index = 0usize;
    for range in &mut bucket_ranges {
        let bucket_count = read_u32(cursor)? as usize;
        *range = (cursor_index, cursor_index + bucket_count);
        cursor_index += bucket_count;
    }
    if cursor_index != count {
        return Err(AppError::InvalidIndex(format!(
            "term dict bucket count mismatch: expected {count}, got {cursor_index}"
        )));
    }
    let expected_len = TERM_DICT_HEADER_LEN
        .checked_add(count.checked_mul(TERM_DICT_ENTRY_LEN).ok_or_else(|| {
            AppError::ValueOutOfRange("term dict size exceeds usize range".into())
        })?)
        .ok_or_else(|| AppError::ValueOutOfRange("term dict size exceeds usize range".into()))?;
    if byte_len != expected_len {
        return Err(AppError::InvalidIndex(format!(
            "term dict size mismatch: expected {expected_len} bytes, got {byte_len}"
        )));
    }
    Ok((count, bucket_ranges))
}

fn term_dict_bucket_id(key: u64) -> usize {
    let mixed = key ^ (key >> 33) ^ (key >> 17);
    (mixed as usize) % TERM_DICT_BUCKET_COUNT
}

fn posting_key_buckets(posting_lists: &HashMap<u64, Vec<u32>>) -> Vec<Vec<u64>> {
    let mut buckets = vec![Vec::new(); TERM_DICT_BUCKET_COUNT];
    for key in posting_lists.keys().copied() {
        buckets[term_dict_bucket_id(key)].push(key);
    }
    for bucket in &mut buckets {
        bucket.sort_unstable();
    }
    buckets
}

fn hash_trigram_window(window: &[u8]) -> u64 {
    debug_assert_eq!(window.len(), POSTING_GRAM_LEN);
    let mut bytes = [0u8; 8];
    bytes[..POSTING_GRAM_LEN].copy_from_slice(window);
    u64::from_le_bytes(bytes)
}
