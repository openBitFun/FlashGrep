use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, RwLock},
};

use memmap2::Mmap;

use crate::index_engine::segment::{
    advise_mmap_random, ordered_posting_keys, TermDictEntry, TermDictionary,
};
use crate::{AppError, Result};

#[derive(Debug)]
pub struct PostingLists {
    storage: PostingListStorage,
}

#[derive(Debug)]
enum PostingListStorage {
    Eager(HashMap<u64, Arc<[u32]>>),
    Lazy(FileBackedPostingLists),
}

#[derive(Debug)]
struct FileBackedPostingLists {
    postings_mmap: Mmap,
    term_dict: TermDictionary,
    version: u32,
    cache: RwLock<HashMap<u64, Arc<[u32]>>>,
}

pub(crate) const POSTINGS_MAGIC: &[u8; 8] = b"CGPSTG01";
pub(crate) const POSTINGS_VERSION_V1: u32 = 1;
pub(crate) const POSTINGS_VERSION_V2: u32 = 2;
pub(crate) const POSTINGS_VERSION_V3: u32 = 3;
const POSTING_COUNT_OFFSET: u64 = 12;
const POSTING_ENCODING_RAW: u8 = 0;
const POSTING_ENCODING_DELTA_VARINT: u8 = 1;
const RAW_POSTING_DOC_COUNT_THRESHOLD: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingBlock {
    pub doc_ids: Vec<u32>,
}

impl Default for PostingLists {
    fn default() -> Self {
        Self {
            storage: PostingListStorage::Eager(HashMap::new()),
        }
    }
}

impl PostingLists {
    pub fn from_eager(posting_lists: HashMap<u64, Vec<u32>>) -> Self {
        Self {
            storage: PostingListStorage::Eager(
                posting_lists
                    .into_iter()
                    .map(|(key, doc_ids)| (key, Arc::<[u32]>::from(doc_ids)))
                    .collect(),
            ),
        }
    }

    pub fn open_lazy(term_dict: TermDictionary, path: &Path) -> Result<Self> {
        let version = verify_postings_header(path, term_dict.len())?;
        let postings_file = File::open(path)?;
        let postings_mmap = unsafe { Mmap::map(&postings_file)? };
        advise_mmap_random(&postings_mmap);
        Ok(Self {
            storage: PostingListStorage::Lazy(FileBackedPostingLists {
                postings_mmap,
                term_dict,
                version,
                cache: RwLock::new(HashMap::new()),
            }),
        })
    }

    pub fn get(&self, key: &u64) -> Result<Option<Arc<[u32]>>> {
        match &self.storage {
            PostingListStorage::Eager(posting_lists) => Ok(posting_lists.get(key).cloned()),
            PostingListStorage::Lazy(posting_lists) => posting_lists.get(key),
        }
    }
}

impl FileBackedPostingLists {
    fn get(&self, key: &u64) -> Result<Option<Arc<[u32]>>> {
        {
            let cache = self
                .cache
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(doc_ids) = cache.get(key) {
                return Ok(Some(doc_ids.clone()));
            }
        }

        let Some(offset) = self.term_dict.postings_offset(*key) else {
            return Ok(None);
        };

        let doc_ids = Arc::<[u32]>::from(self.load_doc_ids(offset)?);
        let mut cache = self
            .cache
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = cache.entry(*key).or_insert_with(|| doc_ids.clone());
        Ok(Some(entry.clone()))
    }

