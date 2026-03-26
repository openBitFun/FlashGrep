use crate::planner::QueryPlan;

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

    pub(super) fn line_may_match_bytes(&self, bytes: &[u8]) -> bool {
        if self.case_insensitive {
            match std::str::from_utf8(bytes) {
                Ok(text) => self.text_may_match(text),
                Err(_) => false,
            }
        } else {
            self.any_branch_matches_bytes(bytes)
        }
    }

    pub(super) fn find_candidate_line(
        &self,
        haystack: &[u8],
        line_terminator: u8,
    ) -> Option<usize> {
        let mut offset = 0usize;
        while offset < haystack.len() {
            let line_len = haystack[offset..]
                .iter()
                .position(|&byte| byte == line_terminator)
                .map_or(haystack.len() - offset, |idx| idx + 1);
            let line_end = offset + line_len;
            if self.line_may_match_bytes(&haystack[offset..line_end]) {
                return Some(offset);
            }
            offset = line_end;
        }
        None
    }

    fn any_branch_matches(&self, haystack: &str) -> bool {
        self.branches
            .iter()
            .any(|branch| branch.iter().all(|literal| haystack.contains(literal)))
    }

    fn any_branch_matches_bytes(&self, haystack: &[u8]) -> bool {
        self.branches.iter().any(|branch| {
            branch
                .iter()
                .all(|literal| contains_bytes(haystack, literal.as_bytes()))
        })
    }
}

pub(super) fn fold_query_literal(literal: &str) -> String {
    if literal.is_ascii() {
        literal.to_ascii_lowercase()
    } else {
        literal.to_lowercase()
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
