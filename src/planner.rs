use regex_syntax::{
    hir::{Hir, HirKind, Repetition},
    parse,
};

use crate::error::{AppError, Result};

const BRANCH_LIMIT: usize = 128;

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub branches: Vec<QueryBranch>,
    pub fallback_to_scan: bool,
}

#[derive(Debug, Clone)]
pub struct QueryBranch {
    pub literals: Vec<String>,
}

pub fn plan(pattern: &str) -> Result<QueryPlan> {
    let hir = parse(pattern).map_err(|error| AppError::InvalidPattern(error.to_string()))?;
    let extracted = extract(&hir)?;
    if extracted.overflow {
        return Ok(QueryPlan {
            branches: Vec::new(),
            fallback_to_scan: true,
        });
    }

    let branches = normalize_branches(extracted.branches);
    let fallback_to_scan =
        branches.is_empty() || branches.iter().any(|branch| branch.literals.is_empty());
    let branches = if fallback_to_scan {
        Vec::new()
    } else {
        branches
    };

    Ok(QueryPlan {
        branches,
        fallback_to_scan,
    })
}

#[derive(Debug, Clone)]
struct Extracted {
    branches: Vec<Vec<Part>>,
    overflow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Literal(String),
    Gap,
}

fn extract(hir: &Hir) -> Result<Extracted> {
    let extracted = match hir.kind() {
        HirKind::Empty => Extracted {
            branches: vec![Vec::new()],
            overflow: false,
        },
        HirKind::Literal(literal) => Extracted {
            branches: vec![vec![Part::Literal(
                String::from_utf8_lossy(&literal.0).into_owned(),
            )]],
            overflow: false,
        },
        HirKind::Class(_) | HirKind::Look(_) => Extracted {
            branches: vec![vec![Part::Gap]],
            overflow: false,
        },
        HirKind::Capture(capture) => extract(&capture.sub)?,
        HirKind::Repetition(repetition) => extract_repetition(repetition)?,
        HirKind::Concat(hirs) => extract_concat(hirs)?,
        HirKind::Alternation(hirs) => extract_alternation(hirs)?,
    };
    Ok(extracted)
}

fn extract_repetition(repetition: &Repetition) -> Result<Extracted> {
    if repetition.min == 0 {
        let repeated = extract(&repetition.sub)?;
        if repeated.overflow {
            return Ok(repeated);
        }
        let mut branches = Vec::with_capacity(repeated.branches.len() + 1);
        branches.push(Vec::new());
        branches.extend(repeated.branches);
        if branches.len() > BRANCH_LIMIT {
            return Ok(Extracted {
                branches: Vec::new(),
                overflow: true,
            });
        }
        return Ok(Extracted {
            branches,
            overflow: false,
        });
    }
    extract(&repetition.sub)
}

fn extract_concat(hirs: &[Hir]) -> Result<Extracted> {
    let mut current = Extracted {
        branches: vec![Vec::new()],
        overflow: false,
    };
    for hir in hirs {
        let next = extract(hir)?;
        current = concat(current, next);
        if current.overflow {
            break;
        }
    }
    Ok(current)
}

fn extract_alternation(hirs: &[Hir]) -> Result<Extracted> {
    let mut branches = Vec::new();
    for hir in hirs {
        let extracted = extract(hir)?;
        if extracted.overflow {
            return Ok(extracted);
        }
        branches.extend(extracted.branches);
        if branches.len() > BRANCH_LIMIT {
            return Ok(Extracted {
                branches: Vec::new(),
                overflow: true,
            });
        }
    }
    Ok(Extracted {
        branches,
        overflow: false,
    })
}

fn concat(left: Extracted, right: Extracted) -> Extracted {
    if left.overflow || right.overflow {
        return Extracted {
            branches: Vec::new(),
            overflow: true,
        };
    }

    let mut branches = Vec::new();
    for left_branch in &left.branches {
        for right_branch in &right.branches {
            branches.push(concat_parts(left_branch, right_branch));
            if branches.len() > BRANCH_LIMIT {
                return Extracted {
                    branches: Vec::new(),
                    overflow: true,
                };
            }
        }
    }

    Extracted {
        branches,
        overflow: false,
    }
}

fn concat_parts(left: &[Part], right: &[Part]) -> Vec<Part> {
    let mut merged = left.to_vec();

    if let (Some(Part::Literal(left_literal)), Some(Part::Literal(right_literal))) =
        (merged.last_mut(), right.first())
    {
        left_literal.push_str(right_literal);
        merged.extend_from_slice(&right[1..]);
    } else {
        merged.extend_from_slice(right);
    }

    merged
}

fn normalize_branches(branches: Vec<Vec<Part>>) -> Vec<QueryBranch> {
    let mut normalized: Vec<QueryBranch> = Vec::new();

    for branch in branches {
        let literals = extract_literals(branch);
        let query_branch = QueryBranch { literals };
        if normalized
            .iter()
            .any(|existing| existing == &query_branch || existing.dominates(&query_branch))
        {
            continue;
        }

        normalized.retain(|existing| !query_branch.dominates(existing));
        if !normalized.iter().any(|existing| existing == &query_branch) {
            normalized.push(query_branch);
        }
    }

    normalized
}

fn extract_literals(parts: Vec<Part>) -> Vec<String> {
    let mut literals = Vec::new();
    let mut current = String::new();

    for part in parts {
        match part {
            Part::Literal(text) => current.push_str(&text),
            Part::Gap => {
                if !current.is_empty() {
                    literals.push(std::mem::take(&mut current));
                }
            }
        }
    }

    if !current.is_empty() {
        literals.push(current);
    }

    literals
}

impl PartialEq for QueryBranch {
    fn eq(&self, other: &Self) -> bool {
        self.literals == other.literals
    }
}

impl Eq for QueryBranch {}

impl QueryBranch {
    fn dominates(&self, other: &Self) -> bool {
        self.literals.iter().all(|needle| {
            other
                .literals
                .iter()
                .any(|haystack| haystack.contains(needle))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::plan;

    #[test]
    fn plan_extracts_literals_from_concat() {
        let plan = plan("foo.*bar").unwrap();
        assert_eq!(plan.branches.len(), 1);
        assert_eq!(plan.branches[0].literals, vec!["foo", "bar"]);
    }

    #[test]
    fn plan_keeps_alternation_as_branches() {
        let plan = plan("get(User|Account)ById").unwrap();
        assert_eq!(plan.branches.len(), 2);
        assert_eq!(plan.branches[0].literals, vec!["getUserById"]);
        assert_eq!(plan.branches[1].literals, vec!["getAccountById"]);
    }

    #[test]
    fn optional_literals_degrade_to_remaining_required_literal() {
        let plan = plan("(foo)?bar").unwrap();
        assert_eq!(plan.branches.len(), 1);
        assert_eq!(plan.branches[0].literals, vec!["bar"]);
    }

    #[test]
    fn unicode_property_parses_without_false_literal() {
        let plan = plan(r"\p{Greek}").unwrap();
        assert!(plan.fallback_to_scan);
        assert!(plan.branches.is_empty());
    }

    #[test]
    fn classes_split_required_literals() {
        let plan = plan(r"foo[0-9]+bar").unwrap();
        assert_eq!(plan.branches.len(), 1);
        assert_eq!(plan.branches[0].literals, vec!["foo", "bar"]);
    }

    #[test]
    fn look_around_prevents_literal_merging() {
        let plan = plan(r"\bPM_RESUME\b").unwrap();
        assert_eq!(plan.branches.len(), 1);
        assert_eq!(plan.branches[0].literals, vec!["PM_RESUME"]);
    }
}
