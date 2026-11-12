use std::{
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;

use crate::{
    config::{BuildConfig, CorpusMode},
    error::Result,
    path_filter::PathFilter,
    path_utils::{
        normalize_lexical_path, repo_relative_path as repo_relative_path_impl,
        resolve_repo_path as resolve_repo_path_impl,
    },
};

#[derive(Debug, Clone)]
pub struct RepositoryFile {
    pub ordinal: usize,
    pub path: PathBuf,
    pub size: u64,
    pub mtime_nanos: u64,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub respect_ignore: bool,
    pub include_hidden: bool,
    pub max_file_size: u64,
    pub max_depth: Option<usize>,
    pub ignore_files: Vec<PathBuf>,
}

pub fn is_workspace_internal_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str(),
            component if component == OsStr::new(".flashgrep-index")
                || component == OsStr::new(".flashgrep-index-engine")
                || component == OsStr::new(".flashgrep-bench")
        )
    })
}

/// Static path policy shared by filesystem walks and Git base inputs.
/// Git membership may exempt ignore rules, never hidden/internal/storage rules.
pub(crate) fn workspace_path_is_eligible(
    root: &Path,
    path: &Path,
    storage_root: Option<&Path>,
    include_hidden: bool,
) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    !is_workspace_internal_path(path)
        && !storage_root.is_some_and(|storage| path.starts_with(storage))
        && relative.components().all(|part| match part {
            std::path::Component::Normal(name) => {
                name != OsStr::new(".git")
                    && (include_hidden || !name.to_string_lossy().starts_with('.'))
            }
            _ => false,
        })
}

pub fn scan_repository(config: &BuildConfig) -> Result<Vec<RepositoryFile>> {
    scan_paths(
        std::slice::from_ref(&config.repo_path),
        Some(&config.storage_root),
        ScanOptions {
            respect_ignore: matches!(config.corpus_mode, CorpusMode::RespectIgnore),
            include_hidden: config.include_hidden,
            max_file_size: config.max_file_size,
            max_depth: None,
            ignore_files: Vec::new(),
        },
        None,
    )
}

pub fn read_text_file(path: &std::path::Path) -> Result<Option<String>> {
    let bytes = fs::read(path)?;
    if looks_binary(&bytes) {
        return Ok(None);
    }

    match String::from_utf8(bytes) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

pub fn is_corpus_text_file(path: &Path) -> Result<bool> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    File::open(path)?.take(8 * 1024).read_to_end(&mut bytes)?;
    Ok(!looks_binary(&bytes))
}

pub fn is_text_file(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut probe = [0u8; 8 * 1024];
    let read = file.read(&mut probe)?;
    Ok(looks_like_text_prefix(&probe[..read]))
}

pub fn repo_relative_path(path: &Path, repo_root: &Path) -> String {
    repo_relative_path_impl(path, repo_root)
}

pub fn resolve_repo_path(repo_root: &Path, path: &str) -> PathBuf {
    resolve_repo_path_impl(repo_root, path)
}

pub fn scan_paths(
    roots: &[PathBuf],
    storage_root: Option<&Path>,
    options: ScanOptions,
    path_filter: Option<&PathFilter>,
) -> Result<Vec<RepositoryFile>> {
    let mut files = Vec::new();

    for root in roots {
        if is_workspace_internal_path(root) {
            continue;
        }
        if root.is_file() {
            if let Some(file) = repository_file(root, files.len(), options.max_file_size)? {
                if path_filter.is_none_or(|filter| filter.matches_file(root)) {
                    files.push(file);
                }
            }
            continue;
        }

        let walker = build_walker_from_options(root, storage_root, &options, None)?;
        for entry in walker.build() {
            let entry = entry?;
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            if path_filter.is_some_and(|filter| !filter.matches_file(entry.path())) {
                continue;
            }
            if let Some(file) = repository_file(entry.path(), files.len(), options.max_file_size)? {
                files.push(file);
            }
        }
    }

    Ok(files)
}

/// Walk selected scopes using ignore rules anchored at the workspace root.
/// Missing scopes are valid: their old index entries are reconciled as deletions.
pub(crate) fn scan_workspace_scopes(
    root: &Path,
    scopes: Option<&[PathBuf]>,
    storage_root: Option<&Path>,
    options: ScanOptions,
) -> Result<Vec<RepositoryFile>> {
    let walker = build_walker_from_options(root, storage_root, &options, scopes)?;
    let mut files = Vec::new();
    for entry in walker.build() {
        let entry = entry?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(file) = repository_file(entry.path(), files.len(), options.max_file_size)? {
            files.push(file);
        }
    }
    Ok(files)
}

fn build_walker_from_options(
    root: &Path,
    storage_root: Option<&Path>,
    options: &ScanOptions,
    scopes: Option<&[PathBuf]>,
) -> Result<WalkBuilder> {
    let mut builder = WalkBuilder::new(root);
    builder.follow_links(false);
    // Enforce size in repository_file. ignore's max_filesize shortcut can
    // bypass filter_entry, which would break scope/internal-path pruning.
    builder.max_depth(options.max_depth);
    let storage_root = storage_root.map(PathBuf::from);
    let scopes = scopes.map(|scopes| {
        let selected = scopes
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let parents = scopes
            .iter()
            .flat_map(|path| path.ancestors().skip(1))
            .map(Path::to_path_buf)
            .collect::<std::collections::HashSet<_>>();
        (selected, parents)
    });
    let workspace_root = root.to_path_buf();
    let include_hidden = options.include_hidden;
    builder.filter_entry(move |entry| {
        let path = entry.path();
        let directory = entry.file_type().is_some_and(|t| t.is_dir());
        let in_scope = scopes.as_ref().is_none_or(|(selected, parents)| {
            path.ancestors().any(|ancestor| selected.contains(ancestor))
                || (directory && parents.contains(path))
        });
        in_scope
            && workspace_path_is_eligible(
                &workspace_root,
                path,
                storage_root.as_deref(),
                include_hidden,
            )
    });

    if options.respect_ignore {
        builder.hidden(!options.include_hidden);
        builder.ignore(true);
        builder.git_ignore(true);
        builder.git_global(true);
        builder.git_exclude(true);
        builder.parents(true);
    } else {
        builder.hidden(!options.include_hidden);
        builder.ignore(false);
        builder.git_ignore(false);
        builder.git_global(false);
        builder.git_exclude(false);
        builder.parents(false);
        builder.require_git(false);
    }

    for ignore_file in &options.ignore_files {
        if let Some(error) = builder.add_ignore(ignore_file) {
            return Err(error.into());
        }
    }

    Ok(builder)
}

