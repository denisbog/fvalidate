//! The validation rule DSL.
//!
//! Grammar (line oriented, `#` starts a comment outside quotes):
//!
//! ```text
//! defaults {
//!   separator  = ","
//!   multi      = false
//!   compare    = eq
//!   report_limit = 10
//! }
//!
//! rule "start and end dates agree" {
//!   left            = start_date
//!   right           = end_date
//!   transform_left  = date(["%Y-%m-%d", "%d/%m/%Y"], "%Y-%m-%d")
//!   transform_right = date("%Y-%m-%d", "%Y-%m-%d")
//!   compare         = eq
//!   mapping         = none
//! }
//!
//! rule "country name -> code" {
//!   left         = country_name
//!   right        = country_code
//!   transform_left = trim | lower
//!   mapping      = auto           # extract the mapping from the data
//! }
//!
//! rule "labels agree with reference" {
//!   left          = labels
//!   right         = ref_labels
//!   multi         = true
//!   separator     = ";"
//!   mapping_files = ["ref1.csv", "ref2.csv"]
//!   mapping_left  = label
//!   mapping_right = id
//! }
//! ```
//!
//! Values are either bare tokens, quoted strings, lists `[...]`, calls
//! `name(args...)` or pipelines `a | b | c`.

use std::path::{Path, PathBuf};

use regex::Regex;

use crate::compare::CompareOp;
use crate::expr::{self, DerivedDef};
use crate::pattern::Separator;
use crate::transform::Transform;

/// A parsed DSL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    List(Vec<Value>),
    Call(String, Vec<Value>),
    Pipeline(Vec<Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        let s = self.as_str()?;
        match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Some(true),
            "false" | "no" | "0" | "off" => Some(false),
            _ => None,
        }
    }

    pub fn as_usize(&self) -> Option<usize> {
        self.as_str()?.trim().parse().ok()
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

/// How one side of a rule selects its value from a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnSpec {
    /// One column, or several columns combined into a composite key.
    Columns(Vec<String>),
    /// First non-empty column wins: `or(a, b, c)`.
    Or(Vec<String>),
}

impl ColumnSpec {
    pub fn names(&self) -> &[String] {
        match self {
            ColumnSpec::Columns(names) | ColumnSpec::Or(names) => names,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.names().is_empty()
    }

    pub fn display(&self) -> String {
        match self {
            ColumnSpec::Columns(names) => names.join(" + "),
            ColumnSpec::Or(names) => format!("or({})", names.join(", ")),
        }
    }
}

/// A row-level boolean condition used by `validation_skipped` and
/// `mapping_filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    /// `in(col, ["a", "b"])`: the value is one of the listed strings.
    In {
        column: ColumnSpec,
        values: Vec<String>,
    },
    /// `any_in([a, b], [...])`: at least one column is one of the values.
    AnyIn {
        columns: Vec<ColumnSpec>,
        values: Vec<String>,
    },
    /// `all_in([a, b], [...])`: every column is one of the values.
    AllIn {
        columns: Vec<ColumnSpec>,
        values: Vec<String>,
    },
    /// `eq(col, "value")`.
    Eq {
        column: ColumnSpec,
        value: String,
    },
    /// `ne(col, "value")`.
    Ne {
        column: ColumnSpec,
        value: String,
    },
    /// `empty(col)`.
    Empty {
        column: ColumnSpec,
    },
    /// `not_empty(col)`.
    NotEmpty {
        column: ColumnSpec,
    },
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
    /// Literal `true` / `false`.
    Const(bool),
}

#[derive(Debug, Clone)]
pub enum MappingSourceDef {
    /// No mapping: left is compared as-is against right.
    None,
    /// Extract the left -> right mapping from the data itself.
    Auto,
    /// One or more ordered reference sources. Lookups are resolved against
    /// them in order, so a later source can fill in values the earlier ones
    /// could not resolve (see [`FallbackMode`]).
    Files(Vec<MappingSource>),
}

/// One `mapping { ... }` block: a set of reference files plus the columns
/// used to key them. Each source may key on different input columns, which is
/// what allows a fallback lookup from another file.
#[derive(Debug, Clone, Default)]
pub struct MappingSource {
    pub files: Vec<PathBuf>,
    /// Reference-file column(s) forming the lookup key.
    pub left: Vec<String>,
    /// Reference-file column(s) forming the target value.
    pub right: Vec<String>,
    /// Data-row key: a column, a composite `[a, b]` or a fallback
    /// `or(a, b, c)`. Defaults to `left` (same names) for a `mapping { ... }`
    /// block, or to the rule's `left` for the legacy `mapping_files` keys.
    pub key: Option<ColumnSpec>,
    /// Set for sources built from the legacy `mapping_files`/`mapping_left`
    /// keys, where an omitted `key` falls back to the rule's `left` columns.
    pub key_from_rule: bool,
    pub multi: bool,
    pub separator: Option<Separator>,
    /// Optional predicate over reference rows; columns resolve against each
    /// file's own header.
    pub filter: Option<Predicate>,
    /// When to consult the next source instead of using this one.
    pub when: FallbackMode,
}

/// Controls when the next mapping source is tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FallbackMode {
    /// Use the next source only when this one has no entry for the key.
    #[default]
    Unmapped,
    /// Also use the next source when this one is ambiguous.
    Ambiguous,
    /// Always use the next source (a later source overrides an earlier one).
    Always,
}

#[derive(Debug, Clone)]
pub struct RuleDefaults {
    pub separator: Separator,
    pub multi: bool,
    pub compare: CompareOp,
    pub report_limit: usize,
    pub mapping_separator: Separator,
    /// Joins several columns into a single composite key.
    pub join_separator: String,
    /// Trim every extracted cell (and mapping-file cell) before transforming.
    pub trim: bool,
    /// When true, a row whose source and target are both empty is skipped
    /// instead of being reported as a failure (the relation is optional).
    pub allow_empty: bool,
}

