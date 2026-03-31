use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::Hasher;

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    files::{read_text_file, repo_relative_path, scan_repository, RepositoryFile},
    index::format::{
        activate_generation, ensure_dir, read_doc_terms_file, read_docs_file, write_lookup_file,
        DocMeta, DocTermsWriter, DocsWriter, IndexBuildSettings, IndexLayout, IndexMetadata,
        LookupEntry, PostingsWriter,
    },
    tokenizer::{create, TokenizerOptions},
};

const INDEX_BUILD_BATCH_SIZE: usize = 512;
const INDEX_BUILD_BUCKET_COUNT: usize = 256;
const BUCKET_RECORD_BYTES: usize = 12;

pub fn build_index(config: &BuildConfig) -> Result<usize> {
    let config = config.normalized()?;
    let snapshot = resolve_base_snapshot(&config)?;
    ensure_dir(&config.index_path)?;
    ensure_dir(&IndexLayout::generations_dir(&config.index_path))?;

    let layout = IndexLayout::for_generation(&config.index_path, &snapshot.generation);
    if snapshot.git_worktree_dirty {
        if layout.docs_path.exists() {
            activate_generation(&config.index_path, &snapshot.generation)?;
            return existing_doc_count(&layout);
        }
    }

    let tokenizer_options = TokenizerOptions {
        min_sparse_len: config.min_sparse_len,
        max_sparse_len: config.max_sparse_len,
    };
    ensure_dir(&layout.data_path)?;
    cleanup_build_artifacts(&layout)?;

    let mut builder = StreamingIndexBuilder::new(&config, &snapshot, &layout)?;
    let cached_docs = load_cached_docs(&config, &snapshot);

    if snapshot.git_worktree_dirty {
        if let Some(head_commit) = snapshot.head_commit.as_deref() {
            build_dirty_head_index(
                &config,
                &layout,
                &mut builder,
                head_commit,
                tokenizer_options,
            )?;
        } else {
            let files = scan_repository(&config)?;
            build_clean_index(
                &files,
                config.tokenizer,
                tokenizer_options,
                &cached_docs,
                &mut builder,
            )?;
        }
    } else {
        let files = scan_repository(&config)?;
        build_clean_index(
            &files,
            config.tokenizer,
            tokenizer_options,
            &cached_docs,
            &mut builder,
        )?;
    }

    let doc_count = builder.finish(config.tokenizer, &layout)?;
    activate_generation(&config.index_path, &snapshot.generation)?;

    Ok(doc_count)
}

pub fn rebuild_index(config: &BuildConfig) -> Result<usize> {
    build_index(config)
}

fn compute_high_freq_doc_threshold(doc_count: usize) -> usize {
    (doc_count / 20).max(256)
}

fn build_clean_index(
    files: &[RepositoryFile],
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
    cached_docs: &HashMap<String, CachedDoc>,
    builder: &mut StreamingIndexBuilder,
) -> Result<()> {
    for batch in files.chunks(INDEX_BUILD_BATCH_SIZE) {
        let mut indexed_files = process_file_batch_in_parallel(
            batch,
            tokenizer_mode,
            tokenizer_options.clone(),
            cached_docs,
        )?;
        indexed_files.sort_unstable_by_key(|doc| doc.ordinal);
        builder.write_batch(indexed_files)?;
    }
    Ok(())
}

fn build_dirty_head_index(
    config: &BuildConfig,
    layout: &IndexLayout,
    builder: &mut StreamingIndexBuilder,
    head_commit: &str,
    tokenizer_options: TokenizerOptions,
) -> Result<()> {
    let checkout = materialize_head_checkout(&config.repo_path, head_commit)?;
    let dirty_paths = dirty_tracked_paths(&config.repo_path, &config.index_path, head_commit)?;
    let files = scan_repository(&BuildConfig {
        repo_path: checkout.root.clone(),
        index_path: layout.root_path.join(".head-snapshot-index"),
        tokenizer: config.tokenizer,
        corpus_mode: config.corpus_mode,
        include_hidden: config.include_hidden,
        max_file_size: config.max_file_size,
        min_sparse_len: config.min_sparse_len,
        max_sparse_len: config.max_sparse_len,
    })?;

    let context = HeadSnapshotContext {
        checkout_root: &checkout.root,
        repo_root: &config.repo_path,
        dirty_paths: &dirty_paths,
    };
    for batch in files.chunks(INDEX_BUILD_BATCH_SIZE) {
        let mut indexed_files = process_file_batch_in_parallel(
            batch,
            config.tokenizer,
            tokenizer_options.clone(),
            &HashMap::new(),
        )?;
        rewrite_head_snapshot_batch(&mut indexed_files, &context)?;
        indexed_files.sort_unstable_by_key(|doc| doc.ordinal);
        builder.write_batch(indexed_files)?;
    }
    Ok(())
}

