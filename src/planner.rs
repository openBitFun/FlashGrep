use crate::error::{AppError, Result};

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
    let mut parser = PatternParser::new(pattern);
    let expression = parser.parse_expression()?;
    if !parser.is_at_end() {
        return Err(AppError::InvalidPattern("unexpected trailing input".into()));
    }

    let branches = normalize_branches(expression.branches, 64);
    let fallback_to_scan =
        branches.is_empty() || branches.iter().all(|branch| branch.literals.is_empty());

    Ok(QueryPlan {
        branches,
        fallback_to_scan,
    })
}

#[derive(Debug, Clone)]
struct ParseResult {
    branches: Vec<Vec<Part>>,
    nullable: bool,
}

#[derive(Debug, Clone)]
enum Part {
    Literal(String),
    Boundary,
}

struct PatternParser<'a> {
    chars: Vec<char>,
    position: usize,
    _source: &'a str,
}

impl<'a> PatternParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
            _source: source,
        }
    }

    fn parse_expression(&mut self) -> Result<ParseResult> {
        let mut alternatives = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.position += 1;
            alternatives.push(self.parse_concat()?);
        }

        let nullable = alternatives.iter().any(|branch| branch.nullable);
        let mut branches = Vec::new();
        for alternative in alternatives {
            branches.extend(alternative.branches);
        }

        Ok(ParseResult { branches, nullable })
    }

    fn parse_concat(&mut self) -> Result<ParseResult> {
        let mut current = ParseResult {
            branches: vec![Vec::new()],
            nullable: true,
        };

        while let Some(ch) = self.peek() {
            if ch == ')' || ch == '|' {
                break;
            }

            let atom = self.parse_atom()?;
            current = concat_results(current, atom);
        }

        Ok(current)
    }

    fn parse_atom(&mut self) -> Result<ParseResult> {
        let base = match self
            .next()
            .ok_or_else(|| AppError::InvalidPattern("unexpected end of pattern".into()))?
        {
            '(' => {
                let nested = self.parse_expression()?;
                if self.next() != Some(')') {
                    return Err(AppError::InvalidPattern("unclosed group".into()));
                }
                nested
            }
            '[' => self.parse_class(),
            '.' | '^' | '$' => boundary_result(),
            '\\' => self.parse_escape()?,
            ch => ParseResult {
                branches: vec![vec![Part::Literal(ch.to_string())]],
                nullable: false,
            },
        };

        Ok(self.apply_quantifier(base))
    }

    fn parse_escape(&mut self) -> Result<ParseResult> {
        let escaped = self
            .next()
            .ok_or_else(|| AppError::InvalidPattern("dangling escape".into()))?;

        let result = match escaped {
            'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'b' | 'B' | 'A' | 'z' | 'Z' => boundary_result(),
            'n' => ParseResult {
                branches: vec![vec![Part::Literal("\n".into())]],
                nullable: false,
            },
            'r' => ParseResult {
                branches: vec![vec![Part::Literal("\r".into())]],
                nullable: false,
            },
            't' => ParseResult {
                branches: vec![vec![Part::Literal("\t".into())]],
                nullable: false,
            },
            other => ParseResult {
                branches: vec![vec![Part::Literal(other.to_string())]],
                nullable: false,
            },
        };

        Ok(result)
    }

    fn parse_class(&mut self) -> ParseResult {
        let mut escaped = false;
        while let Some(ch) = self.next() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                ']' => break,
                _ => {}
            }
        }

        boundary_result()
    }

    fn apply_quantifier(&mut self, base: ParseResult) -> ParseResult {
        match self.peek() {
            Some('*') | Some('?') => {
                self.position += 1;
                boundary_result()
            }
            Some('+') => {
                self.position += 1;
                base
            }
            Some('{') => {
                let (min, _) = self.parse_braced_quantifier();
                if min == 0 {
                    boundary_result()
                } else {
                    base
                }
            }
            _ => base,
        }
    }

    fn parse_braced_quantifier(&mut self) -> (usize, Option<usize>) {
        let checkpoint = self.position;
        self.position += 1;
        let min = self.parse_usize();

        let max = if self.peek() == Some(',') {
            self.position += 1;
            self.parse_usize_opt()
        } else {
            Some(min)
        };

        if self.peek() != Some('}') {
            self.position = checkpoint;
            return (1, Some(1));
        }

        self.position += 1;
        (min, max)
    }

    fn parse_usize(&mut self) -> usize {
        self.parse_usize_opt().unwrap_or(0)
    }

    fn parse_usize_opt(&mut self) -> Option<usize> {
        let start = self.position;
        while matches!(self.peek(), Some('0'..='9')) {
            self.position += 1;
        }

        if self.position == start {
            return None;
        }

        let digits = self.chars[start..self.position].iter().collect::<String>();
        digits.parse().ok()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn next(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.position += 1;
        Some(value)
    }

    fn is_at_end(&self) -> bool {
        self.position >= self.chars.len()
    }
}

fn concat_results(left: ParseResult, right: ParseResult) -> ParseResult {
    let mut branches = Vec::new();
    for left_branch in &left.branches {
        for right_branch in &right.branches {
            let mut merged = left_branch.clone();
            merged.extend(right_branch.iter().cloned());
            branches.push(merged);
        }
    }

    ParseResult {
        branches,
        nullable: left.nullable && right.nullable,
    }
}

fn normalize_branches(branches: Vec<Vec<Part>>, branch_limit: usize) -> Vec<QueryBranch> {
    let mut normalized = Vec::new();

    for branch in branches.into_iter().take(branch_limit) {
        let literals = extract_literals(branch)
            .into_iter()
            .filter(|literal| !literal.is_empty())
            .collect::<Vec<_>>();
        normalized.push(QueryBranch { literals });
    }

    normalized
}

fn boundary_result() -> ParseResult {
    ParseResult {
        branches: vec![vec![Part::Boundary]],
        nullable: true,
    }
}

fn extract_literals(parts: Vec<Part>) -> Vec<String> {
    let mut merged = Vec::new();
    let mut current = String::new();

    for part in parts {
        match part {
            Part::Literal(text) => current.push_str(&text),
            Part::Boundary => {
                if !current.is_empty() {
                    merged.push(std::mem::take(&mut current));
                }
            }
        }
    }

    if !current.is_empty() {
        merged.push(current);
    }

    merged
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
    fn optional_literals_degrade_to_empty_branch() {
        let plan = plan("(foo)?bar").unwrap();
        assert_eq!(plan.branches.len(), 1);
        assert_eq!(plan.branches[0].literals, vec!["bar"]);
    }
}
