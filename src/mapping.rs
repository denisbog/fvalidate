//! Value mapping: the relation `left value -> target value`.
//!
//! A mapping can be:
//! * extracted from the data itself (`auto`), or
//! * loaded from one or more reference files.
//!
//! Because a left value may be associated with several targets, we keep the
//! full set of observed targets (for the ambiguity report) and designate a
//! canonical target (most frequent, ties broken lexicographically) used for
//! validation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use simd_csv::ByteRecord;

use crate::dsl::Predicate;
use crate::pattern::Separator;
use crate::rules::ColumnResolver;

/// left value -> (target value -> number of observations)
pub type MapCounts = HashMap<Box<str>, HashMap<Box<str>, u64>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingOrigin {
    Auto,
    File,
}

#[derive(Debug, Clone)]
pub struct Mapping {
    pub origin: MappingOrigin,
    /// Full relation: left value -> sorted (target, count) pairs.
    pub targets: HashMap<Box<str>, Vec<(Box<str>, u64)>>,
    /// Canonical target for each left value.
    pub canonical: HashMap<Box<str>, Box<str>>,
    /// True when the relation was built from several `mapping_right` columns.
    /// Every stored target is then an *acceptable* value (a union) instead of
    /// an ambiguous observation made by different rows.
    pub multi_target: bool,
}

impl Mapping {
    /// Build a mapping from raw observation counts.
    pub fn from_counts(counts: MapCounts, origin: MappingOrigin) -> Self {
        Self::from_counts_with_targets(counts, origin, false)
    }

    fn from_counts_with_targets(
        counts: MapCounts,
        origin: MappingOrigin,
        multi_target: bool,
    ) -> Self {
        let mut targets: HashMap<Box<str>, Vec<(Box<str>, u64)>> =
            HashMap::with_capacity(counts.len());
        let mut canonical: HashMap<Box<str>, Box<str>> = HashMap::with_capacity(counts.len());

        for (key, hits) in counts {
            let mut entries: Vec<(Box<str>, u64)> = hits.into_iter().collect();
            // Deterministic order: most frequent first, then lexicographic.
            entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            canonical.insert(key.clone(), entries[0].0.clone());
            targets.insert(key, entries);
        }

        Mapping {
            origin,
            targets,
            canonical,
            multi_target,
        }
    }

    pub fn from_counts_auto(counts: MapCounts) -> Self {
        Self::from_counts(counts, MappingOrigin::Auto)
    }

    pub fn expected(&self, key: &str) -> Option<&str> {
        self.canonical.get(key).map(|s| s.as_ref())
    }

    /// Every acceptable target for a key. For a plain mapping this is the
    /// observed relation (one canonical target is used unless it is
    /// ambiguous); for a multi-target mapping all of them are accepted.
    pub fn targets_for(&self, key: &str) -> &[(Box<str>, u64)] {
        self.targets.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_ambiguous(&self, key: &str) -> bool {
        !self.multi_target && self.targets.get(key).is_some_and(|t| t.len() > 1)
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn ambiguous_count(&self) -> usize {
        if self.multi_target {
            return 0;
        }
        self.targets.values().filter(|t| t.len() > 1).count()
    }
}

/// Merge one map's counts into another.
pub fn merge_counts(into: &mut MapCounts, from: MapCounts) {
    for (key, hits) in from {
        let entry = into.entry(key).or_default();
        for (target, count) in hits {
            *entry.entry(target).or_insert(0) += count;
        }
    }
}

/// Increment the observation count for `(left, right)`. Keys are allocated
/// only when a pair is seen for the first time, so the common case does not
/// allocate at all (`entry(left.into())` would box a string on every row).
pub fn bump_counts(counts: &mut MapCounts, left: &str, right: &str) {
    match counts.get_mut(left) {
        Some(hits) => match hits.get_mut(right) {
            Some(count) => *count += 1,
            None => {
                hits.insert(right.into(), 1);
            }
        },
        None => {
            let mut hits = HashMap::new();
            hits.insert(right.into(), 1);
            counts.insert(left.into(), hits);
        }
    }
}

/// Split a cell into tokens. When `multi` is false the whole value is a single
/// token.
/// Split a cell into tokens. When `multi` is false the whole value is a single
/// token. `separator` may be a literal string or a regex.
pub fn split_tokens<'a>(value: &'a str, multi: bool, separator: &Separator) -> Vec<&'a str> {
    if !multi {
        return vec![value];
    }
    separator
        .split(value)
        .into_iter()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect()
}

