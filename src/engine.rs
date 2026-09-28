//! Validation engine.
//!
//! Design (mirroring `xan`):
//!
//! * parsing is delegated to the SIMD-accelerated `simd-csv` reader using a
//!   single reusable `ByteRecord` (no per-row allocation);
//! * when the file is seekable it is split into record-aligned byte segments
//!   with `simd_csv::Seeker`, then each segment is read independently by a
//!   rayon worker — the same strategy xan uses for parallel commands;
//! * rules are compiled to integer column indices once, and the hot loop only
//!   does O(1) indexing and cheap `Cow` transforms;
//! * matching/failing rows are aggregated by their `(left, right, expected)`
//!   values, and each group keeps a bounded, deterministic sample of ids;
//!   ambiguity examples use the same bounded min-hash reservoir. The number of
//!   distinct result groups follows the data cardinality, as in the
//!   development branch.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;
use regex::Regex;
use simd_csv::ByteRecord;

use crate::compare::CompareOp;
use crate::expr::{self, EvalContext, Expr, Value, ValueRef};
use crate::mapping::{self, MapCounts, Mapping, MappingOrigin};
use crate::pattern::Separator;
use crate::progress::Progress;
use crate::report::{
    AmbiguityReport, Example, GroupedExample, MappingEntry, MappingReport, Report, RuleReport,
    TargetExample,
};
use crate::rules::{ColumnRef, CompiledPredicate, MappingPlan, Plan};
use crate::sampler::{fnv1a, Sampler};
use crate::transform::{compose, Transform};

const BUFFER_CAPACITY: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub path: PathBuf,
    pub delimiter: u8,
    pub threads: usize,
    pub id_idx: Option<usize>,
    pub progress: Option<Arc<Progress>>,
}

/// Read the header row of the main input.
pub fn read_headers(path: &Path, delimiter: u8) -> Result<Vec<String>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder.delimiter(delimiter).has_headers(true);
    let mut reader = builder.from_reader(file);

    let headers = reader
        .byte_headers()
        .map_err(|e| format!("cannot read headers of {}: {e}", path.display()))?;

    Ok(headers
        .iter()
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect())
}

/// Compute record-aligned byte segments for parallel processing.
pub fn segments_for(path: &Path, delimiter: u8, count: usize) -> Result<Vec<(u64, u64)>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    let mut builder = simd_csv::SeekerBuilder::new();
    builder.delimiter(delimiter).has_headers(true);

    let seeker = builder
        .from_reader(file)
        .map_err(|e| format!("cannot seek {}: {e}", path.display()))?;

    let Some(mut seeker) = seeker else {
        return Ok(Vec::new());
    };

    let ranges = seeker
        .segments(count.max(1))
        .map_err(|e| format!("cannot split {}: {e}", path.display()))?;

    Ok(ranges.into_iter().filter(|(from, to)| to > from).collect())
}

/// A `Read` adapter that reports how many bytes were consumed.
struct CountingReader<R> {
    inner: R,
    progress: Option<Arc<Progress>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(progress) = &self.progress {
            progress.tick(read as u64);
        }
        Ok(read)
    }
}

fn open_segment(
    path: &Path,
    delimiter: u8,
    from: u64,
    to: u64,
    progress: Option<&Arc<Progress>>,
) -> Result<simd_csv::Reader<CountingReader<std::io::Take<File>>>, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(from))
        .map_err(|e| format!("cannot seek {}: {e}", path.display()))?;

    let limited = file.take(to.saturating_sub(from));
    let counted = CountingReader {
        inner: limited,
        progress: progress.cloned(),
    };
    let mut builder = simd_csv::ReaderBuilder::with_capacity(BUFFER_CAPACITY);
    builder
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true);

    Ok(builder.from_reader(counted))
}

// ---------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------

/// Aggregates matching/failing rows by their `(left, right, expected)` values
/// and keeps up to `limit` example ids per group. Groups are merged across
/// parallel segments and the most frequent groups are reported first.
type GroupKey = (String, String, Option<String>);

