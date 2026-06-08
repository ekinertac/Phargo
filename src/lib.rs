//! FerroPHP — a from-scratch, memory-safe PHP engine written in Rust.
//!
//! **v3b.** On top of v3 (control flow), this adds:
//!   * assignment as an *expression* (`$a = $b = 3`) incl. compound
//!     `+= -= *= /= %= .= **=`
//!   * pre/post `++` / `--`
//!   * the `for (init; cond; step)` loop
//!
//! Execution model recap: a streaming interpreter with a [`Flow`] signal
//! (Normal/Break/Continue) and a `live` flag (false ⇒ parse-but-don't-execute,
//! used for untaken branches and skipped loop bodies). A per-test step budget
//! guards against infinite loops. `switch`/`foreach` and user functions are the
//! next rungs; anything unsupported returns an [`EngineError`] so the matching
//! `.phpt` fails honestly.

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
    Return,
}

#[derive(Clone)]
struct Param {
    name: String,
    /// Source position of the default-value expression, if the param has one.
    default: Option<usize>,
}

#[derive(Clone)]
struct FuncDef {
    params: Vec<Param>,
    /// Position of the body's opening `{`.
    body_start: usize,
}

/// A PHP array key (after normalization): integer or string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AKey {
    Int(i64),
    Str(String),
}

/// A PHP array: an insertion-ordered map. Linear lookup (corpus arrays are
/// small); correctness first.
#[derive(Clone, Debug, Default)]
pub struct PArray {
    entries: Vec<(AKey, Value)>,
    next_index: i64,
}

impl PArray {
    fn get(&self, k: &AKey) -> Option<&Value> {
        self.entries.iter().find(|(ek, _)| ek == k).map(|(_, v)| v)
    }
    fn get_mut(&mut self, k: &AKey) -> Option<&mut Value> {
        self.entries.iter_mut().find(|(ek, _)| ek == k).map(|(_, v)| v)
    }
    fn set(&mut self, k: AKey, v: Value) {
        if let AKey::Int(i) = &k {
            if *i >= self.next_index {
                self.next_index = *i + 1;
            }
        }
        if let Some(slot) = self.entries.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            self.entries.push((k, v));
        }
    }
    fn push(&mut self, v: Value) {
        let i = self.next_index;
        self.entries.push((AKey::Int(i), v));
        self.next_index = i + 1;
    }
}

/// A PHP value — the engine's "zval".
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(PArray),
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
            Value::Array(_) => "Array".to_string(), // PHP emits a notice and "Array"
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
        Value::Array(a) => !a.entries.is_empty(),
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
        Value::Array(a) => Num::I(if a.entries.is_empty() { 0 } else { 1 }),
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
        (Array(x), Array(y)) => {
            x.entries.len() == y.entries.len()
                && x.entries
                    .iter()
                    .zip(&y.entries)
                    .all(|((ka, va), (kb, vb))| ka == kb && strict_eq(va, vb))
        }
        _ => false,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(_), _) | (_, Bool(_)) | (Null, _) | (_, Null) => to_bool(a) == to_bool(b),
        (Array(x), Array(y)) => {
            x.entries.len() == y.entries.len()
                && x.entries
                    .iter()
                    .all(|(k, v)| matches!(y.get(k), Some(w) if loose_eq(v, w)))
        }
        (Array(_), _) | (_, Array(_)) => false,
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
        funcs: HashMap::new(),
        return_val: None,
        call_depth: 0,
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
    /// User-defined functions, keyed by lowercased name (PHP is case-insensitive).
    funcs: HashMap<String, FuncDef>,
    /// Value stashed by `return`, picked up by the active call frame.
    return_val: Option<Value>,
    /// Current function-call nesting depth (guards against stack overflow).
    call_depth: usize,
}

