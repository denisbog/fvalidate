//! Value transformations applied to cells before comparison/mapping.
//!
//! Transforms write into a caller-provided `String` buffer so that a whole
//! pipeline can run with zero allocations once the buffer is warm (the engine
//! reuses one buffer per side across every row).

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use regex::Regex;

/// A single transformation step.
#[derive(Debug, Clone)]
pub enum Transform {
    Lower,
    Upper,
    Trim,
    /// Trim then collapse all internal whitespace runs to a single space.
    Collapse,
    /// Parse a date/datetime using one of `inputs` formats and re-emit using
    /// `output`. Both sides are normalized to the same textual representation.
    Date {
        inputs: Vec<String>,
        output: String,
    },
    /// Parse an integer and re-emit its canonical form (drops leading zeros,
    /// plus signs, etc.).
    Int,
    /// Parse a float and re-emit its canonical form.
    Float,
    /// Normalize common boolean spellings to `true` / `false`.
    Bool,
    Replace {
        from: String,
        to: String,
    },
    /// Regex replacement (xan's `replace(string, regex(...), replacement)`),
    /// supporting capture groups in `replacement` (`$1`, `${name}`).
    RegexReplace {
        pattern: Regex,
        replacement: String,
    },
    /// Extract a capture group from the first regex match (xan's
    /// `match(string, regex(...), group)`). Group 0 is the whole match.
    RegexExtract {
        pattern: Regex,
        group: usize,
    },
    /// Keep only the characters matching the regex (all non-overlapping
    /// matches concatenated).
    RegexKeep {
        pattern: Regex,
    },
    Prefix(String),
    Suffix(String),
}

impl Transform {
    /// Append a stable, human-readable signature used to key the mapping cache.
    pub fn signature(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Transform::Lower => out.push_str("lower"),
            Transform::Upper => out.push_str("upper"),
            Transform::Trim => out.push_str("trim"),
            Transform::Collapse => out.push_str("collapse"),
            Transform::Date { inputs, output } => {
                let _ = write!(out, "date({:?}->{output})", inputs);
            }
            Transform::Int => out.push_str("int"),
            Transform::Float => out.push_str("float"),
            Transform::Bool => out.push_str("bool"),
            Transform::Replace { from, to } => {
                let _ = write!(out, "replace({from:?},{to:?})");
            }
            Transform::RegexReplace {
                pattern,
                replacement,
            } => {
                let _ = write!(out, "regex_replace({:?},{replacement:?})", pattern.as_str());
            }
            Transform::RegexExtract { pattern, group } => {
                let _ = write!(out, "extract({:?},{group})", pattern.as_str());
            }
            Transform::RegexKeep { pattern } => {
                let _ = write!(out, "keep({:?})", pattern.as_str());
            }
            Transform::Prefix(value) => {
                let _ = write!(out, "prefix({value:?})");
            }
            Transform::Suffix(value) => {
                let _ = write!(out, "suffix({value:?})");
            }
        }
    }
}

