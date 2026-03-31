use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crc32fast::Hasher;

use crate::{
    config::{BuildConfig, CorpusMode},
    error::{AppError, Result},
    files::{
        read_text_file, repo_relative_path, resolve_repo_path, scan_paths, RepositoryFile,
        ScanOptions,
    },
    index::{
        builder::{build_config_fingerprint, canonical_repo_root, resolve_git_head},
        format::{
            ensure_dir, read_doc_terms_file, read_docs_file, write_doc_terms_file, write_docs_file,
            write_lookup_file, write_postings_file, DocMeta, IndexBuildSettings, IndexLayout,
            IndexMetadata, LookupEntry, LookupTable, PostingsData, PostingsWriter,
        },
        searcher::IndexWorktreeDiff,
    },
    tokenizer::{create, TokenizerOptions},
};

const OVERLAYS_DIR_NAME: &str = "overlays";
const CURRENT_FILE_NAME: &str = "CURRENT";
const GENERATIONS_DIR_NAME: &str = "generations";
const MANIFEST_FILE_NAME: &str = "manifest.txt";
const TOMBSTONES_FILE_NAME: &str = "tombstones.txt";
const LAST_ACCESSED_FILE_NAME: &str = "last_accessed";

/// Public identity and change counts for an overlay cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySnapshotInfo {
    /// Stable key of the base snapshot this overlay is layered on top of.
    pub base_snapshot_key: String,
    /// Stable key of this overlay cache for one dirty worktree state.
    pub overlay_key: String,
    /// Number of modified tracked files represented by the overlay.
    pub modified_files: usize,
    /// Number of deleted tracked files represented by the overlay.
    pub deleted_files: usize,
    /// Number of newly added files represented by the overlay.
    pub new_files: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct OverlaySnapshotState {
    pub(crate) info: OverlaySnapshotInfo,
    pub(crate) data_path: PathBuf,
    pub(crate) indexed_paths: HashSet<String>,
    pub(crate) indexed_files: HashMap<String, (u64, u64)>,
    pub(crate) shadowed_paths: HashSet<String>,
}

pub(crate) fn current_base_snapshot_key(index_root: &Path) -> Result<Option<String>> {
    let path = IndexLayout::current_path(index_root);
    if !path.exists() {
        return Ok(None);
    }

    let generation = fs::read_to_string(path)?;
    let generation = generation.trim();
    if generation.is_empty() {
        return Err(AppError::InvalidIndex(
            "CURRENT points to an empty base generation".into(),
        ));
    }

    Ok(Some(generation.to_string()))
}

pub(crate) fn write_overlay_snapshot(
    index_root: &Path,
    base_snapshot_key: &str,
    overlay_key: &str,
    repo_root: &Path,
    diff: &IndexWorktreeDiff,
) -> Result<OverlaySnapshotInfo> {
    let info = OverlaySnapshotInfo {
        base_snapshot_key: base_snapshot_key.to_string(),
        overlay_key: overlay_key.to_string(),
        modified_files: diff.modified_files.len(),
        deleted_files: diff.deleted_files.len(),
        new_files: diff.new_files.len(),
    };

    let generation_dir = overlay_generation_dir(index_root, base_snapshot_key, &info.overlay_key);
    fs::create_dir_all(&generation_dir)?;
    fs::write(
        generation_dir.join(MANIFEST_FILE_NAME),
        encode_manifest(&info),
    )?;
    fs::write(
        generation_dir.join(TOMBSTONES_FILE_NAME),
        encode_tombstones(repo_root, diff),
    )?;
    touch_overlay_accessed(&generation_dir)?;

    Ok(info)
}