impl Default for RuleDefaults {
    fn default() -> Self {
        RuleDefaults {
            separator: Separator::literal(","),
            multi: false,
            compare: CompareOp::Eq,
            report_limit: 10,
            mapping_separator: Separator::literal(","),
            join_separator: "|".to_string(),
            trim: false,
            allow_empty: false,
        }
    }
}

/// A rule as written in the DSL, before column indices are resolved.
#[derive(Debug, Clone)]
pub struct RuleDef {
    pub name: String,
    /// One column, a composite key (`[a, b]`) or a fallback (`or(a, b, c)`).
    pub left: ColumnSpec,
    pub right: ColumnSpec,
    pub transform_left: Vec<Transform>,
    pub transform_right: Vec<Transform>,
    /// Xan/moonblade-style expressions computing named values per row.
    pub derive: Vec<DerivedDef>,
    pub compare: Option<CompareOp>,
    pub multi: Option<bool>,
    pub separator: Option<Separator>,
    pub join_separator: Option<String>,
    /// Rule-level regex used by `compare = matches | not_matches`.
    pub pattern: Option<Regex>,
    pub trim: Option<bool>,
    pub allow_empty: Option<bool>,
    /// When this predicate holds, the row is counted as skipped instead of
    /// being validated.
    pub skip: Option<Predicate>,
    /// Selects which rows define a mapping: data rows for `mapping = auto`,
    /// reference rows for `mapping_files`. It never skips validation.
    pub mapping_filter: Option<Predicate>,
    pub mapping: MappingSourceDef,
    pub report_limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub defaults: RuleDefaults,
    pub rules: Vec<RuleDef>,
}

