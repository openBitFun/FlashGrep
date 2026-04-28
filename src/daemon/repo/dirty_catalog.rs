use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    thread,
    time::Duration,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{error::Result, worktree_diff::DirtyPathKind};

use super::{DirtyPathSet, DirtyStats, RepoRuntime};

const DIRTY_CATALOG_VERSION: u32 = 1;
const DIRTY_CATALOG_PERSIST_RETRY_ATTEMPTS: usize = 20;
const DIRTY_CATALOG_PERSIST_RETRY_DELAY_MS: u64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DirtyCatalogState {
    Dirty,
    Building,
    ReadyOverlay,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ObservedFileVersion {
    exists: bool,
    size: Option<u64>,
    modified_at_nanos: Option<u64>,
}

#[derive(Debug, Clone)]
pub(super) struct DirtyCatalogEntry {
    pub(super) kind: DirtyPathKind,
    pub(super) observed_version: ObservedFileVersion,
    pub(super) state: DirtyCatalogState,
    pub(super) last_attempt_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct DirtyCatalog {
    entries: BTreeMap<String, DirtyCatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirtyCatalogFile {
    version: u32,
    entries: Vec<DirtyCatalogFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirtyCatalogFileEntry {
    path: String,
    kind: String,
    observed_version: ObservedFileVersion,
    state: DirtyCatalogState,
    last_attempt_at_ms: Option<u64>,
}

impl DirtyCatalog {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let parsed = match serde_json::from_slice::<DirtyCatalogFile>(&bytes) {
            Ok(parsed) => parsed,
            Err(_) => {
                quarantine_corrupt_catalog(path)?;
                return Ok(Self::default());
            }
        };
        if parsed.version != DIRTY_CATALOG_VERSION {
            quarantine_corrupt_catalog(path)?;
            return Ok(Self::default());
        }

        let mut entries = BTreeMap::new();
        for entry in parsed.entries {
            let Some(kind) = dirty_kind_from_str(&entry.kind) else {
                continue;
            };
            entries.insert(
                entry.path,
                DirtyCatalogEntry {
                    kind,
                    observed_version: entry.observed_version,
                    state: entry.state,
                    last_attempt_at: entry
                        .last_attempt_at_ms
                        .map(|millis| UNIX_EPOCH + std::time::Duration::from_millis(millis)),
                },
            );
        }
        Ok(Self { entries })
    }

    pub(super) fn persist(&self, path: &Path) -> Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let encoded = serde_json::to_vec_pretty(&DirtyCatalogFile {
            version: DIRTY_CATALOG_VERSION,
            entries: self
                .entries
                .iter()
                .map(|(path, entry)| DirtyCatalogFileEntry {
                    path: path.clone(),
                    kind: dirty_kind_to_str(entry.kind).to_string(),
                    observed_version: entry.observed_version.clone(),
                    state: entry.state,
                    last_attempt_at_ms: entry.last_attempt_at.and_then(system_time_millis),
                })
                .collect(),
        })
        .map_err(|error| {
            crate::error::AppError::InvalidIndex(format!(
                "failed to encode dirty catalog at {}: {error}",
                path.display()
            ))
        })?;
        let tmp_path = path.with_extension("tmp");
        persist_dirty_catalog_bytes_with_retry(parent, path, &tmp_path, &encoded)
    }

    pub(super) fn recover_startup(&mut self) {
        for entry in self.entries.values_mut() {
            if matches!(
                entry.state,
                DirtyCatalogState::Building | DirtyCatalogState::Failed
            ) {
                entry.state = DirtyCatalogState::Dirty;
            }
        }
    }

    pub(super) fn reconcile_with_dirty_set(&mut self, dirty: &DirtyPathSet) -> Result<()> {
        let active_paths = dirty.all_paths();
        self.entries
            .retain(|path, _| active_paths.contains(path.as_str()));

        for (path, kind) in dirty.iter_paths() {
            let observed_version = observe_file_version(&path)?;
            match self.entries.get_mut(&path) {
                Some(entry)
                    if entry.kind == kind
                        && entry.state == DirtyCatalogState::ReadyOverlay
                        && entry.observed_version == observed_version => {}
                Some(entry) => {
                    entry.kind = kind;
                    entry.observed_version = observed_version;
                    entry.state = DirtyCatalogState::Dirty;
                }
                None => {
                    self.entries.insert(
                        path,
                        DirtyCatalogEntry {
                            kind,
                            observed_version,
                            state: DirtyCatalogState::Dirty,
                            last_attempt_at: None,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    pub(super) fn visible_dirty_paths(&self) -> DirtyPathSet {
        let mut dirty = DirtyPathSet::default();
        for (path, entry) in &self.entries {
            match entry.kind {
                DirtyPathKind::Modified => {
                    dirty.modified.insert(path.clone());
                }
                DirtyPathKind::Deleted => {
                    dirty.deleted.insert(path.clone());
                }
                DirtyPathKind::New => {
                    dirty.new.insert(path.clone());
                }
            }
        }
        dirty
    }

    pub(super) fn pending_dirty_paths(&self) -> DirtyPathSet {
        let mut dirty = DirtyPathSet::default();
        for (path, entry) in &self.entries {
            if entry.state != DirtyCatalogState::ReadyOverlay {
                match entry.kind {
                    DirtyPathKind::Modified => {
                        dirty.modified.insert(path.clone());
                    }
                    DirtyPathKind::Deleted => {
                        dirty.deleted.insert(path.clone());
                    }
                    DirtyPathKind::New => {
                        dirty.new.insert(path.clone());
                    }
                }
            }
        }
        dirty
    }

    pub(super) fn dirty_stats(&self) -> DirtyStats {
        self.visible_dirty_paths().stats()
    }

    pub(super) fn has_pending_entries(&self) -> bool {
        self.entries
            .values()
            .any(|entry| entry.state != DirtyCatalogState::ReadyOverlay)
    }

    pub(super) fn mark_building(&mut self, paths: &DirtyPathSet) {
        self.mark_paths(paths, DirtyCatalogState::Building);
    }

    pub(super) fn mark_ready(&mut self, paths: &DirtyPathSet) {
        self.mark_paths(paths, DirtyCatalogState::ReadyOverlay);
    }

    pub(super) fn mark_failed(&mut self, paths: &DirtyPathSet) {
        self.mark_paths(paths, DirtyCatalogState::Failed);
    }

    fn mark_paths(&mut self, paths: &DirtyPathSet, state: DirtyCatalogState) {
        let now = Some(SystemTime::now());
        for path in paths.all_paths() {
            if let Some(entry) = self.entries.get_mut(path) {
                entry.state = state;
                entry.last_attempt_at = now;
            }
        }
    }
}

fn persist_dirty_catalog_bytes_with_retry(
    parent: &Path,
    path: &Path,
    tmp_path: &Path,
    encoded: &[u8],
) -> Result<()> {
    for attempt in 0..DIRTY_CATALOG_PERSIST_RETRY_ATTEMPTS {
        match persist_dirty_catalog_bytes(parent, path, tmp_path, encoded) {
            Ok(()) => return Ok(()),
            Err(error) if is_retryable_dirty_catalog_persist_error(&error) => {
                if attempt + 1 == DIRTY_CATALOG_PERSIST_RETRY_ATTEMPTS {
                    return Err(error.into());
                }
                let _ = fs::remove_file(tmp_path);
                thread::sleep(Duration::from_millis(
                    DIRTY_CATALOG_PERSIST_RETRY_DELAY_MS,
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn persist_dirty_catalog_bytes(
    parent: &Path,
    path: &Path,
    tmp_path: &Path,
    encoded: &[u8],
) -> std::io::Result<()> {
    fs::create_dir_all(parent)?;
    fs::write(tmp_path, encoded)?;
    fs::rename(tmp_path, path)
}

fn is_retryable_dirty_catalog_persist_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::NotFound | ErrorKind::AlreadyExists
    ) || error.raw_os_error() == Some(5)
        || error.raw_os_error() == Some(32)
}

pub(super) fn dirty_catalog_path(storage_root: &Path) -> PathBuf {
    storage_root.join("dirty-catalog.json")
}

impl RepoRuntime {
    pub(super) fn recover_dirty_catalog_for_startup(&self) -> Result<()> {
        let path = dirty_catalog_path(&self.build_config.storage_root);
        let mut catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.recover_startup();
        catalog.persist(&path)
    }

    pub(super) fn reconcile_dirty_catalog(&self, dirty: &DirtyPathSet) -> Result<()> {
        let path = dirty_catalog_path(&self.build_config.storage_root);
        let mut catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.reconcile_with_dirty_set(dirty)?;
        catalog.persist(&path)
    }

    pub(super) fn mark_dirty_catalog_building(&self, paths: &DirtyPathSet) -> Result<()> {
        self.update_dirty_catalog_state(paths, DirtyCatalogState::Building)
    }

    pub(super) fn mark_dirty_catalog_ready(&self, paths: &DirtyPathSet) -> Result<()> {
        self.update_dirty_catalog_state(paths, DirtyCatalogState::ReadyOverlay)
    }

    pub(super) fn mark_dirty_catalog_failed(&self, paths: &DirtyPathSet) -> Result<()> {
        self.update_dirty_catalog_state(paths, DirtyCatalogState::Failed)
    }

    pub(super) fn dirty_catalog_visible_paths(&self) -> DirtyPathSet {
        let catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.visible_dirty_paths()
    }

    pub(super) fn dirty_catalog_stats(&self) -> DirtyStats {
        let catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.dirty_stats()
    }

    pub(super) fn dirty_catalog_pending_paths(&self) -> DirtyPathSet {
        let catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.pending_dirty_paths()
    }

    pub(super) fn dirty_catalog_has_pending(&self) -> bool {
        let catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        catalog.has_pending_entries()
    }

    fn update_dirty_catalog_state(
        &self,
        paths: &DirtyPathSet,
        state: DirtyCatalogState,
    ) -> Result<()> {
        let path = dirty_catalog_path(&self.build_config.storage_root);
        let mut catalog = match self.dirty_catalog.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match state {
            DirtyCatalogState::Building => catalog.mark_building(paths),
            DirtyCatalogState::ReadyOverlay => catalog.mark_ready(paths),
            DirtyCatalogState::Failed => catalog.mark_failed(paths),
            DirtyCatalogState::Dirty => {}
        }
        catalog.persist(&path)
    }
}

fn quarantine_corrupt_catalog(path: &Path) -> Result<()> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let quarantine = path.with_extension(format!("corrupt-{suffix}.json"));
    match fs::rename(path, quarantine) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn dirty_kind_to_str(kind: DirtyPathKind) -> &'static str {
    match kind {
        DirtyPathKind::Modified => "modified",
        DirtyPathKind::Deleted => "deleted",
        DirtyPathKind::New => "new",
    }
}

fn dirty_kind_from_str(value: &str) -> Option<DirtyPathKind> {
    match value {
        "modified" => Some(DirtyPathKind::Modified),
        "deleted" => Some(DirtyPathKind::Deleted),
        "new" => Some(DirtyPathKind::New),
        _ => None,
    }
}

fn observe_file_version(path: &str) -> Result<ObservedFileVersion> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(ObservedFileVersion {
            exists: true,
            size: Some(metadata.len()),
            modified_at_nanos: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos().try_into().unwrap_or(u64::MAX)),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(ObservedFileVersion {
            exists: false,
            size: None,
            modified_at_nanos: None,
        }),
        Err(error) => Err(error.into()),
    }
}

fn system_time_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

trait DirtyPathSetExt {
    fn all_paths(&self) -> HashSet<&str>;
    fn iter_paths(&self) -> Vec<(String, DirtyPathKind)>;
}

impl DirtyPathSetExt for DirtyPathSet {
    fn all_paths(&self) -> HashSet<&str> {
        self.modified
            .iter()
            .chain(self.deleted.iter())
            .chain(self.new.iter())
            .map(String::as_str)
            .collect()
    }

    fn iter_paths(&self) -> Vec<(String, DirtyPathKind)> {
        self.modified
            .iter()
            .map(|path| (path.clone(), DirtyPathKind::Modified))
            .chain(
                self.deleted
                    .iter()
                    .map(|path| (path.clone(), DirtyPathKind::Deleted)),
            )
            .chain(
                self.new
                    .iter()
                    .map(|path| (path.clone(), DirtyPathKind::New)),
            )
            .collect()
    }
}