pub(crate) fn read_overlay_snapshot(
    index_root: &Path,
    base_snapshot_key: &str,
) -> Result<Option<OverlaySnapshotInfo>> {
    let overlay_root = overlay_root(index_root, base_snapshot_key);
    let current_path = overlay_root.join(CURRENT_FILE_NAME);
    if !current_path.exists() {
        return Ok(None);
    }

    let overlay_key = fs::read_to_string(&current_path)?;
    let overlay_key = overlay_key.trim();
    if overlay_key.is_empty() {
        return Err(AppError::InvalidIndex(
            "overlay CURRENT points to an empty generation".into(),
        ));
    }

    let manifest_path =
        overlay_generation_dir(index_root, base_snapshot_key, overlay_key).join(MANIFEST_FILE_NAME);
    let manifest = fs::read_to_string(manifest_path)?;
    Ok(Some(decode_manifest(&manifest)?))
}

pub(crate) fn ensure_fresh_overlay(
    config: &BuildConfig,
    diff: &IndexWorktreeDiff,
) -> Result<Option<OverlaySnapshotState>> {
    if diff.is_empty() {
        return Ok(None);
    }

    let Some(base_snapshot_key) = current_base_snapshot_key(&config.index_path)? else {
        return Ok(None);
    };

    let current_files = scan_existing_overlay_files(config, diff)?;
    let overlay_key = compute_overlay_key(config, &base_snapshot_key, diff, &current_files);
    if let Some(state) =
        read_overlay_state_by_key(&config.index_path, &base_snapshot_key, &overlay_key)?
    {
        activate_overlay_key(&config.index_path, &base_snapshot_key, &overlay_key)?;
        return Ok(Some(state));
    }
    if let Some(state) = read_overlay_state(&config.index_path, &base_snapshot_key)? {
        if overlay_matches_diff(&state, diff, &current_files) {
            return Ok(Some(state));
        }
    }

    build_overlay_index(
        config,
        &base_snapshot_key,
        &overlay_key,
        diff,
        &current_files,
    )?;
    read_overlay_state_by_key(&config.index_path, &base_snapshot_key, &overlay_key)
}

pub(crate) fn read_overlay_state(
    index_root: &Path,
    base_snapshot_key: &str,
) -> Result<Option<OverlaySnapshotState>> {
    let Some(info) = read_overlay_snapshot(index_root, base_snapshot_key)? else {
        return Ok(None);
    };

    read_overlay_state_by_key(index_root, base_snapshot_key, &info.overlay_key)
}

fn read_overlay_state_by_key(
    index_root: &Path,
    base_snapshot_key: &str,
    overlay_key: &str,
) -> Result<Option<OverlaySnapshotState>> {
    let manifest_path =
        overlay_generation_dir(index_root, base_snapshot_key, overlay_key).join(MANIFEST_FILE_NAME);
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest = fs::read_to_string(manifest_path)?;
    let info = decode_manifest(&manifest)?;

    let data_path = overlay_generation_dir(index_root, base_snapshot_key, overlay_key);
    let docs_path = data_path.join("docs.bin");
    if !docs_path.exists() {
        return Ok(None);
    }

    let (_, docs) = read_docs_file(&docs_path)?;
    let repo_root = docs_repo_root(index_root, base_snapshot_key, &data_path)?;
    let indexed_paths = docs
        .iter()
        .map(|doc| {
            resolve_repo_path(&repo_root, &doc.path)
                .to_string_lossy()
                .into_owned()
        })
        .collect::<HashSet<_>>();
    let indexed_files = docs
        .into_iter()
        .map(|doc| {
            (
                resolve_repo_path(&repo_root, &doc.path)
                    .to_string_lossy()
                    .into_owned(),
                (doc.size, doc.mtime_nanos),
            )
        })
        .collect::<HashMap<_, _>>();
    let shadowed_paths = read_tombstones(&data_path, &repo_root)?;

    Ok(Some(OverlaySnapshotState {
        info,
        data_path,
        indexed_paths,
        indexed_files,
        shadowed_paths,
    }))
}

