#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPathEntry {
    pub logical_path: String,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPathCatalog {
    pub(super) entries: std::collections::BTreeMap<String, SnapshotPathEntry>,
}

impl SnapshotPathCatalog {
    pub fn get(&self, logical_path: &str) -> Option<&SnapshotPathEntry> {
        self.entries.get(logical_path)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn has_path_under(&self, logical_directory: &str) -> bool {
        if logical_directory.is_empty() {
            return !self.entries.is_empty();
        }

        let prefix = format!("{logical_directory}{}", std::path::MAIN_SEPARATOR);
        self.entries
            .range(logical_directory.to_string()..)
            .next()
            .is_some_and(|(path, _)| path == logical_directory || path.starts_with(&prefix))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SnapshotPathEntry)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.as_str(), entry))
    }
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotVisibleKeys {
    pub live: std::collections::BTreeSet<crate::index_engine::types::DocKey>,
    pub deleted: std::collections::BTreeSet<crate::index_engine::types::DocKey>,
}
