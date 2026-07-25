use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    index_engine::{
        manifest::{Manifest, ManifestStore},
        DeleteSegmentId, ManifestId, SegmentId,
    },
    Result,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcCandidateKind {
    Segment,
    DeleteSegment,
    Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcCandidate {
    pub kind: GcCandidateKind,
    pub id: String,
    pub path: String,
    pub bytes: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reclaimer {
    retained_history_manifests: usize,
}

impl Default for Reclaimer {
    fn default() -> Self {
        Self {
            retained_history_manifests: 1,
        }
    }
}

impl Reclaimer {
    pub fn with_retained_history_manifests(mut self, count: usize) -> Self {
        self.retained_history_manifests = count;
        self
    }

    pub fn plan(
        &self,
        index_root: &Path,
        manifest_store: &dyn ManifestStore,
    ) -> Result<Vec<GcCandidate>> {
        self.plan_with_pins(index_root, manifest_store, &BTreeSet::new())
    }

    pub fn plan_with_pins(
        &self,
        index_root: &Path,
        manifest_store: &dyn ManifestStore,
        pinned_manifest_ids: &BTreeSet<ManifestId>,
    ) -> Result<Vec<GcCandidate>> {
        let current_manifest = manifest_store.load_current()?;
        let (manifests, unreadable_manifest_ids) =
            self.load_manifests(manifest_store, &current_manifest.manifest_id)?;

        let mut retention_roots = pinned_manifest_ids.clone();
        retention_roots.insert(current_manifest.manifest_id);
        let retained = retained_manifests(
            &manifests,
            self.retained_history_manifests,
            &retention_roots,
        );
        let retained_ids = retained
            .iter()
            .map(|manifest| manifest.manifest_id.clone())
            .collect::<BTreeSet<_>>();

        let mut reachable_segments = BTreeMap::<SegmentId, u64>::new();
        let mut reachable_delete_segments = BTreeMap::<DeleteSegmentId, u64>::new();
        for manifest in &retained {
            for segment in &manifest.active_segments {
                reachable_segments
                    .entry(segment.id.clone())
                    .or_insert(segment.size_bytes);
            }
            for delete_segment in &manifest.active_delete_segments {
                reachable_delete_segments
                    .entry(delete_segment.id.clone())
                    .or_insert(delete_segment.size_bytes);
            }
        }

        let mut all_segments = BTreeMap::<SegmentId, u64>::new();
        let mut all_delete_segments = BTreeMap::<DeleteSegmentId, u64>::new();
        for manifest in &manifests {
            for segment in &manifest.active_segments {
                all_segments
                    .entry(segment.id.clone())
                    .or_insert(segment.size_bytes);
            }
            for delete_segment in &manifest.active_delete_segments {
                all_delete_segments
                    .entry(delete_segment.id.clone())
                    .or_insert(delete_segment.size_bytes);
            }
        }
        for (id, bytes) in discover_artifact_directories(&index_root.join("segments"))? {
            all_segments.insert(SegmentId(id), bytes);
        }
        for (id, bytes) in discover_artifact_directories(&index_root.join("deletes"))? {
            all_delete_segments.insert(DeleteSegmentId(id), bytes);
        }

        let mut candidates = Vec::new();
        for manifest in manifests {
            if retained_ids.contains(&manifest.manifest_id) {
                continue;
            }
            let path = manifest_path(index_root, &manifest.manifest_id);
            candidates.push(GcCandidate {
                kind: GcCandidateKind::Manifest,
                id: manifest.manifest_id.0.clone(),
                path: path.display().to_string(),
                bytes: file_size(&path),
                reason: "superseded manifest outside retention window".into(),
            });
        }
        for manifest_id in unreadable_manifest_ids {
            let path = manifest_path(index_root, &manifest_id);
            candidates.push(GcCandidate {
                kind: GcCandidateKind::Manifest,
                id: manifest_id.0,
                path: path.display().to_string(),
                bytes: file_size(&path),
                reason: "unreadable non-current candidate manifest".into(),
            });
        }

        for (segment_id, size_bytes) in all_segments {
            if reachable_segments.contains_key(&segment_id) {
                continue;
            }
            let path = segment_path(index_root, &segment_id);
            candidates.push(GcCandidate {
                kind: GcCandidateKind::Segment,
                id: segment_id.0,
                path: path.display().to_string(),
                bytes: size_bytes,
                reason: "segment is unreachable from retained manifests".into(),
            });
        }

        for (delete_segment_id, size_bytes) in all_delete_segments {
            if reachable_delete_segments.contains_key(&delete_segment_id) {
                continue;
            }
            let path = delete_segment_path(index_root, &delete_segment_id);
            candidates.push(GcCandidate {
                kind: GcCandidateKind::DeleteSegment,
                id: delete_segment_id.0,
                path: path.display().to_string(),
                bytes: size_bytes,
                reason: "delete segment is unreachable from retained manifests".into(),
            });
        }

        candidates.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(candidates)
    }

    pub fn execute(
        &self,
        candidates: &[GcCandidate],
        manifest_store: &dyn ManifestStore,
        segment_store: &dyn crate::index_engine::segment::SegmentStore,
        delete_store: &dyn crate::index_engine::delete::DeleteStore,
    ) -> Result<()> {
        for candidate in candidates {
            match candidate.kind {
                GcCandidateKind::Segment => {
                    segment_store.remove_segment(SegmentId(candidate.id.clone()))?;
                }
                GcCandidateKind::DeleteSegment => {
                    delete_store.remove_delete_segment(DeleteSegmentId(candidate.id.clone()))?;
                }
                GcCandidateKind::Manifest => {
                    manifest_store.remove_manifest(&ManifestId(candidate.id.clone()))?;
                }
            }
        }
        Ok(())
    }

    fn load_manifests(
        &self,
        manifest_store: &dyn ManifestStore,
        current_manifest_id: &ManifestId,
    ) -> Result<(Vec<Manifest>, Vec<ManifestId>)> {
        let mut manifests = Vec::new();
        let mut unreadable = Vec::new();
        for meta in manifest_store.list_manifests()? {
            match manifest_store.load_manifest(&meta.manifest_id) {
                Ok(manifest) => manifests.push(manifest),
                Err(error) if meta.manifest_id == *current_manifest_id => return Err(error),
                Err(_) => unreadable.push(meta.manifest_id),
            }
        }
        manifests.sort_unstable_by_key(|manifest| manifest.generation);
        Ok((manifests, unreadable))
    }
}

fn discover_artifact_directories(root: &Path) -> Result<Vec<(String, u64)>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        artifacts.push((
            entry.file_name().to_string_lossy().into_owned(),
            directory_size(&entry.path())?,
        ));
    }
    Ok(artifacts)
}

