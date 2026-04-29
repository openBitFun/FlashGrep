use super::*;

impl IndexSnapshotReader {
    pub(super) fn can_use_single_segment_candidate_fast_path(&self) -> bool {
        self.snapshot.manifest().active_segments.len() == 1
            && self.snapshot.manifest().active_delete_segments.is_empty()
            && self.data.deleted_keys.is_empty()
            && self.data.docs.is_empty()
    }

    pub(super) fn can_use_streaming_segment_fast_path(&self, query: &QueryRequest) -> bool {
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

    pub(super) fn log_search_phases(
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

    pub(super) fn candidate_documents_single_segment(
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

    pub(super) fn search_single_segment_clean(
        &self,
        query: &QueryRequest,
        plan: &QueryPlan,
    ) -> Result<SearchResults> {
        self.search_single_segment_clean_profiled(query, plan, &mut SearchPhaseProfile::default())
    }

    pub(super) fn search_single_segment_clean_profiled(
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

    pub(super) fn search_multi_segment_clean_profiled(
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