#[derive(Default)]
struct GroupEntry {
    count: u64,
    /// Up to `limit` ids with the smallest FNV-1a hash, sorted by hash so the
    /// retained sample is deterministic and independent of segment order.
    ids: Vec<(u64, String)>,
}

struct ResultGroups {
    /// Zero disables collection entirely (report limit of zero).
    limit: usize,
    groups: HashMap<GroupKey, GroupEntry>,
}

impl ResultGroups {
    fn new(limit: usize) -> Self {
        ResultGroups {
            limit,
            groups: HashMap::new(),
        }
    }

    fn is_disabled(&self) -> bool {
        self.limit == 0
    }

    /// Record one matching or failing row under its `(left, right, expected)`
    /// condition, keeping up to `limit` example ids for that condition.
    fn observe(&mut self, left: &str, right: &str, expected: Option<&str>, id: &str) {
        if self.limit == 0 {
            return;
        }
        let entry = self
            .groups
            .entry((
                left.to_string(),
                right.to_string(),
                expected.map(str::to_string),
            ))
            .or_default();
        entry.count += 1;
        offer_id(&mut entry.ids, self.limit, id);
    }

    fn merge(&mut self, other: ResultGroups) {
        if other.limit > self.limit {
            self.limit = other.limit;
        }
        for (key, other_entry) in other.groups {
            let entry = self.groups.entry(key).or_default();
            entry.count += other_entry.count;
            for (_, id) in other_entry.ids {
                offer_id(&mut entry.ids, self.limit, &id);
            }
        }
    }

    /// The `limit` most frequent groups, most frequent first (ties broken by
    /// value, so the report is deterministic).
    fn top(&self) -> Vec<GroupedExample> {
        let mut list: Vec<(&GroupKey, &GroupEntry)> = self.groups.iter().collect();
        list.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        list.into_iter()
            .take(self.limit)
            .map(|(key, entry)| GroupedExample {
                left: key.0.clone(),
                right: key.1.clone(),
                expected: key.2.clone(),
                count: entry.count,
                ids: entry.ids.iter().map(|(_, id)| id.clone()).collect(),
            })
            .collect()
    }
}

/// Keep the `limit` smallest-hash ids, sorted by hash. Using a hash order (and
/// not the encounter order) makes the retained sample deterministic when
/// parallel segments are merged in arbitrary order.
fn offer_id(ids: &mut Vec<(u64, String)>, limit: usize, id: &str) {
    if limit == 0 || ids.iter().any(|(_, existing)| existing == id) {
        return;
    }
    let hash = fnv1a(id.as_bytes());
    if ids.len() < limit {
        let pos = ids.partition_point(|(h, _)| *h < hash);
        ids.insert(pos, (hash, id.to_string()));
        return;
    }
    if let Some(&(worst, _)) = ids.last() {
        if hash < worst {
            ids.pop();
            let pos = ids.partition_point(|(h, _)| *h < hash);
            ids.insert(pos, (hash, id.to_string()));
        }
    }
}

struct RuleAccum {
    checked: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    /// Rows skipped by an explicit `validation_skipped` predicate.
    validation_skipped: u64,
    transform_errors: u64,
    unmapped: u64,
    pass_results: ResultGroups,
    fail_results: ResultGroups,
    /// Keyed by the ambiguous input value, so up to `limit` distinct inputs are
    /// reported.
    ambiguous_samples: Sampler<Example>,
}

impl RuleAccum {
    fn new(limit: usize) -> Self {
        RuleAccum {
            checked: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            validation_skipped: 0,
            transform_errors: 0,
            unmapped: 0,
            pass_results: ResultGroups::new(limit),
            fail_results: ResultGroups::new(limit),
            ambiguous_samples: Sampler::new(limit),
        }
    }

    fn merge(&mut self, other: RuleAccum) {
        self.checked += other.checked;
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.validation_skipped += other.validation_skipped;
        self.transform_errors += other.transform_errors;
        self.unmapped += other.unmapped;
        self.pass_results.merge(other.pass_results);
        self.fail_results.merge(other.fail_results);
        self.ambiguous_samples.merge(other.ambiguous_samples);
    }
}

