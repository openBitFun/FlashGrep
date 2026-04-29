use super::*;

impl IndexSnapshotReader {
    pub(super) fn candidate_documents(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
    ) -> Result<std::collections::BTreeMap<DocKey, IndexDocument>> {
        self.candidate_documents_profiled(query, plan, None)
    }

    pub(super) fn candidate_documents_profiled(
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
}
