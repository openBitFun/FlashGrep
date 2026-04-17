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
        let manifests = self.load_manifests(manifest_store)?;
        if manifests.is_empty() {
            return Ok(Vec::new());
        }

        let retained = retained_manifests(&manifests, self.retained_history_manifests);
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

    fn load_manifests(&self, manifest_store: &dyn ManifestStore) -> Result<Vec<Manifest>> {
        let mut manifests = manifest_store
            .list_manifests()?
            .into_iter()
            .map(|meta| manifest_store.load_manifest(&meta.manifest_id))
            .collect::<Result<Vec<_>>>()?;
        manifests.sort_unstable_by_key(|manifest| manifest.generation);
        Ok(manifests)
    }
}

fn retained_manifests(manifests: &[Manifest], retained_history_manifests: usize) -> Vec<Manifest> {
    if manifests.is_empty() {
        return Vec::new();
    }

    let mut retained = manifests
        .iter()
        .rev()
        .take(retained_history_manifests + 1)
        .cloned()
        .collect::<Vec<_>>();
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
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}