/// Parse a DSL file.
pub fn load_file(path: &Path) -> Result<Program, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read rules file {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Parse DSL source text.
pub fn parse(text: &str) -> Result<Program, String> {
    let mut defaults = RuleDefaults::default();
    let mut rules: Vec<RuleDef> = Vec::new();

    #[derive(Clone, Copy, PartialEq)]
    enum Module {
        Defaults,
        Rule,
    }

    let mut module: Option<Module> = None;
    let mut current_rule: Option<RuleDef> = None;
    // Set while inside a `mapping { ... }` sub-block of a rule.
    let mut current_mapping: Option<MappingSource> = None;
    let mut accumulator = String::new();
    let mut line_no = 0usize;
    // Carried across lines so a `#` inside a multi-line quoted value is not
    // mistaken for a comment.
    let mut quote_state: Option<char> = None;

    for raw in text.lines() {
        line_no += 1;
        let line = strip_comment_state(raw, &mut quote_state);
        if accumulator.is_empty() && line.trim().is_empty() {
            continue;
        }

        // Join continuation lines until quotes/brackets balance.
        if !accumulator.is_empty() {
            accumulator.push('\n');
        }
        accumulator.push_str(&line);

        if !is_balanced(&accumulator) {
            continue;
        }

        let statement = accumulator.trim().to_string();
        accumulator.clear();
        if statement.is_empty() {
            continue;
        }

        if let Some(header) = statement.strip_suffix('{') {
            let header = header.trim();
            if header == "defaults" {
                module = Some(Module::Defaults);
            } else if let Some(name) = header.strip_prefix("rule") {
                let name = parse_rule_name(name.trim())
                    .ok_or_else(|| format!("line {line_no}: invalid rule name"))?;
                module = Some(Module::Rule);
                current_rule = Some(RuleDef {
                    name,
                    left: ColumnSpec::Columns(Vec::new()),
                    right: ColumnSpec::Columns(Vec::new()),
                    transform_left: Vec::new(),
                    transform_right: Vec::new(),
                    derive: Vec::new(),
                    compare: None,
                    multi: None,
                    separator: None,
                    join_separator: None,
                    pattern: None,
                    trim: None,
                    allow_empty: None,
                    skip: None,
                    mapping_filter: None,
                    mapping: MappingSourceDef::None,
                    report_limit: None,
                });
            } else if header == "mapping" {
                if module != Some(Module::Rule) {
                    return Err(format!("line {line_no}: unexpected block '{header}'"));
                }
                if current_mapping.is_some() {
                    return Err(format!("line {line_no}: nested `mapping` block"));
                }
                current_mapping = Some(MappingSource::default());
            } else {
                return Err(format!("line {line_no}: unexpected block '{header}'"));
            }
            continue;
        }

        if statement == "}" {
            // A `}` closes the innermost `mapping { ... }` block when one is
            // open, otherwise it closes the current `rule`/`defaults` block.
            if let Some(source) = current_mapping.take() {
                match current_rule.as_mut() {
                    Some(rule) => push_mapping_source(&mut rule.mapping, source),
                    None => {
                        return Err(format!("line {line_no}: mapping block outside a rule"));
                    }
                }
                continue;
            }
            if let Some(rule) = current_rule.take() {
                rules.push(validate_rule(rule, line_no)?);
            }
            module = None;
            continue;
        }

        let (key, value) = statement
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected `key = value`"))?;
        let key = key.trim();
        let value = parse_value(value.trim()).map_err(|e| format!("line {line_no}: {e}"))?;

        if let Some(source) = current_mapping.as_mut() {
            apply_mapping_key(source, key, &value, line_no)?;
        } else {
            match module {
                Some(Module::Defaults) => apply_default(&mut defaults, key, &value, line_no)?,
                Some(Module::Rule) => {
                    let rule = current_rule
                        .as_mut()
                        .ok_or_else(|| format!("line {line_no}: assignment outside a rule"))?;
                    apply_rule_key(rule, key, &value, line_no)?;
                }
                None => {
                    return Err(format!(
                        "line {line_no}: assignment `{key}` outside of a block"
                    ))
                }
            }
        }
    }

    if !accumulator.trim().is_empty() {
        return Err("unterminated statement at end of file".to_string());
    }
    if let Some(rule) = current_rule.take() {
        rules.push(validate_rule(rule, line_no)?);
    }
    if rules.is_empty() {
        return Err("no rules were defined".to_string());
    }

    Ok(Program { defaults, rules })
}

fn validate_rule(mut rule: RuleDef, line_no: usize) -> Result<RuleDef, String> {
    let _ = line_no;

    // Finalize each mapping source: fill in the missing side of the
    // reference/data key pair so a block can name only the columns that differ.
    if let MappingSourceDef::Files(sources) = &mut rule.mapping {
        for source in sources.iter_mut() {
            if source.left.is_empty() && source.key.is_none() {
                return Err(format!(
                    "rule '{}': a mapping source needs `left` (reference columns) or `key` (data columns)",
                    rule.name
                ));
            }
            if source.right.is_empty() {
                return Err(format!(
                    "rule '{}': a mapping source needs `right` (reference target columns)",
                    rule.name
                ));
            }
            if source.key.is_none() {
                // A `mapping { ... }` block keys the data by the same column
                // names as the reference; the legacy `mapping_files` keys key
                // by the rule's `left` columns (preserving `or(...)`).
                source.key = if source.key_from_rule && !rule.left.is_empty() {
                    Some(rule.left.clone())
                } else {
                    Some(ColumnSpec::Columns(source.left.clone()))
                };
            }
            if source.left.is_empty() {
                match &source.key {
                    Some(ColumnSpec::Columns(names)) => source.left = names.clone(),
                    _ => {
                        return Err(format!(
                            "rule '{}': a mapping source needs `left` (reference columns)",
                            rule.name
                        ));
                    }
                }
            }
        }
    }

    if rule.left.is_empty() {
        match &rule.mapping {
            MappingSourceDef::Files(sources) if !sources.is_empty() => {
                rule.left = sources[0]
                    .key
                    .clone()
                    .expect("mapping source key is finalized above");
            }
            _ => {
                return Err(format!("rule '{}': `left` column is required", rule.name));
            }
        }
    }
    if rule.right.is_empty() && rule.pattern.is_none() {
        return Err(format!(
            "rule '{}': both `left` and `right` columns are required (or set `pattern`)",
            rule.name
        ));
    }
    Ok(rule)
}

/// Append a `mapping { ... }` block to a rule's mapping, creating the ordered
/// source list on first use.
fn push_mapping_source(mapping: &mut MappingSourceDef, source: MappingSource) {
    match mapping {
        MappingSourceDef::Files(sources) => sources.push(source),
        _ => *mapping = MappingSourceDef::Files(vec![source]),
    }
}

fn apply_default(
    defaults: &mut RuleDefaults,
    key: &str,
    value: &Value,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "separator" => defaults.separator = parse_separator(value, key, line_no)?,
        "multi" => defaults.multi = expect_bool(value, key, line_no)?,
        "compare" => {
            defaults.compare = CompareOp::parse(expect_string_ref(value, key, line_no)?)
                .ok_or_else(|| format!("line {line_no}: unknown comparison operator"))?
        }
        "report_limit" => defaults.report_limit = expect_usize(value, key, line_no)?,
        "mapping_separator" => defaults.mapping_separator = parse_separator(value, key, line_no)?,
        "join_separator" => defaults.join_separator = expect_string(value, key, line_no)?,
        "trim" => defaults.trim = expect_bool(value, key, line_no)?,
        "allow_empty" | "optional" => defaults.allow_empty = expect_bool(value, key, line_no)?,
        other => return Err(format!("line {line_no}: unknown defaults key `{other}`")),
    }
    Ok(())
}

