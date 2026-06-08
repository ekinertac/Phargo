//! Phargo — a from-scratch, memory-safe PHP engine written in Rust (PHP + Cargo).
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

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

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

#[derive(Clone, Debug)]
struct Param {
    name: String,
    /// Source position of the default-value expression, if the param has one.
    default: Option<usize>,
    /// Constructor property promotion: a leading `public`/`private`/`protected`
    /// (and/or `readonly`) modifier means the arg is also stored on `$this`.
    promoted: bool,
}

/// An anonymous function / arrow function. `captures` snapshots the outer
/// variables it closes over (by value). For arrow functions `body_start` points
/// at the single expression after `=>`; otherwise at the body's `{`.
#[derive(Debug)]
pub struct Closure {
    params: Vec<Param>,
    body_start: usize,
    captures: Vec<(String, Value)>,
    arrow: bool,
}

#[derive(Clone)]
struct FuncDef {
    params: Vec<Param>,
    /// Position of the body's opening `{` (usize::MAX for an abstract/no-body method).
    body_start: usize,
}

#[derive(Clone)]
struct ClassDef {
    parent: Option<String>,
    /// (property name, default-expression position).
    props: Vec<(String, Option<usize>)>,
    /// (constant name, value-expression position).
    consts: Vec<(String, usize)>,
    /// Implemented interfaces / extended interfaces.
    interfaces: Vec<String>,
    /// Methods keyed by lowercased name.
    methods: HashMap<String, FuncDef>,
}

/// A PHP object instance. Objects are reference types, so [`Value::Object`]
/// wraps this in `Rc<RefCell<…>>` for shared, mutable handles.
#[derive(Debug)]
pub struct Obj {
    class: String,
    props: Vec<(String, Value)>,
}

impl Obj {
    fn get(&self, k: &str) -> Option<Value> {
        self.props.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }
    fn set(&mut self, k: &str, v: Value) {
        if let Some(slot) = self.props.iter_mut().find(|(n, _)| n == k) {
            slot.1 = v;
        } else {
            self.props.push((k.to_string(), v));
        }
    }
    fn remove(&mut self, k: &str) {
        self.props.retain(|(n, _)| n != k);
    }
}

type ObjRef = Rc<RefCell<Obj>>;

/// A destructuring-assignment target element (`[$a, , [$b, $c]] = …`).
enum DTarget {
    Skip,
    Var(String),
    Nest(Vec<DTarget>),
}

/// A PHP array key (after normalization): integer or string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AKey {
    Int(i64),
    Str(String),
}

/// A PHP array: an insertion-ordered map. Small arrays use a linear scan (no
/// per-array `HashMap` overhead — the corpus has millions of tiny arrays);
/// once an array grows past a threshold we build a key→index map for O(1)
/// lookup (avoids O(n²) when building large arrays in a loop).
#[derive(Clone, Debug, Default)]
pub struct PArray {
    entries: Vec<(AKey, Value)>,
    index: Option<HashMap<AKey, usize>>,
    next_index: i64,
}

const ARRAY_INDEX_THRESHOLD: usize = 32;

impl PArray {
    fn find(&self, k: &AKey) -> Option<usize> {
        match &self.index {
            Some(m) => m.get(k).copied(),
            None => self.entries.iter().position(|(ek, _)| ek == k),
        }
    }
    fn get(&self, k: &AKey) -> Option<&Value> {
        self.find(k).map(|i| &self.entries[i].1)
    }
    fn get_mut(&mut self, k: &AKey) -> Option<&mut Value> {
        let i = self.find(k)?;
        Some(&mut self.entries[i].1)
    }
    fn ensure_index(&mut self) {
        if self.index.is_none() && self.entries.len() >= ARRAY_INDEX_THRESHOLD {
            let mut m = HashMap::with_capacity(self.entries.len() * 2);
            for (i, (k, _)) in self.entries.iter().enumerate() {
                m.insert(k.clone(), i);
            }
            self.index = Some(m);
        }
    }
    fn set(&mut self, k: AKey, v: Value) {
        if let AKey::Int(i) = &k {
            if *i >= self.next_index {
                self.next_index = *i + 1;
            }
        }
        if let Some(idx) = self.find(&k) {
            self.entries[idx].1 = v;
            return;
        }
        let idx = self.entries.len();
        if let Some(m) = &mut self.index {
            m.insert(k.clone(), idx);
        }
        self.entries.push((k, v));
        self.ensure_index();
    }
    fn push(&mut self, v: Value) {
        let k = AKey::Int(self.next_index);
        let idx = self.entries.len();
        if let Some(m) = &mut self.index {
            m.insert(k.clone(), idx);
        }
        self.entries.push((k, v));
        self.next_index += 1;
        self.ensure_index();
    }
    fn remove(&mut self, k: &AKey) {
        if let Some(pos) = self.entries.iter().position(|(ek, _)| ek == k) {
            self.entries.remove(pos);
            self.index = None; // entry indices shifted — rebuild lazily
            self.ensure_index();
        }
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
    Object(ObjRef),
    Closure(Rc<Closure>),
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
            Value::Object(_) => String::new(),      // no __toString support yet
            Value::Closure(_) => "Closure".to_string(),
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
        Value::Object(_) => true,
        Value::Closure(_) => true,
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

/// Parse the leading numeric portion of a string, PHP-style: optional leading
/// whitespace and sign, digits with an optional fraction/exponent. Returns
/// `Num::I(0)` when there is no numeric prefix (e.g. `"abc"` → 0, `"42abc"` → 42).
fn leading_number(s: &str) -> Num {
    let c: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < c.len() && c[i].is_whitespace() {
        i += 1;
    }
    let start = i;
    if i < c.len() && (c[i] == '+' || c[i] == '-') {
        i += 1;
    }
    let int_start = i;
    while i < c.len() && c[i].is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < c.len() && c[i] == '.' {
        let save = i;
        i += 1;
        let frac_start = i;
        while i < c.len() && c[i].is_ascii_digit() {
            i += 1;
        }
        if i > frac_start || i > int_start + 1 {
            is_float = true;
        } else {
            i = save; // a lone '.' with no digits either side
        }
    }
    if i < c.len() && (c[i] == 'e' || c[i] == 'E') && i > int_start {
        let save = i;
        i += 1;
        if i < c.len() && (c[i] == '+' || c[i] == '-') {
            i += 1;
        }
        let exp_start = i;
        while i < c.len() && c[i].is_ascii_digit() {
            i += 1;
        }
        if i > exp_start {
            is_float = true;
        } else {
            i = save;
        }
    }
    if i == int_start && !is_float {
        return Num::I(0);
    }
    let slice: String = c[start..i].iter().collect();
    if is_float {
        Num::F(slice.parse::<f64>().unwrap_or(0.0))
    } else {
        match slice.parse::<i64>() {
            Ok(n) => Num::I(n),
            Err(_) => Num::F(slice.parse::<f64>().unwrap_or(0.0)),
        }
    }
}

fn to_num(v: &Value) -> Num {
    match v {
        Value::Int(n) => Num::I(*n),
        Value::Float(x) => Num::F(*x),
        Value::Bool(b) => Num::I(*b as i64),
        Value::Null => Num::I(0),
        Value::Str(s) => leading_number(s),
        Value::Array(a) => Num::I(if a.entries.is_empty() { 0 } else { 1 }),
        Value::Object(_) => Num::I(1),
        Value::Closure(_) => Num::I(1),
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
    strict_eq_d(a, b, 0)
}

fn strict_eq_d(a: &Value, b: &Value, depth: usize) -> bool {
    if depth > 256 {
        return false; // recursion guard for cyclic structures
    }
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
                    .all(|((ka, va), (kb, vb))| ka == kb && strict_eq_d(va, vb, depth + 1))
        }
        (Object(x), Object(y)) => Rc::ptr_eq(x, y),
        (Closure(x), Closure(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    loose_eq_d(a, b, 0)
}

fn loose_eq_d(a: &Value, b: &Value, depth: usize) -> bool {
    if depth > 256 {
        return false; // recursion guard for cyclic structures (PHP fatals here)
    }
    use Value::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(_), _) | (_, Bool(_)) | (Null, _) | (_, Null) => to_bool(a) == to_bool(b),
        (Array(x), Array(y)) => {
            x.entries.len() == y.entries.len()
                && x.entries
                    .iter()
                    .all(|(k, v)| matches!(y.get(k), Some(w) if loose_eq_d(v, w, depth + 1)))
        }
        (Array(_), _) | (_, Array(_)) => false,
        (Object(x), Object(y)) => {
            Rc::ptr_eq(x, y) || {
                let (a, b) = (x.borrow(), y.borrow());
                a.class == b.class
                    && a.props.len() == b.props.len()
                    && a.props
                        .iter()
                        .all(|(n, v)| matches!(b.get(n), Some(w) if loose_eq_d(v, &w, depth + 1)))
            }
        }
        (Object(_), _) | (_, Object(_)) => false,
        (Closure(x), Closure(y)) => Rc::ptr_eq(x, y),
        (Closure(_), _) | (_, Closure(_)) => false,
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
    run_with_path(source, None)
}

/// Like [`run`], but records the script's file path so `__FILE__`/`__DIR__` and
/// relative `include`/`require` resolve against it.
pub fn run_with_path(source: &str, path: Option<PathBuf>) -> R<String> {
    // The prelude registers the exception/SPL classes, then the script runs.
    let full = format!("{PRELUDE}{source}");
    let mut engine = Engine {
        src: full.chars().collect(),
        pos: 0,
        out: String::new(),
        vars: HashMap::new(),
        live: true,
        steps: 0,
        funcs: HashMap::new(),
        classes: HashMap::new(),
        current_class: None,
        static_props: HashMap::new(),
        return_val: None,
        call_depth: 0,
        thrown: None,
        consts: HashMap::new(),
        ob_stack: Vec::new(),
        cur_file: path,
        included: HashSet::new(),
        enum_cases: HashMap::new(),
    };
    // Bound the main run by the length captured now: `include`/`eval` append to
    // `src` during execution, and the top-level loop must not run into them.
    let end = engine.src.len();
    engine.program_ranged(end)?;
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
    /// User-defined classes, keyed by lowercased name.
    classes: HashMap<String, ClassDef>,
    /// Class name of the currently executing method (for `self`/`parent`/`static`).
    current_class: Option<String>,
    /// Static property values, keyed by (lowercased declaring class, prop name).
    static_props: HashMap<(String, String), Value>,
    /// Value stashed by `return`, picked up by the active call frame.
    return_val: Option<Value>,
    /// Current function-call nesting depth (guards against stack overflow).
    call_depth: usize,
    /// The in-flight thrown exception value (set by `throw`, cleared by `catch`).
    thrown: Option<Value>,
    /// User-defined constants (`define(...)` and top-level `const`), case-sensitive.
    consts: HashMap<String, Value>,
    /// Output-buffering watermarks into `out` (one per active `ob_start`).
    ob_stack: Vec<usize>,
    /// Enum cases, keyed by lowercased enum name → ordered (case name, singleton object).
    enum_cases: HashMap<String, Vec<(String, Value)>>,
    /// The file currently executing (for `__FILE__`/`__DIR__` and relative includes).
    cur_file: Option<PathBuf>,
    /// Canonical paths already pulled in via `include_once`/`require_once`.
    included: HashSet<String>,
}

/// A minimal exception/SPL class hierarchy, parsed before every script so that
/// `new Exception(...)`, `getMessage()`, `instanceof`, and `catch` work via the
/// normal class machinery.
const PRELUDE: &str = r##"<?php
class stdClass {}
interface Throwable {}
interface Stringable {}
interface Traversable {}
interface Iterator extends Traversable {}
interface IteratorAggregate extends Traversable {}
interface ArrayAccess {}
interface Countable {}
interface JsonSerializable {}
class Exception implements Throwable {
    protected $message = "";
    protected $code = 0;
    protected $file = "";
    protected $line = 0;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message;
        $this->code = $code;
        $this->previous = $previous;
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getLine() { return $this->line; }
    public function getFile() { return $this->file; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return []; }
    public function getTraceAsString() { return "#0 {main}"; }
    public function __toString() { return $this->message; }
}
class Error implements Throwable {
    protected $message = "";
    protected $code = 0;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message;
        $this->code = $code;
        $this->previous = $previous;
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return []; }
    public function getTraceAsString() { return "#0 {main}"; }
    public function __toString() { return $this->message; }
}
class ErrorException extends Exception {}
class TypeError extends Error {}
class ValueError extends Error {}
class ArithmeticError extends Error {}
class DivisionByZeroError extends ArithmeticError {}
class ArgumentCountError extends TypeError {}
class UnhandledMatchError extends Error {}
class LogicException extends Exception {}
class BadFunctionCallException extends LogicException {}
class BadMethodCallException extends BadFunctionCallException {}
class DomainException extends LogicException {}
class InvalidArgumentException extends LogicException {}
class LengthException extends LogicException {}
class OutOfRangeException extends LogicException {}
class RuntimeException extends Exception {}
class OutOfBoundsException extends RuntimeException {}
class OverflowException extends RuntimeException {}
class RangeException extends RuntimeException {}
class UnderflowException extends RuntimeException {}
class UnexpectedValueException extends RuntimeException {}
class JsonException extends Exception {}
?>
"##;

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

    /// Skip one or more `#[Attr(...)]` attribute groups (PHP 8 attributes),
    /// which we parse-and-discard. Balances brackets, ignoring string contents.
    fn skip_attributes(&mut self) {
        loop {
            self.skip_ws();
            if !(self.peek() == Some('#') && self.peek_at(1) == Some('[')) {
                break;
            }
            self.pos += 2;
            let mut depth = 1;
            while depth > 0 {
                match self.peek() {
                    Some('[') => {
                        depth += 1;
                        self.pos += 1;
                    }
                    Some(']') => {
                        depth -= 1;
                        self.pos += 1;
                    }
                    Some(q @ ('\'' | '"')) => {
                        self.pos += 1;
                        while let Some(ch) = self.peek() {
                            self.pos += 1;
                            if ch == '\\' {
                                self.pos += 1;
                            } else if ch == q {
                                break;
                            }
                        }
                    }
                    None => break,
                    _ => self.pos += 1,
                }
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

    fn statement(&mut self) -> R<Flow> {
        self.tick()?;
        self.skip_ws();
        self.skip_attributes();
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
                "switch" => return self.switch_statement(),
                "throw" => return self.throw_statement(),
                "try" => return self.try_statement(),
                "class" | "interface" | "trait" => return self.class_decl(false),
                "enum" => return self.class_decl(true),
                "abstract" | "final" | "readonly" => {
                    self.skip_ws();
                    let mut k = self.try_identifier().map(|s| s.to_ascii_lowercase());
                    while matches!(k.as_deref(), Some("abstract") | Some("final") | Some("readonly")) {
                        self.skip_ws();
                        k = self.try_identifier().map(|s| s.to_ascii_lowercase());
                    }
                    if matches!(k.as_deref(), Some("class") | Some("interface") | Some("trait")) {
                        return self.class_decl(false);
                    }
                    if k.as_deref() == Some("enum") {
                        return self.class_decl(true);
                    }
                    return Err(EngineError("expected class after modifier".into()));
                }
                "function" => return self.function_decl(),
                "return" => return self.return_statement(),
                "break" => return self.break_continue(true),
                "continue" => return self.break_continue(false),
                "const" => return self.const_statement(),
                "declare" => return self.declare_statement(),
                "namespace" => return self.namespace_statement(),
                "global" => {
                    // bring named globals into the current scope (best-effort:
                    // parse-skip the declaration)
                    loop {
                        self.skip_ws();
                        if self.peek() == Some('$') {
                            let _ = self.parse_variable_name()?;
                        }
                        self.skip_ws();
                        if self.peek() == Some(',') {
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                    self.end_statement()?;
                    return Ok(Flow::Normal);
                }
                "use" => {
                    // top-level namespace import — parse-skip to `;`
                    while !matches!(self.peek(), None | Some(';')) {
                        self.pos += 1;
                    }
                    self.end_statement()?;
                    return Ok(Flow::Normal);
                }
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
                let s = self.stringify(&value)?;
                self.out.push_str(&s);
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

    fn const_statement(&mut self) -> R<Flow> {
        loop {
            self.skip_ws();
            let name = self
                .try_identifier()
                .ok_or_else(|| EngineError("expected const name".into()))?;
            self.skip_ws();
            self.expect_char('=')?;
            let v = self.expression()?;
            if self.live {
                self.consts.insert(name, v);
            }
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                continue;
            }
            break;
        }
        self.end_statement()?;
        Ok(Flow::Normal)
    }

    /// `declare(directive=value, …);` or `declare(…) { block }`. Directives
    /// (strict_types, ticks, encoding) have no effect here — only the optional
    /// block matters.
    fn declare_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        self.expect_char('(')?;
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                Some('(') => {
                    depth += 1;
                    self.pos += 1;
                }
                Some(')') => {
                    depth -= 1;
                    self.pos += 1;
                }
                None => break,
                _ => self.pos += 1,
            }
        }
        self.skip_ws();
        if self.peek() == Some('{') {
            return self.block();
        }
        self.end_statement()?;
        Ok(Flow::Normal)
    }

    /// `namespace Name;` or `namespace Name { block }`. We don't model namespaces;
    /// the declaration is skipped and any block is executed inline.
    fn namespace_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '\\') {
            self.pos += 1;
        }
        self.skip_ws();
        if self.peek() == Some('{') {
            return self.block();
        }
        self.end_statement()?;
        Ok(Flow::Normal)
    }

    /// Resolve an `include`/`require` path against the current file's directory,
    /// then the working directory.
    fn resolve_include_path(&self, filename: &str) -> Option<PathBuf> {
        let p = Path::new(filename);
        if p.is_absolute() {
            return p.exists().then(|| p.to_path_buf());
        }
        if let Some(dir) = self.cur_file.as_ref().and_then(|f| f.parent()) {
            let cand = dir.join(filename);
            if cand.exists() {
                return Some(cand);
            }
        }
        p.exists().then(|| p.to_path_buf())
    }

    /// `include`/`require`(`_once`): load a file and run it inline, sharing the
    /// current scope. Returns the file's `return` value (or 1).
    fn do_include(&mut self, filename: &str, once: bool, required: bool) -> R<Value> {
        if !self.live {
            return Ok(Value::Null);
        }
        let path = match self.resolve_include_path(filename) {
            Some(p) => p,
            None => {
                if required {
                    return Err(EngineError(format!(
                        "require(): failed opening required '{filename}'"
                    )));
                }
                return Ok(Value::Bool(false));
            }
        };
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let canon_str = canon.to_string_lossy().to_string();
        if once && self.included.contains(&canon_str) {
            return Ok(Value::Bool(true));
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                if required {
                    return Err(EngineError(format!(
                        "require(): failed opening required '{filename}'"
                    )));
                }
                return Ok(Value::Bool(false));
            }
        };
        self.included.insert(canon_str);
        // Append the file's source so every body position stays valid forever.
        let start = self.src.len();
        self.src.extend(content.chars());
        let end = self.src.len();
        let saved_pos = self.pos;
        let saved_file = self.cur_file.replace(canon);
        self.pos = start;
        let flow = self.program_ranged(end);
        self.cur_file = saved_file;
        self.pos = saved_pos;
        let flow = flow?;
        Ok(match flow {
            Flow::Return => self.return_val.take().unwrap_or(Value::Int(1)),
            _ => Value::Int(1),
        })
    }

    /// `eval($code)`: run a PHP code string (no `<?php` tag) inline.
    fn do_eval(&mut self, code: &str) -> R<Value> {
        if !self.live {
            return Ok(Value::Null);
        }
        let start = self.src.len();
        self.src.extend(code.chars());
        let end = self.src.len();
        let saved_pos = self.pos;
        self.pos = start;
        let flow = self.php_body_ranged(end);
        self.pos = saved_pos;
        let flow = flow?;
        Ok(match flow {
            Flow::Return => self.return_val.take().unwrap_or(Value::Null),
            _ => Value::Null,
        })
    }

    /// Build a stream handle (a synthetic `__Stream` object whose props hold the
    /// kind/path/mode and an in-memory buffer + cursor).
    fn make_stream(kind: &str, path: &str, mode: &str, buf: String) -> Value {
        let props = vec![
            ("__kind".to_string(), Value::Str(kind.to_string())),
            ("__path".to_string(), Value::Str(path.to_string())),
            ("__mode".to_string(), Value::Str(mode.to_string())),
            ("__buf".to_string(), Value::Str(buf)),
            ("__pos".to_string(), Value::Int(0)),
        ];
        Value::Object(Rc::new(RefCell::new(Obj {
            class: "__Stream".to_string(),
            props,
        })))
    }

    fn stream_open(&mut self, path: &str, mode: &str) -> Value {
        let m = mode.replace(['b', 't'], "");
        let kind = match path {
            "php://stdout" | "php://output" => "stdout",
            "php://stderr" => "stderr",
            "php://stdin" | "php://input" => "stdin",
            "php://memory" | "php://temp" => "memory",
            _ => "file",
        };
        if kind != "file" {
            return Self::make_stream(kind, path, &m, String::new());
        }
        let first = m.chars().next().unwrap_or('r');
        let buf = match first {
            'r' | 'a' => match std::fs::read(path) {
                Ok(b) => String::from_utf8_lossy(&b).to_string(),
                Err(_) => {
                    if first == 'r' {
                        return Value::Bool(false); // r/r+ require an existing file
                    }
                    String::new()
                }
            },
            _ => String::new(), // w/x/c: start empty (truncate)
        };
        if first == 'w' {
            let _ = std::fs::write(path, b"");
        }
        let h = Self::make_stream("file", path, &m, buf);
        if first == 'a' {
            if let Some(Value::Object(o)) = Some(&h) {
                let len = o.borrow().get("__buf").map(|v| v.to_php_string().len()).unwrap_or(0);
                o.borrow_mut().set("__pos", Value::Int(len as i64));
            }
        }
        h
    }

    fn stream_write(&mut self, h: &Value, data: &str) -> Value {
        let kind = stream_get(h, "__kind").map(|v| v.to_php_string()).unwrap_or_default();
        match kind.as_str() {
            "stdout" => {
                self.out.push_str(data);
                Value::Int(data.len() as i64)
            }
            "stderr" => Value::Int(data.len() as i64), // not captured by --EXPECT--
            _ => {
                // Mutate the buffer in place. The common case (cursor at the end)
                // is an O(data) append; the file is flushed on fclose/fflush so we
                // never rewrite the whole file per write (that was O(n^2)).
                if let Value::Object(o) = h {
                    let mut ob = o.borrow_mut();
                    let pos = ob.get("__pos").map(|v| to_long(&v)).unwrap_or(0).max(0) as usize;
                    if let Some((_, Value::Str(s))) =
                        ob.props.iter_mut().find(|(n, _)| n == "__buf")
                    {
                        if pos >= s.len() {
                            if pos > s.len() {
                                s.push_str(&" ".repeat(pos - s.len()));
                            }
                            s.push_str(data);
                        } else {
                            let mut bytes = std::mem::take(s).into_bytes();
                            for (i, b) in data.bytes().enumerate() {
                                if pos + i < bytes.len() {
                                    bytes[pos + i] = b;
                                } else {
                                    bytes.push(b);
                                }
                            }
                            *s = String::from_utf8_lossy(&bytes).to_string();
                        }
                    }
                    ob.set("__pos", Value::Int((pos + data.len()) as i64));
                }
                Value::Int(data.len() as i64)
            }
        }
    }

    fn stream_read(&mut self, h: &Value, n: Option<usize>) -> Value {
        let buf = stream_get(h, "__buf").map(|v| v.to_php_string()).unwrap_or_default();
        let bytes = buf.into_bytes();
        let pos = stream_get(h, "__pos").map(|v| to_long(&v)).unwrap_or(0).max(0) as usize;
        if pos >= bytes.len() {
            return Value::Str(String::new());
        }
        let end = match n {
            Some(k) => (pos + k).min(bytes.len()),
            None => bytes.len(),
        };
        let slice = String::from_utf8_lossy(&bytes[pos..end]).to_string();
        stream_set(h, "__pos", Value::Int(end as i64));
        Value::Str(slice)
    }

    fn stream_gets(&mut self, h: &Value, max: Option<usize>) -> Value {
        let buf = stream_get(h, "__buf").map(|v| v.to_php_string()).unwrap_or_default();
        let bytes = buf.into_bytes();
        let pos = stream_get(h, "__pos").map(|v| to_long(&v)).unwrap_or(0).max(0) as usize;
        if pos >= bytes.len() {
            return Value::Bool(false);
        }
        let mut end = pos;
        while end < bytes.len() {
            let stop = bytes[end] == b'\n';
            end += 1;
            if stop {
                break;
            }
            if let Some(m) = max {
                if end - pos >= m.saturating_sub(1) {
                    break;
                }
            }
        }
        let line = String::from_utf8_lossy(&bytes[pos..end]).to_string();
        stream_set(h, "__pos", Value::Int(end as i64));
        Value::Str(line)
    }

    fn stream_eof(&self, h: &Value) -> bool {
        let len = stream_get(h, "__buf").map(|v| v.to_php_string().len()).unwrap_or(0);
        let pos = stream_get(h, "__pos").map(|v| to_long(&v)).unwrap_or(0).max(0) as usize;
        pos >= len
    }

    fn stream_flush(&mut self, h: &Value) {
        if let (Some(p), Some(b)) = (stream_get(h, "__path"), stream_get(h, "__buf")) {
            let path = p.to_php_string();
            let kind = stream_get(h, "__kind").map(|v| v.to_php_string()).unwrap_or_default();
            if kind == "file" && !path.is_empty() {
                let _ = std::fs::write(&path, b.to_php_string().as_bytes());
            }
        }
    }

