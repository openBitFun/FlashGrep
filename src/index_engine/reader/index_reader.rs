use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

mod candidates;
mod catalog;
mod diagnostics;
mod fast_path;
mod query_helpers;
mod visibility_cache;

use rayon::prelude::*;

use crate::{
    config::QueryConfig,
    error::AppError,
    index_engine::{
        config::PostingTokenizerConfig,
        delete::DeleteStore,
        manifest::{Manifest, ManifestSnapshot, ManifestStore},
        query::{QueryPlan, QueryRequest},
        reader::state::{
            SharedSnapshotState, SnapshotData, SnapshotLease, SnapshotVisibilityState,
        },
        segment::{collect_document_posting_terms, SegmentStore, SegmentVerifySource},
        types::DocKey,
        writer::IndexDocument,
    },
    planner::plan as build_regex_query_plan,
    query_runtime::{
        requires_multiline_verification, verify_candidate_sources_with_resolver, CountVerifyKind,
        CountVerifyPlan, LinePrefilter, SearchVerifyPlan, VerifyCandidateSource,
    },
    search::{FileCount, FileMatchCount, SearchResults},
    search_engine::{search_documents, SearchDocument},
    Result,
};
use regex::RegexBuilder;

pub use self::catalog::{SnapshotPathCatalog, SnapshotPathEntry, SnapshotVisibleKeys};

use self::query_helpers::{
    document_matches_query_plan, empty_results, matches_filters, matches_key_filters,
    query_request_to_runtime_config,
};

#[cfg(test)]
use self::query_helpers::document_matches_literal_sequence_plan;

// Single-segment snapshots are common for base builds. Keeping this threshold in the tens of
// thousands makes medium candidate sets serialize verification on one thread, which is too slow
// for monorepo queries even when posting recall is already selective.
const SINGLE_SEGMENT_PARALLEL_VERIFY_DOC_THRESHOLD: usize = 128;