fn directory_size(root: &Path) -> Result<u64> {
    let mut bytes = 0_u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                bytes = bytes.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(bytes)
}

fn retained_manifests(
    manifests: &[Manifest],
    retained_history_manifests: usize,
    pinned_manifest_ids: &BTreeSet<ManifestId>,
) -> Vec<Manifest> {
    if manifests.is_empty() {
        return Vec::new();
    }

    let mut retained = manifests
        .iter()
        .rev()
        .take(retained_history_manifests + 1)
        .cloned()
        .collect::<Vec<_>>();
    retained.extend(
        manifests
            .iter()
            .filter(|manifest| pinned_manifest_ids.contains(&manifest.manifest_id))
            .cloned(),
    );
    retained.sort_unstable_by_key(|manifest| manifest.generation);
    retained.dedup_by(|right, left| right.manifest_id == left.manifest_id);
    retained.sort_unstable_by_key(|manifest| manifest.generation);
    retained
}

fn manifest_path(index_root: &Path, manifest_id: &ManifestId) -> PathBuf {
    index_root
        .join("manifests")
        .join(format!("{}.json", manifest_id.0))
}

fn segment_path(index_root: &Path, segment_id: &SegmentId) -> PathBuf {
    index_root.join("segments").join(&segment_id.0)
}

