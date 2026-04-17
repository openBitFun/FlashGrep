use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
};

use crate::index_engine::{
    manifest::Manifest,
    types::{DocKey, SeqNo},
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
}
