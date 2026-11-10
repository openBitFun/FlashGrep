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
        self.iter_scope(logical_directory).next().is_some()
    }

    /// Visit an exact file and its descendants without scanning unrelated paths.
    pub fn iter_scope(
        &self,
        logical_path: &str,
    ) -> impl Iterator<Item = (&str, &SnapshotPathEntry)> {
        let exact = self.entries.get_key_value(logical_path).into_iter();
        let prefix = if logical_path.is_empty() {
            String::new()
        } else {
            format!("{logical_path}{}", std::path::MAIN_SEPARATOR)
        };
        let descendants = self
            .entries
            .range(prefix.clone()..)
            .take_while(move |(path, _)| path.starts_with(&prefix));
        exact
            .chain(descendants)
            .map(|(path, entry)| (path.as_str(), entry))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_lookup_skips_lexically_intervening_siblings() {
        let mut catalog = SnapshotPathCatalog::default();
        let descendant = format!("src{}main.rs", std::path::MAIN_SEPARATOR);
        for path in ["src-old.rs", descendant.as_str(), "unrelated.rs"] {
            catalog.entries.insert(
                path.into(),
                SnapshotPathEntry {
                    logical_path: path.into(),
                    size: 0,
                    modified_at: 0,
                    content_hash: [0; 32],
                },
            );
        }
        assert!(catalog.has_path_under("src"));
        assert_eq!(
            catalog
                .iter_scope("src")
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
            vec![descendant.as_str()]
        );
        assert_eq!(catalog.iter_scope("src-old.rs").count(), 1);
        assert_eq!(catalog.iter_scope("").count(), 3);
        assert_eq!(catalog.iter_scope("missing").count(), 0);
    }
}
