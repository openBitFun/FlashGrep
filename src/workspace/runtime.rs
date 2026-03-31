use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::PathBuf,
    time::UNIX_EPOCH,
};

use crate::{
    config::BuildConfig,
    error::Result,
    files::{read_text_file, scan_repository},
    index::{
        overlay::{ensure_fresh_overlay, OverlaySnapshotState},
        searcher::BranchLiteralDocIds,
        IndexSearcher, IndexWorktreeDiff,
    },
    planner::QueryPlan,
    search_engine::{SearchDocument, SearchDocumentSource},
    QueryConfig,
};

pub(super) struct CandidateRuntime<'a> {
    searcher: &'a IndexSearcher,
    diff: &'a IndexWorktreeDiff,
    query: &'a QueryConfig,
    build_config: &'a BuildConfig,
    overlay: Option<CachedOverlayRef<'a>>,
}

pub(super) struct EffectiveCandidateSet {
    by_path: BTreeMap<String, EffectiveCandidate>,
}

pub(super) struct ResolvedDocuments {
    documents: Vec<SearchDocument>,
}

#[derive(Debug, Clone)]
struct EffectiveCandidate {
    path: String,
}

struct PlannedWorkspaceQuery {
    plan: QueryPlan,
}

struct EffectiveIndexView<'a> {
    base_searcher: &'a IndexSearcher,
    shadowed_paths: HashSet<String>,
    overlay: OverlayLayer<'a>,
}

enum OverlayLayer<'a> {
    Borrowed {
        state: &'a OverlaySnapshotState,
        searcher: &'a IndexSearcher,
    },
    Owned {
        state: OverlaySnapshotState,
        searcher: IndexSearcher,
    },
    None,
}

#[derive(Clone, Copy)]
pub(super) struct CachedOverlayRef<'a> {
    pub(super) state: &'a OverlaySnapshotState,
    pub(super) searcher: &'a IndexSearcher,
}

pub(super) fn scan_text_repository_files(
    build_config: &BuildConfig,
) -> Result<Vec<SearchDocument>> {
    let mut files = Vec::new();
    for file in scan_repository(build_config)? {
        if read_text_file(&file.path)?.is_none() {
            continue;
        }
        files.push(SearchDocument::from_repository_file(&file));
    }
    Ok(files)
}

impl<'a> CandidateRuntime<'a> {
    pub(super) fn new(
        searcher: &'a IndexSearcher,
        diff: &'a IndexWorktreeDiff,
        query: &'a QueryConfig,
        build_config: &'a BuildConfig,
        overlay: Option<CachedOverlayRef<'a>>,
    ) -> Self {
        Self {
            searcher,
            diff,
            query,
            build_config,
            overlay,
        }
    }

    pub(super) fn candidate_documents(&self) -> Result<ResolvedDocuments> {
        let planned_query = PlannedWorkspaceQuery::new(self.query)?;
        let candidates = self.collect_candidates(&planned_query)?;
        resolve_documents(candidates)
    }

