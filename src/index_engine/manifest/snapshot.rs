use std::sync::Arc;

use super::Manifest;

#[derive(Debug, Clone)]
pub struct ManifestSnapshot {
    manifest: Arc<Manifest>,
}

impl ManifestSnapshot {
    pub fn new(manifest: Manifest) -> Self {
        Self {
            manifest: Arc::new(manifest),
        }
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}
