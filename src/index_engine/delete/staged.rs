use super::{DeleteSegmentReaderHandle, DeleteStore, FileDeleteStore, MemoryDeleteSegmentReader};
use crate::{
    index_engine::{
        segment::{DeleteSegmentBuildOutput, DeleteSegmentMeta},
        types::DeleteSegmentId,
    },
    Result,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

pub(crate) struct StagedDeleteStore {
    disk: FileDeleteStore,
    staged: RwLock<HashMap<DeleteSegmentId, DeleteSegmentBuildOutput>>,
    persistence: Mutex<()>,
}
impl StagedDeleteStore {
    pub(crate) fn new(disk: FileDeleteStore) -> Self {
        Self {
            disk,
            staged: RwLock::default(),
            persistence: Mutex::new(()),
        }
    }
}
impl DeleteStore for StagedDeleteStore {
    fn write_delete_segment(&self, build: DeleteSegmentBuildOutput) -> Result<DeleteSegmentMeta> {
        // Bound outstanding tombstone records separately from content segments.
        let ids = {
            let staged = self
                .staged
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if staged.values().map(|b| b.records.len()).sum::<usize>() + build.records.len() > 65536
            {
                staged.keys().cloned().collect::<Vec<_>>()
            } else {
                Vec::new()
            }
        };
        for id in ids {
            self.persist_delete_segment(&id)?;
        }
        if build.records.len() > 65536 {
            return self.disk.write_delete_segment(build);
        }
        let meta = build.meta.clone();
        self.staged
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(meta.delete_segment_id.clone(), build);
        Ok(meta)
    }
    fn open_delete_segment(&self, id: DeleteSegmentId) -> Result<DeleteSegmentReaderHandle> {
        if let Some(b) = self
            .staged
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
        {
            return Ok(Arc::new(MemoryDeleteSegmentReader::new(b.records.clone())));
        }
        self.disk.open_delete_segment(id)
    }
    fn persist_delete_segment(&self, id: &DeleteSegmentId) -> Result<()> {
        let _serial = self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let build = self
            .staged
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        if let Some(b) = build {
            self.disk.write_delete_segment(b)?;
            self.staged
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        Ok(())
    }
    fn remove_delete_segment(&self, id: DeleteSegmentId) -> Result<()> {
        let _serial = self
            .persistence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.staged
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        self.disk.remove_delete_segment(id)
    }
}