    fn collect_candidates(
        &self,
        planned_query: &PlannedWorkspaceQuery,
    ) -> Result<EffectiveCandidateSet> {
        if planned_query.plan.fallback_to_scan {
            return self.collect_candidates_fallback_scan(planned_query);
        }

        let view =
            EffectiveIndexView::new(self.searcher, self.diff, self.build_config, self.overlay)?;
        let use_allowed_paths = planned_query.plan.pure_literal_alternation.is_some();
        let clean_paths = use_allowed_paths.then(|| view.clean_base_paths());
        let base_branches = if let Some(clean_paths) = clean_paths.as_ref() {
            if clean_paths.is_empty() {
                planned_query.empty_literal_doc_id_branches()
            } else {
                view.base_searcher
                    .candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
                        self.query,
                        &planned_query.plan,
                        None,
                        Some(clean_paths),
                    )?
            }
        } else {
            view.base_searcher
                .candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
                    self.query,
                    &planned_query.plan,
                    None,
                    None,
                )?
        };
        let overlay_branches =
            view.overlay_literal_branches(self.query, planned_query, use_allowed_paths)?;

        let mut candidates = EffectiveCandidateSet::new();

        for branch_index in 0..planned_query.branch_count() {
            let branch_candidates = view.merge_literal_branch_candidates(
                &base_branches[branch_index],
                &overlay_branches[branch_index],
                use_allowed_paths,
            );
            candidates.extend_effective(branch_candidates);
        }
        Ok(candidates)
    }

    fn collect_candidates_fallback_scan(
        &self,
        planned_query: &PlannedWorkspaceQuery,
    ) -> Result<EffectiveCandidateSet> {
        let view =
            EffectiveIndexView::new(self.searcher, self.diff, self.build_config, self.overlay)?;
        let mut candidates = EffectiveCandidateSet::new();
        let use_allowed_paths = planned_query.plan.pure_literal_alternation.is_some();
        let clean_paths = use_allowed_paths.then(|| view.clean_base_paths());
        let base_branches = if let Some(clean_paths) = clean_paths.as_ref() {
            if clean_paths.is_empty() {
                planned_query.empty_doc_id_branches()
            } else {
                view.base_searcher
                    .candidate_doc_ids_by_branch_for_plan_with_allowed_paths(
                        self.query,
                        &planned_query.plan,
                        None,
                        Some(clean_paths),
                    )?
            }
        } else {
            view.base_searcher
                .candidate_doc_ids_by_branch_for_plan_with_allowed_paths(
                    self.query,
                    &planned_query.plan,
                    None,
                    None,
                )?
        };
        let overlay_branches =
            view.overlay_doc_id_branches(self.query, planned_query, use_allowed_paths)?;

        for branch_index in 0..planned_query.branch_count() {
            view.extend_base_candidates(
                &mut candidates,
                base_branches[branch_index].clone(),
                use_allowed_paths,
            );
            view.extend_overlay_candidates(&mut candidates, overlay_branches[branch_index].clone());
        }

        Ok(candidates)
    }
}

impl EffectiveCandidateSet {
    fn new() -> Self {
        Self {
            by_path: BTreeMap::new(),
        }
    }

    fn extend_doc_ids(
        &mut self,
        searcher: &IndexSearcher,
        doc_ids: Vec<u32>,
        excluded_paths: Option<&HashSet<String>>,
    ) {
        for doc_id in doc_ids {
            let Ok(path) = searcher.doc_display_path_by_id(doc_id) else {
                continue;
            };
            if excluded_paths.is_some_and(|paths| paths.contains(&path)) {
                continue;
            }
            self.by_path
                .insert(path.clone(), EffectiveCandidate { path });
        }
    }

    fn into_ordered(self) -> Vec<EffectiveCandidate> {
        self.by_path.into_values().collect()
    }

    fn intersect_with(&mut self, other: EffectiveCandidateSet) {
        self.by_path
            .retain(|path, _| other.by_path.contains_key(path));
    }

    fn extend_effective(&mut self, other: EffectiveCandidateSet) {
        self.by_path.extend(other.by_path);
    }

    fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

impl<'a> EffectiveIndexView<'a> {
    fn new(
        base_searcher: &'a IndexSearcher,
        diff: &'a IndexWorktreeDiff,
        build_config: &'a BuildConfig,
        overlay: Option<CachedOverlayRef<'a>>,
    ) -> Result<Self> {
        let shadowed_paths = shadowed_paths(diff);
        let overlay = match overlay {
            Some(overlay) => OverlayLayer::Borrowed {
                state: overlay.state,
                searcher: overlay.searcher,
            },
            None => match ensure_fresh_overlay(build_config, diff)? {
                Some(state) => OverlayLayer::Owned {
                    searcher: IndexSearcher::open(state.data_path.clone())?,
                    state,
                },
                None => OverlayLayer::None,
            },
        };

        Ok(Self {
            base_searcher,
            shadowed_paths,
            overlay,
        })
    }

    fn clean_base_paths(&self) -> HashSet<String> {
        clean_indexed_paths(self.base_searcher, &self.shadowed_paths)
    }

    fn extend_base_candidates(
        &self,
        candidates: &mut EffectiveCandidateSet,
        doc_ids: Vec<u32>,
        use_allowed_paths: bool,
    ) {
        candidates.extend_doc_ids(
            self.base_searcher,
            doc_ids,
            (!use_allowed_paths).then_some(&self.shadowed_paths),
        );
    }

