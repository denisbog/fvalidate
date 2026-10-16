//! Comparison semantics between the "expected" and "actual" value sets.
//!
//! Every cell is normalized into a set of tokens (a single token when the cell
//! is not a comma-separated multi-value). Comparisons therefore always operate
//! on sets, which makes `multi` handling orthogonal to the operator.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// The expected and actual sets are equal.
    Eq,
    /// The expected and actual sets differ.
    Ne,
    /// Every expected token is present in the actual set.
    Subset,
    /// Every actual token is present in the expected set.
    Superset,
    /// The sets share at least one token.
    Intersect,
    /// The value matches a rule-level regex `pattern`.
    Matches,
    /// The value does not match a rule-level regex `pattern`.
    NotMatches,
}

impl CompareOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eq" | "equal" | "equals" | "=" | "==" => Some(CompareOp::Eq),
            "ne" | "neq" | "!=" | "<>" => Some(CompareOp::Ne),
            "subset" | "in" | "contains_all_reverse" => Some(CompareOp::Subset),
            "superset" | "contains" => Some(CompareOp::Superset),
            "intersect" | "overlap" | "any" => Some(CompareOp::Intersect),
            "matches" | "match" | "=~" | "regex" => Some(CompareOp::Matches),
            "not_matches" | "nomatch" | "!~" | "not_match" => Some(CompareOp::NotMatches),
            _ => None,
        }
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, CompareOp::Matches | CompareOp::NotMatches)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CompareOp::Eq => "eq",
            CompareOp::Ne => "ne",
            CompareOp::Subset => "subset",
            CompareOp::Superset => "superset",
            CompareOp::Intersect => "intersect",
            CompareOp::Matches => "matches",
            CompareOp::NotMatches => "not_matches",
        }
    }

    /// `expected` and `actual` must be sorted + deduplicated.
    pub fn evaluate(&self, expected: &[String], actual: &[String]) -> bool {
        match self {
            CompareOp::Eq => expected == actual,
            CompareOp::Ne => expected != actual,
            CompareOp::Subset => is_subset(expected, actual),
            CompareOp::Superset => is_subset(actual, expected),
            CompareOp::Intersect => intersect(expected, actual),
            // Regex comparisons need a compiled pattern and are evaluated by
            // the engine before this point.
            CompareOp::Matches | CompareOp::NotMatches => false,
        }
    }
}

fn is_subset(a: &[String], b: &[String]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    i == a.len()
}

fn intersect(a: &[String], b: &[String]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}