fn apply_rule_key(
    rule: &mut RuleDef,
    key: &str,
    value: &Value,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "left" => rule.left = expect_columns(value, key, line_no)?,
        "right" => rule.right = expect_columns(value, key, line_no)?,
        "transform_left" => rule.transform_left = parse_transforms(value, line_no)?,
        "transform_right" => rule.transform_right = parse_transforms(value, line_no)?,
        "derive" => {
            let source = expect_string_ref(value, key, line_no)?;
            rule.derive = expr::parse_derive(source)
                .map_err(|e| format!("line {line_no}: invalid derive expression: {e}"))?;
        }
        "compare" => {
            rule.compare = Some(
                CompareOp::parse(expect_string_ref(value, key, line_no)?)
                    .ok_or_else(|| format!("line {line_no}: unknown comparison operator"))?,
            )
        }
        "multi" => rule.multi = Some(expect_bool(value, key, line_no)?),
        "separator" => rule.separator = Some(parse_separator(value, key, line_no)?),
        "join_separator" => rule.join_separator = Some(expect_string(value, key, line_no)?),
        "pattern" => rule.pattern = Some(compile_pattern(value, key, line_no)?),
        "trim" => rule.trim = Some(expect_bool(value, key, line_no)?),
        "allow_empty" | "optional" => rule.allow_empty = Some(expect_bool(value, key, line_no)?),
        "validation_skipped" | "skip_when" | "skip" => {
            rule.skip = Some(parse_predicate(value, key, line_no)?)
        }
        "mapping_filter" => rule.mapping_filter = Some(parse_predicate(value, key, line_no)?),
        "report_limit" => rule.report_limit = Some(expect_usize(value, key, line_no)?),
        "mapping" => {
            let raw = expect_string_ref(value, key, line_no)?;
            rule.mapping = match raw.to_ascii_lowercase().as_str() {
                "none" | "off" | "" => MappingSourceDef::None,
                "auto" | "extract" => MappingSourceDef::Auto,
                other => {
                    return Err(format!(
                        "line {line_no}: `mapping` must be `auto` or `none`, got `{other}`"
                    ))
                }
            };
        }
        "mapping_files" => {
            let files = expect_string_list(value, key, line_no)?
                .into_iter()
                .map(PathBuf::from)
                .collect();
            // Legacy keys build (or update) a single source whose data-side
            // key defaults to the rule's `left` columns.
            let mut source = match &rule.mapping {
                MappingSourceDef::Files(sources) if sources.len() == 1 => sources[0].clone(),
                _ => MappingSource::default(),
            };
            source.files = files;
            source.key_from_rule = true;
            rule.mapping = MappingSourceDef::Files(vec![source]);
        }
        "mapping_left" | "mapping_right" | "mapping_multi" | "mapping_separator"
        | "mapping_key" => {
            if !matches!(rule.mapping, MappingSourceDef::Files(_)) {
                rule.mapping = MappingSourceDef::Files(vec![MappingSource::default()]);
            }
            if let MappingSourceDef::Files(sources) = &mut rule.mapping {
                if sources.is_empty() {
                    sources.push(MappingSource::default());
                }
                let source = &mut sources[0];
                source.key_from_rule = true;
                match key {
                    "mapping_left" => source.left = expect_string_or_list(value, key, line_no)?,
                    "mapping_right" => source.right = expect_string_or_list(value, key, line_no)?,
                    "mapping_multi" => source.multi = expect_bool(value, key, line_no)?,
                    "mapping_separator" => {
                        source.separator = Some(parse_separator(value, key, line_no)?)
                    }
                    "mapping_key" => source.key = Some(expect_columns(value, key, line_no)?),
                    _ => unreachable!(),
                }
            }
        }
        other => return Err(format!("line {line_no}: unknown rule key `{other}`")),
    }
    Ok(())
}

/// Apply one `key = value` assignment inside a `mapping { ... }` block.
fn apply_mapping_key(
    source: &mut MappingSource,
    key: &str,
    value: &Value,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "files" | "file" => {
            source.files = expect_string_list(value, key, line_no)?
                .into_iter()
                .map(PathBuf::from)
                .collect();
        }
        "left" | "mapping_left" => source.left = expect_string_or_list(value, key, line_no)?,
        "right" | "mapping_right" => source.right = expect_string_or_list(value, key, line_no)?,
        "key" | "data" | "columns" | "mapping_key" => {
            source.key = Some(expect_columns(value, key, line_no)?)
        }
        "multi" | "mapping_multi" => source.multi = expect_bool(value, key, line_no)?,
        "separator" | "mapping_separator" => {
            source.separator = Some(parse_separator(value, key, line_no)?)
        }
        "filter" | "mapping_filter" => source.filter = Some(parse_predicate(value, key, line_no)?),
        "when" | "on" | "fallback" => source.when = parse_fallback_mode(value, line_no)?,
        other => return Err(format!("line {line_no}: unknown mapping key `{other}`")),
    }
    Ok(())
}

fn parse_fallback_mode(value: &Value, line_no: usize) -> Result<FallbackMode, String> {
    let raw = expect_string_ref(value, "when", line_no)?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "unmapped" | "missing" | "not_found" | "" => Ok(FallbackMode::Unmapped),
        "ambiguous" => Ok(FallbackMode::Ambiguous),
        "always" | "override" => Ok(FallbackMode::Always),
        other => Err(format!(
            "line {line_no}: `when` must be `unmapped`, `ambiguous` or `always`, got `{other}`"
        )),
    }
}

fn expect_string(value: &Value, key: &str, line_no: usize) -> Result<String, String> {
    expect_string_ref(value, key, line_no).map(str::to_string)
}

/// Extract the pattern from a `regex("...")` call, if that is what the value
/// is. Returns `None` for plain strings, which callers may treat as literals.
fn regex_source(value: &Value) -> Option<&str> {
    match value {
        Value::Call(name, args) if name.trim().eq_ignore_ascii_case("regex") => {
            args.first().and_then(Value::as_str)
        }
        _ => None,
    }
}

/// Resolve a value to a regex pattern: either `regex("...")` or a plain
/// string (also treated as a pattern, xan-style).
fn regex_pattern(value: &Value, key: &str, line_no: usize) -> Result<String, String> {
    if let Some(source) = regex_source(value) {
        Ok(source.to_string())
    } else {
        expect_string(value, key, line_no)
    }
}

fn compile_regex(source: &str, line_no: usize) -> Result<Regex, String> {
    Regex::new(source).map_err(|e| format!("line {line_no}: invalid regex `{source}`: {e}"))
}

fn compile_pattern(value: &Value, key: &str, line_no: usize) -> Result<Regex, String> {
    let source = regex_pattern(value, key, line_no)?;
    compile_regex(&source, line_no)
}

/// A separator is a plain string, or a regex when wrapped in `regex("...")`.
fn parse_separator(value: &Value, key: &str, line_no: usize) -> Result<Separator, String> {
    match regex_source(value) {
        Some(source) => Ok(Separator::Regex(compile_regex(source, line_no)?)),
        None => Ok(Separator::Literal(expect_string(value, key, line_no)?)),
    }
}