fn process_file_batch_in_parallel(
    files: &[RepositoryFile],
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
    cached_docs: &HashMap<String, CachedDoc>,
) -> Result<Vec<IndexedFile>> {
    let mut indexed_files = Vec::with_capacity(files.len());
    let mut files_to_process = Vec::new();

    for file in files {
        let path = file.path.to_string_lossy().into_owned();
        if let Some(cached) = cached_docs.get(&path) {
            if cached.size == file.size && cached.mtime_nanos == file.mtime_nanos {
                indexed_files.push(IndexedFile {
                    ordinal: file.ordinal,
                    path,
                    size: file.size,
                    mtime_nanos: file.mtime_nanos,
                    token_hashes: cached.token_hashes.clone(),
                });
                continue;
            }
        }
        files_to_process.push(file.clone());
    }

    if files_to_process.is_empty() {
        return Ok(indexed_files);
    }

    let worker_count = thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(files_to_process.len());
    let chunk_size = files_to_process.len().div_ceil(worker_count);

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in files_to_process.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let options = tokenizer_options.clone();
            handles.push(scope.spawn(move || process_chunk(chunk, tokenizer_mode, options)));
        }

        let mut rebuilt = Vec::new();
        for handle in handles {
            let output = handle
                .join()
                .map_err(|_| AppError::InvalidIndex("index worker panicked".into()))??;
            rebuilt.extend(output);
        }
        indexed_files.extend(rebuilt);
        Ok(indexed_files)
    })
}

fn process_chunk(
    files: Vec<RepositoryFile>,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
) -> Result<Vec<IndexedFile>> {
    let tokenizer = create(tokenizer_mode, tokenizer_options);
    let mut docs = Vec::with_capacity(files.len());

    for file in files {
        let Some(text) = read_text_file(&file.path)? else {
            continue;
        };
        let folded = fold_for_index(text);
        let mut token_hashes = Vec::new();
        tokenizer.collect_document_token_hashes(&folded, &mut token_hashes);
        token_hashes.sort_unstable();
        token_hashes.dedup();

        docs.push(IndexedFile {
            ordinal: file.ordinal,
            path: file.path.to_string_lossy().into_owned(),
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            token_hashes,
        });
    }

    Ok(docs)
}

fn usize_to_u32(value: usize, context: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| AppError::ValueOutOfRange(format!("{context} exceeds u32 range")))
}

fn fold_for_index(mut text: String) -> String {
    if text.is_ascii() {
        text.make_ascii_lowercase();
        text
    } else {
        text.to_lowercase()
    }
}

struct HeadSnapshotContext<'a> {
    checkout_root: &'a Path,
    repo_root: &'a Path,
    dirty_paths: &'a HashSet<String>,
}

fn rewrite_head_snapshot_batch(
    indexed_files: &mut [IndexedFile],
    context: &HeadSnapshotContext<'_>,
) -> Result<()> {
    for file in indexed_files {
        let logical_path = repo_relative_path(Path::new(&file.path), context.checkout_root);
        let worktree_path = context.repo_root.join(&logical_path);
        if !context.dirty_paths.contains(&logical_path) {
            if let Some((size, mtime_nanos)) = worktree_file_identity(&worktree_path)? {
                file.size = size;
                file.mtime_nanos = mtime_nanos;
            }
        } else {
            file.mtime_nanos = 0;
        }
        file.path = worktree_path.to_string_lossy().into_owned();
    }
    Ok(())
}

