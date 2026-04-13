use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    files::{is_text_file, resolve_repo_path, scan_repository},
    index::format::{DocMetaRef, IndexBuildSettings, IndexLayout, LookupTable, PostingsData},
    path_filter::PathFilter,
    tokenizer::TokenizerOptions,
};

use super::{shared::doc_by_id, DirtyPathKind, IndexSearcher, IndexWorktreeDiff};

impl IndexSearcher {
    pub fn open(index_path: PathBuf) -> Result<Self> {
        let layout = IndexLayout::resolve(&index_path)?;
        let docs = crate::index::format::DocsData::open(&layout.docs_path)?;
        let metadata = docs.metadata().clone();
        let lookup = LookupTable::open(&layout.lookup_path)?;
        let postings = PostingsData::open(&layout.postings_path)?;

        Ok(Self {
            index_path,
            metadata: metadata.clone(),
            docs,
            lookup,
            postings,
            repo_root: metadata
                .build
                .as_ref()
                .map(|build| resolve_repo_path(Path::new(&build.repo_root), ".")),
            tokenizer_mode: metadata.tokenizer,
            tokenizer_options: TokenizerOptions {
                min_sparse_len: metadata.min_sparse_len,
                max_sparse_len: metadata.max_sparse_len,
            },
        })
    }

    pub fn build_settings(&self) -> Option<&IndexBuildSettings> {
        self.metadata.build.as_ref()
    }

    pub fn tokenizer_mode(&self) -> crate::config::TokenizerMode {
        self.tokenizer_mode
    }