    fn load_doc_ids(&self, offset: u32) -> Result<Vec<u32>> {
        let start = offset as usize;
        let doc_count_bytes = self
            .postings_mmap
            .get(start..start + 4)
            .ok_or_else(|| AppError::InvalidIndex("posting count header is out of range".into()))?;
        let mut raw_doc_count = [0u8; 4];
        raw_doc_count.copy_from_slice(doc_count_bytes);
        let doc_count = u32::from_le_bytes(raw_doc_count) as usize;
        let body_start = start
            .checked_add(4)
            .ok_or_else(|| AppError::InvalidIndex("posting slice overflow".into()))?;
        match self.version {
            POSTINGS_VERSION_V1 => {
                let body_end = body_start
                    .checked_add(doc_count.checked_mul(4).ok_or_else(|| {
                        AppError::ValueOutOfRange("posting doc count exceeds usize range".into())
                    })?)
                    .ok_or_else(|| AppError::InvalidIndex("posting slice overflow".into()))?;
                let doc_id_bytes =
                    self.postings_mmap
                        .get(body_start..body_end)
                        .ok_or_else(|| {
                            AppError::InvalidIndex("posting doc ids are out of range".into())
                        })?;
                let mut doc_ids = Vec::with_capacity(doc_count);
                for chunk in doc_id_bytes.chunks_exact(4) {
                    let mut raw_doc_id = [0u8; 4];
                    raw_doc_id.copy_from_slice(chunk);
                    doc_ids.push(u32::from_le_bytes(raw_doc_id));
                }
                Ok(doc_ids)
            }
            POSTINGS_VERSION_V2 => decode_posting_deltas(
                self.postings_mmap.get(body_start..).ok_or_else(|| {
                    AppError::InvalidIndex("posting doc ids are out of range".into())
                })?,
                doc_count,
            ),
            POSTINGS_VERSION_V3 => {
                let encoding = *self.postings_mmap.get(body_start).ok_or_else(|| {
                    AppError::InvalidIndex("posting encoding is out of range".into())
                })?;
                let payload = self.postings_mmap.get(body_start + 1..).ok_or_else(|| {
                    AppError::InvalidIndex("posting payload is out of range".into())
                })?;
                decode_posting_payload(payload, doc_count, encoding)
            }
            version => Err(AppError::InvalidIndex(format!(
                "unsupported postings version: {version}"
            ))),
        }
    }
}

pub(crate) fn write_postings_file(
    path: &Path,
    posting_lists: &HashMap<u64, Vec<u32>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    buffer.write_all(POSTINGS_MAGIC)?;
    write_u32(&mut buffer, POSTINGS_VERSION_V3)?;
    write_u32(&mut buffer, posting_lists.len() as u32)?;

    for key in ordered_posting_keys(posting_lists) {
        let doc_ids = posting_lists.get(&key).expect("posting key must exist");
        write_u32(&mut buffer, doc_ids.len() as u32)?;
        write_posting_payload(&mut buffer, doc_ids)?;
    }

    fs::write(path, buffer)?;
    Ok(())
}

pub(crate) struct PostingsStreamWriter {
    writer: BufWriter<File>,
    term_count: u32,
    next_offset: u32,
}

impl PostingsStreamWriter {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        let mut writer = BufWriter::new(File::create(path)?);
        writer.write_all(POSTINGS_MAGIC)?;
        write_u32_to(&mut writer, POSTINGS_VERSION_V3)?;
        write_u32_to(&mut writer, 0)?;
        Ok(Self {
            writer,
            term_count: 0,
            next_offset: 16,
        })
    }

    pub(crate) fn push_doc_ids(&mut self, doc_ids: &[u32]) -> Result<u32> {
        let postings_offset = self.next_offset;
        let doc_count = u32::try_from(doc_ids.len())
            .map_err(|_| AppError::ValueOutOfRange("posting doc count exceeds u32 range".into()))?;
        write_u32_to(&mut self.writer, doc_count)?;
        let posting_bytes = 4usize
            .checked_add(write_posting_payload_to(&mut self.writer, doc_ids)?)
            .ok_or_else(|| AppError::ValueOutOfRange("posting size exceeds usize range".into()))?;
        let posting_bytes = u32::try_from(posting_bytes)
            .map_err(|_| AppError::ValueOutOfRange("posting size exceeds u32 range".into()))?;
        self.next_offset = self
            .next_offset
            .checked_add(posting_bytes)
            .ok_or_else(|| AppError::ValueOutOfRange("postings offset exceeds u32 range".into()))?;
        self.term_count = self.term_count.checked_add(1).ok_or_else(|| {
            AppError::ValueOutOfRange("posting term count exceeds u32 range".into())
        })?;
        Ok(postings_offset)
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(POSTING_COUNT_OFFSET))?;
        write_u32_to(&mut self.writer, self.term_count)?;
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        Ok(())
    }
}