impl Transform {
    /// Apply the transform, appending the result to `out`.
    ///
    /// Returns `false` when a parsing transform failed; in that case `out` is
    /// left with the (unmodified) input.
    pub fn write_into(&self, input: &str, out: &mut String) -> bool {
        match self {
            Transform::Lower => {
                out.extend(input.chars().flat_map(char::to_lowercase));
                true
            }
            Transform::Upper => {
                out.extend(input.chars().flat_map(char::to_uppercase));
                true
            }
            Transform::Trim => {
                out.push_str(input.trim());
                true
            }
            Transform::Collapse => {
                let mut first = true;
                for word in input.split_whitespace() {
                    if !first {
                        out.push(' ');
                    }
                    out.push_str(word);
                    first = false;
                }
                true
            }
            Transform::Date { inputs, output } => match reparse_date(input, inputs, output) {
                Some(value) => {
                    out.push_str(&value);
                    true
                }
                None => {
                    out.push_str(input);
                    false
                }
            },
            Transform::Int => {
                let trimmed = input.trim();
                match trimmed.parse::<i64>() {
                    Ok(value) => {
                        push_i64(out, value);
                        true
                    }
                    Err(_) => {
                        out.push_str(input);
                        false
                    }
                }
            }
            Transform::Float => {
                let trimmed = input.trim();
                match trimmed.parse::<f64>() {
                    Ok(value) if value.is_finite() => {
                        push_float(out, value);
                        true
                    }
                    _ => {
                        out.push_str(input);
                        false
                    }
                }
            }
            Transform::Bool => match parse_bool(input) {
                Some(true) => {
                    out.push_str("true");
                    true
                }
                Some(false) => {
                    out.push_str("false");
                    true
                }
                None => {
                    out.push_str(input);
                    false
                }
            },
            Transform::Replace { from, to } => {
                if input.contains(from.as_str()) {
                    out.push_str(&input.replace(from.as_str(), to));
                } else {
                    out.push_str(input);
                }
                true
            }
            Transform::RegexReplace {
                pattern,
                replacement,
            } => {
                // `replace_all` borrows the input when nothing matched.
                out.push_str(&pattern.replace_all(input, replacement.as_str()));
                true
            }
            Transform::RegexExtract { pattern, group } => {
                match pattern.captures(input).and_then(|caps| caps.get(*group)) {
                    Some(captured) => {
                        out.push_str(captured.as_str());
                        true
                    }
                    None => {
                        out.push_str(input);
                        false
                    }
                }
            }
            Transform::RegexKeep { pattern } => {
                for matched in pattern.find_iter(input) {
                    out.push_str(matched.as_str());
                }
                true
            }
            Transform::Prefix(prefix) => {
                out.push_str(prefix);
                out.push_str(input);
                true
            }
            Transform::Suffix(suffix) => {
                out.push_str(input);
                out.push_str(suffix);
                true
            }
        }
    }
}

/// Apply a full pipeline into `out`. `out` is overwritten and `scratch` is a
/// caller-owned temporary reused across calls, so a multi-step pipeline does
/// not allocate once the buffers are warm. Returns whether all steps
/// succeeded.
pub fn apply_pipeline(
    transforms: &[Transform],
    input: &str,
    out: &mut String,
    scratch: &mut String,
) -> bool {
    out.clear();
    let Some((first, rest)) = transforms.split_first() else {
        out.push_str(input);
        return true;
    };

    let mut ok = first.write_into(input, out);

    // Two buffers are swapped explicitly (rather than reassigning a `&str`)
    // so the borrow checker is happy and no temporary allocation is needed.
    let mut result_in_out = true;

    for transform in rest {
        if result_in_out {
            scratch.clear();
            ok &= transform.write_into(out, scratch);
            result_in_out = false;
        } else {
            out.clear();
            ok &= transform.write_into(scratch, out);
            result_in_out = true;
        }
    }

    if !result_in_out {
        out.clear();
        out.push_str(scratch);
    }

    ok
}

/// Combine several already-extracted cell values into one composite key.
///
/// Each part is optionally trimmed, then runs through `transforms`
/// independently, then the results are joined with `join`. `part` and `scratch`
/// are caller-owned temporaries reused between parts (and across rows), so no
/// allocation is needed once they are warm; the final key is written to `out`.
pub fn compose<S, I>(
    parts: I,
    transforms: &[Transform],
    join: &str,
    trim: bool,
    part: &mut String,
    scratch: &mut String,
    out: &mut String,
) -> bool
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    out.clear();
    let mut ok = true;
    for (index, input) in parts.into_iter().enumerate() {
        if index > 0 {
            out.push_str(join);
        }
        let input = input.as_ref();
        let input = if trim { input.trim() } else { input };
        ok &= apply_pipeline(transforms, input, part, scratch);
        out.push_str(part);
    }
    ok
}

fn push_i64(out: &mut String, value: i64) {
    use std::fmt::Write as _;
    let _ = write!(out, "{value}");
}

