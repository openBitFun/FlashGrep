use super::*;

pub(super) struct PostingBuckets {
    pub(super) state: PostingBucketsState,
}

pub(super) enum PostingBucketsState {
    InMemory(InMemoryPostingBuckets),
    Spilled(SpilledPostingBuckets),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct PostingRecord {
    pub(super) term: u64,
    pub(super) doc_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PostingRunHead {
    record: PostingRecord,
    run_index: usize,
}

struct PendingInlineSource {
    record_offset: u64,
    relative_offset: u64,
    source_len: u32,
}

pub(super) struct InMemoryPostingBuckets {
    spill_root: PathBuf,
    memory_threshold_bytes: usize,
    approximate_bytes: usize,
    appends_since_pressure_check: usize,
    buckets: Vec<HashMap<u64, Vec<u32>>>,
}

pub(super) struct SpilledPostingBuckets {
    bucket_dir: PathBuf,
    bucket_paths: Vec<PathBuf>,
    writers: Vec<BufWriter<File>>,
}

pub(super) struct BaseSnapshotBulkSegmentWriter {
    segment_dir: PathBuf,
    segment_id: SegmentId,
    pub(super) tokenizer: PostingTokenizerConfig,
    path_map: BufWriter<File>,
    path_entry_offsets: Vec<u64>,
    path_map_len: u64,
    docs: BufWriter<File>,
    staged_source_path: PathBuf,
    staged_sources: BufWriter<File>,
    pending_inline_sources: Vec<PendingInlineSource>,
    staged_source_bytes: u64,
    posting_buckets: PostingBuckets,
    doc_count: u32,
    total_source_bytes: u64,
}

impl BaseSnapshotBulkSegmentWriter {
    pub(super) fn new(
        index_root: &Path,
        segment_id: SegmentId,
        tokenizer: PostingTokenizerConfig,
    ) -> Result<Self> {
        let segment_dir = index_root.join("segments").join(&segment_id.0);
        fs::create_dir_all(&segment_dir)?;

        let mut path_map = BufWriter::new(File::create(segment_dir.join(PATH_MAP_FILE))?);
        let mut docs = BufWriter::new(File::create(segment_dir.join(DOCS_FILE))?);
        let staged_source_path = segment_dir.join("docs.inline.tmp");
        let staged_sources = BufWriter::new(File::create(&staged_source_path)?);
        let posting_buckets = PostingBuckets::new(&segment_dir);

        write_segment_header(&mut path_map, PATH_MAP_MAGIC)?;
        write_segment_header_with_version(&mut docs, DOCS_MAGIC, DOCS_VERSION)?;

        Ok(Self {
            segment_dir,
            segment_id,
            tokenizer,
            path_map,
            path_entry_offsets: Vec::new(),
            path_map_len: COUNT_OFFSET + 4,
            docs,
            staged_source_path,
            staged_sources,
            pending_inline_sources: Vec::new(),
            staged_source_bytes: 0,
            posting_buckets,
            doc_count: 0,
            total_source_bytes: 0,
        })
    }

    pub(super) fn add_document(&mut self, prepared: PreparedBaseDocument) -> Result<()> {
        let doc_id = self.doc_count;
        let PreparedBaseDocument {
            document,
            posting_terms,
            source_kind,
        } = prepared;
        let record_offset = COUNT_OFFSET + 4 + u64::from(doc_id) * DOC_RECORD_LEN;

        self.path_entry_offsets.push(self.path_map_len);
        write_string_to(&mut self.path_map, &document.repo_id.0)?;
        write_string_to(&mut self.path_map, &document.logical_path)?;
        let repo_id_len = encoded_string_len(&document.repo_id.0)?;
        let logical_path_len = encoded_string_len(&document.logical_path)?;
        self.path_map_len = self
            .path_map_len
            .checked_add(repo_id_len)
            .and_then(|len| len.checked_add(logical_path_len))
            .ok_or_else(|| AppError::ValueOutOfRange("path map offset overflow".into()))?;

        if matches!(source_kind, SegmentDocSourceKind::Inline) {
            let encoded_len = encoded_source_bytes_len(&document.source_bytes)?;
            let source_len = u32::try_from(encoded_len).map_err(|_| {
                AppError::ValueOutOfRange("inline source length exceeds u32 range".into())
            })?;
            write_source_bytes(&mut self.staged_sources, &document.source_bytes)?;
            self.pending_inline_sources.push(PendingInlineSource {
                record_offset,
                relative_offset: self.staged_source_bytes,
                source_len,
            });
            self.staged_source_bytes = self
                .staged_source_bytes
                .checked_add(encoded_len)
                .ok_or_else(|| {
                    AppError::ValueOutOfRange("base snapshot source staging overflow".into())
                })?;
        }

        write_u32_to(&mut self.docs, doc_id)?;
        write_u64_to(&mut self.docs, 0)?;
        write_u32_to(&mut self.docs, 0)?;
        write_u64_to(&mut self.docs, document.size)?;
        write_u64_to(&mut self.docs, document.modified_at)?;
        self.docs.write_all(&document.content_hash)?;
        self.docs.write_all(&[source_kind as u8])?;
        self.docs.write_all(&[0u8; 7])?;

        self.posting_buckets.append_terms(doc_id, &posting_terms)?;

        self.total_source_bytes = self
            .total_source_bytes
            .saturating_add(document.source_bytes.len() as u64);
        self.doc_count = self.doc_count.checked_add(1).ok_or_else(|| {
            AppError::ValueOutOfRange("base snapshot doc count exceeds u32 range".into())
        })?;
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<Option<SegmentRef>> {
        patch_segment_count(&mut self.path_map, self.doc_count)?;
        patch_segment_count(&mut self.docs, self.doc_count)?;

        self.path_map.flush()?;
        self.staged_sources.flush()?;

        if self.doc_count == 0 {
            fs::remove_dir_all(&self.segment_dir)?;
            return Ok(None);
        }

        let source_data_start = COUNT_OFFSET + 4 + u64::from(self.doc_count) * DOC_RECORD_LEN;
        for inline in &self.pending_inline_sources {
            patch_doc_source_location(
                &mut self.docs,
                inline.record_offset,
                source_data_start
                    .checked_add(inline.relative_offset)
                    .ok_or_else(|| {
                        AppError::ValueOutOfRange("base snapshot source offset overflow".into())
                    })?,
                inline.source_len,
            )?;
        }
        self.docs.seek(SeekFrom::End(0))?;
        let mut staged_source_reader = BufReader::new(File::open(&self.staged_source_path)?);
        std::io::copy(&mut staged_source_reader, &mut self.docs)?;
        self.docs.flush()?;
        let _ = fs::remove_file(&self.staged_source_path);

        let segment_dir = self.segment_dir.clone();
        let segment_id = self.segment_id.clone();
        let doc_count = self.doc_count;
        let total_source_bytes = self.total_source_bytes;
        let meta = SegmentMeta {
            segment_id: segment_id.clone(),
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count,
            live_doc_count: doc_count,
            created_by_merge: false,
        };
        let posting_buckets = self.posting_buckets;
        posting_buckets.finish(
            &segment_dir.join("term_dict.bin"),
            &segment_dir.join("postings.bin"),
        )?;
        write_path_offsets_file(
            &segment_dir.join(PATH_OFFSETS_FILE),
            &self.path_entry_offsets,
        )?;
        write_segment_meta(&segment_dir.join(SEGMENT_META_FILE), &meta)?;

        Ok(Some(SegmentRef {
            id: segment_id,
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count,
            size_bytes: total_source_bytes,
        }))
    }
}

impl PostingBuckets {
    fn new(index_root: &Path) -> Self {
        Self {
            state: PostingBucketsState::InMemory(InMemoryPostingBuckets::new(
                index_root,
                base_snapshot_in_memory_posting_threshold_bytes(),
            )),
        }
    }

    #[cfg(test)]
    pub(super) fn with_memory_threshold(index_root: &Path, memory_threshold_bytes: usize) -> Self {
        Self {
            state: PostingBucketsState::InMemory(InMemoryPostingBuckets::new(
                index_root,
                memory_threshold_bytes,
            )),
        }
    }

    pub(super) fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) -> Result<()> {
        match &mut self.state {
            PostingBucketsState::InMemory(buckets) => {
                buckets.append_terms(doc_id, posting_terms);
                if buckets.should_spill() {
                    let mut spilled = SpilledPostingBuckets::new(&buckets.spill_root)?;
                    buckets.drain_into_spilled(&mut spilled)?;
                    self.state = PostingBucketsState::Spilled(spilled);
                }
                Ok(())
            }
            PostingBucketsState::Spilled(buckets) => buckets.append_terms(doc_id, posting_terms),
        }
    }

    pub(super) fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        match self.state {
            PostingBucketsState::InMemory(buckets) => buckets.finish(term_dict_path, postings_path),
            PostingBucketsState::Spilled(buckets) => buckets.finish(term_dict_path, postings_path),
        }
    }
}

impl InMemoryPostingBuckets {
    fn new(index_root: &Path, memory_threshold_bytes: usize) -> Self {
        let mut buckets = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        for _ in 0..BASE_SNAPSHOT_POSTING_BUCKET_COUNT {
            buckets.push(HashMap::new());
        }
        Self {
            spill_root: index_root.to_path_buf(),
            memory_threshold_bytes,
            approximate_bytes: 0,
            appends_since_pressure_check: 0,
            buckets,
        }
    }

    fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) {
        self.appends_since_pressure_check = self.appends_since_pressure_check.saturating_add(1);
        for term in posting_terms {
            let bucket_id = posting_bucket_id(*term);
            let bucket = &mut self.buckets[bucket_id];
            let entry = bucket.entry(*term).or_insert_with(|| {
                self.approximate_bytes = self
                    .approximate_bytes
                    .saturating_add(estimated_term_entry_bytes(term));
                Vec::new()
            });
            entry.push(doc_id);
            self.approximate_bytes = self
                .approximate_bytes
                .saturating_add(std::mem::size_of::<u32>());
        }
    }

    fn should_spill(&mut self) -> bool {
        if self.approximate_bytes >= self.memory_threshold_bytes {
            return true;
        }
        if self.appends_since_pressure_check < BASE_SNAPSHOT_MEMORY_PRESSURE_CHECK_DOCS {
            return false;
        }
        self.appends_since_pressure_check = 0;
        base_snapshot_memory_pressure_should_spill()
    }

    fn drain_into_spilled(&mut self, spilled: &mut SpilledPostingBuckets) -> Result<()> {
        for bucket in &mut self.buckets {
            for (term, doc_ids) in bucket.drain() {
                for doc_id in doc_ids {
                    spilled.append_term_doc_id(term, doc_id)?;
                }
            }
        }
        self.approximate_bytes = 0;
        Ok(())
    }

    fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        let mut term_dict_writer = TermDictStreamWriter::create(term_dict_path)?;
        let mut postings_writer = PostingsStreamWriter::create(postings_path)?;

        for bucket in self.buckets {
            let mut entries = bucket.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (key, doc_ids) in entries {
                let postings_offset = postings_writer.push_doc_ids(&doc_ids)?;
                term_dict_writer.push_entry(key, postings_offset)?;
            }
        }

        term_dict_writer.finish()?;
        postings_writer.finish()?;
        Ok(())
    }
}

impl SpilledPostingBuckets {
    fn new(index_root: &Path) -> Result<Self> {
        let bucket_dir = index_root.join("posting-buckets");
        fs::create_dir_all(&bucket_dir)?;
        let mut bucket_paths = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        let mut writers = Vec::with_capacity(BASE_SNAPSHOT_POSTING_BUCKET_COUNT);
        for bucket_id in 0..BASE_SNAPSHOT_POSTING_BUCKET_COUNT {
            let path = bucket_dir.join(format!("bucket-{bucket_id:03}.bin"));
            bucket_paths.push(path.clone());
            writers.push(BufWriter::new(File::create(path)?));
        }
        Ok(Self {
            bucket_dir,
            bucket_paths,
            writers,
        })
    }

    fn append_terms(&mut self, doc_id: u32, posting_terms: &[u64]) -> Result<()> {
        for term in posting_terms {
            self.append_term_doc_id(*term, doc_id)?;
        }
        Ok(())
    }

    fn append_term_doc_id(&mut self, term: u64, doc_id: u32) -> Result<()> {
        let bucket_id = posting_bucket_id(term);
        let writer = self
            .writers
            .get_mut(bucket_id)
            .ok_or_else(|| AppError::InvalidIndex(format!("missing posting bucket {bucket_id}")))?;
        write_spilled_posting_record(writer, PostingRecord { term, doc_id })?;
        Ok(())
    }

