use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use crate::{AppError, Result};

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
    fn list_manifests(&self) -> Result<Vec<ManifestMeta>>;
    fn remove_manifest(&self, manifest_id: &crate::index_engine::types::ManifestId) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct MemoryManifestStore {
    current: Arc<RwLock<Manifest>>,
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
            Ok(manifest)
        } else {
            Err(AppError::InvalidIndex(format!(
                "manifest {} is not available in memory store",
                manifest_id.0
            )))
        }
    }

    fn publish(&self, manifest: &Manifest) -> Result<()> {
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
        let mut guard = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.manifest_id != *expected_manifest_id {
            return Ok(false);
        }
        *guard = manifest.clone();
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

    fn publish_locked(&self, manifest: &Manifest) -> Result<()> {
        let file_name = format!("{}.json", manifest.manifest_id.0);
        let path = self.manifest_path(&manifest.manifest_id);
        let encoded = serde_json::to_vec_pretty(manifest)
            .map_err(|error| AppError::Protocol(format!("manifest encode failed: {error}")))?;
        fs::write(&path, encoded)?;
        let tmp_current = self.current_path.with_extension("tmp");
        fs::write(&tmp_current, format!("{file_name}\n"))?;
        fs::rename(tmp_current, &self.current_path)?;
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
        self.publish_locked(manifest)
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
        let current = self.load_current_from_disk()?;
        if current.manifest_id != *expected_manifest_id {
            return Ok(false);
        }
        self.publish_locked(manifest)?;
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
            let manifest = serde_json::from_slice::<Manifest>(&bytes).map_err(|error| {
                AppError::InvalidIndex(format!(
                    "failed to decode manifest {}: {error}",
                    entry.path().display()
                ))
            })?;
            manifests.push(ManifestMeta {
                manifest_id: manifest.manifest_id,
                generation: manifest.generation,
            });
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