/// Check paths supplied outside the walker (for example Git membership) using
/// the workspace walker's no-follow policy, including symlinked ancestors.
pub(crate) fn workspace_file_metadata(root: &Path, path: &Path) -> Result<Option<fs::Metadata>> {
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Ok(None);
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error.into()),
    };
    for parent in path
        .ancestors()
        .skip(1)
        .take_while(|parent| *parent != root)
    {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() => (),
            Ok(_) => return Ok(None),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some(metadata))
}

pub(crate) fn repository_file(
    path: &Path,
    ordinal: usize,
    max_file_size: u64,
) -> Result<Option<RepositoryFile>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > max_file_size {
        return Ok(None);
    }

    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or_default();

    Ok(Some(RepositoryFile {
        ordinal,
        path: normalize_lexical_path(path),
        size: metadata.len(),
        mtime_nanos,
    }))
}

fn looks_binary(bytes: &[u8]) -> bool {
    let probe_len = bytes.len().min(8 * 1024);
    bytes[..probe_len].contains(&0)
}

fn looks_like_text_prefix(bytes: &[u8]) -> bool {
    if looks_binary(bytes) {
        return false;
    }

    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use super::{
        is_corpus_text_file, is_workspace_internal_path, looks_like_text_prefix, scan_paths,
        ScanOptions,
    };

    #[test]
    fn text_prefix_accepts_utf8() {
        assert!(looks_like_text_prefix("hello\nworld".as_bytes()));
    }

    #[test]
    fn text_prefix_rejects_nul() {
        assert!(!looks_like_text_prefix(b"hello\0world"));
    }

    #[test]
    fn text_prefix_accepts_truncated_utf8_tail() {
        let mut bytes = vec![b'a'; 8 * 1024 - 1];
        bytes.push(0xE4);
        assert!(looks_like_text_prefix(&bytes));
    }

    #[test]
    fn text_prefix_rejects_invalid_utf8() {
        assert!(!looks_like_text_prefix(&[0xFF, b'a']));
    }

    #[test]
    fn corpus_text_file_accepts_invalid_utf8_after_text_prefix() {
        let temp = TempDir::new().expect("temp dir should succeed");
        let path = temp.path().join("encoded.txt");
        let mut bytes = vec![b'a'; 8 * 1024];
        bytes.push(0xFF);
        fs::write(&path, bytes).expect("encoded file should be written");

        assert!(is_corpus_text_file(&path).expect("probe should succeed"));
    }

    #[test]
    fn workspace_internal_path_detects_flashgrep_artifacts() {
        assert!(is_workspace_internal_path(Path::new(
            "/tmp/repo/.flashgrep-index/docs.bin"
        )));
        assert!(is_workspace_internal_path(Path::new(
            "/tmp/repo/subdir/.flashgrep-bench/results.json"
        )));
        assert!(!is_workspace_internal_path(Path::new(
            "/tmp/repo/.bitfun/search/flashgrep-index/docs.bin"
        )));
        assert!(!is_workspace_internal_path(Path::new(
            "/tmp/repo/src/main.rs"
        )));
    }

    #[test]
    fn scan_paths_excludes_flashgrep_internal_files_and_configured_index_path() {
        let temp = TempDir::new().expect("temp dir should succeed");
        let repo = temp.path().join("repo");
        fs::create_dir_all(repo.join("src")).expect("src dir should succeed");
        fs::create_dir_all(repo.join(".flashgrep-bench"))
            .expect(".flashgrep-bench dir should succeed");
        fs::create_dir_all(repo.join(".bitfun").join("sessions"))
            .expect(".bitfun dir should succeed");
        fs::create_dir_all(repo.join(".bitfun").join("search").join("flashgrep-index"))
            .expect("index dir should succeed");
        fs::write(repo.join("src").join("main.rs"), "fn main() {}\n")
            .expect("main.rs should be written");
        fs::write(repo.join(".flashgrep-bench").join("summary.json"), "{}\n")
            .expect("bench file should be written");
        fs::write(
            repo.join(".bitfun").join("sessions").join("turn-0000.json"),
            "{}\n",
        )
        .expect("session file should be written");
        fs::write(
            repo.join(".bitfun")
                .join("search")
                .join("flashgrep-index")
                .join("docs.bin"),
            "index\n",
        )
        .expect("index file should be written");

        let scanned = scan_paths(
            &[PathBuf::from(&repo)],
            Some(&repo.join(".bitfun").join("search").join("flashgrep-index")),
            ScanOptions {
                respect_ignore: false,
                include_hidden: true,
                max_file_size: 1024 * 1024,
                max_depth: None,
                ignore_files: Vec::new(),
            },
            None,
        )
        .expect("scan_paths should succeed");

        let mut paths = scanned
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                repo.join(".bitfun").join("sessions").join("turn-0000.json"),
                repo.join("src").join("main.rs"),
            ]
        );
    }
}