/// Accept either a single string or a list of strings. Used for `left`,
/// `right`, `mapping_left` and `mapping_right`, which can name one or several
/// columns (composite keys).
fn expect_string_or_list(value: &Value, key: &str, line_no: usize) -> Result<Vec<String>, String> {
    match value {
        Value::List(items) => {
            if items.is_empty() {
                return Err(format!("line {line_no}: `{key}` list must not be empty"));
            }
            items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        format!("line {line_no}: `{key}` list must contain column names")
                    })
                })
                .collect()
        }
        other => other
            .as_str()
            .map(|s| vec![s.to_string()])
            .ok_or_else(|| format!("line {line_no}: `{key}` expects a column name or a list")),
    }
}

/// Parse a `left`/`right` value: a column name, a composite list `[a, b]`, or
/// a fallback `or(a, b, c)` (aliases: `coalesce`, `first`).
fn expect_columns(value: &Value, key: &str, line_no: usize) -> Result<ColumnSpec, String> {
    match value {
        Value::List(items) => {
            let names = parse_column_names(items, key, line_no)?;
            if names.is_empty() {
                return Err(format!("line {line_no}: `{key}` list must not be empty"));
            }
            Ok(ColumnSpec::Columns(names))
        }
        Value::Call(name, args) if is_or_name(name) => {
            let mut names = Vec::new();
            for arg in args {
                match arg {
                    Value::List(items) => names.extend(parse_column_names(items, key, line_no)?),
                    other => names.push(
                        other
                            .as_str()
                            .ok_or_else(|| {
                                format!("line {line_no}: `{key}` or(...) expects column names")
                            })?
                            .to_string(),
                    ),
                }
            }
            if names.is_empty() {
                return Err(format!(
                    "line {line_no}: `{key}` or(...) needs at least one column"
                ));
            }
            Ok(ColumnSpec::Or(names))
        }
        other => other
            .as_str()
            .map(|s| ColumnSpec::Columns(vec![s.to_string()]))
            .ok_or_else(|| {
                format!("line {line_no}: `{key}` expects a column name, a list or or(...)")
            }),
    }
}

fn is_or_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "or" | "coalesce" | "first" | "first_non_empty"
    )
}

fn parse_column_names(items: &[Value], key: &str, line_no: usize) -> Result<Vec<String>, String> {
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("line {line_no}: `{key}` list must contain column names"))
        })
        .collect()
}

/// Parse a boolean row predicate (`validation_skipped`, `mapping_filter`).
fn parse_predicate(value: &Value, key: &str, line_no: usize) -> Result<Predicate, String> {
    let invalid = || format!("line {line_no}: `{key}` expects a predicate");
    match value {
        Value::Call(name, args) => {
            let lower = name.trim().to_ascii_lowercase();
            match lower.as_str() {
                "in" | "one_of" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::In {
                        column: expect_columns(&args[0], key, line_no)?,
                        values: expect_value_list(&args[1], key, line_no)?,
                    })
                }
                "not_in" | "nin" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::Not(Box::new(Predicate::In {
                        column: expect_columns(&args[0], key, line_no)?,
                        values: expect_value_list(&args[1], key, line_no)?,
                    })))
                }
                "any_in" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::AnyIn {
                        columns: expect_column_group(&args[0], key, line_no)?,
                        values: expect_value_list(&args[1], key, line_no)?,
                    })
                }
                "all_in" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::AllIn {
                        columns: expect_column_group(&args[0], key, line_no)?,
                        values: expect_value_list(&args[1], key, line_no)?,
                    })
                }
                "eq" | "equals" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::Eq {
                        column: expect_columns(&args[0], key, line_no)?,
                        value: expect_string(&args[1], key, line_no)?,
                    })
                }
                "ne" | "neq" | "not_eq" | "not_equals" => {
                    expect_arity(args, 2, name, line_no)?;
                    Ok(Predicate::Ne {
                        column: expect_columns(&args[0], key, line_no)?,
                        value: expect_string(&args[1], key, line_no)?,
                    })
                }
                "empty" | "is_empty" => {
                    expect_arity(args, 1, name, line_no)?;
                    Ok(Predicate::Empty {
                        column: expect_columns(&args[0], key, line_no)?,
                    })
                }
                "not_empty" | "notempty" | "non_empty" | "is_not_empty" => {
                    expect_arity(args, 1, name, line_no)?;
                    Ok(Predicate::NotEmpty {
                        column: expect_columns(&args[0], key, line_no)?,
                    })
                }
                "and" | "all" => {
                    let parts = flatten_predicates(args, key, line_no)?;
                    Ok(Predicate::And(parts))
                }
                "or" | "any" => {
                    let parts = flatten_predicates(args, key, line_no)?;
                    Ok(Predicate::Or(parts))
                }
                "not" => {
                    expect_arity(args, 1, name, line_no)?;
                    Ok(Predicate::Not(Box::new(parse_predicate(
                        &args[0], key, line_no,
                    )?)))
                }
                other => Err(format!(
                    "line {line_no}: unknown predicate `{other}` (try in, any_in, all_in, eq, ne, empty, not_empty, and, or, not)"
                )),
            }
        }
        Value::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "always" | "yes" => Ok(Predicate::Const(true)),
            "false" | "never" | "no" => Ok(Predicate::Const(false)),
            other => Err(format!("line {line_no}: unknown predicate `{other}`")),
        },
        _ => Err(invalid()),
    }
}

fn flatten_predicates(args: &[Value], key: &str, line_no: usize) -> Result<Vec<Predicate>, String> {
    if args.is_empty() {
        return Err(format!(
            "line {line_no}: `{key}` needs at least one predicate"
        ));
    }
    let mut out = Vec::new();
    for arg in args {
        match arg {
            Value::List(items) => {
                for item in items {
                    out.push(parse_predicate(item, key, line_no)?);
                }
            }
            other => out.push(parse_predicate(other, key, line_no)?),
        }
    }
    Ok(out)
}