    fn extend_overlay_candidates(&self, candidates: &mut EffectiveCandidateSet, doc_ids: Vec<u32>) {
        if let Some(overlay_searcher) = self.overlay_searcher() {
            candidates.extend_doc_ids(overlay_searcher, doc_ids, None);
        }
    }

    fn overlay_searcher(&self) -> Option<&IndexSearcher> {
        match &self.overlay {
            OverlayLayer::Borrowed { searcher, .. } => Some(searcher),
            OverlayLayer::Owned { searcher, .. } => Some(searcher),
            OverlayLayer::None => None,
        }
    }

    fn overlay_allowed_paths(&self) -> Option<&HashSet<String>> {
        match &self.overlay {
            OverlayLayer::Borrowed { state, .. } => Some(&state.indexed_paths),
            OverlayLayer::Owned { state, .. } => Some(&state.indexed_paths),
            OverlayLayer::None => None,
        }
    }

    fn overlay_literal_branches(
        &self,
        query: &QueryConfig,
        planned_query: &PlannedWorkspaceQuery,
        use_allowed_paths: bool,
    ) -> Result<Vec<BranchLiteralDocIds>> {
        let Some(overlay_searcher) = self.overlay_searcher() else {
            return Ok(planned_query.empty_literal_doc_id_branches());
        };

        if use_allowed_paths {
            let Some(overlay_allowed) = self.overlay_allowed_paths() else {
                return Ok(planned_query.empty_literal_doc_id_branches());
            };
            if overlay_allowed.is_empty() {
                return Ok(planned_query.empty_literal_doc_id_branches());
            }
            return overlay_searcher
                .candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
                    query,
                    &planned_query.plan,
                    None,
                    Some(overlay_allowed),
                );
        }

        overlay_searcher.candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
            query,
            &planned_query.plan,
            None,
            None,
        )
    }

    fn overlay_doc_id_branches(
        &self,
        query: &QueryConfig,
        planned_query: &PlannedWorkspaceQuery,
        use_allowed_paths: bool,
    ) -> Result<Vec<Vec<u32>>> {
        let Some(overlay_searcher) = self.overlay_searcher() else {
            return Ok(planned_query.empty_doc_id_branches());
        };

        if use_allowed_paths {
            let Some(overlay_allowed) = self.overlay_allowed_paths() else {
                return Ok(planned_query.empty_doc_id_branches());
            };
            if overlay_allowed.is_empty() {
                return Ok(planned_query.empty_doc_id_branches());
            }
            return overlay_searcher.candidate_doc_ids_by_branch_for_plan_with_allowed_paths(
                query,
                &planned_query.plan,
                None,
                Some(overlay_allowed),
            );
        }

        overlay_searcher.candidate_doc_ids_by_branch_for_plan_with_allowed_paths(
            query,
            &planned_query.plan,
            None,
            None,
        )
    }

    fn merge_literal_branch_candidates(
        &self,
        base_branch: &BranchLiteralDocIds,
        overlay_branch: &BranchLiteralDocIds,
        use_allowed_paths: bool,
    ) -> EffectiveCandidateSet {
        let mut branch_candidates = EffectiveCandidateSet::new();
        let mut has_branch_candidates = false;

        for literal_index in 0..base_branch.literals.len() {
            let mut literal_candidates = EffectiveCandidateSet::new();
            self.extend_base_candidates(
                &mut literal_candidates,
                base_branch.literals[literal_index].clone(),
                use_allowed_paths,
            );
            self.extend_overlay_candidates(
                &mut literal_candidates,
                overlay_branch.literals[literal_index].clone(),
            );

            if !has_branch_candidates {
                branch_candidates = literal_candidates;
                has_branch_candidates = true;
            } else {
                branch_candidates.intersect_with(literal_candidates);
            }

            if branch_candidates.is_empty() {
                break;
            }
        }

        if has_branch_candidates {
            branch_candidates
        } else {
            EffectiveCandidateSet::new()
        }
    }
}

pub(super) fn candidate_documents(
    searcher: &IndexSearcher,
    diff: &IndexWorktreeDiff,
    query: &QueryConfig,
    build_config: &BuildConfig,
    overlay: Option<CachedOverlayRef<'_>>,
) -> Result<ResolvedDocuments> {
    let runtime = CandidateRuntime::new(searcher, diff, query, build_config, overlay);
    runtime.candidate_documents()
}

