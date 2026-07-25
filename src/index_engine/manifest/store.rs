use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use crate::{fs_atomic::replace_file, AppError, Result};

use super::{Manifest, ManifestMeta};

pub trait ManifestStore: Send + Sync {
    fn load_current(&self) -> Result<Manifest>;
    fn load_manifest(
        &self,
        manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<Manifest>;
    fn publish(&self, manifest: &Manifest) -> Result<()>;
    fn publish_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        manifest: &Manifest,
    ) -> Result<bool>;
    fn write_candidate(&self, manifest: &Manifest) -> Result<()>;
    fn activate_candidate_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        candidate_manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<bool>;
    fn list_manifests(&self) -> Result<Vec<ManifestMeta>>;
    fn remove_manifest(&self, manifest_id: &crate::index_engine::types::ManifestId) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct MemoryManifestStore {
    current: Arc<RwLock<Manifest>>,
    candidates: Mutex<std::collections::HashMap<String, Manifest>>,
}

impl ManifestStore for MemoryManifestStore {
    fn load_current(&self) -> Result<Manifest> {
        let guard = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard.clone())
    }

    fn load_manifest(
        &self,
        manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<Manifest> {
        let manifest = self.load_current()?;
        if manifest.manifest_id == *manifest_id {
            return Ok(manifest);
        }
        self.candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&manifest_id.0)
            .cloned()
            .ok_or_else(|| {
                AppError::InvalidIndex(format!(
                    "manifest {} is not available in memory store",
                    manifest_id.0
                ))
            })
    }

    fn publish(&self, manifest: &Manifest) -> Result<()> {
        self.write_candidate(manifest)?;
        let mut guard = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = manifest.clone();
        Ok(())
    }