/// A list of columns or a single column, as used by `any_in` / `all_in`.
fn expect_column_group(
    value: &Value,
    key: &str,
    line_no: usize,
) -> Result<Vec<ColumnSpec>, String> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| ColumnSpec::Columns(vec![s.to_string()]))
                    .ok_or_else(|| {
                        format!("line {line_no}: `{key}` list must contain column names")
                    })
            })
            .collect(),
        other => Ok(vec![expect_columns(other, key, line_no)?]),
    }
}

/// A list of literal strings, or a single string treated as a one-element list.
fn expect_value_list(value: &Value, key: &str, line_no: usize) -> Result<Vec<String>, String> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("line {line_no}: `{key}` list must contain strings"))
            })
            .collect(),
        other => other
            .as_str()
            .map(|s| vec![s.to_string()])
            .ok_or_else(|| format!("line {line_no}: `{key}` expects a list of strings")),
    }
}

fn expect_arity(args: &[Value], expected: usize, name: &str, line_no: usize) -> Result<(), String> {
    if args.len() != expected {
        return Err(format!(
            "line {line_no}: `{name}(...)` expects {expected} argument(s), got {}",
            args.len()
        ));
    }
    Ok(())
}

fn expect_string_ref<'a>(value: &'a Value, key: &str, line_no: usize) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects a string value"))
}

fn expect_bool(value: &Value, key: &str, line_no: usize) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects true/false"))
}

fn expect_usize(value: &Value, key: &str, line_no: usize) -> Result<usize, String> {
    value
        .as_usize()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects an integer"))
}

fn expect_string_list(value: &Value, key: &str, line_no: usize) -> Result<Vec<String>, String> {
    let items = value
        .as_list()
        .ok_or_else(|| format!("line {line_no}: `{key}` expects a list [\"a\", \"b\"]"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("line {line_no}: `{key}` list must contain strings"))
        })
        .collect()
}

/// Parse a transform pipeline value into a list of transforms.
fn parse_transforms(value: &Value, line_no: usize) -> Result<Vec<Transform>, String> {
    let steps: Vec<&Value> = match value {
        Value::Pipeline(parts) => parts.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for step in steps {
        if let Some(transform) = parse_transform(step, line_no)? {
            out.push(transform);
        }
    }
    Ok(out)
}

fn parse_transform(value: &Value, line_no: usize) -> Result<Option<Transform>, String> {
    let transform = match value {
        Value::Str(name) => match name.trim().to_ascii_lowercase().as_str() {
            "none" | "identity" | "" => return Ok(None),
            "lower" | "lowercase" => Ok(Transform::Lower),
            "upper" | "uppercase" => Ok(Transform::Upper),
            "trim" => Ok(Transform::Trim),
            "collapse" | "squash" => Ok(Transform::Collapse),
            "int" | "integer" => Ok(Transform::Int),
            "float" | "number" => Ok(Transform::Float),
            "bool" | "boolean" => Ok(Transform::Bool),
            other => Err(format!("line {line_no}: unknown transform `{other}`")),
        },
        Value::Call(name, args) => match name.trim().to_ascii_lowercase().as_str() {
            "date" | "datetime" | "time" => {
                let (inputs, output) = parse_date_args(args, line_no)?;
                Ok(Transform::Date { inputs, output })
            }
            "replace" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {line_no}: replace(from, to) expects 2 arguments"
                    ));
                }
                // xan-style: `replace(regex("p"), "r")` performs a regex
                // replacement with capture-group references; a plain string is
                // a literal replacement.
                if let Some(source) = regex_source(&args[0]) {
                    Ok(Transform::RegexReplace {
                        pattern: compile_regex(source, line_no)?,
                        replacement: expect_string(&args[1], "replace", line_no)?,
                    })
                } else {
                    Ok(Transform::Replace {
                        from: expect_string(&args[0], "replace", line_no)?,
                        to: expect_string(&args[1], "replace", line_no)?,
                    })
                }
            }
            "regex_replace" => {
                if args.len() != 2 {
                    return Err(format!(
                        "line {line_no}: regex_replace(pattern, replacement) expects 2 arguments"
                    ));
                }
                let source = regex_pattern(&args[0], "regex_replace", line_no)?;
                Ok(Transform::RegexReplace {
                    pattern: compile_regex(&source, line_no)?,
                    replacement: expect_string(&args[1], "regex_replace", line_no)?,
                })
            }
            "match" | "capture" => {
                let first = args.first().ok_or_else(|| {
                    format!("line {line_no}: match(pattern[, group]) needs an argument")
                })?;
                let source = regex_pattern(first, "match", line_no)?;
                let group = match args.get(1) {
                    Some(value) => expect_usize(value, "match", line_no)?,
                    None => 0,
                };
                Ok(Transform::RegexExtract {
                    pattern: compile_regex(&source, line_no)?,
                    group,
                })
            }
            "regex_keep" | "keep" => {
                let first = args.first().ok_or_else(|| {
                    format!("line {line_no}: regex_keep(pattern) needs an argument")
                })?;
                let source = regex_pattern(first, "regex_keep", line_no)?;
                Ok(Transform::RegexKeep {
                    pattern: compile_regex(&source, line_no)?,
                })
            }
            "regex" => {
                // Bare `regex("p")` as a transform extracts the whole match.
                let first = args
                    .first()
                    .ok_or_else(|| format!("line {line_no}: regex(pattern) needs an argument"))?;
                let source = regex_pattern(first, "regex", line_no)?;
                Ok(Transform::RegexExtract {
                    pattern: compile_regex(&source, line_no)?,
                    group: 0,
                })
            }
            "prefix" => Ok(Transform::Prefix(expect_string(
                args.first()
                    .ok_or_else(|| format!("line {line_no}: prefix() needs 1 argument"))?,
                "prefix",
                line_no,
            )?)),
            "suffix" => Ok(Transform::Suffix(expect_string(
                args.first()
                    .ok_or_else(|| format!("line {line_no}: suffix() needs 1 argument"))?,
                "suffix",
                line_no,
            )?)),
            other => Err(format!("line {line_no}: unknown transform `{other}`")),
        },
        _ => Err(format!("line {line_no}: invalid transform value")),
    };
    transform.map(Some)
}

