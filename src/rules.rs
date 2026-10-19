//! Compiled rules: DSL definitions resolved against the input headers.

use std::path::PathBuf;

use regex::Regex;

use crate::compare::CompareOp;
use crate::dsl::{ColumnSpec, FallbackMode, MappingSourceDef, Predicate, Program};
use crate::expr::{BinOp, Expr, ValueRef};
use crate::pattern::Separator;
use crate::transform::Transform;

/// Resolve a column reference (a header name or `#index`) against headers.
pub struct ColumnResolver;

impl ColumnResolver {
    pub fn resolve(reference: &str, headers: &[String]) -> Option<usize> {
        let reference = reference.trim();

        if let Some(rest) = reference.strip_prefix('#') {
            if let Ok(index) = rest.trim().parse::<usize>() {
                return (index < headers.len()).then_some(index);
            }
        }

        if let Some(index) = headers.iter().position(|h| h == reference) {
            return Some(index);
        }

        // Case-insensitive fallback.
        headers
            .iter()
            .position(|h| h.eq_ignore_ascii_case(reference))
    }
}

#[derive(Debug, Clone)]
pub enum MappingPlan {
    None,
    /// Extract the mapping from the data itself.
    Auto,
    /// One or more ordered reference sources, resolved left to right.
    Files(Vec<MappingSourcePlan>),
}

/// One compiled `mapping { ... }` source.
#[derive(Debug, Clone)]
pub struct MappingSourcePlan {
    pub files: Vec<PathBuf>,
    /// Reference-file column(s) forming the lookup key.
    pub left: Vec<String>,
    /// Reference-file column(s) forming the target value.
    pub right: Vec<String>,
    /// Data-row value(s) forming the lookup key.
    pub key: ColumnRef,
    pub multi: bool,
    pub separator: Separator,
    /// Optional predicate over reference rows; only matching rows define the
    /// mapping. Columns are resolved against each file's header.
    pub filter: Option<Predicate>,
    /// When to consult the next source instead of this one.
    pub when: FallbackMode,
}

/// A resolved side of a rule: one or more value references (an input column or
/// a value produced by `derive`), or a fallback list where the first non-empty
/// value wins.
#[derive(Debug, Clone)]
pub enum ColumnRef {
    Columns(Vec<ValueRef>),
    Or(Vec<ValueRef>),
}

impl ColumnRef {
    pub fn references(&self) -> &[ValueRef] {
        match self {
            ColumnRef::Columns(values) | ColumnRef::Or(values) => values,
        }
    }

    /// Append every referenced input column index (derived values excluded).
    pub fn collect_columns(&self, out: &mut Vec<usize>) {
        for reference in self.references() {
            if let ValueRef::Column(index) = reference {
                out.push(*index);
            }
        }
    }
}

/// A compiled row predicate. All column references are resolved to indices.
#[derive(Debug, Clone)]
pub enum CompiledPredicate {
    In {
        column: ColumnRef,
        values: Vec<String>,
    },
    AnyIn {
        columns: Vec<ColumnRef>,
        values: Vec<String>,
    },
    AllIn {
        columns: Vec<ColumnRef>,
        values: Vec<String>,
    },
    Eq {
        column: ColumnRef,
        value: String,
    },
    Ne {
        column: ColumnRef,
        value: String,
    },
    Empty {
        column: ColumnRef,
    },
    NotEmpty {
        column: ColumnRef,
    },
    And(Vec<CompiledPredicate>),
    Or(Vec<CompiledPredicate>),
    Not(Box<CompiledPredicate>),
    Const(bool),
}

impl CompiledPredicate {
    /// Append every referenced column index (used to build the row extractor).
    pub fn collect_indices(&self, out: &mut Vec<usize>) {
        match self {
            CompiledPredicate::In { column, .. }
            | CompiledPredicate::Eq { column, .. }
            | CompiledPredicate::Ne { column, .. }
            | CompiledPredicate::Empty { column }
            | CompiledPredicate::NotEmpty { column } => column.collect_columns(out),
            CompiledPredicate::AnyIn { columns, .. } | CompiledPredicate::AllIn { columns, .. } => {
                for column in columns {
                    column.collect_columns(out);
                }
            }
            CompiledPredicate::And(parts) | CompiledPredicate::Or(parts) => {
                for part in parts {
                    part.collect_indices(out);
                }
            }
            CompiledPredicate::Not(inner) => inner.collect_indices(out),
            CompiledPredicate::Const(_) => {}
        }
    }