/// Like [`split_tokens`], but writes owned tokens into a reusable buffer.
/// Existing `String` slots are overwritten in place, so after the first row
/// the hot loop performs no token allocation at all.
pub fn split_tokens_into(value: &str, multi: bool, separator: &Separator, out: &mut Vec<String>) {
    if !multi {
        write_slot(out, 0, value);
        out.truncate(1);
        return;
    }
    let mut count = 0usize;
    for token in separator.split(value) {
        let token = token.trim();
        if !token.is_empty() {
            write_slot(out, count, token);
            count += 1;
        }
    }
    out.truncate(count);
}

#[inline]
fn write_slot(out: &mut Vec<String>, index: usize, value: &str) {
    match out.get_mut(index) {
        Some(slot) => {
            slot.clear();
            slot.push_str(value);
        }
        None => out.push(value.to_string()),
    }
}

/// Positional pairing between left and right tokens.
pub fn pair_tokens<'a>(
    left: &'a [&'a str],
    right: &'a [&'a str],
) -> impl Iterator<Item = (&'a str, &'a str)> {
    left.iter().copied().zip(right.iter().copied())
}

/// Specification for loading a mapping from reference files. All the rule's
/// normalization options are applied to the reference values, so keys and
/// targets match the data exactly.
pub struct FileMappingSpec<'a> {
    pub left_columns: &'a [String],
    pub right_columns: &'a [String],
    pub left_transforms: &'a [crate::transform::Transform],
    pub right_transforms: &'a [crate::transform::Transform],
    pub multi: bool,
    pub value_separator: &'a Separator,
    pub join_separator: &'a str,
    pub trim: bool,
    pub delimiter: u8,
    /// True when the rule's `right` side is itself a list of columns. The
    /// `mapping_right` columns then form a composite value (joined with
    /// `join_separator`) instead of a list of alternative targets.
    pub right_composite: bool,
    /// Optional predicate over reference rows; only matching rows define the
    /// mapping. Column names are resolved against each file's own header.
    pub filter: Option<&'a Predicate>,
}

/// The raw contents of one reference file, parsed once and reused.
struct ReferenceTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// Caches parsed reference tables and the mappings built from them across
/// rules. Rules that ask for the same files with the same columns, transforms
/// and filter share a single `Mapping` and never re-read the file.
#[derive(Default)]
pub struct MappingCache {
    tables: HashMap<(PathBuf, u8), Arc<ReferenceTable>>,
    mappings: HashMap<String, Arc<Mapping>>,
}