fn parse_date_args(args: &[Value], line_no: usize) -> Result<(Vec<String>, String), String> {
    match args.len() {
        1 => Ok((
            vec![expect_string(&args[0], "date", line_no)?],
            "%Y-%m-%d".to_string(),
        )),
        2 => {
            let inputs = match &args[0] {
                Value::List(items) => items
                    .iter()
                    .map(|v| expect_string(v, "date", line_no))
                    .collect::<Result<Vec<_>, _>>()?,
                other => vec![expect_string(other, "date", line_no)?],
            };
            Ok((inputs, expect_string(&args[1], "date", line_no)?))
        }
        _ => Err(format!(
            "line {line_no}: date(format) or date([formats], output) expected"
        )),
    }
}

// ---------------------------------------------------------------------------
// Lexing helpers
// ---------------------------------------------------------------------------

fn parse_rule_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        parse_value(trimmed).ok()?.as_str().map(str::to_string)
    } else {
        Some(trimmed.to_string())
    }
}

/// Strip a `#` comment that is not inside quotes, carrying the quote state in
/// and out so that multi-line quoted values are handled correctly.
fn strip_comment_state(line: &str, quote: &mut Option<char>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut escaped = false;

    for c in line.chars() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match *quote {
            Some(q) => {
                if c == '\\' {
                    escaped = true;
                    out.push(c);
                } else if c == q {
                    *quote = None;
                    out.push(c);
                } else {
                    out.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    *quote = Some(c);
                    out.push(c);
                } else if c == '#' {
                    break;
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Strip a `#` comment that is not inside quotes (single line).
#[cfg(test)]
fn strip_comment(line: &str) -> String {
    let mut quote = None;
    strip_comment_state(line, &mut quote)
}

/// Whether quotes and value brackets are balanced (used for line continuation).
/// Block braces are intentionally ignored: `rule "x" {` is a complete header.
fn is_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                _ => {}
            },
        }
    }
    depth <= 0 && quote.is_none()
}

/// Split `s` on top-level occurrences of `sep`, respecting quotes and brackets.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in s.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    current.push(c);
                } else if c == '[' || c == '(' || c == '{' {
                    depth += 1;
                    current.push(c);
                } else if c == ']' || c == ')' || c == '}' {
                    depth -= 1;
                    current.push(c);
                } else if c == sep && depth == 0 {
                    parts.push(current.trim().to_string());
                    current.clear();
                } else {
                    current.push(c);
                }
            }
        }
    }
    parts.push(current.trim().to_string());
    parts
}

/// Parse a DSL value.
pub fn parse_value(input: &str) -> Result<Value, String> {
    let parts = split_top_level(input, '|');
    if parts.len() > 1 {
        if parts.iter().any(|p| p.is_empty()) {
            return Err("invalid pipeline: empty step".to_string());
        }
        let steps = parts
            .iter()
            .map(|p| parse_atom(p))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Pipeline(steps));
    }
    parse_atom(input)
}

fn parse_atom(input: &str) -> Result<Value, String> {
    let s = input.trim();
    if s.is_empty() {
        return Ok(Value::Str(String::new()));
    }

    // Quoted string.
    let first = s.chars().next().unwrap();
    if first == '"' || first == '\'' {
        if s.len() < 2 || !s.ends_with(first) {
            return Err(format!("unterminated string: {s}"));
        }
        let inner = &s[first.len_utf8()..s.len() - first.len_utf8()];
        return Ok(Value::Str(unescape(inner)));
    }

    // List.
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return Ok(Value::List(Vec::new()));
        }
        let items = split_top_level(inner, ',')
            .iter()
            .map(|p| parse_value(p))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::List(items));
    }

    // Call: identifier(...)
    if let Some(open) = s.find('(') {
        if s.ends_with(')') {
            let name = s[..open].trim();
            if !name.is_empty() && is_identifier(name) {
                let inner = &s[open + 1..s.len() - 1];
                let args = if inner.trim().is_empty() {
                    Vec::new()
                } else {
                    split_top_level(inner, ',')
                        .iter()
                        .map(|p| parse_value(p))
                        .collect::<Result<Vec<_>, _>>()?
                };
                return Ok(Value::Call(name.to_string(), args));
            }
        }
    }

    Ok(Value::Str(s.to_string()))
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