impl ResolvedDocuments {
    pub(super) fn into_documents(self) -> Vec<SearchDocument> {
        self.documents
    }
}

fn shadowed_paths(diff: &IndexWorktreeDiff) -> HashSet<String> {
    diff.modified_files
        .iter()
        .chain(diff.deleted_files.iter())
        .chain(diff.new_files.iter())
        .cloned()
        .collect()
}

fn clean_indexed_paths(searcher: &IndexSearcher, shadowed: &HashSet<String>) -> HashSet<String> {
    searcher
        .indexed_paths_with_allowed_paths(None, None)
        .into_iter()
        .filter(|path| !shadowed.contains(path))
        .collect()
}

fn search_document_from_path(logical_path: String, path: PathBuf) -> Result<SearchDocument> {
    let metadata = fs::metadata(&path)?;
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| match u64::try_from(duration.as_nanos()) {
            Ok(value) => value,
            Err(_) => u64::MAX,
        })
        .unwrap_or_default();

    Ok(SearchDocument {
        logical_path,
        size: metadata.len(),
        mtime_nanos,
        source: SearchDocumentSource::Path(path),
    })
}

impl PlannedWorkspaceQuery {
    fn new(query: &QueryConfig) -> Result<Self> {
        Ok(Self {
            plan: crate::planner::plan(&query.regex_pattern)?,
        })
    }

    fn branch_count(&self) -> usize {
        if self.plan.fallback_to_scan {
            1
        } else {
            self.plan.branches.len()
        }
    }

    fn empty_doc_id_branches(&self) -> Vec<Vec<u32>> {
        vec![Vec::new(); self.branch_count()]
    }

    fn empty_literal_doc_id_branches(&self) -> Vec<BranchLiteralDocIds> {
        self.plan
            .branches
            .iter()
            .map(|branch| BranchLiteralDocIds {
                literals: vec![Vec::new(); branch.literals.len()],
            })
            .collect()
    }
}

