use std::path::Path;

use crate::{error::Result, files::read_text_file, planner::QueryPlan};

#[derive(Debug, Clone)]
pub(super) struct SearchPreparation {
    pub(super) candidate_ids: Vec<u32>,
    pub(super) literal_prefilter: Option<LiteralPrefilter>,
}

#[derive(Debug, Clone)]
pub(super) struct LiteralPrefilter {
    branches: Vec<Vec<String>>,
    case_insensitive: bool,
}

impl LiteralPrefilter {
    pub(super) fn new(plan: &QueryPlan, case_insensitive: bool) -> Option<Self> {
        if plan.fallback_to_scan {
            return None;
        }

        if !plan.branches.iter().any(|branch| branch.literals.len() > 1) {
            return None;
        }

        let branches = plan
            .branches
            .iter()
            .map(|branch| {
                branch
                    .literals
                    .iter()
                    .filter(|literal| !literal.is_empty())
                    .map(|literal| {
                        if case_insensitive {
                            fold_query_literal(literal)
                        } else {
                            literal.clone()
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|branch| !branch.is_empty())
            .collect::<Vec<_>>();

        if branches.is_empty() {
            None
        } else {
            Some(Self {
                branches,
                case_insensitive,
            })
        }
    }

    pub(super) fn file_may_match(&self, path: &Path) -> Result<bool> {
        let Some(text) = read_text_file(path)? else {
            return Ok(false);
        };
        Ok(self.text_may_match(&text))
    }

    pub(super) fn text_may_match(&self, text: &str) -> bool {
        if self.case_insensitive {
            if text.is_ascii() {
                let folded = text.to_ascii_lowercase();
                self.any_branch_matches(&folded)
            } else {
                let folded = text.to_lowercase();
                self.any_branch_matches(&folded)
            }
        } else {
            self.any_branch_matches(text)
        }
    }

    fn any_branch_matches(&self, haystack: &str) -> bool {
        self.branches
            .iter()
            .any(|branch| branch.iter().all(|literal| haystack.contains(literal)))
    }
}

pub(super) fn fold_query_literal(literal: &str) -> String {
    if literal.is_ascii() {
        literal.to_ascii_lowercase()
    } else {
        literal.to_lowercase()
    }
}
