use std::{collections::HashMap, thread};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    files::{read_text_file, scan_repository, RepositoryFile},
    index::format::{
        ensure_dir, write_docs_file, write_lookup_file, write_postings_file, DocMeta, IndexLayout,
        IndexMetadata,
    },
    tokenizer::{create, TokenizerOptions},
};

pub fn build_index(config: &BuildConfig) -> Result<usize> {
    ensure_dir(&config.index_path)?;
    let files = scan_repository(config)?;
    let tokenizer_options = TokenizerOptions {
        min_sparse_len: config.min_sparse_len,
        max_sparse_len: config.max_sparse_len,
    };
    let worker_outputs = process_files_in_parallel(&files, config.tokenizer, tokenizer_options)?;

    let mut indexed_files = worker_outputs
        .iter()
        .flat_map(|output| output.docs.iter().cloned())
        .collect::<Vec<_>>();
    indexed_files.sort_unstable_by_key(|doc| doc.ordinal);

    let mut ordinal_to_doc_id = vec![None; files.len()];
    let mut doc_entries = Vec::with_capacity(indexed_files.len());
    for (doc_id, file) in indexed_files.into_iter().enumerate() {
        let doc_id = usize_to_u32(doc_id, "indexed document id")?;
        ordinal_to_doc_id[file.ordinal] = Some(doc_id);
        doc_entries.push(DocMeta {
            doc_id,
            path: file.path,
            size: file.size,
            mtime_secs: file.mtime_secs,
        });
    }

    let mut inverted = HashMap::<u64, Vec<u32>>::new();
    for output in worker_outputs {
        for (token_hash, ordinals) in output.postings {
            let docs = inverted.entry(token_hash).or_default();
            docs.extend(ordinals.into_iter().filter_map(|ordinal| {
                usize::try_from(ordinal)
                    .ok()
                    .and_then(|ordinal| ordinal_to_doc_id.get(ordinal))
                    .copied()
                    .flatten()
            }));
        }
    }

    let mut postings = inverted.into_iter().collect::<Vec<_>>();
    for (_, docs) in &mut postings {
        docs.sort_unstable();
        docs.dedup();
    }
    postings.sort_unstable_by_key(|(token_hash, _)| *token_hash);
    let high_freq_doc_threshold = (config.tokenizer == crate::config::TokenizerMode::SparseNgram)
        .then(|| compute_high_freq_doc_threshold(doc_entries.len()));

    let layout = IndexLayout::new(&config.index_path);
    write_docs_file(
        &layout.docs_path,
        IndexMetadata {
            tokenizer: config.tokenizer,
            min_sparse_len: config.min_sparse_len,
            max_sparse_len: config.max_sparse_len,
        },
        &doc_entries,
    )?;
    let lookup_entries =
        write_postings_file(&layout.postings_path, &postings, high_freq_doc_threshold)?;
    write_lookup_file(&layout.lookup_path, &lookup_entries)?;

    Ok(doc_entries.len())
}

fn compute_high_freq_doc_threshold(doc_count: usize) -> usize {
    (doc_count / 20).max(256)
}

fn process_files_in_parallel(
    files: &[RepositoryFile],
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
) -> Result<Vec<WorkerOutput>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let worker_count = thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(files.len());
    let chunk_size = files.len().div_ceil(worker_count);

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in files.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let options = tokenizer_options.clone();
            handles.push(scope.spawn(move || process_chunk(chunk, tokenizer_mode, options)));
        }

        let mut outputs = Vec::with_capacity(handles.len());
        for handle in handles {
            let output = handle
                .join()
                .map_err(|_| AppError::InvalidIndex("index worker panicked".into()))??;
            outputs.push(output);
        }
        Ok(outputs)
    })
}

fn process_chunk(
    files: Vec<RepositoryFile>,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
) -> Result<WorkerOutput> {
    let tokenizer = create(tokenizer_mode, tokenizer_options);
    let mut docs = Vec::with_capacity(files.len());
    let mut postings = HashMap::<u64, Vec<u32>>::new();

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
            mtime_secs: file.mtime_secs,
        });

        for token_hash in token_hashes {
            postings
                .entry(token_hash)
                .or_default()
                .push(usize_to_u32(file.ordinal, "indexed file ordinal")?);
        }
    }

    Ok(WorkerOutput { docs, postings })
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

#[derive(Debug, Clone)]
struct IndexedFile {
    ordinal: usize,
    path: String,
    size: u64,
    mtime_secs: u64,
}

struct WorkerOutput {
    docs: Vec<IndexedFile>,
    postings: HashMap<u64, Vec<u32>>,
}
