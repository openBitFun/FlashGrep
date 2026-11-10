//! Worktree fingerprints are independent of immutable base publications. A cached
//! content observation can be reused across commits, but never across file identities.
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct FileStamp(u64, u64, u64, i64, i64, i64, i64);
impl FileStamp {
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self(
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Observation(FileStamp, u32, bool);
#[derive(Debug, Default)]
pub(super) struct WorktreeObservations {
    loaded: bool,
    dirty: bool,
    entries: HashMap<String, Observation>,
}
#[derive(Serialize, Deserialize)]
struct StoredObservations {
    version: u32,
    root: PathBuf,
    entries: HashMap<String, Observation>,
}
impl WorktreeObservations {
    pub(super) fn load(&mut self, storage: &Path, root: &Path) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        // This is a disposable accelerator, not index or publication authority.
        // Missing, incompatible or corrupt observations require a content read.
        if let Ok(bytes) = fs::read(storage.join("worktree-observations.json")) {
            if let Ok(stored) = serde_json::from_slice::<StoredObservations>(&bytes) {
                if stored.version == 1 && stored.root == root {
                    self.entries = stored.entries;
                }
            }
        }
    }

    pub(super) fn inspect(
        &self,
        logical: &str,
        path: &Path,
        metadata: &fs::Metadata,
        force: bool,
        max_size: u64,
    ) -> Result<(Option<[u8; 32]>, Option<Observation>)> {
        let stamp = FileStamp::from_metadata(metadata);
        if !force {
            if let Some(observation) = self.entries.get(logical) {
                if stamp == Some(observation.0) {
                    return Ok((content_hash(observation.1, observation.2), None));
                }
            }
        }
        // One read serves both corpus eligibility and content comparison.
        let file = fs::File::open(path)?;
        let mut bytes = Vec::with_capacity(metadata.len().min(max_size) as usize + 1);
        (&file)
            .take(max_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        let after = FileStamp::from_metadata(&file.metadata()?);
        if stamp != after || bytes.len() as u64 != metadata.len() {
            return Err(AppError::InvalidIndex(format!(
                "file changed during workspace observation: {}",
                path.display()
            )));
        }
        let text = bytes.len() as u64 <= max_size && !bytes[..bytes.len().min(8192)].contains(&0);
        let crc = if text { crc32fast::hash(&bytes) } else { 0 };
        Ok((
            content_hash(crc, text),
            stamp.map(|stamp| Observation(stamp, crc, text)),
        ))
    }

    pub(super) fn record(&mut self, logical: String, observation: Observation) {
        if self.entries.get(&logical) != Some(&observation) {
            self.entries.insert(logical, observation);
            self.dirty = true;
        }
    }

    #[cfg(test)]
    fn content(
        &mut self,
        logical: &str,
        path: &Path,
        metadata: &fs::Metadata,
        force: bool,
        max_size: u64,
    ) -> Result<Option<[u8; 32]>> {
        let (hash, observation) = self.inspect(logical, path, metadata, force, max_size)?;
        if let Some(observation) = observation {
            self.record(logical.to_owned(), observation);
        }
        Ok(hash)
    }

    pub(super) fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        let before = self.entries.len();
        self.entries.retain(|path, _| keep(path));
        self.dirty |= self.entries.len() != before;
    }

    pub(super) fn persist(&mut self, storage: &Path, root: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        #[derive(Serialize)]
        struct Borrowed<'a> {
            version: u32,
            root: &'a Path,
            entries: &'a HashMap<String, Observation>,
        }
        let encoded = serde_json::to_vec(&Borrowed {
            version: 1,
            root,
            entries: &self.entries,
        })
        .map_err(|error| AppError::InvalidIndex(error.to_string()))?;
        fs::create_dir_all(storage)?;
        let target = storage.join("worktree-observations.json");
        let temporary = storage.join("worktree-observations.tmp");
        fs::write(&temporary, encoded)?;
        crate::fs_atomic::replace_file(&temporary, &target)?;
        self.dirty = false;
        Ok(())
    }
}
fn content_hash(crc: u32, text: bool) -> Option<[u8; 32]> {
    text.then(|| {
        let mut hash = [0; 32];
        hash[..4].copy_from_slice(&crc.to_le_bytes());
        hash
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn persisted_observations_detect_same_size_rewrite_with_restored_mtime() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let mut cache = WorktreeObservations::default();
        cache.load(storage.path(), root.path());
        let original = cache
            .content("file.txt", &path, &metadata, false, 1024)
            .unwrap();
        cache.persist(storage.path(), root.path()).unwrap();
        fs::write(&path, "after!\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        let changed = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), changed.len());
        assert_eq!(metadata.modified().unwrap(), changed.modified().unwrap());
        let mut reopened = WorktreeObservations::default();
        reopened.load(storage.path(), root.path());
        let actual = reopened
            .content("file.txt", &path, &changed, false, 1024)
            .unwrap();
        assert_ne!(original, actual);
        assert_eq!(actual, content_hash(crc32fast::hash(b"after!\n"), true));
    }
    #[test]
    fn changed_during_observation_is_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let stale = fs::metadata(&path).unwrap();
        fs::write(&path, "after!\n").unwrap();
        let mut cache = WorktreeObservations::default();
        assert!(cache
            .content("file.txt", &path, &stale, false, 1024)
            .is_err());
        assert!(cache.entries.is_empty());
    }
    #[test]
    fn force_observation_bypasses_cache_and_binary_transition_is_visible() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, "text\n").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let mut cache = WorktreeObservations::default();
        // Simulate a watcher detecting a write hidden by identical filesystem metadata.
        cache.entries.insert(
            "file.txt".into(),
            Observation(FileStamp::from_metadata(&metadata).unwrap(), 123, false),
        );
        assert!(cache
            .content("file.txt", &path, &metadata, true, 1024)
            .unwrap()
            .is_some());
        fs::write(&path, b"\0ext\n").unwrap();
        assert!(cache
            .content(
                "file.txt",
                &path,
                &fs::metadata(&path).unwrap(),
                false,
                1024
            )
            .unwrap()
            .is_none());
    }
    #[test]
    fn checkpoint_retries_failure_and_skips_unchanged_observations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, "data").unwrap();
        let storage = root.path().join("storage");
        fs::write(&storage, "block directory creation").unwrap();
        let mut cache = WorktreeObservations::default();
        let metadata = fs::metadata(&path).unwrap();
        cache
            .content("file.txt", &path, &metadata, false, 1024)
            .unwrap();
        assert!(cache.persist(&storage, root.path()).is_err());
        assert!(cache.dirty);
        fs::remove_file(&storage).unwrap();
        cache.persist(&storage, root.path()).unwrap();
        assert!(!cache.dirty);
        cache
            .content("file.txt", &path, &metadata, true, 1024)
            .unwrap();
        cache.retain(|_| true);
        assert!(!cache.dirty);
        let checkpoint = storage.join("worktree-observations.json");
        let modified = fs::metadata(&checkpoint).unwrap().modified().unwrap();
        cache.persist(&storage, root.path()).unwrap();
        assert_eq!(
            fs::metadata(&checkpoint).unwrap().modified().unwrap(),
            modified
        );
        cache.retain(|_| false);
        assert!(cache.dirty);
        cache.persist(&storage, root.path()).unwrap();
        let mut loaded = WorktreeObservations::default();
        loaded.load(&storage, root.path());
        assert!(loaded.entries.is_empty());
    }

    #[test]
    fn corrupt_or_foreign_cache_is_disposable() {
        let root = tempfile::tempdir().unwrap();
        let other_root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        fs::write(storage.path().join("worktree-observations.json"), "broken").unwrap();
        let mut cache = WorktreeObservations::default();
        cache.load(storage.path(), root.path());
        assert!(cache.entries.is_empty());
        let path = root.path().join("file.txt");
        fs::write(&path, "data").unwrap();
        cache
            .content(
                "file.txt",
                &path,
                &fs::metadata(&path).unwrap(),
                false,
                1024,
            )
            .unwrap();
        cache.persist(storage.path(), root.path()).unwrap();
        let mut foreign = WorktreeObservations::default();
        foreign.load(storage.path(), other_root.path());
        assert!(foreign.entries.is_empty());
    }
}
