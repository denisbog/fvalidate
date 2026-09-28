//! A compact expression language modelled on `xan`'s own "moonblade" engine.
//!
//! It powers the `derive` rule key, which computes extra values from a row:
//!
//! ```text
//! rule "version fields agree" {
//!   derive = 'row.match(/\d+\.\d+/) or "" as version,
//!             (row.match(/\d+\.\d+/) or "").split(".") as (major, minor)'
//!   left   = [major, minor]
//!   right  = [major_declared, minor_declared]
//! }
//! ```
//!
//! The language is intentionally a subset of xan's expression language
//! (<https://github.com/medialab/xan>): literals, regex literals, column
//! identifiers, `or`/`and`/`not`, comparisons, arithmetic, string/list
//! indexing and slicing, method & function calls, lists and pipelines.
//!
//! Expressions are parsed once (regexes are compiled once, at parse time) and
//! then evaluated per row. Evaluation is infallible: type mismatches yield the
//! empty value rather than an error, so a malformed cell simply fails the rule
//! instead of aborting the whole run.

use std::sync::Arc;

use regex::{Regex, RegexBuilder};

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A dynamically typed value produced by an expression.
#[derive(Debug, Clone)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Regex(Arc<Regex>),
}

impl Value {
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::Str(value) => !value.is_empty(),
            Value::List(value) => !value.is_empty(),
            Value::Regex(value) => !value.as_str().is_empty(),
        }
    }

    /// `null` or the empty string.
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::None) || matches!(self, Value::Str(value) if value.is_empty())
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            Value::Float(value) if value.fract() == 0.0 => Some(*value as i64),
            Value::Bool(value) => Some(*value as i64),
            Value::Str(value) => value.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(value) => Some(*value as f64),
            Value::Float(value) => Some(*value),
            Value::Bool(value) => Some(*value as i64 as f64),
            Value::Str(value) => value.trim().parse().ok(),
            _ => None,
        }
    }

    /// Render a value as a single string. Lists are joined with `separator`.
    pub fn scalar_string(&self, separator: &str) -> String {
        match self {
            Value::None => String::new(),
            Value::Bool(value) => value.to_string(),
            Value::Int(value) => value.to_string(),
            Value::Float(value) => format_float(*value),
            Value::Str(value) => value.clone(),
            Value::Regex(value) => value.as_str().to_string(),
            Value::List(items) => items
                .iter()
                .map(|item| item.scalar_string(separator))
                .collect::<Vec<_>>()
                .join(separator),
        }
    }
}