pub(crate) fn read_postings_file(path: &Path, entries: &[TermDictEntry]) -> Result<PostingLists> {
    let bytes = fs::read(path)?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let version = verify_postings_cursor(&mut cursor, entries.len())?;

    let mut posting_lists = HashMap::new();
    for entry in entries {
        let doc_count = read_u32(&mut cursor)? as usize;
        let doc_ids = match version {
            POSTINGS_VERSION_V1 => {
                let mut doc_ids = Vec::with_capacity(doc_count);
                for _ in 0..doc_count {
                    doc_ids.push(read_u32(&mut cursor)?);
                }
                doc_ids
            }
            POSTINGS_VERSION_V2 => read_posting_deltas(&mut cursor, doc_count)?,
            POSTINGS_VERSION_V3 => {
                let mut encoding = [0u8; 1];
                cursor.read_exact(&mut encoding)?;
                let payload = cursor
                    .get_ref()
                    .get(cursor.position() as usize..)
                    .ok_or_else(|| {
                        AppError::InvalidIndex("posting payload is out of range".into())
                    })?;
                let doc_ids = decode_posting_payload(payload, doc_count, encoding[0])?;
                let payload_len =
                    encoded_posting_payload_len_for_encoding(doc_ids.as_slice(), encoding[0])?;
                cursor.set_position(
                    cursor
                        .position()
                        .checked_add(payload_len as u64)
                        .ok_or_else(|| AppError::InvalidIndex("posting cursor overflow".into()))?,
                );
                doc_ids
            }
            other => {
                return Err(AppError::InvalidIndex(format!(
                    "unsupported postings version: {other}"
                )))
            }
        };
        posting_lists.insert(entry.key, doc_ids);
    }
    Ok(PostingLists::from_eager(posting_lists))
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) -> Result<()> {
    buffer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u32_to(writer: &mut impl Write, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn verify_postings_header(path: &Path, expected_count: usize) -> Result<u32> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header)?;
    let mut cursor = Cursor::new(header.as_slice());
    verify_postings_cursor(&mut cursor, expected_count)
}

fn verify_postings_cursor(cursor: &mut Cursor<&[u8]>, expected_count: usize) -> Result<u32> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != POSTINGS_MAGIC {
        return Err(AppError::InvalidIndex("postings magic mismatch".into()));
    }
    let version = read_u32(cursor)?;
    if !matches!(
        version,
        POSTINGS_VERSION_V1 | POSTINGS_VERSION_V2 | POSTINGS_VERSION_V3
    ) {
        return Err(AppError::InvalidIndex(format!(
            "unsupported postings version: {version}"
        )));
    }
    let count = read_u32(cursor)? as usize;
    if count != expected_count {
        return Err(AppError::InvalidIndex(format!(
            "posting term count mismatch: expected {}, got {}",
            expected_count, count
        )));
    }
    Ok(version)
}

fn write_posting_payload(buffer: &mut Vec<u8>, doc_ids: &[u32]) -> Result<()> {
    let _ = write_posting_payload_impl(buffer, doc_ids)?;
    Ok(())
}

pub(crate) fn encoded_posting_len(doc_ids: &[u32]) -> Result<usize> {
    let encoding = posting_encoding(doc_ids);
    let payload_len = encoded_posting_payload_len_for_encoding(doc_ids, encoding)?;
    1usize
        .checked_add(payload_len)
        .ok_or_else(|| AppError::ValueOutOfRange("posting size exceeds usize range".into()))
}

fn posting_encoding(doc_ids: &[u32]) -> u8 {
    if doc_ids.len() >= RAW_POSTING_DOC_COUNT_THRESHOLD {
        POSTING_ENCODING_RAW
    } else {
        POSTING_ENCODING_DELTA_VARINT
    }
}

