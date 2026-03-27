use std::{
    collections::HashMap,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    files::{read_text_file, scan_repository, RepositoryFile},
    index::format::{
        activate_generation, ensure_dir, read_doc_terms_file, read_docs_file, write_doc_terms_file,
        write_docs_file, write_lookup_file, write_postings_file, DocMeta, IndexBuildSettings,
        IndexLayout, IndexMetadata,
    },
    tokenizer::{create, TokenizerOptions},
};

pub fn build_index(config: &BuildConfig) -> Result<usize> {
    let files = scan_repository(config)?;
    ensure_dir(&config.index_path)?;
    let cached_docs = load_cached_docs(config);
    let tokenizer_options = TokenizerOptions {
        min_sparse_len: config.min_sparse_len,
        max_sparse_len: config.max_sparse_len,
    };
    let mut indexed_files =
        process_files_in_parallel(&files, config.tokenizer, tokenizer_options, &cached_docs)?;
    indexed_files.sort_unstable_by_key(|doc| doc.ordinal);

    let mut doc_entries = Vec::with_capacity(indexed_files.len());
    let mut doc_terms = Vec::with_capacity(indexed_files.len());
    let mut inverted = HashMap::<u64, Vec<u32>>::new();
    for (doc_id, file) in indexed_files.into_iter().enumerate() {
        let doc_id = usize_to_u32(doc_id, "indexed document id")?;
        for &token_hash in &file.token_hashes {
            inverted.entry(token_hash).or_default().push(doc_id);
        }
        doc_entries.push(DocMeta {
            doc_id,
            path: file.path,
            size: file.size,
            mtime_nanos: file.mtime_nanos,
        });
        doc_terms.push(file.token_hashes);
    }

    let mut postings = inverted.into_iter().collect::<Vec<_>>();
    for (_, docs) in &mut postings {
        docs.sort_unstable();
        docs.dedup();
    }
    postings.sort_unstable_by_key(|(token_hash, _)| *token_hash);
    let high_freq_doc_threshold = (config.tokenizer == crate::config::TokenizerMode::SparseNgram)
        .then(|| compute_high_freq_doc_threshold(doc_entries.len()));
    let generation = next_generation_name();
    let layout = IndexLayout::for_generation(&config.index_path, &generation);
    ensure_dir(&IndexLayout::generations_dir(&config.index_path))?;
    ensure_dir(&layout.data_path)?;

    write_docs_file(
        &layout.docs_path,
        IndexMetadata {
            tokenizer: config.tokenizer,
            min_sparse_len: config.min_sparse_len,
            max_sparse_len: config.max_sparse_len,
            build: Some(IndexBuildSettings {
                repo_root: std::fs::canonicalize(&config.repo_path)
                    .unwrap_or_else(|_| config.repo_path.clone())
                    .to_string_lossy()
                    .into_owned(),
                corpus_mode: config.corpus_mode,
                include_hidden: config.include_hidden,
                max_file_size: config.max_file_size,
            }),
        },
        &doc_entries,
    )?;
    write_doc_terms_file(&layout.doc_terms_path, &doc_terms)?;
    let lookup_entries =
        write_postings_file(&layout.postings_path, &postings, high_freq_doc_threshold)?;
    write_lookup_file(&layout.lookup_path, &lookup_entries)?;
    activate_generation(&config.index_path, &generation)?;

    Ok(doc_entries.len())
}

pub fn rebuild_index(config: &BuildConfig) -> Result<usize> {
    build_index(config)
}

fn compute_high_freq_doc_threshold(doc_count: usize) -> usize {
    (doc_count / 20).max(256)
}

fn process_files_in_parallel(
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

fn load_cached_docs(config: &BuildConfig) -> HashMap<String, CachedDoc> {
    let Ok(layout) = IndexLayout::resolve(&config.index_path) else {
        return HashMap::new();
    };
    let Ok((metadata, docs)) = read_docs_file(&layout.docs_path) else {
        return HashMap::new();
    };
    if metadata.tokenizer != config.tokenizer
        || metadata.min_sparse_len != config.min_sparse_len
        || metadata.max_sparse_len != config.max_sparse_len
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
        .zip(doc_terms)
        .map(|(doc, token_hashes)| {
            (
                doc.path,
                CachedDoc {
                    size: doc.size,
                    mtime_nanos: doc.mtime_nanos,
                    token_hashes,
                },
            )
        })
        .collect()
}

fn next_generation_name() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("gen-{}-{}", now.as_secs(), now.subsec_nanos())
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
