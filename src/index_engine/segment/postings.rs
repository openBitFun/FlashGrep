use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, RwLock},
};

use memmap2::Mmap;

use crate::index_engine::segment::{
    advise_mmap_sequential, ordered_posting_keys, TermDictEntry, TermDictionary,
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
    cache: RwLock<HashMap<u64, Arc<[u32]>>>,
}

pub(crate) const POSTINGS_MAGIC: &[u8; 8] = b"CGPSTG01";
pub(crate) const POSTINGS_VERSION: u32 = 1;
const POSTING_COUNT_OFFSET: u64 = 12;

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
        verify_postings_header(path, term_dict.len())?;
        let postings_file = File::open(path)?;
        let postings_mmap = unsafe { Mmap::map(&postings_file)? };
        advise_mmap_sequential(&postings_mmap);
        Ok(Self {
            storage: PostingListStorage::Lazy(FileBackedPostingLists {
                postings_mmap,
                term_dict,
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
        let body_end = body_start
            .checked_add(doc_count.checked_mul(4).ok_or_else(|| {
                AppError::ValueOutOfRange("posting doc count exceeds usize range".into())
            })?)
            .ok_or_else(|| AppError::InvalidIndex("posting slice overflow".into()))?;
        let doc_id_bytes = self
            .postings_mmap
            .get(body_start..body_end)
            .ok_or_else(|| AppError::InvalidIndex("posting doc ids are out of range".into()))?;
        let mut doc_ids = Vec::with_capacity(doc_count);
        for chunk in doc_id_bytes.chunks_exact(4) {
            let mut raw_doc_id = [0u8; 4];
            raw_doc_id.copy_from_slice(chunk);
            doc_ids.push(u32::from_le_bytes(raw_doc_id));
        }
        Ok(doc_ids)
    }
}

pub(crate) fn write_postings_file(
    path: &Path,
    posting_lists: &HashMap<u64, Vec<u32>>,
) -> Result<()> {
    let mut buffer = Vec::new();
    buffer.write_all(POSTINGS_MAGIC)?;
    write_u32(&mut buffer, POSTINGS_VERSION)?;
    write_u32(&mut buffer, posting_lists.len() as u32)?;

    for key in ordered_posting_keys(posting_lists) {
        let doc_ids = posting_lists.get(&key).expect("posting key must exist");
        write_u32(&mut buffer, doc_ids.len() as u32)?;
        for doc_id in doc_ids {
            write_u32(&mut buffer, *doc_id)?;
        }
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
        write_u32_to(&mut writer, POSTINGS_VERSION)?;
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
        for doc_id in doc_ids {
            write_u32_to(&mut self.writer, *doc_id)?;
        }
        let posting_bytes = 4usize
            .checked_add(doc_ids.len().checked_mul(4).ok_or_else(|| {
                AppError::ValueOutOfRange("posting size exceeds usize range".into())
            })?)
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
    verify_postings_cursor(&mut cursor, entries.len())?;

    let mut posting_lists = HashMap::new();
    for entry in entries {
        let doc_count = read_u32(&mut cursor)? as usize;
        let mut doc_ids = Vec::with_capacity(doc_count);
        for _ in 0..doc_count {
            doc_ids.push(read_u32(&mut cursor)?);
        }
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

fn verify_postings_header(path: &Path, expected_count: usize) -> Result<()> {
    let mut file = File::open(path)?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header)?;
    let mut cursor = Cursor::new(header.as_slice());
    verify_postings_cursor(&mut cursor, expected_count)
}

fn verify_postings_cursor(cursor: &mut Cursor<&[u8]>, expected_count: usize) -> Result<()> {
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != POSTINGS_MAGIC {
        return Err(AppError::InvalidIndex("postings magic mismatch".into()));
    }
    let version = read_u32(cursor)?;
    if version != POSTINGS_VERSION {
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
    Ok(())
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