const POW_PREC: u8 = 8;
const LOOP_CAP: u64 = 10_000_000;
/// Max interpreter steps per test. A legit program won't hit this; a runaway
/// loop will, and gets turned into an error instead of hanging the whole run.
/// Kept modest so the ~8k EXPECTF tests (now executed) stay fast in aggregate.
const STEP_LIMIT: u64 = 1_000_000;

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
            _ => {}
        }
        if self.starts_with("?>") {
            // php_body consumes `?>` itself; reaching here means it's inside a
            // block/branch (interleaved HTML), which we don't support yet.
            return Err(EngineError("`?>` inside a block is not supported yet".into()));
        }
        // Keyword statements — peek the word without committing.
        let save = self.pos;
        if let Some(word) = self.try_identifier() {
            match word.to_ascii_lowercase().as_str() {
                "echo" => return self.echo_statement(),
                "if" => return self.if_statement(),
                "while" => return self.while_statement(),
                "do" => return self.do_while_statement(),
                "for" => return self.for_statement(),
                "foreach" => return self.foreach_statement(),
                "function" => return self.function_decl(),
                "return" => return self.return_statement(),
                "break" => return self.break_continue(true),
                "continue" => return self.break_continue(false),
                _ => self.pos = save, // not a keyword: fall through to expression statement
            }
        }
        // Expression statement (`$x = …;`, `$i++;`, `(…);`, bare literals).
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

    fn paren_expr(&mut self) -> R<Value> {
        self.expect_char('(')?;
        let v = self.expression()?;
        self.expect_char(')')?;
        Ok(v)
    }

    fn expect_char(&mut self, c: char) -> R<()> {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(EngineError(format!("expected `{c}`")))
        }
    }

    fn block_or_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.block()
        } else {
            self.statement()
        }
    }

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
                    Flow::Return => return Ok(Flow::Return),
                    _ => {}
                }
            } else {
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
                Flow::Return => return Ok(Flow::Return),
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

    /// Evaluate a comma-separated expression list (for `for` init/step),
    /// stopping before `stop`. No-op for an empty clause.
    fn for_expr_list(&mut self, stop: char) -> R<()> {
        self.skip_ws();
        if self.peek() == Some(stop) {
            return Ok(());
        }
        loop {
            let _ = self.expression()?;
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                self.skip_ws();
                continue;
            }
            break;
        }
        Ok(())
    }

    fn for_statement(&mut self) -> R<Flow> {
        self.expect_char('(')?;
        self.for_expr_list(';')?; // init (run once, live)
        self.expect_char(';')?;
        let cond_start = self.pos;
        let mut guard = 0u64;
        loop {
            self.tick()?;
            self.pos = cond_start;
            self.skip_ws();
            let cond = if self.peek() == Some(';') {
                Value::Bool(true)
            } else {
                self.expression()?
            };
            self.expect_char(';')?;
            let step_start = self.pos;
            // locate the `)` by skipping the step clause without executing it
            let prev = self.live;
            self.live = false;
            self.for_expr_list(')')?;
            self.live = prev;
            self.expect_char(')')?;
            if self.live && to_bool(&cond) {
                guard += 1;
                if guard > LOOP_CAP {
                    return Err(EngineError("for loop exceeded iteration cap".into()));
                }
                match self.block_or_statement()? {
                    Flow::Break(n) => {
                        return Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal });
                    }
                    Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                    Flow::Return => return Ok(Flow::Return),
                    _ => {}
                }
                // run the step (live), then loop (which resets to cond_start)
                self.pos = step_start;
                self.for_expr_list(')')?;
            } else {
                let prev = self.live;
                self.live = false;
                let _ = self.block_or_statement()?;
                self.live = prev;
                return Ok(Flow::Normal);
            }
        }
    }

    fn foreach_statement(&mut self) -> R<Flow> {
        self.expect_char('(')?;
        let iterable = self.expression()?;
        self.skip_ws();
        let as_kw = self.try_identifier().map(|s| s.to_ascii_lowercase());
        if as_kw.as_deref() != Some("as") {
            return Err(EngineError("expected `as` in foreach".into()));
        }
        self.skip_ws();
        if self.peek() != Some('$') {
            return Err(EngineError("expected variable in foreach".into()));
        }
        let v1 = self.parse_variable_name()?;
        self.skip_ws();
        let (key_var, val_var) = if self.starts_with("=>") {
            self.pos += 2;
            self.skip_ws();
            if self.peek() != Some('$') {
                return Err(EngineError("expected value variable in foreach".into()));
            }
            (Some(v1), self.parse_variable_name()?)
        } else {
            (None, v1)
        };
        self.expect_char(')')?;
        let body_start = self.pos;

        let entries: Vec<(AKey, Value)> = match &iterable {
            Value::Array(a) => a.entries.clone(),
            _ => Vec::new(),
        };

        // Empty/skipped: parse the body once (not live) to move past it.
        if !self.live || entries.is_empty() {
            let prev = self.live;
            self.live = false;
            let _ = self.block_or_statement()?;
            self.live = prev;
            return Ok(Flow::Normal);
        }

        for (k, v) in entries {
            self.tick()?;
            if let Some(kv) = &key_var {
                self.vars.insert(kv.clone(), akey_to_value(&k));
            }
            self.vars.insert(val_var.clone(), v);
            self.pos = body_start;
            match self.block_or_statement()? {
                Flow::Break(n) => {
                    return Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal });
                }
                Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                Flow::Return => return Ok(Flow::Return),
                _ => {}
            }
        }
        Ok(Flow::Normal)
    }

    // ---- expression parsing ------------------------------------------------

    fn expression(&mut self) -> R<Value> {
        self.parse_assignment()
    }

    /// Assignment is right-associative and lowest precedence: `$a = $b = expr`,
    /// plus compound `+= -= *= /= %= .= **=`. Falls through to binary parsing.
    fn parse_assignment(&mut self) -> R<Value> {
        if self.peek() == Some('$') {
            let save = self.pos;
            let name = self.parse_variable_name()?;
            // optional index chain → array-element lvalue ($a[i], $a[], $a[i][j])
            let mut indices: Vec<Option<Value>> = Vec::new();
            loop {
                let after = self.pos;
                self.skip_ws();
                if self.peek() == Some('[') {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(']') {
                        self.pos += 1;
                        indices.push(None);
                    } else {
                        let k = self.expression()?;
                        self.expect_char(']')?;
                        indices.push(Some(k));
                    }
                } else {
                    self.pos = after;
                    break;
                }
            }
            self.skip_ws();
            if let Some(aop) = self.peek_assign_op() {
                self.pos += aop.len();
                self.skip_ws();
                let rhs = self.parse_assignment()?;
                if indices.is_empty() {
                    let newval = if aop == "=" {
                        rhs
                    } else {
                        let cur = self.vars.get(&name).cloned().unwrap_or(Value::Null);
                        self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
                    };
                    if self.live {
                        self.vars.insert(name, newval.clone());
                    }
                    return Ok(newval);
                }
                return self.assign_indexed(name, indices, aop, rhs);
            }
            self.pos = save; // not an assignment
        }
        self.parse_ternary()
    }

    /// `cond ? a : b` and `cond ?: b`. Only the taken branch is executed (the
    /// other is parsed with `live = false`).
    fn parse_ternary(&mut self) -> R<Value> {
        let cond = self.parse_coalesce()?;
        self.skip_ws();
        if self.peek() == Some('?') && self.peek_at(1) != Some('?') {
            self.pos += 1;
            self.skip_ws();
            let take = self.live && to_bool(&cond);
            if self.peek() == Some(':') {
                self.pos += 1; // short ternary: cond ?: else
                self.skip_ws();
                let prev = self.live;
                self.live = prev && !take;
                let else_v = self.parse_assignment()?;
                self.live = prev;
                return Ok(if !self.live {
                    Value::Null
                } else if take {
                    cond
                } else {
                    else_v
                });
            }
            let prev = self.live;
            self.live = prev && take;
            let then_v = self.parse_assignment()?;
            self.live = prev;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(EngineError("expected `:` in ternary".into()));
            }
            self.pos += 1;
            self.skip_ws();
            let prev = self.live;
            self.live = prev && !take;
            let else_v = self.parse_assignment()?;
            self.live = prev;
            return Ok(if !self.live {
                Value::Null
            } else if take {
                then_v
            } else {
                else_v
            });
        }
        Ok(cond)
    }

    /// `a ?? b` — right-associative; `b` runs only if `a` is null.
    fn parse_coalesce(&mut self) -> R<Value> {
        let left = self.parse_binary(0)?;
        self.skip_ws();
        if self.starts_with("??") {
            self.pos += 2;
            self.skip_ws();
            let left_is_null = matches!(left, Value::Null);
            let prev = self.live;
            self.live = prev && left_is_null;
            let right = self.parse_coalesce()?;
            self.live = prev;
            return Ok(if !self.live {
                Value::Null
            } else if left_is_null {
                right
            } else {
                left
            });
        }
        Ok(left)
    }

    fn assign_indexed(
        &mut self,
        name: String,
        indices: Vec<Option<Value>>,
        aop: &str,
        rhs: Value,
    ) -> R<Value> {
        if !self.live {
            return Ok(rhs);
        }
        let newval = if aop == "=" {
            rhs
        } else {
            let cur = self.read_indexed(&name, &indices);
            self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
        };
        let slot = self.vars.entry(name).or_insert(Value::Null);
        set_path(slot, &indices, newval.clone());
        Ok(newval)
    }

    fn read_indexed(&self, name: &str, indices: &[Option<Value>]) -> Value {
        let mut cur = self.vars.get(name).cloned().unwrap_or(Value::Null);
        for idx in indices {
            let key = match idx {
                None => return Value::Null,
                Some(v) => key_from_value(v),
            };
            cur = match &cur {
                Value::Array(a) => a.get(&key).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
        }
        cur
    }

    fn peek_assign_op(&self) -> Option<&'static str> {
        for s in ["**=", "+=", "-=", "*=", "/=", "%=", ".="] {
            if self.starts_with(s) {
                return Some(s);
            }
        }
        // plain `=`, but not `==`, `===`, or `=>`
        if self.starts_with("=") && self.peek_at(1) != Some('=') && self.peek_at(1) != Some('>') {
            Some("=")
        } else {
            None
        }
    }

    fn inc_dec(&self, v: &Value, inc: bool) -> Value {
        if inc {
            match v {
                Value::Null => Value::Int(1),
                _ => arith("+", v, &Value::Int(1)),
            }
        } else {
            match v {
                Value::Null => Value::Null, // PHP: --null stays null
                _ => arith("-", v, &Value::Int(1)),
            }
        }
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
            // don't mistake compound-assign / `++` etc. for a binary operator
            if self.peek_assign_op().is_some() {
                break;
            }
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
        if self.starts_with("++") || self.starts_with("--") {
            let inc = self.starts_with("++");
            self.pos += 2;
            self.skip_ws();
            if self.peek() != Some('$') {
                return Err(EngineError("`++`/`--` requires a variable".into()));
            }
            let name = self.parse_variable_name()?;
            let cur = self.vars.get(&name).cloned().unwrap_or(Value::Null);
            let nv = self.inc_dec(&cur, inc);
            if self.live {
                self.vars.insert(name, nv.clone());
            }
            return Ok(nv);
        }
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

    /// Parse a `( arg, arg, … )` argument list, evaluating each argument.
    fn parse_args(&mut self) -> R<Vec<Value>> {
        self.expect_char('(')?;
        let mut args = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(args);
        }
        loop {
            args.push(self.expression()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(EngineError(format!("expected `,` or `)`, found {other:?}")))
                }
            }
        }
        Ok(args)
    }

    /// Dispatch a built-in function. User-defined functions come in v4b.
    fn call_function(&mut self, name: &str, args: Vec<Value>) -> R<Value> {
        if !self.live {
            return Ok(Value::Null);
        }
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        let v = match name.to_ascii_lowercase().as_str() {
            "var_dump" => {
                let mut s = String::new();
                for a in &args {
                    s.push_str(&var_dump_str(a, 0));
                }
                self.out.push_str(&s);
                Value::Null
            }
            "print_r" => {
                let s = print_r_str(&arg(0));
                if to_bool(&arg(1)) {
                    Value::Str(s)
                } else {
                    self.out.push_str(&s);
                    Value::Bool(true)
                }
            }
            "var_export" => {
                let s = var_export_str(&arg(0));
                if to_bool(&arg(1)) {
                    Value::Str(s)
                } else {
                    self.out.push_str(&s);
                    Value::Null
                }
            }
            "gettype" => Value::Str(php_type_name(&arg(0)).to_string()),
            "strlen" => Value::Int(arg(0).to_php_string().len() as i64),
            "strtoupper" => Value::Str(arg(0).to_php_string().to_ascii_uppercase()),
            "strtolower" => Value::Str(arg(0).to_php_string().to_ascii_lowercase()),
            "trim" => Value::Str(arg(0).to_php_string().trim().to_string()),
            "ltrim" => Value::Str(arg(0).to_php_string().trim_start().to_string()),
            "rtrim" | "chop" => Value::Str(arg(0).to_php_string().trim_end().to_string()),
            "ucfirst" => Value::Str(ucfirst(&arg(0).to_php_string())),
            "lcfirst" => Value::Str(lcfirst(&arg(0).to_php_string())),
            "strrev" => Value::Str(arg(0).to_php_string().chars().rev().collect()),
            "str_repeat" => {
                let n = to_long(&arg(1)).clamp(0, 10_000_000) as usize;
                Value::Str(arg(0).to_php_string().repeat(n))
            }
            "ord" => Value::Int(arg(0).to_php_string().bytes().next().unwrap_or(0) as i64),
            "chr" => Value::Str((to_long(&arg(0)).rem_euclid(256) as u8 as char).to_string()),
            "strpos" => {
                let (h, n) = (arg(0).to_php_string(), arg(1).to_php_string());
                match h.find(&n) {
                    Some(i) => Value::Int(i as i64),
                    None => Value::Bool(false),
                }
            }
            "sprintf" => {
                if args.is_empty() {
                    Value::Str(String::new())
                } else {
                    Value::Str(php_sprintf(&args[0].to_php_string(), &args[1..]))
                }
            }
            "printf" => {
                if args.is_empty() {
                    Value::Int(0)
                } else {
                    let out = php_sprintf(&args[0].to_php_string(), &args[1..]);
                    let n = out.len() as i64;
                    self.out.push_str(&out);
                    Value::Int(n)
                }
            }
            "substr" => {
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let total = chars.len() as i64;
                let mut start = to_long(&arg(1));
                if start < 0 {
                    start = (total + start).max(0);
                } else if start > total {
                    start = total;
                }
                let start = start as usize;
                let end = if args.len() >= 3 && !matches!(arg(2), Value::Null) {
                    let l = to_long(&arg(2));
                    if l < 0 {
                        (total + l).max(start as i64) as usize
                    } else {
                        (start + l as usize).min(chars.len())
                    }
                } else {
                    chars.len()
                };
                let end = end.clamp(start, chars.len());
                Value::Str(chars[start..end].iter().collect())
            }
            "str_replace" => {
                let search = arg(0).to_php_string();
                let replace = arg(1).to_php_string();
                let subject = arg(2).to_php_string();
                if search.is_empty() {
                    Value::Str(subject)
                } else {
                    Value::Str(subject.replace(&search, &replace))
                }
            }
            "abs" => match to_num(&arg(0)) {
                Num::I(n) => Value::Int(n.wrapping_abs()),
                Num::F(x) => Value::Float(x.abs()),
            },
            "floor" => Value::Float(to_f64(&arg(0)).floor()),
            "ceil" => Value::Float(to_f64(&arg(0)).ceil()),
            "sqrt" => Value::Float(to_f64(&arg(0)).sqrt()),
            "round" => {
                let f = 10f64.powi(to_long(&arg(1)) as i32);
                Value::Float((to_f64(&arg(0)) * f).round() / f)
            }
            "intdiv" => {
                let b = to_long(&arg(1));
                if b == 0 {
                    return Err(EngineError("Division by zero".into()));
                }
                Value::Int(to_long(&arg(0)) / b)
            }
            "pow" => self.apply_binary("**", arg(0), arg(1))?,
            "max" => {
                if compare(&arg(0), &arg(1)) == Ordering::Less {
                    arg(1)
                } else {
                    arg(0)
                }
            }
            "min" => {
                if compare(&arg(0), &arg(1)) == Ordering::Greater {
                    arg(1)
                } else {
                    arg(0)
                }
            }
            "intval" => Value::Int(to_long(&arg(0))),
            "floatval" | "doubleval" => Value::Float(to_f64(&arg(0))),
            "strval" => Value::Str(arg(0).to_php_string()),
            "boolval" => Value::Bool(to_bool(&arg(0))),
            "is_int" | "is_integer" | "is_long" => Value::Bool(matches!(arg(0), Value::Int(_))),
            "is_float" | "is_double" => Value::Bool(matches!(arg(0), Value::Float(_))),
            "is_string" => Value::Bool(matches!(arg(0), Value::Str(_))),
            "is_bool" => Value::Bool(matches!(arg(0), Value::Bool(_))),
            "is_null" => Value::Bool(matches!(arg(0), Value::Null)),
            "is_numeric" => Value::Bool(match arg(0) {
                Value::Int(_) | Value::Float(_) => true,
                Value::Str(s) => is_numeric_str(&s),
                _ => false,
            }),
            "is_scalar" => Value::Bool(matches!(
                arg(0),
                Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
            )),
            "is_array" => Value::Bool(matches!(arg(0), Value::Array(_))),
            "is_object" | "is_callable" => Value::Bool(false),
            "count" | "sizeof" => match arg(0) {
                Value::Array(a) => Value::Int(a.entries.len() as i64),
                _ => Value::Int(1),
            },
            "in_array" => match arg(1) {
                Value::Array(a) => {
                    let needle = arg(0);
                    Value::Bool(a.entries.iter().any(|(_, v)| loose_eq(&needle, v)))
                }
                _ => Value::Bool(false),
            },
            "array_keys" => match arg(0) {
                Value::Array(a) => {
                    let mut r = PArray::default();
                    for (k, _) in &a.entries {
                        r.push(akey_to_value(k));
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_values" => match arg(0) {
                Value::Array(a) => {
                    let mut r = PArray::default();
                    for (_, v) in &a.entries {
                        r.push(v.clone());
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_reverse" => match arg(0) {
                Value::Array(a) => {
                    let mut r = PArray::default();
                    for (_, v) in a.entries.iter().rev() {
                        r.push(v.clone());
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_sum" => match arg(0) {
                Value::Array(a) => {
                    let mut acc = Value::Int(0);
                    for (_, v) in &a.entries {
                        acc = arith("+", &acc, v);
                    }
                    acc
                }
                _ => Value::Int(0),
            },
            "array_merge" => {
                let mut r = PArray::default();
                for a in &args {
                    if let Value::Array(arr) = a {
                        for (k, v) in &arr.entries {
                            match k {
                                AKey::Int(_) => r.push(v.clone()),
                                AKey::Str(s) => r.set(AKey::Str(s.clone()), v.clone()),
                            }
                        }
                    }
                }
                Value::Array(r)
            }
            "implode" | "join" => {
                let (glue, arr) = if let Value::Array(a) = arg(0) {
                    (String::new(), a)
                } else {
                    match arg(1) {
                        Value::Array(a) => (arg(0).to_php_string(), a),
                        _ => (arg(0).to_php_string(), PArray::default()),
                    }
                };
                let parts: Vec<String> =
                    arr.entries.iter().map(|(_, v)| v.to_php_string()).collect();
                Value::Str(parts.join(&glue))
            }
            "explode" => {
                let d = arg(0).to_php_string();
                let s = arg(1).to_php_string();
                if d.is_empty() {
                    return Err(EngineError("explode(): empty delimiter".into()));
                }
                let mut r = PArray::default();
                for part in s.split(&d) {
                    r.push(Value::Str(part.to_string()));
                }
                Value::Array(r)
            }
            "range" => {
                let lo = to_long(&arg(0));
                let hi = to_long(&arg(1));
                let mut r = PArray::default();
                if lo <= hi {
                    for i in lo..=hi {
                        r.push(Value::Int(i));
                    }
                } else {
                    for i in (hi..=lo).rev() {
                        r.push(Value::Int(i));
                    }
                }
                Value::Array(r)
            }
            _ => {
                let key = name.to_ascii_lowercase();
                if let Some(func) = self.funcs.get(&key).cloned() {
                    return self.call_user_function(func, args.clone());
                }
                return Err(EngineError(format!("unknown function `{name}()`")));
            }
        };
        Ok(v)
    }

    /// `function name(params) : T { body }` — record it and skip the body.
    fn function_decl(&mut self) -> R<Flow> {
        self.skip_ws();
        let name = match self.try_identifier() {
            Some(n) => n,
            None => return Err(EngineError("expected function name".into())),
        };
        self.expect_char('(')?;
        let mut params = Vec::new();
        self.skip_ws();
        if self.peek() != Some(')') {
            loop {
                self.skip_ws();
                if self.peek() == Some('?') {
                    self.pos += 1; // nullable type
                    self.skip_ws();
                }
                // type hint (incl. unions and namespaces) — parsed and ignored
                if matches!(self.peek(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '\\') {
                    while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '\\' || c == '|') {
                        self.pos += 1;
                    }
                    self.skip_ws();
                }
                if self.peek() == Some('&') {
                    self.pos += 1; // by-reference — treated as by-value for now
                    self.skip_ws();
                }
                if self.starts_with("...") {
                    self.pos += 3; // variadic — treated as a single param for now
                    self.skip_ws();
                }
                if self.peek() != Some('$') {
                    return Err(EngineError("expected parameter variable".into()));
                }
                let pname = self.parse_variable_name()?;
                self.skip_ws();
                let mut default = None;
                if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                    self.pos += 1;
                    self.skip_ws();
                    default = Some(self.pos);
                    let prev = self.live;
                    self.live = false;
                    let _ = self.expression()?; // skip past the default expression
                    self.live = prev;
                }
                params.push(Param { name: pname, default });
                self.skip_ws();
                match self.peek() {
                    Some(',') => self.pos += 1,
                    Some(')') => break,
                    other => {
                        return Err(EngineError(format!(
                            "expected `,` or `)` in parameters, found {other:?}"
                        )))
                    }
                }
            }
        }
        self.expect_char(')')?;
        // optional `: returnType`
        self.skip_ws();
        if self.peek() == Some(':') {
            self.pos += 1;
            self.skip_ws();
            if self.peek() == Some('?') {
                self.pos += 1;
                self.skip_ws();
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '\\' || c == '|') {
                self.pos += 1;
            }
            self.skip_ws();
        }
        if self.peek() != Some('{') {
            return Err(EngineError("expected `{` for function body".into()));
        }
        let body_start = self.pos;
        self.pos += 1; // consume {
        self.skip_to_block_end()?;
        if self.live {
            self.funcs
                .insert(name.to_ascii_lowercase(), FuncDef { params, body_start });
        }
        Ok(Flow::Normal)
    }

    fn return_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        let val = if self.peek() == Some(';') || self.starts_with("?>") {
            Value::Null
        } else {
            self.expression()?
        };
        self.end_statement()?;
        if self.live {
            self.return_val = Some(val);
            Ok(Flow::Return)
        } else {
            Ok(Flow::Normal)
        }
    }

    /// Call a user function: bind params in a fresh scope, run the body
    /// (re-entrant via pos save/restore), and pick up the `return` value.
    fn call_user_function(&mut self, func: FuncDef, args: Vec<Value>) -> R<Value> {
        self.tick()?;
        self.call_depth += 1;
        if self.call_depth > 2000 {
            return Err(EngineError("maximum function nesting level reached".into()));
        }
        let saved_pos = self.pos;
        let mut bound: Vec<(String, Value)> = Vec::with_capacity(func.params.len());
        for (i, p) in func.params.iter().enumerate() {
            let v = if let Some(a) = args.get(i) {
                a.clone()
            } else if let Some(dstart) = p.default {
                self.pos = dstart;
                self.expression()?
            } else {
                Value::Null
            };
            bound.push((p.name.clone(), v));
        }
        let saved_vars = std::mem::take(&mut self.vars);
        let saved_ret = self.return_val.take();
        for (n, v) in bound {
            self.vars.insert(n, v);
        }
        self.pos = func.body_start;
        let flow = self.block()?;
        let ret = if matches!(flow, Flow::Return) {
            self.return_val.take().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        self.vars = saved_vars;
        self.return_val = saved_ret;
        self.pos = saved_pos;
        self.call_depth -= 1;
        Ok(ret)
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
            Some('[') => {
                self.pos += 1;
                self.parse_array_items(']')
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
                    "array" => {
                        self.skip_ws();
                        if self.peek() == Some('(') {
                            self.pos += 1;
                            self.parse_array_items(')')
                        } else {
                            Err(EngineError("expected `(` after `array`".into()))
                        }
                    }
                    _ => {
                        let after = self.pos;
                        self.skip_ws();
                        if self.peek() == Some('(') {
                            let args = self.parse_args()?;
                            self.call_function(&id, args)
                        } else {
                            self.pos = after;
                            Err(EngineError(format!(
                                "bare identifier `{id}` (constants/user functions not yet supported)"
                            )))
                        }
                    }
                }
            }
            other => Err(EngineError(format!(
                "v3b cannot parse expression near {other:?} ({:?})",
                self.snippet()
            ))),
        }
    }

    /// A `$var` reference, with optional index reads (`$a[k]`, string offsets)
    /// and post `++`/`--`.
    fn variable(&mut self) -> R<Value> {
        let name = self.parse_variable_name()?;
        let mut cur = self.vars.get(&name).cloned().unwrap_or(Value::Null);
        let mut indexed = false;
        loop {
            let after = self.pos;
            self.skip_ws();
            if self.peek() == Some('[') {
                indexed = true;
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(']') {
                    return Err(EngineError("cannot use [] for reading".into()));
                }
                let k = self.expression()?;
                self.expect_char(']')?;
                let key = key_from_value(&k);
                cur = match &cur {
                    Value::Array(a) => a.get(&key).cloned().unwrap_or(Value::Null),
                    Value::Str(s) => match &key {
                        AKey::Int(i) => s
                            .chars()
                            .nth(*i as usize)
                            .map(|c| Value::Str(c.to_string()))
                            .unwrap_or(Value::Str(String::new())),
                        _ => Value::Null,
                    },
                    _ => Value::Null,
                };
            } else {
                self.pos = after;
                break;
            }
        }
        if !indexed {
            let after = self.pos;
            self.skip_ws();
            if self.starts_with("++") || self.starts_with("--") {
                let inc = self.starts_with("++");
                self.pos += 2;
                let nv = self.inc_dec(&cur, inc);
                if self.live {
                    self.vars.insert(name, nv);
                }
                return Ok(cur); // post-inc/dec yields the OLD value
            }
            self.pos = after;
        }
        Ok(cur)
    }

    fn parse_array_items(&mut self, close: char) -> R<Value> {
        let mut arr = PArray::default();
        self.skip_ws();
        if self.peek() == Some(close) {
            self.pos += 1;
            return Ok(Value::Array(arr));
        }
        loop {
            let first = self.expression()?;
            self.skip_ws();
            if self.starts_with("=>") {
                self.pos += 2;
                self.skip_ws();
                let val = self.expression()?;
                arr.set(key_from_value(&first), val);
            } else {
                arr.push(first);
            }
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(close) {
                        self.pos += 1;
                        break;
                    }
                }
                Some(c) if c == close => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(EngineError(format!(
                        "expected `,` or `{close}` in array literal, found {other:?}"
                    )))
                }
            }
        }
        Ok(Value::Array(arr))
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

/// Normalize a value used as an array key (PHP: "5" -> int 5, bools/floats ->
/// int, null -> "").
fn key_from_value(v: &Value) -> AKey {
    match v {
        Value::Int(n) => AKey::Int(*n),
        Value::Bool(b) => AKey::Int(*b as i64),
        Value::Float(x) => AKey::Int(*x as i64),
        Value::Null => AKey::Str(String::new()),
        Value::Str(s) => {
            if let Ok(n) = s.parse::<i64>() {
                if n.to_string() == *s {
                    return AKey::Int(n);
                }
            }
            AKey::Str(s.clone())
        }
        Value::Array(_) => AKey::Str(String::new()),
    }
}

fn akey_to_value(k: &AKey) -> Value {
    match k {
        AKey::Int(i) => Value::Int(*i),
        AKey::Str(s) => Value::Str(s.clone()),
    }
}

/// Write `val` into `slot` following an index path, auto-vivifying arrays.
/// `None` in the path means append (`$a[] = …`).
fn set_path(slot: &mut Value, indices: &[Option<Value>], val: Value) {
    if indices.is_empty() {
        *slot = val;
        return;
    }
    if !matches!(slot, Value::Array(_)) {
        *slot = Value::Array(PArray::default());
    }
    let arr = match slot {
        Value::Array(a) => a,
        _ => unreachable!(),
    };
    let (first, rest) = indices.split_first().unwrap();
    let key = match first {
        None => AKey::Int(arr.next_index),
        Some(v) => key_from_value(v),
    };
    if rest.is_empty() {
        arr.set(key, val);
    } else {
        if arr.get(&key).is_none() {
            arr.set(key.clone(), Value::Array(PArray::default()));
        }
        set_path(arr.get_mut(&key).unwrap(), rest, val);
    }
}

// ---- built-in output / formatting helpers ----------------------------------

fn php_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) => "double",
        Value::Str(_) => "string",
        Value::Array(_) => "array",
    }
}

