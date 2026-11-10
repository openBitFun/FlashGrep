use crate::{
    config::BuildConfig,
    files::is_workspace_internal_path,
    index_engine::{
        query::QueryFilters, reader::SnapshotPathCatalog, DocKey, EngineConfig, IndexDocument,
        IndexEngine, IndexSnapshotReader, RepoId, WorkspaceBaseSnapshotIndex, WorkspaceSearchView,
    },
    AppError, QueryConfig, Result, SearchMode,
};
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayStorage {
    Memory,
    Disk,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    Watcher,
    Git,
}
#[derive(Clone)]
pub struct Options {
    pub build: BuildConfig,
    pub storage: OverlayStorage,
    pub discovery: Discovery,
    /// Reuse an existing immutable Git base built by the production daemon.
    pub git_base_storage: Option<PathBuf>,
    pub reconcile_interval: Duration,
    pub event_capacity: usize,
}
#[derive(Default, Clone, Serialize)]
pub struct Status {
    pub publications: usize,
    pub dirty_paths: usize,
    pub dirty_sample: Vec<String>,
    pub content_bytes: usize,
    pub last_discovery_ms: f64,
    pub last_publish_ms: f64,
    pub reconciliations: usize,
    pub queue_drops: usize,
    pub os_rescan_requests: usize,
    pub git_rescan_requests: usize,
    pub watcher_errors: usize,
    pub pending: usize,
    pub busy: bool,
    pub last_error: Option<String>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: u64,
}
struct Published {
    overlay: Option<IndexSnapshotReader>,
}
pub struct WatchWorkspace {
    root: PathBuf,
    base: IndexSnapshotReader,
    view: WorkspaceSearchView,
    published: Arc<RwLock<Published>>,
    status: Arc<Mutex<Status>>,
    pending: Arc<AtomicUsize>,
    reconcile: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    _watcher: notify::RecommendedWatcher,
}
fn error(message: impl ToString) -> AppError {
    AppError::Protocol(message.to_string())
}
fn stamp(path: &Path) -> Result<Stamp> {
    let m = fs::metadata(path)?;
    Ok(Stamp {
        size: m.len(),
        mtime: m
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(error)?
            .as_nanos()
            .min(u64::MAX as u128) as u64,
    })
}
// BTreeMap prefix ranges keep a one-file event proportional to that scope,
// rather than walking the entire catalog once for every notification batch.
fn scoped_keys<T>(map: &BTreeMap<String, T>, root: &Path, scopes: &[PathBuf]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for scope in scopes {
        if root.starts_with(scope) {
            keys.extend(map.keys().cloned());
            continue;
        }
        let Ok(relative) = scope.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy();
        if map.contains_key(relative.as_ref()) {
            keys.insert(relative.to_string());
        }
        let prefix = format!("{relative}/");
        keys.extend(
            map.range(prefix.clone()..)
                .take_while(|(key, _)| key.starts_with(&prefix))
                .map(|(key, _)| key.clone()),
        );
    }
    keys
}
// Walk from the workspace root, pruning unrelated branches rather than treating
// an explicitly named (possibly ignored) file as an unconditional walker root.
fn inventory(config: &BuildConfig, scopes: &[PathBuf]) -> Result<BTreeMap<String, Stamp>> {
    let root = config.repo_path.clone();
    let storage = config.storage_root.clone();
    let repo_root = root.clone();
    let include_hidden = config.include_hidden;
    let ancestors: HashSet<PathBuf> = scopes
        .iter()
        .flat_map(|p| p.ancestors().map(Path::to_path_buf))
        .collect();
    let scopes: HashSet<PathBuf> = scopes.iter().cloned().collect();
    let mut walk = ignore::WalkBuilder::new(&root);
    let ignore = matches!(config.corpus_mode, crate::CorpusMode::RespectIgnore);
    walk.hidden(!config.include_hidden)
        .follow_links(false)
        .git_ignore(ignore)
        .git_global(ignore)
        .git_exclude(ignore)
        .ignore(ignore)
        .parents(ignore);
    // ignore 0.4.25 returns from its filesize check before running filter_entry
    // on files. Apply the size cap below so scope/hidden filters also run.
    walk.filter_entry(move |e| {
        (include_hidden
            || !e.path().strip_prefix(&repo_root).is_ok_and(|p| {
                p.components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            }))
            && !e.path().starts_with(&storage)
            && !is_workspace_internal_path(e.path())
            && !e.path().components().any(|c| c.as_os_str() == ".git")
            && (ancestors.contains(e.path()) || e.path().ancestors().any(|p| scopes.contains(p)))
    });
    let mut files = BTreeMap::new();
    for entry in walk.build() {
        let entry = entry?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            match stamp(entry.path()) {
                Ok(value) if value.size <= config.max_file_size => {
                    files.insert(
                        entry
                            .path()
                            .strip_prefix(&config.repo_path)
                            .map_err(error)?
                            .to_string_lossy()
                            .into_owned(),
                        value,
                    );
                }
                Ok(_) => (),
                Err(AppError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(files)
}
fn document(
    config: &BuildConfig,
    repo: &RepoId,
    path: &str,
    tracked: bool,
) -> Result<Option<IndexDocument>> {
    let abs = config.repo_path.join(path);
    let meta = match if tracked {
        fs::metadata(&abs)
    } else {
        fs::symlink_metadata(&abs)
    } {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() > config.max_file_size {
        return Ok(None);
    }
    let bytes = fs::read(&abs)?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(None);
    }
    let mut hash = [0; 32];
    hash[..4].copy_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
    Ok(Some(IndexDocument {
        key: DocKey {
            repo_id: repo.clone(),
            logical_path: path.into(),
        },
        repo_id: repo.clone(),
        logical_path: path.into(),
        file_name: Path::new(path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        language: None,
        size: bytes.len() as u64,
        modified_at: stamp(&abs)?.mtime,
        content_hash: hash,
        source_bytes: Arc::from(bytes),
    }))
}
fn git_paths(config: &BuildConfig, head: &str, scopes: &[PathBuf]) -> Result<BTreeSet<String>> {
    let mut result = BTreeSet::new();
    // Bound argv even for batched directory notifications.
    for chunk in scopes.chunks(64) {
        for args in [
            vec!["diff", "--name-only", "-z", "--no-renames", head, "--"],
            vec!["ls-files", "--others", "--exclude-standard", "-z", "--"],
        ] {
            let output = Command::new("git")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .arg("-C")
                .arg(&config.repo_path)
                .args(args)
                .args(chunk)
                .output()?;
            if !output.status.success() {
                return Err(error(String::from_utf8_lossy(&output.stderr)));
            }
            for p in output.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                result.insert(std::str::from_utf8(p).map_err(error)?.to_owned());
            }
        }
    }
    Ok(result)
}
fn tracked_paths(config: &BuildConfig) -> Result<BTreeMap<String, ()>> {
    let output = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(&config.repo_path)
        .args(["ls-files", "--cached", "-z"])
        .output()?;
    if !output.status.success() {
        return Err(error(String::from_utf8_lossy(&output.stderr)));
    }
    let mut paths = BTreeMap::new();
    for path in output.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = std::str::from_utf8(path).map_err(error)?;
        if !config.include_hidden
            && Path::new(path)
                .components()
                .any(|p| p.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if !is_workspace_internal_path(Path::new(path)) {
            paths.insert(path.to_string(), ());
        }
    }
    Ok(paths)
}
fn git_metadata_paths(config: &BuildConfig) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let resolve = |arg: &str| -> Result<PathBuf> {
        let output = Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(&config.repo_path)
            .args(["rev-parse", arg])
            .output()?;
        if !output.status.success() {
            return Err(error(String::from_utf8_lossy(&output.stderr)));
        }
        let path = PathBuf::from(std::str::from_utf8(&output.stdout).map_err(error)?.trim());
        Ok(fs::canonicalize(if path.is_absolute() {
            path
        } else {
            config.repo_path.join(path)
        })?)
    };
    let git = resolve("--git-dir")?;
    let common = resolve("--git-common-dir")?;
    let events = vec![
        git.join("index"),
        git.join("HEAD"),
        common.join("packed-refs"),
        common.join("refs"),
    ];
    let mut watches = vec![git, common.clone()];
    if common.join("refs").is_dir() {
        watches.push(common.join("refs"));
    }
    watches.sort();
    watches.dedup();
    Ok((watches, events))
}
struct Updater {
    options: Options,
    base: SnapshotPathCatalog,
    repo: RepoId,
    head: Option<String>,
    tracked: BTreeMap<String, ()>,
    observed: BTreeMap<String, Stamp>,
    dirty: BTreeMap<String, Option<IndexDocument>>,
    generation: usize,
}
impl Updater {
    fn update(
        &mut self,
        scopes: &[PathBuf],
        full: bool,
        published: &RwLock<Published>,
        status: &Mutex<Status>,
    ) -> Result<()> {
        let started = Instant::now();
        let mut files = inventory(&self.options.build, scopes)?;
        if self.head.is_some() {
            if full {
                self.tracked = tracked_paths(&self.options.build)?;
            }
            // Git base construction includes tracked paths even when ignore rules
            // match, including tracked symlinks to files. Match that policy here.
            for path in scoped_keys(&self.tracked, &self.options.build.repo_path, scopes) {
                if files.contains_key(&path) {
                    continue;
                }
                let abs = self.options.build.repo_path.join(&path);
                match fs::metadata(&abs) {
                    Ok(meta)
                        if meta.is_file() && meta.len() <= self.options.build.max_file_size =>
                    {
                        files.insert(path, stamp(&abs)?);
                    }
                    Ok(_) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let observed_keys = scoped_keys(&self.observed, &self.options.build.repo_path, scopes);
        let mut candidates = BTreeSet::new();
        if self.options.discovery == Discovery::Git {
            candidates = git_paths(
                &self.options.build,
                self.head
                    .as_deref()
                    .ok_or_else(|| error("Git discovery requires a committed Git base"))?,
                scopes,
            )?;
            candidates.extend(scoped_keys(
                &self.dirty,
                &self.options.build.repo_path,
                scopes,
            ));
        } else {
            let explicit: HashSet<_> = scopes.iter().filter(|p| p.is_file()).collect();
            for (p, stamp) in &files {
                if self.observed.get(p) != Some(stamp)
                    || (!full && explicit.contains(&self.options.build.repo_path.join(p)))
                {
                    candidates.insert(p.clone());
                }
            }
            candidates.extend(
                observed_keys
                    .iter()
                    .filter(|p| !files.contains_key(*p))
                    .cloned(),
            );
        }
        // Ignore-rule changes and offline deletions can change eligibility even
        // when Git reports no content diff for the affected tracked files.
        candidates.extend(
            observed_keys
                .iter()
                .filter(|p| !files.contains_key(*p))
                .cloned(),
        );
        if self.options.discovery == Discovery::Git {
            candidates.extend(
                files
                    .keys()
                    .filter(|p| !self.observed.contains_key(*p))
                    .cloned(),
            );
        }
        let mut next = self.dirty.clone();
        for path in candidates {
            let doc = if files.contains_key(&path) {
                document(
                    &self.options.build,
                    &self.repo,
                    &path,
                    self.tracked.contains_key(&path),
                )?
            } else {
                None
            };
            match (self.base.get(&path), &doc) {
                (None, None) => {
                    next.remove(&path);
                }
                (Some(base), Some(doc))
                    if base.size == doc.size && base.content_hash == doc.content_hash =>
                {
                    next.remove(&path);
                }
                _ => {
                    next.insert(path, doc);
                }
            }
        }
        let discovery_ms = started.elapsed().as_secs_f64() * 1000.;
        if next != self.dirty {
            let start = Instant::now();
            self.generation += 1;
            // Build off the query lock and atomically swap the complete immutable
            // overlay. This first experiment deliberately uses identical full-dirty
            // publication for disk/memory to isolate storage cost.
            let root = self
                .options
                .build
                .storage_root
                .join(format!("experiment-overlay-{}", self.generation));
            let config = EngineConfig {
                index_root: root,
                tokenizer: (&self.options.build).into(),
                ..EngineConfig::default()
            };
            let engine = match self.options.storage {
                OverlayStorage::Memory => IndexEngine::in_memory(config)?,
                OverlayStorage::Disk => IndexEngine::open(config)?,
            };
            for (path, doc) in &next {
                if let Some(doc) = doc {
                    engine.writer().add_document(doc.clone())?;
                } else {
                    engine.writer().delete_document(&DocKey {
                        repo_id: self.repo.clone(),
                        logical_path: path.clone(),
                    })?;
                }
            }
            engine.writer().commit()?;
            let snapshot = engine.reader().open_committed()?;
            *published
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Published {
                overlay: Some(snapshot),
            };
            self.dirty = next;
            let mut s = status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.publications += 1;
            s.dirty_paths = self.dirty.len();
            s.dirty_sample = self.dirty.keys().take(16).cloned().collect();
            s.content_bytes = self
                .dirty
                .values()
                .flatten()
                .map(|d| d.source_bytes.len())
                .sum();
            s.last_publish_ms = start.elapsed().as_secs_f64() * 1000.;
        }
        for path in observed_keys {
            self.observed.remove(&path);
        }
        self.observed.extend(files);
        let mut s = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_discovery_ms = discovery_ms;
        s.last_error = None;
        if full {
            s.reconciliations += 1;
        }
        Ok(())
    }
}
impl WatchWorkspace {
    pub fn open(mut options: Options) -> Result<Self> {
        options.build = options.build.normalized()?;
        let root = options.build.repo_path.clone();
        if root.starts_with(&options.build.storage_root) {
            return Err(error("experiment storage must not contain the workspace"));
        }
        fs::create_dir_all(&options.build.storage_root)?;
        // Both the filesystem base and overlay belong to this experiment run.
        // Refuse reuse, including memory runs that left no overlay artifacts.
        if fs::read_dir(&options.build.storage_root)?
            .next()
            .transpose()?
            .is_some()
        {
            return Err(error(
                "use a fresh experiment storage directory for each run",
            ));
        }
        let (git_watches, git_events) = if options.git_base_storage.is_some() {
            git_metadata_paths(&options.build)?
        } else {
            (Vec::new(), Vec::new())
        };
        let event_root = root.clone();
        if options.event_capacity == 0 {
            return Err(error("event capacity must be positive"));
        }
        let (tx, rx) = mpsc::sync_channel(options.event_capacity);
        let status = Arc::new(Mutex::new(Status::default()));
        let event_status = status.clone();
        let pending = Arc::new(AtomicUsize::new(0));
        let reconcile = Arc::new(AtomicBool::new(false));
        let p = pending.clone();
        let rec = reconcile.clone();
        let storage = options.build.storage_root.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    if event.need_rescan() {
                        event_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .os_rescan_requests += 1;
                        rec.store(true, Ordering::SeqCst);
                    }
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        return;
                    }
                    if event.paths.iter().any(|p| {
                        git_events.iter().any(|g| {
                            p == g
                                || (g.file_name().is_some_and(|n| n == "refs") && p.starts_with(g))
                        })
                    }) {
                        event_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .git_rescan_requests += 1;
                        rec.store(true, Ordering::SeqCst);
                    }

                    let paths: Vec<_> = event
                        .paths
                        .into_iter()
                        .filter(|p| {
                            p.starts_with(&event_root)
                                && !p.starts_with(&storage)
                                && !is_workspace_internal_path(p)
                                && !p.components().any(|c| c.as_os_str() == ".git")
                        })
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    p.fetch_add(1, Ordering::SeqCst);
                    if tx.try_send(paths).is_err() {
                        event_status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .queue_drops += 1;
                        p.fetch_sub(1, Ordering::SeqCst);
                        rec.store(true, Ordering::SeqCst);
                    }
                }
                Err(_) => {
                    event_status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .watcher_errors += 1;
                    rec.store(true, Ordering::SeqCst);
                }
            })
            .map_err(error)?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(error)?;
        for path in git_watches {
            let mode = if path.file_name().is_some_and(|p| p == "refs") {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            watcher.watch(&path, mode).map_err(error)?;
        }

        let files = inventory(&options.build, std::slice::from_ref(&root))?;
        let (base, head) = if let Some(storage) = &options.git_base_storage {
            let mut config = options.build.clone();
            config.storage_root = storage.clone();
            let meta = WorkspaceBaseSnapshotIndex::read_snapshot_metadata(&config)?
                .ok_or_else(|| error("Git base missing"))?;
            let fingerprint = format!(
                "{}-",
                crate::git_snapshot::build_config_fingerprint(&config)
            );
            if meta.repo_root.as_deref() != Some(root.to_string_lossy().as_ref())
                || !meta
                    .config_fingerprint
                    .as_deref()
                    .is_some_and(|f| f.starts_with(&fingerprint))
            {
                return Err(error(
                    "Git base workspace or indexing configuration does not match",
                ));
            }

            (
                WorkspaceBaseSnapshotIndex::open_read_only(&config)?.open_committed()?,
                meta.head_commit,
            )
        } else {
            let engine = IndexEngine::open(EngineConfig {
                index_root: options.build.storage_root.join("base"),
                tokenizer: (&options.build).into(),
                ..EngineConfig::default()
            })?;
            let repo = RepoId(root.to_string_lossy().into_owned());
            for (i, path) in files.keys().enumerate() {
                if let Some(doc) = document(&options.build, &repo, path, false)? {
                    engine.writer().add_document(doc)?;
                }
                if i % 512 == 511 {
                    engine.writer().commit()?;
                }
            }
            engine.writer().commit()?;
            (engine.reader().open_committed()?, None)
        };
        if options.discovery == Discovery::Git && head.is_none() {
            return Err(error("Git discovery requires --git-base-storage"));
        }
        let repo = base
            .visible_keys()?
            .live
            .iter()
            .next()
            .map(|k| k.repo_id.clone())
            .unwrap_or_else(|| RepoId(root.to_string_lossy().into_owned()));
        let catalog = base.path_catalog()?;
        let observed = catalog
            .iter()
            .map(|(p, e)| {
                (
                    p.to_string(),
                    Stamp {
                        size: e.size,
                        mtime: e.modified_at,
                    },
                )
            })
            .collect();
        let mut updater = Updater {
            options: options.clone(),
            base: catalog,
            repo,
            head,
            tracked: BTreeMap::new(),
            observed,
            dirty: BTreeMap::new(),
            generation: 0,
        };
        let published = Arc::new(RwLock::new(Published { overlay: None }));
        // Startup is always reconciled, including changes made while offline.
        // Git can reconcile startup against its immutable base without hashing
        // every unchanged file. Watcher mode still uses direct event paths later.
        if updater.head.is_some() {
            updater.options.discovery = Discovery::Git;
        }
        updater.update(std::slice::from_ref(&root), true, &published, &status)?;
        updater.options.discovery = options.discovery;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_worker = stop.clone();
        let pub_worker = published.clone();
        let stat = status.clone();
        let pend = pending.clone();
        let recon = reconcile.clone();
        let worker = thread::spawn(move || {
            let mut last_full = Instant::now();
            while !stop_worker.load(Ordering::SeqCst) {
                let mut scopes = Vec::new();
                let mut consumed = 0;
                if let Ok(paths) = rx.recv_timeout(Duration::from_millis(50)) {
                    scopes.extend(paths);
                    consumed += 1;
                    let until = Instant::now() + Duration::from_millis(20);
                    while Instant::now() < until {
                        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                            Ok(paths) => {
                                scopes.extend(paths);
                                consumed += 1;
                            }
                            Err(_) => break,
                        }
                    }
                }
                let full = recon.swap(false, Ordering::SeqCst)
                    || last_full.elapsed() >= options.reconcile_interval
                    || scopes.iter().any(|p| {
                        matches!(
                            p.file_name().and_then(|s| s.to_str()),
                            Some(".gitignore" | ".ignore")
                        )
                    });
                if full {
                    scopes = vec![root.clone()];
                }
                if scopes.is_empty() {
                    continue;
                }
                scopes.sort();
                scopes.dedup();
                let mut pruned = Vec::new();
                for scope in scopes {
                    if !pruned.iter().any(|p: &PathBuf| scope.starts_with(p)) {
                        pruned.push(scope);
                    }
                }
                stat.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .busy = true;
                if let Err(e) = updater.update(&pruned, full, &pub_worker, &stat) {
                    stat.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .last_error = Some(e.to_string());
                    recon.store(true, Ordering::SeqCst);
                }
                if full {
                    last_full = Instant::now();
                }
                pend.fetch_sub(consumed, Ordering::SeqCst);
                stat.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .busy = false;
            }
        });
        Ok(Self {
            root: options.build.repo_path.clone(),
            base,
            view: WorkspaceSearchView::open(options.build)?,
            published,
            status,
            pending,
            reconcile,
            stop,
            worker: Some(worker),
            _watcher: watcher,
        })
    }
    pub fn search(&self, pattern: &str) -> Result<Vec<String>> {
        self.search_with_glob(pattern, None)
    }
    pub fn search_with_glob(&self, pattern: &str, glob: Option<&str>) -> Result<Vec<String>> {
        Ok(self
            .search_query(
                &QueryConfig {
                    regex_pattern: pattern.into(),
                    patterns: vec![pattern.into()],
                    fixed_strings: true,
                    search_mode: SearchMode::FilesWithMatches,
                    ..QueryConfig::default()
                },
                glob,
            )?
            .matched_paths)
    }
    /// Experimental query surface for comparing storage with identical queries.
    pub fn search_query(
        &self,
        query: &QueryConfig,
        glob: Option<&str>,
    ) -> Result<crate::SearchResults> {
        let filter = glob
            .map(|glob| {
                crate::path_filter::PathFilter::new(
                    crate::path_filter::PathFilterArgs {
                        globs: vec![glob.into()],
                        ..Default::default()
                    },
                    &self.root,
                )
            })
            .transpose()?;
        let overlay = self
            .published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .overlay
            .clone();
        self.view.search_workspace_snapshots(
            query,
            &QueryFilters::default(),
            filter.as_ref(),
            Some(&self.base),
            overlay.as_ref(),
        )
    }
    pub fn status(&self) -> Status {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        status.pending = self.pending.load(Ordering::SeqCst)
            + usize::from(self.reconcile.load(Ordering::SeqCst));
        status
    }
    pub fn reconcile(&self) {
        self.reconcile.store(true, Ordering::SeqCst);
    }
}
impl Drop for WatchWorkspace {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(ws: &WatchWorkspace, token: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = ws.status();
            if ws.search(token).expect("query").len() == count
                && s.pending == 0
                && !s.busy
                && s.last_error.is_none()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{token}: expected {count}, status {}",
                serde_json::to_string(&s).expect("status")
            );
            thread::sleep(Duration::from_millis(30));
        }
    }
    #[test]
    fn plain_directory_watcher_converges_across_restore_rename_ignore_and_binary() {
        for storage in [OverlayStorage::Memory, OverlayStorage::Disk] {
            let temp = tempfile::tempdir().expect("temp");
            let root = temp.path().join("repo");
            fs::create_dir(&root).expect("dir");
            let a = root.join("a.cc");
            fs::write(&a, "OLD_TOKEN").expect("base");
            fs::write(root.join(".ignore"), "ignored/\n").expect("ignore");
            let ws = WatchWorkspace::open(Options {
                build: BuildConfig {
                    repo_path: root.clone(),
                    storage_root: temp.path().join("index"),
                    ..BuildConfig::default()
                },
                storage,
                discovery: Discovery::Watcher,
                git_base_storage: None,
                reconcile_interval: Duration::from_secs(60),
                event_capacity: 1024,
            })
            .expect("open plain workspace");
            fs::remove_file(&a).expect("delete");
            wait(&ws, "OLD_TOKEN", 0);
            fs::write(&a, "OLD_TOKEN").expect("restore");
            wait(&ws, "OLD_TOKEN", 1);
            fs::write(root.join("b.cc"), "NEW_TOKEN").expect("new");
            wait(&ws, "NEW_TOKEN", 1);
            wait(&ws, "OLD_TOKEN", 1);
            fs::create_dir(root.join("nested")).expect("nested");
            fs::rename(&a, root.join("nested/a.cc")).expect("move");
            wait(&ws, "OLD_TOKEN", 1);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ws
                .search("OLD_TOKEN")
                .expect("search")
                .iter()
                .any(|p| p.ends_with("nested/a.cc"))
            {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(30));
            }
            fs::create_dir(root.join("ignored")).expect("ignored dir");
            fs::write(root.join("ignored/c.cc"), "NEW_TOKEN").expect("ignored file");
            // Explicit reconciliation tests the same ignore rules as event discovery.
            ws.reconcile();
            wait(&ws, "NEW_TOKEN", 1);
            fs::write(root.join(".ignore"), "").expect("unignore");
            wait(&ws, "NEW_TOKEN", 2);
            fs::write(root.join("b.cc"), b"NEW_TOKEN\0").expect("binary");
            wait(&ws, "NEW_TOKEN", 1);
            fs::write(root.join("b.cc"), "NEW_TOKEN").expect("text again");
            wait(&ws, "NEW_TOKEN", 2);
        }
    }
    #[test]
    fn query_surface_returns_regex_lines_from_base_and_overlay_concurrently() {
        for storage in [OverlayStorage::Memory, OverlayStorage::Disk] {
            let temp = tempfile::tempdir().expect("temp");
            let root = temp.path().join("repo");
            fs::create_dir(&root).expect("dir");
            fs::write(root.join("base.cc"), "base token_123\n").expect("base");
            let ws = WatchWorkspace::open(Options {
                build: BuildConfig {
                    repo_path: root.clone(),
                    storage_root: temp.path().join("index"),
                    ..Default::default()
                },
                storage,
                discovery: Discovery::Watcher,
                git_base_storage: None,
                reconcile_interval: Duration::from_secs(60),
                event_capacity: 16384,
            })
            .expect("workspace");
            fs::write(root.join("new.cc"), "first\nnew token_456\n").expect("new");
            wait(&ws, "token_456", 1);
            let query = QueryConfig {
                regex_pattern: "token_[0-9]+".into(),
                patterns: vec!["token_[0-9]+".into()],
                fixed_strings: false,
                search_mode: SearchMode::LineMatches,
                ..Default::default()
            };
            thread::scope(|scope| {
                for _ in 0..4 {
                    let ws = &ws;
                    let query = &query;
                    scope.spawn(move || {
                        for _ in 0..4 {
                            let result = ws.search_query(query, Some("*.cc")).expect("regex");
                            let mut hits: Vec<_> = result
                                .flattened_line_matches()
                                .into_iter()
                                .map(|m| {
                                    (
                                        PathBuf::from(m.path).file_name().unwrap().to_owned(),
                                        m.line_number,
                                    )
                                })
                                .collect();
                            hits.sort();
                            assert_eq!(hits, vec![("base.cc".into(), 1), ("new.cc".into(), 2)]);
                        }
                    });
                }
            });
        }
    }
    #[test]
    fn git_base_reconciles_offline_ignore_changes_and_branch_file_changes() {
        for discovery in [Discovery::Watcher, Discovery::Git] {
            let temp = tempfile::tempdir().expect("temp");
            let root = temp.path().join("repo");
            fs::create_dir(&root).expect("dir");
            let git = |args: &[&str]| {
                let out = Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .output()
                    .expect("git");
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
            };
            git(&["init", "-q"]);
            git(&["config", "user.name", "Experiment"]);
            git(&["config", "user.email", "experiment@localhost"]);
            fs::write(root.join("a.cc"), "OLD_TOKEN").expect("base file");
            git(&["add", "."]);
            git(&["commit", "-qm", "base"]);
            let base_config = BuildConfig {
                repo_path: root.clone(),
                storage_root: temp.path().join("base"),
                ..BuildConfig::default()
            }
            .normalized()
            .expect("config");
            WorkspaceBaseSnapshotIndex::build_with_options(
                &base_config,
                &RepoId(base_config.repo_path.to_string_lossy().into_owned()),
                crate::base_snapshot_build::BaseSnapshotBuildOptions::new().requiring_git_head(),
            )
            .expect("build Git base");
            // Tracked paths keep their ignore exemption; an ignored untracked
            // file becomes searchable when only the Git index changes.
            fs::write(root.join(".ignore"), "a.cc\nb.cc\n").expect("offline ignore");
            fs::write(root.join("b.cc"), "NEW_TOKEN").expect("ignored untracked file");
            let ws = WatchWorkspace::open(Options {
                build: BuildConfig {
                    repo_path: root.clone(),
                    storage_root: temp.path().join("experiment"),
                    ..BuildConfig::default()
                },
                storage: OverlayStorage::Memory,
                discovery,
                git_base_storage: Some(base_config.storage_root),
                reconcile_interval: Duration::from_secs(60),
                event_capacity: 1024,
            })
            .expect("open");
            wait(&ws, "OLD_TOKEN", 1);
            wait(&ws, "NEW_TOKEN", 0);
            git(&["add", "-f", "b.cc"]);
            // Only the Git index changed; no corpus write should be required.
            wait(&ws, "NEW_TOKEN", 1);
            fs::write(root.join(".ignore"), "").expect("unignore");
            wait(&ws, "OLD_TOKEN", 1);
            fs::write(root.join("b.cc"), "NEW_TOKEN").expect("branch file");
            git(&["add", "."]);
            git(&["commit", "-qm", "branch change"]);
            wait(&ws, "NEW_TOKEN", 1);
            git(&["checkout", "-q", "HEAD~1"]);
            wait(&ws, "NEW_TOKEN", 0);
            wait(&ws, "OLD_TOKEN", 1);
        }
    }

    #[test]
    fn ignore_whitelist_does_not_override_hidden_file_policy() {
        let temp = tempfile::tempdir().expect("temp");
        fs::write(temp.path().join(".ignore"), "!.secret.cc\n").expect("whitelist");
        fs::write(temp.path().join(".secret.cc"), "SECRET_TOKEN").expect("hidden file");
        let config = BuildConfig {
            repo_path: temp.path().to_path_buf(),
            storage_root: temp.path().join("index"),
            ..BuildConfig::default()
        };
        assert!(inventory(&config, &[config.repo_path.clone()])
            .expect("inventory")
            .is_empty());
    }

    #[test]
    fn file_event_inventory_does_not_visit_sibling_files() {
        let temp = tempfile::tempdir().expect("temp");
        fs::write(temp.path().join("a.cc"), "A").expect("a");
        fs::write(temp.path().join("b.cc"), "B").expect("b");
        fs::write(temp.path().join("large.cc"), "01234567890123456789").expect("large");
        let config = BuildConfig {
            repo_path: temp.path().to_path_buf(),
            storage_root: temp.path().join("index"),
            max_file_size: 16,
            ..BuildConfig::default()
        };
        let files = inventory(&config, &[config.repo_path.join("a.cc")]).expect("scoped inventory");
        assert_eq!(files.keys().cloned().collect::<Vec<_>>(), vec!["a.cc"]);
        assert_eq!(
            inventory(&config, &[config.repo_path.clone()])
                .expect("full inventory")
                .len(),
            2
        );
    }

    #[test]
    fn catalog_scope_uses_directory_boundaries() {
        let map: BTreeMap<_, _> = ["src/a.cc", "src/deep/b.cc", "src2/c.cc", "src.cc"]
            .into_iter()
            .map(|p| (p.to_string(), ()))
            .collect();
        let actual = scoped_keys(&map, Path::new("/repo"), &[PathBuf::from("/repo/src")]);
        assert_eq!(
            actual,
            BTreeSet::from(["src/a.cc".to_string(), "src/deep/b.cc".to_string()])
        );
        assert_eq!(
            scoped_keys(&map, Path::new("/repo"), &[PathBuf::from("/repo/src.cc")]),
            BTreeSet::from(["src.cc".to_string()])
        );
    }

    #[test]
    fn periodic_reconciliation_repairs_missed_notifications() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("repo");
        fs::create_dir(&root).expect("dir");
        fs::write(root.join("a.cc"), "OLD_TOKEN").expect("base");
        let mut ws = WatchWorkspace::open(Options {
            build: BuildConfig {
                repo_path: root.clone(),
                storage_root: temp.path().join("index"),
                ..BuildConfig::default()
            },
            storage: OverlayStorage::Memory,
            discovery: Discovery::Watcher,
            git_base_storage: None,
            reconcile_interval: Duration::from_secs(1),
            event_capacity: 1024,
        })
        .expect("open");
        ws._watcher
            .unwatch(&fs::canonicalize(&root).expect("root"))
            .expect("disable notifications");
        fs::write(root.join("b.cc"), "NEW_TOKEN").expect("missed add");
        wait(&ws, "NEW_TOKEN", 1);
        fs::remove_file(root.join("a.cc")).expect("missed deletion");
        wait(&ws, "OLD_TOKEN", 0);
    }
}