    fn finish(self, term_dict_path: &Path, postings_path: &Path) -> Result<()> {
        let SpilledPostingBuckets {
            bucket_dir,
            bucket_paths,
            mut writers,
        } = self;

        for writer in &mut writers {
            writer.flush()?;
        }
        drop(writers);

        let mut term_dict_writer = TermDictStreamWriter::create(term_dict_path)?;
        let mut postings_writer = PostingsStreamWriter::create(postings_path)?;
        let run_root = bucket_dir.join("runs");
        fs::create_dir_all(&run_root)?;

        for (bucket_id, bucket_path) in bucket_paths.iter().enumerate() {
            if fs::metadata(bucket_path).map_or(true, |metadata| metadata.len() == 0) {
                let _ = fs::remove_file(bucket_path);
                continue;
            }
            let run_paths = write_sorted_posting_runs(
                bucket_path,
                &run_root,
                bucket_id,
                base_snapshot_spilled_bucket_sort_max_records_per_run(),
            )?;
            fs::remove_file(bucket_path)?;
            merge_sorted_posting_runs(&run_paths, &mut term_dict_writer, &mut postings_writer)?;
        }

        term_dict_writer.finish()?;
        postings_writer.finish()?;
        let _ = fs::remove_dir_all(bucket_dir);
        Ok(())
    }
}

pub(super) fn estimated_term_entry_bytes(_term: &u64) -> usize {
    std::mem::size_of::<u64>()
        .saturating_add(std::mem::size_of::<Vec<u32>>())
        .saturating_add(std::mem::size_of::<usize>() * 2)
}

fn posting_bucket_id(term: u64) -> usize {
    let mixed = term ^ (term >> 33) ^ (term >> 17);
    (mixed as usize) % BASE_SNAPSHOT_POSTING_BUCKET_COUNT
}

fn read_spilled_posting_record(reader: &mut impl Read) -> Result<Option<(u64, u32)>> {
    let Some(term) = read_optional_u64_from(reader)? else {
        return Ok(None);
    };
    let doc_id = read_u32_from(reader)?;
    Ok(Some((term, doc_id)))
}

pub(super) fn write_spilled_posting_record(
    writer: &mut impl Write,
    record: PostingRecord,
) -> Result<()> {
    write_u64_to(writer, record.term)?;
    write_u32_to(writer, record.doc_id)?;
    Ok(())
}

fn read_posting_record(reader: &mut impl Read) -> Result<Option<PostingRecord>> {
    read_spilled_posting_record(reader)
        .map(|record| record.map(|(term, doc_id)| PostingRecord { term, doc_id }))
}

pub(super) fn write_sorted_posting_runs(
    bucket_path: &Path,
    run_root: &Path,
    bucket_id: usize,
    max_records_per_run: usize,
) -> Result<Vec<PathBuf>> {
    let mut reader = BufReader::new(File::open(bucket_path)?);
    let mut run_paths = Vec::new();
    let mut records = Vec::with_capacity(max_records_per_run.max(1));

    loop {
        records.clear();
        while records.len() < max_records_per_run.max(1) {
            let Some(record) = read_posting_record(&mut reader)? else {
                break;
            };
            records.push(record);
        }
        if records.is_empty() {
            break;
        }

        records.sort_unstable_by_key(|record| (record.term, record.doc_id));
        let run_path = run_root.join(format!(
            "bucket-{bucket_id:03}-run-{:06}.bin",
            run_paths.len()
        ));
        let mut writer = BufWriter::new(File::create(&run_path)?);
        for record in &records {
            write_spilled_posting_record(&mut writer, *record)?;
        }
        writer.flush()?;
        run_paths.push(run_path);
    }

    Ok(run_paths)
}