    fn publish_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        manifest: &Manifest,
    ) -> Result<bool> {
        self.write_candidate(manifest)?;
        self.activate_candidate_if_current(expected_manifest_id, &manifest.manifest_id)
    }

    fn write_candidate(&self, manifest: &Manifest) -> Result<()> {
        let mut candidates = self
            .candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = candidates.get(&manifest.manifest_id.0) {
            if existing != manifest {
                return Err(AppError::InvalidIndex(format!(
                    "manifest id collision for {}",
                    manifest.manifest_id.0
                )));
            }
            return Ok(());
        }
        candidates.insert(manifest.manifest_id.0.clone(), manifest.clone());
        Ok(())
    }

    fn activate_candidate_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        candidate_manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<bool> {
        let candidate = self
            .candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&candidate_manifest_id.0)
            .cloned()
            .ok_or_else(|| {
                AppError::InvalidIndex(format!(
                    "candidate manifest {} is not available",
                    candidate_manifest_id.0
                ))
            })?;
        let mut guard = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.manifest_id != *expected_manifest_id {
            return Ok(false);
        }
        *guard = candidate;
        Ok(true)
    }

    fn list_manifests(&self) -> Result<Vec<ManifestMeta>> {
        let manifest = self.load_current()?;
        Ok(vec![ManifestMeta {
            manifest_id: manifest.manifest_id,
            generation: manifest.generation,
        }])
    }

    fn remove_manifest(&self, manifest_id: &crate::index_engine::types::ManifestId) -> Result<()> {
        let current = self.load_current()?;
        if current.manifest_id == *manifest_id {
            return Err(AppError::InvalidIndex(format!(
                "refusing to remove current manifest {}",
                manifest_id.0
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::ManifestId;

    #[test]
    fn candidate_activation_uses_parent_compare_and_swap() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let store = FileManifestStore::open(temp.path()).expect("store should open");
        let parent = Manifest {
            manifest_id: ManifestId("parent".into()),
            generation: 1,
            ..Manifest::default()
        };
        store.publish(&parent).expect("parent should publish");
        let candidate = Manifest {
            manifest_id: ManifestId("candidate".into()),
            generation: 2,
            ..Manifest::default()
        };
        store
            .write_candidate(&candidate)
            .expect("candidate should be written");
        assert_eq!(
            store
                .load_current()
                .expect("current should load")
                .manifest_id,
            parent.manifest_id
        );
        assert!(!store
            .activate_candidate_if_current(&ManifestId("stale".into()), &candidate.manifest_id)
            .expect("stale CAS should be handled"));
        assert_eq!(
            store
                .load_current()
                .expect("current should load")
                .manifest_id,
            parent.manifest_id
        );
        assert!(store
            .activate_candidate_if_current(&parent.manifest_id, &candidate.manifest_id)
            .expect("matching CAS should succeed"));
        assert_eq!(
            store
                .load_current()
                .expect("current should load")
                .manifest_id,
            candidate.manifest_id
        );
    }
}

#[derive(Debug)]
pub struct FileManifestStore {
    current_path: PathBuf,
    manifests_dir: PathBuf,
    publish_lock: Mutex<()>,
}

impl FileManifestStore {
    pub fn open(index_root: &Path) -> Result<Self> {
        fs::create_dir_all(index_root)?;
        let manifests_dir = index_root.join("manifests");
        fs::create_dir_all(&manifests_dir)?;
        Ok(Self {
            current_path: index_root.join("CURRENT_MANIFEST"),
            manifests_dir,
            publish_lock: Mutex::new(()),
        })
    }

    fn manifest_path(&self, manifest_id: &crate::index_engine::types::ManifestId) -> PathBuf {
        self.manifests_dir.join(format!("{}.json", manifest_id.0))
    }

    fn load_current_from_disk(&self) -> Result<Manifest> {
        if !self.current_path.exists() {
            return Ok(Manifest::default());
        }
        let current = fs::read_to_string(&self.current_path)?;
        let manifest_name = current.trim();
        if manifest_name.is_empty() {
            return Ok(Manifest::default());
        }
        let manifest_path = self.manifests_dir.join(manifest_name);
        let bytes = fs::read(&manifest_path)?;
        serde_json::from_slice(&bytes).map_err(|error| {
            AppError::InvalidIndex(format!(
                "failed to decode manifest {}: {error}",
                manifest_path.display()
            ))
        })
    }

    fn write_candidate_locked(&self, manifest: &Manifest) -> Result<()> {
        let path = self.manifest_path(&manifest.manifest_id);
        let encoded = serde_json::to_vec_pretty(manifest)
            .map_err(|error| AppError::Protocol(format!("manifest encode failed: {error}")))?;
        if path.exists() {
            let existing = fs::read(&path)?;
            let existing = serde_json::from_slice::<Manifest>(&existing).map_err(|error| {
                AppError::InvalidIndex(format!(
                    "failed to decode manifest {}: {error}",
                    path.display()
                ))
            })?;
            if existing != *manifest {
                return Err(AppError::InvalidIndex(format!(
                    "manifest id collision for {}",
                    manifest.manifest_id.0
                )));
            }
            return Ok(());
        }
        fs::write(&path, encoded)?;
        Ok(())
    }

    fn activate_candidate_locked(
        &self,
        candidate_manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<()> {
        let file_name = format!("{}.json", candidate_manifest_id.0);
        let path = self.manifest_path(candidate_manifest_id);
        if !path.exists() {
            return Err(AppError::InvalidIndex(format!(
                "candidate manifest {} is not available",
                candidate_manifest_id.0
            )));
        }
        let tmp_current = self.current_path.with_extension("tmp");
        fs::write(&tmp_current, format!("{file_name}\n"))?;
        replace_file(&tmp_current, &self.current_path)?;
        Ok(())
    }
}

impl ManifestStore for FileManifestStore {
    fn load_current(&self) -> Result<Manifest> {
        self.load_current_from_disk()
    }

    fn load_manifest(
        &self,
        manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<Manifest> {
        let manifest_path = self.manifest_path(manifest_id);
        let bytes = fs::read(&manifest_path)?;
        serde_json::from_slice(&bytes).map_err(|error| {
            AppError::InvalidIndex(format!(
                "failed to decode manifest {}: {error}",
                manifest_path.display()
            ))
        })
    }

    fn publish(&self, manifest: &Manifest) -> Result<()> {
        let _guard = self
            .publish_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write_candidate_locked(manifest)?;
        self.activate_candidate_locked(&manifest.manifest_id)
    }

    fn publish_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        manifest: &Manifest,
    ) -> Result<bool> {
        let _guard = self
            .publish_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write_candidate_locked(manifest)?;
        let current = self.load_current_from_disk()?;
        if current.manifest_id != *expected_manifest_id {
            return Ok(false);
        }
        self.activate_candidate_locked(&manifest.manifest_id)?;
        Ok(true)
    }

    fn write_candidate(&self, manifest: &Manifest) -> Result<()> {
        let _guard = self
            .publish_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.write_candidate_locked(manifest)
    }

    fn activate_candidate_if_current(
        &self,
        expected_manifest_id: &crate::index_engine::types::ManifestId,
        candidate_manifest_id: &crate::index_engine::types::ManifestId,
    ) -> Result<bool> {
        let _guard = self
            .publish_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.load_current_from_disk()?;
        if current.manifest_id != *expected_manifest_id {
            return Ok(false);
        }
        self.activate_candidate_locked(candidate_manifest_id)?;
        Ok(true)
    }

    fn list_manifests(&self) -> Result<Vec<ManifestMeta>> {
        let mut manifests = Vec::new();
        for entry in fs::read_dir(&self.manifests_dir)? {
            let entry = entry?;
            if entry.path().extension() != Some(OsStr::new("json")) {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            match serde_json::from_slice::<Manifest>(&bytes) {
                Ok(manifest) => manifests.push(ManifestMeta {
                    manifest_id: manifest.manifest_id,
                    generation: manifest.generation,
                }),
                Err(_) => {
                    let path = entry.path();
                    let Some(file_stem) = path.file_stem().and_then(OsStr::to_str) else {
                        continue;
                    };
                    manifests.push(ManifestMeta {
                        manifest_id: crate::index_engine::types::ManifestId(file_stem.into()),
                        generation: 0,
                    });
                }
            }
        }
        manifests.sort_unstable_by_key(|meta| meta.generation);
        Ok(manifests)
    }

    fn remove_manifest(&self, manifest_id: &crate::index_engine::types::ManifestId) -> Result<()> {
        let _guard = self
            .publish_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self.load_current_from_disk()?;
        if current.manifest_id == *manifest_id {
            return Err(AppError::InvalidIndex(format!(
                "refusing to remove current manifest {}",
                manifest_id.0
            )));
        }
        let path = self.manifest_path(manifest_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