fn push_float(out: &mut String, value: f64) {
    use std::fmt::Write as _;
    if value == value.trunc() && value.abs() < 1e15 {
        let _ = write!(out, "{}", value as i64);
    } else {
        let _ = write!(out, "{value}");
    }
}

fn parse_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" | "vrai" | "oui" => Some(true),
        "false" | "f" | "no" | "n" | "0" | "faux" | "non" => Some(false),
        _ => None,
    }
}

fn reparse_date(input: &str, inputs: &[String], output: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }

    for fmt in inputs {
        // Fast path for the common fixed-width formats, so the chrono parser
        // is only paid for unusual formats. Results are identical: a value
        // that does not match falls through to chrono.
        if let Some(dt) = fast_parse_date(value, fmt) {
            return Some(format_output(dt, output));
        }
        if let Ok(dt) = NaiveDateTime::parse_from_str(value, fmt) {
            return Some(format_output(dt, output));
        }
        if let Ok(date) = NaiveDate::parse_from_str(value, fmt) {
            let dt = date.and_time(NaiveTime::MIN);
            return Some(format_output(dt, output));
        }
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Some(format_output(dt.naive_utc(), output));
    }

    None
}

/// Emit a parsed date, with a fast path for the most common output format.
fn format_output(dt: NaiveDateTime, output: &str) -> String {
    if output == "%Y-%m-%d" {
        use chrono::Datelike;
        format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day())
    } else {
        dt.format(output).to_string()
    }
}

fn parse_uint(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Some(value)
}

/// Parse a handful of fixed-width date formats without going through chrono's
/// generic parser. Returns `None` when the format is not special-cased or the
/// value does not fit it (the caller then retries with chrono).
fn fast_parse_date(value: &str, fmt: &str) -> Option<NaiveDateTime> {
    let bytes = value.as_bytes();
    let (year, month, day) = match fmt {
        "%Y-%m-%d" => split_ymd(bytes, b'-')?,
        "%Y/%m/%d" => split_ymd(bytes, b'/')?,
        "%d/%m/%Y" => split_dmy(bytes, b'/')?,
        "%d-%m-%Y" => split_dmy(bytes, b'-')?,
        _ => return None,
    };
    NaiveDate::from_ymd_opt(year as i32, month, day)?.and_hms_opt(0, 0, 0)
}

fn split_ymd(bytes: &[u8], sep: u8) -> Option<(u32, u32, u32)> {
    if bytes.len() != 10 || bytes[4] != sep || bytes[7] != sep {
        return None;
    }
    Some((
        parse_uint(&bytes[0..4])?,
        parse_uint(&bytes[5..7])?,
        parse_uint(&bytes[8..10])?,
    ))
}