fn encoded_posting_payload_len_for_encoding(doc_ids: &[u32], encoding: u8) -> Result<usize> {
    match encoding {
        POSTING_ENCODING_RAW => doc_ids
            .len()
            .checked_mul(4)
            .ok_or_else(|| AppError::ValueOutOfRange("posting size exceeds usize range".into())),
        POSTING_ENCODING_DELTA_VARINT => {
            let mut previous = 0u32;
            let mut len = 0usize;
            for (index, doc_id) in doc_ids.iter().copied().enumerate() {
                if index > 0 && doc_id <= previous {
                    return Err(AppError::InvalidIndex(
                        "posting doc ids must be strictly increasing".into(),
                    ));
                }
                let delta = if index == 0 {
                    doc_id
                } else {
                    doc_id - previous
                };
                len = len.checked_add(varint_u32_len(delta)).ok_or_else(|| {
                    AppError::ValueOutOfRange("posting size exceeds usize range".into())
                })?;
                previous = doc_id;
            }
            Ok(len)
        }
        value => Err(AppError::InvalidIndex(format!(
            "invalid posting encoding: {value}"
        ))),
    }
}

fn write_posting_payload_to(writer: &mut impl Write, doc_ids: &[u32]) -> Result<usize> {
    write_posting_payload_impl(writer, doc_ids)
}

fn write_posting_payload_impl(writer: &mut impl Write, doc_ids: &[u32]) -> Result<usize> {
    let encoding = posting_encoding(doc_ids);
    writer.write_all(&[encoding])?;
    let payload_len = match encoding {
        POSTING_ENCODING_RAW => write_raw_postings(writer, doc_ids)?,
        POSTING_ENCODING_DELTA_VARINT => write_posting_deltas_impl(writer, doc_ids)?,
        value => {
            return Err(AppError::InvalidIndex(format!(
                "invalid posting encoding: {value}"
            )))
        }
    };
    1usize
        .checked_add(payload_len)
        .ok_or_else(|| AppError::ValueOutOfRange("posting size exceeds usize range".into()))
}

fn write_raw_postings(writer: &mut impl Write, doc_ids: &[u32]) -> Result<usize> {
    let mut written = 0usize;
    for doc_id in doc_ids {
        write_u32_to(writer, *doc_id)?;
        written += 4;
    }
    Ok(written)
}

fn decode_posting_payload(bytes: &[u8], doc_count: usize, encoding: u8) -> Result<Vec<u32>> {
    match encoding {
        POSTING_ENCODING_RAW => decode_raw_postings(bytes, doc_count),
        POSTING_ENCODING_DELTA_VARINT => decode_posting_deltas(bytes, doc_count),
        value => Err(AppError::InvalidIndex(format!(
            "invalid posting encoding: {value}"
        ))),
    }
}

fn decode_raw_postings(bytes: &[u8], doc_count: usize) -> Result<Vec<u32>> {
    let body_end = doc_count
        .checked_mul(4)
        .ok_or_else(|| AppError::ValueOutOfRange("posting doc count exceeds usize range".into()))?;
    let doc_id_bytes = bytes
        .get(..body_end)
        .ok_or_else(|| AppError::InvalidIndex("posting doc ids are out of range".into()))?;
    let mut doc_ids = Vec::with_capacity(doc_count);
    for chunk in doc_id_bytes.chunks_exact(4) {
        let mut raw_doc_id = [0u8; 4];
        raw_doc_id.copy_from_slice(chunk);
        doc_ids.push(u32::from_le_bytes(raw_doc_id));
    }
    Ok(doc_ids)
}

fn write_posting_deltas_impl(writer: &mut impl Write, doc_ids: &[u32]) -> Result<usize> {
    let mut previous = 0u32;
    let mut written = 0usize;
    for (index, doc_id) in doc_ids.iter().copied().enumerate() {
        if index > 0 && doc_id <= previous {
            return Err(AppError::InvalidIndex(
                "posting doc ids must be strictly increasing".into(),
            ));
        }
        let delta = if index == 0 {
            doc_id
        } else {
            doc_id - previous
        };
        written += write_varint_u32(writer, delta)?;
        previous = doc_id;
    }
    Ok(written)
}

