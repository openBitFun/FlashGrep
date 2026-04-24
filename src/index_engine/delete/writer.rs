use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use crate::{
    index_engine::{
        delete::{
            read_delete_records, read_delete_segment_meta, write_delete_records,
            write_delete_segment_meta, DeleteRecord, MemoryDeleteSegmentReader, DELETE_META_FILE,
            DELETE_RECORDS_FILE,
        },
        segment::{DeleteSegmentBuildOutput, DeleteSegmentMeta},
        types::DeleteSegmentId,
    },
    AppError, Result,
};

#[derive(Debug, Default)]
pub struct DeleteBuffer {
    pub records: Vec<DeleteRecord>,
}

pub trait DeleteStore: Send + Sync {
    fn write_delete_segment(&self, build: DeleteSegmentBuildOutput) -> Result<DeleteSegmentMeta>;
    fn open_delete_segment(
        &self,
        id: DeleteSegmentId,
    ) -> Result<crate::index_engine::delete::DeleteSegmentReaderHandle>;
    fn remove_delete_segment(&self, id: DeleteSegmentId) -> Result<()>;
}

#[derive(Default)]
pub struct MemoryDeleteStore {
    delete_segments: RwLock<HashMap<DeleteSegmentId, DeleteSegmentMeta>>,
}

impl DeleteStore for MemoryDeleteStore {
    fn write_delete_segment(&self, build: DeleteSegmentBuildOutput) -> Result<DeleteSegmentMeta> {
        let mut guard = self
            .delete_segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(build.meta.delete_segment_id.clone(), build.meta.clone());
        Ok(build.meta)
    }

    fn open_delete_segment(
        &self,
        _id: DeleteSegmentId,
    ) -> Result<crate::index_engine::delete::DeleteSegmentReaderHandle> {
        Ok(std::sync::Arc::new(MemoryDeleteSegmentReader::new(
            Vec::new(),
        )))
    }

    fn remove_delete_segment(&self, id: DeleteSegmentId) -> Result<()> {
        let mut guard = self
            .delete_segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&id);
        Ok(())
    }
}

#[derive(Debug)]
pub struct FileDeleteStore {
    deletes_dir: PathBuf,
}

impl FileDeleteStore {
    pub fn open(index_root: &Path) -> Result<Self> {
        let deletes_dir = index_root.join("deletes");
        fs::create_dir_all(&deletes_dir)?;
        Ok(Self { deletes_dir })
    }

    fn delete_dir(&self, id: &DeleteSegmentId) -> PathBuf {
        self.deletes_dir.join(&id.0)
    }
}

impl DeleteStore for FileDeleteStore {
    fn write_delete_segment(&self, build: DeleteSegmentBuildOutput) -> Result<DeleteSegmentMeta> {
        let delete_dir = self.delete_dir(&build.meta.delete_segment_id);
        fs::create_dir_all(&delete_dir)?;
        write_delete_segment_meta(&delete_dir.join(DELETE_META_FILE), &build.meta)?;
        write_delete_records(&delete_dir.join(DELETE_RECORDS_FILE), &build.records)?;
        Ok(build.meta)
    }

    fn open_delete_segment(
        &self,
        id: DeleteSegmentId,
    ) -> Result<crate::index_engine::delete::DeleteSegmentReaderHandle> {
        let delete_dir = self.delete_dir(&id);
        let _meta = read_delete_segment_meta(&delete_dir.join(DELETE_META_FILE))?;
        let records = read_delete_records(&delete_dir.join(DELETE_RECORDS_FILE))?;
        Ok(std::sync::Arc::new(MemoryDeleteSegmentReader::new(records)))
    }

    fn remove_delete_segment(&self, id: DeleteSegmentId) -> Result<()> {
        let delete_dir = self.delete_dir(&id);
        if delete_dir.exists() {
            for required in [DELETE_META_FILE, DELETE_RECORDS_FILE] {
                let path = delete_dir.join(required);
                if !path.exists() {
                    return Err(AppError::InvalidIndex(format!(
                        "delete segment {} is missing {} before removal",
                        id.0, required
                    )));
                }
            }
            fs::remove_dir_all(delete_dir)?;
        }
        Ok(())
    }
}