#[allow(clippy::while_let_on_iterator)]
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_program() {
        let src = r#"
            defaults {
              separator = ";"
              report_limit = 3
            }
            rule "r1" {
              left = a
              right = b
              transform_left = trim | lower
              mapping = auto
            }
            rule "r2" {
              left = c
              right = d
              mapping_files = ["m1.csv", "m2.csv"]
              mapping_left = name
              mapping_right = code
            }
        "#;
        let program = parse(src).unwrap();
        assert_eq!(program.rules.len(), 2);
        assert_eq!(program.defaults.separator, Separator::literal(";"));
        assert_eq!(program.rules[0].transform_left.len(), 2);
        assert!(matches!(program.rules[0].mapping, MappingSourceDef::Auto));
        match &program.rules[1].mapping {
            MappingSourceDef::Files(sources) => {
                let source = &sources[0];
                assert_eq!(source.files.len(), 2);
                assert_eq!(source.left, &["name".to_string()]);
                assert_eq!(source.right, &["code".to_string()]);
                // Legacy `mapping_left` keys the data by the rule's `left`.
                assert_eq!(source.key, Some(ColumnSpec::Columns(vec!["c".to_string()])));
            }
            _ => panic!("expected files mapping"),
        }
    }

    #[test]
    fn parses_values() {
        assert_eq!(parse_value("\"a,b\"").unwrap(), Value::Str("a,b".into()));
        assert!(matches!(parse_value("[a, b]").unwrap(), Value::List(_)));
        assert!(matches!(
            parse_value("date(\"%Y\", \"%Y\")").unwrap(),
            Value::Call(..)
        ));
        assert!(matches!(parse_value("a | b").unwrap(), Value::Pipeline(_)));
    }

    #[test]
    fn parses_composite_columns() {
        let src = r#"
            rule "r" {
              left = [a, b]
              right = c
              join_separator = "|"
              mapping_files = ["m.csv"]
              mapping_left = [x, y]
              mapping_right = z
            }
        "#;
        let program = parse(src).unwrap();
        let rule = &program.rules[0];
        assert_eq!(rule.left, ColumnSpec::Columns(vec!["a".into(), "b".into()]));
        assert_eq!(rule.right, ColumnSpec::Columns(vec!["c".into()]));
        assert_eq!(rule.join_separator.as_deref(), Some("|"));
        match &rule.mapping {
            MappingSourceDef::Files(sources) => {
                let source = &sources[0];
                assert_eq!(source.left, &["x".to_string(), "y".to_string()]);
                assert_eq!(source.right, &["z".to_string()]);
                assert_eq!(
                    source.key,
                    Some(ColumnSpec::Columns(vec![
                        "a".to_string(),
                        "b".to_string()
                    ]))
                );
            }
            _ => panic!("expected files mapping"),
        }
    }

    #[test]
    fn parses_regex_constructs() {
        let src = r#"
            defaults {
              separator = regex("\\s*[;,|]\\s*")
            }
            rule "r" {
              left = a
              transform_left = replace(regex("[^0-9]"), "") | match(regex("^(\\d+)"), 1)
              pattern = "^[A-Z]{2}$"
              compare = matches
            }
        "#;
        let program = parse(src).unwrap();
        assert!(matches!(program.defaults.separator, Separator::Regex(_)));
        let rule = &program.rules[0];
        assert_eq!(rule.transform_left.len(), 2);
        assert!(matches!(
            rule.transform_left[0],
            Transform::RegexReplace { .. }
        ));
        assert!(matches!(
            rule.transform_left[1],
            Transform::RegexExtract { group: 1, .. }
        ));
        assert!(rule.pattern.is_some());
        assert_eq!(rule.compare, Some(CompareOp::Matches));
    }

    #[test]
    fn rejects_invalid_regex() {
        let src = r#"
            rule "r" {
              left = a
              right = b
              transform_left = replace(regex("("), "")
            }
        "#;
        let error = parse(src).unwrap_err();
        assert!(error.contains("invalid regex"), "{error}");
    }

    #[test]
    fn comments_and_quotes() {
        assert_eq!(
            strip_comment(r#"a = "x # y" # real comment"#).trim(),
            r#"a = "x # y""#
        );
    }

    #[test]
    fn hash_inside_multiline_string_is_not_a_comment() {
        let src = "rule \"r\" {\n  left = a\n  right = b\n  pattern = \"x\n#y\"\n  compare = matches\n}\n";
        let program = parse(src).unwrap();
        assert_eq!(program.rules[0].pattern.as_ref().unwrap().as_str(), "x\n#y");
    }

    #[test]
    fn parses_or_fallback_columns() {
        let src = r#"
            rule "r" {
              left = or(country_name, country_short, country_en)
              right = country_code
            }
        "#;
        let program = parse(src).unwrap();
        assert_eq!(
            program.rules[0].left,
            ColumnSpec::Or(vec![
                "country_name".into(),
                "country_short".into(),
                "country_en".into()
            ])
        );
        assert_eq!(
            program.rules[0].left.display(),
            "or(country_name, country_short, country_en)"
        );
    }

    #[test]
    fn parses_validation_skipped_and_mapping_filter() {
        let src = r#"
            rule "r" {
              left = code
              right = expected
              validation_skipped = any_in([col1, col2, col3], ["val1", "val2"])
              mapping_filter = eq(kind, "primary")
            }
            rule "single" {
              left = code
              right = expected
              validation_skipped = in(status, ["archived", "deleted"])
            }
        "#;
        let program = parse(src).unwrap();
        match program.rules[0].skip.as_ref().unwrap() {
            Predicate::AnyIn { columns, values } => {
                assert_eq!(columns.len(), 3);
                assert_eq!(values, &["val1".to_string(), "val2".to_string()]);
            }
            other => panic!("expected AnyIn, got {other:?}"),
        }
        assert_eq!(
            program.rules[0].mapping_filter,
            Some(Predicate::Eq {
                column: ColumnSpec::Columns(vec!["kind".into()]),
                value: "primary".into(),
            })
        );
        assert!(matches!(
            program.rules[1].skip.as_ref().unwrap(),
            Predicate::In { .. }
        ));
    }

    #[test]
    fn rejects_unknown_predicate() {
        let src = r#"
            rule "r" {
              left = a
              right = b
              validation_skipped = bogus(a, ["x"])
            }
        "#;
        let error = parse(src).unwrap_err();
        assert!(error.contains("unknown predicate"), "{error}");
    }
}
