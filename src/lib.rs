//! FerroPHP — a from-scratch, memory-safe PHP engine written in Rust.
//!
//! **v3.** Building on v2 (the expression engine), this adds control flow:
//!   * `if` / `elseif` / `else` (incl. `else if`)
//!   * `while` and `do … while`
//!   * `break` / `continue` (with optional level)
//!   * `//`, `#`, and `/* … */` comments
//!
//! Control flow is implemented over the streaming interpreter via two ideas:
//!   * a [`Flow`] signal returned by every statement (Normal/Break/Continue)
//!   * a `live` flag — when false, statements are *parsed but not executed*
//!     (no output, no assignment, no arithmetic errors). Untaken branches and
//!     skipped loop bodies run with `live = false`.
//!
//! `for`/`foreach`/`switch` (and `++`/`--`) are the next rung. Anything
//! unsupported returns an [`EngineError`] so the matching `.phpt` fails honestly.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;

#[derive(Debug)]
pub struct EngineError(pub String);

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EngineError {}

type R<T> = Result<T, EngineError>;

/// Control-flow signal bubbled up from statement execution.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
}

/// A PHP value — the engine's "zval". Scalars only, so far.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// Convert to the exact string PHP would print for this value.
    pub fn to_php_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(true) => "1".to_string(),
            Value::Bool(false) => String::new(),
            Value::Int(n) => n.to_string(),
            Value::Float(x) => format_php_float(*x),
            Value::Str(s) => s.clone(),
        }
    }
}

// ---- value coercion helpers (PHP type juggling) ----------------------------

enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn as_f64(&self) -> f64 {
        match self {
            Num::I(n) => *n as f64,
            Num::F(x) => *x,
        }
    }
}

fn to_bool(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Float(x) => *x != 0.0,
        Value::Str(s) => !(s.is_empty() || s == "0"),
    }
}

fn is_numeric_str(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    if t.parse::<i64>().is_ok() {
        return true;
    }
    matches!(t.parse::<f64>(), Ok(x) if x.is_finite())
}

fn to_num(v: &Value) -> Num {
    match v {
        Value::Int(n) => Num::I(*n),
        Value::Float(x) => Num::F(*x),
        Value::Bool(b) => Num::I(*b as i64),
        Value::Null => Num::I(0),
        Value::Str(s) => {
            let t = s.trim();
            if let Ok(n) = t.parse::<i64>() {
                Num::I(n)
            } else if let Ok(x) = t.parse::<f64>() {
                Num::F(x)
            } else {
                Num::I(0)
            }
        }
    }
}

fn to_f64(v: &Value) -> f64 {
    to_num(v).as_f64()
}

fn to_long(v: &Value) -> i64 {
    match to_num(v) {
        Num::I(n) => n,
        Num::F(x) => x as i64,
    }
}

fn negate(v: &Value) -> Value {
    match to_num(v) {
        Num::I(n) => Value::Int(n.wrapping_neg()),
        Num::F(x) => Value::Float(-x),
    }
}

fn numeric(v: &Value) -> Value {
    match to_num(v) {
        Num::I(n) => Value::Int(n),
        Num::F(x) => Value::Float(x),
    }
}

fn strict_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        _ => false,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(_), _) | (_, Bool(_)) | (Null, _) | (_, Null) => to_bool(a) == to_bool(b),
        (Str(x), Str(y)) => {
            if is_numeric_str(x) && is_numeric_str(y) {
                to_f64(a) == to_f64(b)
            } else {
                x == y
            }
        }
        (Int(_) | Float(_), Str(s)) | (Str(s), Int(_) | Float(_)) => {
            if is_numeric_str(s) {
                to_f64(a) == to_f64(b)
            } else {
                a.to_php_string() == b.to_php_string()
            }
        }
        _ => to_f64(a) == to_f64(b),
    }
}

fn compare(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    match (a, b) {
        (Str(x), Str(y)) if !(is_numeric_str(x) && is_numeric_str(y)) => x.cmp(y),
        _ => to_f64(a).partial_cmp(&to_f64(b)).unwrap_or(Ordering::Equal),
    }
}

