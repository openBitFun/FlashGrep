use std::sync::Arc;

use crate::index_engine::{delete::DeleteRecord, types::DocKey};

pub trait DeleteSegmentReader: Send + Sync {
    fn contains_key(&self, key: &DocKey) -> bool;
    fn records(&self) -> &[DeleteRecord];
}

pub type DeleteSegmentReaderHandle = Arc<dyn DeleteSegmentReader>;

#[derive(Debug)]
pub struct MemoryDeleteSegmentReader {
    records: Vec<DeleteRecord>,
}

impl MemoryDeleteSegmentReader {
    pub fn new(records: Vec<DeleteRecord>) -> Self {
        Self { records }
    }
}

impl DeleteSegmentReader for MemoryDeleteSegmentReader {
    fn contains_key(&self, key: &DocKey) -> bool {
        self.records.iter().any(|record| &record.key == key)
    }

    fn records(&self) -> &[DeleteRecord] {
        &self.records
    }
}
