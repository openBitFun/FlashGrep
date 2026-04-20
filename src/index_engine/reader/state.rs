use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, RwLock},
};

use crate::index_engine::{
    manifest::Manifest,
    types::{DocKey, ManifestId, SeqNo},
    writer::IndexDocument,
};

#[derive(Debug, Clone)]
pub(crate) struct VersionedDocument {
    pub(crate) seq_no: SeqNo,
    pub(crate) document: IndexDocument,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SnapshotData {
    pub(crate) manifest: Manifest,
    pub(crate) docs: BTreeMap<DocKey, VersionedDocument>,
    pub(crate) deleted_keys: BTreeSet<DocKey>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SharedSnapshotState {
    refreshed: Arc<RwLock<SnapshotData>>,
    committed: Arc<RwLock<SnapshotData>>,
    leases: SnapshotLeaseRegistry,
}

impl SharedSnapshotState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn load_refreshed(&self) -> SnapshotData {
        self.refreshed
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn load_committed(&self) -> SnapshotData {
        self.committed
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn publish_refreshed(&self, snapshot: SnapshotData) {
        let mut guard = self
            .refreshed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = snapshot;
    }

    pub(crate) fn publish_committed(&self, snapshot: SnapshotData) {
        let mut guard = self
            .committed
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = snapshot;
    }

    pub(crate) fn acquire_lease(&self, manifest_id: ManifestId) -> SnapshotLease {
        self.leases.acquire(manifest_id)
    }

    pub(crate) fn pinned_manifest_ids(&self) -> BTreeSet<ManifestId> {
        self.leases.pinned_manifest_ids()
    }
}

#[derive(Debug, Clone, Default)]
struct SnapshotLeaseRegistry {
    inner: Arc<Mutex<BTreeMap<ManifestId, usize>>>,
}

impl SnapshotLeaseRegistry {
    fn acquire(&self, manifest_id: ManifestId) -> SnapshotLease {
        {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard.entry(manifest_id.clone()).or_default() += 1;
        }
        SnapshotLease {
            _entry: Arc::new(SnapshotLeaseEntry {
                registry: self.clone(),
                manifest_id,
            }),
        }
    }

    fn pinned_manifest_ids(&self) -> BTreeSet<ManifestId> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn release(&self, manifest_id: &ManifestId) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = match guard.get_mut(manifest_id) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            guard.remove(manifest_id);
        }
    }
}

#[derive(Debug)]
struct SnapshotLeaseEntry {
    registry: SnapshotLeaseRegistry,
    manifest_id: ManifestId,
}

impl Drop for SnapshotLeaseEntry {
    fn drop(&mut self) {
        self.registry.release(&self.manifest_id);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotLease {
    _entry: Arc<SnapshotLeaseEntry>,
}