pub(super) fn merge_sorted_posting_runs(
    run_paths: &[PathBuf],
    term_dict_writer: &mut TermDictStreamWriter,
    postings_writer: &mut PostingsStreamWriter,
) -> Result<()> {
    if run_paths.is_empty() {
        return Ok(());
    }

    let mut readers = run_paths
        .iter()
        .map(|path| Ok(Some(BufReader::new(File::open(path)?))))
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::<Reverse<PostingRunHead>>::new();
    for (run_index, reader_slot) in readers.iter_mut().enumerate() {
        let Some(reader) = reader_slot.as_mut() else {
            continue;
        };
        if let Some(record) = read_posting_record(reader)? {
            heap.push(Reverse(PostingRunHead { record, run_index }));
        } else {
            *reader_slot = None;
            fs::remove_file(&run_paths[run_index])?;
        }
    }

    let mut current_term = None;
    let mut current_doc_ids = Vec::new();

    while let Some(Reverse(head)) = heap.pop() {
        let term = head.record.term;
        if current_term != Some(term) {
            flush_streamed_posting_term(
                current_term,
                &current_doc_ids,
                term_dict_writer,
                postings_writer,
            )?;
            current_term = Some(term);
            current_doc_ids.clear();
        }
        if current_doc_ids.last().copied() != Some(head.record.doc_id) {
            current_doc_ids.push(head.record.doc_id);
        }

        let run_index = head.run_index;
        let Some(reader) = readers.get_mut(run_index).and_then(Option::as_mut) else {
            return Err(AppError::InvalidIndex(format!(
                "missing posting run reader for index {run_index}"
            )));
        };
        if let Some(record) = read_posting_record(reader)? {
            heap.push(Reverse(PostingRunHead { record, run_index }));
        } else {
            readers[run_index] = None;
            fs::remove_file(&run_paths[run_index])?;
        }
    }

    flush_streamed_posting_term(
        current_term,
        &current_doc_ids,
        term_dict_writer,
        postings_writer,
    )?;
    Ok(())
}

fn flush_streamed_posting_term(
    term: Option<u64>,
    doc_ids: &[u32],
    term_dict_writer: &mut TermDictStreamWriter,
    postings_writer: &mut PostingsStreamWriter,
) -> Result<()> {
    let Some(term) = term else {
        return Ok(());
    };
    if doc_ids.is_empty() {
        return Ok(());
    }
    let postings_offset = postings_writer.push_doc_ids(doc_ids)?;
    term_dict_writer.push_entry(term, postings_offset)?;
    Ok(())
}

fn write_segment_header(writer: &mut BufWriter<File>, magic: &[u8; 8]) -> Result<()> {
    write_segment_header_with_version(writer, magic, SEGMENT_FILE_VERSION)
}

fn write_segment_header_with_version(
    writer: &mut BufWriter<File>,
    magic: &[u8; 8],
    version: u32,
) -> Result<()> {
    writer.write_all(magic)?;
    write_u32_to(writer, version)?;
    write_u32_to(writer, 0)?;
    Ok(())
}

fn patch_segment_count(writer: &mut BufWriter<File>, count: u32) -> Result<()> {
    writer.flush()?;
    writer.seek(SeekFrom::Start(COUNT_OFFSET))?;
    write_u32_to(writer, count)?;
    writer.seek(SeekFrom::End(0))?;
    Ok(())
}

fn patch_doc_source_location(
    writer: &mut BufWriter<File>,
    record_offset: u64,
    source_offset: u64,
    source_len: u32,
) -> Result<()> {
    writer.flush()?;
    writer.seek(SeekFrom::Start(record_offset + 4))?;
    write_u64_to(writer, source_offset)?;
    write_u32_to(writer, source_len)?;
    writer.seek(SeekFrom::End(0))?;
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

fn write_string_to(writer: &mut impl Write, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("string length exceeds u32 range".into()))?;
    write_u32_to(writer, len)?;
    writer.write_all(bytes)?;
    Ok(())
}

fn encoded_string_len(value: &str) -> Result<u64> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("string length exceeds u32 range".into()))?;
    Ok(u64::from(len) + 4)
}

fn write_bytes_to(writer: &mut impl Write, value: &[u8]) -> Result<()> {
    let len = u32::try_from(value.len())
        .map_err(|_| AppError::ValueOutOfRange("byte slice length exceeds u32 range".into()))?;
    write_u32_to(writer, len)?;
    writer.write_all(value)?;
    Ok(())
}

fn read_optional_u32_from(reader: &mut impl Read) -> Result<Option<u32>> {
    let mut bytes = [0u8; 4];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(u32::from_le_bytes(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_u64_from(reader: &mut impl Read) -> Result<Option<u64>> {
    let mut bytes = [0u8; 8];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(Some(u64::from_le_bytes(bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_u32_from(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}