/// `var_dump` output (with trailing newline). `indent` is the leading space
/// count for this value's line — arrays recurse with `indent + 2`.
fn var_dump_str(v: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    match v {
        Value::Int(n) => format!("{pad}int({n})\n"),
        Value::Float(x) => format!("{pad}float({})\n", format_php_float(*x)),
        Value::Bool(b) => format!("{pad}bool({})\n", if *b { "true" } else { "false" }),
        Value::Str(s) => format!("{pad}string({}) \"{}\"\n", s.len(), s),
        Value::Null => format!("{pad}NULL\n"),
        Value::Array(a) => {
            let mut out = format!("{pad}array({}) {{\n", a.entries.len());
            let kp = " ".repeat(indent + 2);
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => format!("[{i}]"),
                    AKey::Str(s) => format!("[\"{s}\"]"),
                };
                out.push_str(&format!("{kp}{ks}=>\n"));
                out.push_str(&var_dump_str(val, indent + 2));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
    }
}

fn print_r_str(v: &Value) -> String {
    print_r_inner(v, 0)
}

fn print_r_inner(v: &Value, depth: usize) -> String {
    match v {
        Value::Array(a) => {
            let paren = " ".repeat(depth * 8);
            let item = " ".repeat(depth * 8 + 4);
            let mut s = String::from("Array\n");
            s.push_str(&format!("{paren}(\n"));
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => i.to_string(),
                    AKey::Str(st) => st.clone(),
                };
                s.push_str(&format!("{item}[{ks}] => {}\n", print_r_inner(val, depth + 1)));
            }
            s.push_str(&format!("{paren})\n"));
            s
        }
        _ => v.to_php_string(),
    }
}

