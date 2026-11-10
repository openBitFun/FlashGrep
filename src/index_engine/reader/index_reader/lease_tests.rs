use super::*;
use crate::index_engine::{
    config::EngineConfig,
    delete::FileDeleteStore,
    manifest::{FileManifestStore, ManifestMeta},
    segment::FileSegmentStore,
    writer::IndexEngine,
    ManifestId, RepoId,
};
use std::sync::{mpsc, Mutex};

struct PausedManifestStore {
    inner: FileManifestStore,
    loaded: mpsc::Sender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
}

impl ManifestStore for PausedManifestStore {
    fn load_current(&self) -> Result<Manifest> {
        let manifest = self.inner.load_current()?;
        self.loaded.send(()).unwrap();
        self.resume.lock().unwrap().recv().unwrap();
        Ok(manifest)
    }
    fn load_manifest(&self, id: &ManifestId) -> Result<Manifest> {
        let manifest = self.inner.load_manifest(id)?;
        self.loaded.send(()).unwrap();
        self.resume.lock().unwrap().recv().unwrap();
        Ok(manifest)
    }
    fn publish(&self, m: &Manifest) -> Result<()> {
        self.inner.publish(m)
    }
    fn publish_if_current(&self, id: &ManifestId, m: &Manifest) -> Result<bool> {
        self.inner.publish_if_current(id, m)
    }
    fn write_candidate(&self, m: &Manifest) -> Result<()> {
        self.inner.write_candidate(m)
    }
    fn activate_candidate_if_current(
        &self,
        expected: &ManifestId,
        candidate: &ManifestId,
    ) -> Result<bool> {
        self.inner
            .activate_candidate_if_current(expected, candidate)
    }
    fn list_manifests(&self) -> Result<Vec<ManifestMeta>> {
        self.inner.list_manifests()
    }
    fn remove_manifest(&self, id: &ManifestId) -> Result<()> {
        self.inner.remove_manifest(id)
    }
}

#[test]
fn manifest_open_pins_segments_before_concurrent_vacuum_can_reclaim_them() {
    manifest_open_vacuum_race(false);
}

#[test]
fn persisted_current_open_pins_segments_before_concurrent_vacuum_can_reclaim_them() {
    manifest_open_vacuum_race(true);
}

fn manifest_open_vacuum_race(persisted_current: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("index");
    let engine = Arc::new(
        IndexEngine::open(EngineConfig {
            index_root: root.clone(),
            ..Default::default()
        })
        .unwrap(),
    );
    let doc = |name: &str| {
        let repo_id = RepoId("repo".into());
        IndexDocument {
            key: DocKey {
                repo_id: repo_id.clone(),
                logical_path: name.into(),
            },
            repo_id,
            logical_path: name.into(),
            file_name: name.into(),
            language: None,
            size: 6,
            modified_at: 1,
            content_hash: [0; 32],
            source_bytes: Arc::from(&b"TOKEN\n"[..]),
        }
    };
    engine.writer().add_document(doc("old.txt")).unwrap();
    engine.writer().commit().unwrap();
    let old = engine.admin().manifest().unwrap();
    let (loaded_tx, loaded_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let factory = IndexReaderFactory::new(
        Arc::new(PausedManifestStore {
            inner: FileManifestStore::open(&root).unwrap(),
            loaded: loaded_tx,
            resume: Mutex::new(resume_rx),
        }),
        Arc::new(FileSegmentStore::open(&root, PostingTokenizerConfig::default()).unwrap()),
        Arc::new(FileDeleteStore::open(&root).unwrap()),
        SharedSnapshotState::for_index_root(&root).unwrap(),
        PostingTokenizerConfig::default(),
    );
    let opening = std::thread::spawn(move || {
        if persisted_current {
            factory.open_persisted_current()
        } else {
            factory.open_at_manifest(old.manifest_id)
        }
    });
    loaded_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // Advance beyond retained history so vacuum would remove this manifest and
    // its unique segment unless the opening reader has registered a lease.
    for i in 0..3 {
        engine.writer().begin_replacement();
        engine
            .writer()
            .add_document(doc(&format!("new{i}.txt")))
            .unwrap();
        engine.writer().commit().unwrap();
    }

    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let gc_engine = Arc::clone(&engine);
    let gc = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = gc_engine.admin().vacuum();
        finished_tx.send(()).unwrap();
        result
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "vacuum must wait until the loaded manifest is pinned"
    );
    resume_tx.send(()).unwrap();
    let reader = opening.join().unwrap().unwrap();
    gc.join().unwrap().unwrap();
    let keys = reader.visible_keys().unwrap();
    assert_eq!(keys.live.len(), 1);
    assert_eq!(keys.live.iter().next().unwrap().logical_path, "old.txt");
    let segment = root
        .join("segments")
        .join(&reader.manifest().active_segments[0].id.0);
    assert!(segment.exists());
    drop(reader);
    engine.admin().vacuum().unwrap();
    assert!(
        !segment.exists(),
        "the old segment is reclaimable after the reader drops"
    );
}