#[derive(Debug, Clone, Default)]
pub struct SearchProfile {
    pub candidate_docs: usize,
    pub active_segments: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SearchPhaseProfile {
    pub prepare: Duration,
    pub collect_candidates: Duration,
    pub filter_candidates: Duration,
    pub verify: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct CandidateBuildProfile {
    pub load_deletes: Duration,
    pub open_segments: Duration,
    pub visibility_scan: Duration,
    pub collect_documents: Duration,
    pub overlay_docs: Duration,
    pub final_filter: Duration,
    pub total: Duration,
    pub visibility_scanned_docs: usize,
    pub candidate_doc_visits: usize,
    pub materialized_docs: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DetailedSearchProfile {
    pub active_segments: usize,
    pub used_single_segment_fast_path: bool,
    pub used_streaming_segment_fast_path: bool,
    pub candidate_docs: usize,
    pub searches_with_match: usize,
    pub matched_lines: usize,
    pub matched_occurrences: usize,
    pub bytes_searched: u64,
    pub hits: usize,
    pub phases: SearchPhaseProfile,
    pub candidate_build: Option<CandidateBuildProfile>,
}

#[derive(Debug, Clone)]
pub struct DryRunSegmentProfile {
    pub segment_id: crate::index_engine::types::SegmentId,
    pub filtered_doc_ids: usize,
    pub candidate_doc_ids: usize,
    pub visible_candidate_docs: usize,
    pub estimated_verify_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct DryRunQueryProfile {
    pub plan: QueryPlan,
    pub fallback_to_scan: bool,
    pub active_segments: usize,
    pub total_filtered_doc_ids: usize,
    pub total_candidate_doc_ids: usize,
    pub candidate_docs: usize,
    pub estimated_verify_bytes: u64,
    pub segments: Vec<DryRunSegmentProfile>,
}

#[derive(Debug, Default)]
struct SegmentCountResults {
    candidate_docs: usize,
    searches_with_match: usize,
    bytes_searched: u64,
    matched_lines: usize,
    matched_occurrences: usize,
    file_counts: Vec<FileCount>,
    file_match_counts: Vec<FileMatchCount>,
}

#[derive(Debug, Default, Clone, Copy)]
struct CandidateSourceProfile {
    inline_docs: usize,
    inline_bytes: u64,
    worktree_docs: usize,
    worktree_bytes: u64,
    git_head_blob_docs: usize,
    git_head_blob_bytes: u64,
}

pub struct IndexReaderFactory {
    manifest_store: Arc<dyn ManifestStore>,
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
    snapshots: SharedSnapshotState,
    tokenizer: PostingTokenizerConfig,
}

impl IndexReaderFactory {
    pub(crate) fn new(
        manifest_store: Arc<dyn ManifestStore>,
        segment_store: Arc<dyn SegmentStore>,
        delete_store: Arc<dyn DeleteStore>,
        snapshots: SharedSnapshotState,
        tokenizer: PostingTokenizerConfig,
    ) -> Self {
        Self {
            manifest_store,
            segment_store,
            delete_store,
            snapshots,
            tokenizer,
        }
    }

    pub fn open_snapshot(&self) -> Result<IndexSnapshotReader> {
        let snapshot = self.snapshots.load_refreshed();
        let lease = self
            .snapshots
            .acquire_lease(snapshot.manifest.manifest_id.clone());
        Ok(IndexSnapshotReader::new(
            ManifestSnapshot::new(snapshot.manifest.clone()),
            Arc::clone(&self.segment_store),
            Arc::clone(&self.delete_store),
            lease,
            snapshot,
            self.tokenizer.clone(),
        ))
    }

    pub fn open_committed(&self) -> Result<IndexSnapshotReader> {
        let snapshot = self.snapshots.load_committed();
        let lease = self
            .snapshots
            .acquire_lease(snapshot.manifest.manifest_id.clone());
        Ok(IndexSnapshotReader::new(
            ManifestSnapshot::new(snapshot.manifest.clone()),
            Arc::clone(&self.segment_store),
            Arc::clone(&self.delete_store),
            lease,
            snapshot,
            self.tokenizer.clone(),
        ))
    }

    pub fn open_persisted_current(&self) -> Result<IndexSnapshotReader> {
        let manifest = self.manifest_store.load_current()?;
        Ok(self.open_with_manifest(manifest))
    }

    pub fn open_at_manifest(
        &self,
        manifest_id: crate::index_engine::types::ManifestId,
    ) -> Result<IndexSnapshotReader> {
        let manifest = self.manifest_store.load_manifest(&manifest_id)?;
        if manifest.manifest_id != manifest_id {
            return Err(AppError::InvalidIndex(format!(
                "requested manifest {} but loaded {}",
                manifest_id.0, manifest.manifest_id.0
            )));
        }
        Ok(self.open_with_manifest(manifest))
    }

    fn open_with_manifest(&self, manifest: Manifest) -> IndexSnapshotReader {
        let lease = self.snapshots.acquire_lease(manifest.manifest_id.clone());
        IndexSnapshotReader::new(
            ManifestSnapshot::new(manifest.clone()),
            Arc::clone(&self.segment_store),
            Arc::clone(&self.delete_store),
            lease,
            SnapshotData {
                manifest,
                ..SnapshotData::default()
            },
            self.tokenizer.clone(),
        )
    }
}

#[derive(Clone)]
pub struct IndexSnapshotReader {
    snapshot: ManifestSnapshot,
    segment_store: Arc<dyn SegmentStore>,
    delete_store: Arc<dyn DeleteStore>,
    _lease: SnapshotLease,
    data: SnapshotData,
    tokenizer: PostingTokenizerConfig,
}

impl IndexSnapshotReader {
    pub(crate) fn new(
        snapshot: ManifestSnapshot,
        segment_store: Arc<dyn SegmentStore>,
        delete_store: Arc<dyn DeleteStore>,
        lease: SnapshotLease,
        data: SnapshotData,
        tokenizer: PostingTokenizerConfig,
    ) -> Self {
        Self {
            snapshot,
            segment_store,
            delete_store,
            _lease: lease,
            data,
            tokenizer,
        }
    }

    pub fn prewarm_visibility_cache(&self) -> Result<()> {
        let _ = self.cached_visibility_state(None)?;
        Ok(())
    }

    pub fn prewarm_search_state(&self) -> Result<()> {
        self.prewarm_visibility_cache()
    }

    pub fn manifest(&self) -> &Manifest {
        self.snapshot.manifest()
    }

    pub fn visible_documents(&self) -> Result<std::collections::BTreeMap<DocKey, IndexDocument>> {
        let mut effective_docs = std::collections::BTreeMap::<
            DocKey,
            (crate::index_engine::types::SeqNo, IndexDocument),
        >::new();
        let mut latest_delete_seq =
            std::collections::BTreeMap::<DocKey, crate::index_engine::types::SeqNo>::new();

        for delete_segment_ref in &self.snapshot.manifest().active_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment_ref.id.clone())?;
            for record in reader.records() {
                latest_delete_seq
                    .entry(record.key.clone())
                    .and_modify(|seq_no| *seq_no = (*seq_no).max(record.seq_no))
                    .or_insert(record.seq_no);
            }
        }

        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        for segment_ref in &active_segments {
            let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                let document = reader.document(doc_id)?;
                if latest_delete_seq
                    .get(&document.key)
                    .is_some_and(|seq_no| *seq_no > segment_ref.max_seq_no)
                {
                    continue;
                }

                let replace = effective_docs
                    .get(&document.key)
                    .is_none_or(|(seq_no, _)| *seq_no < segment_ref.max_seq_no);
                if replace {
                    effective_docs.insert(
                        document.key.clone(),
                        (segment_ref.max_seq_no, document.clone()),
                    );
                }
            }
        }

        for key in &self.data.deleted_keys {
            effective_docs.remove(key);
        }
        for (key, entry) in &self.data.docs {
            effective_docs.insert(key.clone(), (entry.seq_no, entry.document.clone()));
        }

        Ok(effective_docs
            .into_iter()
            .map(|(key, (_, document))| (key, document))
            .collect())
    }

    pub fn path_catalog(&self) -> Result<SnapshotPathCatalog> {
        let mut effective_entries = std::collections::BTreeMap::<
            DocKey,
            (crate::index_engine::types::SeqNo, SnapshotPathEntry),
        >::new();
        let mut latest_delete_seq =
            std::collections::BTreeMap::<DocKey, crate::index_engine::types::SeqNo>::new();

        for delete_segment_ref in &self.snapshot.manifest().active_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment_ref.id.clone())?;
            for record in reader.records() {
                latest_delete_seq
                    .entry(record.key.clone())
                    .and_modify(|seq_no| *seq_no = (*seq_no).max(record.seq_no))
                    .or_insert(record.seq_no);
            }
        }

        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        for segment_ref in &active_segments {
            let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                let metadata = reader.doc_metadata(doc_id)?;
                if latest_delete_seq
                    .get(metadata.key)
                    .is_some_and(|seq_no| *seq_no > segment_ref.max_seq_no)
                {
                    continue;
                }

                let replace = effective_entries
                    .get(metadata.key)
                    .is_none_or(|(seq_no, _)| *seq_no < segment_ref.max_seq_no);
                if replace {
                    effective_entries.insert(
                        metadata.key.clone(),
                        (
                            segment_ref.max_seq_no,
                            SnapshotPathEntry {
                                logical_path: metadata.key.logical_path.clone(),
                                size: metadata.size,
                                modified_at: metadata.modified_at,
                                content_hash: metadata.content_hash,
                            },
                        ),
                    );
                }
            }
        }

        for key in &self.data.deleted_keys {
            effective_entries.remove(key);
        }
        for (key, entry) in &self.data.docs {
            effective_entries.insert(
                key.clone(),
                (
                    entry.seq_no,
                    SnapshotPathEntry {
                        logical_path: entry.document.logical_path.clone(),
                        size: entry.document.size,
                        modified_at: entry.document.modified_at,
                        content_hash: entry.document.content_hash,
                    },
                ),
            );
        }

        Ok(SnapshotPathCatalog {
            entries: effective_entries
                .into_iter()
                .map(|(_, (_, entry))| (entry.logical_path.clone(), entry))
                .collect(),
        })
    }

    pub fn visible_keys(&self) -> Result<SnapshotVisibleKeys> {
        let visibility = self.cached_visibility_state(None)?;
        Ok(SnapshotVisibleKeys {
            live: visibility.live_keys.clone(),
            deleted: visibility.deleted_keys.clone(),
        })
    }

    pub fn search(&self, query: &QueryRequest) -> Result<SearchResults> {
        let total_started = Instant::now();
        let plan = QueryPlan::build(query.clone(), &self.tokenizer);

        if self.can_use_single_segment_candidate_fast_path() && query.filters.languages.is_empty() {
            let mut phases = SearchPhaseProfile::default();
            let results = self.search_single_segment_clean_profiled(query, &plan, &mut phases)?;
            phases.total = total_started.elapsed();
            self.log_search_phases("single_segment_clean", query, &phases, &results);
            return Ok(results);
        }
        if self.can_use_streaming_segment_fast_path(query) {
            let mut phases = SearchPhaseProfile::default();
            let results = self.search_multi_segment_clean_profiled(query, &plan, &mut phases)?;
            phases.total = total_started.elapsed();
            self.log_search_phases("multi_segment_clean", query, &phases, &results);
            return Ok(results);
        }

        let regex = RegexBuilder::new(&query.pattern)
            .case_insensitive(query.case_insensitive)
            .build()?;
        self.search_with_regex(query, &plan, &regex)
    }

    fn search_with_regex(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        _regex: &regex::Regex,
    ) -> Result<SearchResults> {
        self.search_with_regex_profiled(query, plan, None, None)
    }

    fn search_with_regex_profiled(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        mut phases: Option<&mut SearchPhaseProfile>,
        candidate_build: Option<&mut CandidateBuildProfile>,
    ) -> Result<SearchResults> {
        let candidate_started = Instant::now();
        let effective_docs = self.candidate_documents_profiled(query, plan, candidate_build)?;
        if let Some(phases) = phases.as_deref_mut() {
            phases.collect_candidates = candidate_started.elapsed();
        }

        let verify_started = Instant::now();
        let config = query_request_to_runtime_config(query);
        let documents = effective_docs
            .values()
            .filter(|doc| matches_filters(doc, query))
            .map(|doc| {
                SearchDocument::from_loaded_bytes(
                    doc.logical_path.clone(),
                    doc.size,
                    doc.modified_at,
                    doc.source_bytes.clone(),
                )
            })
            .collect::<Vec<_>>();
        let results = search_documents(&config, &documents)?;
        if let Some(phases) = phases.as_mut() {
            phases.verify = verify_started.elapsed();
        }

        Ok(results)
    }

    pub fn candidate_paths(&self, query: &QueryRequest) -> Result<Vec<String>> {
        let plan = QueryPlan::build(query.clone(), &self.tokenizer);
        Ok(self
            .candidate_documents(query, &plan)?
            .into_values()
            .map(|document| document.logical_path)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::document_matches_literal_sequence_plan;
    use crate::{
        index_engine::{
            query::{QueryFilters, QueryPlan, QueryRequest},
            types::{DocKey, RepoId, ShardId},
            writer::IndexDocument,
            EngineConfig, IndexEngine,
        },
        search::SearchMode,
        TokenizerMode,
    };

    fn doc(body: &str) -> IndexDocument {
        let repo_id = RepoId("repo".into());
        let logical_path = "file.rs".to_string();
        IndexDocument {
            key: DocKey {
                repo_id: repo_id.clone(),
                logical_path: logical_path.clone(),
            },
            repo_id,
            logical_path,
            file_name: "file.rs".into(),
            language: Some("rust".into()),
            size: body.len() as u64,
            modified_at: 0,
            content_hash: [0; 32],
            source_bytes: Arc::<[u8]>::from(body.as_bytes()),
        }
    }

    fn request(pattern: &str, case_insensitive: bool) -> QueryRequest {
        QueryRequest {
            pattern: pattern.into(),
            mode: SearchMode::CountOnly,
            case_insensitive,
            max_count: None,
            top_k: None,
            filters: QueryFilters::default(),
            prefer_worktree_for_git_head_blob: false,
        }
    }

    fn request_with_mode(pattern: &str, mode: SearchMode, top_k: Option<usize>) -> QueryRequest {
        QueryRequest {
            mode,
            top_k,
            ..request(pattern, false)
        }
    }

    fn trigram_tokenizer() -> crate::index_engine::config::PostingTokenizerConfig {
        crate::index_engine::config::PostingTokenizerConfig {
            mode: TokenizerMode::Trigram,
            min_sparse_len: 3,
            max_sparse_len: 8,
        }
    }

    #[test]
    fn ordered_literal_filter_rejects_wrong_order() {
        let plan = QueryPlan::build(request("foo.*bar", false), &trigram_tokenizer());
        assert!(document_matches_literal_sequence_plan(
            &doc("foo hello bar"),
            &plan
        ));
        assert!(!document_matches_literal_sequence_plan(
            &doc("bar hello foo"),
            &plan
        ));
    }

    #[test]
    fn ordered_literal_filter_respects_branch_alternation() {
        let plan = QueryPlan::build(request("foo.*bar|baz.*qux", false), &trigram_tokenizer());
        assert!(document_matches_literal_sequence_plan(
            &doc("baz hello qux"),
            &plan
        ));
        assert!(!document_matches_literal_sequence_plan(
            &doc("qux hello baz"),
            &plan
        ));
    }

    #[test]
    fn ordered_literal_filter_folds_case_when_requested() {
        let plan = QueryPlan::build(request("Foo.*BAR", true), &trigram_tokenizer());
        assert!(document_matches_literal_sequence_plan(
            &doc("xx foo yy bar zz"),
            &plan
        ));
        assert!(!document_matches_literal_sequence_plan(
            &doc("xx bar yy foo zz"),
            &plan
        ));
    }

    #[test]
    fn multi_segment_count_modes_honor_global_result_limit() {
        let temp = tempdir().expect("temp dir should succeed");
        let engine = IndexEngine::open(EngineConfig {
            shard_id: ShardId("limit-test".into()),
            index_root: temp.path().join("index"),
            tokenizer: trigram_tokenizer(),
            ..EngineConfig::default()
        })
        .expect("engine should open");
        let repo_id = RepoId("repo".into());

        for segment in 0..2 {
            for index in 0..30 {
                let logical_path = format!("seg{segment}/file_{index:02}.txt");
                engine
                    .writer()
                    .add_document(IndexDocument {
                        key: DocKey {
                            repo_id: repo_id.clone(),
                            logical_path: logical_path.clone(),
                        },
                        repo_id: repo_id.clone(),
                        logical_path,
                        file_name: format!("file_{index:02}.txt"),
                        language: Some("txt".into()),
                        size: 6,
                        modified_at: 0,
                        content_hash: [0; 32],
                        source_bytes: Arc::<[u8]>::from(b"MATCH\n".to_vec()),
                    })
                    .expect("document should add");
            }
            engine.writer().commit().expect("segment should commit");
        }

        assert_eq!(
            engine
                .admin()
                .manifest()
                .expect("manifest should load")
                .active_segments
                .len(),
            2
        );
        let reader = engine
            .reader()
            .open_committed()
            .expect("reader should open");

        let files = reader
            .search(&request_with_mode(
                "MATCH",
                SearchMode::FilesWithMatches,
                Some(20),
            ))
            .expect("files search should succeed");
        assert_eq!(files.searches_with_match, 20);
        assert_eq!(files.matched_paths.len(), 20);

        let line_counts = reader
            .search(&request_with_mode("MATCH", SearchMode::CountOnly, Some(20)))
            .expect("count search should succeed");
        assert_eq!(line_counts.searches_with_match, 20);
        assert_eq!(line_counts.file_counts.len(), 20);
        assert_eq!(line_counts.matched_lines, 20);

        let occurrence_counts = reader
            .search(&request_with_mode(
                "MATCH",
                SearchMode::CountMatches,
                Some(20),
            ))
            .expect("count matches search should succeed");
        assert_eq!(occurrence_counts.searches_with_match, 20);
        assert_eq!(occurrence_counts.file_match_counts.len(), 20);
        assert_eq!(occurrence_counts.matched_occurrences, 20);
    }
}