fn var_export_str(v: &Value) -> String {
    var_export_inner(v, 0)
}

fn var_export_inner(v: &Value, indent: usize) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            let s = format_php_float(*x);
            if s.contains(['.', 'e', 'E', 'N', 'I']) {
                s
            } else {
                format!("{s}.0") // var_export keeps floats float-looking: 1 -> 1.0
            }
        }
        Value::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let pad = " ".repeat(indent);
            let ipad = " ".repeat(indent + 2);
            let mut s = String::from("array (\n");
            for (k, val) in &a.entries {
                let ks = match k {
                    AKey::Int(i) => i.to_string(),
                    AKey::Str(st) => format!("'{}'", st.replace('\\', "\\\\").replace('\'', "\\'")),
                };
                match val {
                    Value::Array(_) => s.push_str(&format!(
                        "{ipad}{ks} => \n{ipad}{},\n",
                        var_export_inner(val, indent + 2)
                    )),
                    _ => s.push_str(&format!("{ipad}{ks} => {},\n", var_export_inner(val, indent + 2))),
                }
            }
            s.push_str(&format!("{pad})"));
            s
        }
    }
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn lcfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// A pragmatic `sprintf`: flags `-0+ '`, width, `.precision`, and the
/// `s d i u f F x X o b c e` specifiers (no positional args yet).
fn php_sprintf(fmt: &str, args: &[Value]) -> String {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut argi = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if chars.get(i) == Some(&'%') {
            out.push('%');
            i += 1;
            continue;
        }
        let (mut left, mut zero, mut plus, mut space) = (false, false, false, false);
        let mut pad = ' ';
        loop {
            match chars.get(i) {
                Some('-') => left = true,
                Some('0') => zero = true,
                Some('+') => plus = true,
                Some(' ') => space = true,
                Some('\'') => {
                    i += 1;
                    if let Some(&c) = chars.get(i) {
                        pad = c;
                    }
                }
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        while let Some(c) = chars.get(i) {
            if c.is_ascii_digit() {
                width = (width * 10 + (*c as usize - '0' as usize)).min(1_000_000);
                i += 1;
            } else {
                break;
            }
        }
        let mut prec: Option<usize> = None;
        if chars.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(c) = chars.get(i) {
                if c.is_ascii_digit() {
                    p = (p * 10 + (*c as usize - '0' as usize)).min(10_000);
                    i += 1;
                } else {
                    break;
                }
            }
            prec = Some(p);
        }
        let spec = match chars.get(i) {
            Some(c) => *c,
            None => break,
        };
        i += 1;
        let a = args.get(argi).cloned().unwrap_or(Value::Null);
        argi += 1;
        let mut body = match spec {
            's' => {
                let t = a.to_php_string();
                match prec {
                    Some(p) => t.chars().take(p).collect(),
                    None => t,
                }
            }
            'd' | 'i' => {
                let n = to_long(&a);
                let sign = if n < 0 {
                    "-"
                } else if plus {
                    "+"
                } else if space {
                    " "
                } else {
                    ""
                };
                format!("{sign}{}", n.unsigned_abs())
            }
            'u' => (to_long(&a) as u64).to_string(),
            'f' | 'F' => {
                let p = prec.unwrap_or(6);
                let v = to_f64(&a);
                let sign = if v < 0.0 {
                    "-"
                } else if plus {
                    "+"
                } else if space {
                    " "
                } else {
                    ""
                };
                format!("{sign}{:.*}", p, v.abs())
            }
            'x' => format!("{:x}", to_long(&a)),
            'X' => format!("{:X}", to_long(&a)),
            'o' => format!("{:o}", to_long(&a)),
            'b' => format!("{:b}", to_long(&a)),
            'c' => (to_long(&a).rem_euclid(256) as u8 as char).to_string(),
            'e' => format!("{:e}", to_f64(&a)),
            _ => {
                argi -= 1;
                String::new()
            }
        };
        let bodylen = body.chars().count();
        if bodylen < width {
            let n = width - bodylen;
            if left {
                body.push_str(&pad.to_string().repeat(n));
            } else if zero && (body.starts_with('-') || body.starts_with('+')) {
                let (sign, rest) = body.split_at(1);
                body = format!("{sign}{}{rest}", "0".repeat(n));
            } else {
                let p = if zero { '0' } else { pad };
                body = format!("{}{}", p.to_string().repeat(n), body);
            }
        }
        out.push_str(&body);
    }
    out
}