fn cleanup_build_artifacts(layout: &IndexLayout) -> Result<()> {
    let spill_dir = spill_dir_path(layout);
    if spill_dir.exists() {
        fs::remove_dir_all(&spill_dir)?;
    }
    for path in [
        &layout.trigram_fallback_doc_terms_path,
        &layout.trigram_fallback_lookup_path,
        &layout.trigram_fallback_postings_path,
    ] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn spill_dir_path(layout: &IndexLayout) -> PathBuf {
    layout.data_path.join("postings-spill")
}

struct StreamingIndexBuilder {
    repo_path: PathBuf,
    docs_writer: DocsWriter,
    doc_terms_writer: DocTermsWriter,
    bucket_spiller: BucketSpiller,
    spill_dir: PathBuf,
    doc_count: usize,
}

impl StreamingIndexBuilder {
    fn new(config: &BuildConfig, snapshot: &BaseSnapshot, layout: &IndexLayout) -> Result<Self> {
        let docs_writer = DocsWriter::create(
            &layout.docs_path,
            IndexMetadata {
                tokenizer: config.tokenizer,
                min_sparse_len: config.min_sparse_len,
                max_sparse_len: config.max_sparse_len,
                fallback_trigram: None,
                build: Some(snapshot.build_settings(config)),
            },
        )?;
        let doc_terms_writer = DocTermsWriter::create(&layout.doc_terms_path)?;
        let spill_dir = spill_dir_path(layout);
        let bucket_spiller = BucketSpiller::create(&spill_dir)?;
        Ok(Self {
            repo_path: config.repo_path.clone(),
            docs_writer,
            doc_terms_writer,
            bucket_spiller,
            spill_dir,
            doc_count: 0,
        })
    }

    fn write_batch(&mut self, indexed_files: Vec<IndexedFile>) -> Result<()> {
        for file in indexed_files {
            let doc_id = usize_to_u32(self.doc_count, "indexed document id")?;
            self.docs_writer.write_doc(&DocMeta {
                doc_id,
                path: repo_relative_path(Path::new(&file.path), &self.repo_path),
                size: file.size,
                mtime_nanos: file.mtime_nanos,
            })?;
            self.doc_terms_writer.write_doc_terms(&file.token_hashes)?;
            self.bucket_spiller
                .write_doc_terms(&file.token_hashes, doc_id)?;
            self.doc_count += 1;
        }
        Ok(())
    }

    fn finish(
        mut self,
        tokenizer: crate::config::TokenizerMode,
        layout: &IndexLayout,
    ) -> Result<usize> {
        self.bucket_spiller.finish()?;
        let docs_count = self.docs_writer.finish()?;
        let doc_terms_count = self.doc_terms_writer.finish()?;
        if docs_count != self.doc_count || doc_terms_count != self.doc_count {
            return Err(AppError::InvalidIndex(format!(
                "streamed doc count mismatch: docs={docs_count}, doc_terms={doc_terms_count}, expected={}",
                self.doc_count
            )));
        }

        let high_freq_doc_threshold = (tokenizer == crate::config::TokenizerMode::SparseNgram)
            .then(|| compute_high_freq_doc_threshold(self.doc_count));
        let mut lookup_entries = merge_postings_from_bucket_files(
            &self.spill_dir,
            &layout.postings_path,
            high_freq_doc_threshold,
        )?;
        lookup_entries.sort_unstable_by_key(|entry| entry.token_hash);
        write_lookup_file(&layout.lookup_path, &lookup_entries)?;
        if self.spill_dir.exists() {
            fs::remove_dir_all(&self.spill_dir)?;
        }
        Ok(self.doc_count)
    }
}

struct BucketSpiller {
    writers: Vec<BufWriter<File>>,
}

impl BucketSpiller {
    fn create(dir: &Path) -> Result<Self> {
        debug_assert!(INDEX_BUILD_BUCKET_COUNT.is_power_of_two());
        ensure_dir(dir)?;
        let mut writers = Vec::with_capacity(INDEX_BUILD_BUCKET_COUNT);
        for bucket in 0..INDEX_BUILD_BUCKET_COUNT {
            let path = bucket_path(dir, bucket);
            let file = File::create(path)?;
            writers.push(BufWriter::new(file));
        }
        Ok(Self { writers })
    }

    fn write_doc_terms(&mut self, token_hashes: &[u64], doc_id: u32) -> Result<()> {
        for &token_hash in token_hashes {
            let bucket = bucket_for(token_hash);
            let writer = &mut self.writers[bucket];
            writer.write_all(&token_hash.to_le_bytes())?;
            writer.write_all(&doc_id.to_le_bytes())?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        for writer in &mut self.writers {
            writer.flush()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BucketRecord {
    token_hash: u64,
    doc_id: u32,
}

fn merge_postings_from_bucket_files(
    spill_dir: &Path,
    postings_path: &Path,
    high_freq_doc_threshold: Option<usize>,
) -> Result<Vec<LookupEntry>> {
    let mut postings_writer = PostingsWriter::create_streaming(postings_path)?;
    for bucket in 0..INDEX_BUILD_BUCKET_COUNT {
        let mut records = read_bucket_records(&bucket_path(spill_dir, bucket))?;
        records.sort_unstable_by_key(|record| (record.token_hash, record.doc_id));

        let mut index = 0usize;
        while index < records.len() {
            let token_hash = records[index].token_hash;
            let mut doc_ids = Vec::new();
            let mut last_doc_id = None;
            while index < records.len() && records[index].token_hash == token_hash {
                let doc_id = records[index].doc_id;
                if last_doc_id != Some(doc_id) {
                    doc_ids.push(doc_id);
                    last_doc_id = Some(doc_id);
                }
                index += 1;
            }
            postings_writer.write_posting_list(token_hash, &doc_ids, high_freq_doc_threshold)?;
        }
    }
    postings_writer.finish()
}

fn read_bucket_records(path: &Path) -> Result<Vec<BucketRecord>> {
    let file = File::open(path)?;
    let byte_len = usize::try_from(file.metadata()?.len())
        .map_err(|_| AppError::ValueOutOfRange("bucket spill length exceeds usize range".into()))?;
    if byte_len % BUCKET_RECORD_BYTES != 0 {
        return Err(AppError::InvalidIndex(format!(
            "bucket spill file has invalid length: {}",
            path.display()
        )));
    }

    let mut reader = BufReader::new(file);
    let mut records = Vec::with_capacity(byte_len / BUCKET_RECORD_BYTES);
    let mut buffer = [0u8; BUCKET_RECORD_BYTES];
    loop {
        match reader.read_exact(&mut buffer) {
            Ok(()) => {
                let mut token_bytes = [0u8; 8];
                token_bytes.copy_from_slice(&buffer[..8]);
                let mut doc_bytes = [0u8; 4];
                doc_bytes.copy_from_slice(&buffer[8..]);
                records.push(BucketRecord {
                    token_hash: u64::from_le_bytes(token_bytes),
                    doc_id: u32::from_le_bytes(doc_bytes),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(records)
}

fn bucket_path(dir: &Path, bucket: usize) -> PathBuf {
    dir.join(format!("bucket-{bucket:03}.bin"))
}

fn bucket_for(token_hash: u64) -> usize {
    (token_hash as usize) & (INDEX_BUILD_BUCKET_COUNT - 1)
}

fn load_cached_docs(config: &BuildConfig, snapshot: &BaseSnapshot) -> HashMap<String, CachedDoc> {
    let layout = IndexLayout::for_generation(&config.index_path, &snapshot.generation);
    if !layout.docs_path.exists() {
        return HashMap::new();
    }
    let Ok((metadata, docs)) = read_docs_file(&layout.docs_path) else {
        return HashMap::new();
    };
    if metadata.tokenizer != config.tokenizer
        || metadata.min_sparse_len != config.min_sparse_len
        || metadata.max_sparse_len != config.max_sparse_len
        || !build_settings_match(
            metadata.build.as_ref(),
            config,
            snapshot.head_commit.as_deref(),
            &snapshot.config_fingerprint,
        )
    {
        return HashMap::new();
    }

    let Ok(doc_terms) = read_doc_terms_file(&layout.doc_terms_path) else {
        return HashMap::new();
    };
    if docs.len() != doc_terms.len() {
        return HashMap::new();
    }

    docs.into_iter()
        .enumerate()
        .map(|(index, doc)| {
            (
                doc.path,
                CachedDoc {
                    size: doc.size,
                    mtime_nanos: doc.mtime_nanos,
                    token_hashes: doc_terms[index].clone(),
                },
            )
        })
        .collect()
}

fn build_settings_match(
    build: Option<&IndexBuildSettings>,
    config: &BuildConfig,
    expected_head_commit: Option<&str>,
    expected_config_fingerprint: &str,
) -> bool {
    let Some(build) = build else {
        return false;
    };
    build.repo_root == canonical_repo_root(&config.repo_path)
        && build.corpus_mode == config.corpus_mode
        && build.include_hidden == config.include_hidden
        && build.max_file_size == config.max_file_size
        && build.head_commit.as_deref() == expected_head_commit
        && build.config_fingerprint.as_deref() == Some(expected_config_fingerprint)
}

fn existing_doc_count(layout: &IndexLayout) -> Result<usize> {
    let (_, docs) = read_docs_file(&layout.docs_path)?;
    Ok(docs.len())
}

fn resolve_base_snapshot(config: &BuildConfig) -> Result<BaseSnapshot> {
    let config_fingerprint = build_config_fingerprint(config);
    match resolve_git_head(&config.repo_path)? {
        Some(head_commit) => {
            let generation = format!("base-git-{head_commit}-{config_fingerprint}");
            let worktree_dirty = git_worktree_dirty(&config.repo_path, &config.index_path)?;
            Ok(BaseSnapshot {
                generation,
                head_commit: Some(head_commit),
                config_fingerprint,
                git_worktree_dirty: worktree_dirty,
            })
        }
        None => {
            let repo_fingerprint = repo_fingerprint(&config.repo_path);
            Ok(BaseSnapshot {
                generation: format!("base-repo-{repo_fingerprint}-{config_fingerprint}"),
                head_commit: None,
                config_fingerprint,
                git_worktree_dirty: false,
            })
        }
    }
}

pub(crate) fn resolve_git_head(repo_root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let head_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head_commit.is_empty() {
        return Ok(None);
    }
    Ok(Some(head_commit))
}

fn git_worktree_dirty(repo_root: &Path, index_path: &Path) -> Result<bool> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_root).args([
        "status",
        "--porcelain=v1",
        "--untracked-files=all",
        "--",
        ".",
    ]);
    if let Some(pathspec) = index_exclude_pathspec(repo_root, index_path) {
        command.arg(pathspec);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to inspect git worktree state for {}",
            repo_root.display()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Some(path) = line.get(3..) else {
            return Ok(true);
        };
        if line.starts_with("?? ") && is_ignored_untracked_artifact(path, repo_root, index_path) {
            continue;
        }
        return Ok(true);
    }

    Ok(false)
}

fn dirty_tracked_paths(
    repo_root: &Path,
    index_path: &Path,
    head_commit: &str,
) -> Result<HashSet<String>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "--name-only", "-z", head_commit, "--", "."]);
    if let Some(pathspec) = index_exclude_pathspec(repo_root, index_path) {
        command.arg(pathspec);
    }
    let output = command.output()?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to diff worktree against {head_commit} for {}",
            repo_root.display()
        )));
    }

    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn materialize_head_checkout(
    repo_root: &Path,
    head_commit: &str,
) -> Result<MaterializedHeadCheckout> {
    let checkout = MaterializedHeadCheckout::new()?;
    fs::create_dir_all(checkout.root.join(".git"))?;
    run_git_with_temp_index(
        repo_root,
        &checkout.index_path,
        ["read-tree", head_commit],
        &format!(
            "failed to load git tree {head_commit} from {} into a temporary index",
            repo_root.display()
        ),
    )?;
    run_git_with_temp_index(
        repo_root,
        &checkout.index_path,
        [
            "checkout-index",
            "--all",
            "--force",
            "--prefix",
            checkout.prefix_arg.as_str(),
        ],
        &format!(
            "failed to materialize git tree {head_commit} into {}",
            checkout.root.display()
        ),
    )?;
    Ok(checkout)
}

fn run_git_with_temp_index<const N: usize>(
    repo_root: &Path,
    index_path: &Path,
    args: [&str; N],
    error_message: &str,
) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .env("GIT_INDEX_FILE", index_path)
        .args(args)
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        return Err(AppError::InvalidIndex(error_message.to_string()));
    }
    Err(AppError::InvalidIndex(format!("{error_message}: {stderr}")))
}