fn build_overlay_index(
    config: &BuildConfig,
    base_snapshot_key: &str,
    overlay_key: &str,
    diff: &IndexWorktreeDiff,
    files: &[RepositoryFile],
) -> Result<OverlaySnapshotInfo> {
    let info = write_overlay_snapshot(
        &config.index_path,
        base_snapshot_key,
        overlay_key,
        &config.repo_path,
        diff,
    )?;
    let generation_dir =
        overlay_generation_dir(&config.index_path, base_snapshot_key, &info.overlay_key);
    ensure_dir(&generation_dir)?;

    let tokenizer_options = TokenizerOptions {
        min_sparse_len: config.min_sparse_len,
        max_sparse_len: config.max_sparse_len,
    };
    let cached_docs = load_overlay_cached_docs(config, base_snapshot_key);
    let mut indexed_files = process_overlay_files(
        files,
        &config.repo_path,
        config.tokenizer,
        tokenizer_options,
        &cached_docs,
    )?;
    sort_overlay_files_in_stable_order(
        &mut indexed_files,
        previous_overlay_path_order(&config.index_path, base_snapshot_key)?,
    );

    let mut docs = Vec::with_capacity(indexed_files.len());
    let mut doc_terms = Vec::with_capacity(indexed_files.len());
    let mut inverted = HashMap::<u64, Vec<u32>>::new();
    for (doc_id, file) in indexed_files.into_iter().enumerate() {
        let doc_id = usize_to_u32(doc_id, "overlay doc id")?;
        for &token_hash in &file.token_hashes {
            inverted.entry(token_hash).or_default().push(doc_id);
        }
        docs.push(DocMeta {
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
        .then(|| compute_high_freq_doc_threshold(docs.len()));

    let layout = IndexLayout::new(&generation_dir);
    write_docs_file(
        &layout.docs_path,
        IndexMetadata {
            tokenizer: config.tokenizer,
            min_sparse_len: config.min_sparse_len,
            max_sparse_len: config.max_sparse_len,
            fallback_trigram: None,
            build: Some(IndexBuildSettings {
                repo_root: canonical_repo_root(&config.repo_path),
                corpus_mode: config.corpus_mode,
                include_hidden: config.include_hidden,
                max_file_size: config.max_file_size,
                head_commit: resolve_git_head(&config.repo_path)?,
                config_fingerprint: Some(build_config_fingerprint(config)),
            }),
        },
        &docs,
    )?;
    write_doc_terms_file(&layout.doc_terms_path, &doc_terms)?;
    let lookup_entries = if let Some(previous_overlay) =
        load_previous_overlay_index(&config.index_path, base_snapshot_key)?
    {
        write_overlay_postings_incremental(
            &layout.postings_path,
            &previous_overlay,
            &docs,
            &doc_terms,
            high_freq_doc_threshold,
        )?
    } else {
        write_postings_file(&layout.postings_path, &postings, high_freq_doc_threshold)?
    };
    write_lookup_file(&layout.lookup_path, &lookup_entries)?;
    activate_overlay_key(&config.index_path, base_snapshot_key, &info.overlay_key)?;

    Ok(info)
}

fn overlay_root(index_root: &Path, base_snapshot_key: &str) -> PathBuf {
    index_root.join(OVERLAYS_DIR_NAME).join(base_snapshot_key)
}

fn overlay_generation_dir(
    index_root: &Path,
    base_snapshot_key: &str,
    overlay_key: &str,
) -> PathBuf {
    overlay_root(index_root, base_snapshot_key)
        .join(GENERATIONS_DIR_NAME)
        .join(overlay_key)
}

fn scan_existing_overlay_files(
    config: &BuildConfig,
    diff: &IndexWorktreeDiff,
) -> Result<Vec<RepositoryFile>> {
    let roots = diff
        .modified_files
        .iter()
        .chain(diff.new_files.iter())
        .map(|path| resolve_repo_path(&config.repo_path, path))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for mut file in scan_paths(
        &roots,
        Some(&config.index_path),
        ScanOptions {
            respect_ignore: matches!(config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: config.include_hidden,
            max_file_size: config.max_file_size,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )? {
        if read_text_file(&file.path)?.is_some() {
            file.ordinal = files.len();
            files.push(file);
        }
    }

    Ok(files)
}

fn process_overlay_files(
    files: &[RepositoryFile],
    repo_root: &Path,
    tokenizer_mode: crate::config::TokenizerMode,
    tokenizer_options: TokenizerOptions,
    cached_docs: &HashMap<String, CachedOverlayDoc>,
) -> Result<Vec<IndexedOverlayFile>> {
    let tokenizer = create(tokenizer_mode, tokenizer_options);
    let mut docs = Vec::with_capacity(files.len());
    for file in files {
        let logical_path = repo_relative_path(&file.path, repo_root);
        if let Some(cached) = cached_docs.get(&logical_path) {
            if cached.size == file.size && cached.mtime_nanos == file.mtime_nanos {
                docs.push(IndexedOverlayFile {
                    path: logical_path,
                    size: file.size,
                    mtime_nanos: file.mtime_nanos,
                    token_hashes: cached.token_hashes.clone(),
                });
                continue;
            }
        }
        let Some(text) = read_text_file(&file.path)? else {
            continue;
        };
        let folded = fold_for_index(text);
        let mut token_hashes = Vec::new();
        tokenizer.collect_document_token_hashes(&folded, &mut token_hashes);
        token_hashes.sort_unstable();
        token_hashes.dedup();

        docs.push(IndexedOverlayFile {
            path: logical_path,
            size: file.size,
            mtime_nanos: file.mtime_nanos,
            token_hashes,
        });
    }
    Ok(docs)
}

fn sort_overlay_files_in_stable_order(
    indexed_files: &mut [IndexedOverlayFile],
    previous_order: Vec<String>,
) {
    let previous_positions = previous_order
        .into_iter()
        .enumerate()
        .map(|(idx, path)| (path, idx))
        .collect::<HashMap<_, _>>();
    indexed_files.sort_unstable_by(|left, right| {
        let left_key = previous_positions
            .get(&left.path)
            .copied()
            .unwrap_or(usize::MAX);
        let right_key = previous_positions
            .get(&right.path)
            .copied()
            .unwrap_or(usize::MAX);
        left_key
            .cmp(&right_key)
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn encode_manifest(info: &OverlaySnapshotInfo) -> String {
    format!(
        "base_snapshot_key={}\noverlay_key={}\nmodified_files={}\ndeleted_files={}\nnew_files={}\n",
        info.base_snapshot_key,
        info.overlay_key,
        info.modified_files,
        info.deleted_files,
        info.new_files
    )
}

fn activate_overlay_key(
    index_root: &Path,
    base_snapshot_key: &str,
    overlay_key: &str,
) -> Result<()> {
    let overlay_root = overlay_root(index_root, base_snapshot_key);
    let current_path = overlay_root.join(CURRENT_FILE_NAME);
    let temp_path = overlay_root.join(format!("{CURRENT_FILE_NAME}.tmp"));
    fs::create_dir_all(overlay_root)?;
    fs::write(&temp_path, format!("{overlay_key}\n"))?;
    fs::rename(temp_path, current_path)?;
    touch_overlay_accessed(&overlay_generation_dir(
        index_root,
        base_snapshot_key,
        overlay_key,
    ))?;
    Ok(())
}

pub(crate) fn prune_overlay_caches(index_root: &Path, keep_per_base: usize) -> Result<usize> {
    let overlays_root = index_root.join(OVERLAYS_DIR_NAME);
    let Ok(entries) = fs::read_dir(&overlays_root) else {
        return Ok(0);
    };

    let mut removed = 0usize;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Some(base_snapshot_key) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        removed +=
            prune_overlay_generations_for_base(index_root, &base_snapshot_key, keep_per_base)?;
    }
    Ok(removed)
}

fn encode_tombstones(repo_root: &Path, diff: &IndexWorktreeDiff) -> String {
    diff.modified_files
        .iter()
        .chain(diff.deleted_files.iter())
        .map(|path| format!("{}\n", repo_relative_path(Path::new(path), repo_root)))
        .collect()
}

fn decode_manifest(manifest: &str) -> Result<OverlaySnapshotInfo> {
    let mut base_snapshot_key = None;
    let mut overlay_key = None;
    let mut modified_files = None;
    let mut deleted_files = None;
    let mut new_files = None;

    for line in manifest.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "base_snapshot_key" | "base_generation" => base_snapshot_key = Some(value.to_string()),
            "overlay_key" => overlay_key = Some(value.to_string()),
            "overlay_generation" => {
                overlay_key.get_or_insert_with(|| value.to_string());
            }
            "modified_files" => modified_files = Some(parse_count(key, value)?),
            "deleted_files" => deleted_files = Some(parse_count(key, value)?),
            "new_files" => new_files = Some(parse_count(key, value)?),
            _ => {}
        }
    }

    Ok(OverlaySnapshotInfo {
        base_snapshot_key: base_snapshot_key.ok_or_else(|| {
            AppError::InvalidIndex("overlay manifest missing base_snapshot_key".into())
        })?,
        overlay_key: overlay_key.unwrap_or_else(|| "overlay-legacy".to_string()),
        modified_files: modified_files.ok_or_else(|| {
            AppError::InvalidIndex("overlay manifest missing modified_files".into())
        })?,
        deleted_files: deleted_files.ok_or_else(|| {
            AppError::InvalidIndex("overlay manifest missing deleted_files".into())
        })?,
        new_files: new_files
            .ok_or_else(|| AppError::InvalidIndex("overlay manifest missing new_files".into()))?,
    })
}

fn read_tombstones(generation_dir: &Path, repo_root: &Path) -> Result<HashSet<String>> {
    let path = generation_dir.join(TOMBSTONES_FILE_NAME);
    if !path.exists() {
        return Ok(HashSet::new());
    }

    Ok(fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            resolve_repo_path(repo_root, line)
                .to_string_lossy()
                .into_owned()
        })
        .collect())
}

fn prune_overlay_generations_for_base(
    index_root: &Path,
    base_snapshot_key: &str,
    keep_per_base: usize,
) -> Result<usize> {
    let generations_dir = overlay_root(index_root, base_snapshot_key).join(GENERATIONS_DIR_NAME);
    let Ok(entries) = fs::read_dir(&generations_dir) else {
        return Ok(0);
    };
    let current_generation =
        read_overlay_snapshot(index_root, base_snapshot_key)?.map(|info| info.overlay_key);

    let mut generations = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let generation = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let last_accessed = overlay_last_accessed_nanos(&path);
        generations.push((generation, path, last_accessed));
    }

    generations.sort_unstable_by(|left, right| {
        let left_current = current_generation.as_deref() == Some(left.0.as_str());
        let right_current = current_generation.as_deref() == Some(right.0.as_str());
        right_current
            .cmp(&left_current)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.cmp(&right.0))
    });

    let keep_count = keep_per_base.max(usize::from(current_generation.is_some()));
    let mut removed = 0usize;
    for (_, path, _) in generations.into_iter().skip(keep_count) {
        if path.exists() {
            fs::remove_dir_all(path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn overlay_last_accessed_nanos(generation_dir: &Path) -> u128 {
    fs::metadata(generation_dir.join(LAST_ACCESSED_FILE_NAME))
        .or_else(|_| fs::metadata(generation_dir.join(MANIFEST_FILE_NAME)))
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn touch_overlay_accessed(generation_dir: &Path) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    fs::write(
        generation_dir.join(LAST_ACCESSED_FILE_NAME),
        format!("{now}\n"),
    )?;
    Ok(())
}

fn docs_repo_root(
    _index_root: &Path,
    _base_snapshot_key: &str,
    data_path: &Path,
) -> Result<PathBuf> {
    let docs_path = data_path.join("docs.bin");
    let (metadata, _) = read_docs_file(&docs_path)?;
    let Some(build) = metadata.build else {
        return Err(AppError::InvalidIndex(
            "overlay docs are missing build metadata".into(),
        ));
    };
    Ok(resolve_repo_path(Path::new(&build.repo_root), "."))
}

fn overlay_matches_diff(
    state: &OverlaySnapshotState,
    diff: &IndexWorktreeDiff,
    current_files: &[RepositoryFile],
) -> bool {
    let indexed_paths = current_files
        .iter()
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect::<HashSet<_>>();
    let indexed_files = current_files
        .iter()
        .map(|file| {
            (
                file.path.to_string_lossy().into_owned(),
                (file.size, file.mtime_nanos),
            )
        })
        .collect::<HashMap<_, _>>();
    let shadowed_paths = diff
        .modified_files
        .iter()
        .chain(diff.deleted_files.iter())
        .cloned()
        .collect::<HashSet<_>>();
    state.indexed_paths == indexed_paths
        && state.indexed_files == indexed_files
        && state.shadowed_paths == shadowed_paths
}

fn compute_overlay_key(
    config: &BuildConfig,
    base_snapshot_key: &str,
    diff: &IndexWorktreeDiff,
    current_files: &[RepositoryFile],
) -> String {
    let mut hasher = Hasher::new();
    hasher.update(base_snapshot_key.as_bytes());
    hasher.update(build_config_fingerprint(config).as_bytes());
    hasher.update(b"modified");
    for path in &diff.modified_files {
        hasher.update(repo_relative_path(Path::new(path), &config.repo_path).as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(b"deleted");
    for path in &diff.deleted_files {
        hasher.update(repo_relative_path(Path::new(path), &config.repo_path).as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(b"new");
    for path in &diff.new_files {
        hasher.update(repo_relative_path(Path::new(path), &config.repo_path).as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(b"files");
    for file in current_files {
        hasher.update(repo_relative_path(&file.path, &config.repo_path).as_bytes());
        hasher.update(&[0]);
        hasher.update(&file.size.to_le_bytes());
        hasher.update(&file.mtime_nanos.to_le_bytes());
    }
    format!("overlay-{:08x}", hasher.finalize())
}

fn load_overlay_cached_docs(
    config: &BuildConfig,
    base_snapshot_key: &str,
) -> HashMap<String, CachedOverlayDoc> {
    let generations_dir =
        overlay_root(&config.index_path, base_snapshot_key).join(GENERATIONS_DIR_NAME);
    let Ok(entries) = fs::read_dir(generations_dir) else {
        return HashMap::new();
    };

    let mut cached = HashMap::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let data_path = entry.path();
        let docs_path = data_path.join("docs.bin");
        let doc_terms_path = data_path.join("doc_terms.bin");
        if !docs_path.exists() || !doc_terms_path.exists() {
            continue;
        }

        let Ok((metadata, docs)) = read_docs_file(&docs_path) else {
            continue;
        };
        if !overlay_build_settings_match(metadata.build.as_ref(), config) {
            continue;
        }
        let Ok(doc_terms) = read_doc_terms_file(&doc_terms_path) else {
            continue;
        };
        if docs.len() != doc_terms.len() {
            continue;
        }

        for (doc, token_hashes) in docs.into_iter().zip(doc_terms) {
            cached.entry(doc.path).or_insert(CachedOverlayDoc {
                size: doc.size,
                mtime_nanos: doc.mtime_nanos,
                token_hashes,
            });
        }
    }

    cached
}

fn load_previous_overlay_index(
    index_root: &Path,
    base_snapshot_key: &str,
) -> Result<Option<PreviousOverlayIndex>> {
    let Some(info) = read_overlay_snapshot(index_root, base_snapshot_key)? else {
        return Ok(None);
    };
    let data_path = overlay_generation_dir(index_root, base_snapshot_key, &info.overlay_key);
    let docs_path = data_path.join("docs.bin");
    let doc_terms_path = data_path.join("doc_terms.bin");
    let lookup_path = data_path.join("lookup.bin");
    let postings_path = data_path.join("postings.bin");
    if !docs_path.exists()
        || !doc_terms_path.exists()
        || !lookup_path.exists()
        || !postings_path.exists()
    {
        return Ok(None);
    }

    let (_, docs) = read_docs_file(&docs_path)?;
    let doc_terms = read_doc_terms_file(&doc_terms_path)?;
    if docs.len() != doc_terms.len() {
        return Ok(None);
    }

    Ok(Some(PreviousOverlayIndex {
        docs,
        doc_terms,
        lookup: LookupTable::open(&lookup_path)?,
        postings: PostingsData::open(&postings_path)?,
    }))
}

fn write_overlay_postings_incremental(
    postings_path: &Path,
    previous: &PreviousOverlayIndex,
    docs: &[DocMeta],
    doc_terms: &[Vec<u64>],
    high_freq_doc_threshold: Option<usize>,
) -> Result<Vec<LookupEntry>> {
    let previous_docs = previous
        .docs
        .iter()
        .cloned()
        .zip(previous.doc_terms.iter().cloned())
        .map(|(doc, token_hashes)| (doc.path.clone(), PreviousOverlayDoc { doc, token_hashes }))
        .collect::<HashMap<_, _>>();
    let current_docs = docs
        .iter()
        .cloned()
        .zip(doc_terms.iter().cloned())
        .map(|(doc, token_hashes)| (doc.path.clone(), CurrentOverlayDoc { doc, token_hashes }))
        .collect::<HashMap<_, _>>();

    let mut affected_old_doc_ids = HashSet::new();
    let mut affected_tokens = HashSet::new();
    let mut added_doc_ids_by_token = HashMap::<u64, Vec<u32>>::new();
    let mut all_paths = previous_docs.keys().cloned().collect::<HashSet<_>>();
    all_paths.extend(current_docs.keys().cloned());
    for path in all_paths {
        let previous_doc = previous_docs.get(&path);
        let current_doc = current_docs.get(&path);
        if overlay_doc_unchanged(previous_doc, current_doc) {
            continue;
        }
        if let Some(previous_doc) = previous_doc {
            affected_old_doc_ids.insert(previous_doc.doc.doc_id);
            affected_tokens.extend(previous_doc.token_hashes.iter().copied());
        }
        if let Some(current_doc) = current_doc {
            for &token_hash in &current_doc.token_hashes {
                affected_tokens.insert(token_hash);
                added_doc_ids_by_token
                    .entry(token_hash)
                    .or_default()
                    .push(current_doc.doc.doc_id);
            }
        }
    }

    let mut rewritten = HashMap::<u64, Vec<u32>>::new();
    for &token_hash in &affected_tokens {
        let mut doc_ids = previous
            .lookup
            .find(token_hash)
            .map(|entry| previous.postings.decode(entry))
            .transpose()?
            .unwrap_or_default();
        doc_ids.retain(|doc_id| !affected_old_doc_ids.contains(doc_id));
        if let Some(added) = added_doc_ids_by_token.get(&token_hash) {
            doc_ids.extend(added.iter().copied());
        }
        doc_ids.sort_unstable();
        doc_ids.dedup();
        if !doc_ids.is_empty() {
            rewritten.insert(token_hash, doc_ids);
        }
    }

    let old_entries = previous.lookup.entries();
    let mut new_token_hashes = rewritten.keys().copied().collect::<Vec<_>>();
    new_token_hashes.sort_unstable();
    let final_count = old_entries
        .iter()
        .filter(|entry| !affected_tokens.contains(&entry.token_hash))
        .count()
        + rewritten.len();
    let mut writer = PostingsWriter::create(postings_path, final_count)?;
    let mut next_new_index = 0usize;
    for entry in old_entries {
        while next_new_index < new_token_hashes.len()
            && new_token_hashes[next_new_index] < entry.token_hash
        {
            let token_hash = new_token_hashes[next_new_index];
            writer.write_posting_list(
                token_hash,
                rewritten
                    .get(&token_hash)
                    .expect("rewritten posting should exist"),
                high_freq_doc_threshold,
            )?;
            next_new_index += 1;
        }

        if affected_tokens.contains(&entry.token_hash) {
            if let Some(doc_ids) = rewritten.get(&entry.token_hash) {
                writer.write_posting_list(entry.token_hash, doc_ids, high_freq_doc_threshold)?;
            }
        } else {
            writer.write_posting_bytes(
                entry.token_hash,
                previous.postings.bytes(entry)?,
                entry.doc_freq,
                entry.flags,
            )?;
        }

        if next_new_index < new_token_hashes.len()
            && new_token_hashes[next_new_index] == entry.token_hash
        {
            next_new_index += 1;
        }
    }
    while next_new_index < new_token_hashes.len() {
        let token_hash = new_token_hashes[next_new_index];
        writer.write_posting_list(
            token_hash,
            rewritten
                .get(&token_hash)
                .expect("rewritten posting should exist"),
            high_freq_doc_threshold,
        )?;
        next_new_index += 1;
    }

    writer.finish()
}

fn previous_overlay_path_order(index_root: &Path, base_snapshot_key: &str) -> Result<Vec<String>> {
    let Some(info) = read_overlay_snapshot(index_root, base_snapshot_key)? else {
        return Ok(Vec::new());
    };
    let docs_path =
        overlay_generation_dir(index_root, base_snapshot_key, &info.overlay_key).join("docs.bin");
    if !docs_path.exists() {
        return Ok(Vec::new());
    }
    let (_, docs) = read_docs_file(&docs_path)?;
    Ok(docs.into_iter().map(|doc| doc.path).collect())
}

fn overlay_build_settings_match(build: Option<&IndexBuildSettings>, config: &BuildConfig) -> bool {
    let Some(build) = build else {
        return false;
    };
    build.repo_root == canonical_repo_root(&config.repo_path)
        && build.corpus_mode == config.corpus_mode
        && build.include_hidden == config.include_hidden
        && build.max_file_size == config.max_file_size
        && build.config_fingerprint.as_deref() == Some(build_config_fingerprint(config).as_str())
}

fn overlay_doc_unchanged(
    previous: Option<&PreviousOverlayDoc>,
    current: Option<&CurrentOverlayDoc>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            previous.doc.doc_id == current.doc.doc_id
                && previous.doc.size == current.doc.size
                && previous.doc.mtime_nanos == current.doc.mtime_nanos
                && previous.token_hashes == current.token_hashes
        }
        _ => false,
    }
}

fn parse_count(field: &str, value: &str) -> Result<usize> {
    value.parse::<usize>().map_err(|error| {
        AppError::InvalidIndex(format!("overlay manifest has invalid {field}: {error}"))
    })
}

fn compute_high_freq_doc_threshold(doc_count: usize) -> usize {
    (doc_count / 20).max(256)
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

struct IndexedOverlayFile {
    path: String,
    size: u64,
    mtime_nanos: u64,
    token_hashes: Vec<u64>,
}

struct CachedOverlayDoc {
    size: u64,
    mtime_nanos: u64,
    token_hashes: Vec<u64>,
}

struct PreviousOverlayIndex {
    docs: Vec<DocMeta>,
    doc_terms: Vec<Vec<u64>>,
    lookup: LookupTable,
    postings: PostingsData,
}

struct PreviousOverlayDoc {
    doc: DocMeta,
    token_hashes: Vec<u64>,
}

struct CurrentOverlayDoc {
    doc: DocMeta,
    token_hashes: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::{decode_manifest, encode_manifest, OverlaySnapshotInfo};

    #[test]
    fn overlay_manifest_roundtrip_uses_public_snapshot_keys() {
        let info = OverlaySnapshotInfo {
            base_snapshot_key: "base-git-123".into(),
            overlay_key: "overlay-abc".into(),
            modified_files: 1,
            deleted_files: 2,
            new_files: 3,
        };

        let decoded = decode_manifest(&encode_manifest(&info)).expect("manifest should decode");
        assert_eq!(decoded, info);
    }

    #[test]
    fn overlay_manifest_decodes_legacy_generation_fields() {
        let manifest = "\
base_generation=base-git-123
overlay_generation=overlay-abc
modified_files=1
deleted_files=2
new_files=3
";

        let decoded = decode_manifest(manifest).expect("legacy manifest should decode");
        assert_eq!(decoded.base_snapshot_key, "base-git-123");
        assert_eq!(decoded.overlay_key, "overlay-abc");
        assert_eq!(decoded.modified_files, 1);
        assert_eq!(decoded.deleted_files, 2);
        assert_eq!(decoded.new_files, 3);
    }
}