    /// Evaluate the predicate, reading a column's value through `get`.
    pub fn evaluate<'a, F>(&self, get: &F, trim: bool) -> bool
    where
        F: Fn(usize) -> &'a str,
    {
        match self {
            CompiledPredicate::Const(value) => *value,
            CompiledPredicate::In { column, values } => value_of(column, get, trim)
                .is_some_and(|value| values.iter().any(|candidate| candidate == value)),
            CompiledPredicate::AnyIn { columns, values } => columns.iter().any(|column| {
                value_of(column, get, trim)
                    .is_some_and(|value| values.iter().any(|candidate| candidate == value))
            }),
            CompiledPredicate::AllIn { columns, values } => columns.iter().all(|column| {
                value_of(column, get, trim)
                    .is_some_and(|value| values.iter().any(|candidate| candidate == value))
            }),
            CompiledPredicate::Eq { column, value } => {
                value_of(column, get, trim) == Some(value.as_str())
            }
            CompiledPredicate::Ne { column, value } => {
                value_of(column, get, trim) != Some(value.as_str())
            }
            CompiledPredicate::Empty { column } => value_of(column, get, trim).is_none(),
            CompiledPredicate::NotEmpty { column } => value_of(column, get, trim).is_some(),
            CompiledPredicate::And(parts) => parts.iter().all(|part| part.evaluate(get, trim)),
            CompiledPredicate::Or(parts) => parts.iter().any(|part| part.evaluate(get, trim)),
            CompiledPredicate::Not(inner) => !inner.evaluate(get, trim),
        }
    }
}

/// The single normalized value a predicate looks at, or `None` when the
/// selected column(s) are empty.
fn value_of<'a, F>(side: &ColumnRef, get: &F, trim: bool) -> Option<&'a str>
where
    F: Fn(usize) -> &'a str,
{
    let normalize = |value: &'a str| if trim { value.trim() } else { value };
    match side {
        ColumnRef::Columns(values) => {
            let value = normalize(reference_value(values.first(), get));
            (!value.is_empty()).then_some(value)
        }
        ColumnRef::Or(values) => values
            .iter()
            .map(|value| normalize(reference_value(Some(value), get)))
            .find(|value| !value.is_empty()),
    }
}

/// A predicate can only reference input columns, so a derived value (if one
/// somehow reached here) resolves to the empty string.
fn reference_value<'a, F>(reference: Option<&ValueRef>, get: &F) -> &'a str
where
    F: Fn(usize) -> &'a str,
{
    match reference {
        Some(ValueRef::Column(index)) => get(*index),
        _ => "",
    }
}

#[derive(Debug, Clone)]
pub struct CompiledDerived {
    pub names: Vec<String>,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    /// Human-readable, e.g. `product + region` for composite keys.
    pub left_name: String,
    pub right_name: String,
    /// One or more values per side; `Or` picks the first non-empty one.
    pub left: ColumnRef,
    pub right: ColumnRef,
    pub transform_left: Vec<Transform>,
    pub transform_right: Vec<Transform>,
    /// Named values computed from the row before the comparison.
    pub derive: Vec<CompiledDerived>,
    pub compare: CompareOp,
    pub multi: bool,
    pub separator: Separator,
    pub join_separator: String,
    /// Compiled regex for `compare = matches | not_matches`.
    pub pattern: Option<Regex>,
    pub trim: bool,
    pub allow_empty: bool,
    /// When this holds, the row is skipped (see `validation_skipped`).
    pub skip: Option<CompiledPredicate>,
    /// The `validation_skipped` predicate as written, for the report.
    pub skip_source: Option<String>,
    /// Predicate restricting which data rows an `auto` mapping observes
    /// (compiled against the input headers). `None` for file mappings, whose
    /// filter is resolved against each reference file instead.
    pub auto_mapping_filter: Option<CompiledPredicate>,
    pub mapping: MappingPlan,
    pub report_limit: usize,
}