/// A compiled `derive` clause with its column references remapped to positions
/// in the extracted per-row cell vector.
#[derive(Debug, Clone)]
struct DerivedSlot {
    /// How many named outputs the clause produces.
    names: usize,
    expr: Expr,
}

/// Column slots so the hot loop indexes a small per-row vector instead of
/// repeatedly hashing header names.
struct Slots {
    needed: Vec<usize>,
    /// Maps a CSV column index to its position in the extracted cell vector.
    slot_of: Vec<usize>,
    id_slot: Option<usize>,
    /// Per rule: compiled `derive` clauses, in evaluation order.
    rule_derive: Vec<Vec<DerivedSlot>>,
}

impl Slots {
    fn build(plan: &Plan, id_idx: Option<usize>) -> Self {
        let mut needed: Vec<usize> = Vec::with_capacity(plan.rules.len() * 2 + 1);
        for rule in &plan.rules {
            rule.left.collect_columns(&mut needed);
            rule.right.collect_columns(&mut needed);
            if let Some(predicate) = &rule.skip {
                predicate.collect_indices(&mut needed);
            }
            if let Some(predicate) = &rule.auto_mapping_filter {
                predicate.collect_indices(&mut needed);
            }
            for derived in &rule.derive {
                derived.expr.column_refs(&mut needed);
            }
        }
        if let Some(id) = id_idx {
            needed.push(id);
        }
        needed.sort_unstable();
        needed.dedup();

        let width = needed.last().map_or(0, |&column| column + 1);
        let mut slot_of = vec![usize::MAX; width];
        for (slot, &column) in needed.iter().enumerate() {
            slot_of[column] = slot;
        }
        let id_slot = id_idx.map(|id| slot_of[id]);

        // Remap each rule's `derive` expressions from input column indices to
        // positions in the extracted `cells` vector.
        let rule_derive = plan
            .rules
            .iter()
            .map(|rule| {
                rule.derive
                    .iter()
                    .map(|derived| {
                        let mut expr = derived.expr.clone();
                        expr.remap_refs(&mut |reference| match reference {
                            ValueRef::Column(index) => ValueRef::Column(slot_of[index]),
                            other => other,
                        });
                        DerivedSlot {
                            names: derived.names.len(),
                            expr,
                        }
                    })
                    .collect()
            })
            .collect();

        Slots {
            needed,
            slot_of,
            id_slot,
            rule_derive,
        }
    }

    /// The extracted cell for a CSV column (empty when the row is short).
    #[inline]
    fn cell<'a>(&self, cells: &'a [String], column: usize) -> &'a str {
        cells[self.slot_of[column]].as_str()
    }

    #[inline]
    fn extract(&self, record: &ByteRecord, cells: &mut Vec<String>) {
        if cells.len() < self.needed.len() {
            cells.resize_with(self.needed.len(), String::new);
        }
        for (slot, &column) in self.needed.iter().enumerate() {
            let raw = record.get(column).unwrap_or(b"");
            let cell = &mut cells[slot];
            cell.clear();
            cell.push_str(&String::from_utf8_lossy(raw));
        }
    }
}

/// Compose one side of a rule into `out`. `Or` selects the first non-empty
/// candidate column before applying the transform pipeline.
#[allow(clippy::too_many_arguments)]
/// Evaluate a rule's `derive` block, appending the named outputs to `derived`.
#[inline]
fn evaluate_derived(
    slots: &[DerivedSlot],
    cells: &[String],
    separator: &Separator,
    derived: &mut Vec<String>,
) {
    derived.clear();
    if slots.is_empty() {
        return;
    }
    let join = match separator {
        Separator::Literal(literal) => literal.as_str(),
        Separator::Regex(pattern) => pattern.as_str(),
    };
    for clause in slots {
        let value = {
            let ctx = EvalContext {
                cells,
                derived: derived.as_slice(),
            };
            expr::eval(&clause.expr, &ctx)
        };

        if clause.names > 1 {
            if let Value::List(items) = value {
                for index in 0..clause.names {
                    derived.push(
                        items
                            .get(index)
                            .map(|item| item.scalar_string(join))
                            .unwrap_or_default(),
                    );
                }
                continue;
            }
            derived.push(value.scalar_string(join));
            for _ in 1..clause.names {
                derived.push(String::new());
            }
        } else {
            derived.push(value.scalar_string(join));
        }
    }
}