impl MappingCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(
        &mut self,
        files: &[PathBuf],
        spec: &FileMappingSpec,
    ) -> Result<Arc<Mapping>, String> {
        if files.is_empty() {
            return Err("mapping_files is set but no files were provided".to_string());
        }

        let key = mapping_signature(files, spec);
        if let Some(mapping) = self.mappings.get(&key) {
            return Ok(mapping.clone());
        }

        let mapping = Arc::new(self.build(files, spec)?);
        self.mappings.insert(key, mapping.clone());
        Ok(mapping)
    }

    fn table(&mut self, path: &Path, delimiter: u8) -> Result<Arc<ReferenceTable>, String> {
        let key = (path.to_path_buf(), delimiter);
        if let Some(table) = self.tables.get(&key) {
            return Ok(table.clone());
        }
        let table = Arc::new(read_table(path, delimiter)?);
        self.tables.insert(key, table.clone());
        Ok(table)
    }

    fn build(&mut self, files: &[PathBuf], spec: &FileMappingSpec) -> Result<Mapping, String> {
        let mut counts: MapCounts = HashMap::new();
        let mut left_scratch = String::new();
        let mut right_scratch = String::new();
        let mut part_buf = String::new();
        let mut left_key = String::new();
        let mut right_key = String::new();

        for path in files {
            let table = self.table(path, spec.delimiter)?;
            let left_idx = resolve_columns(path, spec.left_columns, &table.headers, "left")?;
            let right_idx = resolve_columns(path, spec.right_columns, &table.headers, "right")?;

            // The filter is compiled against this file's own header row.
            let filter = match spec.filter {
                Some(predicate) => Some(crate::rules::compile_predicate(
                    &format!("mapping file {}", path.display()),
                    predicate,
                    &table.headers,
                )?),
                None => None,
            };

            for row in &table.rows {
                if let Some(filter) = &filter {
                    let get = |column: usize| row.get(column).map(String::as_str).unwrap_or("");
                    if !filter.evaluate(&get, spec.trim) {
                        continue;
                    }
                }

                crate::transform::compose(
                    left_idx
                        .iter()
                        .map(|&i| row.get(i).map(String::as_str).unwrap_or("")),
                    spec.left_transforms,
                    spec.join_separator,
                    spec.trim,
                    &mut left_scratch,
                    &mut part_buf,
                    &mut left_key,
                );
                // Several `mapping_right` columns list alternative acceptable
                // values: every non-empty one becomes a candidate target for
                // the key (a union), so the row passes when *any* of them
                // matches the right side. When the rule's `right` is itself a
                // list of columns the mapping columns form a composite value
                // instead. A single column keeps the plain
                // relation/canonical behaviour.
                let left_tokens = split_tokens(&left_key, spec.multi, spec.value_separator);
                if right_idx.len() > 1 && !spec.right_composite {
                    for &i in &right_idx {
                        let raw = row.get(i).map(String::as_str).unwrap_or("");
                        let cell = if spec.trim { raw.trim() } else { raw };
                        crate::transform::apply_pipeline(
                            spec.right_transforms,
                            cell,
                            &mut right_scratch,
                            &mut part_buf,
                        );
                        if right_scratch.trim().is_empty() {
                            continue;
                        }
                        let right_tokens =
                            split_tokens(&right_scratch, spec.multi, spec.value_separator);
                        for l in &left_tokens {
                            for r in &right_tokens {
                                bump_counts(&mut counts, l, r);
                            }
                        }
                    }
                } else {
                    crate::transform::compose(
                        right_idx
                            .iter()
                            .map(|&i| row.get(i).map(String::as_str).unwrap_or("")),
                        spec.right_transforms,
                        spec.join_separator,
                        spec.trim,
                        &mut right_scratch,
                        &mut part_buf,
                        &mut right_key,
                    );
                    let right_tokens =
                        split_tokens(&right_key, spec.multi, spec.value_separator);
                    for (l, r) in pair_tokens(&left_tokens, &right_tokens) {
                        bump_counts(&mut counts, l, r);
                    }
                }
            }
        }

        Ok(Mapping::from_counts_with_targets(
            counts,
            MappingOrigin::File,
            spec.right_columns.len() > 1 && !spec.right_composite,
        ))
    }
}

fn resolve_columns(
    path: &Path,
    columns: &[String],
    headers: &[String],
    side: &str,
) -> Result<Vec<usize>, String> {
    columns
        .iter()
        .map(|column| {
            ColumnResolver::resolve(column, headers).ok_or_else(|| {
                format!(
                    "mapping file {}: {} column '{}' not found (available: {})",
                    path.display(),
                    side,
                    column,
                    headers.join(", ")
                )
            })
        })
        .collect()
}

fn read_table(path: &Path, delimiter: u8) -> Result<ReferenceTable, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("cannot open mapping file {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(64 * 1024);
    builder
        .delimiter(delimiter)
        .has_headers(true)
        .flexible(true);
    let mut reader = builder.from_reader(file);

    let headers: Vec<String> = {
        let record = reader
            .byte_headers()
            .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;
        record
            .iter()
            .map(|cell| String::from_utf8_lossy(cell).into_owned())
            .collect()
    };

    let mut rows = Vec::new();
    let mut record = ByteRecord::new();
    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => rows.push(
                record
                    .iter()
                    .map(|cell| String::from_utf8_lossy(cell).into_owned())
                    .collect(),
            ),
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }
    }

    Ok(ReferenceTable { headers, rows })
}