impl CompiledRule {
    /// Every input column index the rule reads: both sides, the `derive`
    /// expressions and any row predicates. Used by the GUI to show just the
    /// attributes a rule evaluates.
    pub fn used_columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.left.collect_columns(&mut out);
        self.right.collect_columns(&mut out);
        if let MappingPlan::Files(sources) = &self.mapping {
            for source in sources {
                source.key.collect_columns(&mut out);
            }
        }
        if let Some(predicate) = &self.skip {
            predicate.collect_indices(&mut out);
        }
        if let Some(predicate) = &self.auto_mapping_filter {
            predicate.collect_indices(&mut out);
        }
        for derived in &self.derive {
            derived.expr.column_refs(&mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub rules: Vec<CompiledRule>,
    /// Optional label for the left side, copied from `defaults` and used by the
    /// report (defaults to `left`).
    pub left_label: Option<String>,
    /// Optional label for the right side, copied from `defaults` and used by
    /// the report (defaults to `right`).
    pub right_label: Option<String>,
}

fn resolve_all(
    context: &str,
    side: &str,
    columns: &[String],
    headers: &[String],
    derived_names: &mut Vec<String>,
    mut derive_sink: Option<&mut Vec<CompiledDerived>>,
) -> Result<Vec<ValueRef>, String> {
    let mut references = Vec::with_capacity(columns.len());

    for column in columns {
        // A `derive`d name shadows an input column of the same name.
        if let Some(index) = derived_names.iter().position(|name| name == column) {
            references.push(ValueRef::Derived(index));
            continue;
        }

        if let Some(index) = ColumnResolver::resolve(column, headers) {
            references.push(ValueRef::Column(index));
            continue;
        }

        // Not a column: allow a bare expression such as `col1 - col2` on
        // either side. It is compiled into an anonymous derived value so the
        // rest of the engine keeps operating on the usual side references.
        if let Some(sink) = derive_sink.as_deref_mut() {
            if let Ok(mut expression) = crate::expr::parse_filter(column) {
                if resolve_expr(context, &mut expression, headers, derived_names).is_ok() {
                    // The synthetic name is only used for index bookkeeping;
                    // `\u{0}` cannot appear in a real column name.
                    let index = derived_names.len();
                    let name = format!("\u{0}{index}");
                    derived_names.push(name.clone());
                    sink.push(CompiledDerived {
                        names: vec![name],
                        expr: expression,
                    });
                    references.push(ValueRef::Derived(index));
                    continue;
                }
            }
        }

        return Err(format!(
            "{context}: {side} column '{column}' not found (available: {})",
            headers.join(", ")
        ));
    }

    Ok(references)
}

fn compile_columns(
    context: &str,
    side: &str,
    spec: &ColumnSpec,
    headers: &[String],
    derived_names: &mut Vec<String>,
    derive_sink: Option<&mut Vec<CompiledDerived>>,
) -> Result<ColumnRef, String> {
    let references = resolve_all(
        context,
        side,
        spec.names(),
        headers,
        derived_names,
        derive_sink,
    )?;
    Ok(match spec {
        ColumnSpec::Columns(_) => ColumnRef::Columns(references),
        ColumnSpec::Or(_) => ColumnRef::Or(references),
    })
}

/// Predicates operate on a single value, so a composite list is not allowed.
fn compile_condition_column(
    context: &str,
    spec: &ColumnSpec,
    headers: &[String],
) -> Result<ColumnRef, String> {
    if let ColumnSpec::Columns(names) = spec {
        if names.len() > 1 {
            return Err(format!(
                "{context}: a predicate column must be a single column or or(...), got [{}]",
                names.join(", ")
            ));
        }
    }
    let mut derived = Vec::new();
    compile_columns(context, "condition", spec, headers, &mut derived, None)
}

/// Compile a DSL predicate against a set of headers. `context` labels any
/// resolution error (e.g. `rule 'x'` or `mapping file 'ref.csv'`).
pub fn compile_predicate(
    context: &str,
    predicate: &Predicate,
    headers: &[String],
) -> Result<CompiledPredicate, String> {
    Ok(match predicate {
        Predicate::In { column, values } => CompiledPredicate::In {
            column: compile_condition_column(context, column, headers)?,
            values: values.clone(),
        },
        Predicate::AnyIn { columns, values } => CompiledPredicate::AnyIn {
            columns: columns
                .iter()
                .map(|column| compile_condition_column(context, column, headers))
                .collect::<Result<Vec<_>, _>>()?,
            values: values.clone(),
        },
        Predicate::AllIn { columns, values } => CompiledPredicate::AllIn {
            columns: columns
                .iter()
                .map(|column| compile_condition_column(context, column, headers))
                .collect::<Result<Vec<_>, _>>()?,
            values: values.clone(),
        },
        Predicate::Eq { column, value } => CompiledPredicate::Eq {
            column: compile_condition_column(context, column, headers)?,
            value: value.clone(),
        },
        Predicate::Ne { column, value } => CompiledPredicate::Ne {
            column: compile_condition_column(context, column, headers)?,
            value: value.clone(),
        },
        Predicate::Empty { column } => CompiledPredicate::Empty {
            column: compile_condition_column(context, column, headers)?,
        },
        Predicate::NotEmpty { column } => CompiledPredicate::NotEmpty {
            column: compile_condition_column(context, column, headers)?,
        },
        Predicate::And(parts) => CompiledPredicate::And(
            parts
                .iter()
                .map(|part| compile_predicate(context, part, headers))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Predicate::Or(parts) => CompiledPredicate::Or(
            parts
                .iter()
                .map(|part| compile_predicate(context, part, headers))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Predicate::Not(inner) => {
            CompiledPredicate::Not(Box::new(compile_predicate(context, inner, headers)?))
        }
        Predicate::Const(value) => CompiledPredicate::Const(*value),
    })
}

/// Resolve the identifiers of a `derive` expression against earlier derived
/// names (first) and input headers (second).
///
/// `in_any(...)` calls are resolved here too: `in_any([a, b, c], [accepted...])`
/// becomes a disjunction of plain membership tests.
fn resolve_expr(
    context: &str,
    expr: &mut Expr,
    headers: &[String],
    derived: &[String],
) -> Result<(), String> {
    if let Expr::Call { name, args } = expr {
        if name == "in_any" {
            let mut args = args.clone();
            if args.len() != 2 {
                return Err(format!(
                    "{context}: in_any(values, accepted) expects 2 arguments, got {}",
                    args.len()
                ));
            }
            let values = match args.remove(0) {
                Expr::List(values) => values,
                _ => {
                    return Err(format!(
                        "{context}: in_any(values, accepted) expects a list of value expressions"
                    ))
                }
            };
            let accepted = args.remove(0);
            *expr = resolve_in_any(context, values, &accepted, headers, derived)?;
            return Ok(());
        }
        if name == "col" {
            *expr = resolve_col_call(args, headers)
                .map_err(|message| format!("{context}: {message}"))?;
            return Ok(());
        }
    }

    match expr {
        Expr::Ident(name) => {
            if let Some(index) = derived.iter().position(|value| value == name) {
                *expr = Expr::Ref(ValueRef::Derived(index));
            } else if let Some(index) = ColumnResolver::resolve(name, headers) {
                *expr = Expr::Ref(ValueRef::Column(index));
            } else {
                return Err(format!(
                    "{context}: expression references unknown column or value '{name}'"
                ));
            }
        }
        Expr::List(items) => items
            .iter_mut()
            .try_for_each(|item| resolve_expr(context, item, headers, derived))?,
        Expr::Call { args, .. } => args
            .iter_mut()
            .try_for_each(|arg| resolve_expr(context, arg, headers, derived))?,
        Expr::Binary { lhs, rhs, .. } => {
            resolve_expr(context, lhs, headers, derived)?;
            resolve_expr(context, rhs, headers, derived)?;
        }
        Expr::Unary { operand, .. } => resolve_expr(context, operand, headers, derived)?,
        Expr::Index { target, index } => {
            resolve_expr(context, target, headers, derived)?;
            resolve_expr(context, index, headers, derived)?;
        }
        Expr::Slice { target, start, end } => {
            resolve_expr(context, target, headers, derived)?;
            if let Some(start) = start {
                resolve_expr(context, start, headers, derived)?;
            }
            if let Some(end) = end {
                resolve_expr(context, end, headers, derived)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_in_any(
    context: &str,
    values: Vec<Expr>,
    accepted: &Expr,
    headers: &[String],
    derived: &[String],
) -> Result<Expr, String> {
    if values.is_empty() {
        return Err(format!("{context}: in_any values list must not be empty"));
    }

    let accepted_values = literal_string_list(context, "accepted values", accepted)?;
    let accepted_list = Expr::List(accepted_values.into_iter().map(Expr::Str).collect());

    let mut result: Option<Expr> = None;
    for mut value in values {
        resolve_expr(context, &mut value, headers, derived)?;
        let test = Expr::Binary {
            op: BinOp::In,
            lhs: Box::new(value),
            rhs: Box::new(accepted_list.clone()),
        };
        result = Some(match result {
            None => test,
            Some(previous) => Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(previous),
                rhs: Box::new(test),
            },
        });
    }

    Ok(result.expect("values is non-empty"))
}

fn literal_string(context: &str, what: &str, expr: &Expr) -> Result<String, String> {
    match expr {
        Expr::Str(value) => Ok(value.clone()),
        _ => Err(format!("{context}: in_any {what} must be a string literal")),
    }
}

fn literal_string_list(context: &str, what: &str, expr: &Expr) -> Result<Vec<String>, String> {
    match expr {
        Expr::List(items) => items
            .iter()
            .map(|item| literal_string(context, what, item))
            .collect(),
        _ => Err(format!("{context}: in_any {what} must be a list of strings")),
    }
}

/// Compile a standalone moonblade filter expression (used by the GUI's
/// "moonblade" search mode) against the input headers.
///
/// Identifiers resolve to input columns like in a `derive` expression, and the
/// xan `col("name")` / `col(index)` forms name columns that are not valid
/// identifiers (names with spaces) or duplicate headers (`col("text", 1)`).
/// Unknown columns are reported as an error so a typo cannot silently match
/// every row.
#[allow(dead_code)] // used by the GUI (`gui` feature)
pub fn compile_filter(source: &str, headers: &[String]) -> Result<Expr, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("empty expression".to_string());
    }
    let mut expression = crate::expr::parse_filter(trimmed)?;
    resolve_expr("filter", &mut expression, headers, &[])?;
    Ok(expression)
}


fn resolve_col_call(args: &[Expr], headers: &[String]) -> Result<Expr, String> {
    let index = match args {
        [Expr::Str(name)] => headers.iter().position(|header| header == name),
        [Expr::Str(name), Expr::Int(occurrence)] => headers
            .iter()
            .enumerate()
            .filter(|(_, header)| *header == name)
            .nth((*occurrence).max(0) as usize)
            .map(|(index, _)| index),
        [Expr::Int(index)] => resolve_index(*index, headers.len()),
        [Expr::Unary {
            op: crate::expr::UnOp::Neg,
            operand,
        }] => match operand.as_ref() {
            Expr::Int(index) => resolve_index(-index, headers.len()),
            _ => None,
        },
        _ => {
            return Err(
                "col() expects a column name, an index, or (name, occurrence)".to_string(),
            )
        }
    };
    match index {
        Some(index) => Ok(Expr::Ref(ValueRef::Column(index))),
        None => Err("col() references a column that does not exist".to_string()),
    }
}

fn resolve_index(index: i64, len: usize) -> Option<usize> {
    if index < 0 {
        let from_end = len as i64 + index;
        (from_end >= 0)
            .then_some(from_end as usize)
            .filter(|position| *position < len)
    } else {
        let position = index as usize;
        (position < len).then_some(position)
    }
}

pub fn compile(program: Program, headers: &[String]) -> Result<Plan, String> {
    let mut rules = Vec::with_capacity(program.rules.len());

    for def in program.rules {
        let context = format!("rule '{}'", def.name);

        // Resolve `derive` clauses in order; each may reference the names
        // produced by an earlier clause as well as input columns.
        let mut derived_names: Vec<String> = Vec::new();
        let mut derive = Vec::with_capacity(def.derive.len());
        for derived in def.derive {
            let mut expr = derived.expr;
            resolve_expr(&context, &mut expr, headers, &derived_names)?;
            derived_names.extend(derived.names.iter().cloned());
            derive.push(CompiledDerived {
                names: derived.names,
                expr,
            });
        }

        // A `check` rule is a single boolean expression. It is compiled into
        // two anonymous derived values (the result and the literal `true`), so
        // the normal comparison pipeline reports pass/fail without a separate
        // execution path.
        let check_source = def.check.clone();
        let check = match check_source.as_deref() {
            Some(source) => {
                let mut expr = crate::expr::parse_filter(source).map_err(|e| {
                    format!("{context}: invalid `check` expression: {e}")
                })?;
                resolve_expr(&context, &mut expr, headers, &derived_names)?;
                Some(expr)
            }
            None => None,
        };

        let (left, right) = if let Some(expr) = check {
            let result_index = derived_names.len();
            let result_name = format!("\u{0}{result_index}");
            derived_names.push(result_name.clone());
            derive.push(CompiledDerived {
                names: vec![result_name],
                expr,
            });

            let expected_index = derived_names.len();
            let expected_name = format!("\u{0}{expected_index}");
            derived_names.push(expected_name.clone());
            derive.push(CompiledDerived {
                names: vec![expected_name],
                expr: Expr::Str("true".to_string()),
            });

            (
                ColumnRef::Columns(vec![ValueRef::Derived(result_index)]),
                ColumnRef::Columns(vec![ValueRef::Derived(expected_index)]),
            )
        } else {
            let left = compile_columns(
                &context,
                "left",
                &def.left,
                headers,
                &mut derived_names,
                Some(&mut derive),
            )?;
            let right = compile_columns(
                &context,
                "right",
                &def.right,
                headers,
                &mut derived_names,
                Some(&mut derive),
            )?;
            (left, right)
        };
        let skip = def
            .skip
            .as_ref()
            .map(|predicate| compile_predicate(&context, predicate, headers))
            .transpose()?;

        let multi = def.multi.unwrap_or(program.defaults.multi);
        let separator = def
            .separator
            .clone()
            .unwrap_or_else(|| program.defaults.separator.clone());
        let report_limit = def.report_limit.unwrap_or(program.defaults.report_limit);
        let join_separator = def
            .join_separator
            .clone()
            .unwrap_or_else(|| program.defaults.join_separator.clone());
        let trim = def.trim.unwrap_or(program.defaults.trim);
        let allow_empty = def.allow_empty.unwrap_or(program.defaults.allow_empty);

        // `pattern` implies a regex comparison unless stated otherwise. A
        // check rule always compares its boolean result with `true`.
        let compare = if check_source.is_some() {
            CompareOp::Eq
        } else {
            match def.compare {
                Some(compare) => compare,
                None if def.pattern.is_some() => CompareOp::Matches,
                None => program.defaults.compare,
            }
        };
        if compare.is_regex() && def.pattern.is_none() {
            return Err(format!(
                "rule '{}': compare = {} requires a `pattern`",
                def.name,
                compare.as_str()
            ));
        }
        if def.pattern.is_some() && def.right.is_empty() && !compare.is_regex() {
            return Err(format!(
                "rule '{}': `pattern` with a non-regex comparison needs a `right` column",
                def.name
            ));
        }

        let mapping = match &def.mapping {
            MappingSourceDef::None => MappingPlan::None,
            MappingSourceDef::Auto => MappingPlan::Auto,
            MappingSourceDef::Files(sources) => MappingPlan::Files(
                sources
                    .iter()
                    .map(|source| {
                        let key_spec = source.key.clone().unwrap_or_else(|| def.left.clone());
                        let key = compile_columns(
                            &context,
                            "mapping key",
                            &key_spec,
                            headers,
                            &mut derived_names,
                            Some(&mut derive),
                        )?;
                        Ok(MappingSourcePlan {
                            files: source.files.clone(),
                            left: source.left.clone(),
                            right: source.right.clone(),
                            key,
                            multi: source.multi,
                            separator: source
                                .separator
                                .clone()
                                .unwrap_or_else(|| program.defaults.mapping_separator.clone()),
                            // The reference filter is compiled later, against
                            // each file's own header row.
                            filter: source.filter.clone().or_else(|| def.mapping_filter.clone()),
                            when: source.when,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
            ),
        };

        // `mapping_filter` filters the rows that define a mapping. For `auto`
        // mappings that is resolved here; for file mappings it travels with
        // the plan and is resolved per reference file.
        let auto_mapping_filter = match &def.mapping {
            MappingSourceDef::Auto => def
                .mapping_filter
                .as_ref()
                .map(|predicate| compile_predicate(&context, predicate, headers))
                .transpose()?,
            MappingSourceDef::None => {
                if def.mapping_filter.is_some() {
                    return Err(format!(
                        "{context}: `mapping_filter` requires `mapping = auto` or `mapping_files`"
                    ));
                }
                None
            }
            MappingSourceDef::Files(_) => None,
        };

        let left_name = if let Some(source) = &check_source {
            source.clone()
        } else if !def.left.is_empty() {
            def.left.display()
        } else if let MappingSourceDef::Files(sources) = &def.mapping {
            sources
                .first()
                .map(|source| source.key.as_ref().map(ColumnSpec::display).unwrap_or_default())
                .unwrap_or_default()
        } else {
            def.left.display()
        };
        let right_name = if check_source.is_some() {
            "true".to_string()
        } else if def.right.is_empty() {
            "(pattern)".to_string()
        } else {
            def.right.display()
        };

        rules.push(CompiledRule {
            name: def.name,
            left_name,
            right_name,
            left,
            right,
            transform_left: def.transform_left,
            transform_right: def.transform_right,
            derive,
            compare,
            multi,
            separator,
            join_separator,
            pattern: def.pattern,
            trim,
            allow_empty,
            skip,
            skip_source: def.skip_source,
            auto_mapping_filter,
            mapping,
            report_limit,
        });
    }

    Ok(Plan {
        rules,
        left_label: program.defaults.left_label,
        right_label: program.defaults.right_label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{eval, EvalContext};

    fn headers() -> Vec<String> {
        vec![
            "name".to_string(),
            "count".to_string(),
            "Full Name".to_string(),
        ]
    }

    fn matches(expression: &Expr, row: &[&str]) -> bool {
        let cells: Vec<String> = row.iter().map(|value| value.to_string()).collect();
        eval(expression, &EvalContext { cells: &cells, derived: &[] }).is_truthy()
    }

    #[test]
    fn compile_filter_resolves_columns_and_col_calls() {
        let headers = headers();
        let expression =
            compile_filter("count > 10 and lower(name) eq \"alice\"", &headers).unwrap();
        assert!(matches(&expression, &["Alice", "15", "x"]));
        assert!(!matches(&expression, &["Bob", "15", "x"]));
        assert!(!matches(&expression, &["Alice", "5", "x"]));

        // `col("...")` reaches names that are not valid identifiers.
        let expression = compile_filter("col(\"Full Name\") eq \"A\"", &headers).unwrap();
        assert!(matches(&expression, &["n", "1", "A"]));
        assert!(!matches(&expression, &["n", "1", "B"]));
    }

    #[test]
    fn compile_filter_reports_errors() {
        let headers = headers();
        assert!(compile_filter("nope > 1", &headers).is_err());
        assert!(compile_filter("", &headers).is_err());
        assert!(compile_filter("1 +", &headers).is_err());
    }
}