fn ipow(mut base: i64, mut exp: i64) -> Option<i64> {
    let mut result: i64 = 1;
    while exp > 0 {
        if exp & 1 == 1 {
            result = result.checked_mul(base)?;
        }
        exp >>= 1;
        if exp > 0 {
            base = base.checked_mul(base)?;
        }
    }
    Some(result)
}

fn format_php_float(x: f64) -> String {
    if x.is_nan() {
        return "NAN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "INF".into() } else { "-INF".into() };
    }
    if x == x.trunc() && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let mut s = format!("{x:.14}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

// ---- engine ----------------------------------------------------------------

/// Execute PHP `source` and return everything it would have printed to stdout.
pub fn run(source: &str) -> R<String> {
    let mut engine = Engine {
        src: source.chars().collect(),
        pos: 0,
        out: String::new(),
        vars: HashMap::new(),
        live: true,
        steps: 0,
    };
    engine.program()?;
    Ok(engine.out)
}

struct Engine {
    src: Vec<char>,
    pos: usize,
    out: String,
    vars: HashMap<String, Value>,
    /// When false, statements are parsed but not executed (skipped branches).
    live: bool,
    /// Hard budget on interpreter steps — a backstop against infinite loops
    /// (`catch_unwind` stops panics, not hangs).
    steps: u64,
}

const POW_PREC: u8 = 8;
const LOOP_CAP: u64 = 10_000_000;
/// Max interpreter steps per test. A legit program won't hit this; a runaway
/// loop will, and gets turned into an error instead of hanging the whole run.
const STEP_LIMIT: u64 = 5_000_000;

impl Engine {
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.src.get(self.pos + i).copied() == Some(c))
    }

    /// Skip whitespace AND comments (`//`, `#`, `/* */`).
    fn skip_ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.pos += 1,
                Some('/') if self.peek_at(1) == Some('/') => self.line_comment(),
                Some('/') if self.peek_at(1) == Some('*') => self.block_comment(),
                Some('#') if self.peek_at(1) != Some('[') => self.line_comment(),
                _ => break,
            }
        }
    }

    fn line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' || self.starts_with("?>") {
                break;
            }
            self.pos += 1;
        }
    }

    fn block_comment(&mut self) {
        self.pos += 2; // consume /*
        while self.pos < self.src.len() {
            if self.starts_with("*/") {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }

    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > STEP_LIMIT {
            Err(EngineError("step limit exceeded (possible infinite loop)".into()))
        } else {
            Ok(())
        }
    }

    fn program(&mut self) -> R<()> {
        while self.pos < self.src.len() {
            if self.starts_with("<?php") {
                self.pos += 5;
                self.php_body()?;
            } else {
                self.out.push(self.src[self.pos]);
                self.pos += 1;
            }
        }
        Ok(())
    }

    fn php_body(&mut self) -> R<()> {
        loop {
            self.skip_ws();
            if self.pos >= self.src.len() {
                return Ok(());
            }
            if self.starts_with("?>") {
                self.pos += 2;
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                return Ok(());
            }
            self.statement()?; // top-level break/continue is invalid PHP; ignore the Flow
        }
    }

    fn statement(&mut self) -> R<Flow> {
        self.tick()?;
        self.skip_ws();
        match self.peek() {
            None => return Ok(Flow::Normal),
            Some(';') => {
                self.pos += 1;
                return Ok(Flow::Normal);
            }
            Some('{') => return self.block(),
            Some('$') => return self.variable_statement(),
            _ => {}
        }
        if self.starts_with("?>") {
            // php_body consumes `?>` itself; reaching here means it's inside a
            // block/branch (interleaved HTML), which we don't support yet.
            return Err(EngineError("`?>` inside a block is not supported yet".into()));
        }
        if let Some(word) = self.try_identifier() {
            return match word.to_ascii_lowercase().as_str() {
                "echo" => self.echo_statement(),
                "if" => self.if_statement(),
                "while" => self.while_statement(),
                "do" => self.do_while_statement(),
                "break" => self.break_continue(true),
                "continue" => self.break_continue(false),
                _ => Err(EngineError(format!(
                    "v3 does not support the `{word}` statement yet"
                ))),
            };
        }
        Err(EngineError(format!(
            "v3 cannot parse statement near {:?}",
            self.snippet()
        )))
    }

    /// Assignment (`$x = …;`) or a bare expression statement starting with `$`.
    fn variable_statement(&mut self) -> R<Flow> {
        let save = self.pos;
        let name = self.parse_variable_name()?;
        self.skip_ws();
        if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
            self.pos += 1;
            self.skip_ws();
            let value = self.expression()?;
            if self.live {
                self.vars.insert(name, value);
            }
            self.end_statement()?;
            return Ok(Flow::Normal);
        }
        self.pos = save;
        let _ = self.expression()?;
        self.end_statement()?;
        Ok(Flow::Normal)
    }

    fn end_statement(&mut self) -> R<()> {
        self.skip_ws();
        match self.peek() {
            Some(';') => {
                self.pos += 1;
                Ok(())
            }
            _ if self.starts_with("?>") => Ok(()),
            other => Err(EngineError(format!("expected `;`, found {other:?}"))),
        }
    }

    fn echo_statement(&mut self) -> R<Flow> {
        loop {
            let value = self.expression()?;
            if self.live {
                self.out.push_str(&value.to_php_string());
            }
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                self.skip_ws();
                continue;
            }
            break;
        }
        self.end_statement()?;
        Ok(Flow::Normal)
    }

    fn break_continue(&mut self, is_break: bool) -> R<Flow> {
        self.skip_ws();
        let mut level = 1u32;
        if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            level = to_long(&self.number()).max(1) as u32;
        }
        self.end_statement()?;
        if self.live {
            Ok(if is_break {
                Flow::Break(level)
            } else {
                Flow::Continue(level)
            })
        } else {
            Ok(Flow::Normal)
        }
    }

    // ---- control flow ------------------------------------------------------

    /// `( expr )`
    fn paren_expr(&mut self) -> R<Value> {
        self.skip_ws();
        if self.peek() != Some('(') {
            return Err(EngineError("expected `(`".into()));
        }
        self.pos += 1;
        let v = self.expression()?;
        self.skip_ws();
        if self.peek() != Some(')') {
            return Err(EngineError("expected `)`".into()));
        }
        self.pos += 1;
        Ok(v)
    }

    /// Execute either a `{ … }` block or a single statement.
    fn block_or_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.block()
        } else {
            self.statement()
        }
    }

    /// Execute a `{ … }` block. On a non-Normal Flow, skips the rest of the
    /// block (so the cursor lands past the closing `}`) and propagates it.
    fn block(&mut self) -> R<Flow> {
        self.pos += 1; // consume {
        loop {
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    return Ok(Flow::Normal);
                }
                None => return Err(EngineError("unterminated `{` block".into())),
                _ => {
                    let f = self.statement()?;
                    if f != Flow::Normal {
                        self.skip_to_block_end()?;
                        return Ok(f);
                    }
                }
            }
        }
    }

    /// From inside a block (one `{` already open), consume up to and including
    /// its matching `}`, respecting nested braces, strings, and comments.
    fn skip_to_block_end(&mut self) -> R<()> {
        let mut depth = 1;
        while depth > 0 {
            self.skip_ws();
            match self.peek() {
                None => return Err(EngineError("unterminated `{` block".into())),
                Some('{') => {
                    depth += 1;
                    self.pos += 1;
                }
                Some('}') => {
                    depth -= 1;
                    self.pos += 1;
                }
                Some('\'') => {
                    let _ = self.single_quoted()?;
                }
                Some('"') => {
                    let _ = self.double_quoted()?;
                }
                Some(_) => self.pos += 1,
            }
        }
        Ok(())
    }

    /// Run a branch (block or statement) with `live` ANDed with `take`.
    fn exec_branch(&mut self, take: bool) -> R<Flow> {
        let prev = self.live;
        self.live = prev && take;
        let f = self.block_or_statement()?;
        self.live = prev;
        Ok(f)
    }

    fn if_statement(&mut self) -> R<Flow> {
        let cond = self.paren_expr()?;
        let take = self.live && to_bool(&cond);
        let mut flow = self.exec_branch(take)?;
        let mut done = take;
        loop {
            let save = self.pos;
            self.skip_ws();
            let kw = self.try_identifier().map(|s| s.to_ascii_lowercase());
            match kw.as_deref() {
                Some("elseif") => {
                    let c = self.paren_expr()?;
                    let t = self.live && !done && to_bool(&c);
                    let f = self.exec_branch(t)?;
                    if t {
                        flow = f;
                    }
                    done |= t;
                }
                Some("else") => {
                    let save2 = self.pos;
                    self.skip_ws();
                    let next = self.try_identifier().map(|s| s.to_ascii_lowercase());
                    if next.as_deref() == Some("if") {
                        let c = self.paren_expr()?;
                        let t = self.live && !done && to_bool(&c);
                        let f = self.exec_branch(t)?;
                        if t {
                            flow = f;
                        }
                        done |= t;
                    } else {
                        self.pos = save2;
                        let t = self.live && !done;
                        let f = self.exec_branch(t)?;
                        if t {
                            flow = f;
                        }
                        return Ok(flow);
                    }
                }
                _ => {
                    self.pos = save;
                    return Ok(flow);
                }
            }
        }
    }

    fn while_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        if self.peek() != Some('(') {
            return Err(EngineError("expected `(` after `while`".into()));
        }
        let cond_start = self.pos;
        let mut guard = 0u64;
        loop {
            self.tick()?;
            self.pos = cond_start;
            let cond = self.paren_expr()?;
            if self.live && to_bool(&cond) {
                guard += 1;
                if guard > LOOP_CAP {
                    return Err(EngineError("while loop exceeded iteration cap".into()));
                }
                match self.block_or_statement()? {
                    Flow::Break(n) => {
                        return Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal });
                    }
                    Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                    _ => {}
                }
            } else {
                // skip the body once so the cursor lands after the loop
                let prev = self.live;
                self.live = false;
                let _ = self.block_or_statement()?;
                self.live = prev;
                return Ok(Flow::Normal);
            }
        }
    }

    fn do_while_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        let body_start = self.pos;
        let mut guard = 0u64;
        loop {
            self.tick()?;
            self.pos = body_start;
            let f = self.block_or_statement()?;
            self.skip_ws();
            let kw = self.try_identifier().map(|s| s.to_ascii_lowercase());
            if kw.as_deref() != Some("while") {
                return Err(EngineError("expected `while` after `do` block".into()));
            }
            let cond = self.paren_expr()?;
            self.end_statement()?;
            let again = match f {
                Flow::Break(n) => {
                    if n > 1 {
                        return Ok(Flow::Break(n - 1));
                    }
                    false
                }
                Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                _ => self.live && to_bool(&cond),
            };
            if again {
                guard += 1;
                if guard > LOOP_CAP {
                    return Err(EngineError("do-while exceeded iteration cap".into()));
                }
                continue;
            }
            return Ok(Flow::Normal);
        }
    }

    // ---- expression parsing (precedence climbing) --------------------------

    fn expression(&mut self) -> R<Value> {
        self.parse_binary(0)
    }

    fn peek_operator(&self) -> Option<(&'static str, u8, bool)> {
        for &(s, p, r) in &[("===", 3, false), ("!==", 3, false), ("<=>", 3, false)] {
            if self.starts_with(s) {
                return Some((s, p, r));
            }
        }
        for &(s, p, r) in &[
            ("==", 3, false),
            ("!=", 3, false),
            ("<=", 4, false),
            (">=", 4, false),
            ("&&", 2, false),
            ("||", 1, false),
            ("**", POW_PREC, true),
        ] {
            if self.starts_with(s) {
                return Some((s, p, r));
            }
        }
        for &(s, p, r) in &[
            (".", 5, false),
            ("+", 6, false),
            ("-", 6, false),
            ("*", 7, false),
            ("/", 7, false),
            ("%", 7, false),
            ("<", 4, false),
            (">", 4, false),
        ] {
            if self.starts_with(s) {
                return Some((s, p, r));
            }
        }
        None
    }

    fn parse_binary(&mut self, min_prec: u8) -> R<Value> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            let (op, prec, right_assoc) = match self.peek_operator() {
                Some(x) => x,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.tick()?;
            self.pos += op.len();
            self.skip_ws();
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_binary(next_min)?;
            left = self.apply_binary(op, left, right)?;
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> R<Value> {
        self.skip_ws();
        match self.peek() {
            Some('!') => {
                self.pos += 1;
                let v = self.parse_unary()?;
                Ok(Value::Bool(!to_bool(&v)))
            }
            Some('-') => {
                self.pos += 1;
                let v = self.parse_binary(POW_PREC)?;
                Ok(negate(&v))
            }
            Some('+') => {
                self.pos += 1;
                let v = self.parse_binary(POW_PREC)?;
                Ok(numeric(&v))
            }
            _ => self.primary(),
        }
    }

    fn apply_binary(&self, op: &str, l: Value, r: Value) -> R<Value> {
        use Value::*;
        if !self.live {
            return Ok(Null); // skipped code: parse only, never compute or error
        }
        let v = match op {
            "." => Str(format!("{}{}", l.to_php_string(), r.to_php_string())),
            "+" | "-" | "*" => arith(op, &l, &r),
            "/" => {
                if to_f64(&r) == 0.0 {
                    return Err(EngineError("Division by zero".into()));
                }
                if let (Num::I(x), Num::I(y)) = (to_num(&l), to_num(&r)) {
                    if y != 0 && x % y == 0 {
                        return Ok(Int(x / y));
                    }
                }
                Float(to_f64(&l) / to_f64(&r))
            }
            "%" => {
                let y = to_long(&r);
                if y == 0 {
                    return Err(EngineError("Modulo by zero".into()));
                }
                Int(to_long(&l) % y)
            }
            "**" => match (to_num(&l), to_num(&r)) {
                (Num::I(x), Num::I(y)) if y >= 0 => match ipow(x, y) {
                    Some(n) => Int(n),
                    None => Float((x as f64).powf(y as f64)),
                },
                _ => Float(to_f64(&l).powf(to_f64(&r))),
            },
            "==" => Bool(loose_eq(&l, &r)),
            "!=" => Bool(!loose_eq(&l, &r)),
            "===" => Bool(strict_eq(&l, &r)),
            "!==" => Bool(!strict_eq(&l, &r)),
            "<" => Bool(compare(&l, &r) == Ordering::Less),
            ">" => Bool(compare(&l, &r) == Ordering::Greater),
            "<=" => Bool(compare(&l, &r) != Ordering::Greater),
            ">=" => Bool(compare(&l, &r) != Ordering::Less),
            "<=>" => Int(match compare(&l, &r) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }),
            "&&" => Bool(to_bool(&l) && to_bool(&r)),
            "||" => Bool(to_bool(&l) || to_bool(&r)),
            _ => return Err(EngineError(format!("unknown operator `{op}`"))),
        };
        Ok(v)
    }

    fn primary(&mut self) -> R<Value> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let v = self.expression()?;
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.pos += 1;
                    Ok(v)
                } else {
                    Err(EngineError("expected `)`".into()))
                }
            }
            Some('"') => Ok(Value::Str(self.double_quoted()?)),
            Some('\'') => Ok(Value::Str(self.single_quoted()?)),
            Some('$') => self.variable(),
            Some(c) if c.is_ascii_digit() => Ok(self.number()),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let id = self.try_identifier().unwrap();
                match id.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => Err(EngineError(format!(
                        "v3 cannot use identifier `{id}` in an expression yet"
                    ))),
                }
            }
            other => Err(EngineError(format!(
                "v3 cannot parse expression near {other:?} ({:?})",
                self.snippet()
            ))),
        }
    }

    fn variable(&mut self) -> R<Value> {
        let name = self.parse_variable_name()?;
        Ok(self.vars.get(&name).cloned().unwrap_or(Value::Null))
    }

    fn parse_variable_name(&mut self) -> R<String> {
        debug_assert_eq!(self.peek(), Some('$'));
        self.pos += 1;
        if !matches!(self.peek(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
            return Err(EngineError("expected a variable name after `$`".into()));
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        Ok(self.src[start..self.pos].iter().collect())
    }

    fn number(&mut self) -> Value {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                is_float = true;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }
        let text: String = self.src[start..self.pos].iter().collect();
        if is_float {
            Value::Float(text.parse::<f64>().unwrap_or(0.0))
        } else {
            match text.parse::<i64>() {
                Ok(n) => Value::Int(n),
                Err(_) => Value::Float(text.parse::<f64>().unwrap_or(0.0)),
            }
        }
    }

    fn single_quoted(&mut self) -> R<String> {
        self.pos += 1;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            match c {
                '\\' => match self.peek_at(1) {
                    Some('\'') => {
                        s.push('\'');
                        self.pos += 2;
                    }
                    Some('\\') => {
                        s.push('\\');
                        self.pos += 2;
                    }
                    _ => {
                        s.push('\\');
                        self.pos += 1;
                    }
                },
                '\'' => {
                    self.pos += 1;
                    return Ok(s);
                }
                _ => {
                    s.push(c);
                    self.pos += 1;
                }
            }
        }
        Err(EngineError("unterminated single-quoted string".into()))
    }

    fn double_quoted(&mut self) -> R<String> {
        self.pos += 1;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c == '\\' {
                let (ch, adv) = match self.peek_at(1) {
                    Some('n') => ('\n', 2),
                    Some('t') => ('\t', 2),
                    Some('r') => ('\r', 2),
                    Some('"') => ('"', 2),
                    Some('\\') => ('\\', 2),
                    Some('$') => ('$', 2),
                    _ => ('\\', 1),
                };
                s.push(ch);
                self.pos += adv;
                continue;
            }
            if c == '"' {
                self.pos += 1;
                return Ok(s);
            }
            if c == '$' && matches!(self.peek_at(1), Some(d) if d.is_ascii_alphabetic() || d == '_') {
                let name = self.parse_variable_name()?;
                let value = self.vars.get(&name).cloned().unwrap_or(Value::Null);
                s.push_str(&value.to_php_string());
                continue;
            }
            s.push(c);
            self.pos += 1;
        }
        Err(EngineError("unterminated double-quoted string".into()))
    }

    fn try_identifier(&mut self) -> Option<String> {
        let first = self.peek()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        Some(self.src[start..self.pos].iter().collect())
    }

    fn snippet(&self) -> String {
        let end = (self.pos + 16).min(self.src.len());
        self.src[self.pos..end].iter().collect()
    }
}

fn arith(op: &str, l: &Value, r: &Value) -> Value {
    match (to_num(l), to_num(r)) {
        (Num::I(x), Num::I(y)) => {
            let checked = match op {
                "+" => x.checked_add(y),
                "-" => x.checked_sub(y),
                "*" => x.checked_mul(y),
                _ => None,
            };
            match checked {
                Some(n) => Value::Int(n),
                None => Value::Float(apply_f(op, x as f64, y as f64)),
            }
        }
        (a, b) => Value::Float(apply_f(op, a.as_f64(), b.as_f64())),
    }
}

fn apply_f(op: &str, x: f64, y: f64) -> f64 {
    match op {
        "+" => x + y,
        "-" => x - y,
        "*" => x * y,
        _ => f64::NAN,
    }
}