fn worktree_file_identity(path: &Path) -> Result<Option<(u64, u64)>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| match u64::try_from(duration.as_nanos()) {
            Ok(value) => value,
            Err(_) => u64::MAX,
        })
        .unwrap_or_default();
    Ok(Some((metadata.len(), mtime_nanos)))
}

pub(crate) fn build_config_fingerprint(config: &BuildConfig) -> String {
    let mut hasher = Hasher::new();
    hasher.update(config.tokenizer.as_str().as_bytes());
    hasher.update(config.corpus_mode.as_str().as_bytes());
    hasher.update(&[u8::from(config.include_hidden)]);
    hasher.update(&config.max_file_size.to_le_bytes());
    hasher.update(&(config.min_sparse_len as u64).to_le_bytes());
    hasher.update(&(config.max_sparse_len as u64).to_le_bytes());
    format!("{:08x}", hasher.finalize())
}

fn repo_fingerprint(repo_root: &Path) -> String {
    let mut hasher = Hasher::new();
    hasher.update(canonical_repo_root(repo_root).as_bytes());
    format!("{:08x}", hasher.finalize())
}

pub(crate) fn canonical_repo_root(repo_root: &Path) -> String {
    std::fs::canonicalize(repo_root)
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn index_exclude_pathspec(repo_root: &Path, index_path: &Path) -> Option<String> {
    let relative = index_path.strip_prefix(repo_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let path = relative.to_string_lossy().replace('\\', "/");
    Some(format!(":(exclude){path}"))
}

fn is_ignored_untracked_artifact(path: &str, repo_root: &Path, index_path: &Path) -> bool {
    let normalized = path.replace('\\', "/");
    if let Some(relative_index_path) = index_path.strip_prefix(repo_root).ok() {
        let relative_index_path = relative_index_path.to_string_lossy().replace('\\', "/");
        if normalized == relative_index_path || normalized.starts_with(&format!("{relative_index_path}/")) {
            return true;
        }
    }

    normalized
        .split('/')
        .any(|component| component == ".bitfun-index" || component == ".bitfun-bench")
}

struct BaseSnapshot {
    generation: String,
    head_commit: Option<String>,
    config_fingerprint: String,
    git_worktree_dirty: bool,
}

impl BaseSnapshot {
    fn build_settings(&self, config: &BuildConfig) -> IndexBuildSettings {
        IndexBuildSettings {
            repo_root: canonical_repo_root(&config.repo_path),
            corpus_mode: config.corpus_mode,
            include_hidden: config.include_hidden,
            max_file_size: config.max_file_size,
            head_commit: self.head_commit.clone(),
            config_fingerprint: Some(self.config_fingerprint.clone()),
        }
    }
}

struct MaterializedHeadCheckout {
    root: PathBuf,
    index_path: PathBuf,
    prefix_arg: String,
}

impl MaterializedHeadCheckout {
    fn new() -> Result<Self> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "bitfun-grep-head-{}-{}-{}",
            std::process::id(),
            now.as_secs(),
            now.subsec_nanos()
        ));
        fs::create_dir_all(&root)?;
        let index_path = root.join("head.index");
        let prefix_arg = format!("{}/", root.to_string_lossy());
        Ok(Self {
            root,
            index_path,
            prefix_arg,
        })
    }
}

impl Drop for MaterializedHeadCheckout {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Clone)]
struct IndexedFile {
    ordinal: usize,
    path: String,
    size: u64,
    mtime_nanos: u64,
    token_hashes: Vec<u64>,
}

struct CachedDoc {
    size: u64,
    mtime_nanos: u64,
    token_hashes: Vec<u64>,
}