/// The string value of one side reference: an extracted cell or a derived
/// output.
#[inline]
fn side_value<'a>(
    reference: &ValueRef,
    cells: &'a [String],
    slots: &Slots,
    derived: &'a [String],
) -> &'a str {
    match reference {
        ValueRef::Column(index) => slots.cell(cells, *index),
        ValueRef::Derived(index) => derived.get(*index).map(String::as_str).unwrap_or(""),
    }
}

fn compose_side(
    side: &ColumnRef,
    cells: &[String],
    slots: &Slots,
    derived: &[String],
    transforms: &[Transform],
    join: &str,
    trim: bool,
    part: &mut String,
    scratch: &mut String,
    out: &mut String,
) -> bool {
    match side {
        ColumnRef::Columns(values) => compose(
            values
                .iter()
                .map(|reference| side_value(reference, cells, slots, derived)),
            transforms,
            join,
            trim,
            part,
            scratch,
            out,
        ),
        ColumnRef::Or(values) => {
            let chosen = values
                .iter()
                .map(|reference| side_value(reference, cells, slots, derived))
                .find(|value| !(if trim { value.trim() } else { *value }).is_empty());
            match chosen {
                Some(value) => compose(
                    std::iter::once(value),
                    transforms,
                    join,
                    trim,
                    part,
                    scratch,
                    out,
                ),
                None => {
                    out.clear();
                    true
                }
            }
        }
    }
}

/// Evaluate a compiled predicate against the extracted row cells.
#[inline]
fn predicate_holds(
    predicate: &CompiledPredicate,
    cells: &[String],
    slots: &Slots,
    trim: bool,
) -> bool {
    predicate.evaluate(&|column| slots.cell(cells, column), trim)
}

// ---------------------------------------------------------------------------
// Pass 1: build auto mappings
// ---------------------------------------------------------------------------

fn fill_tokens(buffer: &str, multi: bool, separator: &Separator, out: &mut Vec<String>) {
    mapping::split_tokens_into(buffer, multi, separator, out);
}

