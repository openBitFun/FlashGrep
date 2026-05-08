use super::*;

impl IndexSnapshotReader {
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
                hits: results.result_units(query.mode),
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
                hits: results.result_units(query.mode),
                phases,
                candidate_build: None,
            });
        }

        let prepare_elapsed = prepare_started.elapsed();
        let mut phases = SearchPhaseProfile {
            prepare: prepare_elapsed,
            ..SearchPhaseProfile::default()
        };
        let mut candidate_build = CandidateBuildProfile::default();
        let results = self.search_with_regex_profiled(
            query,
            &plan,
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
            hits: results.result_units(query.mode),
            phases,
            candidate_build: Some(candidate_build),
        })
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
}