fn format_float(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The canonical single-string form of a value (lists joined with `,`).
fn scalar(value: &Value) -> String {
    value.scalar_string(",")
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// Where a resolved identifier reads its value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef {
    /// A column of the input row, identified by its header index.
    Column(usize),
    /// An output produced earlier by the same rule's `derive` block.
    Derived(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Rem,
    Pow,
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Regex(Arc<Regex>),
    /// An identifier as written; resolved to a `Ref` by `rules::compile`.
    Ident(String),
    /// The piped value placeholder (`_`).
    Current,
    Ref(ValueRef),
    List(Vec<Expr>),
    Call {
        name: String,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Slice {
        target: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
}

/// One `expr as name` (or `expr as (a, b)`) clause of a `derive` block.
#[derive(Debug, Clone)]
pub struct DerivedDef {
    pub names: Vec<String>,
    pub expr: Expr,
}

impl Expr {
    /// Collect the header indices referenced by `ValueRef::Column` nodes.
    pub fn column_refs(&self, out: &mut Vec<usize>) {
        match self {
            Expr::Ref(ValueRef::Column(index)) => out.push(*index),
            Expr::List(items) => items.iter().for_each(|item| item.column_refs(out)),
            Expr::Call { args, .. } => args.iter().for_each(|arg| arg.column_refs(out)),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.column_refs(out);
                rhs.column_refs(out);
            }
            Expr::Unary { operand, .. } => operand.column_refs(out),
            Expr::Index { target, index } => {
                target.column_refs(out);
                index.column_refs(out);
            }
            Expr::Slice { target, start, end } => {
                target.column_refs(out);
                if let Some(start) = start {
                    start.column_refs(out);
                }
                if let Some(end) = end {
                    end.column_refs(out);
                }
            }
            _ => {}
        }
    }

    /// Rewrite every `ValueRef` with `f` (used to translate header indices into
    /// per-segment cell slots).
    pub fn remap_refs(&mut self, f: &mut dyn FnMut(ValueRef) -> ValueRef) {
        match self {
            Expr::Ref(reference) => *reference = f(*reference),
            Expr::List(items) => items.iter_mut().for_each(|item| item.remap_refs(f)),
            Expr::Call { args, .. } => args.iter_mut().for_each(|arg| arg.remap_refs(f)),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.remap_refs(f);
                rhs.remap_refs(f);
            }
            Expr::Unary { operand, .. } => operand.remap_refs(f),
            Expr::Index { target, index } => {
                target.remap_refs(f);
                index.remap_refs(f);
            }
            Expr::Slice { target, start, end } => {
                target.remap_refs(f);
                if let Some(start) = start {
                    start.remap_refs(f);
                }
                if let Some(end) = end {
                    end.remap_refs(f);
                }
            }
            _ => {}
        }
    }

    /// Fail on unknown function names so typos are caught when rules load.
    pub fn check_functions(&self) -> Result<(), String> {
        self.check_functions_with(false)
    }

    /// Like [`Expr::check_functions`], but optionally allows the filter-only
    /// `col("column name")` reference.
    fn check_functions_with(&self, allow_col: bool) -> Result<(), String> {
        match self {
            Expr::Call { name, args } => {
                if !(is_known_function(name) || (allow_col && name == "col")) {
                    return Err(format!("unknown function `{name}`"));
                }
                args.iter()
                    .try_for_each(|arg| arg.check_functions_with(allow_col))
            }
            Expr::List(items) => items
                .iter()
                .try_for_each(|item| item.check_functions_with(allow_col)),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.check_functions_with(allow_col)?;
                rhs.check_functions_with(allow_col)
            }
            Expr::Unary { operand, .. } => operand.check_functions_with(allow_col),
            Expr::Index { target, index } => {
                target.check_functions_with(allow_col)?;
                index.check_functions_with(allow_col)
            }
            Expr::Slice { target, start, end } => {
                target.check_functions_with(allow_col)?;
                if let Some(start) = start {
                    start.check_functions_with(allow_col)?;
                }
                if let Some(end) = end {
                    end.check_functions_with(allow_col)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn count_current(&self, count: &mut usize) {
        match self {
            Expr::Current => *count += 1,
            Expr::List(items) => items.iter().for_each(|item| item.count_current(count)),
            Expr::Call { args, .. } => args.iter().for_each(|arg| arg.count_current(count)),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.count_current(count);
                rhs.count_current(count);
            }
            Expr::Unary { operand, .. } => operand.count_current(count),
            Expr::Index { target, index } => {
                target.count_current(count);
                index.count_current(count);
            }
            Expr::Slice { target, start, end } => {
                target.count_current(count);
                if let Some(start) = start {
                    start.count_current(count);
                }
                if let Some(end) = end {
                    end.count_current(count);
                }
            }
            _ => {}
        }
    }

    fn substitute_current(&mut self, value: &Expr) {
        match self {
            Expr::Current => *self = value.clone(),
            Expr::List(items) => items.iter_mut().for_each(|item| item.substitute_current(value)),
            Expr::Call { args, .. } => {
                args.iter_mut().for_each(|arg| arg.substitute_current(value))
            }
            Expr::Binary { lhs, rhs, .. } => {
                lhs.substitute_current(value);
                rhs.substitute_current(value);
            }
            Expr::Unary { operand, .. } => operand.substitute_current(value),
            Expr::Index { target, index } => {
                target.substitute_current(value);
                index.substitute_current(value);
            }
            Expr::Slice { target, start, end } => {
                target.substitute_current(value);
                if let Some(start) = start {
                    start.substitute_current(value);
                }
                if let Some(end) = end {
                    end.substitute_current(value);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Tok {
    Int(i64),
    Float(f64),
    Str(String),
    Regex(Arc<Regex>),
    Ident(String),
    Underscore,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    Pipe,
    PipePipe,
    AmpAmp,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    SlashSlash,
    Percent,
    StarStar,
    PlusPlus,
    Eof,
}

/// Whether a token can be the last token of a value (used to tell a regex
/// literal from a division).
fn ends_value(token: &Tok) -> bool {
    match token {
        Tok::Int(_)
        | Tok::Float(_)
        | Tok::Str(_)
        | Tok::Regex(_)
        | Tok::Underscore
        | Tok::RParen
        | Tok::RBracket => true,
        Tok::Ident(name) => !matches!(
            name.as_str(),
            "or" | "and" | "not" | "in" | "as" | "eq" | "ne" | "lt" | "le" | "gt" | "ge"
        ),
        _ => false,
    }
}

fn lex(source: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = source.chars().collect();
    let mut pos = 0usize;
    let mut toks: Vec<Tok> = Vec::new();
    let mut prev_value = false;

    while pos < chars.len() {
        let c = chars[pos];
        if c.is_whitespace() {
            pos += 1;
            continue;
        }

        match c {
            '(' => {
                toks.push(Tok::LParen);
                pos += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                pos += 1;
            }
            '[' => {
                toks.push(Tok::LBracket);
                pos += 1;
            }
            ']' => {
                toks.push(Tok::RBracket);
                pos += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                pos += 1;
            }
            ':' => {
                toks.push(Tok::Colon);
                pos += 1;
            }
            '.' => {
                toks.push(Tok::Dot);
                pos += 1;
            }
            '+' => {
                if chars.get(pos + 1) == Some(&'+') {
                    toks.push(Tok::PlusPlus);
                    pos += 2;
                } else {
                    toks.push(Tok::Plus);
                    pos += 1;
                }
            }
            '-' => {
                toks.push(Tok::Minus);
                pos += 1;
            }
            '*' => {
                if chars.get(pos + 1) == Some(&'*') {
                    toks.push(Tok::StarStar);
                    pos += 2;
                } else {
                    toks.push(Tok::Star);
                    pos += 1;
                }
            }
            '%' => {
                toks.push(Tok::Percent);
                pos += 1;
            }
            '=' => {
                if chars.get(pos + 1) == Some(&'=') {
                    toks.push(Tok::EqEq);
                    pos += 2;
                } else {
                    return Err("unexpected `=`, use `==` to compare".to_string());
                }
            }
            '!' => {
                if chars.get(pos + 1) == Some(&'=') {
                    toks.push(Tok::NotEq);
                    pos += 2;
                } else {
                    toks.push(Tok::Bang);
                    pos += 1;
                }
            }
            '<' => {
                if chars.get(pos + 1) == Some(&'=') {
                    toks.push(Tok::Le);
                    pos += 2;
                } else {
                    toks.push(Tok::Lt);
                    pos += 1;
                }
            }
            '>' => {
                if chars.get(pos + 1) == Some(&'=') {
                    toks.push(Tok::Ge);
                    pos += 2;
                } else {
                    toks.push(Tok::Gt);
                    pos += 1;
                }
            }
            '&' => {
                if chars.get(pos + 1) == Some(&'&') {
                    toks.push(Tok::AmpAmp);
                    pos += 2;
                } else {
                    return Err("unexpected `&`, use `&&` or `and`".to_string());
                }
            }
            '|' => {
                if chars.get(pos + 1) == Some(&'|') {
                    toks.push(Tok::PipePipe);
                    pos += 2;
                } else {
                    toks.push(Tok::Pipe);
                    pos += 1;
                }
            }
            '/' => {
                if prev_value {
                    if chars.get(pos + 1) == Some(&'/') {
                        toks.push(Tok::SlashSlash);
                        pos += 2;
                    } else {
                        toks.push(Tok::Slash);
                        pos += 1;
                    }
                } else {
                    let (regex, next) = lex_regex(&chars, pos)?;
                    toks.push(Tok::Regex(regex));
                    pos = next;
                }
            }
            '"' | '\'' | '`' => {
                let (string, next) = lex_string(&chars, pos)?;
                toks.push(Tok::Str(string));
                pos = next;
            }
            _ if c.is_ascii_digit() => {
                let (token, next) = lex_number(&chars, pos)?;
                toks.push(token);
                pos = next;
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = pos;
                pos += 1;
                while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
                    pos += 1;
                }
                let word: String = chars[start..pos].iter().collect();
                if word == "_" {
                    toks.push(Tok::Underscore);
                } else {
                    toks.push(Tok::Ident(word));
                }
            }
            other => return Err(format!("unexpected character `{other}`")),
        }

        prev_value = toks.last().is_some_and(ends_value);
    }

    toks.push(Tok::Eof);
    Ok(toks)
}

fn lex_regex(chars: &[char], start: usize) -> Result<(Arc<Regex>, usize), String> {
    let mut i = start + 1;
    let mut pattern = String::new();
    let mut escaped = false;

    loop {
        let Some(&c) = chars.get(i) else {
            return Err("unterminated regex literal".to_string());
        };
        if escaped {
            pattern.push('\\');
            pattern.push(c);
            escaped = false;
            i += 1;
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                i += 1;
            }
            '/' => {
                i += 1;
                break;
            }
            _ => {
                pattern.push(c);
                i += 1;
            }
        }
    }

    let mut case_insensitive = false;
    while let Some(&flag) = chars.get(i) {
        if !flag.is_ascii_alphabetic() {
            break;
        }
        if flag == 'i' {
            case_insensitive = true;
        }
        i += 1;
    }

    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|e| format!("invalid regex /{pattern}/: {e}"))?;

    Ok((Arc::new(regex), i))
}

fn lex_string(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let quote = chars[start];
    let mut i = start + 1;
    let mut out = String::new();

    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            let Some(&next) = chars.get(i + 1) else {
                return Err("unterminated escape sequence".to_string());
            };
            match next {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                '`' => out.push('`'),
                '0' => out.push('\0'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
            i += 2;
        } else if c == quote {
            return Ok((out, i + 1));
        } else {
            out.push(c);
            i += 1;
        }
    }

    Err("unterminated string literal".to_string())
}

fn lex_number(chars: &[char], start: usize) -> Result<(Tok, usize), String> {
    let mut i = start;
    let mut text = String::new();
    let mut is_float = false;

    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
        if chars[i] != '_' {
            text.push(chars[i]);
        }
        i += 1;
    }

    if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
        is_float = true;
        text.push('.');
        i += 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            text.push(chars[i]);
            i += 1;
        }
    }

    if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
        is_float = true;
        text.push('e');
        i += 1;
        if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
            text.push(chars[i]);
            i += 1;
        }
        while i < chars.len() && chars[i].is_ascii_digit() {
            text.push(chars[i]);
            i += 1;
        }
    }

    if is_float {
        text.parse::<f64>()
            .map(|value| (Tok::Float(value), i))
            .map_err(|_| format!("invalid number `{text}`"))
    } else {
        text.parse::<i64>()
            .map(|value| (Tok::Int(value), i))
            .map_err(|_| format!("invalid number `{text}`"))
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn peek_at(&self, offset: usize) -> &Tok {
        self.toks.get(self.pos + offset).unwrap_or(&Tok::Eof)
    }

    fn bump(&mut self) -> Tok {
        let token = self.peek().clone();
        if !matches!(token, Tok::Eof) {
            self.pos += 1;
        }
        token
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    fn eat(&mut self, token: &Tok) -> bool {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(token) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_ident(&mut self, name: &str) -> bool {
        if matches!(self.peek(), Tok::Ident(value) if value == name) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Tok) -> Result<(), String> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(format!("expected {token:?}, got {:?}", self.peek()))
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        // `|` is the lowest-precedence operator.
        let mut expr = self.parse_or()?;
        while self.eat(&Tok::Pipe) {
            let rhs = self.parse_or()?;
            expr = pipe(expr, rhs);
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Tok::PipePipe) || self.eat_ident("or") {
            let rhs = self.parse_and()?;
            lhs = binary(BinOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_equality()?;
        while self.eat(&Tok::AmpAmp) || self.eat_ident("and") {
            let rhs = self.parse_equality()?;
            lhs = binary(BinOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Tok::EqEq => BinOp::Eq,
                Tok::NotEq => BinOp::Ne,
                Tok::Ident(name) if name == "eq" => BinOp::Eq,
                Tok::Ident(name) if name == "ne" => BinOp::Ne,
                Tok::Ident(name) if name == "in" => BinOp::In,
                Tok::Ident(name) if name == "not" => {
                    if matches!(self.peek_at(1), Tok::Ident(next) if next == "in") {
                        self.pos += 2;
                        let rhs = self.parse_comparison()?;
                        lhs = unary(UnOp::Not, binary(BinOp::In, lhs, rhs));
                        continue;
                    }
                    break;
                }
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_comparison()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                Tok::Ident(name) if name == "lt" => BinOp::Lt,
                Tok::Ident(name) if name == "le" => BinOp::Le,
                Tok::Ident(name) if name == "gt" => BinOp::Gt,
                Tok::Ident(name) if name == "ge" => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_additive()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                Tok::PlusPlus => BinOp::Concat,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::SlashSlash => BinOp::IDiv,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_power()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_power(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_unary()?;
        if self.eat(&Tok::StarStar) {
            let rhs = self.parse_power()?;
            Ok(binary(BinOp::Pow, lhs, rhs))
        } else {
            Ok(lhs)
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Bang) || self.eat_ident("not") {
            let operand = self.parse_unary()?;
            return Ok(unary(UnOp::Not, operand));
        }
        if self.eat(&Tok::Minus) {
            let operand = self.parse_unary()?;
            return Ok(unary(UnOp::Neg, operand));
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat(&Tok::Dot) {
                let name = match self.bump() {
                    Tok::Ident(name) => name,
                    other => return Err(format!("expected a method name after `.`, got {other:?}")),
                };
                if self.eat(&Tok::LParen) {
                    let mut args = vec![expr];
                    self.parse_call_args(&mut args)?;
                    expr = Expr::Call { name, args };
                } else {
                    expr = Expr::Call {
                        name: "get".to_string(),
                        args: vec![expr, Expr::Str(name)],
                    };
                }
            } else if self.eat(&Tok::LBracket) {
                expr = self.parse_index(expr)?;
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_index(&mut self, target: Expr) -> Result<Expr, String> {
        if self.eat(&Tok::Colon) {
            if self.eat(&Tok::RBracket) {
                return Ok(Expr::Slice {
                    target: Box::new(target),
                    start: None,
                    end: None,
                });
            }
            let end = self.parse_expr()?;
            self.expect(&Tok::RBracket)?;
            return Ok(Expr::Slice {
                target: Box::new(target),
                start: None,
                end: Some(Box::new(end)),
            });
        }

        let first = self.parse_expr()?;
        if self.eat(&Tok::Colon) {
            if self.eat(&Tok::RBracket) {
                return Ok(Expr::Slice {
                    target: Box::new(target),
                    start: Some(Box::new(first)),
                    end: None,
                });
            }
            let end = self.parse_expr()?;
            self.expect(&Tok::RBracket)?;
            return Ok(Expr::Slice {
                target: Box::new(target),
                start: Some(Box::new(first)),
                end: Some(Box::new(end)),
            });
        }
        self.expect(&Tok::RBracket)?;
        Ok(Expr::Index {
            target: Box::new(target),
            index: Box::new(first),
        })
    }

    fn parse_call_args(&mut self, args: &mut Vec<Expr>) -> Result<(), String> {
        if self.eat(&Tok::RParen) {
            return Ok(());
        }
        loop {
            args.push(self.parse_expr()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen)?;
            return Ok(());
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Tok::Int(value) => Ok(Expr::Int(value)),
            Tok::Float(value) => Ok(Expr::Float(value)),
            Tok::Str(value) => Ok(Expr::Str(value)),
            Tok::Regex(value) => Ok(Expr::Regex(value)),
            Tok::Underscore => Ok(Expr::Current),
            Tok::LParen => {
                let expr = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(expr)
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                if self.eat(&Tok::RBracket) {
                    return Ok(Expr::List(items));
                }
                loop {
                    items.push(self.parse_expr()?);
                    if self.eat(&Tok::Comma) {
                        continue;
                    }
                    self.expect(&Tok::RBracket)?;
                    break;
                }
                Ok(Expr::List(items))
            }
            Tok::Ident(name) => match name.as_str() {
                "true" => Ok(Expr::Bool(true)),
                "false" => Ok(Expr::Bool(false)),
                "null" => Ok(Expr::Null),
                _ => {
                    if self.eat(&Tok::LParen) {
                        let mut args = Vec::new();
                        self.parse_call_args(&mut args)?;
                        Ok(Expr::Call { name, args })
                    } else {
                        Ok(Expr::Ident(name))
                    }
                }
            },
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

fn binary(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn unary(op: UnOp, operand: Expr) -> Expr {
    Expr::Unary {
        op,
        operand: Box::new(operand),
    }
}

/// Thread a piped value into the right-hand expression: every `_` is replaced
/// by the left expression, or, when there is none and the right side is a call,
/// the value is prepended as its first argument (so `x | lower` works).
fn pipe(lhs: Expr, mut rhs: Expr) -> Expr {
    let mut count = 0;
    rhs.count_current(&mut count);
    if count > 0 {
        rhs.substitute_current(&lhs);
        rhs
    } else if let Expr::Call { name, mut args } = rhs {
        args.insert(0, lhs);
        Expr::Call { name, args }
    } else {
        rhs
    }
}

/// Parse a single expression.
#[allow(dead_code)]
pub fn parse(source: &str) -> Result<Expr, String> {
    parse_inner(source, false)
}

/// Parse a `mapping_filter` expression. Like [`parse`], but additionally allows
/// the filter-only `col("column name")` reference, used to name reference-file
/// columns that contain spaces.
#[allow(dead_code)]
pub fn parse_filter(source: &str) -> Result<Expr, String> {
    parse_inner(source, true)
}

#[allow(dead_code)]
fn parse_inner(source: &str, allow_col: bool) -> Result<Expr, String> {
    let toks = lex(source)?;
    let mut parser = Parser::new(toks);
    let expr = parser.parse_expr()?;
    if !parser.at_eof() {
        return Err(format!(
            "unexpected trailing input near {:?}",
            parser.peek()
        ));
    }
    expr.check_functions_with(allow_col)?;
    Ok(expr)
}

/// Parse a `derive` block: a comma-separated list of `expr as name` clauses,
/// where `name` may be a tuple such as `(major, minor)`.
pub fn parse_derive(source: &str) -> Result<Vec<DerivedDef>, String> {
    let toks = lex(source)?;
    let mut parser = Parser::new(toks);
    let mut defs = Vec::new();

    loop {
        if parser.at_eof() {
            break;
        }
        let expr = parser.parse_expr()?;

        if !parser.eat_ident("as") {
            return Err("each derive clause must end with `as <name>`".to_string());
        }
        let names = parser.parse_names()?;
        expr.check_functions()?;
        defs.push(DerivedDef { names, expr });

        if parser.eat(&Tok::Comma) {
            continue;
        }
        break;
    }

    if !parser.at_eof() {
        return Err(format!("unexpected trailing input near {:?}", parser.peek()));
    }
    if defs.is_empty() {
        return Err("derive expression is empty".to_string());
    }
    Ok(defs)
}

impl Parser {
    fn parse_names(&mut self) -> Result<Vec<String>, String> {
        if self.eat(&Tok::LParen) {
            let mut names = Vec::new();
            loop {
                names.push(self.expect_name()?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen)?;
                break;
            }
            if names.is_empty() {
                return Err("empty name tuple after `as`".to_string());
            }
            Ok(names)
        } else {
            Ok(vec![self.expect_name()?])
        }
    }

    fn expect_name(&mut self) -> Result<String, String> {
        match self.bump() {
            Tok::Ident(name) => Ok(name),
            Tok::Str(name) => Ok(name),
            other => Err(format!("expected a name after `as`, got {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Per-row evaluation context: the extracted cells (already remapped to slots)
/// and the values produced so far by the rule's `derive` block.
pub struct EvalContext<'a> {
    pub cells: &'a [String],
    pub derived: &'a [String],
}

pub fn eval(expr: &Expr, ctx: &EvalContext) -> Value {
    match expr {
        Expr::Null => Value::None,
        Expr::Bool(value) => Value::Bool(*value),
        Expr::Int(value) => Value::Int(*value),
        Expr::Float(value) => Value::Float(*value),
        Expr::Str(value) => Value::Str(value.clone()),
        Expr::Regex(value) => Value::Regex(value.clone()),
        Expr::Ident(_) | Expr::Current => Value::None,
        Expr::Ref(ValueRef::Column(slot)) => match ctx.cells.get(*slot) {
            Some(cell) => Value::Str(cell.clone()),
            None => Value::None,
        },
        Expr::Ref(ValueRef::Derived(index)) => match ctx.derived.get(*index) {
            Some(value) => Value::Str(value.clone()),
            None => Value::None,
        },
        Expr::List(items) => Value::List(items.iter().map(|item| eval(item, ctx)).collect()),
        Expr::Call { name, args } => eval_call(name, args, ctx),
        Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, ctx),
        Expr::Unary { op, operand } => {
            let value = eval(operand, ctx);
            match op {
                UnOp::Not => Value::Bool(!value.is_truthy()),
                UnOp::Neg => match value.as_number() {
                    Some(number) => number_value(-number),
                    None => Value::None,
                },
            }
        }
        Expr::Index { target, index } => {
            let target = eval(target, ctx);
            let index = eval(index, ctx);
            get_value(&target, &index)
        }
        Expr::Slice { target, start, end } => {
            let target = eval(target, ctx);
            let start = start.as_ref().and_then(|expr| eval(expr, ctx).as_int());
            let end = end.as_ref().and_then(|expr| eval(expr, ctx).as_int());
            slice_value(&target, start, end)
        }
    }
}

fn eval_call(name: &str, args: &[Expr], ctx: &EvalContext) -> Value {
    match name {
        "if" => {
            let condition = args.first().map(|arg| eval(arg, ctx)).unwrap_or(Value::None);
            let branch = if condition.is_truthy() { 1 } else { 2 };
            args.get(branch).map(|arg| eval(arg, ctx)).unwrap_or(Value::None)
        }
        "unless" => {
            let condition = args.first().map(|arg| eval(arg, ctx)).unwrap_or(Value::None);
            let branch = if condition.is_truthy() { 2 } else { 1 };
            args.get(branch).map(|arg| eval(arg, ctx)).unwrap_or(Value::None)
        }
        _ => {
            let values: Vec<Value> = args.iter().map(|arg| eval(arg, ctx)).collect();
            call_builtin(name, &values)
        }
    }
}

fn eval_binary(op: BinOp, lhs: &Expr, rhs: &Expr, ctx: &EvalContext) -> Value {
    match op {
        BinOp::Or => {
            let left = eval(lhs, ctx);
            if left.is_truthy() {
                left
            } else {
                eval(rhs, ctx)
            }
        }
        BinOp::And => {
            let left = eval(lhs, ctx);
            if left.is_truthy() {
                eval(rhs, ctx)
            } else {
                left
            }
        }
        _ => {
            let left = eval(lhs, ctx);
            let right = eval(rhs, ctx);
            apply_binary(op, &left, &right)
        }
    }
}

fn apply_binary(op: BinOp, left: &Value, right: &Value) -> Value {
    match op {
        BinOp::Eq => Value::Bool(values_equal(left, right)),
        BinOp::Ne => Value::Bool(!values_equal(left, right)),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ordering = compare_values(left, right);
            match ordering {
                Some(ordering) => {
                    use std::cmp::Ordering;
                    let result = match op {
                        BinOp::Lt => ordering == Ordering::Less,
                        BinOp::Le => ordering != Ordering::Greater,
                        BinOp::Gt => ordering == Ordering::Greater,
                        BinOp::Ge => ordering != Ordering::Less,
                        _ => unreachable!(),
                    };
                    Value::Bool(result)
                }
                None => Value::None,
            }
        }
        BinOp::In => contains_value(right, left),
        BinOp::Add => match (left.as_number(), right.as_number()) {
            (Some(a), Some(b)) => number_value(a + b),
            _ => Value::Str(format!("{}{}", scalar(left), scalar(right))),
        },
        BinOp::Sub => numeric(left, right, |a, b| a - b),
        BinOp::Mul => numeric(left, right, |a, b| a * b),
        BinOp::Div => match (left.as_number(), right.as_number()) {
            (Some(_), Some(0.0)) => Value::None,
            (Some(a), Some(b)) => Value::Float(a / b),
            _ => Value::None,
        },
        BinOp::IDiv => match (left.as_number(), right.as_number()) {
            (Some(_), Some(0.0)) => Value::None,
            (Some(a), Some(b)) => number_value((a / b).trunc()),
            _ => Value::None,
        },
        BinOp::Rem => match (left.as_number(), right.as_number()) {
            (Some(_), Some(0.0)) => Value::None,
            (Some(a), Some(b)) => number_value(a % b),
            _ => Value::None,
        },
        BinOp::Pow => numeric(left, right, f64::powf),
        BinOp::Concat => Value::Str(format!("{}{}", scalar(left), scalar(right))),
        BinOp::Or | BinOp::And => unreachable!(),
    }
}

fn numeric(left: &Value, right: &Value, op: impl Fn(f64, f64) -> f64) -> Value {
    match (left.as_number(), right.as_number()) {
        (Some(a), Some(b)) => number_value(op(a, b)),
        _ => Value::None,
    }
}

fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        Value::Int(number as i64)
    } else {
        Value::Float(number)
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left.as_number(), right.as_number()) {
        (Some(a), Some(b)) => a == b,
        _ => scalar(left) == scalar(right),
    }
}

fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left.as_number(), right.as_number()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => Some(scalar(left).cmp(&scalar(right))),
    }
}

fn get_value(target: &Value, index: &Value) -> Value {
    let resolve = |len: i64, position: i64| {
        let position = if position < 0 { len + position } else { position };
        (position >= 0).then_some(position as usize)
    };
    match target {
        Value::List(items) => match index.as_int().and_then(|p| resolve(items.len() as i64, p)) {
            Some(position) => items.get(position).cloned().unwrap_or(Value::None),
            None => Value::None,
        },
        Value::Str(text) => match index.as_int().and_then(|p| resolve(text.chars().count() as i64, p)) {
            Some(position) => text
                .chars()
                .nth(position)
                .map(|c| Value::Str(c.to_string()))
                .unwrap_or(Value::None),
            None => Value::None,
        },
        _ => Value::None,
    }
}

fn slice_value(target: &Value, start: Option<i64>, end: Option<i64>) -> Value {
    match target {
        Value::List(items) => {
            let len = items.len() as i64;
            let (from, to) = slice_bounds(len, start, end);
            Value::List(items[from..to].to_vec())
        }
        Value::Str(text) => {
            let chars: Vec<char> = text.chars().collect();
            let len = chars.len() as i64;
            let (from, to) = slice_bounds(len, start, end);
            Value::Str(chars[from..to].iter().collect())
        }
        _ => Value::None,
    }
}

/// Clamp `[start, end)` to a valid range, supporting negative indices from the
/// end (Python-style).
fn slice_bounds(len: i64, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let resolve = |value: i64| {
        if value < 0 {
            (len + value).max(0)
        } else {
            value.min(len)
        }
    };
    let from = resolve(start.unwrap_or(0)).clamp(0, len);
    let to = resolve(end.unwrap_or(len)).clamp(0, len);
    if to < from {
        (from as usize, from as usize)
    } else {
        (from as usize, to as usize)
    }
}

fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        "match"
            | "split"
            | "replace"
            | "trim"
            | "ltrim"
            | "rtrim"
            | "lower"
            | "upper"
            | "len"
            | "length"
            | "count"
            | "join"
            | "contains"
            | "startswith"
            | "endswith"
            | "int"
            | "integer"
            | "float"
            | "number"
            | "string"
            | "str"
            | "bool"
            | "boolean"
            | "get"
            | "slice"
            | "first"
            | "last"
            | "abs"
            | "round"
            | "floor"
            | "ceil"
            | "min"
            | "max"
            | "coalesce"
            | "is_null"
            | "is_empty"
            | "concat"
            | "to_string"
            | "if"
            | "unless"
            | "in_any"
    )
}

fn call_builtin(name: &str, args: &[Value]) -> Value {
    let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::None);

    match name {
        "match" => regex_match(args),
        "split" => split_value(args),
        "replace" => replace_value(args),
        "trim" => string_map(args, |text| text.trim().to_string()),
        "ltrim" => string_map(args, |text| text.trim_start().to_string()),
        "rtrim" => string_map(args, |text| text.trim_end().to_string()),
        "lower" => string_map(args, |text| text.to_lowercase()),
        "upper" => string_map(args, |text| text.to_uppercase()),
        "len" | "length" | "count" => match &arg(0) {
            Value::List(items) => Value::Int(items.len() as i64),
            Value::None => Value::Int(0),
            other => Value::Int(scalar(other).chars().count() as i64),
        },
        "join" => {
            let list = arg(0);
            let separator = scalar(&arg(1));
            match list {
                Value::List(items) => Value::Str(
                    items
                        .iter()
                        .map(|item| item.scalar_string(&separator))
                        .collect::<Vec<_>>()
                        .join(&separator),
                ),
                other => Value::Str(scalar(&other)),
            }
        }
        "contains" => contains_value(&arg(0), &arg(1)),
        "startswith" => Value::Bool(scalar(&arg(0)).starts_with(&scalar(&arg(1)))),
        "endswith" => Value::Bool(scalar(&arg(0)).ends_with(&scalar(&arg(1)))),
        "int" | "integer" => arg(0).as_int().map(Value::Int).unwrap_or(Value::None),
        "float" | "number" => arg(0).as_number().map(Value::Float).unwrap_or(Value::None),
        "string" | "str" | "to_string" => Value::Str(scalar(&arg(0))),
        "bool" | "boolean" => Value::Bool(arg(0).is_truthy()),
        "get" => get_value(&arg(0), &arg(1)),
        "slice" => {
            let start = args.get(1).and_then(Value::as_int);
            let end = args.get(2).and_then(Value::as_int);
            slice_value(&arg(0), start, end)
        }
        "first" => match arg(0) {
            Value::List(items) => items.first().cloned().unwrap_or(Value::None),
            other => Value::Str(scalar(&other).chars().next().map(String::from).unwrap_or_default()),
        },
        "last" => match arg(0) {
            Value::List(items) => items.last().cloned().unwrap_or(Value::None),
            other => Value::Str(scalar(&other).chars().last().map(String::from).unwrap_or_default()),
        },
        "abs" => arg(0).as_number().map(|n| number_value(n.abs())).unwrap_or(Value::None),
        "round" => arg(0).as_number().map(|n| number_value(n.round())).unwrap_or(Value::None),
        "floor" => arg(0).as_number().map(|n| number_value(n.floor())).unwrap_or(Value::None),
        "ceil" => arg(0).as_number().map(|n| number_value(n.ceil())).unwrap_or(Value::None),
        "min" => extremum(args, true),
        "max" => extremum(args, false),
        "coalesce" => args
            .iter()
            .find(|value| !value.is_nullish())
            .cloned()
            .unwrap_or(Value::None),
        "is_null" => Value::Bool(arg(0).is_nullish()),
        "is_empty" => Value::Bool(match &arg(0) {
            Value::List(items) => items.is_empty(),
            Value::None => true,
            other => scalar(other).is_empty(),
        }),
        "concat" => Value::Str(args.iter().map(scalar).collect::<String>()),
        _ => Value::None,
    }
}

fn string_map(args: &[Value], f: impl Fn(&str) -> String) -> Value {
    match args.first() {
        Some(Value::List(items)) => Value::List(
            items
                .iter()
                .map(|item| Value::Str(f(&item.scalar_string(","))))
                .collect(),
        ),
        Some(other) => Value::Str(f(&other.scalar_string(","))),
        None => Value::None,
    }
}

fn regex_match(args: &[Value]) -> Value {
    let haystack = scalar(&args.first().cloned().unwrap_or(Value::None));
    let Some(Value::Regex(regex)) = args.get(1) else {
        return Value::None;
    };
    let group = args.get(2).and_then(Value::as_int).unwrap_or(0).max(0) as usize;
    match regex.captures(&haystack) {
        Some(captures) => captures
            .get(group)
            .map(|matched| Value::Str(matched.as_str().to_string()))
            .unwrap_or(Value::None),
        None => Value::None,
    }
}

fn split_value(args: &[Value]) -> Value {
    let text = scalar(&args.first().cloned().unwrap_or(Value::None));
    let limit = args.get(2).and_then(Value::as_int).map(|n| n.max(0) as usize);

    match args.get(1) {
        Some(Value::Regex(regex)) => {
            let pieces: Vec<Value> = match limit {
                Some(limit) => regex
                    .splitn(&text, limit + 1)
                    .map(|piece| Value::Str(piece.to_string()))
                    .collect(),
                None => regex
                    .split(&text)
                    .map(|piece| Value::Str(piece.to_string()))
                    .collect(),
            };
            Value::List(pieces)
        }
        Some(pattern) => {
            let pattern = scalar(pattern);
            let pieces: Vec<Value> = match limit {
                Some(limit) => text
                    .splitn(limit + 1, pattern.as_str())
                    .map(|piece| Value::Str(piece.to_string()))
                    .collect(),
                None => text
                    .split(pattern.as_str())
                    .map(|piece| Value::Str(piece.to_string()))
                    .collect(),
            };
            Value::List(pieces)
        }
        None => Value::List(vec![Value::Str(text)]),
    }
}

fn replace_value(args: &[Value]) -> Value {
    let text = scalar(&args.first().cloned().unwrap_or(Value::None));
    let replacement = scalar(&args.get(2).cloned().unwrap_or(Value::None));
    match args.get(1) {
        Some(Value::Regex(regex)) => Value::Str(regex.replace_all(&text, replacement.as_str()).into_owned()),
        Some(pattern) => Value::Str(text.replace(&scalar(pattern), &replacement)),
        None => Value::Str(text),
    }
}

fn contains_value(haystack: &Value, needle: &Value) -> Value {
    match haystack {
        Value::List(items) => Value::Bool(items.iter().any(|item| values_equal(item, needle))),
        _ => {
            let haystack = scalar(haystack);
            match needle {
                Value::Regex(regex) => Value::Bool(regex.is_match(&haystack)),
                other => Value::Bool(haystack.contains(&scalar(other))),
            }
        }
    }
}

fn extremum(args: &[Value], minimum: bool) -> Value {
    let candidates: Vec<&Value> = match args.first() {
        Some(Value::List(items)) => items.iter().collect(),
        _ => args.iter().collect(),
    };
    let mut best: Option<&Value> = None;
    for candidate in candidates {
        match best {
            None => best = Some(candidate),
            Some(current) => {
                let ordering = compare_values(candidate, current);
                let replace = match ordering {
                    Some(std::cmp::Ordering::Less) => minimum,
                    Some(std::cmp::Ordering::Greater) => !minimum,
                    _ => false,
                };
                if replace {
                    best = Some(candidate);
                }
            }
        }
    }
    best.cloned().unwrap_or(Value::None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn resolve(expr: &mut Expr, columns: &[&str]) {
        match expr {
            Expr::Ident(name) => {
                let index = columns
                    .iter()
                    .position(|column| column == name)
                    .unwrap_or_else(|| panic!("unknown column {name}"));
                *expr = Expr::Ref(ValueRef::Column(index));
            }
            Expr::List(items) => items.iter_mut().for_each(|item| resolve(item, columns)),
            Expr::Call { args, .. } => args.iter_mut().for_each(|arg| resolve(arg, columns)),
            Expr::Binary { lhs, rhs, .. } => {
                resolve(lhs, columns);
                resolve(rhs, columns);
            }
            Expr::Unary { operand, .. } => resolve(operand, columns),
            Expr::Index { target, index } => {
                resolve(target, columns);
                resolve(index, columns);
            }
            Expr::Slice { target, start, end } => {
                resolve(target, columns);
                if let Some(start) = start {
                    resolve(start, columns);
                }
                if let Some(end) = end {
                    resolve(end, columns);
                }
            }
            _ => {}
        }
    }

    fn evaluate(source: &str, columns: &[&str]) -> Value {
        let mut expr = parse(source).unwrap();
        resolve(&mut expr, columns);
        let cells = cells(columns);
        let ctx = EvalContext {
            cells: &cells,
            derived: &[],
        };
        eval(&expr, &ctx)
    }

    #[test]
    fn regex_and_split() {
        let value = evaluate(r#""v1.2.3".match(/\d+\.\d+/).split(".")"#, &[]);
        match value {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].scalar_string(","), "1");
                assert_eq!(items[1].scalar_string(","), "2");
            }
            other => panic!("expected a list, got {other:?}"),
        }
    }

    #[test]
    fn method_chain_and_or_coalesce() {
        let value = evaluate(
            r#"("v1.2.3".match(/\d+\.\d+/) or "").split(".")"#,
            &[],
        );
        assert_eq!(value.scalar_string(","), "1,2");
    }

    #[test]
    fn or_returns_first_truthy() {
        assert_eq!(evaluate(r#"null or "fallback""#, &[]).scalar_string(","), "fallback");
        assert_eq!(evaluate(r#""" or "fallback""#, &[]).scalar_string(","), "fallback");
        assert_eq!(evaluate(r#""kept" or "fallback""#, &[]).scalar_string(","), "kept");
    }

    #[test]
    fn arithmetic_and_concat() {
        assert_eq!(evaluate("1 + 2 * 3", &[]).scalar_string(","), "7");
        assert_eq!(evaluate(r#""a" ++ "b""#, &[]).scalar_string(","), "ab");
        assert_eq!(evaluate("2 ** 3 ** 2", &[]).scalar_string(","), "512");
    }

    #[test]
    fn indexing_and_slicing() {
        assert_eq!(evaluate(r#""abc"[1]"#, &[]).scalar_string(","), "b");
        assert_eq!(evaluate(r#""abcdef"[1:3]"#, &[]).scalar_string(","), "bc");
        assert_eq!(evaluate("[10, 20, 30][-1]", &[]).scalar_string(","), "30");
    }

    #[test]
    fn pipe_threads_value() {
        assert_eq!(
            evaluate(r#""a,b,c" | split(",") | join("-")"#, &[]).scalar_string(","),
            "a-b-c"
        );
    }

    #[test]
    fn derive_parses_named_tuples() {
        let defs = parse_derive(
            r#"row.match(/\d+\.\d+/) or "" as version, (row.match(/\d+\.\d+/) or "").split(".") as (major, minor)"#,
        )
        .unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].names, vec!["version"]);
        assert_eq!(defs[1].names, vec!["major", "minor"]);
    }

    #[test]
    fn regex_vs_division() {
        assert_eq!(evaluate("6 / 2", &[]).scalar_string(","), "3");
        assert_eq!(evaluate(r#""a".match(/a/)"#, &[]).scalar_string(","), "a");
    }
}