/// Count records in one segment without applying any rule. Used to assign
/// exact global row numbers when there is no id column, so validation can
/// still run in parallel.
fn count_rows_segment(
    path: &Path,
    delimiter: u8,
    from: u64,
    to: u64,
    progress: Option<&Arc<Progress>>,
) -> Result<u64, String> {
    let mut reader = open_segment(path, delimiter, from, to, progress)?;
    let mut record = ByteRecord::new();
    let mut count = 0u64;
    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => count += 1,
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn build_counts_segment(
    path: &Path,
    delimiter: u8,
    rules: &[crate::rules::CompiledRule],
    auto_indices: &[usize],
    slots: &Slots,
    from: u64,
    to: u64,
    progress: Option<&Arc<Progress>>,
) -> Result<Vec<MapCounts>, String> {
    let mut result: Vec<MapCounts> = auto_indices.iter().map(|_| MapCounts::new()).collect();
    let mut reader = open_segment(path, delimiter, from, to, progress)?;
    let mut record = ByteRecord::new();
    let mut cells: Vec<String> = Vec::with_capacity(slots.needed.len());
    let mut left_buf = String::new();
    let mut right_buf = String::new();
    let mut component_buf = String::new();
    let mut component_buf2 = String::new();
    let mut left_tokens: Vec<String> = Vec::new();
    let mut right_tokens: Vec<String> = Vec::new();
    let mut derived: Vec<String> = Vec::new();

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }

        slots.extract(&record, &mut cells);

        for (position, &rule_index) in auto_indices.iter().enumerate() {
            let rule = &rules[rule_index];

            // Rows skipped for validation, or excluded by the auto-mapping
            // filter, do not contribute to the extracted mapping.
            if let Some(predicate) = &rule.skip {
                if predicate_holds(predicate, &cells, slots, rule.trim) {
                    continue;
                }
            }
            if let Some(predicate) = &rule.auto_mapping_filter {
                if !predicate_holds(predicate, &cells, slots, rule.trim) {
                    continue;
                }
            }

            evaluate_derived(&slots.rule_derive[rule_index], &cells, &rule.separator, &mut derived);

            compose_side(
                &rule.left,
                &cells,
                slots,
                &derived,
                &rule.transform_left,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut component_buf2,
                &mut left_buf,
            );
            compose_side(
                &rule.right,
                &cells,
                slots,
                &derived,
                &rule.transform_right,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut component_buf2,
                &mut right_buf,
            );
            fill_tokens(&left_buf, rule.multi, &rule.separator, &mut left_tokens);
            fill_tokens(&right_buf, rule.multi, &rule.separator, &mut right_tokens);

            let counts = &mut result[position];
            for (left, right) in left_tokens.iter().zip(right_tokens.iter()) {
                mapping::bump_counts(counts, left, right);
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Pass 2: validate
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn validate_segment(
    path: &Path,
    delimiter: u8,
    rules: &[crate::rules::CompiledRule],
    mappings: &[Option<Arc<Mapping>>],
    slots: &Slots,
    from: u64,
    to: u64,
    row_base: Option<u64>,
    progress: Option<&Arc<Progress>>,
) -> Result<Vec<RuleAccum>, String> {
    let mut accums: Vec<RuleAccum> = rules
        .iter()
        .map(|rule| RuleAccum::new(rule.report_limit))
        .collect();
    let mut reader = open_segment(path, delimiter, from, to, progress)?;
    let mut record = ByteRecord::new();
    let mut cells: Vec<String> = Vec::with_capacity(slots.needed.len());

    let mut expected_buf: Vec<String> = Vec::new();
    let mut left_buf = String::new();
    let mut right_buf = String::new();
    let mut component_buf = String::new();
    let mut component_buf2 = String::new();
    let mut left_tokens: Vec<String> = Vec::new();
    let mut right_tokens: Vec<String> = Vec::new();
    let mut derived: Vec<String> = Vec::new();

    let mut local_row: u64 = 0;

    loop {
        match reader.read_byte_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(format!("error reading {}: {e}", path.display())),
        }

        local_row += 1;
        let row_number = row_base.map(|base| base + local_row - 1);
        slots.extract(&record, &mut cells);

        for (rule_index, rule) in rules.iter().enumerate() {
            let accum = &mut accums[rule_index];

            // An explicit `validation_skipped` predicate marks the row as
            // skipped. `mapping_filter` no longer skips validation; it only
            // selects which rows define the mapping.
            if let Some(predicate) = &rule.skip {
                if predicate_holds(predicate, &cells, slots, rule.trim) {
                    accum.checked += 1;
                    accum.validation_skipped += 1;
                    continue;
                }
            }

            evaluate_derived(&slots.rule_derive[rule_index], &cells, &rule.separator, &mut derived);

            let left_ok = compose_side(
                &rule.left,
                &cells,
                slots,
                &derived,
                &rule.transform_left,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut component_buf2,
                &mut left_buf,
            );
            let right_ok = compose_side(
                &rule.right,
                &cells,
                slots,
                &derived,
                &rule.transform_right,
                &rule.join_separator,
                rule.trim,
                &mut component_buf,
                &mut component_buf2,
                &mut right_buf,
            );
            // Optional relation: when both the source and the target are empty
            // the row is skipped instead of being reported as a failure. For
            // pattern rules there is no target, so an empty value is skipped.
            if rule.allow_empty
                && left_buf.is_empty()
                && (rule.pattern.is_some() || right_buf.is_empty())
            {
                accum.checked += 1;
                accum.skipped += 1;
                continue;
            }

            // Regex rules only need the left value: the rule-level pattern
            // decides the outcome, mirroring xan's `match(value, regex(...))`.
            let id = match slots.id_slot {
                Some(slot) => {
                    let value = cells[slot].trim();
                    if value.is_empty() {
                        format!("row:{}", row_number.unwrap_or(local_row))
                    } else {
                        value.to_string()
                    }
                }
                None => format!("row:{}", row_number.unwrap_or(local_row)),
            };

            let mapping = mappings[rule_index].as_deref();
            let expected: &[String];
            let matched;

            if let Some(pattern) = &rule.pattern {
                let is_match = pattern.is_match(&left_buf);
                matched = match rule.compare {
                    CompareOp::NotMatches => left_ok && !is_match,
                    _ => left_ok && is_match,
                };
                expected = &[];
            } else {
                fill_tokens(&left_buf, rule.multi, &rule.separator, &mut left_tokens);
                fill_tokens(&right_buf, rule.multi, &rule.separator, &mut right_tokens);

                if let Some(mapping) = mapping {
                    let mut count = 0usize;
                    for token in &left_tokens {
                        match mapping.expected(token) {
                            Some(target) => token_slot(&mut expected_buf, count).push_str(target),
                            None => {
                                accum.unmapped += 1;
                                // `\u{0}` cannot appear in a real target, so
                                // this sentinel can never accidentally match.
                                let slot = token_slot(&mut expected_buf, count);
                                slot.push('\u{0}');
                                slot.push_str(token);
                            }
                        }
                        count += 1;

                        if mapping.is_ambiguous(token) && !accum.ambiguous_samples.is_disabled() {
                            accum.ambiguous_samples.offer_with(token, || Example {
                                id: id.clone(),
                                row: row_number,
                                left: token.clone(),
                                right: right_buf.clone(),
                                expected: mapping.expected(token).map(str::to_string),
                            });
                        }
                    }
                    expected_buf.truncate(count);
                    expected_buf.sort_unstable();
                    expected_buf.dedup();
                    expected = &expected_buf;
                } else {
                    // No mapping: the transformed left value *is* the
                    // expected set, so sort it in place instead of cloning.
                    left_tokens.sort_unstable();
                    left_tokens.dedup();
                    expected = &left_tokens;
                }

                right_tokens.sort_unstable();
                right_tokens.dedup();

                matched = left_ok && right_ok && rule.compare.evaluate(expected, &right_tokens);
            }

            accum.checked += 1;
            if !left_ok || !right_ok {
                accum.transform_errors += 1;
            }

            if matched {
                accum.passed += 1;
                if !accum.pass_results.is_disabled() {
                    let expected_example =
                        sample_expected(rule.pattern.as_ref(), mapping, expected);
                    accum.pass_results.observe(
                        &left_buf,
                        &right_buf,
                        expected_example.as_deref(),
                        &id,
                    );
                }
            } else {
                accum.failed += 1;
                if !accum.fail_results.is_disabled() {
                    let expected_example =
                        sample_expected(rule.pattern.as_ref(), mapping, expected);
                    accum.fail_results.observe(
                        &left_buf,
                        &right_buf,
                        expected_example.as_deref(),
                        &id,
                    );
                }
            }
        }
    }

    Ok(accums)
}

/// Return a cleared `String` at `index`, growing the buffer if needed. Used to
/// reuse token storage across rows instead of allocating a new `String`.
#[inline]
fn token_slot(out: &mut Vec<String>, index: usize) -> &mut String {
    if index >= out.len() {
        out.push(String::new());
    }
    let slot = &mut out[index];
    slot.clear();
    slot
}

fn sample_expected(
    pattern: Option<&Regex>,
    mapping: Option<&Mapping>,
    expected: &[String],
) -> Option<String> {
    match pattern {
        Some(pattern) => Some(pattern.as_str().to_string()),
        None => mapping_expected(mapping, expected),
    }
}

fn mapping_expected(mapping: Option<&Mapping>, expected: &[String]) -> Option<String> {
    mapping.map(|_| {
        expected
            .iter()
            .map(|value| match value.strip_prefix('\u{0}') {
                Some(unmapped) => format!("<unmapped:{unmapped}>"),
                None => value.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

pub fn run(plan: &Plan, config: &EngineConfig) -> Result<Report, String> {
    let id_idx = config.id_idx;
    let thread_count = config.threads.max(1);

    let segments = segments_for(&config.path, config.delimiter, thread_count)?;

    // Global row numbers need per-segment offsets, so when there is no id
    // column we run a cheap parallel counting pass instead of serialising the
    // whole validation.
    let need_row_offsets = id_idx.is_none() && segments.len() > 1;

    let slots = Slots::build(plan, id_idx);

    // Resolve file-based mappings up-front, reusing parsed reference tables
    // and equivalent mappings across rules.
    let mut cache = mapping::MappingCache::new();
    let mut mappings: Vec<Option<Arc<Mapping>>> = Vec::with_capacity(plan.rules.len());
    for rule in &plan.rules {
        let mapping = match &rule.mapping {
            MappingPlan::Files {
                files,
                left,
                right,
                multi,
                separator,
                filter,
            } => cache
                .load(
                    files,
                    &mapping::FileMappingSpec {
                        left_columns: left,
                        right_columns: right,
                        left_transforms: &rule.transform_left,
                        right_transforms: &rule.transform_right,
                        multi: *multi,
                        value_separator: separator,
                        join_separator: &rule.join_separator,
                        trim: rule.trim,
                        delimiter: config.delimiter,
                        filter: filter.as_ref(),
                    },
                )
                .map(Some)?,
            _ => None,
        };
        mappings.push(mapping);
    }

    let auto_indices: Vec<usize> = plan
        .rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| matches!(rule.mapping, MappingPlan::Auto))
        .map(|(index, _)| index)
        .collect();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .map_err(|e| format!("cannot create thread pool: {e}"))?;

    // Pass 1 (auto mapping) and pass 2 (validation) each read the whole file;
    // the optional counting pass does too.
    let progress = config.progress.as_ref();
    if let Some(progress) = progress {
        if progress.enabled() {
            let mut passes: u64 = if auto_indices.is_empty() { 1 } else { 2 };
            if need_row_offsets {
                passes += 1;
            }
            let file_size = std::fs::metadata(&config.path)
                .map(|m| m.len())
                .unwrap_or(0);
            progress.set_total(file_size * passes);
        }
    }

    // First row number (1-based) of each segment, or `None` when row numbers
    // are not available (parallel run with an id column).
    let row_bases: Vec<Option<u64>> = if segments.len() <= 1 {
        vec![Some(1); segments.len()]
    } else if need_row_offsets {
        let counts: Vec<u64> = pool.install(|| {
            segments
                .par_iter()
                .map(|&(from, to)| {
                    count_rows_segment(&config.path, config.delimiter, from, to, progress)
                })
                .collect::<Result<Vec<_>, String>>()
        })?;
        let mut bases = Vec::with_capacity(counts.len());
        let mut next = 1u64;
        for count in counts {
            bases.push(Some(next));
            next += count;
        }
        bases
    } else {
        vec![None; segments.len()]
    };

    // Pass 1: extract auto mappings from the data.
    if !auto_indices.is_empty() {
        let per_segment: Vec<Vec<MapCounts>> = pool.install(|| {
            segments
                .par_iter()
                .map(|&(from, to)| {
                    build_counts_segment(
                        &config.path,
                        config.delimiter,
                        &plan.rules,
                        &auto_indices,
                        &slots,
                        from,
                        to,
                        progress,
                    )
                })
                .collect::<Result<Vec<_>, String>>()
        })?;

        let mut totals: Vec<MapCounts> =
            (0..auto_indices.len()).map(|_| MapCounts::new()).collect();
        for segment in per_segment {
            for (position, counts) in segment.into_iter().enumerate() {
                mapping::merge_counts(&mut totals[position], counts);
            }
        }
        for (position, &rule_index) in auto_indices.iter().enumerate() {
            mappings[rule_index] = Some(Arc::new(Mapping::from_counts_auto(std::mem::take(
                &mut totals[position],
            ))));
        }
    }

    // Pass 2: validate every rule.
    let per_segment: Vec<Vec<RuleAccum>> = pool.install(|| {
        segments
            .par_iter()
            .zip(row_bases.par_iter())
            .map(|(&(from, to), &row_base)| {
                validate_segment(
                    &config.path,
                    config.delimiter,
                    &plan.rules,
                    &mappings,
                    &slots,
                    from,
                    to,
                    row_base,
                    progress,
                )
            })
            .collect::<Result<Vec<_>, String>>()
    })?;

    let mut accums: Vec<RuleAccum> = plan
        .rules
        .iter()
        .map(|rule| RuleAccum::new(rule.report_limit))
        .collect();
    for segment in per_segment {
        for (index, accum) in segment.into_iter().enumerate() {
            accums[index].merge(accum);
        }
    }

    if let Some(progress) = progress {
        progress.finish();
    }

    Ok(build_report(plan, accums, mappings))
}

fn build_report(
    plan: &Plan,
    accums: Vec<RuleAccum>,
    mappings: Vec<Option<Arc<Mapping>>>,
) -> Report {
    let mut rules = Vec::with_capacity(plan.rules.len());
    let mut rows_checked = 0u64;

    for (index, rule) in plan.rules.iter().enumerate() {
        let accum = &accums[index];
        rows_checked = rows_checked.max(accum.checked);

        let mapping_report = mappings[index].as_ref().map(|mapping| {
            build_mapping_report(mapping, &accum.ambiguous_samples, rule.report_limit)
        });

        let ambiguous_inputs = mapping_report
            .as_ref()
            .map_or(0, |report| report.ambiguous_inputs);
        let status = if accum.failed == 0 && ambiguous_inputs == 0 {
            "passed"
        } else {
            "failed"
        };

        rules.push(RuleReport {
            name: rule.name.clone(),
            left: rule.left_name.clone(),
            right: rule.right_name.clone(),
            status: status.to_string(),
            rows_checked: accum.checked,
            rows_passed: accum.passed,
            rows_failed: accum.failed,
            rows_skipped: accum.skipped,
            rows_validation_skipped: accum.validation_skipped,
            transform_errors: accum.transform_errors,
            unmapped_values: accum.unmapped,
            pass_results: accum.pass_results.top(),
            fail_results: accum.fail_results.top(),
            mapping: mapping_report,
        });
    }

    let rules_passed = rules.iter().filter(|r| r.passed()).count();

    Report {
        rules_total: rules.len(),
        rules_passed,
        rules_failed: rules.len() - rules_passed,
        rows_checked,
        rules,
    }
}

fn build_mapping_report(
    mapping: &Mapping,
    ambiguous_samples: &Sampler<Example>,
    limit: usize,
) -> MappingReport {
    let mut entries: Vec<MappingEntry> = mapping
        .targets
        .iter()
        .map(|(input, targets)| MappingEntry {
            input: input.to_string(),
            canonical: mapping.expected(input).unwrap_or("").to_string(),
            ambiguous: targets.len() > 1,
            targets: targets
                .iter()
                .map(|(value, count)| TargetExample {
                    value: value.to_string(),
                    count: *count,
                })
                .collect(),
        })
        .collect();
    entries.sort_by(|a, b| a.input.cmp(&b.input));

    // Collect one example row per ambiguous input.
    let examples_by_input: HashMap<String, Example> = ambiguous_samples
        .payloads()
        .into_iter()
        .map(|example| (example.left.clone(), example))
        .collect();

    let mut ambiguous_inputs: Vec<&MappingEntry> =
        entries.iter().filter(|entry| entry.ambiguous).collect();
    // Report the worst offenders first (most targets, then most observations).
    ambiguous_inputs.sort_by(|a, b| {
        b.targets
            .len()
            .cmp(&a.targets.len())
            .then_with(|| total_count(b).cmp(&total_count(a)))
            .then_with(|| a.input.cmp(&b.input))
    });

    let ambiguities: Vec<AmbiguityReport> = ambiguous_inputs
        .into_iter()
        .take(limit)
        .map(|entry| AmbiguityReport {
            input: entry.input.clone(),
            targets: entry.targets.iter().take(limit).cloned().collect(),
            examples: examples_by_input
                .get(&entry.input)
                .cloned()
                .into_iter()
                .collect(),
        })
        .collect();

    MappingReport {
        source: match mapping.origin {
            MappingOrigin::Auto => "auto".to_string(),
            MappingOrigin::File => "file".to_string(),
        },
        distinct_inputs: mapping.len(),
        ambiguous_inputs: mapping.ambiguous_count(),
        entries,
        ambiguities,
    }
}

fn total_count(entry: &MappingEntry) -> u64 {
    entry.targets.iter().map(|t| t.count).sum()
}