    pub fn tokenizer_options(&self) -> &TokenizerOptions {
        &self.tokenizer_options
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn ensure_query_scope_supported(
        &self,
        include_hidden: bool,
        no_ignore: bool,
        max_file_size: Option<u64>,
    ) -> Result<()> {
        let Some(build) = self.build_settings() else {
            if include_hidden || no_ignore || max_file_size.is_some() {
                return Err(AppError::InvalidIndex(
                    "index metadata is too old for search-scope overrides; rebuild the index"
                        .into(),
                ));
            }
            return Ok(());
        };

        if include_hidden && !build.include_hidden {
            return Err(AppError::InvalidIndex(
                "searching hidden files requires an index built with --hidden".into(),
            ));
        }
        if no_ignore && matches!(build.corpus_mode, crate::config::CorpusMode::RespectIgnore) {
            return Err(AppError::InvalidIndex(
                "searching ignored files requires an index built with --no-ignore".into(),
            ));
        }
        if max_file_size.is_some_and(|value| value > build.max_file_size) {
            return Err(AppError::InvalidIndex(format!(
                "search --max-filesize={} exceeds the index build limit of {} bytes",
                max_file_size.unwrap_or_default(),
                build.max_file_size,
            )));
        }
        Ok(())
    }

    pub fn diff_against_worktree(&self) -> Result<IndexWorktreeDiff> {
        let Some(build) = self.build_settings() else {
            return Ok(IndexWorktreeDiff::default());
        };

        let current_files = scan_repository(&BuildConfig {
            repo_path: PathBuf::from(&build.repo_root),
            index_path: self.index_path.clone(),
            tokenizer: self.metadata.tokenizer,
            corpus_mode: build.corpus_mode,
            include_hidden: build.include_hidden,
            max_file_size: build.max_file_size,
            min_sparse_len: self.metadata.min_sparse_len,
            max_sparse_len: self.metadata.max_sparse_len,
        })?;
        let current = current_files
            .into_iter()
            .filter_map(|file| match is_text_file(&file.path) {
                Ok(true) => Some(Ok(file)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(|file| {
                (
                    file.path.to_string_lossy().into_owned(),
                    (file.size, file.mtime_nanos),
                )
            })
            .collect::<HashMap<_, _>>();
        let indexed_paths = self
            .docs
            .iter()
            .map(|doc| self.doc_display_path_ref(doc))
            .collect::<HashSet<_>>();

        let mut modified_files = Vec::new();
        let mut deleted_files = Vec::new();
        for doc in self.docs.iter() {
            let doc_path = self.doc_display_path_ref(doc);
            match current.get(&doc_path) {
                Some(&(size, mtime_nanos))
                    if size == doc.size() && mtime_nanos == doc.mtime_nanos() => {}
                Some(_) => modified_files.push(doc_path),
                None => deleted_files.push(doc_path),
            }
        }

        modified_files.sort_unstable();
        deleted_files.sort_unstable();
        let mut new_files = current
            .keys()
            .filter(|path| !indexed_paths.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        new_files.sort_unstable();

        Ok(IndexWorktreeDiff {
            modified_files,
            deleted_files,
            new_files,
        })
    }

    pub fn stale_reason(&self) -> Result<Option<String>> {
        let diff = self.diff_against_worktree()?;
        if let Some(path) = diff.modified_files.first() {
            return Ok(Some(format!("indexed file changed: {path}")));
        }
        if let Some(path) = diff.deleted_files.first() {
            return Ok(Some(format!("indexed file disappeared: {path}")));
        }
        if let Some(path) = diff.new_files.first() {
            return Ok(Some(format!("new file is missing from the index: {path}")));
        }
        Ok(None)
    }

    pub(crate) fn classify_dirty_path(&self, path: &str) -> Result<Option<DirtyPathKind>> {
        let indexed = self
            .docs
            .iter()
            .find(|doc| self.doc_display_path_ref(*doc) == path);

        match indexed {
            Some(doc) => match fs::metadata(path) {
                Ok(metadata) => {
                    if !metadata.is_file() || !is_text_file(Path::new(path))? {
                        return Ok(Some(DirtyPathKind::Deleted));
                    }
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| match u64::try_from(duration.as_nanos()) {
                            Ok(value) => value,
                            Err(_) => u64::MAX,
                        })
                        .unwrap_or_default();
                    if metadata.len() != doc.size() || modified != doc.mtime_nanos() {
                        Ok(Some(DirtyPathKind::Modified))
                    } else {
                        Ok(None)
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(Some(DirtyPathKind::Deleted))
                }
                Err(error) => Err(error.into()),
            },
            None => {
                let path = Path::new(path);
                if !path.exists() || !path.is_file() {
                    return Ok(None);
                }
                if is_text_file(path)? {
                    Ok(Some(DirtyPathKind::New))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub(crate) fn has_indexed_path_under(&self, directory: &str) -> bool {
        let prefix = format!("{directory}{}", std::path::MAIN_SEPARATOR);
        self.docs.iter().any(|doc| {
            let path = self.doc_display_path_ref(doc);
            path == directory || path.starts_with(&prefix)
        })
    }

    pub(crate) fn indexed_paths_with_allowed_paths(
        &self,
        filter: Option<&PathFilter>,
        allowed_paths: Option<&HashSet<String>>,
    ) -> Vec<String> {
        self.docs
            .iter()
            .filter(|doc| self.doc_matches_filters(*doc, filter, allowed_paths))
            .map(|doc| self.doc_display_path_ref(doc))
            .collect()
    }

    pub(super) fn doc_display_path_ref(&self, doc: DocMetaRef<'_>) -> String {
        self.doc_resolved_path_ref(doc)
            .to_string_lossy()
            .into_owned()
    }

    pub(super) fn doc_resolved_path_ref(&self, doc: DocMetaRef<'_>) -> PathBuf {
        self.resolve_doc_path_str(doc.path())
    }

    pub(super) fn resolve_doc_path_by_id(&self, doc_id: u32) -> Result<PathBuf> {
        Ok(self.doc_resolved_path_ref(doc_by_id(&self.docs, doc_id)?))
    }

    pub(super) fn doc_display_path(&self, doc: &crate::index::format::DocMeta) -> String {
        self.doc_display_path_str(&doc.path)
    }

    fn doc_display_path_str(&self, path: &str) -> String {
        self.resolve_doc_path_str(path)
            .to_string_lossy()
            .into_owned()
    }

    fn resolve_doc_path_str(&self, path: &str) -> PathBuf {
        if Path::new(path).is_absolute() {
            return PathBuf::from(path);
        }

        if let Some(repo_root) = &self.repo_root {
            return resolve_repo_path(repo_root, path);
        }

        PathBuf::from(path)
    }
}
