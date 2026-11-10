use std::{fs, path::PathBuf, time::UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
    fs_atomic::replace_file,
    index_engine::{
        config::PostingTokenizerConfig,
        delete::{DeleteStore, FileDeleteStore},
        manifest::{FileManifestStore, Manifest, ManifestSource, ManifestStore},
        segment::{FileSegmentStore, SegmentStore},
        ManifestId,
    },
};

use super::{BaseSnapshotBuildLock, WorkspaceBaseSnapshotIndex};

const HEAD_CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeadManifestCacheEntry {
    pub head_commit: String,
    pub manifest_id: ManifestId,
    pub config_fingerprint: String,
    pub repo_root: String,
    pub last_used_unix_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HeadManifestCacheFile {
    version: u32,
    entries: Vec<HeadManifestCacheEntry>,
}

impl WorkspaceBaseSnapshotIndex {
    pub(crate) fn record_active_head_in_cache(
        build_config: &BuildConfig,
        capacity: usize,
    ) -> Result<()> {
        let _build_lock = BaseSnapshotBuildLock::acquire(build_config, &mut || false)?;
        let store = FileManifestStore::open(&Self::index_root(build_config))?;
        let manifest = store.load_current()?;
        Self::record_manifest_in_cache(build_config, &manifest, capacity)
    }

    pub(crate) fn record_manifest_in_cache(
        build_config: &BuildConfig,
        manifest: &Manifest,
        capacity: usize,
    ) -> Result<()> {
        let Some(ManifestSource::GitBase {
            head_commit,
            config_fingerprint,
            repo_root,
            ..
        }) = manifest.source.as_ref()
        else {
            return Ok(());
        };
        let mut cache = load_cache(build_config);
        cache.entries.retain(|entry| {
            !(entry.head_commit == *head_commit
                && entry.config_fingerprint == *config_fingerprint
                && entry.repo_root == *repo_root)
        });
        let last_used_unix_secs = cache
            .entries
            .iter()
            .map(|entry| entry.last_used_unix_secs)
            .max()
            .map_or_else(now_unix_secs, |last| {
                now_unix_secs().max(last.saturating_add(1))
            });
        cache.entries.push(HeadManifestCacheEntry {
            head_commit: head_commit.clone(),
            manifest_id: manifest.manifest_id.clone(),
            config_fingerprint: config_fingerprint.clone(),
            repo_root: repo_root.clone(),
            last_used_unix_secs,
        });
        cache
            .entries
            .sort_unstable_by_key(|entry| std::cmp::Reverse(entry.last_used_unix_secs));
        cache.entries.truncate(capacity);
        write_cache(build_config, &cache)
    }

    pub(crate) fn activate_cached_head(
        build_config: &BuildConfig,
        target_head: &str,
        capacity: usize,
    ) -> Result<Option<ManifestId>> {
        if capacity == 0 {
            let mut cache = load_cache(build_config);
            if !cache.entries.is_empty() {
                cache.entries.clear();
                write_cache(build_config, &cache)?;
            }
            return Ok(None);
        }
        let expected_config = super::base_snapshot_config_fingerprint(build_config);
        let expected_root = super::canonical_repo_root(&build_config.repo_path);
        let mut cache = load_cache(build_config);
        let Some(index) = cache.entries.iter().position(|entry| {
            entry.head_commit == target_head
                && entry.config_fingerprint == expected_config
                && entry.repo_root == expected_root
        }) else {
            return Ok(None);
        };
        let entry = cache.entries[index].clone();
        let store = FileManifestStore::open(&Self::index_root(build_config))?;
        let current = store.load_current()?;
        let candidate = match store.load_manifest(&entry.manifest_id) {
            Ok(candidate) => candidate,
            Err(AppError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                cache.entries.remove(index);
                write_cache(build_config, &cache)?;
                return Ok(None);
            }
            Err(AppError::InvalidIndex(_)) => {
                cache.entries.remove(index);
                write_cache(build_config, &cache)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let valid = matches!(
            candidate.source,
            Some(ManifestSource::GitBase {
                ref head_commit,
                ref config_fingerprint,
                ref repo_root,
                ..
            }) if head_commit == target_head
                && config_fingerprint == &expected_config
                && repo_root == &expected_root
        );
        if !valid || !manifest_artifacts_valid(build_config, &candidate) {
            cache.entries.remove(index);
            write_cache(build_config, &cache)?;
            return Ok(None);
        }
        if current.manifest_id != candidate.manifest_id
            && !store.activate_candidate_if_current(&current.manifest_id, &candidate.manifest_id)?
        {
            return Ok(None);
        }
        cache.entries[index].last_used_unix_secs = cache
            .entries
            .iter()
            .map(|entry| entry.last_used_unix_secs)
            .max()
            .map_or_else(now_unix_secs, |last| {
                now_unix_secs().max(last.saturating_add(1))
            });
        cache
            .entries
            .sort_unstable_by_key(|entry| std::cmp::Reverse(entry.last_used_unix_secs));
        cache.entries.truncate(capacity);
        write_cache(build_config, &cache)?;
        Ok(Some(candidate.manifest_id))
    }

    pub(crate) fn cached_manifest_ids(build_config: &BuildConfig) -> Vec<ManifestId> {
        load_cache(build_config)
            .entries
            .into_iter()
            .map(|entry| entry.manifest_id)
            .collect()
    }
}

fn cache_path(build_config: &BuildConfig) -> PathBuf {
    WorkspaceBaseSnapshotIndex::index_root(build_config).join("HEAD_MANIFESTS.json")
}

fn manifest_artifacts_valid(build_config: &BuildConfig, manifest: &Manifest) -> bool {
    let Some(ManifestSource::GitBase { head_commit, .. }) = manifest.source.as_ref() else {
        return false;
    };
    let index_root = WorkspaceBaseSnapshotIndex::index_root(build_config);
    let Ok(segment_store) = FileSegmentStore::open_with_repo_root(
        &index_root,
        PostingTokenizerConfig::from(build_config),
        Some(build_config.repo_path.clone()),
        Some(head_commit.clone()),
    ) else {
        return false;
    };
    if !manifest
        .active_segments
        .iter()
        .all(|segment| segment_store.open_segment(segment.id.clone()).is_ok())
    {
        return false;
    }
    let Ok(delete_store) = FileDeleteStore::open(&index_root) else {
        return false;
    };
    manifest
        .active_delete_segments
        .iter()
        .all(|segment| delete_store.open_delete_segment(segment.id.clone()).is_ok())
}

fn load_cache(build_config: &BuildConfig) -> HeadManifestCacheFile {
    let Ok(bytes) = fs::read(cache_path(build_config)) else {
        return HeadManifestCacheFile {
            version: HEAD_CACHE_VERSION,
            entries: Vec::new(),
        };
    };
    serde_json::from_slice::<HeadManifestCacheFile>(&bytes)
        .ok()
        .filter(|cache| cache.version == HEAD_CACHE_VERSION)
        .unwrap_or(HeadManifestCacheFile {
            version: HEAD_CACHE_VERSION,
            entries: Vec::new(),
        })
}

fn write_cache(build_config: &BuildConfig, cache: &HeadManifestCacheFile) -> Result<()> {
    let path = cache_path(build_config);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(cache).map_err(|error| {
        AppError::Protocol(format!("HEAD manifest cache encode failed: {error}"))
    })?;
    fs::write(&tmp, bytes)?;
    replace_file(&tmp, &path)?;
    Ok(())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CorpusMode, TokenizerMode};

    fn config(temp: &tempfile::TempDir) -> BuildConfig {
        BuildConfig {
            git_mode: Default::default(),
            repo_path: temp.path().join("repo"),
            storage_root: temp.path().join("storage"),
            tokenizer: TokenizerMode::SparseNgram,
            corpus_mode: CorpusMode::NoIgnore,
            include_hidden: false,
            max_file_size: 1024,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    fn manifest(build_config: &BuildConfig, head: &str, generation: u64) -> Manifest {
        Manifest {
            manifest_id: ManifestId(format!("manifest-{head}")),
            generation,
            source: Some(ManifestSource::GitBase {
                head_commit: head.into(),
                config_fingerprint: super::super::base_snapshot_config_fingerprint(build_config),
                repo_root: super::super::canonical_repo_root(&build_config.repo_path),
                document_count: 0,
                parent_manifest_id: None,
                foundation_segment_ids: Vec::new(),
                delta_depth: 0,
                delta_bytes: 0,
            }),
            ..Manifest::default()
        }
    }

    #[test]
    fn head_cache_enforces_lru_capacity_and_can_be_disabled() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        fs::create_dir_all(temp.path().join("repo")).expect("repo dir should succeed");
        let build_config = config(&temp);
        for (generation, head) in ["h0", "h1", "h2"].into_iter().enumerate() {
            WorkspaceBaseSnapshotIndex::record_manifest_in_cache(
                &build_config,
                &manifest(&build_config, head, generation as u64),
                2,
            )
            .expect("cache record should succeed");
        }
        let cache = load_cache(&build_config);
        assert_eq!(
            cache
                .entries
                .iter()
                .map(|entry| entry.head_commit.as_str())
                .collect::<Vec<_>>(),
            ["h2", "h1"]
        );

        WorkspaceBaseSnapshotIndex::record_manifest_in_cache(
            &build_config,
            &manifest(&build_config, "h1", 3),
            2,
        )
        .expect("cache touch should succeed");
        WorkspaceBaseSnapshotIndex::record_manifest_in_cache(
            &build_config,
            &manifest(&build_config, "h3", 4),
            2,
        )
        .expect("cache record should succeed");
        assert_eq!(
            load_cache(&build_config)
                .entries
                .iter()
                .map(|entry| entry.head_commit.as_str())
                .collect::<Vec<_>>(),
            ["h3", "h1"]
        );

        WorkspaceBaseSnapshotIndex::record_manifest_in_cache(
            &build_config,
            &manifest(&build_config, "disabled", 5),
            0,
        )
        .expect("disabled cache write should succeed");
        assert!(load_cache(&build_config).entries.is_empty());
    }

    #[test]
    fn corrupt_head_cache_is_ignored_without_affecting_the_index() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        fs::create_dir_all(temp.path().join("repo")).expect("repo dir should succeed");
        let build_config = config(&temp);
        let index_root = WorkspaceBaseSnapshotIndex::index_root(&build_config);
        fs::create_dir_all(&index_root).expect("index root should succeed");
        fs::write(cache_path(&build_config), b"not json").expect("corrupt write should succeed");
        assert!(WorkspaceBaseSnapshotIndex::cached_manifest_ids(&build_config).is_empty());
    }

    #[test]
    fn corrupt_cached_manifest_is_evicted_without_changing_current() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        fs::create_dir_all(temp.path().join("repo")).expect("repo dir should succeed");
        let build_config = config(&temp);
        let store = FileManifestStore::open(&WorkspaceBaseSnapshotIndex::index_root(&build_config))
            .expect("manifest store should open");
        let parent = manifest(&build_config, "parent", 1);
        let candidate = manifest(&build_config, "candidate", 2);
        store.publish(&parent).expect("parent should publish");
        store
            .write_candidate(&candidate)
            .expect("candidate should write");
        WorkspaceBaseSnapshotIndex::record_manifest_in_cache(&build_config, &candidate, 4)
            .expect("candidate should cache");
        fs::write(
            WorkspaceBaseSnapshotIndex::index_root(&build_config)
                .join("manifests")
                .join(format!("{}.json", candidate.manifest_id.0)),
            b"not json",
        )
        .expect("candidate corruption should succeed");

        assert_eq!(
            WorkspaceBaseSnapshotIndex::activate_cached_head(&build_config, "candidate", 4,)
                .expect("cache corruption should be handled"),
            None
        );
        assert_eq!(
            store
                .load_current()
                .expect("current should load")
                .manifest_id,
            parent.manifest_id
        );
        assert!(load_cache(&build_config).entries.is_empty());
    }
}
