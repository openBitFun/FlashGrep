use super::*;

impl IndexSnapshotReader {
    pub(super) fn cached_visibility_state(
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
        let mut latest_retraction_seq = std::collections::BTreeMap::<DocKey, u64>::new();
        let delete_started = Instant::now();
        for delete_segment_ref in &self.snapshot.manifest().active_delete_segments {
            let reader = self
                .delete_store
                .open_delete_segment(delete_segment_ref.id.clone())?;
            for record in reader.records() {
                if record.retract_to_base {
                    latest_retraction_seq
                        .entry(record.key.clone())
                        .and_modify(|seq| *seq = (*seq).max(record.seq_no))
                        .or_insert(record.seq_no);
                }
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
                    || self.data.retracted_keys.contains(key)
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
            if !live_keys.contains(key)
                && !self.data.docs.contains_key(key)
                && !self.data.retracted_keys.contains(key)
                && (self.data.deleted_keys.contains(key)
                    || latest_retraction_seq.get(key) != latest_delete_seq.get(key))
            {
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
}