fn resolve_documents(candidates: EffectiveCandidateSet) -> Result<ResolvedDocuments> {
    let mut documents = Vec::new();

    for candidate in candidates.into_ordered() {
        let path = PathBuf::from(&candidate.path);
        if !path.exists() || read_text_file(&path)?.is_none() {
            continue;
        }
        documents.push(search_document_from_path(candidate.path, path)?);
    }

    Ok(ResolvedDocuments { documents })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::TempDir;

    use super::{EffectiveIndexView, PlannedWorkspaceQuery};
    use crate::{
        build_index,
        config::{BuildConfig, CorpusMode, TokenizerMode},
        IndexSearcher, QueryConfig,
    };

    struct RuntimeRepo {
        _temp: TempDir,
        repo: PathBuf,
        index: PathBuf,
    }

    impl RuntimeRepo {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temp dir should succeed");
            let repo = temp.path().join("repo");
            let index = repo.join(".bitfun-index");
            fs::create_dir_all(&repo).expect("repo dir should succeed");
            Self {
                _temp: temp,
                repo,
                index,
            }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.repo.join(relative)
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.path(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("parent dir should succeed");
            }
            fs::write(&path, contents).expect("write should succeed");
            path
        }

        fn remove(&self, relative: &str) {
            fs::remove_file(self.path(relative)).expect("remove should succeed");
        }

        fn build_config(&self) -> BuildConfig {
            BuildConfig {
                repo_path: self.repo.clone(),
                index_path: self.index.clone(),
                tokenizer: TokenizerMode::Trigram,
                corpus_mode: CorpusMode::RespectIgnore,
                include_hidden: false,
                max_file_size: 1024 * 1024,
                min_sparse_len: 3,
                max_sparse_len: 32,
            }
        }

        fn build(&self) -> BuildConfig {
            let config = self.build_config();
            build_index(&config).expect("build should succeed");
            config
        }
    }

    fn query(pattern: &str) -> QueryConfig {
        QueryConfig {
            regex_pattern: pattern.into(),
            patterns: vec![pattern.into()],
            ..QueryConfig::default()
        }
    }

    fn merged_literal_candidate_paths(
        view: &EffectiveIndexView<'_>,
        query: &QueryConfig,
    ) -> Vec<String> {
        let planned_query =
            PlannedWorkspaceQuery::new(query).expect("query planning should succeed");
        assert!(
            !planned_query.plan.fallback_to_scan,
            "test expects indexed literal query"
        );
        let use_allowed_paths = planned_query.plan.pure_literal_alternation.is_some();
        let clean_paths = use_allowed_paths.then(|| view.clean_base_paths());
        let base_branches = if let Some(clean_paths) = clean_paths.as_ref() {
            if clean_paths.is_empty() {
                planned_query.empty_literal_doc_id_branches()
            } else {
                view.base_searcher
                    .candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
                        query,
                        &planned_query.plan,
                        None,
                        Some(clean_paths),
                    )
                    .expect("base branch lookup should succeed")
            }
        } else {
            view.base_searcher
                .candidate_doc_ids_by_branch_and_literal_for_plan_with_allowed_paths(
                    query,
                    &planned_query.plan,
                    None,
                    None,
                )
                .expect("base branch lookup should succeed")
        };
        let overlay_branches = view
            .overlay_literal_branches(query, &planned_query, use_allowed_paths)
            .expect("overlay branch lookup should succeed");
        view.merge_literal_branch_candidates(
            &base_branches[0],
            &overlay_branches[0],
            use_allowed_paths,
        )
        .into_ordered()
        .into_iter()
        .map(|candidate| candidate.path)
        .collect()
    }

    #[test]
    fn effective_index_view_clean_base_paths_excludes_shadowed_paths() {
        let repo = RuntimeRepo::new();
        let keep = repo.write("keep.rs", "const NAME: &str = \"TOKEN\";\n");
        let tracked = repo.write("tracked.rs", "const NAME: &str = \"TOKEN\";\n");
        let deleted = repo.write("deleted.rs", "const NAME: &str = \"TOKEN\";\n");
        let config = repo.build();

        repo.write("tracked.rs", "const NAME: &str = \"UPDATED\";\n");
        repo.remove("deleted.rs");

        let searcher = IndexSearcher::open(repo.index.clone()).expect("searcher should open");
        let diff = searcher
            .diff_against_worktree()
            .expect("diff should succeed");
        let view =
            EffectiveIndexView::new(&searcher, &diff, &config, None).expect("view should open");
        let clean_paths = view.clean_base_paths();

        assert!(clean_paths.contains(&keep.to_string_lossy().into_owned()));
        assert!(!clean_paths.contains(&tracked.to_string_lossy().into_owned()));
        assert!(!clean_paths.contains(&deleted.to_string_lossy().into_owned()));
    }

    #[test]
    fn effective_index_view_merge_literal_branch_candidates_includes_overlay_docs() {
        let repo = RuntimeRepo::new();
        let base = repo.write("base.rs", "const NAME: &str = \"TOKEN\";\n");
        let added = repo.write("added.rs", "const NAME: &str = \"OTHER\";\n");
        let config = repo.build();

        repo.write("added.rs", "const NAME: &str = \"TOKEN\";\n");

        let searcher = IndexSearcher::open(repo.index.clone()).expect("searcher should open");
        let diff = searcher
            .diff_against_worktree()
            .expect("diff should succeed");
        let view =
            EffectiveIndexView::new(&searcher, &diff, &config, None).expect("view should open");

        let paths = merged_literal_candidate_paths(&view, &query("TOKEN"));

        assert_eq!(
            paths,
            vec![
                added.to_string_lossy().into_owned(),
                base.to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn effective_index_view_merge_literal_branch_candidates_excludes_shadowed_base_docs() {
        let repo = RuntimeRepo::new();
        let keep = repo.write("keep.rs", "const NAME: &str = \"STALE\";\n");
        let tracked = repo.write("tracked.rs", "const NAME: &str = \"STALE\";\n");
        let config = repo.build();

        repo.write("tracked.rs", "const NAME: &str = \"FRESH\";\n");

        let searcher = IndexSearcher::open(repo.index.clone()).expect("searcher should open");
        let diff = searcher
            .diff_against_worktree()
            .expect("diff should succeed");
        let view =
            EffectiveIndexView::new(&searcher, &diff, &config, None).expect("view should open");

        let paths = merged_literal_candidate_paths(&view, &query("STALE"));

        assert_eq!(paths, vec![keep.to_string_lossy().into_owned()]);
        assert!(!paths.contains(&tracked.to_string_lossy().into_owned()));
    }
}