fn delete_segment_path(index_root: &Path, delete_segment_id: &DeleteSegmentId) -> PathBuf {
    index_root.join("deletes").join(&delete_segment_id.0)
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_engine::{
        config::PostingTokenizerConfig,
        delete::FileDeleteStore,
        manifest::{FileManifestStore, SegmentRef},
        segment::FileSegmentStore,
    };

    fn segment(id: &str) -> SegmentRef {
        SegmentRef {
            id: SegmentId(id.into()),
            min_seq_no: 1,
            max_seq_no: 1,
            doc_count: 1,
            size_bytes: 1,
        }
    }

    fn manifest(id: &str, generation: u64, segments: &[&str]) -> Manifest {
        Manifest {
            manifest_id: ManifestId(id.into()),
            generation,
            active_segments: segments.iter().map(|id| segment(id)).collect(),
            ..Manifest::default()
        }
    }

    #[test]
    fn pinned_manifest_keeps_its_unique_artifacts_until_eviction() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let store = FileManifestStore::open(temp.path()).expect("manifest store should open");
        let old = manifest("old", 1, &["shared", "old-only"]);
        let middle = manifest("middle", 2, &["shared", "middle-only"]);
        let current = manifest("current", 3, &["shared", "current-only"]);
        store
            .write_candidate(&old)
            .expect("old manifest should write");
        store
            .write_candidate(&middle)
            .expect("middle manifest should write");
        store.publish(&current).expect("current should publish");

        let reclaimer = Reclaimer::default().with_retained_history_manifests(0);
        let pinned = BTreeSet::from([old.manifest_id.clone()]);
        let candidates = reclaimer
            .plan_with_pins(temp.path(), &store, &pinned)
            .expect("GC plan should succeed");
        assert!(candidates.iter().any(|candidate| candidate.id == "middle"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.id == "middle-only"));
        assert!(!candidates.iter().any(|candidate| candidate.id == "old"));
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.id == "old-only"));
        assert!(!candidates.iter().any(|candidate| candidate.id == "shared"));

        let after_eviction = reclaimer
            .plan(temp.path(), &store)
            .expect("GC plan should succeed");
        assert!(after_eviction.iter().any(|candidate| candidate.id == "old"));
        assert!(after_eviction
            .iter()
            .any(|candidate| candidate.id == "old-only"));
        assert!(!after_eviction
            .iter()
            .any(|candidate| candidate.id == "shared"));
        assert!(!after_eviction
            .iter()
            .any(|candidate| candidate.id == "current-only"));
    }

    #[test]
    fn current_manifest_is_retained_even_when_its_generation_is_older() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let store = FileManifestStore::open(temp.path()).expect("manifest store should open");
        let old = manifest("old-current", 1, &["old-current-only"]);
        let newest = manifest("newest", 3, &["newest-only"]);
        store.publish(&old).expect("old manifest should publish");
        store
            .write_candidate(&newest)
            .expect("newest manifest should write");
        assert!(store
            .activate_candidate_if_current(&old.manifest_id, &newest.manifest_id)
            .expect("newest activation should succeed"));
        assert!(store
            .activate_candidate_if_current(&newest.manifest_id, &old.manifest_id)
            .expect("cached rollback activation should succeed"));

        let candidates = Reclaimer::default()
            .with_retained_history_manifests(0)
            .plan(temp.path(), &store)
            .expect("GC plan should succeed");
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.id == "old-current"));
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.id == "old-current-only"));
    }

    #[test]
    fn gc_reclaims_partial_orphan_artifacts_and_corrupt_non_current_manifest() {
        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let store = FileManifestStore::open(temp.path()).expect("manifest store should open");
        let current = manifest("current", 1, &[]);
        store.publish(&current).expect("current should publish");
        let orphan_segment = temp.path().join("segments/orphan-segment");
        let orphan_delete = temp.path().join("deletes/orphan-delete");
        fs::create_dir_all(&orphan_segment).expect("orphan segment should create");
        fs::create_dir_all(&orphan_delete).expect("orphan delete should create");
        fs::write(orphan_segment.join("partial.bin"), b"partial")
            .expect("partial segment should write");
        fs::write(orphan_delete.join("partial.bin"), b"partial")
            .expect("partial delete should write");
        fs::write(
            temp.path().join("manifests/corrupt-candidate.json"),
            b"not json",
        )
        .expect("corrupt candidate should write");

        let reclaimer = Reclaimer::default().with_retained_history_manifests(0);
        let candidates = reclaimer
            .plan(temp.path(), &store)
            .expect("GC plan should tolerate corrupt candidate");
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == GcCandidateKind::Segment && candidate.id == "orphan-segment"
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == GcCandidateKind::DeleteSegment && candidate.id == "orphan-delete"
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == GcCandidateKind::Manifest && candidate.id == "corrupt-candidate"
        }));

        let segment_store = FileSegmentStore::open(temp.path(), PostingTokenizerConfig::default())
            .expect("segment store should open");
        let delete_store = FileDeleteStore::open(temp.path()).expect("delete store should open");
        reclaimer
            .execute(&candidates, &store, &segment_store, &delete_store)
            .expect("GC should remove partial artifacts");
        assert!(!orphan_segment.exists());
        assert!(!orphan_delete.exists());
        assert!(!temp
            .path()
            .join("manifests/corrupt-candidate.json")
            .exists());
        assert_eq!(
            store
                .load_current()
                .expect("current should remain")
                .manifest_id,
            current.manifest_id
        );
    }
}
