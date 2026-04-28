use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

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
    search::{
        FileCount, FileMatch, FileMatchCount, MatchLocation, SearchHit, SearchLine, SearchResults,
    },
    Result,
};
use regex::RegexBuilder;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPathEntry {
    pub logical_path: String,
    pub size: u64,
    pub modified_at: u64,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotPathCatalog {
    entries: std::collections::BTreeMap<String, SnapshotPathEntry>,
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
    pub live: std::collections::BTreeSet<DocKey>,
    pub deleted: std::collections::BTreeSet<DocKey>,
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

    pub fn profile_search_detailed(&self, query: &QueryRequest) -> Result<DetailedSearchProfile> {
        let total_started = Instant::now();
        let prepare_started = Instant::now();
        let plan = QueryPlan::build(query.clone(), &self.tokenizer);

        if self.can_use_single_segment_candidate_fast_path() && query.filters.languages.is_empty() {
            let mut phases = SearchPhaseProfile {
                prepare: prepare_started.elapsed(),
                ..SearchPhaseProfile::default()
            };
            let results = self.search_single_segment_clean_profiled(query, &plan, &mut phases)?;
            phases.total = total_started.elapsed();
            return Ok(DetailedSearchProfile {
                active_segments: self.snapshot.manifest().active_segments.len(),
                used_single_segment_fast_path: true,
                used_streaming_segment_fast_path: false,
                candidate_docs: results.candidate_docs,
                searches_with_match: results.searches_with_match,
                matched_lines: results.matched_lines,
                matched_occurrences: results.matched_occurrences,
                bytes_searched: results.bytes_searched,
                hits: results.hits.len(),
                phases,
                candidate_build: None,
            });
        }
        if self.can_use_streaming_segment_fast_path(query) {
            let mut phases = SearchPhaseProfile {
                prepare: prepare_started.elapsed(),
                ..SearchPhaseProfile::default()
            };
            let results = self.search_multi_segment_clean_profiled(query, &plan, &mut phases)?;
            phases.total = total_started.elapsed();
            return Ok(DetailedSearchProfile {
                active_segments: self.snapshot.manifest().active_segments.len(),
                used_single_segment_fast_path: false,
                used_streaming_segment_fast_path: true,
                candidate_docs: results.candidate_docs,
                searches_with_match: results.searches_with_match,
                matched_lines: results.matched_lines,
                matched_occurrences: results.matched_occurrences,
                bytes_searched: results.bytes_searched,
                hits: results.hits.len(),
                phases,
                candidate_build: None,
            });
        }

        let regex = RegexBuilder::new(&query.pattern)
            .case_insensitive(query.case_insensitive)
            .build()?;
        let prepare_elapsed = prepare_started.elapsed();
        let mut phases = SearchPhaseProfile {
            prepare: prepare_elapsed,
            ..SearchPhaseProfile::default()
        };
        let mut candidate_build = CandidateBuildProfile::default();
        let results = self.search_with_regex_profiled(
            query,
            &plan,
            &regex,
            Some(&mut phases),
            Some(&mut candidate_build),
        )?;
        phases.total = total_started.elapsed();
        candidate_build.total = phases.collect_candidates;
        Ok(DetailedSearchProfile {
            active_segments: self.snapshot.manifest().active_segments.len(),
            used_single_segment_fast_path: false,
            used_streaming_segment_fast_path: false,
            candidate_docs: results.candidate_docs,
            searches_with_match: results.searches_with_match,
            matched_lines: results.matched_lines,
            matched_occurrences: results.matched_occurrences,
            bytes_searched: results.bytes_searched,
            hits: results.hits.len(),
            phases,
            candidate_build: Some(candidate_build),
        })
    }

    fn search_with_regex(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        regex: &regex::Regex,
    ) -> Result<SearchResults> {
        self.search_with_regex_profiled(query, plan, regex, None, None)
    }

    fn search_with_regex_profiled(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        regex: &regex::Regex,
        mut phases: Option<&mut SearchPhaseProfile>,
        candidate_build: Option<&mut CandidateBuildProfile>,
    ) -> Result<SearchResults> {
        let mut results = empty_results();
        let candidate_started = Instant::now();
        let effective_docs = self.candidate_documents_profiled(query, plan, candidate_build)?;
        if let Some(phases) = phases.as_deref_mut() {
            phases.collect_candidates = candidate_started.elapsed();
        }

        results.candidate_docs = effective_docs.len();
        results.bytes_searched = effective_docs
            .values()
            .map(|document| document.source_bytes.len() as u64)
            .sum();

        let verify_started = Instant::now();
        for doc in effective_docs.values() {
            if !matches_filters(doc, query) {
                results.candidate_docs -= 1;
                results.bytes_searched -= doc.source_bytes.len() as u64;
                continue;
            }

            accumulate_doc_search(doc, query, regex, &mut results);
            if query.top_k.is_some_and(|limit| results.hits.len() >= limit) {
                break;
            }
        }
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

    pub fn profile_search(&self, query: &QueryRequest) -> Result<SearchProfile> {
        let results = self.search(query)?;
        Ok(SearchProfile {
            candidate_docs: results.candidate_docs,
            active_segments: self.snapshot.manifest().active_segments.len(),
        })
    }

    pub fn dry_run_query(&self, query: &QueryRequest) -> Result<DryRunQueryProfile> {
        let plan = QueryPlan::build(query.clone(), &self.tokenizer);
        let visibility = self.cached_visibility_state(None)?;

        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        let mut opened_segments = Vec::with_capacity(active_segments.len());
        for segment_ref in &active_segments {
            opened_segments.push((
                segment_ref.clone(),
                self.segment_store.open_segment(segment_ref.id.clone())?,
            ));
        }

        let mut visible_candidates = std::collections::BTreeSet::<DocKey>::new();
        let mut total_filtered_doc_ids = 0usize;
        let mut total_candidate_doc_ids = 0usize;
        let mut estimated_verify_bytes = 0u64;
        let mut segments = Vec::with_capacity(opened_segments.len());

        for (segment_ref, reader) in &opened_segments {
            let filtered_doc_ids = reader.collect_doc_ids_for_filters(&query.filters)?;
            let candidate_ids = reader.collect_candidates(&plan, 0)?;
            total_filtered_doc_ids += filtered_doc_ids.len();
            total_candidate_doc_ids += candidate_ids.len();
            let segment_candidate_doc_ids = candidate_ids.len();

            let mut segment_visible_candidate_docs = 0usize;
            let mut segment_estimated_verify_bytes = 0u64;
            for doc_id in candidate_ids {
                let key = reader.doc_key(doc_id)?;
                if !matches_key_filters(key, query) {
                    continue;
                }
                if visibility
                    .latest_live_segment_seq
                    .get(key)
                    .is_some_and(|seq_no| *seq_no != segment_ref.max_seq_no)
                {
                    continue;
                }
                if !visibility.latest_live_segment_seq.contains_key(key) {
                    continue;
                }
                let metadata = reader.doc_metadata(doc_id)?;
                segment_visible_candidate_docs += 1;
                segment_estimated_verify_bytes += metadata.size;
                visible_candidates.insert(key.clone());
            }

            segments.push(DryRunSegmentProfile {
                segment_id: segment_ref.id.clone(),
                filtered_doc_ids: filtered_doc_ids.len(),
                candidate_doc_ids: segment_candidate_doc_ids,
                visible_candidate_docs: segment_visible_candidate_docs,
                estimated_verify_bytes: segment_estimated_verify_bytes,
            });
            estimated_verify_bytes += segment_estimated_verify_bytes;
        }

        for (key, entry) in &self.data.docs {
            if matches_filters(&entry.document, query)
                && document_matches_query_plan(&entry.document, &plan, &self.tokenizer)
            {
                visible_candidates.insert(key.clone());
                estimated_verify_bytes += entry.document.size;
            }
        }

        Ok(DryRunQueryProfile {
            plan: plan.clone(),
            fallback_to_scan: plan.fallback_to_scan,
            active_segments: self.snapshot.manifest().active_segments.len(),
            total_filtered_doc_ids,
            total_candidate_doc_ids,
            candidate_docs: visible_candidates.len(),
            estimated_verify_bytes,
            segments,
        })
    }

    fn candidate_documents(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
    ) -> Result<std::collections::BTreeMap<DocKey, IndexDocument>> {
        self.candidate_documents_profiled(query, plan, None)
    }

    fn candidate_documents_profiled(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        mut profile: Option<&mut CandidateBuildProfile>,
    ) -> Result<std::collections::BTreeMap<DocKey, IndexDocument>> {
        if self.can_use_single_segment_candidate_fast_path() {
            return self.candidate_documents_single_segment(query, plan);
        }

        let mut effective_docs = std::collections::BTreeMap::<DocKey, IndexDocument>::new();
        let visibility = self.cached_visibility_state(profile.as_deref_mut())?;

        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        let open_started = Instant::now();
        let mut opened_segments = Vec::with_capacity(active_segments.len());
        for segment_ref in &active_segments {
            opened_segments.push((
                segment_ref.clone(),
                self.segment_store.open_segment(segment_ref.id.clone())?,
            ));
        }
        if let Some(profile) = profile.as_deref_mut() {
            profile.open_segments += open_started.elapsed();
        }

        let collect_started = Instant::now();
        let mut candidate_doc_visits = 0usize;
        let mut materialized_docs = 0usize;
        for (segment_ref, reader) in &opened_segments {
            let candidate_ids = reader.collect_candidates(plan, 0)?;
            for doc_id in candidate_ids {
                candidate_doc_visits += 1;
                let key = reader.doc_key(doc_id)?;
                if !matches_key_filters(key, query) {
                    continue;
                }
                if visibility
                    .latest_live_segment_seq
                    .get(key)
                    .is_some_and(|seq_no| *seq_no != segment_ref.max_seq_no)
                {
                    continue;
                }
                if !visibility.latest_live_segment_seq.contains_key(key) {
                    continue;
                }
                let document = reader.document(doc_id)?;
                materialized_docs += 1;
                effective_docs
                    .entry(key.clone())
                    .or_insert_with(|| document.clone());
            }
        }
        if let Some(profile) = profile.as_deref_mut() {
            profile.collect_documents = collect_started.elapsed();
            profile.candidate_doc_visits = candidate_doc_visits;
            profile.materialized_docs = materialized_docs;
        }

        let overlay_started = Instant::now();
        for (key, entry) in &self.data.docs {
            if document_matches_query_plan(&entry.document, plan, &self.tokenizer) {
                effective_docs.insert(key.clone(), entry.document.clone());
            }
        }
        if let Some(profile) = profile.as_mut() {
            profile.overlay_docs = overlay_started.elapsed();
        }

        let filter_started = Instant::now();
        effective_docs.retain(|_, doc| matches_filters(doc, query));
        if let Some(profile) = profile.as_mut() {
            profile.final_filter = filter_started.elapsed();
        }
        Ok(effective_docs)
    }

    fn cached_visibility_state(
        &self,
        profile: Option<&mut CandidateBuildProfile>,
    ) -> Result<Arc<SnapshotVisibilityState>> {
        if let Some(state) = self.data.visibility_cache.get() {
            return Ok(Arc::clone(state));
        }

        let state = Arc::new(self.build_visibility_state(profile)?);
        if self.data.visibility_cache.set(Arc::clone(&state)).is_err() {
            return self.data.visibility_cache.get().cloned().ok_or_else(|| {
                AppError::Protocol("failed to read cached snapshot visibility".into())
            });
        }
        Ok(state)
    }

    fn build_visibility_state(
        &self,
        mut profile: Option<&mut CandidateBuildProfile>,
    ) -> Result<SnapshotVisibilityState> {
        let mut latest_delete_seq =
            std::collections::BTreeMap::<DocKey, crate::index_engine::types::SeqNo>::new();
        let delete_started = Instant::now();
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
        if let Some(profile) = profile.as_deref_mut() {
            profile.load_deletes = delete_started.elapsed();
        }

        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        let open_started = Instant::now();
        let mut opened_segments = Vec::with_capacity(active_segments.len());
        for segment_ref in &active_segments {
            opened_segments.push((
                segment_ref.clone(),
                self.segment_store.open_segment(segment_ref.id.clone())?,
            ));
        }
        if let Some(profile) = profile.as_deref_mut() {
            profile.open_segments += open_started.elapsed();
        }

        let visibility_started = Instant::now();
        let mut latest_live_segment_seq =
            std::collections::BTreeMap::<DocKey, crate::index_engine::types::SeqNo>::new();
        let mut visibility_scanned_docs = 0usize;
        for (segment_ref, reader) in &opened_segments {
            for doc_id in 0..reader.doc_count() as crate::index_engine::types::SegmentDocId {
                visibility_scanned_docs += 1;
                let key = reader.doc_key(doc_id)?;
                if latest_live_segment_seq.contains_key(key)
                    || self.data.deleted_keys.contains(key)
                    || self.data.docs.contains_key(key)
                {
                    continue;
                }
                if latest_delete_seq
                    .get(key)
                    .is_some_and(|seq_no| *seq_no > segment_ref.max_seq_no)
                {
                    continue;
                }
                latest_live_segment_seq.insert(key.clone(), segment_ref.max_seq_no);
            }
        }
        if let Some(profile) = profile.as_mut() {
            profile.visibility_scan = visibility_started.elapsed();
            profile.visibility_scanned_docs = visibility_scanned_docs;
        }

        let mut live_keys = latest_live_segment_seq
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut deleted_keys = self.data.deleted_keys.clone();
        for key in self.data.docs.keys() {
            deleted_keys.remove(key);
            live_keys.insert(key.clone());
        }
        for key in latest_delete_seq.keys() {
            if !live_keys.contains(key) && !self.data.docs.contains_key(key) {
                deleted_keys.insert(key.clone());
            }
        }

        Ok(SnapshotVisibilityState {
            latest_live_segment_seq,
            latest_delete_seq,
            live_keys,
            deleted_keys,
        })
    }

    fn can_use_single_segment_candidate_fast_path(&self) -> bool {
        self.snapshot.manifest().active_segments.len() == 1
            && self.snapshot.manifest().active_delete_segments.is_empty()
            && self.data.deleted_keys.is_empty()
            && self.data.docs.is_empty()
    }

    fn can_use_streaming_segment_fast_path(&self, query: &QueryRequest) -> bool {
        self.snapshot.manifest().active_segments.len() > 1
            && self.data.deleted_keys.is_empty()
            && self.data.docs.is_empty()
            && query.filters.languages.is_empty()
            && matches!(
                query.mode,
                crate::search::SearchMode::CountOnly | crate::search::SearchMode::CountMatches
            )
    }

    fn verify_source_for_query(
        &self,
        reader: &crate::index_engine::segment::SegmentReaderHandle,
        query: &QueryRequest,
        doc_id: crate::index_engine::types::SegmentDocId,
    ) -> Result<VerifyCandidateSource> {
        Ok(
            match reader.verify_source_for_query(doc_id, query.prefer_worktree_for_git_head_blob)? {
                SegmentVerifySource::Bytes(bytes) => VerifyCandidateSource::Bytes(bytes),
                SegmentVerifySource::Mmap(mmap) => VerifyCandidateSource::Mmap(mmap),
                SegmentVerifySource::Path(path) => VerifyCandidateSource::Path(path),
            },
        )
    }

    fn profile_candidate_sources_enabled() -> bool {
        env::var_os("FLASHGREP_PROFILE_CANDIDATE_SOURCES").is_some()
    }

    fn profile_search_phases_enabled() -> bool {
        env::var_os("FLASHGREP_PROFILE_SEARCH_PHASES").is_some()
    }

    fn collect_candidate_source_profile(
        &self,
        reader: &crate::index_engine::segment::SegmentReaderHandle,
        candidate_ids: &[crate::index_engine::types::SegmentDocId],
    ) -> Result<CandidateSourceProfile> {
        let mut profile = CandidateSourceProfile::default();
        for &doc_id in candidate_ids {
            let metadata = reader.doc_metadata(doc_id)?;
            match reader.doc_source_kind(doc_id)? {
                crate::index_engine::segment::SegmentDocSourceKind::Inline => {
                    profile.inline_docs += 1;
                    profile.inline_bytes += metadata.size;
                }
                crate::index_engine::segment::SegmentDocSourceKind::Worktree => {
                    profile.worktree_docs += 1;
                    profile.worktree_bytes += metadata.size;
                }
                crate::index_engine::segment::SegmentDocSourceKind::GitHeadBlob => {
                    profile.git_head_blob_docs += 1;
                    profile.git_head_blob_bytes += metadata.size;
                }
            }
        }
        Ok(profile)
    }

    fn log_candidate_source_profile(
        &self,
        label: &str,
        query: &QueryRequest,
        profile: CandidateSourceProfile,
    ) {
        if !Self::profile_candidate_sources_enabled() {
            return;
        }
        eprintln!(
            "candidate_sources label={label} pattern={:?} prefer_worktree_for_git_head_blob={} inline_docs={} inline_bytes={} worktree_docs={} worktree_bytes={} git_head_blob_docs={} git_head_blob_bytes={}",
            query.pattern,
            query.prefer_worktree_for_git_head_blob,
            profile.inline_docs,
            profile.inline_bytes,
            profile.worktree_docs,
            profile.worktree_bytes,
            profile.git_head_blob_docs,
            profile.git_head_blob_bytes,
        );
    }

    fn log_search_phases(
        &self,
        label: &str,
        query: &QueryRequest,
        phases: &SearchPhaseProfile,
        results: &SearchResults,
    ) {
        if !Self::profile_search_phases_enabled() {
            return;
        }
        eprintln!(
            "search_phases label={label} pattern={:?} active_segments={} candidate_docs={} bytes_searched={} prepare_s={:.6} collect_s={:.6} filter_s={:.6} verify_s={:.6} total_s={:.6}",
            query.pattern,
            self.snapshot.manifest().active_segments.len(),
            results.candidate_docs,
            results.bytes_searched,
            phases.prepare.as_secs_f64(),
            phases.collect_candidates.as_secs_f64(),
            phases.filter_candidates.as_secs_f64(),
            phases.verify.as_secs_f64(),
            phases.total.as_secs_f64(),
        );
    }

    fn candidate_documents_single_segment(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
    ) -> Result<std::collections::BTreeMap<DocKey, IndexDocument>> {
        let segment_ref = self
            .snapshot
            .manifest()
            .active_segments
            .first()
            .ok_or_else(|| {
                AppError::InvalidIndex("single-segment fast path is missing a segment".into())
            })?;
        let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
        let mut effective_docs = std::collections::BTreeMap::<DocKey, IndexDocument>::new();

        // Clean base snapshots commonly have a single visible segment with no deletes or
        // overlay state. In that case, avoid a full visibility scan before verifying regexes.
        for doc_id in reader.collect_candidates(plan, 0)? {
            let key = reader.doc_key(doc_id)?;
            if !matches_key_filters(key, query) {
                continue;
            }

            let document = reader.document(doc_id)?;
            if !matches_filters(document, query) {
                continue;
            }

            effective_docs.insert(key.clone(), document.clone());
        }

        Ok(effective_docs)
    }

    fn search_single_segment_clean(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
    ) -> Result<SearchResults> {
        self.search_single_segment_clean_profiled(query, plan, &mut SearchPhaseProfile::default())
    }

    fn search_single_segment_clean_profiled(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        phases: &mut SearchPhaseProfile,
    ) -> Result<SearchResults> {
        let segment_ref = self
            .snapshot
            .manifest()
            .active_segments
            .first()
            .ok_or_else(|| {
                AppError::InvalidIndex("single-segment fast path is missing a segment".into())
            })?;
        let collect_started = Instant::now();
        let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
        let candidate_ids = reader.collect_candidates(plan, 0)?;
        phases.collect_candidates = collect_started.elapsed();
        let filter_started = Instant::now();
        let mut filtered_candidate_ids = Vec::with_capacity(candidate_ids.len());
        let mut candidate_bytes = 0u64;
        for doc_id in candidate_ids {
            let key = reader.doc_key(doc_id)?;
            if !matches_key_filters(key, query) {
                continue;
            }
            candidate_bytes += reader.doc_metadata(doc_id)?.size;
            filtered_candidate_ids.push(doc_id);
        }
        phases.filter_candidates = filter_started.elapsed();
        if Self::profile_candidate_sources_enabled() {
            let profile =
                self.collect_candidate_source_profile(&reader, &filtered_candidate_ids)?;
            self.log_candidate_source_profile("single_segment_clean", query, profile);
        }

        if matches!(
            query.mode,
            crate::search::SearchMode::CountOnly | crate::search::SearchMode::CountMatches
        ) {
            let verify_started = Instant::now();
            let config = query_request_to_runtime_config(query);
            let multiline_verifier = requires_multiline_verification(&config)?;
            let query_plan = build_regex_query_plan(&config.regex_pattern)?;
            let line_prefilter = (!multiline_verifier)
                .then(|| LinePrefilter::compile(&config, &query_plan))
                .flatten();
            let verify_plan = CountVerifyPlan::compile(
                &config,
                multiline_verifier,
                line_prefilter,
                if matches!(query.mode, crate::search::SearchMode::CountOnly) {
                    CountVerifyKind::Lines
                } else {
                    CountVerifyKind::Occurrences
                },
            )?;
            let counts = verify_plan.verify_candidate_counts_by_doc_with_sources(
                &filtered_candidate_ids,
                reader.doc_count(),
                &|doc_id| self.verify_source_for_query(&reader, query, doc_id),
            )?;
            let searches_with_match = counts.len();
            let matched_lines = counts.iter().map(|count| count.matched_lines).sum();
            let matched_occurrences = counts.iter().map(|count| count.matched_occurrences).sum();
            let (file_counts, file_match_counts) =
                if matches!(query.mode, crate::search::SearchMode::CountOnly) {
                    (
                        counts
                            .into_iter()
                            .map(|count| {
                                Ok(FileCount {
                                    path: reader.doc_key(count.doc_id)?.logical_path.clone(),
                                    matched_lines: count.matched_lines,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                        Vec::new(),
                    )
                } else {
                    (
                        Vec::new(),
                        counts
                            .into_iter()
                            .map(|count| {
                                Ok(FileMatchCount {
                                    path: reader.doc_key(count.doc_id)?.logical_path.clone(),
                                    matched_occurrences: count.matched_occurrences,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                    )
                };
            phases.verify = verify_started.elapsed();
            return Ok(SearchResults {
                candidate_docs: filtered_candidate_ids.len(),
                searches_with_match,
                bytes_searched: candidate_bytes,
                matched_lines,
                matched_occurrences,
                file_counts,
                file_match_counts,
                hits: Vec::new(),
            });
        }
        let config = query_request_to_runtime_config(query);
        let multiline_verifier = requires_multiline_verification(&config)?;
        let query_plan = build_regex_query_plan(&config.regex_pattern)?;
        let line_prefilter = (!multiline_verifier)
            .then(|| LinePrefilter::compile(&config, &query_plan))
            .flatten();
        let line_prefilter_ref = line_prefilter.as_ref();
        let verify_started = Instant::now();
        if query.top_k.is_none()
            && filtered_candidate_ids.len() >= SINGLE_SEGMENT_PARALLEL_VERIFY_DOC_THRESHOLD
        {
            let matcher = crate::query_runtime::build_matcher(&config, multiline_verifier)?;
            let outcomes = verify_candidate_sources_with_resolver(
                &filtered_candidate_ids,
                reader.doc_count(),
                &|doc_id| self.verify_source_for_query(&reader, query, doc_id),
                &matcher,
                line_prefilter_ref,
                multiline_verifier,
                query.mode,
                None,
                None,
                0,
                0,
            )?;
            let mut results = empty_results();
            results.candidate_docs = filtered_candidate_ids.len();
            for task in outcomes {
                results.bytes_searched += task.outcome.bytes_searched;
                results.matched_lines += task.outcome.matched_lines;
                results.matched_occurrences += task.outcome.matched_occurrences;
                if !task.outcome.matches.is_empty() {
                    results.searches_with_match += 1;
                    let path = reader.doc_key(task.doc_id)?.logical_path.clone();
                    results.hits.push(SearchHit {
                        path,
                        matches: task.outcome.matches,
                        lines: task.outcome.lines,
                    });
                } else if task.outcome.matched_lines > 0 || task.outcome.matched_occurrences > 0 {
                    results.searches_with_match += 1;
                }
            }
            phases.verify = verify_started.elapsed();
            return Ok(results);
        }

        let verify_plan =
            SearchVerifyPlan::compile(&config, multiline_verifier, line_prefilter.clone())?;
        let mut runtime = verify_plan.create_runtime();
        let mut results = empty_results();
        for doc_id in filtered_candidate_ids {
            results.candidate_docs += 1;
            let source = self.verify_source_for_query(&reader, query, doc_id)?;
            let outcome = runtime.verify_source(&source)?;
            results.bytes_searched += outcome.bytes_searched;
            results.matched_lines += outcome.matched_lines;
            results.matched_occurrences += outcome.matched_occurrences;
            if !outcome.matches.is_empty() {
                results.searches_with_match += 1;
                let path = reader.doc_key(doc_id)?.logical_path.clone();
                results.hits.push(SearchHit {
                    path,
                    matches: outcome.matches,
                    lines: outcome.lines,
                });
            } else if outcome.matched_lines > 0 || outcome.matched_occurrences > 0 {
                results.searches_with_match += 1;
            }
            if query.top_k.is_some_and(|limit| results.hits.len() >= limit) {
                phases.verify = verify_started.elapsed();
                return Ok(results);
            }
        }

        phases.verify = verify_started.elapsed();
        Ok(results)
    }

    fn search_multi_segment_clean_profiled(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
        phases: &mut SearchPhaseProfile,
    ) -> Result<SearchResults> {
        let visibility = self.cached_visibility_state(None)?;
        let mut active_segments = self.snapshot.manifest().active_segments.clone();
        active_segments.sort_by(|left, right| {
            right
                .max_seq_no
                .cmp(&left.max_seq_no)
                .then_with(|| right.min_seq_no.cmp(&left.min_seq_no))
        });

        let config = query_request_to_runtime_config(query);
        let multiline_verifier = requires_multiline_verification(&config)?;
        let query_plan = build_regex_query_plan(&config.regex_pattern)?;
        let line_prefilter = (!multiline_verifier)
            .then(|| LinePrefilter::compile(&config, &query_plan))
            .flatten();
        let line_prefilter_ref = line_prefilter.as_ref();

        let collect_started = Instant::now();
        let mut candidate_filter_elapsed = Duration::default();
        let mut collected_segments = Vec::with_capacity(active_segments.len());
        for segment_ref in &active_segments {
            let reader = self.segment_store.open_segment(segment_ref.id.clone())?;
            let candidate_ids = reader.collect_candidates(plan, 0)?;
            let filter_started = Instant::now();
            let mut filtered_candidate_ids = Vec::with_capacity(candidate_ids.len());
            let mut candidate_bytes = 0u64;
            for doc_id in candidate_ids {
                let key = reader.doc_key(doc_id)?;
                if !matches_key_filters(key, query) {
                    continue;
                }
                if visibility
                    .latest_live_segment_seq
                    .get(key)
                    .is_some_and(|seq_no| *seq_no != segment_ref.max_seq_no)
                    || !visibility.latest_live_segment_seq.contains_key(key)
                {
                    continue;
                }
                candidate_bytes += reader.doc_metadata(doc_id)?.size;
                filtered_candidate_ids.push(doc_id);
            }
            candidate_filter_elapsed += filter_started.elapsed();
            if !filtered_candidate_ids.is_empty() {
                collected_segments.push((reader, filtered_candidate_ids, candidate_bytes));
            }
        }
        phases.collect_candidates = collect_started.elapsed();
        phases.filter_candidates = candidate_filter_elapsed;
        if Self::profile_candidate_sources_enabled() {
            let mut profile = CandidateSourceProfile::default();
            for (reader, filtered_candidate_ids, _) in &collected_segments {
                let segment_profile =
                    self.collect_candidate_source_profile(reader, filtered_candidate_ids)?;
                profile.inline_docs += segment_profile.inline_docs;
                profile.inline_bytes += segment_profile.inline_bytes;
                profile.worktree_docs += segment_profile.worktree_docs;
                profile.worktree_bytes += segment_profile.worktree_bytes;
                profile.git_head_blob_docs += segment_profile.git_head_blob_docs;
                profile.git_head_blob_bytes += segment_profile.git_head_blob_bytes;
            }
            self.log_candidate_source_profile("multi_segment_clean", query, profile);
        }

        let verify_started = Instant::now();
        let mut results = empty_results();
        if matches!(
            query.mode,
            crate::search::SearchMode::CountOnly | crate::search::SearchMode::CountMatches
        ) {
            let verify_plan = CountVerifyPlan::compile(
                &config,
                multiline_verifier,
                line_prefilter,
                if matches!(query.mode, crate::search::SearchMode::CountOnly) {
                    CountVerifyKind::Lines
                } else {
                    CountVerifyKind::Occurrences
                },
            )?;
            let segment_results = collected_segments
                .into_par_iter()
                .map(
                    |(reader, filtered_candidate_ids, candidate_bytes)| -> Result<_> {
                        let counts = verify_plan.verify_candidate_counts_by_doc_with_sources(
                            &filtered_candidate_ids,
                            reader.doc_count(),
                            &|doc_id| self.verify_source_for_query(&reader, query, doc_id),
                        )?;

                        let mut segment = SegmentCountResults {
                            candidate_docs: filtered_candidate_ids.len(),
                            searches_with_match: counts.len(),
                            bytes_searched: candidate_bytes,
                            matched_lines: counts.iter().map(|count| count.matched_lines).sum(),
                            matched_occurrences: counts
                                .iter()
                                .map(|count| count.matched_occurrences)
                                .sum(),
                            ..SegmentCountResults::default()
                        };

                        if matches!(query.mode, crate::search::SearchMode::CountOnly) {
                            segment.file_counts = counts
                                .into_iter()
                                .map(|count| {
                                    Ok(FileCount {
                                        path: reader.doc_key(count.doc_id)?.logical_path.clone(),
                                        matched_lines: count.matched_lines,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                        } else {
                            segment.file_match_counts = counts
                                .into_iter()
                                .map(|count| {
                                    Ok(FileMatchCount {
                                        path: reader.doc_key(count.doc_id)?.logical_path.clone(),
                                        matched_occurrences: count.matched_occurrences,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                        }

                        Ok(segment)
                    },
                )
                .collect::<Vec<_>>();

            for segment in segment_results {
                let segment = segment?;
                results.candidate_docs += segment.candidate_docs;
                results.searches_with_match += segment.searches_with_match;
                results.bytes_searched += segment.bytes_searched;
                results.matched_lines += segment.matched_lines;
                results.matched_occurrences += segment.matched_occurrences;
                results.file_counts.extend(segment.file_counts);
                results.file_match_counts.extend(segment.file_match_counts);
            }
            results
                .file_counts
                .sort_unstable_by(|left, right| left.path.cmp(&right.path));
            results
                .file_match_counts
                .sort_unstable_by(|left, right| left.path.cmp(&right.path));
            phases.verify = verify_started.elapsed();
            return Ok(results);
        }

        let matcher = crate::query_runtime::build_matcher(&config, multiline_verifier)?;
        let verify_plan =
            SearchVerifyPlan::compile(&config, multiline_verifier, line_prefilter.clone())?;
        for (reader, filtered_candidate_ids, candidate_bytes) in collected_segments {
            results.candidate_docs += filtered_candidate_ids.len();
            results.bytes_searched += candidate_bytes;
            if query.top_k.is_none()
                && filtered_candidate_ids.len() >= SINGLE_SEGMENT_PARALLEL_VERIFY_DOC_THRESHOLD
            {
                let outcomes = verify_candidate_sources_with_resolver(
                    &filtered_candidate_ids,
                    reader.doc_count(),
                    &|doc_id| self.verify_source_for_query(&reader, query, doc_id),
                    &matcher,
                    line_prefilter_ref,
                    multiline_verifier,
                    query.mode,
                    None,
                    None,
                    0,
                    0,
                )?;
                for task in outcomes {
                    results.matched_lines += task.outcome.matched_lines;
                    results.matched_occurrences += task.outcome.matched_occurrences;
                    if !task.outcome.matches.is_empty() {
                        results.searches_with_match += 1;
                        let path = reader.doc_key(task.doc_id)?.logical_path.clone();
                        results.hits.push(SearchHit {
                            path,
                            matches: task.outcome.matches,
                            lines: task.outcome.lines,
                        });
                    } else if task.outcome.matched_lines > 0 || task.outcome.matched_occurrences > 0
                    {
                        results.searches_with_match += 1;
                    }
                }
                continue;
            }

            let mut runtime = verify_plan.create_runtime();
            for doc_id in filtered_candidate_ids {
                let source = self.verify_source_for_query(&reader, query, doc_id)?;
                let outcome = runtime.verify_source(&source)?;
                results.matched_lines += outcome.matched_lines;
                results.matched_occurrences += outcome.matched_occurrences;
                if !outcome.matches.is_empty() {
                    results.searches_with_match += 1;
                    let path = reader.doc_key(doc_id)?.logical_path.clone();
                    results.hits.push(SearchHit {
                        path,
                        matches: outcome.matches,
                        lines: outcome.lines,
                    });
                } else if outcome.matched_lines > 0 || outcome.matched_occurrences > 0 {
                    results.searches_with_match += 1;
                }
                if query.top_k.is_some_and(|limit| results.hits.len() >= limit) {
                    results
                        .hits
                        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
                    phases.verify = verify_started.elapsed();
                    return Ok(results);
                }
            }
        }

        results
            .hits
            .sort_unstable_by(|left, right| left.path.cmp(&right.path));
        phases.verify = verify_started.elapsed();
        Ok(results)
    }
}

fn query_request_to_runtime_config(query: &QueryRequest) -> QueryConfig {
    QueryConfig {
        regex_pattern: query.pattern.clone(),
        case_insensitive: query.case_insensitive,
        search_mode: query.mode,
        ..QueryConfig::default()
    }
}

fn matches_filters(doc: &crate::index_engine::writer::IndexDocument, query: &QueryRequest) -> bool {
    if !matches_key_filters(&doc.key, query) {
        return false;
    }
    if !query.filters.languages.is_empty()
        && !query
            .filters
            .languages
            .iter()
            .any(|language| doc.language.as_deref() == Some(language.as_str()))
    {
        return false;
    }
    true
}

fn document_matches_query_plan(
    doc: &IndexDocument,
    plan: &QueryPlan,
    tokenizer: &PostingTokenizerConfig,
) -> bool {
    if plan.fallback_to_scan || plan.candidate_branches.is_empty() {
        return true;
    }

    if !document_matches_literal_sequence_plan(doc, plan) {
        return false;
    }

    let terms = collect_document_posting_terms(&doc.source_bytes, tokenizer)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    plan.candidate_branches.iter().any(|branch| {
        branch.grams.is_empty() || branch.grams.iter().all(|gram| terms.contains(gram))
    })
}

fn document_matches_literal_sequence_plan(doc: &IndexDocument, plan: &QueryPlan) -> bool {
    let requires_case_fold = plan.request.case_insensitive;
    let text = String::from_utf8_lossy(&doc.source_bytes);
    let folded_storage;
    let haystack = if requires_case_fold {
        folded_storage = if text.is_ascii() {
            text.to_ascii_lowercase()
        } else {
            text.to_lowercase()
        };
        folded_storage.as_str()
    } else {
        text.as_ref()
    };

    plan.candidate_branches.iter().any(|branch| {
        branch.ordered_literals.is_empty()
            || literals_match_in_order(haystack, &branch.ordered_literals)
    })
}

fn literals_match_in_order(haystack: &str, literals: &[String]) -> bool {
    let mut search_from = 0usize;
    for literal in literals {
        let Some(offset) = haystack[search_from..].find(literal) else {
            return false;
        };
        search_from += offset + literal.len();
    }
    true
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(std::path::MAIN_SEPARATOR))
}

fn matches_key_filters(key: &DocKey, query: &QueryRequest) -> bool {
    if query
        .filters
        .repo
        .as_ref()
        .is_some_and(|repo| repo != &key.repo_id)
    {
        return false;
    }
    if !query.filters.path_prefixes.is_empty()
        && !query
            .filters
            .path_prefixes
            .iter()
            .any(|prefix| path_matches_prefix(&key.logical_path, prefix))
    {
        return false;
    }
    true
}

fn accumulate_doc_search(
    doc: &IndexDocument,
    query: &QueryRequest,
    regex: &regex::Regex,
    results: &mut SearchResults,
) {
    accumulate_search_bytes(&doc.logical_path, &doc.source_bytes, query, regex, results);
}

fn accumulate_search_bytes(
    path: &str,
    source_bytes: &[u8],
    query: &QueryRequest,
    regex: &regex::Regex,
    results: &mut SearchResults,
) {
    let text = String::from_utf8_lossy(source_bytes);
    let mut doc_matched_lines = 0usize;
    let mut doc_matched_occurrences = 0usize;
    let mut doc_matches = Vec::new();
    let materializes_matches = query.mode.materializes_matches();
    let first_hit_only = matches!(query.mode, crate::search::SearchMode::FirstHitOnly);

    for (line_index, line) in text.lines().enumerate() {
        if materializes_matches {
            let matches = regex.find_iter(line).collect::<Vec<_>>();
            if matches.is_empty() {
                continue;
            }

            doc_matched_lines += 1;
            doc_matched_occurrences += matches.len();
            for matched in matches {
                doc_matches.push(FileMatch {
                    location: MatchLocation {
                        line: line_index + 1,
                        column: matched.start() + 1,
                    },
                    snippet: line.to_string(),
                    matched_text: matched.as_str().to_string(),
                });
                if first_hit_only {
                    break;
                }
            }
            if first_hit_only && !doc_matches.is_empty() {
                break;
            }
            continue;
        }

        let match_count = regex.find_iter(line).count();
        if match_count == 0 {
            continue;
        }

        doc_matched_lines += 1;
        doc_matched_occurrences += match_count;
    }

    if doc_matched_lines == 0 {
        return;
    }

    results.searches_with_match += 1;
    results.matched_lines += doc_matched_lines;
    results.matched_occurrences += doc_matched_occurrences;
    results.file_counts.push(FileCount {
        path: path.to_string(),
        matched_lines: doc_matched_lines,
    });
    results.file_match_counts.push(FileMatchCount {
        path: path.to_string(),
        matched_occurrences: doc_matched_occurrences,
    });

    if materializes_matches {
        let lines = doc_matches
            .iter()
            .cloned()
            .map(SearchLine::Match)
            .collect::<Vec<_>>();
        results.hits.push(SearchHit {
            path: path.to_string(),
            matches: doc_matches,
            lines,
        });
    }
}

fn empty_results() -> SearchResults {
    SearchResults {
        candidate_docs: 0,
        searches_with_match: 0,
        bytes_searched: 0,
        matched_lines: 0,
        matched_occurrences: 0,
        file_counts: Vec::new(),
        file_match_counts: Vec::new(),
        hits: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::document_matches_literal_sequence_plan;
    use crate::{
        index_engine::{
            query::{QueryFilters, QueryPlan, QueryRequest},
            types::{DocKey, RepoId},
            writer::IndexDocument,
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
            top_k: None,
            filters: QueryFilters::default(),
            prefer_worktree_for_git_head_blob: false,
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
}