/// `DD<sep>MM<sep>YYYY` -> `(year, month, day)`.
fn split_dmy(bytes: &[u8], sep: u8) -> Option<(u32, u32, u32)> {
    if bytes.len() != 10 || bytes[2] != sep || bytes[5] != sep {
        return None;
    }
    Some((
        parse_uint(&bytes[6..10])?,
        parse_uint(&bytes[3..5])?,
        parse_uint(&bytes[0..2])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_date_paths_match_chrono() {
        for (fmt, value) in [
            ("%Y-%m-%d", "2020-12-31"),
            ("%Y/%m/%d", "2020/01/05"),
            ("%d/%m/%Y", "31/12/2020"),
            ("%d-%m-%Y", "05-01-2020"),
        ] {
            let fast = fast_parse_date(value, fmt).expect("fast path should parse");
            let chrono = NaiveDateTime::parse_from_str(value, fmt)
                .or_else(|_| {
                    NaiveDate::parse_from_str(value, fmt).map(|d| d.and_time(NaiveTime::MIN))
                })
                .expect("chrono should parse");
            assert_eq!(fast, chrono, "{fmt} {value}");
        }
        // Invalid dates and non-matching widths fall back to chrono.
        assert!(fast_parse_date("2020-02-30", "%Y-%m-%d").is_none());
        assert!(fast_parse_date("2020-2-3", "%Y-%m-%d").is_none());
        assert!(fast_parse_date("2020-01-05", "%d/%m/%Y").is_none());
    }

    #[test]
    fn dates_are_normalized() {
        let transform = Transform::Date {
            inputs: vec!["%d/%m/%Y".into(), "%Y-%m-%d".into()],
            output: "%Y-%m-%d".into(),
        };
        let mut out = String::new();
        assert!(transform.write_into("31/12/2020", &mut out));
        assert_eq!(out, "2020-12-31");
        out.clear();
        assert!(transform.write_into("2020-01-05", &mut out));
        assert_eq!(out, "2020-01-05");
        out.clear();
        assert!(!transform.write_into("not a date", &mut out));
    }

    #[test]
    fn pipeline_chains() {
        let pipeline = vec![Transform::Trim, Transform::Lower];
        let mut out = String::new();
        let mut scratch = String::new();
        assert!(apply_pipeline(
            &pipeline,
            "  HeLLo ",
            &mut out,
            &mut scratch
        ));
        assert_eq!(out, "hello");
    }

    #[test]
    fn pipeline_of_three() {
        let pipeline = vec![
            Transform::Trim,
            Transform::Upper,
            Transform::Suffix("!".into()),
        ];
        let mut out = String::new();
        let mut scratch = String::new();
        apply_pipeline(&pipeline, " ab ", &mut out, &mut scratch);
        assert_eq!(out, "AB!");
    }

    #[test]
    fn bools() {
        let mut out = String::new();
        assert!(Transform::Bool.write_into("YES", &mut out));
        assert_eq!(out, "true");
        out.clear();
        assert!(Transform::Bool.write_into("0", &mut out));
        assert_eq!(out, "false");
    }

    #[test]
    fn regex_transforms() {
        let mut out = String::new();

        let strip = Transform::RegexReplace {
            pattern: Regex::new("[^0-9]").unwrap(),
            replacement: String::new(),
        };
        assert!(strip.write_into("+33 6 12", &mut out));
        assert_eq!(out, "33612");

        let extract = Transform::RegexExtract {
            pattern: Regex::new(r"(\d{4})-(\d{2})").unwrap(),
            group: 2,
        };
        out.clear();
        assert!(extract.write_into("2020-07-01", &mut out));
        assert_eq!(out, "07");

        // No match is a failed transform and leaves the input untouched.
        out.clear();
        assert!(!extract.write_into("not a date", &mut out));
        assert_eq!(out, "not a date");

        let reorder = Transform::RegexReplace {
            pattern: Regex::new(r"(\w+)@(\w+)").unwrap(),
            replacement: "$2/$1".to_string(),
        };
        out.clear();
        assert!(reorder.write_into("bob@example", &mut out));
        assert_eq!(out, "example/bob");

        let keep = Transform::RegexKeep {
            pattern: Regex::new(r"\d").unwrap(),
        };
        out.clear();
        assert!(keep.write_into("a1b2c3", &mut out));
        assert_eq!(out, "123");
    }

    #[test]
    fn composite_keys() {
        let transforms = vec![Transform::Trim, Transform::Lower];
        let mut part = String::new();
        let mut scratch = String::new();
        let mut out = String::new();
        let ok = compose(
            [" Widget ", "EU"],
            &transforms,
            "|",
            true,
            &mut part,
            &mut scratch,
            &mut out,
        );
        assert!(ok);
        assert_eq!(out, "widget|eu");

        // Without trimming, surrounding whitespace is preserved.
        let mut untrimmed = String::new();
        compose(
            [" Widget ", "EU"],
            &[] as &[Transform],
            "|",
            false,
            &mut part,
            &mut scratch,
            &mut untrimmed,
        );
        assert_eq!(untrimmed, " Widget |EU");
    }
}