    fn stream_getcsv(&mut self, h: &Value) -> Value {
        let line = match self.stream_gets(h, None) {
            Value::Str(s) => s,
            _ => return Value::Bool(false),
        };
        if line.is_empty() {
            return Value::Bool(false);
        }
        let line = line.trim_end_matches(['\n', '\r']);
        let mut fields = PArray::default();
        let mut cur = String::new();
        let mut in_q = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if in_q {
                if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        cur.push('"');
                        i += 1;
                    } else {
                        in_q = false;
                    }
                } else {
                    cur.push(c);
                }
            } else if c == '"' {
                in_q = true;
            } else if c == ',' {
                fields.push(Value::Str(std::mem::take(&mut cur)));
            } else {
                cur.push(c);
            }
            i += 1;
        }
        fields.push(Value::Str(cur));
        Value::Array(fields)
    }

    /// Program loop (HTML + `<?php`) bounded by `end`, capturing a top-level
    /// `return` (which an included file may use).
    fn program_ranged(&mut self, end: usize) -> R<Flow> {
        while self.pos < end {
            if self.starts_with("<?php") {
                self.pos += 5;
                let f = self.php_body_ranged(end)?;
                if matches!(f, Flow::Return) {
                    return Ok(f);
                }
            } else if self.starts_with("<?=") {
                self.pos += 3;
                let v = self.expression()?;
                if self.live {
                    let s = self.stringify(&v)?;
                    self.out.push_str(&s);
                }
                self.skip_ws();
                if self.peek() == Some(';') {
                    self.pos += 1;
                }
            } else {
                self.out.push(self.src[self.pos]);
                self.pos += 1;
            }
        }
        Ok(Flow::Normal)
    }

    fn php_body_ranged(&mut self, end: usize) -> R<Flow> {
        loop {
            self.skip_ws();
            if self.pos >= end {
                return Ok(Flow::Normal);
            }
            if self.starts_with("?>") {
                self.pos += 2;
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                return Ok(Flow::Normal);
            }
            let f = self.statement()?;
            if matches!(f, Flow::Return) {
                return Ok(f);
            }
        }
    }

    /// Resolve a magic constant (`__CLASS__`, `__LINE__`, …). Returns `None`
    /// for any other identifier.
    fn magic_constant(&self, id: &str) -> Option<Value> {
        Some(match id {
            "__CLASS__" | "__TRAIT__" => {
                Value::Str(self.current_class.clone().unwrap_or_default())
            }
            "__NAMESPACE__" => Value::Str(String::new()),
            "__METHOD__" | "__FUNCTION__" => Value::Str(String::new()),
            "__FILE__" => Value::Str(
                self.cur_file
                    .as_ref()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            "__DIR__" => Value::Str(
                self.cur_file
                    .as_ref()
                    .and_then(|f| f.parent())
                    .map(|d| d.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            "__LINE__" => {
                let upto = self.pos.min(self.src.len());
                let nl = self.src[..upto].iter().filter(|c| **c == '\n').count();
                let prelude_nl = PRELUDE.matches('\n').count();
                Value::Int((nl.saturating_sub(prelude_nl) + 1) as i64)
            }
            _ => return None,
        })
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

        // Helper to parse the loop body once without executing (to move the
        // cursor past it when the loop runs zero times).
        macro_rules! skip_body {
            () => {{
                let prev = self.live;
                self.live = false;
                self.pos = body_start;
                let _ = self.block_or_statement()?;
                self.live = prev;
            }};
        }

        if !self.live {
            skip_body!();
            return Ok(Flow::Normal);
        }

        // IteratorAggregate: foreach over getIterator()'s result.
        let mut iterable = iterable;
        if let Value::Object(o) = &iterable {
            let class = o.borrow().class.clone();
            if self.lookup_method(&class, "getiterator").is_some() {
                iterable = self.call_method(&iterable.clone(), "getIterator", Vec::new())?;
            }
        }

        // Iterator object: drive iteration via rewind/valid/current/key/next.
        if let Value::Object(o) = &iterable {
            let class = o.borrow().class.clone();
            if self.lookup_method(&class, "valid").is_some()
                && self.lookup_method(&class, "current").is_some()
            {
                let it = iterable.clone();
                self.call_method(&it, "rewind", Vec::new())?;
                let mut ran = false;
                loop {
                    self.tick()?;
                    if !to_bool(&self.call_method(&it, "valid", Vec::new())?) {
                        break;
                    }
                    ran = true;
                    let cur = self.call_method(&it, "current", Vec::new())?;
                    if let Some(kv) = &key_var {
                        let k = self.call_method(&it, "key", Vec::new())?;
                        self.vars.insert(kv.clone(), k);
                    }
                    self.vars.insert(val_var.clone(), cur);
                    self.pos = body_start;
                    let flow = self.block_or_statement()?;
                    self.call_method(&it, "next", Vec::new())?;
                    match flow {
                        Flow::Break(n) => {
                            return Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal });
                        }
                        Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                        Flow::Return => return Ok(Flow::Return),
                        _ => {}
                    }
                }
                if !ran {
                    skip_body!();
                }
                return Ok(Flow::Normal);
            }
        }

        // Array, or plain object (iterate its public properties).
        let entries: Vec<(AKey, Value)> = match &iterable {
            Value::Array(a) => a.entries.clone(),
            Value::Object(o) => o
                .borrow()
                .props
                .iter()
                .filter(|(n, _)| !n.starts_with("__"))
                .map(|(n, v)| (AKey::Str(n.clone()), v.clone()))
                .collect(),
            _ => Vec::new(),
        };

        if entries.is_empty() {
            skip_body!();
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

    fn switch_statement(&mut self) -> R<Flow> {
        let subject = self.paren_expr()?;
        self.skip_ws();
        if self.peek() != Some('{') {
            return Err(EngineError("expected `{` after switch(...)".into()));
        }
        self.pos += 1;
        let mut matched = false;
        let mut default_pos: Option<usize> = None;
        loop {
            self.tick()?;
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(EngineError("unterminated switch".into())),
                _ => {}
            }
            let save = self.pos;
            let kw = self.try_identifier().map(|s| s.to_ascii_lowercase());
            match kw.as_deref() {
                Some("case") => {
                    // Only evaluate the case value while still looking for a match.
                    if !matched && self.live {
                        let cv = self.expression()?;
                        self.consume_case_colon()?;
                        if loose_eq(&subject, &cv) {
                            matched = true;
                        }
                    } else {
                        let prev = self.live;
                        self.live = false;
                        let _ = self.expression()?;
                        self.live = prev;
                        self.consume_case_colon()?;
                    }
                }
                Some("default") => {
                    self.consume_case_colon()?;
                    default_pos = Some(self.pos);
                }
                _ => {
                    self.pos = save;
                    let prev = self.live;
                    self.live = prev && matched;
                    let f = self.statement()?;
                    self.live = prev;
                    if let Some(flow) = self.switch_flow(f)? {
                        return Ok(flow);
                    }
                }
            }
        }
        // No case matched → run default (with fall-through) if there is one.
        if !matched && self.live {
            if let Some(dp) = default_pos {
                return self.run_switch_from(dp);
            }
        }
        Ok(Flow::Normal)
    }

    /// Execute switch statements from `pos` (used for `default` on no match),
    /// skipping any `case`/`default` labels, until `break`/`}`.
    fn run_switch_from(&mut self, pos: usize) -> R<Flow> {
        self.pos = pos;
        loop {
            self.tick()?;
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    return Ok(Flow::Normal);
                }
                None => return Err(EngineError("unterminated switch".into())),
                _ => {}
            }
            let save = self.pos;
            let kw = self.try_identifier().map(|s| s.to_ascii_lowercase());
            match kw.as_deref() {
                Some("case") => {
                    let prev = self.live;
                    self.live = false;
                    let _ = self.expression()?;
                    self.live = prev;
                    self.consume_case_colon()?;
                }
                Some("default") => self.consume_case_colon()?,
                _ => {
                    self.pos = save;
                    let f = self.statement()?;
                    if let Some(flow) = self.switch_flow(f)? {
                        return Ok(flow);
                    }
                }
            }
        }
    }

    /// Map a body statement's Flow inside a switch: Some(flow) exits the switch
    /// (consuming the rest of the block), None continues.
    fn switch_flow(&mut self, f: Flow) -> R<Option<Flow>> {
        match f {
            Flow::Normal => Ok(None),
            Flow::Break(n) => {
                self.skip_to_block_end()?;
                Ok(Some(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal }))
            }
            Flow::Continue(n) => {
                self.skip_to_block_end()?;
                Ok(Some(if n > 1 { Flow::Continue(n - 1) } else { Flow::Normal }))
            }
            Flow::Return => {
                self.skip_to_block_end()?;
                Ok(Some(Flow::Return))
            }
        }
    }

    fn consume_case_colon(&mut self) -> R<()> {
        self.skip_ws();
        if self.peek() == Some(':') || self.peek() == Some(';') {
            self.pos += 1;
            Ok(())
        } else {
            Err(EngineError("expected `:` after case/default".into()))
        }
    }

    fn throw_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        let v = self.expression()?;
        self.end_statement()?;
        if self.live {
            self.thrown = Some(v);
            Err(EngineError("uncaught exception".into()))
        } else {
            Ok(Flow::Normal)
        }
    }

    fn try_statement(&mut self) -> R<Flow> {
        self.skip_ws();
        if self.peek() != Some('{') {
            return Err(EngineError("expected `{` after try".into()));
        }
        let try_start = self.pos;
        self.pos += 1;
        self.skip_to_block_end()?;
        let try_end = self.pos;

        // Run the try block; a thrown exception arrives as Err with self.thrown set.
        self.pos = try_start;
        let mut pending: Option<Value> = None;
        let mut result_flow = Flow::Normal;
        match self.block() {
            Ok(f) => result_flow = f,
            Err(e) => match self.thrown.take() {
                Some(thrown) => pending = Some(thrown),
                None => return Err(e), // a real engine error, not a PHP throw
            },
        }
        self.pos = try_end;

        // catch clauses
        loop {
            let save = self.pos;
            self.skip_ws();
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() != Some("catch") {
                self.pos = save;
                break;
            }
            self.expect_char('(')?;
            let mut types: Vec<String> = Vec::new();
            loop {
                self.skip_ws();
                if self.peek() == Some('\\') {
                    self.pos += 1;
                }
                if let Some(t) = self.try_identifier() {
                    types.push(t);
                } else {
                    break;
                }
                self.skip_ws();
                if self.peek() == Some('|') {
                    self.pos += 1;
                    continue;
                }
                break;
            }
            self.skip_ws();
            let catchvar = if self.peek() == Some('$') {
                Some(self.parse_variable_name()?)
            } else {
                None
            };
            self.expect_char(')')?;
            self.skip_ws();
            if self.peek() != Some('{') {
                return Err(EngineError("expected `{` after catch".into()));
            }
            let catch_start = self.pos;
            self.pos += 1;
            self.skip_to_block_end()?;
            let catch_end = self.pos;

            let matched = match &pending {
                Some(Value::Object(o)) => {
                    let tclass = o.borrow().class.clone();
                    types.iter().any(|t| self.is_instance(&tclass, t))
                }
                _ => false,
            };
            if matched {
                if let (Some(var), Some(thrown)) = (&catchvar, &pending) {
                    if self.live {
                        self.vars.insert(var.clone(), thrown.clone());
                    }
                }
                pending = None;
                self.pos = catch_start;
                result_flow = self.block()?;
                self.pos = catch_end;
            } else {
                self.pos = catch_end;
            }
        }

        // finally — always runs
        let save = self.pos;
        self.skip_ws();
        if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("finally") {
            self.skip_ws();
            if self.peek() != Some('{') {
                return Err(EngineError("expected `{` after finally".into()));
            }
            let ff = self.block()?;
            if ff != Flow::Normal {
                return Ok(ff); // break/continue/return in finally overrides everything
            }
        } else {
            self.pos = save;
        }

        // Uncaught — re-raise after finally.
        if let Some(thrown) = pending {
            self.thrown = Some(thrown);
            return Err(EngineError("uncaught exception".into()));
        }
        Ok(result_flow)
    }

    // ---- expression parsing ------------------------------------------------

    fn expression(&mut self) -> R<Value> {
        self.parse_assignment()
    }

    /// Assignment is right-associative and lowest precedence: `$a = $b = expr`,
    /// plus compound `+= -= *= /= %= .= **=`. Falls through to binary parsing.
    fn parse_assignment(&mut self) -> R<Value> {
        // list()/[...] destructuring assignment
        self.skip_ws();
        if self.peek() == Some('[') || self.starts_with("list") {
            let save = self.pos;
            if let Some(targets) = self.try_destructure_targets()? {
                self.skip_ws();
                if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                    self.pos += 1;
                    self.skip_ws();
                    let rhs = self.parse_assignment()?;
                    if self.live {
                        self.bind_destructure(&targets, &rhs);
                    }
                    return Ok(rhs);
                }
            }
            self.pos = save;
        }
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
            // object property lvalue: $obj->prop = ... or $obj->prop[k] = ...
            if indices.is_empty() && self.starts_with("->") {
                let s2 = self.pos;
                self.pos += 2;
                self.skip_ws();
                if let Some(prop) = self.try_identifier() {
                    // optional index chain on the property
                    let mut pindices: Vec<Option<Value>> = Vec::new();
                    loop {
                        let after = self.pos;
                        self.skip_ws();
                        if self.peek() == Some('[') {
                            self.pos += 1;
                            self.skip_ws();
                            if self.peek() == Some(']') {
                                self.pos += 1;
                                pindices.push(None);
                            } else {
                                let k = self.expression()?;
                                self.expect_char(']')?;
                                pindices.push(Some(k));
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
                        if pindices.is_empty() {
                            return self.assign_property(&name, &prop, aop, rhs);
                        }
                        return self.assign_property_indexed(&name, &prop, pindices, aop, rhs);
                    }
                }
                self.pos = s2;
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
        // ArrayAccess: `$obj[k] = v` / `$obj[] = v` → offsetSet (single level).
        if indices.len() == 1 {
            if let Some(Value::Object(_)) = self.vars.get(&name) {
                let obj = self.vars.get(&name).cloned().unwrap();
                let key = indices[0].clone().unwrap_or(Value::Null);
                let newval = if aop == "=" {
                    rhs
                } else {
                    let cur = self.call_method(&obj, "offsetGet", vec![key.clone()])?;
                    self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
                };
                self.call_method(&obj, "offsetSet", vec![key, newval.clone()])?;
                return Ok(newval);
            }
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

    /// Assign to an array element of an object property: `$obj->prop[k] = v`,
    /// `$obj->prop[] = v` (single property level, nested indices).
    fn assign_property_indexed(
        &mut self,
        name: &str,
        prop: &str,
        indices: Vec<Option<Value>>,
        aop: &str,
        rhs: Value,
    ) -> R<Value> {
        if !self.live {
            return Ok(rhs);
        }
        let o = match self.vars.get(name) {
            Some(Value::Object(o)) => o.clone(),
            _ => {
                return Err(EngineError(format!(
                    "attempt to assign element of property `{prop}` on a non-object"
                )))
            }
        };
        // For compound ops, read the (small) current leaf first.
        let newval = if aop == "=" {
            rhs
        } else {
            let cur = index_get(&o.borrow().get(prop).unwrap_or(Value::Null), &indices);
            self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
        };
        // Mutate the property's array IN PLACE (no whole-array clone per write).
        let mut ob = o.borrow_mut();
        if !ob.props.iter().any(|(n, _)| n == prop) {
            ob.props.push((prop.to_string(), Value::Array(PArray::default())));
        }
        let slot = &mut ob.props.iter_mut().find(|(n, _)| n == prop).unwrap().1;
        if !matches!(slot, Value::Array(_)) {
            *slot = Value::Array(PArray::default());
        }
        set_path(slot, &indices, newval.clone());
        Ok(newval)
    }

    fn read_indexed(&self, name: &str, indices: &[Option<Value>]) -> Value {
        let mut cur = match self.vars.get(name) {
            Some(v) => v,
            None => return Value::Null,
        };
        for idx in indices {
            let key = match idx {
                None => return Value::Null,
                Some(v) => key_from_value(v),
            };
            cur = match cur {
                Value::Array(a) => match a.get(&key) {
                    Some(v) => v,
                    None => return Value::Null,
                },
                _ => return Value::Null,
            };
        }
        cur.clone()
    }

    /// Parse a `[...]` or `list(...)` destructuring pattern. Returns None (so the
    /// caller rewinds to normal expression parsing) if it isn't a valid pattern.
    fn try_destructure_targets(&mut self) -> R<Option<Vec<DTarget>>> {
        let close = if self.peek() == Some('[') {
            self.pos += 1;
            ']'
        } else {
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() != Some("list") {
                return Ok(None);
            }
            self.skip_ws();
            if self.peek() != Some('(') {
                return Ok(None);
            }
            self.pos += 1;
            ')'
        };
        let mut targets = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(close) {
                self.pos += 1;
                break;
            }
            if self.peek() == Some(',') {
                self.pos += 1;
                targets.push(DTarget::Skip);
                continue;
            }
            match self.peek() {
                Some('$') => {
                    let name = self.parse_variable_name()?;
                    targets.push(DTarget::Var(name));
                }
                Some('[') => match self.try_destructure_targets()? {
                    Some(t) => targets.push(DTarget::Nest(t)),
                    None => return Ok(None),
                },
                Some(c) if c.is_ascii_alphabetic() => {
                    let save = self.pos;
                    if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("list") {
                        self.pos = save;
                        match self.try_destructure_targets()? {
                            Some(t) => targets.push(DTarget::Nest(t)),
                            None => return Ok(None),
                        }
                    } else {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            }
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                continue;
            }
            if self.peek() == Some(close) {
                self.pos += 1;
                break;
            }
            return Ok(None);
        }
        Ok(Some(targets))
    }

    fn bind_destructure(&mut self, targets: &[DTarget], rhs: &Value) {
        for (i, t) in targets.iter().enumerate() {
            let v = match rhs {
                Value::Array(a) => a.get(&AKey::Int(i as i64)).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            };
            match t {
                DTarget::Skip => {}
                DTarget::Var(name) => {
                    self.vars.insert(name.clone(), v);
                }
                DTarget::Nest(sub) => self.bind_destructure(sub, &v),
            }
        }
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

    fn maybe_instanceof(&mut self, left: Value) -> R<Value> {
        let save = self.pos;
        self.skip_ws();
        if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("instanceof") {
            self.skip_ws();
            let cname = if self.peek() == Some('$') {
                match self.parse_unary()? {
                    Value::Object(o) => o.borrow().class.clone(),
                    other => other.to_php_string(),
                }
            } else {
                if self.peek() == Some('\\') {
                    self.pos += 1;
                }
                self.try_identifier().unwrap_or_default()
            };
            let result = match &left {
                Value::Object(o) => self.is_instance(&o.borrow().class, &cname),
                _ => false,
            };
            return Ok(Value::Bool(self.live && result));
        }
        self.pos = save;
        Ok(left)
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
        left = self.maybe_instanceof(left)?;
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
            Some('@') => {
                // error-suppression operator: evaluate the operand, but turn any
                // runtime error it raises into a silent null.
                self.pos += 1;
                match self.parse_unary() {
                    Ok(v) => Ok(v),
                    Err(_) => Ok(Value::Null),
                }
            }
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
            Some('(') => {
                if let Some(ct) = self.try_cast() {
                    let v = self.parse_unary()?;
                    Ok(self.apply_cast(&ct, v))
                } else {
                    self.primary()
                }
            }
            _ => self.primary(),
        }
    }

    /// If the cursor sits on a type-cast `(int)`, `(string)`, … consume it and
    /// return the lowercased type name. Otherwise leave the cursor untouched.
    fn try_cast(&mut self) -> Option<String> {
        let save = self.pos;
        debug_assert_eq!(self.peek(), Some('('));
        self.pos += 1;
        self.skip_ws();
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let word: String = self.src[start..self.pos].iter().collect::<String>().to_ascii_lowercase();
        self.skip_ws();
        let is_cast = matches!(
            word.as_str(),
            "int" | "integer"
                | "bool"
                | "boolean"
                | "float"
                | "double"
                | "real"
                | "string"
                | "binary"
                | "array"
                | "object"
                | "unset"
        );
        if is_cast && self.peek() == Some(')') {
            self.pos += 1;
            Some(word)
        } else {
            self.pos = save;
            None
        }
    }

    fn apply_cast(&self, ty: &str, v: Value) -> Value {
        if !self.live {
            return Value::Null;
        }
        match ty {
            "int" | "integer" => Value::Int(to_long(&v)),
            "bool" | "boolean" => Value::Bool(to_bool(&v)),
            "float" | "double" | "real" => Value::Float(to_f64(&v)),
            "string" | "binary" => Value::Str(v.to_php_string()),
            "unset" => Value::Null,
            "array" => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(PArray::default()),
                Value::Object(o) => {
                    let mut a = PArray::default();
                    for (k, val) in &o.borrow().props {
                        a.set(AKey::Str(k.clone()), val.clone());
                    }
                    Value::Array(a)
                }
                other => {
                    let mut a = PArray::default();
                    a.push(other);
                    Value::Array(a)
                }
            },
            "object" => match v {
                Value::Object(_) => v,
                Value::Array(a) => {
                    let props = a
                        .entries
                        .into_iter()
                        .map(|(k, val)| {
                            let name = match k {
                                AKey::Int(i) => i.to_string(),
                                AKey::Str(s) => s,
                            };
                            (name, val)
                        })
                        .collect();
                    Value::Object(Rc::new(RefCell::new(Obj {
                        class: "stdClass".into(),
                        props,
                    })))
                }
                Value::Null => Value::Object(Rc::new(RefCell::new(Obj {
                    class: "stdClass".into(),
                    props: Vec::new(),
                }))),
                scalar => Value::Object(Rc::new(RefCell::new(Obj {
                    class: "stdClass".into(),
                    props: vec![("scalar".to_string(), scalar)],
                }))),
            },
            _ => v,
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
            "str_contains" => {
                Value::Bool(arg(0).to_php_string().contains(&arg(1).to_php_string()))
            }
            "str_starts_with" => {
                Value::Bool(arg(0).to_php_string().starts_with(&arg(1).to_php_string()))
            }
            "str_ends_with" => {
                Value::Bool(arg(0).to_php_string().ends_with(&arg(1).to_php_string()))
            }
            "ucwords" => {
                let mut out = String::new();
                let mut cap = true;
                for c in arg(0).to_php_string().chars() {
                    if cap && c.is_alphabetic() {
                        out.extend(c.to_uppercase());
                    } else {
                        out.push(c);
                    }
                    cap = c.is_whitespace();
                }
                Value::Str(out)
            }
            "str_split" => {
                let n = if args.len() >= 2 {
                    to_long(&arg(1)).max(1) as usize
                } else {
                    1
                };
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let mut r = PArray::default();
                if chars.is_empty() {
                    r.push(Value::Str(String::new()));
                } else {
                    for chunk in chars.chunks(n) {
                        r.push(Value::Str(chunk.iter().collect()));
                    }
                }
                Value::Array(r)
            }
            "str_pad" => {
                let s = arg(0).to_php_string();
                let len = (to_long(&arg(1)).max(0) as usize).min(10_000_000);
                let padstr = if args.len() >= 3 {
                    arg(2).to_php_string()
                } else {
                    " ".to_string()
                };
                let ptype = if args.len() >= 4 { to_long(&arg(3)) } else { 1 };
                let cur = s.chars().count();
                if cur >= len || padstr.is_empty() {
                    Value::Str(s)
                } else {
                    let total = len - cur;
                    let make = |n: usize| -> String { padstr.chars().cycle().take(n).collect() };
                    match ptype {
                        0 => Value::Str(format!("{}{}", make(total), s)), // STR_PAD_LEFT
                        2 => {
                            let l = total / 2;
                            Value::Str(format!("{}{}{}", make(l), s, make(total - l)))
                        }
                        _ => Value::Str(format!("{}{}", s, make(total))), // STR_PAD_RIGHT
                    }
                }
            }
            "number_format" => {
                let n = to_f64(&arg(0));
                let dec = if args.len() >= 2 {
                    to_long(&arg(1)).clamp(0, 100) as usize
                } else {
                    0
                };
                let dp = if args.len() >= 3 {
                    arg(2).to_php_string()
                } else {
                    ".".to_string()
                };
                let ts = if args.len() >= 4 {
                    arg(3).to_php_string()
                } else {
                    ",".to_string()
                };
                let formatted = format!("{:.*}", dec, n.abs());
                let (int_part, frac_part) = match formatted.split_once('.') {
                    Some((a, b)) => (a.to_string(), b.to_string()),
                    None => (formatted.clone(), String::new()),
                };
                let digits: Vec<char> = int_part.chars().collect();
                let mut grouped = String::new();
                for (i, c) in digits.iter().enumerate() {
                    if i > 0 && (digits.len() - i) % 3 == 0 {
                        grouped.push_str(&ts);
                    }
                    grouped.push(*c);
                }
                let mut res = String::new();
                let nonzero = grouped.chars().any(|c| c.is_ascii_digit() && c != '0')
                    || frac_part.chars().any(|c| c != '0');
                if n < 0.0 && nonzero {
                    res.push('-');
                }
                res.push_str(&grouped);
                if dec > 0 {
                    res.push_str(&dp);
                    res.push_str(&frac_part);
                }
                Value::Str(res)
            }
            "dechex" => Value::Str(format!("{:x}", to_long(&arg(0)))),
            "decbin" => Value::Str(format!("{:b}", to_long(&arg(0)))),
            "decoct" => Value::Str(format!("{:o}", to_long(&arg(0)))),
            "hexdec" => Value::Int(
                i64::from_str_radix(arg(0).to_php_string().trim_start_matches("0x"), 16).unwrap_or(0),
            ),
            "bindec" => Value::Int(i64::from_str_radix(&arg(0).to_php_string(), 2).unwrap_or(0)),
            "octdec" => Value::Int(i64::from_str_radix(&arg(0).to_php_string(), 8).unwrap_or(0)),
            "pi" => Value::Float(std::f64::consts::PI),
            "call_user_func" => {
                if args.is_empty() {
                    Value::Null
                } else {
                    return self.call_callable(&args[0].clone(), args[1..].to_vec());
                }
            }
            "call_user_func_array" => {
                let cb = arg(0);
                let callargs = match arg(1) {
                    Value::Array(a) => a.entries.into_iter().map(|(_, v)| v).collect(),
                    _ => Vec::new(),
                };
                return self.call_callable(&cb, callargs);
            }
            "array_map" => match arg(1) {
                Value::Array(a) => {
                    let cb = arg(0);
                    let mut r = PArray::default();
                    for (k, v) in a.entries {
                        let m = if matches!(cb, Value::Null) {
                            v
                        } else {
                            self.call_callable(&cb, vec![v])?
                        };
                        r.set(k, m);
                    }
                    Value::Array(r)
                }
                _ => Value::Null,
            },
            "array_filter" => match arg(0) {
                Value::Array(a) => {
                    let cb = arg(1);
                    let mut r = PArray::default();
                    for (k, v) in a.entries {
                        let keep = if matches!(cb, Value::Null) {
                            to_bool(&v)
                        } else {
                            to_bool(&self.call_callable(&cb, vec![v.clone()])?)
                        };
                        if keep {
                            r.set(k, v);
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Null,
            },
            "array_reduce" => match arg(0) {
                Value::Array(a) => {
                    let cb = arg(1);
                    let mut acc = arg(2);
                    for (_, v) in a.entries {
                        acc = self.call_callable(&cb, vec![acc, v])?;
                    }
                    acc
                }
                _ => arg(2),
            },
            "array_search" => match arg(1) {
                Value::Array(a) => {
                    let needle = arg(0);
                    match a.entries.iter().find(|(_, v)| loose_eq(&needle, v)) {
                        Some((k, _)) => akey_to_value(k),
                        None => Value::Bool(false),
                    }
                }
                _ => Value::Bool(false),
            },
            "array_key_exists" | "key_exists" => match arg(1) {
                Value::Array(a) => Value::Bool(a.get(&key_from_value(&arg(0))).is_some()),
                _ => Value::Bool(false),
            },
            "array_flip" => match arg(0) {
                Value::Array(a) => {
                    let mut r = PArray::default();
                    for (k, v) in &a.entries {
                        r.set(key_from_value(v), akey_to_value(k));
                    }
                    Value::Array(r)
                }
                _ => Value::Null,
            },
            "array_unique" => match arg(0) {
                Value::Array(a) => {
                    let mut r = PArray::default();
                    let mut seen: Vec<String> = Vec::new();
                    for (k, v) in &a.entries {
                        let s = v.to_php_string();
                        if !seen.contains(&s) {
                            seen.push(s);
                            r.set(k.clone(), v.clone());
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Null,
            },
            "array_slice" => match arg(0) {
                Value::Array(a) => {
                    let len = a.entries.len() as i64;
                    let mut off = to_long(&arg(1));
                    if off < 0 {
                        off = (len + off).max(0);
                    }
                    let off = off.min(len) as usize;
                    let count = if args.len() >= 3 && !matches!(arg(2), Value::Null) {
                        let l = to_long(&arg(2));
                        if l < 0 {
                            ((len + l) - off as i64).max(0) as usize
                        } else {
                            l.max(0) as usize
                        }
                    } else {
                        a.entries.len() - off
                    };
                    let mut r = PArray::default();
                    for (k, v) in a.entries.iter().skip(off).take(count) {
                        match k {
                            AKey::Int(_) => r.push(v.clone()),
                            AKey::Str(s) => r.set(AKey::Str(s.clone()), v.clone()),
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Null,
            },
            "strcmp" => {
                let (a, b) = (arg(0).to_php_string(), arg(1).to_php_string());
                Value::Int(match a.cmp(&b) {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                })
            }
            "strcasecmp" => {
                let a = arg(0).to_php_string().to_ascii_lowercase();
                let b = arg(1).to_php_string().to_ascii_lowercase();
                Value::Int(match a.cmp(&b) {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                })
            }
            "fmod" => Value::Float(to_f64(&arg(0)) % to_f64(&arg(1))),
            "log" => {
                let x = to_f64(&arg(0));
                if args.len() >= 2 {
                    Value::Float(x.log(to_f64(&arg(1))))
                } else {
                    Value::Float(x.ln())
                }
            }
            "log10" => Value::Float(to_f64(&arg(0)).log10()),
            "exp" => Value::Float(to_f64(&arg(0)).exp()),
            "sin" => Value::Float(to_f64(&arg(0)).sin()),
            "cos" => Value::Float(to_f64(&arg(0)).cos()),
            "tan" => Value::Float(to_f64(&arg(0)).tan()),
            "json_encode" => Value::Str(json_encode_value(&arg(0), 0)),
            "json_decode" => {
                let s = arg(0).to_php_string();
                let assoc = to_bool(&arg(1));
                json_decode_str(&s, assoc).unwrap_or(Value::Null)
            }
            "array_fill" => {
                let start = to_long(&arg(0));
                let count = to_long(&arg(1)).clamp(0, 10_000_000);
                let val = arg(2);
                let mut r = PArray::default();
                for i in 0..count {
                    r.set(AKey::Int(start + i), val.clone());
                }
                Value::Array(r)
            }
            "array_fill_keys" => match arg(0) {
                Value::Array(keys) => {
                    let val = arg(1);
                    let mut r = PArray::default();
                    for (_, k) in &keys.entries {
                        r.set(key_from_value(k), val.clone());
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_combine" => match (arg(0), arg(1)) {
                (Value::Array(keys), Value::Array(vals)) => {
                    let mut r = PArray::default();
                    for ((_, k), (_, v)) in keys.entries.iter().zip(vals.entries.iter()) {
                        r.set(key_from_value(k), v.clone());
                    }
                    Value::Array(r)
                }
                _ => Value::Bool(false),
            },
            "array_column" => match arg(0) {
                Value::Array(rows) => {
                    let col = arg(1);
                    let idx = arg(2);
                    let mut r = PArray::default();
                    for (_, row) in &rows.entries {
                        let cell = match row {
                            Value::Array(a) => a.get(&key_from_value(&col)).cloned(),
                            Value::Object(o) => o.borrow().get(&col.to_php_string()),
                            _ => None,
                        };
                        if let Some(c) = cell {
                            if matches!(idx, Value::Null) {
                                r.push(c);
                            } else {
                                let ikey = match row {
                                    Value::Array(a) => {
                                        a.get(&key_from_value(&idx)).cloned().unwrap_or(Value::Null)
                                    }
                                    Value::Object(o) => {
                                        o.borrow().get(&idx.to_php_string()).unwrap_or(Value::Null)
                                    }
                                    _ => Value::Null,
                                };
                                r.set(key_from_value(&ikey), c);
                            }
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_product" => match arg(0) {
                Value::Array(a) => {
                    let mut acc = Value::Int(1);
                    for (_, v) in &a.entries {
                        acc = arith("*", &acc, v);
                    }
                    acc
                }
                _ => Value::Int(0),
            },
            "array_pad" => match arg(0) {
                Value::Array(a) => {
                    let size = to_long(&arg(1));
                    let val = arg(2);
                    let cur = a.entries.len() as i64;
                    // cap padding so a huge size can't allocate billions of elements
                    let target = size.saturating_abs().max(cur).min(cur + 10_000_000);
                    let need = (target - cur).max(0);
                    let mut r = PArray::default();
                    if size < 0 {
                        for _ in 0..need {
                            r.push(val.clone());
                        }
                    }
                    for (k, v) in &a.entries {
                        match k {
                            AKey::Int(_) => r.push(v.clone()),
                            AKey::Str(s) => r.set(AKey::Str(s.clone()), v.clone()),
                        }
                    }
                    if size >= 0 {
                        for _ in 0..need {
                            r.push(val.clone());
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_key_first" => match arg(0) {
                Value::Array(a) => a
                    .entries
                    .first()
                    .map(|(k, _)| akey_to_value(k))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "array_key_last" => match arg(0) {
                Value::Array(a) => a
                    .entries
                    .last()
                    .map(|(k, _)| akey_to_value(k))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            },
            "array_diff" => match arg(0) {
                Value::Array(a) => {
                    let mut excl: HashSet<String> = HashSet::new();
                    for o in &args[1..] {
                        if let Value::Array(b) = o {
                            for (_, bv) in &b.entries {
                                excl.insert(bv.to_php_string());
                            }
                        }
                    }
                    let mut r = PArray::default();
                    for (k, v) in &a.entries {
                        if !excl.contains(&v.to_php_string()) {
                            r.set(k.clone(), v.clone());
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_intersect" => match arg(0) {
                Value::Array(a) => {
                    let sets: Vec<HashSet<String>> = args[1..]
                        .iter()
                        .filter_map(|o| match o {
                            Value::Array(b) => {
                                Some(b.entries.iter().map(|(_, v)| v.to_php_string()).collect())
                            }
                            _ => None,
                        })
                        .collect();
                    let mut r = PArray::default();
                    for (k, v) in &a.entries {
                        let s = v.to_php_string();
                        if sets.iter().all(|set| set.contains(&s)) {
                            r.set(k.clone(), v.clone());
                        }
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "ctype_digit" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            }
            "ctype_alpha" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_alphabetic()))
            }
            "ctype_alnum" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric()))
            }
            "ctype_space" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_whitespace()))
            }
            "ctype_upper" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase()))
            }
            "ctype_lower" => {
                let s = arg(0).to_php_string();
                Value::Bool(!s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase()))
            }
            "substr_count" => {
                let h = arg(0).to_php_string();
                let n = arg(1).to_php_string();
                if n.is_empty() {
                    Value::Int(0)
                } else {
                    Value::Int(h.matches(&n).count() as i64)
                }
            }
            "str_word_count" => {
                Value::Int(arg(0).to_php_string().split_whitespace().count() as i64)
            }
            "array_chunk" => match arg(0) {
                Value::Array(a) => {
                    let size = to_long(&arg(1)).max(1) as usize;
                    let preserve = to_bool(&arg(2));
                    let mut r = PArray::default();
                    let mut chunk = PArray::default();
                    for (k, v) in &a.entries {
                        if preserve {
                            chunk.set(k.clone(), v.clone());
                        } else {
                            chunk.push(v.clone());
                        }
                        if chunk.entries.len() >= size {
                            r.push(Value::Array(std::mem::take(&mut chunk)));
                        }
                    }
                    if !chunk.entries.is_empty() {
                        r.push(Value::Array(chunk));
                    }
                    Value::Array(r)
                }
                _ => Value::Array(PArray::default()),
            },
            "array_merge_recursive" => {
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
            "str_ireplace" => {
                let search = arg(0).to_php_string();
                let replace = arg(1).to_php_string();
                let subject = arg(2).to_php_string();
                if search.is_empty() {
                    Value::Str(subject)
                } else {
                    let lsub = subject.to_lowercase();
                    let lsearch = search.to_lowercase();
                    let mut out = String::new();
                    let mut last = 0;
                    let mut idx = 0;
                    while let Some(pos) = lsub[idx..].find(&lsearch) {
                        let abs = idx + pos;
                        out.push_str(&subject[last..abs]);
                        out.push_str(&replace);
                        idx = abs + lsearch.len();
                        last = idx;
                    }
                    out.push_str(&subject[last..]);
                    Value::Str(out)
                }
            }
            "substr_replace" => {
                let s: Vec<char> = arg(0).to_php_string().chars().collect();
                let replace = arg(1).to_php_string();
                let total = s.len() as i64;
                let mut start = to_long(&arg(2));
                if start < 0 {
                    start = (total + start).max(0);
                } else {
                    start = start.min(total);
                }
                let start = start as usize;
                let len = if args.len() >= 4 && !matches!(arg(3), Value::Null) {
                    let l = to_long(&arg(3));
                    if l < 0 {
                        ((total + l) - start as i64).max(0) as usize
                    } else {
                        (l as usize).min(s.len() - start)
                    }
                } else {
                    s.len() - start
                };
                let end = (start + len).min(s.len());
                let mut out: String = s[..start].iter().collect();
                out.push_str(&replace);
                out.extend(s[end..].iter());
                Value::Str(out)
            }
            "nl2br" => {
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let mut out = String::new();
                let mut i = 0;
                while i < chars.len() {
                    if chars[i] == '\r' && chars.get(i + 1) == Some(&'\n') {
                        out.push_str("<br />\r\n");
                        i += 2;
                    } else if chars[i] == '\n' || chars[i] == '\r' {
                        out.push_str("<br />");
                        out.push(chars[i]);
                        i += 1;
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                Value::Str(out)
            }
            "addslashes" => {
                let mut out = String::new();
                for c in arg(0).to_php_string().chars() {
                    match c {
                        '\'' | '"' | '\\' => {
                            out.push('\\');
                            out.push(c);
                        }
                        '\0' => out.push_str("\\0"),
                        _ => out.push(c),
                    }
                }
                Value::Str(out)
            }
            "stripslashes" => {
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let mut out = String::new();
                let mut i = 0;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                Value::Str(out)
            }
            "vsprintf" => {
                let fmt = arg(0).to_php_string();
                let vals: Vec<Value> = match arg(1) {
                    Value::Array(a) => a.entries.into_iter().map(|(_, v)| v).collect(),
                    _ => Vec::new(),
                };
                Value::Str(php_sprintf(&fmt, &vals))
            }
            "vprintf" => {
                let fmt = arg(0).to_php_string();
                let vals: Vec<Value> = match arg(1) {
                    Value::Array(a) => a.entries.into_iter().map(|(_, v)| v).collect(),
                    _ => Vec::new(),
                };
                let out = php_sprintf(&fmt, &vals);
                let n = out.len() as i64;
                self.out.push_str(&out);
                Value::Int(n)
            }
            "strtr" => {
                let s = arg(0).to_php_string();
                if args.len() == 2 {
                    match arg(1) {
                        Value::Array(a) => {
                            let mut pairs: Vec<(Vec<char>, String)> = a
                                .entries
                                .iter()
                                .map(|(k, v)| {
                                    let key = match k {
                                        AKey::Int(n) => n.to_string(),
                                        AKey::Str(s) => s.clone(),
                                    };
                                    (key.chars().collect::<Vec<char>>(), v.to_php_string())
                                })
                                .filter(|(k, _)| !k.is_empty())
                                .collect();
                            pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
                            let chars: Vec<char> = s.chars().collect();
                            let mut out = String::new();
                            let mut i = 0;
                            'outer: while i < chars.len() {
                                for (from, to) in &pairs {
                                    if i + from.len() <= chars.len() && chars[i..i + from.len()] == from[..] {
                                        out.push_str(to);
                                        i += from.len();
                                        continue 'outer;
                                    }
                                }
                                out.push(chars[i]);
                                i += 1;
                            }
                            Value::Str(out)
                        }
                        _ => Value::Str(s),
                    }
                } else {
                    let from: Vec<char> = arg(1).to_php_string().chars().collect();
                    let to: Vec<char> = arg(2).to_php_string().chars().collect();
                    let n = from.len().min(to.len());
                    let out: String = s
                        .chars()
                        .map(|c| match from[..n].iter().position(|&fc| fc == c) {
                            Some(idx) => to[idx],
                            None => c,
                        })
                        .collect();
                    Value::Str(out)
                }
            }
            "chunk_split" => {
                let s: Vec<char> = arg(0).to_php_string().chars().collect();
                let len = if args.len() >= 2 {
                    to_long(&arg(1)).max(1) as usize
                } else {
                    76
                };
                let end = if args.len() >= 3 {
                    arg(2).to_php_string()
                } else {
                    "\r\n".to_string()
                };
                let mut out = String::new();
                for chunk in s.chunks(len) {
                    out.extend(chunk.iter());
                    out.push_str(&end);
                }
                Value::Str(out)
            }
            "compact" => {
                let mut r = PArray::default();
                for a in &args {
                    let name = a.to_php_string();
                    if let Some(v) = self.vars.get(&name).cloned() {
                        r.set(AKey::Str(name), v);
                    }
                }
                Value::Array(r)
            }
            "levenshtein" => {
                let a: Vec<char> = arg(0).to_php_string().chars().collect();
                let b: Vec<char> = arg(1).to_php_string().chars().collect();
                let (m, n) = (a.len(), b.len());
                if m == 0 {
                    Value::Int(n as i64)
                } else if n == 0 {
                    Value::Int(m as i64)
                } else {
                    let mut prev: Vec<usize> = (0..=n).collect();
                    let mut cur = vec![0usize; n + 1];
                    for i in 1..=m {
                        cur[0] = i;
                        for j in 1..=n {
                            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
                            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
                        }
                        std::mem::swap(&mut prev, &mut cur);
                    }
                    Value::Int(prev[n] as i64)
                }
            }
            "array_is_list" => match arg(0) {
                Value::Array(a) => Value::Bool(
                    a.entries
                        .iter()
                        .enumerate()
                        .all(|(i, (k, _))| matches!(k, AKey::Int(n) if *n == i as i64)),
                ),
                _ => Value::Bool(false),
            },
            "quotemeta" => {
                let mut out = String::new();
                for c in arg(0).to_php_string().chars() {
                    if ".\\+*?[^]$()".contains(c) {
                        out.push('\\');
                    }
                    out.push(c);
                }
                Value::Str(out)
            }
            "md5" => Value::Str(md5_hex(arg(0).to_php_string().as_bytes())),
            "sha1" => Value::Str(sha1_hex(arg(0).to_php_string().as_bytes())),
            "crc32" => Value::Int(crc32(arg(0).to_php_string().as_bytes()) as i64),
            "hash" => {
                let algo = arg(0).to_php_string().to_ascii_lowercase();
                let bytes = arg(1).to_php_string();
                let bytes = bytes.as_bytes();
                match algo.as_str() {
                    "md5" => Value::Str(md5_hex(bytes)),
                    "sha1" => Value::Str(sha1_hex(bytes)),
                    "crc32b" => Value::Str(format!("{:08x}", crc32(bytes))),
                    _ => return Err(EngineError(format!("hash(): unknown algorithm `{algo}`"))),
                }
            }
            "hash_equals" => {
                Value::Bool(arg(0).to_php_string() == arg(1).to_php_string())
            }
            "base64_encode" => Value::Str(base64_encode(arg(0).to_php_string().as_bytes())),
            "base64_decode" => {
                Value::Str(String::from_utf8_lossy(&base64_decode(&arg(0).to_php_string())).into_owned())
            }
            "bin2hex" => {
                let mut o = String::new();
                for b in arg(0).to_php_string().bytes() {
                    o.push_str(&format!("{b:02x}"));
                }
                Value::Str(o)
            }
            "hex2bin" => {
                let s = arg(0).to_php_string();
                let bytes: Vec<u8> = (0..s.len() / 2)
                    .filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
                    .collect();
                Value::Str(String::from_utf8_lossy(&bytes).into_owned())
            }
            "htmlspecialchars" => {
                let s = arg(0).to_php_string();
                Value::Str(
                    s.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('"', "&quot;")
                        .replace('\'', "&#039;"),
                )
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
            "max" => array_extreme(&args, true),
            "min" => array_extreme(&args, false),
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
            "is_object" => Value::Bool(matches!(arg(0), Value::Object(_))),
            "is_callable" => Value::Bool(match arg(0) {
                Value::Str(s) => !s.is_empty(),
                Value::Array(a) => a.entries.len() == 2,
                _ => false,
            }),
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
            "preg_quote" => {
                let s = arg(0).to_php_string();
                let delim = match arg(1) {
                    Value::Str(d) => d.chars().next(),
                    _ => None,
                };
                Value::Str(rx_quote(&s, delim))
            }
            "preg_replace" => {
                let limit = args.get(3).map(to_long).unwrap_or(-1);
                let mut count = 0i64;
                let pats: Vec<String> = match arg(0) {
                    Value::Array(a) => {
                        a.entries.iter().map(|(_, v)| v.to_php_string()).collect()
                    }
                    other => vec![other.to_php_string()],
                };
                let repl_is_array = matches!(arg(1), Value::Array(_));
                let repls: Vec<String> = match arg(1) {
                    Value::Array(a) => {
                        a.entries.iter().map(|(_, v)| v.to_php_string()).collect()
                    }
                    other => vec![other.to_php_string()],
                };
                let apply = |subj: String, count: &mut i64| -> Option<String> {
                    let mut cur = subj;
                    for (pi, p) in pats.iter().enumerate() {
                        let rx = rx_compile(p)?;
                        let r = if repl_is_array {
                            repls.get(pi).cloned().unwrap_or_default()
                        } else {
                            repls.first().cloned().unwrap_or_default()
                        };
                        cur = rx_replace_str(&rx, &r, &cur, limit, count);
                    }
                    Some(cur)
                };
                match arg(2) {
                    Value::Array(a) => {
                        let mut out = PArray::default();
                        for (k, v) in a.entries {
                            match apply(v.to_php_string(), &mut count) {
                                Some(s) => out.set(k, Value::Str(s)),
                                None => return Ok(Value::Null),
                            }
                        }
                        Value::Array(out)
                    }
                    other => match apply(other.to_php_string(), &mut count) {
                        Some(s) => Value::Str(s),
                        None => Value::Null,
                    },
                }
            }
            "preg_replace_callback" => {
                let cb = arg(1);
                let limit = args.get(3).map(to_long).unwrap_or(-1);
                let mut count = 0i64;
                let rx = match rx_compile(&arg(0).to_php_string()) {
                    Some(r) => r,
                    None => return Ok(Value::Null),
                };
                match arg(2) {
                    Value::Array(a) => {
                        let mut out = PArray::default();
                        for (k, v) in a.entries {
                            let s = rx_replace_cb(
                                &rx,
                                &v.to_php_string(),
                                limit,
                                &mut count,
                                |slots, text| {
                                    let mut m = PArray::default();
                                    self.fill_match_array(&mut m, &rx, text, slots);
                                    Ok(self
                                        .call_callable(&cb, vec![Value::Array(m)])?
                                        .to_php_string())
                                },
                            )?;
                            out.set(k, Value::Str(s));
                        }
                        Value::Array(out)
                    }
                    other => {
                        let s = rx_replace_cb(
                            &rx,
                            &other.to_php_string(),
                            limit,
                            &mut count,
                            |slots, text| {
                                let mut m = PArray::default();
                                self.fill_match_array(&mut m, &rx, text, slots);
                                Ok(self
                                    .call_callable(&cb, vec![Value::Array(m)])?
                                    .to_php_string())
                            },
                        )?;
                        Value::Str(s)
                    }
                }
            }
            "preg_split" => {
                let limit = args.get(2).map(to_long).unwrap_or(-1);
                let split_flags = args.get(3).map(to_long).unwrap_or(0);
                let no_empty = split_flags & 1 != 0;
                let delim_capture = split_flags & 2 != 0;
                let rx = match rx_compile(&arg(0).to_php_string()) {
                    Some(r) => r,
                    None => return Ok(Value::Bool(false)),
                };
                let text: Vec<char> = arg(1).to_php_string().chars().collect();
                let mut steps = 0usize;
                let mut result = PArray::default();
                let mut last = 0usize;
                let mut pos = 0usize;
                let mut pieces = 0i64;
                let max = if limit <= 0 { i64::MAX } else { limit };
                while pos <= text.len() {
                    if pieces + 1 >= max {
                        break;
                    }
                    match rx.exec(&text, pos, &mut steps) {
                        Some(slots) => {
                            let (ms, me) = (slots[0], slots[1]);
                            let piece: String = text[last..ms].iter().collect();
                            if !(no_empty && piece.is_empty()) {
                                result.push(Value::Str(piece));
                                pieces += 1;
                            }
                            if delim_capture {
                                for g in 1..=rx.ngroups {
                                    let gs = rx_group_str(&text, &slots, g);
                                    if !(no_empty && gs.is_empty()) {
                                        result.push(Value::Str(gs));
                                    }
                                }
                            }
                            if me > ms {
                                last = me;
                                pos = me;
                            } else {
                                last = ms;
                                pos = ms + 1;
                            }
                        }
                        None => break,
                    }
                }
                let tail: String = text[last..].iter().collect();
                if !(no_empty && tail.is_empty()) {
                    result.push(Value::Str(tail));
                }
                Value::Array(result)
            }
            "serialize" => Value::Str(php_serialize(&arg(0), 0)),
            "unserialize" => {
                let s = arg(0).to_php_string();
                let mut p = 0usize;
                php_unserialize(s.as_bytes(), &mut p, 0).unwrap_or(Value::Bool(false))
            }
            "define" => {
                let name = arg(0).to_php_string();
                self.consts.insert(name, arg(1));
                Value::Bool(true)
            }
            "defined" => Value::Bool(
                self.consts.contains_key(&arg(0).to_php_string())
                    || php_constant(&arg(0).to_php_string()).is_some(),
            ),
            "constant" => {
                let n = arg(0).to_php_string();
                self.consts
                    .get(&n)
                    .cloned()
                    .or_else(|| php_constant(&n))
                    .unwrap_or(Value::Null)
            }
            // ---- output buffering --------------------------------------------
            "ob_start" => {
                self.ob_stack.push(self.out.len());
                Value::Bool(true)
            }
            "ob_get_contents" => match self.ob_stack.last() {
                Some(&w) => Value::Str(self.out[w..].to_string()),
                None => Value::Bool(false),
            },
            "ob_get_clean" => match self.ob_stack.pop() {
                Some(w) => {
                    let s = self.out[w..].to_string();
                    self.out.truncate(w);
                    Value::Str(s)
                }
                None => Value::Bool(false),
            },
            "ob_get_flush" => match self.ob_stack.pop() {
                Some(w) => Value::Str(self.out[w..].to_string()),
                None => Value::Bool(false),
            },
            "ob_end_clean" => match self.ob_stack.pop() {
                Some(w) => {
                    self.out.truncate(w);
                    Value::Bool(true)
                }
                None => Value::Bool(false),
            },
            "ob_end_flush" | "ob_flush" | "flush" => {
                if name.eq_ignore_ascii_case("ob_end_flush") {
                    self.ob_stack.pop();
                }
                Value::Bool(true)
            }
            "ob_get_level" => Value::Int(self.ob_stack.len() as i64),
            "ob_get_length" => match self.ob_stack.last() {
                Some(&w) => Value::Int((self.out.len() - w) as i64),
                None => Value::Bool(false),
            },
            // ---- environment / setup no-ops ----------------------------------
            "error_reporting" => Value::Int(32767),
            "ini_set" | "ini_get" => Value::Bool(false),
            "set_error_handler" | "set_exception_handler" | "restore_error_handler"
            | "restore_exception_handler" => Value::Null,
            "spl_autoload_register" | "register_shutdown_function" | "register_tick_function" => {
                Value::Bool(true)
            }
            "trigger_error" | "set_time_limit" | "declare" | "gc_collect_cycles"
            | "gc_enable" | "gc_disable" | "clearstatcache" | "usleep" | "sleep"
            | "srand" | "mt_srand" | "putenv" | "setlocale" | "header"
            | "date_default_timezone_set" => Value::Bool(true),
            "date_default_timezone_get" => Value::Str("UTC".into()),
            "assert" => Value::Bool(true),
            "getenv" => match std::env::var(arg(0).to_php_string()) {
                Ok(v) => Value::Str(v),
                Err(_) => Value::Bool(false),
            },
            "class_exists" | "interface_exists" | "trait_exists" | "enum_exists" => {
                Value::Bool(self.classes.contains_key(&arg(0).to_php_string().to_ascii_lowercase()))
            }
            "function_exists" => {
                Value::Bool(self.funcs.contains_key(&arg(0).to_php_string().to_ascii_lowercase()))
            }
            // ---- filesystem --------------------------------------------------
            "file_put_contents" => {
                let path = arg(0).to_php_string();
                let data = match arg(1) {
                    Value::Array(a) => a
                        .entries
                        .iter()
                        .map(|(_, v)| v.to_php_string())
                        .collect::<String>(),
                    other => other.to_php_string(),
                };
                let append = to_long(&arg(2)) & 8 != 0;
                let res = if append {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(data.as_bytes()))
                } else {
                    std::fs::write(&path, data.as_bytes())
                };
                match res {
                    Ok(_) => Value::Int(data.len() as i64),
                    Err(_) => Value::Bool(false),
                }
            }
            "file_get_contents" => match std::fs::read(arg(0).to_php_string()) {
                Ok(b) => Value::Str(String::from_utf8_lossy(&b).to_string()),
                Err(_) => Value::Bool(false),
            },
            "file" => match std::fs::read_to_string(arg(0).to_php_string()) {
                Ok(s) => {
                    let mut r = PArray::default();
                    let keep_nl = to_long(&arg(1)) & 2 == 0; // FILE_IGNORE_NEW_LINES = 2
                    for line in s.split_inclusive('\n') {
                        let l = if keep_nl {
                            line.to_string()
                        } else {
                            line.trim_end_matches(['\n', '\r']).to_string()
                        };
                        r.push(Value::Str(l));
                    }
                    Value::Array(r)
                }
                Err(_) => Value::Bool(false),
            },
            "file_exists" => Value::Bool(Path::new(&arg(0).to_php_string()).exists()),
            "is_file" => Value::Bool(Path::new(&arg(0).to_php_string()).is_file()),
            "is_dir" => Value::Bool(Path::new(&arg(0).to_php_string()).is_dir()),
            "is_readable" | "is_writable" | "is_writeable" => {
                Value::Bool(Path::new(&arg(0).to_php_string()).exists())
            }
            "unlink" => Value::Bool(std::fs::remove_file(arg(0).to_php_string()).is_ok()),
            "rmdir" => Value::Bool(std::fs::remove_dir(arg(0).to_php_string()).is_ok()),
            "mkdir" => {
                let path = arg(0).to_php_string();
                let recursive = to_bool(&arg(2));
                let res = if recursive {
                    std::fs::create_dir_all(&path)
                } else {
                    std::fs::create_dir(&path)
                };
                Value::Bool(res.is_ok())
            }
            "rename" => {
                Value::Bool(std::fs::rename(arg(0).to_php_string(), arg(1).to_php_string()).is_ok())
            }
            "copy" => {
                Value::Bool(std::fs::copy(arg(0).to_php_string(), arg(1).to_php_string()).is_ok())
            }
            "filesize" => match std::fs::metadata(arg(0).to_php_string()) {
                Ok(m) => Value::Int(m.len() as i64),
                Err(_) => Value::Bool(false),
            },
            "touch" => {
                let path = arg(0).to_php_string();
                let r = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&path);
                Value::Bool(r.is_ok())
            }
            "scandir" => match std::fs::read_dir(arg(0).to_php_string()) {
                Ok(rd) => {
                    let mut names: Vec<String> = vec![".".into(), "..".into()];
                    for e in rd.flatten() {
                        names.push(e.file_name().to_string_lossy().to_string());
                    }
                    names.sort();
                    let mut r = PArray::default();
                    for n in names {
                        r.push(Value::Str(n));
                    }
                    Value::Array(r)
                }
                Err(_) => Value::Bool(false),
            },
            "realpath" => match std::fs::canonicalize(arg(0).to_php_string()) {
                Ok(p) => Value::Str(p.to_string_lossy().trim_start_matches(r"\\?\").to_string()),
                Err(_) => Value::Bool(false),
            },
            "sys_get_temp_dir" => {
                Value::Str(std::env::temp_dir().to_string_lossy().trim_end_matches(['/', '\\']).to_string())
            }
            "getcwd" => match std::env::current_dir() {
                Ok(p) => Value::Str(p.to_string_lossy().to_string()),
                Err(_) => Value::Bool(false),
            },
            "tempnam" => {
                let dir = arg(0).to_php_string();
                let prefix = arg(1).to_php_string();
                let mut n = self.steps;
                let mut path;
                loop {
                    n = n.wrapping_mul(1103515245).wrapping_add(12345);
                    path = format!("{dir}/{prefix}{:x}", n & 0xffffff);
                    if !Path::new(&path).exists() {
                        break;
                    }
                }
                match std::fs::write(&path, b"") {
                    Ok(_) => Value::Str(path),
                    Err(_) => Value::Bool(false),
                }
            }
            // ---- path strings (no filesystem access) -------------------------
            "basename" => {
                let p = arg(0).to_php_string();
                let p = p.trim_end_matches(['/', '\\']);
                let mut base = p.rsplit(['/', '\\']).next().unwrap_or(p).to_string();
                let suffix = arg(1).to_php_string();
                if !suffix.is_empty() && base.ends_with(&suffix) && base != suffix {
                    base.truncate(base.len() - suffix.len());
                }
                Value::Str(base)
            }
            "dirname" => {
                let mut s = arg(0).to_php_string();
                let levels = to_long(&arg(1)).max(1);
                for _ in 0..levels {
                    let t = s.trim_end_matches(['/', '\\']);
                    let next = match t.rfind(['/', '\\']) {
                        Some(0) => "/".to_string(),
                        Some(i) => t[..i].to_string(),
                        None => ".".to_string(),
                    };
                    if next == s {
                        break; // reached the root / cwd — can't go higher
                    }
                    s = next;
                }
                Value::Str(s)
            }
            "pathinfo" => {
                let p = arg(0).to_php_string();
                let pt = p.trim_end_matches(['/', '\\']);
                let base = pt.rsplit(['/', '\\']).next().unwrap_or(pt).to_string();
                let dir = match pt.rfind(['/', '\\']) {
                    Some(0) => "/".to_string(),
                    Some(i) => pt[..i].to_string(),
                    None => ".".to_string(),
                };
                let (filename, ext) = match base.rfind('.') {
                    Some(i) if i > 0 => (base[..i].to_string(), base[i + 1..].to_string()),
                    _ => (base.clone(), String::new()),
                };
                let mut r = PArray::default();
                r.set(AKey::Str("dirname".into()), Value::Str(dir));
                r.set(AKey::Str("basename".into()), Value::Str(base));
                if !ext.is_empty() {
                    r.set(AKey::Str("extension".into()), Value::Str(ext));
                }
                r.set(AKey::Str("filename".into()), Value::Str(filename));
                Value::Array(r)
            }
            "extension_loaded" => Value::Bool(false),
            // ---- more string / misc builtins ---------------------------------
            "strstr" | "strchr" => {
                let (h, nd) = (arg(0).to_php_string(), arg(1).to_php_string());
                let before = to_bool(&arg(2));
                match h.find(&nd) {
                    Some(i) => Value::Str(if before { h[..i].to_string() } else { h[i..].to_string() }),
                    None => Value::Bool(false),
                }
            }
            "stristr" => {
                let (h, nd) = (arg(0).to_php_string(), arg(1).to_php_string());
                let before = to_bool(&arg(2));
                match h.to_lowercase().find(&nd.to_lowercase()) {
                    Some(i) => Value::Str(if before { h[..i].to_string() } else { h[i..].to_string() }),
                    None => Value::Bool(false),
                }
            }
            "strrchr" => {
                let h = arg(0).to_php_string();
                let nd = arg(1).to_php_string();
                match nd.chars().next() {
                    Some(c) => match h.rfind(c) {
                        Some(i) => Value::Str(h[i..].to_string()),
                        None => Value::Bool(false),
                    },
                    None => Value::Bool(false),
                }
            }
            "strpbrk" => {
                let h = arg(0).to_php_string();
                let set: Vec<char> = arg(1).to_php_string().chars().collect();
                match h.char_indices().find(|(_, c)| set.contains(c)) {
                    Some((i, _)) => Value::Str(h[i..].to_string()),
                    None => Value::Bool(false),
                }
            }
            "fdiv" => {
                let (a, b) = (to_f64(&arg(0)), to_f64(&arg(1)));
                Value::Float(a / b)
            }
            "chdir" => Value::Bool(true), // sandboxed; relative I/O stays in scratch
            "class_alias" => {
                let orig = arg(0).to_php_string().to_ascii_lowercase();
                let alias = arg(1).to_php_string().to_ascii_lowercase();
                if let Some(def) = self.classes.get(&orig).cloned() {
                    self.classes.insert(alias, def);
                    Value::Bool(true)
                } else {
                    Value::Bool(false)
                }
            }
            "array_walk" => {
                let cb = arg(1);
                let extra = arg(2);
                if let Value::Array(a) = arg(0) {
                    for (k, v) in a.entries {
                        let mut callargs = vec![v, akey_to_value(&k)];
                        if !matches!(extra, Value::Null) {
                            callargs.push(extra.clone());
                        }
                        self.call_callable(&cb, callargs)?;
                    }
                }
                Value::Bool(true)
            }
            "wordwrap" => {
                let s = arg(0).to_php_string();
                let width = if args.len() > 1 { to_long(&arg(1)).max(1) as usize } else { 75 };
                let brk = if args.len() > 2 { arg(2).to_php_string() } else { "\n".into() };
                let mut out = String::new();
                let mut line_len = 0usize;
                for (i, word) in s.split(' ').enumerate() {
                    if i > 0 {
                        if line_len + 1 + word.len() > width {
                            out.push_str(&brk);
                            line_len = 0;
                        } else {
                            out.push(' ');
                            line_len += 1;
                        }
                    }
                    out.push_str(word);
                    line_len += word.len();
                }
                Value::Str(out)
            }
            "htmlentities" => {
                let s = arg(0).to_php_string();
                let mut out = String::new();
                for c in s.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '"' => out.push_str("&quot;"),
                        '\'' => out.push_str("&#039;"),
                        _ => out.push(c),
                    }
                }
                Value::Str(out)
            }
            "html_entity_decode" | "htmlspecialchars_decode" => {
                let s = arg(0)
                    .to_php_string()
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&#039;", "'")
                    .replace("&#39;", "'")
                    .replace("&apos;", "'")
                    .replace("&nbsp;", "\u{a0}")
                    .replace("&amp;", "&");
                Value::Str(s)
            }
            "filter_var" => {
                let v = arg(0);
                let filter = if args.len() > 1 { to_long(&arg(1)) } else { 516 };
                match filter {
                    257 => {
                        // FILTER_VALIDATE_INT
                        let s = v.to_php_string();
                        match s.trim().parse::<i64>() {
                            Ok(n) => Value::Int(n),
                            Err(_) => Value::Bool(false),
                        }
                    }
                    259 => {
                        let s = v.to_php_string();
                        match s.trim().parse::<f64>() {
                            Ok(x) => Value::Float(x),
                            Err(_) => Value::Bool(false),
                        }
                    }
                    258 => {
                        // FILTER_VALIDATE_BOOLEAN
                        let s = v.to_php_string().trim().to_ascii_lowercase();
                        match s.as_str() {
                            "1" | "true" | "on" | "yes" => Value::Bool(true),
                            "0" | "false" | "off" | "no" | "" => Value::Bool(false),
                            _ => Value::Null,
                        }
                    }
                    274 => {
                        let s = v.to_php_string();
                        let ok = s.contains('@')
                            && s.split('@').count() == 2
                            && s.rsplit('@').next().map(|d| d.contains('.')).unwrap_or(false)
                            && !s.contains(' ');
                        if ok { Value::Str(s) } else { Value::Bool(false) }
                    }
                    273 => {
                        let s = v.to_php_string();
                        if s.contains("://") && !s.contains(' ') {
                            Value::Str(s)
                        } else {
                            Value::Bool(false)
                        }
                    }
                    275 => {
                        let s = v.to_php_string();
                        let ok = s.split('.').count() == 4
                            && s.split('.').all(|o| o.parse::<u8>().is_ok());
                        if ok { Value::Str(s) } else { Value::Bool(false) }
                    }
                    _ => Value::Str(v.to_php_string()),
                }
            }
            // ---- mbstring (UTF-8; codepoint == Rust char) --------------------
            "mb_strlen" => Value::Int(arg(0).to_php_string().chars().count() as i64),
            "mb_strtoupper" => Value::Str(arg(0).to_php_string().to_uppercase()),
            "mb_strtolower" => Value::Str(arg(0).to_php_string().to_lowercase()),
            "mb_convert_case" => {
                let s = arg(0).to_php_string();
                let r = match to_long(&arg(1)) {
                    0 => s.to_uppercase(),
                    1 => s.to_lowercase(),
                    _ => {
                        // MB_CASE_TITLE
                        let mut out = String::new();
                        let mut start = true;
                        for c in s.chars() {
                            if c.is_alphanumeric() {
                                if start {
                                    out.extend(c.to_uppercase());
                                    start = false;
                                } else {
                                    out.extend(c.to_lowercase());
                                }
                            } else {
                                out.push(c);
                                start = true;
                            }
                        }
                        out
                    }
                };
                Value::Str(r)
            }
            "mb_substr" => {
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let n = chars.len() as i64;
                let mut start = to_long(&arg(1));
                if start < 0 {
                    start = (n + start).max(0);
                }
                let start = start.min(n) as usize;
                let end = match args.get(2) {
                    Some(v) if !matches!(v, Value::Null) => {
                        let l = to_long(v);
                        if l < 0 {
                            (n + l).max(start as i64) as usize
                        } else {
                            (start + l as usize).min(n as usize)
                        }
                    }
                    _ => n as usize,
                };
                Value::Str(chars[start..end.max(start)].iter().collect())
            }
            "mb_str_split" => {
                let chars: Vec<char> = arg(0).to_php_string().chars().collect();
                let size = if args.len() > 1 {
                    to_long(&arg(1)).max(1) as usize
                } else {
                    1
                };
                let mut r = PArray::default();
                for chunk in chars.chunks(size) {
                    r.push(Value::Str(chunk.iter().collect()));
                }
                Value::Array(r)
            }
            "mb_strpos" | "mb_stripos" => {
                let (mut h, mut nd) = (arg(0).to_php_string(), arg(1).to_php_string());
                if name.eq_ignore_ascii_case("mb_stripos") {
                    h = h.to_lowercase();
                    nd = nd.to_lowercase();
                }
                let off = to_long(&arg(2)).max(0) as usize;
                let hchars: Vec<char> = h.chars().collect();
                let ndchars: Vec<char> = nd.chars().collect();
                let mut found = Value::Bool(false);
                if !ndchars.is_empty() && off <= hchars.len() {
                    for i in off..=hchars.len().saturating_sub(ndchars.len()) {
                        if hchars[i..i + ndchars.len()] == ndchars[..] {
                            found = Value::Int(i as i64);
                            break;
                        }
                    }
                }
                found
            }
            "mb_ord" => match arg(0).to_php_string().chars().next() {
                Some(c) => Value::Int(c as i64),
                None => Value::Bool(false),
            },
            "mb_chr" => match char::from_u32(to_long(&arg(0)) as u32) {
                Some(c) => Value::Str(c.to_string()),
                None => Value::Bool(false),
            },
            "mb_strwidth" => Value::Int(arg(0).to_php_string().chars().count() as i64),
            "mb_internal_encoding" => {
                if matches!(arg(0), Value::Null) {
                    Value::Str("UTF-8".into())
                } else {
                    Value::Bool(true)
                }
            }
            "mb_detect_encoding" => Value::Str("UTF-8".into()),
            "mb_check_encoding" => Value::Bool(true),
            "mb_convert_encoding" => Value::Str(arg(0).to_php_string()),
            "mb_substitute_character" => {
                if matches!(arg(0), Value::Null) {
                    Value::Str("none".into())
                } else {
                    Value::Bool(true)
                }
            }
            "mb_language" => {
                if matches!(arg(0), Value::Null) {
                    Value::Str("neutral".into())
                } else {
                    Value::Bool(true)
                }
            }
            // ---- streams (fopen family) --------------------------------------
            "fopen" => {
                let path = arg(0).to_php_string();
                let mode = arg(1).to_php_string();
                self.stream_open(&path, &mode)
            }
            "fwrite" | "fputs" => {
                let data = arg(1).to_php_string();
                let data = if args.len() > 2 {
                    let n = to_long(&arg(2)).max(0) as usize;
                    data.chars().take(n).collect()
                } else {
                    data
                };
                self.stream_write(&arg(0), &data)
            }
            "fread" => {
                let n = to_long(&arg(1)).max(0) as usize;
                self.stream_read(&arg(0), Some(n))
            }
            "stream_get_contents" => self.stream_read(&arg(0), None),
            "fgets" => {
                let max = if args.len() > 1 {
                    Some(to_long(&arg(1)).max(0) as usize)
                } else {
                    None
                };
                self.stream_gets(&arg(0), max)
            }
            "fgetc" => self.stream_read(&arg(0), Some(1)),
            "feof" => Value::Bool(self.stream_eof(&arg(0))),
            "ftell" => Value::Int(stream_get(&arg(0), "__pos").map(|v| to_long(&v)).unwrap_or(0)),
            "rewind" => {
                stream_set(&arg(0), "__pos", Value::Int(0));
                Value::Bool(true)
            }
            "fseek" => {
                let off = to_long(&arg(1));
                let whence = to_long(&arg(2));
                let len = stream_get(&arg(0), "__buf").map(|v| v.to_php_string().len() as i64).unwrap_or(0);
                let cur = stream_get(&arg(0), "__pos").map(|v| to_long(&v)).unwrap_or(0);
                let np = match whence {
                    1 => cur + off,    // SEEK_CUR
                    2 => len + off,    // SEEK_END
                    _ => off,          // SEEK_SET
                }
                .max(0);
                stream_set(&arg(0), "__pos", Value::Int(np));
                Value::Int(0)
            }
            "fclose" | "fflush" => {
                self.stream_flush(&arg(0));
                if name.eq_ignore_ascii_case("fclose") {
                    stream_set(&arg(0), "__closed", Value::Bool(true));
                }
                Value::Bool(true)
            }
            "fgetcsv" => self.stream_getcsv(&arg(0)),
            "fputcsv" => {
                let fields = match arg(1) {
                    Value::Array(a) => a,
                    _ => return Ok(Value::Bool(false)),
                };
                let mut line = String::new();
                for (i, (_, v)) in fields.entries.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    let s = v.to_php_string();
                    if s.contains([',', '"', '\n', '\r']) {
                        line.push('"');
                        line.push_str(&s.replace('"', "\"\""));
                        line.push('"');
                    } else {
                        line.push_str(&s);
                    }
                }
                line.push('\n');
                self.stream_write(&arg(0), &line)
            }
            "is_resource" => Value::Bool(matches!(&arg(0), Value::Object(o)
                if o.borrow().class == "__Stream"
                && !matches!(o.borrow().get("__closed"), Some(Value::Bool(true))))),
            "get_resource_type" => Value::Str("stream".into()),
            "readfile" => match std::fs::read(arg(0).to_php_string()) {
                Ok(b) => {
                    let s = String::from_utf8_lossy(&b).to_string();
                    let n = s.len();
                    self.out.push_str(&s);
                    Value::Int(n as i64)
                }
                Err(_) => Value::Bool(false),
            },
            "fpassthru" => {
                let s = self.stream_read(&arg(0), None).to_php_string();
                let n = s.len();
                self.out.push_str(&s);
                Value::Int(n as i64)
            }
            "range" => {
                let lo = to_long(&arg(0));
                let hi = to_long(&arg(1));
                if ((hi as i128) - (lo as i128)).unsigned_abs() > 10_000_000 {
                    return Err(EngineError("range(): too many elements".into()));
                }
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
                    return self.call_user_function(func, args.clone(), None, None);
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
        let def = self.parse_callable_def()?;
        if self.live {
            self.funcs.insert(name.to_ascii_lowercase(), def);
        }
        Ok(Flow::Normal)
    }

    /// Parse `( params ) [: type] { body }` (or `;` for an abstract method),
    /// positioned just before the `(`. Records the body span and skips it.
    fn parse_callable_def(&mut self) -> R<FuncDef> {
        self.expect_char('(')?;
        let params = self.parse_params()?;
        self.expect_char(')')?;
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
        if self.peek() == Some(';') {
            self.pos += 1;
            return Ok(FuncDef {
                params,
                body_start: usize::MAX, // abstract / interface method: no body
            });
        }
        if self.peek() != Some('{') {
            return Err(EngineError("expected `{` for function body".into()));
        }
        let body_start = self.pos;
        self.pos += 1; // consume {
        self.skip_to_block_end()?;
        Ok(FuncDef { params, body_start })
    }

    /// Parse a parameter list, positioned just after `(`, stopping at `)`.
    fn parse_params(&mut self) -> R<Vec<Param>> {
        let mut params = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            return Ok(params);
        }
        loop {
            self.skip_ws();
            // attributes `#[...]` before a parameter
            self.skip_attributes();
            // constructor property promotion: leading visibility / readonly
            let mut promoted = false;
            loop {
                let save = self.pos;
                match self.try_identifier().map(|s| s.to_ascii_lowercase()) {
                    Some(ref m)
                        if matches!(m.as_str(), "public" | "private" | "protected" | "readonly") =>
                    {
                        promoted = true;
                        self.skip_ws();
                    }
                    _ => {
                        self.pos = save;
                        break;
                    }
                }
            }
            if self.peek() == Some('?') {
                self.pos += 1; // nullable type
                self.skip_ws();
            }
            // type hint (incl. unions/intersections/namespaces) — parsed and ignored
            if matches!(self.peek(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '\\') {
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '\\' || c == '|' || c == '&') {
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
            params.push(Param {
                name: pname,
                default,
                promoted,
            });
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
        Ok(params)
    }

    /// `class Name [extends Parent] [implements …] { members }`. Also used for
    /// `interface`/`trait` (members may be abstract).
    fn class_decl(&mut self, is_enum: bool) -> R<Flow> {
        self.skip_ws();
        let name = match self.try_identifier() {
            Some(n) => n,
            None => return Err(EngineError("expected class name".into())),
        };
        let mut parent = None;
        let mut interfaces: Vec<String> = Vec::new();
        let mut enum_cases: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        // backed enum: `enum E: string { ... }` — note and skip the backing type
        if is_enum && self.peek() == Some(':') {
            self.pos += 1;
            self.skip_ws();
            let _ = self.try_identifier();
            self.skip_ws();
        }
        {
            let save = self.pos;
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("extends") {
                let mut first = true;
                loop {
                    self.skip_ws();
                    if self.peek() == Some('\\') {
                        self.pos += 1;
                    }
                    match self.try_identifier() {
                        Some(n) if first => {
                            parent = Some(n);
                            first = false;
                        }
                        Some(n) => interfaces.push(n), // interface extending multiple
                        None => break,
                    }
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        continue;
                    }
                    break;
                }
            } else {
                self.pos = save;
            }
        }
        self.skip_ws();
        {
            let save = self.pos;
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("implements") {
                loop {
                    self.skip_ws();
                    if self.peek() == Some('\\') {
                        self.pos += 1;
                    }
                    match self.try_identifier() {
                        Some(n) => interfaces.push(n),
                        None => break,
                    }
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        continue;
                    }
                    break;
                }
            } else {
                self.pos = save;
            }
        }
        self.expect_char('{')?;
        let mut props: Vec<(String, Option<usize>)> = Vec::new();
        let mut static_props: Vec<(String, Option<usize>)> = Vec::new();
        let mut consts: Vec<(String, usize)> = Vec::new();
        let mut methods: HashMap<String, FuncDef> = HashMap::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(EngineError("unterminated class body".into())),
                _ => {}
            }
            // consume modifiers / type tokens until we reach function/const/$prop
            let mut is_static = false;
            let kind: &str = loop {
                self.skip_ws();
                self.skip_attributes();
                self.skip_ws();
                if self.peek() == Some('$') {
                    break "prop";
                }
                if self.peek() == Some('}') {
                    break "end";
                }
                // nullable / union / intersection / namespace separators in types
                if matches!(self.peek(), Some('?') | Some('|') | Some('&') | Some('\\')) {
                    self.pos += 1;
                    continue;
                }
                match self.try_identifier().map(|s| s.to_ascii_lowercase()) {
                    Some(ref m) if m == "function" => break "function",
                    Some(ref m) if m == "const" => break "const",
                    Some(ref m) if m == "use" => break "use",
                    Some(ref m) if is_enum && m == "case" => break "case",
                    Some(m) => {
                        if m == "static" {
                            is_static = true;
                        }
                        continue; // modifier or type name — ignore (track `static`)
                    }
                    None => return Err(EngineError("unexpected token in class body".into())),
                }
            };
            match kind {
                "end" => {
                    self.expect_char('}')?;
                    break;
                }
                "case" => {
                    self.skip_ws();
                    let cname = self
                        .try_identifier()
                        .ok_or_else(|| EngineError("expected enum case name".into()))?;
                    self.skip_ws();
                    let mut props = vec![("name".to_string(), Value::Str(cname.clone()))];
                    if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                        self.pos += 1;
                        let v = self.expression()?;
                        props.push(("value".to_string(), v));
                    }
                    if self.live {
                        let obj = Value::Object(Rc::new(RefCell::new(Obj {
                            class: name.clone(),
                            props,
                        })));
                        enum_cases.push((cname, obj));
                    }
                    self.skip_ws();
                    if self.peek() == Some(';') {
                        self.pos += 1;
                    }
                }
                "use" => {
                    // trait use: merge the named traits' members into this class
                    loop {
                        self.skip_ws();
                        if self.peek() == Some('\\') {
                            self.pos += 1;
                        }
                        let tname = match self.try_identifier() {
                            Some(n) => n,
                            None => break,
                        };
                        let tl = tname.to_ascii_lowercase();
                        if let Some(td) = self.classes.get(&tl).cloned() {
                            for (mn, md) in td.methods {
                                methods.entry(mn).or_insert(md);
                            }
                            for pr in td.props {
                                if !props.iter().any(|(n, _)| *n == pr.0) {
                                    props.push(pr);
                                }
                            }
                            for cs in td.consts {
                                if !consts.iter().any(|(n, _)| *n == cs.0) {
                                    consts.push(cs);
                                }
                            }
                        }
                        self.skip_ws();
                        if self.peek() == Some(',') {
                            self.pos += 1;
                            continue;
                        }
                        break;
                    }
                    self.skip_ws();
                    if self.peek() == Some('{') {
                        // conflict-resolution / aliasing block — skip it
                        self.pos += 1;
                        let mut depth = 1;
                        while depth > 0 {
                            match self.peek() {
                                Some('{') => {
                                    depth += 1;
                                    self.pos += 1;
                                }
                                Some('}') => {
                                    depth -= 1;
                                    self.pos += 1;
                                }
                                None => break,
                                _ => self.pos += 1,
                            }
                        }
                    } else if self.peek() == Some(';') {
                        self.pos += 1;
                    }
                }
                "function" => {
                    self.skip_ws();
                    let mname = self
                        .try_identifier()
                        .ok_or_else(|| EngineError("expected method name".into()))?;
                    let def = self.parse_callable_def()?;
                    methods.insert(mname.to_ascii_lowercase(), def);
                }
                "const" => loop {
                    self.skip_ws();
                    let mut cname = self
                        .try_identifier()
                        .ok_or_else(|| EngineError("expected const name".into()))?;
                    self.skip_ws();
                    // typed constant: `const TYPE NAME = ...`
                    if matches!(self.peek(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
                        cname = self
                            .try_identifier()
                            .ok_or_else(|| EngineError("expected const name".into()))?;
                        self.skip_ws();
                    }
                    self.expect_char('=')?;
                    self.skip_ws();
                    let cpos = self.pos;
                    let prev = self.live;
                    self.live = false;
                    let _ = self.expression()?;
                    self.live = prev;
                    consts.push((cname, cpos));
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        continue;
                    }
                    if self.peek() == Some(';') {
                        self.pos += 1;
                    }
                    break;
                },
                "prop" => loop {
                    let pname = self.parse_variable_name()?;
                    self.skip_ws();
                    let mut default = None;
                    if self.peek() == Some('=') && self.peek_at(1) != Some('=') {
                        self.pos += 1;
                        self.skip_ws();
                        default = Some(self.pos);
                        let prev = self.live;
                        self.live = false;
                        let _ = self.expression()?;
                        self.live = prev;
                    }
                    if is_static {
                        static_props.push((pname, default));
                    } else {
                        props.push((pname, default));
                    }
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        self.skip_ws();
                        continue; // public $a, $b;
                    }
                    if self.peek() == Some(';') {
                        self.pos += 1;
                    }
                    break;
                },
                _ => return Err(EngineError("unexpected class member".into())),
            }
        }
        if self.live {
            let clower = name.to_ascii_lowercase();
            for (pn, defpos) in &static_props {
                let v = match defpos {
                    Some(p) => {
                        let s = self.pos;
                        self.pos = *p;
                        let val = self.expression()?;
                        self.pos = s;
                        val
                    }
                    None => Value::Null,
                };
                self.static_props.insert((clower.clone(), pn.clone()), v);
            }
            self.classes.insert(
                clower.clone(),
                ClassDef {
                    parent,
                    props,
                    consts,
                    interfaces,
                    methods,
                },
            );
            if is_enum {
                self.enum_cases.insert(clower, enum_cases);
            }
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
    fn call_user_function(
        &mut self,
        func: FuncDef,
        args: Vec<Value>,
        this: Option<Value>,
        class_ctx: Option<String>,
    ) -> R<Value> {
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
        let saved_class = std::mem::replace(&mut self.current_class, class_ctx);
        for (n, v) in bound {
            self.vars.insert(n, v);
        }
        if let Some(t) = this {
            // constructor property promotion: copy promoted args onto $this
            if let Value::Object(o) = &t {
                for p in &func.params {
                    if p.promoted {
                        if let Some(v) = self.vars.get(&p.name).cloned() {
                            o.borrow_mut().set(&p.name, v);
                        }
                    }
                }
            }
            self.vars.insert("this".to_string(), t);
        }
        let body_result = if func.body_start == usize::MAX {
            Ok(Flow::Normal) // abstract / no body
        } else {
            self.pos = func.body_start;
            self.block()
        };
        // The return value is read before we restore the previous frame.
        let ret = match &body_result {
            Ok(Flow::Return) => self.return_val.take().unwrap_or(Value::Null),
            _ => Value::Null,
        };
        // Restore the caller's frame ALWAYS — even when unwinding a thrown
        // exception — so a `catch` higher up resumes with correct state.
        self.vars = saved_vars;
        self.return_val = saved_ret;
        self.current_class = saved_class;
        self.pos = saved_pos;
        self.call_depth -= 1;
        body_result?; // propagate a thrown exception (or error) after cleanup
        Ok(ret)
    }

    /// Resolve a method by walking the class → parent chain.
    fn lookup_method(&self, class: &str, method: &str) -> Option<FuncDef> {
        let mlow = method.to_ascii_lowercase();
        let mut cur = Some(class.to_ascii_lowercase());
        let mut guard = 0;
        while let Some(cn) = cur {
            guard += 1;
            if guard > 10_000 {
                return None; // cyclic inheritance guard
            }
            let cd = self.classes.get(&cn)?;
            if let Some(m) = cd.methods.get(&mlow) {
                return Some(m.clone());
            }
            cur = cd.parent.as_ref().map(|p| p.to_ascii_lowercase());
        }
        None
    }

    /// `new Class(args)` → allocate, set property defaults, run `__construct`.
    fn instantiate(&mut self, cname: &str, args: Vec<Value>) -> R<Value> {
        if !self.live {
            return Ok(Value::Null);
        }
        let bare = cname.trim_start_matches('\\');
        let cd = match self.classes.get(&bare.to_ascii_lowercase()).cloned() {
            Some(c) => c,
            None => return Err(EngineError(format!("class `{cname}` not found"))),
        };
        let mut obj = Obj {
            class: bare.to_string(),
            props: Vec::new(),
        };
        self.init_props(&cd, &mut obj, 0)?;
        let oref = Rc::new(RefCell::new(obj));
        if let Some(ctor) = self.lookup_method(bare, "__construct") {
            self.call_user_function(
                ctor,
                args,
                Some(Value::Object(oref.clone())),
                Some(bare.to_string()),
            )?;
        }
        Ok(Value::Object(oref))
    }

    /// Set property defaults (parents first, so child defaults win).
    fn init_props(&mut self, cd: &ClassDef, obj: &mut Obj, depth: usize) -> R<()> {
        if depth > 1000 {
            return Ok(());
        }
        if let Some(p) = &cd.parent {
            if let Some(pd) = self.classes.get(&p.to_ascii_lowercase()).cloned() {
                self.init_props(&pd, obj, depth + 1)?;
            }
        }
        for (pname, defpos) in &cd.props {
            let v = match defpos {
                Some(pos) => {
                    let save = self.pos;
                    self.pos = *pos;
                    let val = self.expression()?;
                    self.pos = save;
                    val
                }
                None => Value::Null,
            };
            obj.set(pname, v);
        }
        Ok(())
    }

    fn call_method(&mut self, recv: &Value, method: &str, args: Vec<Value>) -> R<Value> {
        if !self.live {
            return Ok(Value::Null);
        }
        let oref = match recv {
            Value::Object(o) => o.clone(),
            _ => {
                return Err(EngineError(format!(
                    "call to method `{method}()` on a non-object"
                )))
            }
        };
        let class = oref.borrow().class.clone();
        match self.lookup_method(&class, method) {
            Some(def) => {
                self.call_user_function(def, args, Some(Value::Object(oref)), Some(class))
            }
            None => Err(EngineError(format!(
                "call to undefined method {class}::{method}()"
            ))),
        }
    }

    /// Built-ins that take their first argument by reference (sort family,
    /// array_push/pop/shift/unshift). The first arg must be a plain `$var`.
    fn byref_call(&mut self, name: &str) -> R<Value> {
        self.expect_char('(')?;
        self.skip_ws();
        if self.peek() != Some('$') {
            // first arg isn't a simple variable — consume to `)` and bail
            let mut depth = 1;
            while depth > 0 {
                match self.peek() {
                    Some('(') => {
                        depth += 1;
                        self.pos += 1;
                    }
                    Some(')') => {
                        depth -= 1;
                        self.pos += 1;
                    }
                    Some('\'') => {
                        let _ = self.single_quoted()?;
                    }
                    Some('"') => {
                        let _ = self.double_quoted()?;
                    }
                    None => break,
                    _ => self.pos += 1,
                }
            }
            return Ok(Value::Bool(false));
        }
        let varname = self.parse_variable_name()?;
        let mut rest: Vec<Value> = Vec::new();
        self.skip_ws();
        while self.peek() == Some(',') {
            self.pos += 1;
            self.skip_ws();
            if self.peek() == Some(')') {
                break;
            }
            rest.push(self.expression()?);
            self.skip_ws();
        }
        self.expect_char(')')?;
        if !self.live {
            return Ok(Value::Null);
        }
        let lname = name.to_ascii_lowercase();
        let mut arr = match self.vars.get(&varname).cloned() {
            Some(Value::Array(a)) => a,
            _ => PArray::default(),
        };
        let result = match lname.as_str() {
            "array_push" => {
                for v in rest {
                    arr.push(v);
                }
                Value::Int(arr.entries.len() as i64)
            }
            "array_pop" => {
                let v = arr.entries.pop().map(|(_, v)| v).unwrap_or(Value::Null);
                arr.index = None;
                arr.ensure_index();
                v
            }
            "array_shift" => {
                if arr.entries.is_empty() {
                    Value::Null
                } else {
                    let (_, v) = arr.entries.remove(0);
                    arr = reindex(std::mem::take(&mut arr.entries));
                    v
                }
            }
            "array_unshift" => {
                let old = std::mem::take(&mut arr.entries);
                let mut na = PArray::default();
                for v in rest {
                    na.push(v);
                }
                for (k, val) in old {
                    match k {
                        AKey::Int(_) => na.push(val),
                        AKey::Str(s) => na.set(AKey::Str(s), val),
                    }
                }
                let n = na.entries.len() as i64;
                arr = na;
                Value::Int(n)
            }
            "sort" | "rsort" => {
                let mut vals: Vec<Value> = arr.entries.iter().map(|(_, v)| v.clone()).collect();
                if lname == "sort" {
                    vals.sort_by(compare);
                } else {
                    vals.sort_by(|a, b| compare(b, a));
                }
                let mut na = PArray::default();
                for v in vals {
                    na.push(v);
                }
                arr = na;
                Value::Bool(true)
            }
            "asort" | "arsort" => {
                let mut entries = std::mem::take(&mut arr.entries);
                if lname == "asort" {
                    entries.sort_by(|(_, a), (_, b)| compare(a, b));
                } else {
                    entries.sort_by(|(_, a), (_, b)| compare(b, a));
                }
                let mut na = PArray::default();
                for (k, v) in entries {
                    na.set(k, v);
                }
                arr = na;
                Value::Bool(true)
            }
            "ksort" | "krsort" => {
                let mut entries = std::mem::take(&mut arr.entries);
                if lname == "ksort" {
                    entries.sort_by(|(a, _), (b, _)| {
                        compare(&akey_to_value(a), &akey_to_value(b))
                    });
                } else {
                    entries.sort_by(|(a, _), (b, _)| {
                        compare(&akey_to_value(b), &akey_to_value(a))
                    });
                }
                let mut na = PArray::default();
                for (k, v) in entries {
                    na.set(k, v);
                }
                arr = na;
                Value::Bool(true)
            }
            "usort" | "uasort" | "uksort" => {
                let cb = rest.first().cloned().unwrap_or(Value::Null);
                let mut entries = std::mem::take(&mut arr.entries);
                let mut err: Option<EngineError> = None;
                entries.sort_by(|a, b| {
                    let (x, y) = if lname == "uksort" {
                        (akey_to_value(&a.0), akey_to_value(&b.0))
                    } else {
                        (a.1.clone(), b.1.clone())
                    };
                    match self.call_callable(&cb, vec![x, y]) {
                        Ok(r) => to_long(&r).cmp(&0),
                        Err(e) => {
                            if err.is_none() {
                                err = Some(e);
                            }
                            Ordering::Equal
                        }
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
                let mut na = PArray::default();
                if lname == "usort" {
                    for (_, v) in entries {
                        na.push(v);
                    }
                } else {
                    for (k, v) in entries {
                        na.set(k, v);
                    }
                }
                arr = na;
                Value::Bool(true)
            }
            _ => Value::Bool(false),
        };
        self.vars.insert(varname, Value::Array(arr));
        Ok(result)
    }

    /// Handle `preg_match` / `preg_match_all`, whose `$matches` (3rd arg) is by-ref.
    fn preg_match_call(&mut self, all: bool) -> R<Value> {
        self.expect_char('(')?;
        self.skip_ws();
        let pattern = self.expression()?;
        self.skip_ws();
        let mut matches_var: Option<String> = None;
        let mut rest: Vec<Value> = Vec::new();
        let mut subject = Value::Null;
        if self.peek() == Some(',') {
            self.pos += 1;
            self.skip_ws();
            subject = self.expression()?;
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some('$') {
                    matches_var = Some(self.parse_variable_name()?);
                } else {
                    let _ = self.expression()?;
                }
                self.skip_ws();
                while self.peek() == Some(',') {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        break;
                    }
                    rest.push(self.expression()?);
                    self.skip_ws();
                }
            }
        }
        self.expect_char(')')?;
        if !self.live {
            return Ok(Value::Null);
        }
        let flags = rest.first().map(to_long).unwrap_or(0);
        let offset = rest.get(1).map(to_long).unwrap_or(0).max(0) as usize;
        let rx = match rx_compile(&pattern.to_php_string()) {
            Some(r) => r,
            None => return Ok(Value::Bool(false)),
        };
        let text: Vec<char> = subject.to_php_string().chars().collect();
        let start = offset.min(text.len());
        let mut steps = 0usize;
        if all {
            let set_order = flags & 2 != 0; // PREG_SET_ORDER
            let mut sets: Vec<Vec<usize>> = Vec::new();
            let mut pos = start;
            loop {
                match rx.exec(&text, pos, &mut steps) {
                    Some(slots) => {
                        let (ms, me) = (slots[0], slots[1]);
                        pos = if me > ms { me } else { me + 1 };
                        sets.push(slots);
                        if pos > text.len() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            let count = sets.len();
            let mut result = PArray::default();
            if set_order {
                for slots in &sets {
                    let mut m = PArray::default();
                    self.fill_match_array(&mut m, &rx, &text, slots);
                    result.push(Value::Array(m));
                }
            } else {
                for g in 0..=rx.ngroups {
                    let mut col = PArray::default();
                    for slots in &sets {
                        col.push(Value::Str(rx_group_str(&text, slots, g)));
                    }
                    if let Some((nm, _)) = rx.names.iter().find(|(_, idx)| *idx == g) {
                        result.set(AKey::Str(nm.clone()), Value::Array(col.clone()));
                    }
                    result.set(AKey::Int(g as i64), Value::Array(col));
                }
            }
            if let Some(v) = matches_var {
                self.vars.insert(v, Value::Array(result));
            }
            Ok(Value::Int(count as i64))
        } else {
            match rx.exec(&text, start, &mut steps) {
                Some(slots) => {
                    let mut m = PArray::default();
                    self.fill_match_array(&mut m, &rx, &text, &slots);
                    if let Some(v) = matches_var {
                        self.vars.insert(v, Value::Array(m));
                    }
                    Ok(Value::Int(1))
                }
                None => {
                    if let Some(v) = matches_var {
                        self.vars.insert(v, Value::Array(PArray::default()));
                    }
                    Ok(Value::Int(0))
                }
            }
        }
    }

    /// Build the `$matches` array for a single match, trimming trailing unset groups.
    fn fill_match_array(&self, m: &mut PArray, rx: &Rx, text: &[char], slots: &[usize]) {
        let mut last = 0;
        for g in 0..=rx.ngroups {
            if slots[2 * g] != usize::MAX {
                last = g;
            }
        }
        for g in 0..=last {
            let s = rx_group_str(text, slots, g);
            if let Some((nm, _)) = rx.names.iter().find(|(_, idx)| *idx == g) {
                m.set(AKey::Str(nm.clone()), Value::Str(s.clone()));
            }
            m.set(AKey::Int(g as i64), Value::Str(s));
        }
    }

    /// Apply any trailing `->prop` / `->method()` / `?->` chain to an already
    /// computed value (used after `Enum::Case`, `(expr)`, `new X`, … which the
    /// `$var` path doesn't cover).
    fn chain_access(&mut self, mut val: Value) -> R<Value> {
        loop {
            let save = self.pos;
            self.skip_ws();
            if self.starts_with("?->") || self.starts_with("->") {
                let nullsafe = self.starts_with("?->");
                self.pos += if nullsafe { 3 } else { 2 };
                if nullsafe && matches!(val, Value::Null) {
                    // consume the member (and any call) but yield null
                    self.skip_ws();
                    let _ = self.try_identifier();
                    self.skip_ws();
                    if self.peek() == Some('(') {
                        let _ = self.parse_args()?;
                    }
                    val = Value::Null;
                    continue;
                }
                self.skip_ws();
                let member = self
                    .try_identifier()
                    .ok_or_else(|| EngineError("expected name after `->`".into()))?;
                self.skip_ws();
                if self.peek() == Some('(') {
                    let args = self.parse_args()?;
                    if !self.live {
                        val = Value::Null;
                        continue;
                    }
                    val = self.call_method(&val, &member, args)?;
                } else {
                    val = match &val {
                        Value::Object(o) => o.borrow().get(&member).unwrap_or(Value::Null),
                        _ => Value::Null,
                    };
                }
            } else {
                self.pos = save;
                return Ok(val);
            }
        }
    }

    /// Invoke a PHP callable: a function-name string, or `[receiver, "method"]`.
    fn call_callable(&mut self, callable: &Value, args: Vec<Value>) -> R<Value> {
        match callable {
            Value::Closure(c) => {
                let c = c.clone();
                self.call_closure(&c, args)
            }
            Value::Str(name) => self.call_function(name, args),
            Value::Array(a) if a.entries.len() == 2 => {
                let recv = a.get(&AKey::Int(0)).cloned().unwrap_or(Value::Null);
                let method = a
                    .get(&AKey::Int(1))
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_php_string();
                match &recv {
                    Value::Object(_) => self.call_method(&recv, &method, args),
                    Value::Str(cls) => match self.lookup_method(cls, &method) {
                        Some(def) => self.call_user_function(def, args, None, Some(cls.clone())),
                        None => Err(EngineError(format!("undefined method {cls}::{method}()"))),
                    },
                    _ => Err(EngineError("invalid callable array".into())),
                }
            }
            _ => Err(EngineError("value is not callable".into())),
        }
    }

    fn call_closure(&mut self, c: &Rc<Closure>, args: Vec<Value>) -> R<Value> {
        self.tick()?;
        self.call_depth += 1;
        if self.call_depth > 2000 {
            return Err(EngineError("maximum function nesting level reached".into()));
        }
        let saved_pos = self.pos;
        let mut bound: Vec<(String, Value)> = Vec::with_capacity(c.params.len());
        for (i, p) in c.params.iter().enumerate() {
            let v = if let Some(a) = args.get(i) {
                a.clone()
            } else if let Some(d) = p.default {
                self.pos = d;
                self.expression()?
            } else {
                Value::Null
            };
            bound.push((p.name.clone(), v));
        }
        let saved_vars = std::mem::take(&mut self.vars);
        let saved_ret = self.return_val.take();
        for (n, v) in &c.captures {
            self.vars.insert(n.clone(), v.clone());
        }
        for (n, v) in bound {
            self.vars.insert(n, v); // params override captures
        }
        let result: R<Value> = if c.arrow {
            self.pos = c.body_start;
            self.expression()
        } else {
            self.pos = c.body_start;
            match self.block() {
                Ok(Flow::Return) => Ok(self.return_val.take().unwrap_or(Value::Null)),
                Ok(_) => Ok(Value::Null),
                Err(e) => Err(e),
            }
        };
        self.vars = saved_vars;
        self.return_val = saved_ret;
        self.pos = saved_pos;
        self.call_depth -= 1;
        result
    }

    /// Parse a closure literal: `function (params) [use (...)] { body }` or an
    /// arrow `fn (params) => expr`. Captures are snapshotted by value.
    fn parse_closure(&mut self, arrow: bool) -> R<Value> {
        self.skip_ws();
        self.expect_char('(')?;
        let params = self.parse_params()?;
        self.expect_char(')')?;
        let mut captures: Vec<(String, Value)> = Vec::new();
        if arrow {
            // arrow functions auto-capture the entire current scope by value
            for (k, v) in &self.vars {
                captures.push((k.clone(), v.clone()));
            }
        } else {
            self.skip_ws();
            let save = self.pos;
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("use") {
                self.expect_char('(')?;
                loop {
                    self.skip_ws();
                    if self.peek() == Some('&') {
                        self.pos += 1;
                        self.skip_ws();
                    }
                    if self.peek() != Some('$') {
                        break;
                    }
                    let name = self.parse_variable_name()?;
                    let val = self.vars.get(&name).cloned().unwrap_or(Value::Null);
                    captures.push((name, val));
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        continue;
                    }
                    break;
                }
                self.expect_char(')')?;
            } else {
                self.pos = save;
            }
        }
        // optional return type
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
        let body_start;
        if arrow {
            self.skip_ws();
            if !self.starts_with("=>") {
                return Err(EngineError("expected `=>` in arrow function".into()));
            }
            self.pos += 2;
            self.skip_ws();
            body_start = self.pos;
            let prev = self.live;
            self.live = false;
            let _ = self.expression()?; // skip past the expression body
            self.live = prev;
        } else {
            self.skip_ws();
            if self.peek() != Some('{') {
                return Err(EngineError("expected `{` for closure body".into()));
            }
            body_start = self.pos;
            self.pos += 1;
            self.skip_to_block_end()?;
        }
        Ok(Value::Closure(Rc::new(Closure {
            params,
            body_start,
            captures,
            arrow,
        })))
    }

    /// `Class::member` — class constants, `::class`, and static/`self`/`parent`
    /// method calls.
    fn static_access(&mut self, id: &str) -> R<Value> {
        let class = self.resolve_class(id)?;
        self.skip_ws();
        if self.peek() == Some('$') {
            let pname = self.parse_variable_name()?;
            let dc = self
                .find_static_class(&class, &pname)
                .unwrap_or_else(|| class.to_ascii_lowercase());
            let key = (dc, pname);
            self.skip_ws();
            if let Some(aop) = self.peek_assign_op() {
                self.pos += aop.len();
                self.skip_ws();
                let rhs = self.parse_assignment()?;
                if !self.live {
                    return Ok(rhs);
                }
                let newval = if aop == "=" {
                    rhs
                } else {
                    let cur = self.static_props.get(&key).cloned().unwrap_or(Value::Null);
                    self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
                };
                self.static_props.insert(key, newval.clone());
                return Ok(newval);
            }
            if !self.live {
                return Ok(Value::Null);
            }
            return Ok(self.static_props.get(&key).cloned().unwrap_or(Value::Null));
        }
        let member = self
            .try_identifier()
            .ok_or_else(|| EngineError("expected member after `::`".into()))?;
        if member.eq_ignore_ascii_case("class") {
            return Ok(Value::Str(class));
        }
        let after = self.pos;
        self.skip_ws();
        if self.peek() == Some('(') {
            let args = self.parse_args()?;
            if !self.live {
                return Ok(Value::Null);
            }
            // enum built-in static methods: cases() / from() / tryFrom()
            let clower = class.to_ascii_lowercase();
            if self.enum_cases.contains_key(&clower) {
                let ml = member.to_ascii_lowercase();
                if ml == "cases" {
                    let mut a = PArray::default();
                    for (_, obj) in &self.enum_cases[&clower] {
                        a.push(obj.clone());
                    }
                    return Ok(Value::Array(a));
                }
                if ml == "from" || ml == "tryfrom" {
                    let target = args.first().cloned().unwrap_or(Value::Null);
                    for (_, obj) in &self.enum_cases[&clower] {
                        if let Value::Object(o) = obj {
                            let val = o.borrow().get("value");
                            if let Some(v) = val {
                                if loose_eq_d(&v, &target, 0) {
                                    return Ok(obj.clone());
                                }
                            }
                        }
                    }
                    if ml == "tryfrom" {
                        return Ok(Value::Null);
                    }
                    return Err(EngineError(format!(
                        "{}::from(): no case matches the given value",
                        class
                    )));
                }
            }
            let def = self.lookup_method(&class, &member).ok_or_else(|| {
                EngineError(format!("call to undefined method {class}::{member}()"))
            })?;
            // preserve the current $this for self::/parent:: calls in instance context
            let this = self.vars.get("this").cloned();
            let r = self.call_user_function(def, args, this, Some(class))?;
            return self.chain_access(r);
        }
        // class constant
        self.pos = after;
        if !self.live {
            return Ok(Value::Null);
        }
        // enum case access: `EnumName::CaseName`
        if let Some(cases) = self.enum_cases.get(&class.to_ascii_lowercase()) {
            if let Some((_, obj)) = cases.iter().find(|(n, _)| n == &member) {
                let obj = obj.clone();
                return self.chain_access(obj);
            }
        }
        let cpos = match self.lookup_const(&class, &member) {
            Some(pos) => pos,
            None => return Err(EngineError(format!("undefined constant {class}::{member}"))),
        };
        // Guard against self-referencing constants (PHP fatals on these).
        self.call_depth += 1;
        if self.call_depth > 2000 {
            return Err(EngineError("self-referencing constant".into()));
        }
        let save = self.pos;
        self.pos = cpos;
        let v = self.expression()?;
        self.pos = save;
        self.call_depth -= 1;
        Ok(v)
    }

    /// `match (subject) { c1, c2 => r, ..., default => r }` — an expression
    /// using strict `===`; throws UnhandledMatchError if nothing matches.
    fn match_expression(&mut self) -> R<Value> {
        self.expect_char('(')?;
        let subject = self.expression()?;
        self.expect_char(')')?;
        self.skip_ws();
        if self.peek() != Some('{') {
            return Err(EngineError("expected `{` after match(...)".into()));
        }
        self.pos += 1;
        let mut found = false;
        let mut result = Value::Null;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(EngineError("unterminated match".into())),
                _ => {}
            }
            let save = self.pos;
            let mut is_default = false;
            if self.try_identifier().map(|s| s.to_ascii_lowercase()).as_deref() == Some("default") {
                self.skip_ws();
                if self.starts_with("=>") {
                    is_default = true;
                } else {
                    self.pos = save;
                }
            } else {
                self.pos = save;
            }
            let arm_matches = if is_default {
                !found
            } else {
                let mut m = false;
                loop {
                    let cond = if found {
                        let prev = self.live;
                        self.live = false;
                        let v = self.expression()?;
                        self.live = prev;
                        v
                    } else {
                        self.expression()?
                    };
                    if !found && self.live && strict_eq(&subject, &cond) {
                        m = true;
                    }
                    self.skip_ws();
                    if self.peek() == Some(',') {
                        self.pos += 1;
                        self.skip_ws();
                        if self.starts_with("=>") {
                            break;
                        }
                        continue;
                    }
                    break;
                }
                m
            };
            self.skip_ws();
            if !self.starts_with("=>") {
                return Err(EngineError("expected `=>` in match arm".into()));
            }
            self.pos += 2;
            self.skip_ws();
            if arm_matches && !found {
                found = true;
                result = self.expression()?;
            } else {
                let prev = self.live;
                self.live = false;
                let _ = self.expression()?;
                self.live = prev;
            }
            self.skip_ws();
            if self.peek() == Some(',') {
                self.pos += 1;
            }
        }
        if !found && self.live {
            let exc = self.instantiate(
                "UnhandledMatchError",
                vec![Value::Str("Unhandled match case".to_string())],
            )?;
            self.thrown = Some(exc);
            return Err(EngineError("unhandled match".into()));
        }
        Ok(result)
    }

    fn resolve_class(&self, id: &str) -> R<String> {
        match id.to_ascii_lowercase().as_str() {
            "self" | "static" => self
                .current_class
                .clone()
                .ok_or_else(|| EngineError("`self`/`static` used outside class".into())),
            "parent" => {
                let c = self
                    .current_class
                    .as_ref()
                    .ok_or_else(|| EngineError("`parent` used outside class".into()))?;
                self.classes
                    .get(&c.to_ascii_lowercase())
                    .and_then(|cd| cd.parent.clone())
                    .ok_or_else(|| EngineError("class has no parent".into()))
            }
            _ => Ok(id.trim_start_matches('\\').to_string()),
        }
    }

    /// Is `class` a `target` (same class, a subclass, or implements the interface)?
    fn is_instance(&self, class: &str, target: &str) -> bool {
        let t = target.trim_start_matches('\\').to_ascii_lowercase();
        if t == "mixed" {
            return true;
        }
        let mut cur = Some(class.to_ascii_lowercase());
        let mut guard = 0;
        while let Some(cn) = cur {
            guard += 1;
            if guard > 10_000 {
                return false;
            }
            if cn == t {
                return true;
            }
            let cd = match self.classes.get(&cn) {
                Some(c) => c,
                None => return false,
            };
            for iface in &cd.interfaces {
                if self.iface_is(&iface.to_ascii_lowercase(), &t, 0) {
                    return true;
                }
            }
            cur = cd.parent.as_ref().map(|p| p.to_ascii_lowercase());
        }
        false
    }

    fn iface_is(&self, iface: &str, target: &str, depth: usize) -> bool {
        if depth > 1000 {
            return false;
        }
        if iface == target {
            return true;
        }
        if let Some(cd) = self.classes.get(iface) {
            if let Some(p) = &cd.parent {
                if self.iface_is(&p.to_ascii_lowercase(), target, depth + 1) {
                    return true;
                }
            }
            for i in &cd.interfaces {
                if self.iface_is(&i.to_ascii_lowercase(), target, depth + 1) {
                    return true;
                }
            }
        }
        false
    }

    /// Walk the class chain to find which class actually declares static `$name`.
    fn find_static_class(&self, class: &str, name: &str) -> Option<String> {
        let mut cur = Some(class.to_ascii_lowercase());
        let mut guard = 0;
        while let Some(cn) = cur {
            guard += 1;
            if guard > 10_000 {
                return None;
            }
            if self.static_props.contains_key(&(cn.clone(), name.to_string())) {
                return Some(cn);
            }
            cur = self
                .classes
                .get(&cn)
                .and_then(|cd| cd.parent.clone())
                .map(|p| p.to_ascii_lowercase());
        }
        None
    }

    fn lookup_const(&self, class: &str, name: &str) -> Option<usize> {
        let mut cur = Some(class.to_ascii_lowercase());
        let mut guard = 0;
        while let Some(cn) = cur {
            guard += 1;
            if guard > 10_000 {
                return None;
            }
            let cd = self.classes.get(&cn)?;
            if let Some((_, pos)) = cd.consts.iter().find(|(n, _)| n == name) {
                return Some(*pos);
            }
            cur = cd.parent.as_ref().map(|p| p.to_ascii_lowercase());
        }
        None
    }

    /// Stringify a value, invoking `__toString` for objects that define it.
    fn stringify(&mut self, v: &Value) -> R<String> {
        if let Value::Object(o) = v {
            let class = o.borrow().class.clone();
            if let Some(def) = self.lookup_method(&class, "__tostring") {
                let r = self.call_user_function(def, Vec::new(), Some(v.clone()), Some(class))?;
                return Ok(r.to_php_string());
            }
        }
        Ok(v.to_php_string())
    }

    fn assign_property(&mut self, name: &str, prop: &str, aop: &str, rhs: Value) -> R<Value> {
        if !self.live {
            return Ok(rhs);
        }
        match self.vars.get(name).cloned().unwrap_or(Value::Null) {
            Value::Object(o) => {
                let newval = if aop == "=" {
                    rhs
                } else {
                    let cur = o.borrow().get(prop).unwrap_or(Value::Null);
                    self.apply_binary(&aop[..aop.len() - 1], cur, rhs)?
                };
                o.borrow_mut().set(prop, newval.clone());
                Ok(newval)
            }
            _ => Err(EngineError(format!(
                "attempt to assign property `{prop}` on a non-object"
            ))),
        }
    }

    fn primary(&mut self) -> R<Value> {
        self.skip_ws();
        if self.starts_with("<<<") {
            return self.parse_heredoc();
        }
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let v = self.expression()?;
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.pos += 1;
                    self.chain_access(v)
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
                    "print" => {
                        let v = self.parse_unary()?;
                        if self.live {
                            let s = self.stringify(&v)?;
                            self.out.push_str(&s);
                        }
                        Ok(Value::Int(1))
                    }
                    "include" | "require" | "include_once" | "require_once" => {
                        let once = id.ends_with("_once") || id.ends_with("_ONCE");
                        let required = id.to_ascii_lowercase().starts_with("require");
                        let fname = self.parse_ternary()?;
                        self.do_include(&fname.to_php_string(), once, required)
                    }
                    "eval" => {
                        self.skip_ws();
                        self.expect_char('(')?;
                        let code = self.expression()?;
                        self.skip_ws();
                        self.expect_char(')')?;
                        self.do_eval(&code.to_php_string())
                    }
                    "clone" => {
                        let v = self.parse_unary()?;
                        match v {
                            Value::Object(o) => {
                                let (class, props) = {
                                    let b = o.borrow();
                                    (b.class.clone(), b.props.clone())
                                };
                                let newo = Rc::new(RefCell::new(Obj { class: class.clone(), props }));
                                let nv = Value::Object(newo);
                                if let Some(def) = self.lookup_method(&class, "__clone") {
                                    self.call_user_function(def, Vec::new(), Some(nv.clone()), Some(class))?;
                                }
                                Ok(nv)
                            }
                            other => Ok(other),
                        }
                    }
                    "isset" => {
                        self.expect_char('(')?;
                        let mut all = true;
                        loop {
                            let v = self.lvalue_value()?;
                            if !matches!(&v, Some(x) if !matches!(x, Value::Null)) {
                                all = false;
                            }
                            self.skip_ws();
                            if self.peek() == Some(',') {
                                self.pos += 1;
                                continue;
                            }
                            break;
                        }
                        self.expect_char(')')?;
                        Ok(Value::Bool(self.live && all))
                    }
                    "empty" => {
                        self.expect_char('(')?;
                        let v = self.lvalue_value()?.unwrap_or(Value::Null);
                        self.expect_char(')')?;
                        Ok(Value::Bool(!to_bool(&v)))
                    }
                    "unset" => {
                        self.expect_char('(')?;
                        loop {
                            self.unset_one()?;
                            self.skip_ws();
                            if self.peek() == Some(',') {
                                self.pos += 1;
                                continue;
                            }
                            break;
                        }
                        self.expect_char(')')?;
                        Ok(Value::Null)
                    }
                    "function" => self.parse_closure(false),
                    "fn" => self.parse_closure(true),
                    "match" => self.match_expression(),
                    "new" => {
                        self.skip_ws();
                        if self.peek() == Some('\\') {
                            self.pos += 1;
                        }
                        let cname = self
                            .try_identifier()
                            .ok_or_else(|| EngineError("expected class name after `new`".into()))?;
                        self.skip_ws();
                        let args = if self.peek() == Some('(') {
                            self.parse_args()?
                        } else {
                            Vec::new()
                        };
                        self.instantiate(&cname, args)
                    }
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
                        if self.starts_with("::") {
                            self.pos += 2;
                            self.static_access(&id)
                        } else if self.peek() == Some('(') {
                            let lid = id.to_ascii_lowercase();
                            if lid == "preg_match" || lid == "preg_match_all" {
                                self.preg_match_call(lid == "preg_match_all")
                            } else if is_byref_builtin(&id) {
                                self.byref_call(&id)
                            } else {
                                let args = self.parse_args()?;
                                self.call_function(&id, args)
                            }
                        } else {
                            self.pos = after;
                            if let Some(v) = self.magic_constant(&id) {
                                Ok(v)
                            } else if matches!(id.as_str(), "STDIN" | "STDOUT" | "STDERR") {
                                let kind = match id.as_str() {
                                    "STDOUT" => "stdout",
                                    "STDERR" => "stderr",
                                    _ => "stdin",
                                };
                                Ok(Self::make_stream(kind, "", "w", String::new()))
                            } else if let Some(v) = self.consts.get(&id) {
                                Ok(v.clone())
                            } else if let Some(v) = php_constant(&id) {
                                Ok(v)
                            } else if !self.live {
                                Ok(Value::Null)
                            } else {
                                Err(EngineError(format!("undefined constant `{id}`")))
                            }
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
        enum Acc {
            Index(Value),
            Prop(String),
            Method(String, Vec<Value>),
            Call(Vec<Value>),
        }
        let name = self.parse_variable_name()?;
        let mut accs: Vec<Acc> = Vec::new();
        loop {
            let after = self.pos;
            self.skip_ws();
            if self.peek() == Some('[') {
                self.pos += 1;
                self.skip_ws();
                if self.peek() == Some(']') {
                    return Err(EngineError("cannot use [] for reading".into()));
                }
                let k = self.expression()?;
                self.expect_char(']')?;
                accs.push(Acc::Index(k));
            } else if self.starts_with("->") {
                self.pos += 2;
                self.skip_ws();
                let member = self
                    .try_identifier()
                    .ok_or_else(|| EngineError("expected name after `->`".into()))?;
                let a2 = self.pos;
                self.skip_ws();
                if self.peek() == Some('(') {
                    let args = self.parse_args()?;
                    accs.push(Acc::Method(member, args));
                } else {
                    self.pos = a2;
                    accs.push(Acc::Prop(member));
                }
            } else if self.peek() == Some('(') {
                let args = self.parse_args()?;
                accs.push(Acc::Call(args));
            } else {
                self.pos = after;
                break;
            }
        }

        // Plain variable: maybe post-increment/decrement.
        if accs.is_empty() {
            let cur = self.vars.get(&name).cloned().unwrap_or(Value::Null);
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
            return Ok(cur);
        }

        // Resolve the *leading* run of `[index]` accesses by reference so we
        // never clone a whole array container — only the small leaf element.
        // (Skipped when the base is an object, e.g. ArrayAccess, so each index
        // dispatches through offsetGet below.)
        let base_is_object = matches!(self.vars.get(&name), Some(Value::Object(_)));
        let mut i = 0;
        let mut cur = if base_is_object {
            self.vars.get(&name).cloned().unwrap_or(Value::Null)
        } else {
            let mut keys: Vec<AKey> = Vec::new();
            while let Some(Acc::Index(k)) = accs.get(i) {
                keys.push(key_from_value(k));
                i += 1;
            }
            self.read_keys(&name, &keys)
        };
        // Apply remaining accesses on the now-small value.
        while i < accs.len() {
            cur = match &accs[i] {
                Acc::Index(k) => match &cur {
                    Value::Array(a) => a.get(&key_from_value(k)).cloned().unwrap_or(Value::Null),
                    Value::Str(s) => match key_from_value(k) {
                        AKey::Int(n) => s
                            .chars()
                            .nth(n as usize)
                            .map(|c| Value::Str(c.to_string()))
                            .unwrap_or(Value::Str(String::new())),
                        _ => Value::Null,
                    },
                    Value::Object(_) => self.call_method(&cur, "offsetGet", vec![k.clone()])?,
                    _ => Value::Null,
                },
                Acc::Prop(p) => read_property(&cur, p),
                Acc::Method(m, args) => self.call_method(&cur, m, args.clone())?,
                Acc::Call(args) => self.call_callable(&cur, args.clone())?,
            };
            i += 1;
        }

        // Post-increment/decrement on a property (`$o->p++`) or index
        // (`$a[k]++`) lvalue — single-level only.
        let after = self.pos;
        self.skip_ws();
        if self.starts_with("++") || self.starts_with("--") {
            let inc = self.starts_with("++");
            let single_prop = matches!(accs.as_slice(), [Acc::Prop(_)]);
            let all_index = !accs.is_empty() && accs.iter().all(|a| matches!(a, Acc::Index(_)));
            if single_prop || all_index {
                self.pos += 2;
                if self.live {
                    let nv = self.inc_dec(&cur, inc);
                    if let [Acc::Prop(p)] = accs.as_slice() {
                        self.assign_property(&name, p, "=", nv)?;
                    } else {
                        let indices: Vec<Option<Value>> = accs
                            .iter()
                            .map(|a| match a {
                                Acc::Index(k) => Some(k.clone()),
                                _ => None,
                            })
                            .collect();
                        self.assign_indexed(name, indices, "=", nv)?;
                    }
                }
                return Ok(cur); // post-inc/dec yields the OLD value
            }
        }
        self.pos = after;
        Ok(cur)
    }

    /// Resolve a variable + index/prop chain for `isset`/`empty` without
    /// erroring on undefined (returns `None` if any level is missing).
    fn lvalue_value(&mut self) -> R<Option<Value>> {
        self.skip_ws();
        if self.peek() != Some('$') {
            return Ok(Some(self.expression()?)); // empty() allows any expression
        }
        let name = self.parse_variable_name()?;
        let mut cur: Option<Value> = if self.live {
            self.vars.get(&name).cloned()
        } else {
            None
        };
        loop {
            let after = self.pos;
            self.skip_ws();
            if self.peek() == Some('[') {
                self.pos += 1;
                self.skip_ws();
                let k = self.expression()?;
                self.expect_char(']')?;
                let key = key_from_value(&k);
                cur = match cur {
                    Some(Value::Array(a)) => a.get(&key).cloned(),
                    Some(obj @ Value::Object(_)) => {
                        if to_bool(&self.call_method(&obj, "offsetExists", vec![k.clone()])?) {
                            Some(self.call_method(&obj, "offsetGet", vec![k])?)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
            } else if self.starts_with("->") {
                self.pos += 2;
                self.skip_ws();
                let prop = self
                    .try_identifier()
                    .ok_or_else(|| EngineError("expected property after `->`".into()))?;
                cur = match cur {
                    Some(Value::Object(o)) => o.borrow().get(&prop),
                    _ => None,
                };
            } else {
                self.pos = after;
                break;
            }
        }
        Ok(cur)
    }

    /// Parse and remove a single `unset()` target (`$x`, `$a[k]`, `$o->p`).
    fn unset_one(&mut self) -> R<()> {
        self.skip_ws();
        if self.peek() != Some('$') {
            let _ = self.expression()?;
            return Ok(());
        }
        let name = self.parse_variable_name()?;
        // Consume a full chain of `->prop` / `[k]` so parsing always succeeds.
        enum Seg {
            Prop(String),
            Index(Value),
        }
        let mut segs: Vec<Seg> = Vec::new();
        loop {
            let after = self.pos;
            self.skip_ws();
            if self.peek() == Some('[') {
                self.pos += 1;
                self.skip_ws();
                let k = self.expression()?;
                self.expect_char(']')?;
                segs.push(Seg::Index(k));
            } else if self.starts_with("->") {
                self.pos += 2;
                self.skip_ws();
                let prop = self
                    .try_identifier()
                    .ok_or_else(|| EngineError("expected property after `->`".into()))?;
                segs.push(Seg::Prop(prop));
            } else {
                self.pos = after;
                break;
            }
        }
        if !self.live {
            return Ok(());
        }
        match segs.as_slice() {
            [] => {
                self.vars.remove(&name);
            }
            [Seg::Index(k)] => match self.vars.get(&name) {
                Some(Value::Object(_)) => {
                    let obj = self.vars.get(&name).cloned().unwrap();
                    self.call_method(&obj, "offsetUnset", vec![k.clone()])?;
                }
                _ => {
                    let key = key_from_value(k);
                    if let Some(Value::Array(a)) = self.vars.get_mut(&name) {
                        a.remove(&key);
                    }
                }
            },
            [Seg::Prop(p)] => {
                if let Some(Value::Object(o)) = self.vars.get(&name) {
                    o.borrow_mut().remove(p);
                }
            }
            [Seg::Prop(p), Seg::Index(k)] => {
                if let Some(Value::Object(o)) = self.vars.get(&name).cloned() {
                    let mut arr = o.borrow().get(p).unwrap_or(Value::Null);
                    if let Value::Array(a) = &mut arr {
                        a.remove(&key_from_value(k));
                        o.borrow_mut().set(p, arr);
                    } else if matches!(arr, Value::Object(_)) {
                        self.call_method(&arr, "offsetUnset", vec![k.clone()])?;
                    }
                }
            }
            _ => {} // deeper chains: tokens consumed, best-effort no-op
        }
        Ok(())
    }

    /// Read `$name` then a run of index keys by reference, cloning only the leaf.
    fn read_keys(&self, name: &str, keys: &[AKey]) -> Value {
        let mut cur = match self.vars.get(name) {
            Some(v) => v,
            None => return Value::Null,
        };
        for key in keys {
            cur = match cur {
                Value::Array(a) => match a.get(key) {
                    Some(v) => v,
                    None => return Value::Null,
                },
                Value::Str(s) => {
                    return match key {
                        AKey::Int(n) => s
                            .chars()
                            .nth(*n as usize)
                            .map(|c| Value::Str(c.to_string()))
                            .unwrap_or(Value::Str(String::new())),
                        _ => Value::Null,
                    }
                }
                _ => return Value::Null,
            };
        }
        cur.clone()
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
        // hex / binary / octal prefixes (with optional `_` digit separators)
        if self.peek() == Some('0') {
            let radix = match self.peek_at(1) {
                Some('x' | 'X') => Some(16),
                Some('b' | 'B') => Some(2),
                Some('o' | 'O') => Some(8),
                _ => None,
            };
            if let Some(r) = radix {
                self.pos += 2;
                let ds = self.pos;
                while matches!(self.peek(), Some(c) if c.is_digit(r) || c == '_') {
                    self.pos += 1;
                }
                let t: String = self.src[ds..self.pos].iter().filter(|c| **c != '_').collect();
                return Value::Int(i64::from_str_radix(&t, r).unwrap_or(0));
            }
        }
        let start = self.pos;
        let digit = |c: char| c.is_ascii_digit() || c == '_';
        while matches!(self.peek(), Some(c) if digit(c)) {
            self.pos += 1;
        }
        let mut is_float = false;
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(c) if digit(c)) {
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
                while matches!(self.peek(), Some(c) if digit(c)) {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }
        let text: String = self.src[start..self.pos].iter().filter(|c| **c != '_').collect();
        // legacy octal literal: a leading 0 followed by only octal digits
        if !is_float
            && text.len() > 1
            && text.starts_with('0')
            && text.chars().all(|c| ('0'..='7').contains(&c))
        {
            if let Ok(n) = i64::from_str_radix(&text, 8) {
                return Value::Int(n);
            }
        }
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
                let sv = self.stringify(&value)?;
                s.push_str(&sv);
                continue;
            }
            s.push(c);
            self.pos += 1;
        }
        Err(EngineError("unterminated double-quoted string".into()))
    }

    fn read_label(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        self.src[start..self.pos].iter().collect()
    }

    /// `<<<EOT … EOT` (heredoc, interpolated) or `<<<'EOT' … EOT` (nowdoc, raw),
    /// with PHP 7.3 flexible (indented) closing markers.
    fn parse_heredoc(&mut self) -> R<Value> {
        self.pos += 3; // <<<
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.pos += 1;
        }
        let nowdoc = match self.peek() {
            Some('\'') => {
                self.pos += 1;
                true
            }
            Some('"') => {
                self.pos += 1;
                false
            }
            _ => false,
        };
        let label = self.read_label();
        if label.is_empty() {
            return Err(EngineError("invalid heredoc label".into()));
        }
        if matches!(self.peek(), Some('\'') | Some('"')) {
            self.pos += 1;
        }
        // consume the rest of the opening line
        while matches!(self.peek(), Some(c) if c != '\n') {
            self.pos += 1;
        }
        if self.peek() == Some('\n') {
            self.pos += 1;
        }
        let label_len = label.chars().count();
        let mut lines: Vec<String> = Vec::new();
        let mut closing_indent = 0;
        loop {
            let line_start = self.pos;
            let mut ws = 0;
            while matches!(self.peek(), Some(' ') | Some('\t')) {
                self.pos += 1;
                ws += 1;
            }
            if self.starts_with(&label) {
                let after = self.pos + label_len;
                let next = self.src.get(after).copied();
                if !matches!(next, Some(c) if c.is_ascii_alphanumeric() || c == '_') {
                    self.pos = after; // consume the label; leave the rest (`;` etc.)
                    closing_indent = ws;
                    break;
                }
            }
            // not the closing marker — take the whole line as content
            self.pos = line_start;
            let mut line = String::new();
            while matches!(self.peek(), Some(c) if c != '\n') {
                line.push(self.src[self.pos]);
                self.pos += 1;
            }
            if self.peek() == Some('\n') {
                self.pos += 1;
            }
            lines.push(line);
            if self.pos >= self.src.len() {
                break;
            }
        }
        let mut content = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.push('\n');
            }
            content.extend(line.chars().skip(closing_indent)); // strip closing indent (7.3+)
        }
        if nowdoc {
            Ok(Value::Str(content))
        } else {
            Ok(Value::Str(self.interpolate(&content)?))
        }
    }

    /// Process `$var` interpolation and escape sequences in a string (used by
    /// heredoc; mirrors double-quoted semantics minus `\"`).
    fn interpolate(&mut self, s: &str) -> R<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                let (ch, adv) = match chars[i + 1] {
                    'n' => ('\n', 2),
                    't' => ('\t', 2),
                    'r' => ('\r', 2),
                    '\\' => ('\\', 2),
                    '$' => ('$', 2),
                    _ => ('\\', 1),
                };
                out.push(ch);
                i += adv;
                continue;
            }
            if c == '$' && matches!(chars.get(i + 1), Some(d) if d.is_ascii_alphabetic() || *d == '_') {
                let start = i + 1;
                let mut j = start;
                while matches!(chars.get(j), Some(d) if d.is_ascii_alphanumeric() || *d == '_') {
                    j += 1;
                }
                let name: String = chars[start..j].iter().collect();
                let v = self.vars.get(&name).cloned().unwrap_or(Value::Null);
                let sv = self.stringify(&v)?;
                out.push_str(&sv);
                i = j;
                continue;
            }
            out.push(c);
            i += 1;
        }
        Ok(out)
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
        Value::Object(_) => AKey::Str(String::new()),
        Value::Closure(_) => AKey::Str(String::new()),
    }
}

fn akey_to_value(k: &AKey) -> Value {
    match k {
        AKey::Int(i) => Value::Int(*i),
        AKey::Str(s) => Value::Str(s.clone()),
    }
}

/// Read a value following an index path (returns Null on any miss / append).
fn index_get(v: &Value, indices: &[Option<Value>]) -> Value {
    let mut cur = v;
    for idx in indices {
        let key = match idx {
            Some(k) => key_from_value(k),
            None => return Value::Null,
        };
        cur = match cur {
            Value::Array(a) => match a.get(&key) {
                Some(x) => x,
                None => return Value::Null,
            },
            _ => return Value::Null,
        };
    }
    cur.clone()
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

fn read_property(v: &Value, name: &str) -> Value {
    match v {
        Value::Object(o) => o.borrow().get(name).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

// ---- hashing / encoding ----------------------------------------------------

fn md5_hex(msg: &[u8]) -> String {
    #[rustfmt::skip]
    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // K[i] = floor(2^32 * abs(sin(i+1)))
    let mut k = [0u32; 64];
    for (i, kv) in k.iter_mut().enumerate() {
        *kv = (2f64.powi(32) * ((i as f64 + 1.0).sin().abs())).floor() as u32;
    }
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    let mut m = msg.to_vec();
    let bits = (msg.len() as u64).wrapping_mul(8);
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&bits.to_le_bytes());
    for chunk in m.chunks(64) {
        let mut w = [0u32; 16];
        for (i, wv) in w.iter_mut().enumerate() {
            *wv = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(k[i])
                .wrapping_add(w[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(s[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = String::with_capacity(32);
    for v in [a0, b0, c0, d0] {
        for byte in v.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

fn sha1_hex(msg: &[u8]) -> String {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut m = msg.to_vec();
    let bits = (msg.len() as u64).wrapping_mul(8);
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&bits.to_be_bytes());
    for chunk in m.chunks(64) {
        let mut w = [0u32; 80];
        for (i, wv) in w.iter_mut().take(16).enumerate() {
            *wv = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = if i < 20 {
                ((b & c) | (!b & d), 0x5A82_7999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9_EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC)
            } else {
                (b ^ c ^ d, 0xCA62_C1D6)
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = String::with_capacity(40);
    for v in h {
        out.push_str(&format!("{v:08x}"));
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Vec<u8> {
    let dec = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let clean: Vec<u32> = s.bytes().filter_map(dec).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let n = (chunk[0] << 18)
            | (chunk[1] << 12)
            | (chunk.get(2).copied().unwrap_or(0) << 6)
            | chunk.get(3).copied().unwrap_or(0);
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn is_byref_builtin(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "sort"
            | "rsort"
            | "asort"
            | "arsort"
            | "ksort"
            | "krsort"
            | "usort"
            | "uasort"
            | "uksort"
            | "array_push"
            | "array_pop"
            | "array_shift"
            | "array_unshift"
    )
}

/// Read a property off a stream handle object.
fn stream_get(h: &Value, k: &str) -> Option<Value> {
    match h {
        Value::Object(o) => o.borrow().get(k),
        _ => None,
    }
}

/// Set a property on a stream handle object.
fn stream_set(h: &Value, k: &str, v: Value) {
    if let Value::Object(o) = h {
        o.borrow_mut().set(k, v);
    }
}

/// Rebuild an array after a shift/unshift: integer keys renumber, string keys stay.
fn reindex(entries: Vec<(AKey, Value)>) -> PArray {
    let mut na = PArray::default();
    for (k, v) in entries {
        match k {
            AKey::Int(_) => na.push(v),
            AKey::Str(s) => na.set(AKey::Str(s), v),
        }
    }
    na
}

// ---- Regex engine (from-scratch, backtracking VM) --------------------------
//
// A small PCRE-ish engine compiled to a recursive backtracking bytecode VM.
// Supports: literals, `.`, char classes `[...]` (+ `\d \w \s` and negations),
// anchors `^ $`, word boundaries `\b \B`, groups `()` (capturing, `(?:)`
// non-capturing, named `(?P<n>)`/`(?<n>)`), alternation `|`, quantifiers
// `* + ? {n,m}` (greedy + lazy `?`), backreferences `\1`, and lookahead
// `(?=)`/`(?!)`. A global step budget guards against catastrophic backtracking.

#[derive(Clone)]
enum ClassItem {
    Ch(char),
    Range(char, char),
    Pre(char), // 'd' 'D' 'w' 'W' 's' 'S'
}

#[derive(Clone)]
enum Re {
    Empty,
    Char(char),
    Any,
    Class(bool, Vec<ClassItem>), // negated, items
    Start,
    End,
    WordB(bool), // true = \b, false = \B
    Backref(usize),
    Group(Option<usize>, Box<Re>),
    Look(bool, Box<Re>), // true = negative lookahead
    Alt(Box<Re>, Box<Re>),
    Concat(Vec<Re>),
    Star(Box<Re>, bool), // greedy
    Plus(Box<Re>, bool),
    Quest(Box<Re>, bool),
    Repeat(Box<Re>, usize, Option<usize>, bool),
}

#[derive(Clone)]
enum Inst {
    Char(char),
    Any,
    Class(bool, Vec<ClassItem>),
    Save(usize),
    Split(usize, usize),
    Jmp(usize),
    Start,
    End,
    WordB(bool),
    Backref(usize),
    Look(bool, Vec<Inst>),
    Match,
}

#[derive(Clone, Copy)]
struct RxFlags {
    ci: bool,
    dotall: bool,
    multiline: bool,
}

pub struct Rx {
    prog: Vec<Inst>,
    ngroups: usize,
    flags: RxFlags,
    names: Vec<(String, usize)>,
    anchored: bool, // pattern begins with ^ (non-multiline) — only try at start
}

struct ReParser {
    c: Vec<char>,
    i: usize,
    ngroups: usize,
    names: Vec<(String, usize)>,
    extended: bool,
}

impl ReParser {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }
    fn at(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }
    fn skip_x_ws(&mut self) {
        if !self.extended {
            return;
        }
        loop {
            match self.peek() {
                Some(ch) if ch.is_whitespace() => self.i += 1,
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn alt(&mut self) -> Re {
        let mut left = self.concat();
        while self.peek() == Some('|') {
            self.i += 1;
            let right = self.concat();
            left = Re::Alt(Box::new(left), Box::new(right));
        }
        left
    }

    fn concat(&mut self) -> Re {
        let mut items = Vec::new();
        loop {
            self.skip_x_ws();
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            if let Some(node) = self.quant() {
                items.push(node);
            } else {
                break;
            }
        }
        if items.is_empty() {
            Re::Empty
        } else if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Re::Concat(items)
        }
    }

    fn quant(&mut self) -> Option<Re> {
        let atom = self.atom()?;
        self.skip_x_ws();
        let node = match self.peek() {
            Some('*') => {
                self.i += 1;
                Re::Star(Box::new(atom), self.greedy())
            }
            Some('+') => {
                self.i += 1;
                Re::Plus(Box::new(atom), self.greedy())
            }
            Some('?') => {
                self.i += 1;
                Re::Quest(Box::new(atom), self.greedy())
            }
            Some('{') => {
                if let Some((min, max)) = self.try_brace() {
                    Re::Repeat(Box::new(atom), min, max, self.greedy())
                } else {
                    atom
                }
            }
            _ => atom,
        };
        Some(node)
    }

    fn greedy(&mut self) -> bool {
        match self.peek() {
            Some('?') => {
                self.i += 1;
                false
            }
            Some('+') => {
                self.i += 1;
                true // possessive — approximate as greedy
            }
            _ => true,
        }
    }

    fn try_brace(&mut self) -> Option<(usize, Option<usize>)> {
        let save = self.i;
        self.i += 1; // consume {
        let min = self.read_int();
        let (min, max) = match self.peek() {
            Some('}') if min.is_some() => {
                let m = min.unwrap();
                (m, Some(m))
            }
            Some(',') => {
                self.i += 1;
                let max = self.read_int();
                (min.unwrap_or(0), max)
            }
            _ => {
                self.i = save;
                return None;
            }
        };
        if self.peek() != Some('}') {
            self.i = save;
            return None;
        }
        self.i += 1; // consume }
        let max = max.map(|m| m.min(min.max(1000)).max(min)); // cap explosion
        Some((min.min(1000), max))
    }

    fn read_int(&mut self) -> Option<usize> {
        let start = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            None
        } else {
            self.c[start..self.i]
                .iter()
                .collect::<String>()
                .parse()
                .ok()
        }
    }

    fn atom(&mut self) -> Option<Re> {
        self.skip_x_ws();
        let ch = self.peek()?;
        match ch {
            '(' => self.group(),
            '[' => self.class(),
            '.' => {
                self.i += 1;
                Some(Re::Any)
            }
            '^' => {
                self.i += 1;
                Some(Re::Start)
            }
            '$' => {
                self.i += 1;
                Some(Re::End)
            }
            '\\' => self.escape(),
            ')' | '|' => None,
            _ => {
                self.i += 1;
                Some(Re::Char(ch))
            }
        }
    }

    fn group(&mut self) -> Option<Re> {
        self.i += 1; // consume (
        let mut capturing = true;
        let mut name: Option<String> = None;
        let mut look: Option<bool> = None; // Some(neg) for lookahead
        let mut lookbehind = false;
        if self.peek() == Some('?') {
            self.i += 1;
            match self.peek() {
                Some(':') => {
                    self.i += 1;
                    capturing = false;
                }
                Some('=') => {
                    self.i += 1;
                    capturing = false;
                    look = Some(false);
                }
                Some('!') => {
                    self.i += 1;
                    capturing = false;
                    look = Some(true);
                }
                Some('P') => {
                    self.i += 1;
                    if self.peek() == Some('<') {
                        self.i += 1;
                        name = Some(self.read_name('>'));
                    }
                }
                Some('<') => {
                    // (?<name>) named, OR (?<= / (?<! lookbehind
                    if matches!(self.at(1), Some('=') | Some('!')) {
                        self.i += 2; // consume < and =/!
                        capturing = false;
                        lookbehind = true; // approximated as always-pass zero-width
                    } else {
                        self.i += 1;
                        name = Some(self.read_name('>'));
                    }
                }
                Some('\'') => {
                    self.i += 1;
                    name = Some(self.read_name('\''));
                }
                _ => {
                    // unknown construct — treat as non-capturing
                    capturing = false;
                }
            }
        }
        let idx = if capturing {
            self.ngroups += 1;
            if let Some(n) = &name {
                self.names.push((n.clone(), self.ngroups));
            }
            Some(self.ngroups)
        } else {
            None
        };
        let inner = self.alt();
        if self.peek() == Some(')') {
            self.i += 1;
        }
        if lookbehind {
            // approximate: zero-width assertion that always passes
            return Some(Re::Empty);
        }
        if let Some(neg) = look {
            return Some(Re::Look(neg, Box::new(inner)));
        }
        Some(Re::Group(idx, Box::new(inner)))
    }

    fn read_name(&mut self, close: char) -> String {
        let start = self.i;
        while let Some(ch) = self.peek() {
            if ch == close {
                break;
            }
            self.i += 1;
        }
        let nm: String = self.c[start..self.i].iter().collect();
        if self.peek() == Some(close) {
            self.i += 1;
        }
        nm
    }

    fn class(&mut self) -> Option<Re> {
        self.i += 1; // consume [
        let mut neg = false;
        if self.peek() == Some('^') {
            neg = true;
            self.i += 1;
        }
        let mut items: Vec<ClassItem> = Vec::new();
        // leading ] is a literal
        if self.peek() == Some(']') {
            items.push(ClassItem::Ch(']'));
            self.i += 1;
        }
        while let Some(ch) = self.peek() {
            if ch == ']' {
                self.i += 1;
                break;
            }
            if ch == '\\' {
                self.i += 1;
                if let Some(e) = self.peek() {
                    self.i += 1;
                    match e {
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                            items.push(ClassItem::Pre(e));
                            continue;
                        }
                        _ => {
                            let lo = class_escape_char(e);
                            // possible range with escaped lo
                            if self.peek() == Some('-')
                                && self.at(1).is_some()
                                && self.at(1) != Some(']')
                            {
                                self.i += 1;
                                let hi = self.read_class_char();
                                items.push(ClassItem::Range(lo, hi));
                            } else {
                                items.push(ClassItem::Ch(lo));
                            }
                            continue;
                        }
                    }
                }
                continue;
            }
            self.i += 1;
            if self.peek() == Some('-') && self.at(1).is_some() && self.at(1) != Some(']') {
                self.i += 1; // consume -
                let hi = self.read_class_char();
                items.push(ClassItem::Range(ch, hi));
            } else {
                items.push(ClassItem::Ch(ch));
            }
        }
        Some(Re::Class(neg, items))
    }

    fn read_class_char(&mut self) -> char {
        match self.peek() {
            Some('\\') => {
                self.i += 1;
                let e = self.peek().unwrap_or('\\');
                self.i += 1;
                class_escape_char(e)
            }
            Some(ch) => {
                self.i += 1;
                ch
            }
            None => '\0',
        }
    }

    fn escape(&mut self) -> Option<Re> {
        self.i += 1; // consume backslash
        let e = self.peek()?;
        self.i += 1;
        let node = match e {
            'd' => Re::Class(false, vec![ClassItem::Pre('d')]),
            'D' => Re::Class(false, vec![ClassItem::Pre('D')]),
            'w' => Re::Class(false, vec![ClassItem::Pre('w')]),
            'W' => Re::Class(false, vec![ClassItem::Pre('W')]),
            's' => Re::Class(false, vec![ClassItem::Pre('s')]),
            'S' => Re::Class(false, vec![ClassItem::Pre('S')]),
            'b' => Re::WordB(true),
            'B' => Re::WordB(false),
            'A' => Re::Start,
            'z' | 'Z' => Re::End,
            'n' => Re::Char('\n'),
            't' => Re::Char('\t'),
            'r' => Re::Char('\r'),
            'f' => Re::Char('\x0c'),
            'v' => Re::Char('\x0b'),
            '0' => Re::Char('\0'),
            '1'..='9' => {
                // backreference (read full number)
                let mut n = e as usize - '0' as usize;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    n = n * 10 + (self.peek().unwrap() as usize - '0' as usize);
                    self.i += 1;
                }
                Re::Backref(n)
            }
            'x' => {
                // \xHH
                let mut hex = String::new();
                while hex.len() < 2 && matches!(self.peek(), Some(c) if c.is_ascii_hexdigit()) {
                    hex.push(self.peek().unwrap());
                    self.i += 1;
                }
                let code = u32::from_str_radix(&hex, 16).unwrap_or(0);
                Re::Char(char::from_u32(code).unwrap_or('\0'))
            }
            other => Re::Char(other),
        };
        Some(node)
    }
}

fn class_escape_char(e: char) -> char {
    match e {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        'f' => '\x0c',
        'v' => '\x0b',
        '0' => '\0',
        other => other,
    }
}

fn rx_is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn rx_ceq(a: char, b: char, ci: bool) -> bool {
    a == b || (ci && a.to_ascii_lowercase() == b.to_ascii_lowercase())
}

fn class_item_match(it: &ClassItem, c: char, ci: bool) -> bool {
    match it {
        ClassItem::Ch(x) => rx_ceq(c, *x, ci),
        ClassItem::Range(lo, hi) => {
            (*lo <= c && c <= *hi)
                || (ci && {
                    let cl = c.to_ascii_lowercase();
                    let cu = c.to_ascii_uppercase();
                    (*lo <= cl && cl <= *hi) || (*lo <= cu && cu <= *hi)
                })
        }
        ClassItem::Pre(p) => match p {
            'd' => c.is_ascii_digit(),
            'D' => !c.is_ascii_digit(),
            'w' => rx_is_word(c),
            'W' => !rx_is_word(c),
            's' => c.is_ascii_whitespace(),
            'S' => !c.is_ascii_whitespace(),
            _ => false,
        },
    }
}

fn class_matches(neg: bool, items: &[ClassItem], c: char, ci: bool) -> bool {
    let mut m = false;
    for it in items {
        if class_item_match(it, c, ci) {
            m = true;
            break;
        }
    }
    m ^ neg
}

const RX_STEP_BUDGET: usize = 2_000_000;
const RX_DEPTH_CAP: usize = 40_000;

struct RxCtx<'a> {
    text: &'a [char],
    flags: RxFlags,
    steps: usize,
}

fn rx_run(prog: &[Inst], pc: usize, sp: usize, slots: &mut Vec<usize>, ctx: &mut RxCtx, depth: usize) -> bool {
    ctx.steps += 1;
    if ctx.steps > RX_STEP_BUDGET || depth > RX_DEPTH_CAP {
        return false;
    }
    let text = ctx.text;
    let flags = ctx.flags;
    let d = depth + 1;
    match &prog[pc] {
        Inst::Match => true,
        Inst::Char(c) => {
            sp < text.len()
                && rx_ceq(text[sp], *c, flags.ci)
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Any => {
            sp < text.len()
                && (flags.dotall || text[sp] != '\n')
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Class(neg, items) => {
            sp < text.len()
                && class_matches(*neg, items, text[sp], flags.ci)
                && rx_run(prog, pc + 1, sp + 1, slots, ctx, d)
        }
        Inst::Save(n) => {
            let n = *n;
            let old = if n < slots.len() {
                let o = slots[n];
                slots[n] = sp;
                o
            } else {
                usize::MAX
            };
            if rx_run(prog, pc + 1, sp, slots, ctx, d) {
                true
            } else {
                if n < slots.len() {
                    slots[n] = old;
                }
                false
            }
        }
        Inst::Jmp(x) => rx_run(prog, *x, sp, slots, ctx, d),
        Inst::Split(x, y) => {
            let (x, y) = (*x, *y);
            rx_run(prog, x, sp, slots, ctx, d) || rx_run(prog, y, sp, slots, ctx, d)
        }
        Inst::Start => {
            let ok = sp == 0 || (flags.multiline && sp > 0 && text[sp - 1] == '\n');
            ok && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::End => {
            let ok = sp == text.len()
                || (flags.multiline && text[sp] == '\n')
                || (!flags.multiline && sp + 1 == text.len() && text[sp] == '\n');
            ok && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::WordB(want) => {
            let before = sp > 0 && rx_is_word(text[sp - 1]);
            let after = sp < text.len() && rx_is_word(text[sp]);
            let boundary = before != after;
            (boundary == *want) && rx_run(prog, pc + 1, sp, slots, ctx, d)
        }
        Inst::Backref(n) => {
            let (gs, ge) = if 2 * n + 1 < slots.len() {
                (slots[2 * n], slots[2 * n + 1])
            } else {
                (usize::MAX, usize::MAX)
            };
            if gs == usize::MAX || ge == usize::MAX {
                return rx_run(prog, pc + 1, sp, slots, ctx, d);
            }
            let len = ge - gs;
            if sp + len <= text.len() && (0..len).all(|k| rx_ceq(text[sp + k], text[gs + k], flags.ci))
            {
                rx_run(prog, pc + 1, sp + len, slots, ctx, d)
            } else {
                false
            }
        }
        Inst::Look(neg, sub) => {
            let neg = *neg;
            let snapshot = slots.clone();
            let ok = rx_run(sub, 0, sp, slots, ctx, d);
            if ok != neg {
                if neg {
                    *slots = snapshot;
                }
                rx_run(prog, pc + 1, sp, slots, ctx, d)
            } else {
                *slots = snapshot;
                false
            }
        }
    }
}

fn rx_emit(re: &Re, prog: &mut Vec<Inst>) {
    match re {
        Re::Empty => {}
        Re::Char(c) => prog.push(Inst::Char(*c)),
        Re::Any => prog.push(Inst::Any),
        Re::Class(neg, items) => prog.push(Inst::Class(*neg, items.clone())),
        Re::Start => prog.push(Inst::Start),
        Re::End => prog.push(Inst::End),
        Re::WordB(b) => prog.push(Inst::WordB(*b)),
        Re::Backref(n) => prog.push(Inst::Backref(*n)),
        Re::Concat(v) => {
            for r in v {
                rx_emit(r, prog);
            }
        }
        Re::Group(idx, inner) => {
            if let Some(i) = idx {
                prog.push(Inst::Save(2 * i));
                rx_emit(inner, prog);
                prog.push(Inst::Save(2 * i + 1));
            } else {
                rx_emit(inner, prog);
            }
        }
        Re::Look(neg, inner) => {
            let mut sub = Vec::new();
            rx_emit(inner, &mut sub);
            sub.push(Inst::Match);
            prog.push(Inst::Look(*neg, sub));
        }
        Re::Alt(a, b) => {
            let split = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(a, prog);
            let jmp = prog.len();
            prog.push(Inst::Jmp(0));
            let l2 = prog.len();
            rx_emit(b, prog);
            let l3 = prog.len();
            prog[split] = Inst::Split(split + 1, l2);
            prog[jmp] = Inst::Jmp(l3);
        }
        Re::Star(inner, greedy) => {
            let l1 = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(inner, prog);
            prog.push(Inst::Jmp(l1));
            let l3 = prog.len();
            prog[l1] = if *greedy {
                Inst::Split(l1 + 1, l3)
            } else {
                Inst::Split(l3, l1 + 1)
            };
        }
        Re::Plus(inner, greedy) => {
            let l1 = prog.len();
            rx_emit(inner, prog);
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            let l3 = prog.len();
            prog[sp] = if *greedy {
                Inst::Split(l1, l3)
            } else {
                Inst::Split(l3, l1)
            };
        }
        Re::Quest(inner, greedy) => {
            let sp = prog.len();
            prog.push(Inst::Split(0, 0));
            rx_emit(inner, prog);
            let l3 = prog.len();
            prog[sp] = if *greedy {
                Inst::Split(sp + 1, l3)
            } else {
                Inst::Split(l3, sp + 1)
            };
        }
        Re::Repeat(inner, min, max, greedy) => {
            for _ in 0..*min {
                rx_emit(inner, prog);
            }
            match max {
                None => rx_emit(&Re::Star(inner.clone(), *greedy), prog),
                Some(mx) => {
                    for _ in *min..*mx {
                        rx_emit(&Re::Quest(inner.clone(), *greedy), prog);
                    }
                }
            }
        }
    }
}

/// Parse a PHP regex literal of the form `/pattern/flags` into a compiled `Rx`.
fn rx_compile(raw: &str) -> Option<Rx> {
    let chars: Vec<char> = raw.trim().chars().collect();
    if chars.is_empty() {
        return None;
    }
    let open = chars[0];
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c if c.is_ascii_alphanumeric() || c == '\\' || c == ' ' => return None,
        c => c,
    };
    // find the last unescaped closing delimiter
    let end = (1..chars.len()).rev().find(|&i| chars[i] == close)?;
    let pattern: String = chars[1..end].iter().collect();
    let flag_str: String = chars[end + 1..].iter().collect();
    let mut flags = RxFlags {
        ci: false,
        dotall: false,
        multiline: false,
    };
    let mut extended = false;
    for f in flag_str.chars() {
        match f {
            'i' => flags.ci = true,
            's' => flags.dotall = true,
            'm' => flags.multiline = true,
            'x' => extended = true,
            'u' | 'U' | 'D' | 'A' | 'X' | 'S' => {} // ignore/no-op
            _ => {}
        }
    }
    let mut p = ReParser {
        c: pattern.chars().collect(),
        i: 0,
        ngroups: 0,
        names: Vec::new(),
        extended,
    };
    let root = p.alt();
    let ngroups = p.ngroups;
    let names = p.names;
    let anchored = matches!(&root, Re::Start)
        || matches!(&root, Re::Concat(v) if matches!(v.first(), Some(Re::Start)));
    let mut prog = Vec::new();
    prog.push(Inst::Save(0));
    rx_emit(&root, &mut prog);
    prog.push(Inst::Save(1));
    prog.push(Inst::Match);
    Some(Rx {
        prog,
        ngroups,
        flags,
        names,
        anchored: anchored && !flags.multiline,
    })
}

impl Rx {
    /// Find the leftmost match at or after `start`. Returns capture slots.
    fn exec(&self, text: &[char], start: usize, steps: &mut usize) -> Option<Vec<usize>> {
        let mut ctx = RxCtx {
            text,
            flags: self.flags,
            steps: *steps,
        };
        let mut from = start;
        let result = loop {
            let mut slots = vec![usize::MAX; 2 * (self.ngroups + 1)];
            if rx_run(&self.prog, 0, from, &mut slots, &mut ctx, 0) {
                break Some(slots);
            }
            if self.anchored || from >= text.len() || ctx.steps > RX_STEP_BUDGET {
                break None;
            }
            from += 1;
        };
        *steps = ctx.steps;
        result
    }
}

/// Extract the substring for capture group `g` (returns empty string if unset).
fn rx_group_str(text: &[char], slots: &[usize], g: usize) -> String {
    let (s, e) = (slots[2 * g], slots[2 * g + 1]);
    if s == usize::MAX || e == usize::MAX || s > e || e > text.len() {
        String::new()
    } else {
        text[s..e].iter().collect()
    }
}

/// Expand a `preg_replace` replacement template (`$1`, `${1}`, `\1`) using slots.
fn rx_expand_repl(repl: &str, text: &[char], slots: &[usize], ngroups: usize) -> String {
    let rc: Vec<char> = repl.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let group_str = |n: usize| {
        if n <= ngroups {
            rx_group_str(text, slots, n)
        } else {
            String::new()
        }
    };
    while i < rc.len() {
        let ch = rc[i];
        if (ch == '$' || ch == '\\') && i + 1 < rc.len() {
            if rc[i + 1] == '{' {
                // ${n}
                let mut j = i + 2;
                let mut num = String::new();
                while j < rc.len() && rc[j].is_ascii_digit() {
                    num.push(rc[j]);
                    j += 1;
                }
                if j < rc.len() && rc[j] == '}' && !num.is_empty() {
                    out.push_str(&group_str(num.parse().unwrap_or(0)));
                    i = j + 1;
                    continue;
                }
            } else if rc[i + 1].is_ascii_digit() {
                let mut j = i + 1;
                let mut num = String::new();
                while j < rc.len() && rc[j].is_ascii_digit() && num.len() < 2 {
                    num.push(rc[j]);
                    j += 1;
                }
                out.push_str(&group_str(num.parse().unwrap_or(0)));
                i = j;
                continue;
            }
        }
        if ch == '\\' && i + 1 < rc.len() && rc[i + 1] == '\\' {
            out.push('\\');
            i += 2;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// preg_replace on a single subject string. Returns None on a bad pattern.
fn rx_replace_str(rx: &Rx, repl: &str, subject: &str, limit: i64, count: &mut i64) -> String {
    let text: Vec<char> = subject.chars().collect();
    let mut out = String::new();
    let mut pos = 0usize;
    let mut steps = 0usize;
    let max = if limit < 0 { i64::MAX } else { limit };
    let mut done = 0i64;
    while pos <= text.len() && done < max {
        match rx.exec(&text, pos, &mut steps) {
            Some(slots) => {
                let (ms, me) = (slots[0], slots[1]);
                out.extend(&text[pos..ms]);
                out.push_str(&rx_expand_repl(repl, &text, &slots, rx.ngroups));
                *count += 1;
                done += 1;
                if me > ms {
                    pos = me;
                } else {
                    // empty match — emit one char to avoid looping
                    if me < text.len() {
                        out.push(text[me]);
                    }
                    pos = me + 1;
                }
            }
            None => break,
        }
    }
    if pos < text.len() {
        out.extend(&text[pos..]);
    }
    out
}

/// preg_replace_callback on a single subject. Calls `f` with each match array.
fn rx_replace_cb<F: FnMut(&[usize], &[char]) -> R<String>>(
    rx: &Rx,
    subject: &str,
    limit: i64,
    count: &mut i64,
    mut f: F,
) -> R<String> {
    let text: Vec<char> = subject.chars().collect();
    let mut out = String::new();
    let mut pos = 0usize;
    let mut steps = 0usize;
    let max = if limit < 0 { i64::MAX } else { limit };
    let mut done = 0i64;
    while pos <= text.len() && done < max {
        match rx.exec(&text, pos, &mut steps) {
            Some(slots) => {
                let (ms, me) = (slots[0], slots[1]);
                out.extend(&text[pos..ms]);
                out.push_str(&f(&slots, &text)?);
                *count += 1;
                done += 1;
                if me > ms {
                    pos = me;
                } else {
                    if me < text.len() {
                        out.push(text[me]);
                    }
                    pos = me + 1;
                }
            }
            None => break,
        }
    }
    if pos < text.len() {
        out.extend(&text[pos..]);
    }
    Ok(out)
}

fn rx_quote(s: &str, delim: Option<char>) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let special = matches!(
            c,
            '.' | '\\'
                | '+'
                | '*'
                | '?'
                | '['
                | '^'
                | ']'
                | '$'
                | '('
                | ')'
                | '{'
                | '}'
                | '='
                | '!'
                | '<'
                | '>'
                | '|'
                | ':'
                | '-'
                | '#'
        ) || Some(c) == delim;
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- serialize / unserialize -----------------------------------------------

fn php_serialize(v: &Value, depth: usize) -> String {
    if depth > 256 {
        return "N;".into();
    }
    match v {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", if *b { 1 } else { 0 }),
        Value::Int(n) => format!("i:{n};"),
        Value::Float(x) => {
            let s = if x.is_nan() {
                "NAN".into()
            } else if x.is_infinite() {
                if *x > 0.0 { "INF".into() } else { "-INF".into() }
            } else {
                format!("{x}")
            };
            format!("d:{s};")
        }
        Value::Str(s) => format!("s:{}:\"{}\";", s.len(), s),
        Value::Array(a) => {
            let mut out = format!("a:{}:{{", a.entries.len());
            for (k, val) in &a.entries {
                match k {
                    AKey::Int(i) => out.push_str(&format!("i:{i};")),
                    AKey::Str(s) => out.push_str(&format!("s:{}:\"{}\";", s.len(), s)),
                }
                out.push_str(&php_serialize(val, depth + 1));
            }
            out.push('}');
            out
        }
        Value::Object(o) => {
            let b = o.borrow();
            let mut out = format!("O:{}:\"{}\":{}:{{", b.class.len(), b.class, b.props.len());
            for (k, val) in &b.props {
                out.push_str(&format!("s:{}:\"{}\";", k.len(), k));
                out.push_str(&php_serialize(val, depth + 1));
            }
            out.push('}');
            out
        }
        Value::Closure(_) => "N;".into(),
    }
}

fn unser_read_until(b: &[u8], pos: &mut usize, end: u8) -> String {
    let start = *pos;
    while *pos < b.len() && b[*pos] != end {
        *pos += 1;
    }
    let s = String::from_utf8_lossy(&b[start..*pos]).to_string();
    if *pos < b.len() {
        *pos += 1; // consume the delimiter
    }
    s
}

fn unser_string(b: &[u8], pos: &mut usize) -> Option<String> {
    // <len>:"<bytes>";
    let len: usize = unser_read_until(b, pos, b':').parse().ok()?;
    if *pos >= b.len() || b[*pos] != b'"' {
        return None;
    }
    *pos += 1;
    if *pos + len > b.len() {
        return None;
    }
    let s = String::from_utf8_lossy(&b[*pos..*pos + len]).to_string();
    *pos += len;
    // expect "  ;
    if *pos < b.len() && b[*pos] == b'"' {
        *pos += 1;
    }
    if *pos < b.len() && b[*pos] == b';' {
        *pos += 1;
    }
    Some(s)
}

fn php_unserialize(b: &[u8], pos: &mut usize, depth: usize) -> Option<Value> {
    if depth > 256 || *pos >= b.len() {
        return None;
    }
    let t = b[*pos];
    *pos += 1;
    match t {
        b'N' => {
            if *pos < b.len() && b[*pos] == b';' {
                *pos += 1;
            }
            Some(Value::Null)
        }
        b'b' => {
            *pos += 1; // skip ':'
            let v = b.get(*pos).copied() == Some(b'1');
            *pos += 1;
            if *pos < b.len() && b[*pos] == b';' {
                *pos += 1;
            }
            Some(Value::Bool(v))
        }
        b':' => unreachable!(),
        b'i' => {
            *pos += 1; // skip ':'
            unser_read_until(b, pos, b';').parse().ok().map(Value::Int)
        }
        b'd' => {
            *pos += 1; // skip ':'
            let s = unser_read_until(b, pos, b';');
            let x = match s.as_str() {
                "NAN" => f64::NAN,
                "INF" => f64::INFINITY,
                "-INF" => f64::NEG_INFINITY,
                _ => s.parse().ok()?,
            };
            Some(Value::Float(x))
        }
        b's' => {
            *pos += 1; // skip ':'
            unser_string(b, pos).map(Value::Str)
        }
        b'a' => {
            *pos += 1; // skip ':'
            let count: usize = unser_read_until(b, pos, b':').parse().ok()?;
            if *pos < b.len() && b[*pos] == b'{' {
                *pos += 1;
            }
            let mut arr = PArray::default();
            for _ in 0..count {
                let key = php_unserialize(b, pos, depth + 1)?;
                let val = php_unserialize(b, pos, depth + 1)?;
                arr.set(key_from_value(&key), val);
            }
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
            }
            Some(Value::Array(arr))
        }
        b'O' => {
            *pos += 1; // skip ':'
            let class = unser_string(b, pos)?;
            if *pos < b.len() && b[*pos] == b':' {
                *pos += 1; // separator between class name and property count
            }
            let count: usize = unser_read_until(b, pos, b':').parse().ok()?;
            if *pos < b.len() && b[*pos] == b'{' {
                *pos += 1;
            }
            let mut props: Vec<(String, Value)> = Vec::new();
            for _ in 0..count {
                let key = php_unserialize(b, pos, depth + 1)?;
                let val = php_unserialize(b, pos, depth + 1)?;
                props.push((key.to_php_string(), val));
            }
            if *pos < b.len() && b[*pos] == b'}' {
                *pos += 1;
            }
            Some(Value::Object(Rc::new(RefCell::new(Obj { class, props }))))
        }
        _ => None,
    }
}

// ---- JSON ------------------------------------------------------------------

fn json_encode_value(v: &Value, depth: usize) -> String {
    if depth > 512 {
        return "null".to_string();
    }
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            if x.is_finite() {
                let s = format_php_float(*x);
                if s.contains(['.', 'e', 'E']) {
                    s
                } else {
                    format!("{s}.0")
                }
            } else {
                "0".to_string()
            }
        }
        Value::Str(s) => json_encode_string(s),
        Value::Array(a) => {
            let is_list = a
                .entries
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, AKey::Int(n) if *n == i as i64));
            if is_list {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(_, v)| json_encode_value(v, depth + 1))
                    .collect();
                format!("[{}]", parts.join(","))
            } else {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(k, v)| {
                        let key = match k {
                            AKey::Int(n) => n.to_string(),
                            AKey::Str(s) => s.clone(),
                        };
                        format!("{}:{}", json_encode_string(&key), json_encode_value(v, depth + 1))
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
        Value::Object(o) => {
            let ob = o.borrow();
            let parts: Vec<String> = ob
                .props
                .iter()
                .map(|(k, v)| format!("{}:{}", json_encode_string(k), json_encode_value(v, depth + 1)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Closure(_) => "null".to_string(),
    }
}

fn json_encode_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_decode_str(s: &str, assoc: bool) -> Option<Value> {
    let chars: Vec<char> = s.chars().collect();
    let mut p = 0usize;
    let v = json_parse(&chars, &mut p, assoc, 0)?;
    json_ws(&chars, &mut p);
    if p >= chars.len() {
        Some(v)
    } else {
        None
    }
}

fn json_ws(c: &[char], p: &mut usize) {
    while *p < c.len() && c[*p].is_whitespace() {
        *p += 1;
    }
}

fn json_parse(c: &[char], p: &mut usize, assoc: bool, depth: usize) -> Option<Value> {
    if depth > 512 {
        return None;
    }
    json_ws(c, p);
    let ch = *c.get(*p)?;
    match ch {
        '{' => {
            *p += 1;
            let mut arr = PArray::default();
            let mut props: Vec<(String, Value)> = Vec::new();
            json_ws(c, p);
            if c.get(*p) == Some(&'}') {
                *p += 1;
            } else {
                loop {
                    json_ws(c, p);
                    let key = json_string(c, p)?;
                    json_ws(c, p);
                    if c.get(*p) != Some(&':') {
                        return None;
                    }
                    *p += 1;
                    let val = json_parse(c, p, assoc, depth + 1)?;
                    if assoc {
                        arr.set(key_from_value(&Value::Str(key)), val);
                    } else {
                        props.push((key, val));
                    }
                    json_ws(c, p);
                    match c.get(*p) {
                        Some(',') => {
                            *p += 1;
                            continue;
                        }
                        Some('}') => {
                            *p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            if assoc {
                Some(Value::Array(arr))
            } else {
                Some(Value::Object(Rc::new(RefCell::new(Obj {
                    class: "stdClass".to_string(),
                    props,
                }))))
            }
        }
        '[' => {
            *p += 1;
            let mut arr = PArray::default();
            json_ws(c, p);
            if c.get(*p) == Some(&']') {
                *p += 1;
            } else {
                loop {
                    let val = json_parse(c, p, assoc, depth + 1)?;
                    arr.push(val);
                    json_ws(c, p);
                    match c.get(*p) {
                        Some(',') => {
                            *p += 1;
                            continue;
                        }
                        Some(']') => {
                            *p += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Some(Value::Array(arr))
        }
        '"' => Some(Value::Str(json_string(c, p)?)),
        't' => {
            if c[*p..].starts_with(&['t', 'r', 'u', 'e']) {
                *p += 4;
                Some(Value::Bool(true))
            } else {
                None
            }
        }
        'f' => {
            if c[*p..].starts_with(&['f', 'a', 'l', 's', 'e']) {
                *p += 5;
                Some(Value::Bool(false))
            } else {
                None
            }
        }
        'n' => {
            if c[*p..].starts_with(&['n', 'u', 'l', 'l']) {
                *p += 4;
                Some(Value::Null)
            } else {
                None
            }
        }
        _ => {
            let start = *p;
            if c.get(*p) == Some(&'-') {
                *p += 1;
            }
            while *p < c.len() && (c[*p].is_ascii_digit() || matches!(c[*p], '.' | 'e' | 'E' | '+' | '-')) {
                *p += 1;
            }
            let numstr: String = c[start..*p].iter().collect();
            if let Ok(n) = numstr.parse::<i64>() {
                Some(Value::Int(n))
            } else if let Ok(f) = numstr.parse::<f64>() {
                Some(Value::Float(f))
            } else {
                None
            }
        }
    }
}

fn json_string(c: &[char], p: &mut usize) -> Option<String> {
    if c.get(*p) != Some(&'"') {
        return None;
    }
    *p += 1;
    let mut s = String::new();
    while let Some(&ch) = c.get(*p) {
        match ch {
            '"' => {
                *p += 1;
                return Some(s);
            }
            '\\' => {
                *p += 1;
                let e = *c.get(*p)?;
                match e {
                    '"' => s.push('"'),
                    '\\' => s.push('\\'),
                    '/' => s.push('/'),
                    'n' => s.push('\n'),
                    'r' => s.push('\r'),
                    't' => s.push('\t'),
                    'b' => s.push('\u{8}'),
                    'f' => s.push('\u{c}'),
                    'u' => {
                        let hex: String = c.get(*p + 1..*p + 5)?.iter().collect();
                        let code = u32::from_str_radix(&hex, 16).ok()?;
                        s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        *p += 4;
                    }
                    _ => return None,
                }
                *p += 1;
            }
            _ => {
                s.push(ch);
                *p += 1;
            }
        }
    }
    None
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
        Value::Object(_) => "object",
        Value::Closure(_) => "object",
    }
}

/// `var_dump` output (with trailing newline). `indent` is the leading space
/// count for this value's line — arrays recurse with `indent + 2`.
fn var_dump_str(v: &Value, indent: usize) -> String {
    if indent > 256 {
        return format!("{}*RECURSION*\n", " ".repeat(indent)); // cyclic object guard
    }
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
        Value::Object(o) => {
            let ob = o.borrow();
            let mut out = format!("{pad}object({})#1 ({}) {{\n", ob.class, ob.props.len());
            let kp = " ".repeat(indent + 2);
            for (n, v) in &ob.props {
                out.push_str(&format!("{kp}[\"{n}\"]=>\n"));
                out.push_str(&var_dump_str(v, indent + 2));
            }
            out.push_str(&format!("{pad}}}\n"));
            out
        }
        Value::Closure(_) => format!("{pad}object(Closure)#1 (0) {{\n{pad}}}\n"),
    }
}

fn print_r_str(v: &Value) -> String {
    print_r_inner(v, 0)
}

fn print_r_inner(v: &Value, depth: usize) -> String {
    if depth > 128 {
        return " *RECURSION*".to_string();
    }
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
        Value::Object(o) => {
            let ob = o.borrow();
            let paren = " ".repeat(depth * 8);
            let item = " ".repeat(depth * 8 + 4);
            let mut s = format!("{} Object\n", ob.class);
            s.push_str(&format!("{paren}(\n"));
            for (n, v) in &ob.props {
                s.push_str(&format!("{item}[{n}] => {}\n", print_r_inner(v, depth + 1)));
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
    if indent > 256 {
        return "NULL".to_string(); // cyclic object guard
    }
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
        Value::Object(o) => {
            let ob = o.borrow();
            let pad = " ".repeat(indent);
            let ipad = " ".repeat(indent + 2);
            let mut s = format!("\\{}::__set_state(array(\n", ob.class);
            for (n, v) in &ob.props {
                s.push_str(&format!("{ipad}'{n}' => {},\n", var_export_inner(v, indent + 2)));
            }
            s.push_str(&format!("{pad}))"));
            s
        }
        Value::Closure(_) => "NULL".to_string(),
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

/// Well-known PHP constants (case-sensitive).
fn php_constant(name: &str) -> Option<Value> {
    Some(match name {
        "PHP_EOL" => Value::Str("\n".into()),
        "PHP_INT_MAX" => Value::Int(i64::MAX),
        "PHP_INT_MIN" => Value::Int(i64::MIN),
        "PHP_INT_SIZE" => Value::Int(8),
        "PHP_FLOAT_EPSILON" => Value::Float(f64::EPSILON),
        "PHP_FLOAT_MAX" => Value::Float(f64::MAX),
        "PHP_FLOAT_MIN" => Value::Float(f64::MIN_POSITIVE),
        "PHP_VERSION" => Value::Str("8.3.0".into()),
        "PHP_MAJOR_VERSION" => Value::Int(8),
        "PHP_OS" => Value::Str("Linux".into()),
        "PHP_OS_FAMILY" => Value::Str("Linux".into()),
        "M_PI" => Value::Float(std::f64::consts::PI),
        "M_E" => Value::Float(std::f64::consts::E),
        "M_SQRT2" => Value::Float(std::f64::consts::SQRT_2),
        "NAN" => Value::Float(f64::NAN),
        "INF" => Value::Float(f64::INFINITY),
        "STR_PAD_RIGHT" => Value::Int(1),
        "STR_PAD_LEFT" => Value::Int(0),
        "STR_PAD_BOTH" => Value::Int(2),
        "COUNT_NORMAL" => Value::Int(0),
        "COUNT_RECURSIVE" => Value::Int(1),
        "FILE_APPEND" => Value::Int(8),
        "FILE_USE_INCLUDE_PATH" => Value::Int(1),
        "FILE_IGNORE_NEW_LINES" => Value::Int(2),
        "FILE_SKIP_EMPTY_LINES" => Value::Int(4),
        "FILE_NO_DEFAULT_CONTEXT" => Value::Int(16),
        "LOCK_SH" => Value::Int(1),
        "LOCK_EX" => Value::Int(2),
        "LOCK_UN" => Value::Int(3),
        "SEEK_SET" => Value::Int(0),
        "SEEK_CUR" => Value::Int(1),
        "SEEK_END" => Value::Int(2),
        "PATHINFO_DIRNAME" => Value::Int(1),
        "PATHINFO_BASENAME" => Value::Int(2),
        "PATHINFO_EXTENSION" => Value::Int(4),
        "PATHINFO_FILENAME" => Value::Int(8),
        "DIRECTORY_SEPARATOR" => Value::Str(if cfg!(windows) { "\\".into() } else { "/".into() }),
        "PATH_SEPARATOR" => Value::Str(if cfg!(windows) { ";".into() } else { ":".into() }),
        "PHP_VERSION_ID" => Value::Int(80300),
        "PHP_MINOR_VERSION" => Value::Int(3),
        "PHP_RELEASE_VERSION" => Value::Int(0),
        "PHP_EXTRA_VERSION" => Value::Str(String::new()),
        "PHP_SAPI" => Value::Str("cli".into()),
        "PHP_BINARY" => Value::Str(String::new()),
        "PHP_MAXPATHLEN" => Value::Int(4096),
        "PHP_FLOAT_DIG" => Value::Int(15),
        "PHP_WINDOWS_VERSION_MAJOR" => Value::Int(10),
        "DEFAULT_INCLUDE_PATH" => Value::Str(".".into()),
        "SORT_REGULAR" => Value::Int(0),
        "SORT_NUMERIC" => Value::Int(1),
        "SORT_STRING" => Value::Int(2),
        "SORT_DESC" => Value::Int(3),
        "SORT_ASC" => Value::Int(4),
        "SORT_LOCALE_STRING" => Value::Int(5),
        "SORT_NATURAL" => Value::Int(6),
        "SORT_FLAG_CASE" => Value::Int(8),
        "CASE_LOWER" => Value::Int(0),
        "CASE_UPPER" => Value::Int(1),
        "ARRAY_FILTER_USE_KEY" => Value::Int(2),
        "ARRAY_FILTER_USE_BOTH" => Value::Int(1),
        "ENT_QUOTES" => Value::Int(3),
        "ENT_COMPAT" => Value::Int(2),
        "ENT_NOQUOTES" => Value::Int(0),
        "ENT_HTML401" => Value::Int(0),
        "ENT_HTML5" => Value::Int(48),
        "ENT_SUBSTITUTE" => Value::Int(8),
        "ENT_IGNORE" => Value::Int(4),
        "JSON_PRETTY_PRINT" => Value::Int(128),
        "JSON_UNESCAPED_SLASHES" => Value::Int(64),
        "JSON_UNESCAPED_UNICODE" => Value::Int(256),
        "JSON_THROW_ON_ERROR" => Value::Int(4194304),
        "JSON_ERROR_NONE" => Value::Int(0),
        "JSON_HEX_TAG" => Value::Int(1),
        "FILTER_VALIDATE_INT" => Value::Int(257),
        "FILTER_VALIDATE_BOOLEAN" | "FILTER_VALIDATE_BOOL" => Value::Int(258),
        "FILTER_VALIDATE_FLOAT" => Value::Int(259),
        "FILTER_VALIDATE_REGEXP" => Value::Int(272),
        "FILTER_VALIDATE_URL" => Value::Int(273),
        "FILTER_VALIDATE_EMAIL" => Value::Int(274),
        "FILTER_VALIDATE_IP" => Value::Int(275),
        "FILTER_DEFAULT" => Value::Int(516),
        "FILTER_SANITIZE_STRING" => Value::Int(513),
        "FILTER_FLAG_ALLOW_THOUSAND" => Value::Int(8192),
        "LC_ALL" => Value::Int(6),
        "LC_CTYPE" => Value::Int(0),
        "LC_NUMERIC" => Value::Int(4),
        "LC_TIME" => Value::Int(2),
        "LC_COLLATE" => Value::Int(3),
        "LC_MONETARY" => Value::Int(1),
        "M_SQRT3" => Value::Float(1.7320508075688772),
        "M_SQRT1_2" => Value::Float(std::f64::consts::FRAC_1_SQRT_2),
        "M_PI_2" => Value::Float(std::f64::consts::FRAC_PI_2),
        "M_PI_4" => Value::Float(std::f64::consts::FRAC_PI_4),
        "M_2_PI" => Value::Float(std::f64::consts::FRAC_2_PI),
        "M_LN2" => Value::Float(std::f64::consts::LN_2),
        "M_LN10" => Value::Float(std::f64::consts::LN_10),
        "M_LOG2E" => Value::Float(std::f64::consts::LOG2_E),
        "M_EULER" => Value::Float(0.5772156649015329),
        "E_DEPRECATED" => Value::Int(8192),
        "E_STRICT" => Value::Int(2048),
        "E_USER_ERROR" => Value::Int(256),
        "E_USER_WARNING" => Value::Int(512),
        "E_USER_NOTICE" => Value::Int(1024),
        "E_USER_DEPRECATED" => Value::Int(16384),
        "E_COMPILE_ERROR" => Value::Int(64),
        "E_PARSE" => Value::Int(4),
        "PHP_ROUND_HALF_UP" => Value::Int(1),
        "PHP_ROUND_HALF_DOWN" => Value::Int(2),
        "PHP_ROUND_HALF_EVEN" => Value::Int(3),
        "PHP_ROUND_HALF_ODD" => Value::Int(4),
        "PREG_PATTERN_ORDER" => Value::Int(1),
        "PREG_SET_ORDER" => Value::Int(2),
        "PREG_OFFSET_CAPTURE" => Value::Int(256),
        "PREG_SPLIT_NO_EMPTY" => Value::Int(1),
        "PREG_SPLIT_DELIM_CAPTURE" => Value::Int(2),
        "PREG_SPLIT_OFFSET_CAPTURE" => Value::Int(4),
        "E_ALL" => Value::Int(32767),
        "E_WARNING" => Value::Int(2),
        "E_NOTICE" => Value::Int(8),
        "E_ERROR" => Value::Int(1),
        _ => return None,
    })
}

/// `max`/`min` over either a single array argument or the full argument list.
fn array_extreme(args: &[Value], want_max: bool) -> Value {
    let candidates: Vec<Value> = if args.len() == 1 {
        match &args[0] {
            Value::Array(a) => a.entries.iter().map(|(_, v)| v.clone()).collect(),
            other => vec![other.clone()],
        }
    } else {
        args.to_vec()
    };
    let mut best: Option<Value> = None;
    for v in candidates {
        best = Some(match best {
            None => v,
            Some(b) => {
                let ord = compare(&v, &b);
                let take = if want_max {
                    ord == Ordering::Greater
                } else {
                    ord == Ordering::Less
                };
                if take {
                    v
                } else {
                    b
                }
            }
        });
    }
    best.unwrap_or(Value::Bool(false))
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