/// A deterministic key describing exactly what a rule needs from a set of
/// reference files, so equivalent rules share one `Mapping`.
fn mapping_signature(files: &[PathBuf], spec: &FileMappingSpec) -> String {
    let mut out = String::new();
    for file in files {
        out.push_str(&file.display().to_string());
        out.push('\u{1f}');
    }
    out.push_str(&spec.left_columns.join("\u{1e}"));
    out.push('\u{1f}');
    out.push_str(&spec.right_columns.join("\u{1e}"));
    out.push('\u{1f}');
    out.push_str(if spec.right_composite {
        "right=composite"
    } else {
        "right=single"
    });
    out.push('\u{1f}');
    for transform in spec.left_transforms {
        transform.signature(&mut out);
        out.push('\u{1e}');
    }
    out.push('\u{1f}');
    for transform in spec.right_transforms {
        transform.signature(&mut out);
        out.push('\u{1e}');
    }
    out.push('\u{1f}');
    out.push_str(if spec.multi { "multi=1" } else { "multi=0" });
    out.push_str(";sep=");
    spec.value_separator.signature(&mut out);
    out.push_str(";join=");
    out.push_str(spec.join_separator);
    out.push_str(if spec.trim { ";trim=1" } else { ";trim=0" });
    out.push_str(";filter=");
    if let Some(filter) = spec.filter {
        out.push_str(&format!("{filter:?}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::{ColumnSpec, Predicate};

    fn spec<'a>(
        left: &'a [String],
        right: &'a [String],
        separator: &'a Separator,
        filter: Option<&'a Predicate>,
    ) -> FileMappingSpec<'a> {
        FileMappingSpec {
            left_columns: left,
            right_columns: right,
            left_transforms: &[],
            right_transforms: &[],
            multi: false,
            value_separator: separator,
            join_separator: "|",
            trim: false,
            delimiter: b',',
            right_composite: false,
            filter,
        }
    }

    fn path() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/loose_map.csv"
        ))
    }

    #[test]
    fn filters_reference_rows() {
        let left = vec!["code".to_string()];
        let right = vec!["expected".to_string()];
        let separator = Separator::literal(",");
        let filter = Predicate::Eq {
            column: ColumnSpec::Columns(vec!["category".into()]),
            value: "standard".into(),
        };
        let mut cache = MappingCache::new();
        let mapping = cache
            .load(&[path()], &spec(&left, &right, &separator, Some(&filter)))
            .unwrap();

        assert_eq!(mapping.expected("FR"), Some("FR"));
        assert_eq!(mapping.expected("SKIP"), Some("WRONG"));
        // The deprecated row was filtered out.
        assert_eq!(mapping.expected("LEGACY"), None);
        assert_eq!(mapping.len(), 6);
    }

    #[test]
    fn caches_equivalent_mappings() {
        let left = vec!["code".to_string()];
        let right = vec!["expected".to_string()];
        let separator = Separator::literal(",");
        let mut cache = MappingCache::new();

        let first = cache
            .load(&[path()], &spec(&left, &right, &separator, None))
            .unwrap();
        let second = cache
            .load(&[path()], &spec(&left, &right, &separator, None))
            .unwrap();
        // Same rule requirements -> the very same mapping, no re-read.
        assert!(Arc::ptr_eq(&first, &second));

        // A different filter must build a different mapping.
        let filter = Predicate::Eq {
            column: ColumnSpec::Columns(vec!["category".into()]),
            value: "deprecated".into(),
        };
        let filtered = cache
            .load(&[path()], &spec(&left, &right, &separator, Some(&filter)))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &filtered));
        assert_eq!(filtered.expected("LEGACY"), Some("OLD"));
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn multi_targets_are_alternatives_not_ambiguities() {
        let mut hits = HashMap::new();
        hits.insert("DOC".into(), 1u64);
        hits.insert("SUB".into(), 1u64);
        let mut counts: MapCounts = HashMap::new();
        counts.insert("A|B".into(), hits);

        let mapping = Mapping::from_counts_with_targets(counts, MappingOrigin::File, true);
        assert!(mapping.multi_target);
        assert_eq!(mapping.targets_for("A|B").len(), 2);
        // Several `mapping_right` columns are acceptable values, so the key is
        // not reported as ambiguous.
        assert!(!mapping.is_ambiguous("A|B"));
        assert_eq!(mapping.ambiguous_count(), 0);
    }
}