fn read_posting_deltas(cursor: &mut Cursor<&[u8]>, doc_count: usize) -> Result<Vec<u32>> {
    let remaining = cursor
        .get_ref()
        .get(cursor.position() as usize..)
        .ok_or_else(|| AppError::InvalidIndex("posting payload is out of range".into()))?;
    let (doc_ids, consumed) = decode_posting_deltas_with_len(remaining, doc_count)?;
    cursor.set_position(
        cursor
            .position()
            .checked_add(consumed as u64)
            .ok_or_else(|| AppError::InvalidIndex("posting cursor overflow".into()))?,
    );
    Ok(doc_ids)
}

fn decode_posting_deltas(bytes: &[u8], doc_count: usize) -> Result<Vec<u32>> {
    decode_posting_deltas_with_len(bytes, doc_count).map(|(doc_ids, _)| doc_ids)
}

fn decode_posting_deltas_with_len(bytes: &[u8], doc_count: usize) -> Result<(Vec<u32>, usize)> {
    let mut doc_ids = Vec::with_capacity(doc_count);
    let mut previous = 0u32;
    let mut offset = 0usize;

    for index in 0..doc_count {
        let (delta, len) =
            read_varint_u32_from_slice(bytes.get(offset..).ok_or_else(|| {
                AppError::InvalidIndex("posting payload is out of range".into())
            })?)?;
        offset = offset
            .checked_add(len)
            .ok_or_else(|| AppError::InvalidIndex("posting payload overflow".into()))?;
        let doc_id = if index == 0 {
            delta
        } else {
            previous
                .checked_add(delta)
                .ok_or_else(|| AppError::InvalidIndex("posting doc id delta overflow".into()))?
        };
        doc_ids.push(doc_id);
        previous = doc_id;
    }

    Ok((doc_ids, offset))
}

fn write_varint_u32(writer: &mut impl Write, mut value: u32) -> Result<usize> {
    let mut written = 0usize;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
        written += 1;
        if value == 0 {
            return Ok(written);
        }
    }
}

fn read_varint_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut value = 0u32;
    let mut shift = 0u32;
    loop {
        if shift >= 35 {
            return Err(AppError::InvalidIndex("posting varint is too large".into()));
        }
        let mut byte = [0u8; 1];
        cursor.read_exact(&mut byte)?;
        value |= u32::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

fn read_varint_u32_from_slice(bytes: &[u8]) -> Result<(u32, usize)> {
    let mut value = 0u32;
    let mut shift = 0u32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if shift >= 35 {
            return Err(AppError::InvalidIndex("posting varint is too large".into()));
        }
        value |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, index + 1));
        }
        shift += 7;
    }
    Err(AppError::InvalidIndex(
        "posting varint ended unexpectedly".into(),
    ))
}

fn varint_u32_len(mut value: u32) -> usize {
    let mut len = 1usize;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tempfile::TempDir;

    use super::{write_postings_file, PostingLists};
    use crate::index_engine::segment::{write_term_dict_file, TermDictionary};

    #[test]
    fn lazy_postings_follow_bucketed_term_dict_offsets() {
        let temp = TempDir::new().expect("temp dir");
        let term_dict_path = temp.path().join("term_dict.bin");
        let postings_path = temp.path().join("postings.bin");
        let posting_lists = HashMap::from([
            (1u64, vec![10u32]),
            (66u64, vec![20u32, 21u32]),
            (2u64, vec![30u32]),
        ]);

        write_term_dict_file(&term_dict_path, &posting_lists).expect("write term dict");
        write_postings_file(&postings_path, &posting_lists).expect("write postings");

        let dict = TermDictionary::open(&term_dict_path).expect("open term dict");
        let postings = PostingLists::open_lazy(dict, &postings_path).expect("open postings");

        assert_eq!(
            postings.get(&1).expect("posting lookup").as_deref(),
            Some([10u32].as_slice())
        );
        assert_eq!(
            postings.get(&66).expect("posting lookup").as_deref(),
            Some([20u32, 21u32].as_slice())
        );
        assert_eq!(
            postings.get(&2).expect("posting lookup").as_deref(),
            Some([30u32].as_slice())
        );
    }
}
