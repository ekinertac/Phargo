//! The v2 runtime value model — byte-correct by construction.
//!
//! Unlike the legacy engine (whose `Value::Str` is a Rust `String`, which mangles
//! high bytes), here a PHP string is a `Vec<u8>`. Scalars, arrays (ordered maps),
//! and the coercion rules (PHP's type juggling) live here; the evaluator and
//! builtins are built on top.
#![allow(dead_code)]

use super::ast;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Array(Arr),
    Object(Rc<RefCell<Obj>>),
    Closure(Rc<ClosureVal>),
    /// A PHP reference: a shared, mutable cell. Variables that alias the same
    /// storage (`$b = &$a`, `&$param`, `use (&$x)`) hold `Ref` to one cell.
    /// References live directly under variable names in scopes; reads deref and
    /// writes write through. `deref()` collapses a (possibly chained) ref.
    Ref(Rc<RefCell<Value>>),
}

impl Value {
    /// Follow a reference to the underlying value (clone). Non-refs pass through.
    pub fn deref(&self) -> Value {
        match self {
            Value::Ref(cell) => cell.borrow().deref(),
            other => other.clone(),
        }
    }
}

/// A runtime closure: the function body plus its captured environment.
#[derive(Debug)]
pub struct ClosureVal {
    pub kind: ClosureKind,
    pub captures: Vec<(String, Value)>,
    pub bound_this: Option<Value>,
}

#[derive(Debug)]
pub enum ClosureKind {
    Full(Rc<ast::Closure>),
    Arrow(Rc<ast::ArrowFn>),
}

impl ClosureKind {
    /// Cheap clone sharing the underlying Rc'd AST.
    pub fn clone_rc(&self) -> ClosureKind {
        match self {
            ClosureKind::Full(f) => ClosureKind::Full(f.clone()),
            ClosureKind::Arrow(f) => ClosureKind::Arrow(f.clone()),
        }
    }
}

thread_local! {
    /// Monotonic object-id counter (PHP assigns sequential handles at creation).
    /// Reset per run via `reset_object_ids()`.
    static NEXT_OBJ_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Reset the object-id counter — call at the start of each run so the scoreboard
/// (which reuses the worker thread across tests) numbers each test from #1.
pub fn reset_object_ids() {
    NEXT_OBJ_ID.with(|c| c.set(1));
}

fn next_object_id() -> u64 {
    NEXT_OBJ_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    })
}

/// A PHP object instance: a class name plus insertion-ordered properties.
/// Objects are reference types, so `Value::Object` is an `Rc<RefCell<…>>`.
#[derive(Debug)]
pub struct Obj {
    pub class: String,
    pub props: Vec<(String, Value)>,
    /// Sequential instance id (the `#N` shown by var_dump), assigned at creation.
    pub id: u64,
}

impl Obj {
    /// Create an empty instance of `class`, assigning the next sequential id.
    pub fn new(class: impl Into<String>) -> Obj {
        Obj { class: class.into(), props: Vec::new(), id: next_object_id() }
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.props.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }
    pub fn set(&mut self, name: &str, v: Value) {
        if let Some(slot) = self.props.iter_mut().find(|(k, _)| k == name) {
            slot.1 = v;
        } else {
            self.props.push((name.to_string(), v));
        }
    }
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.props.iter_mut().find(|(k, _)| k == name).map(|(_, v)| v)
    }
}

pub fn new_obj(class: &str) -> Value {
    Value::Object(Rc::new(RefCell::new(Obj::new(class))))
}

/// Array key: PHP normalizes integer-like string keys to ints.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Str(Vec<u8>),
}

/// An insertion-ordered map (PHP arrays), with a hash index for O(1) lookup.
#[derive(Debug, Clone, Default)]
pub struct Arr {
    pub entries: Vec<(Key, Value)>,
    index: HashMap<Key, usize>,
    next_int: i64,
    /// Internal pointer for reset()/next()/current()/key()/end()/prev().
    pub pos: usize,
}

impl Arr {
    pub fn new() -> Self {
        Arr::default()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Normalize a value used as a key (int-like strings → ints; bools/floats → ints; null → "").
    pub fn norm_key(v: &Value) -> Key {
        if let Value::Ref(c) = v {
            return Arr::norm_key(&c.borrow());
        }
        match v {
            Value::Int(n) => Key::Int(*n),
            Value::Bool(b) => Key::Int(*b as i64),
            Value::Float(f) => Key::Int(*f as i64),
            Value::Null => Key::Str(Vec::new()),
            Value::Str(s) => {
                if let Some(n) = canonical_int_key(s) {
                    Key::Int(n)
                } else {
                    Key::Str(s.clone())
                }
            }
            Value::Array(_) | Value::Object(_) | Value::Closure(_) | Value::Ref(_) => Key::Int(0),
        }
    }

    pub fn get(&self, k: &Key) -> Option<&Value> {
        self.index.get(k).map(|&i| &self.entries[i].1)
    }
    pub fn get_mut(&mut self, k: &Key) -> Option<&mut Value> {
        if let Some(&i) = self.index.get(k) {
            Some(&mut self.entries[i].1)
        } else {
            None
        }
    }

    pub fn insert(&mut self, k: Key, v: Value) {
        if let Key::Int(n) = &k {
            if *n >= self.next_int {
                self.next_int = n + 1;
            }
        }
        if let Some(&i) = self.index.get(&k) {
            // Write through a reference element (created by `foreach (… as &$v)`)
            // so aliases of the cell observe the assignment, PHP-style.
            if let (Value::Ref(cell), false) = (&self.entries[i].1, matches!(v, Value::Ref(_))) {
                *cell.borrow_mut() = v;
                return;
            }
            self.entries[i].1 = v;
        } else {
            self.index.insert(k.clone(), self.entries.len());
            self.entries.push((k, v));
        }
    }

    /// `$a[] = v` — append with the next integer key.
    pub fn push(&mut self, v: Value) {
        let k = Key::Int(self.next_int);
        self.insert(k, v);
    }

    pub fn remove(&mut self, k: &Key) -> Option<Value> {
        if let Some(i) = self.index.get(k).copied() {
            let (_, v) = self.entries.remove(i);
            self.reindex();
            Some(v)
        } else {
            None
        }
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, (k, _)) in self.entries.iter().enumerate() {
            self.index.insert(k.clone(), i);
        }
    }
}

/// PHP only treats a string as an integer key if it's the *canonical* decimal
/// form of an int (no leading zeros, optional leading `-`, fits in i64).
fn canonical_int_key(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = if s[0] == b'-' { (true, &s[1..]) } else { (false, s) };
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits[0] == b'0' {
        return None; // leading zero
    }
    if neg && digits == b"0" {
        return None; // "-0" isn't canonical
    }
    let txt = std::str::from_utf8(s).ok()?;
    txt.parse::<i64>().ok()
}

// ---- coercions (PHP type juggling) -------------------------------------

pub fn to_bool(v: &Value) -> bool {
    if let Value::Ref(c) = v {
        return to_bool(&c.borrow());
    }
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(n) => *n != 0,
        Value::Float(f) => *f != 0.0,
        Value::Str(s) => !(s.is_empty() || s == b"0"),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) | Value::Closure(_) => true,
        Value::Ref(c) => to_bool(&c.borrow()),
    }
}

pub fn to_i64(v: &Value) -> i64 {
    if let Value::Ref(c) = v {
        return to_i64(&c.borrow());
    }
    match v {
        Value::Null => 0,
        Value::Bool(b) => *b as i64,
        Value::Int(n) => *n,
        Value::Float(f) => *f as i64,
        Value::Str(s) => leading_number(s).as_i64(),
        Value::Array(a) => !a.is_empty() as i64,
        Value::Object(_) | Value::Closure(_) => 1,
        Value::Ref(c) => to_i64(&c.borrow()),
    }
}

pub fn to_f64(v: &Value) -> f64 {
    if let Value::Ref(c) = v {
        return to_f64(&c.borrow());
    }
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => *b as i64 as f64,
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        Value::Str(s) => leading_number(s).as_f64(),
        Value::Array(a) => !a.is_empty() as i64 as f64,
        Value::Object(_) | Value::Closure(_) => 1.0,
        Value::Ref(c) => to_f64(&c.borrow()),
    }
}

pub fn to_bytes(v: &Value) -> Vec<u8> {
    if let Value::Ref(c) = v {
        return to_bytes(&c.borrow());
    }
    match v {
        Value::Null => Vec::new(),
        Value::Bool(true) => b"1".to_vec(),
        Value::Bool(false) => Vec::new(),
        Value::Int(n) => n.to_string().into_bytes(),
        Value::Float(f) => format_float(*f).into_bytes(),
        Value::Str(s) => s.clone(),
        Value::Array(_) => b"Array".to_vec(),
        // __toString is handled by the evaluator's stringify(); this is the fallback.
        Value::Object(_) | Value::Closure(_) => Vec::new(),
        Value::Ref(_) => Vec::new(),
    }
}

pub fn type_name(v: &Value) -> &'static str {
    if let Value::Ref(c) = v {
        return type_name(&c.borrow());
    }
    match v {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) => "double",
        Value::Str(_) => "string",
        Value::Array(_) => "array",
        Value::Object(o) if o.borrow().class == "__Stream" => "resource",
        Value::Object(_) | Value::Closure(_) => "object",
        Value::Ref(_) => "NULL",
    }
}

/// A numeric value that's either an int or float, used by arithmetic.
#[derive(Clone, Copy)]
pub enum Num {
    Int(i64),
    Float(f64),
}
impl Num {
    pub fn as_i64(self) -> i64 {
        match self {
            Num::Int(n) => n,
            Num::Float(f) => f as i64,
        }
    }
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(n) => n as f64,
            Num::Float(f) => f,
        }
    }
    pub fn to_value(self) -> Value {
        match self {
            Num::Int(n) => Value::Int(n),
            Num::Float(f) => Value::Float(f),
        }
    }
}

/// PHP's "leading-numeric string" rule: parse an optional numeric prefix.
pub fn leading_number(s: &[u8]) -> Num {
    let txt = String::from_utf8_lossy(s);
    let t = txt.trim_start();
    let bytes = t.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut is_float = false;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < n && bytes[i] == b'.' {
        is_float = true;
        i += 1;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < n && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        if j < n && bytes[j].is_ascii_digit() {
            is_float = true;
            i = j;
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    let prefix = &t[..i];
    if prefix.is_empty() || prefix == "+" || prefix == "-" {
        return Num::Int(0);
    }
    if is_float {
        Num::Float(prefix.parse().unwrap_or(0.0))
    } else {
        match prefix.parse::<i64>() {
            Ok(n) => Num::Int(n),
            Err(_) => Num::Float(prefix.parse().unwrap_or(0.0)),
        }
    }
}

/// Is the whole string a valid PHP numeric string?
pub fn is_numeric_str(s: &[u8]) -> bool {
    let t = match std::str::from_utf8(s) {
        Ok(t) => t.trim(),
        Err(_) => return false,
    };
    if t.is_empty() {
        return false;
    }
    t.parse::<i64>().is_ok() || t.parse::<f64>().is_ok()
}

/// The numeric view of a value for arithmetic (strings coerced).
pub fn to_num(v: &Value) -> Num {
    match v {
        Value::Int(n) => Num::Int(*n),
        Value::Float(f) => Num::Float(*f),
        Value::Bool(b) => Num::Int(*b as i64),
        Value::Null => Num::Int(0),
        Value::Str(s) => leading_number(s),
        Value::Array(_) => Num::Int(0),
        Value::Object(_) | Value::Closure(_) => Num::Int(1),
        Value::Ref(c) => to_num(&c.borrow()),
    }
}

/// PHP's `number_format`-free float→string (matches `echo`/string cast).
pub fn format_float(x: f64) -> String {
    if x.is_nan() {
        return "NAN".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-INF".into() } else { "INF".into() };
    }
    if x == x.trunc() && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    // PHP default precision is 14 significant digits.
    let mut s = format!("{:.*}", 14, x);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    // fall back to %G-ish for very large/small
    let g = format!("{:e}", x);
    if s.len() > g.len() + 4 {
        return reformat_exp(x);
    }
    s
}

fn reformat_exp(x: f64) -> String {
    let s = format!("{:E}", x);
    s.replace('E', "E+").replace("E+-", "E-")
}

// ---- comparison & equality --------------------------------------------

pub fn loose_eq(a: &Value, b: &Value) -> bool {
    loose_eq_d(&a.deref(), &b.deref(), 0)
}

/// `depth` guards against cyclic object graphs (`$a->x = $a; $a == $b`) recursing
/// without bound — PHP throws "Nesting level too deep" there; we just stop.
fn loose_eq_d(a: &Value, b: &Value, depth: usize) -> bool {
    use Value::*;
    if depth > 256 {
        return false;
    }
    // Reference elements inside containers compare by their target value.
    if matches!(a, Ref(_)) || matches!(b, Ref(_)) {
        return loose_eq_d(&a.deref(), &b.deref(), depth);
    }
    match (a, b) {
        (Null, Null) => true,
        (Bool(_), _) | (_, Bool(_)) => to_bool(a) == to_bool(b),
        (Null, _) => !to_bool(b) && matches!(b, Null | Bool(false)) || is_nullish(b),
        (_, Null) => loose_eq_d(b, a, depth),
        // Int/Int compares EXACTLY — a round trip through f64 collapses
        // values near ±2^63 (operator_equals_variation_64bit)
        (Int(x), Int(y)) => x == y,
        (Int(_) | Float(_), Int(_) | Float(_)) => to_f64(a) == to_f64(b),
        (Str(x), Str(y)) => {
            if is_numeric_str(x) && is_numeric_str(y) {
                // integral strings compare exactly (f64 collapses ±2^63)
                match (int_str(x), int_str(y)) {
                    (Some(ix), Some(iy)) => ix == iy,
                    _ => to_f64(a) == to_f64(b),
                }
            } else {
                x == y
            }
        }
        (Int(i), Str(s)) | (Str(s), Int(i)) => {
            if is_numeric_str(s) {
                match int_str(s) {
                    Some(is) => *i == is,
                    None => to_f64(a) == to_f64(b),
                }
            } else {
                to_bytes(a) == to_bytes(b)
            }
        }
        (Float(_), Str(s)) | (Str(s), Float(_)) => {
            // PHP 8: number vs non-numeric string compares as strings
            if is_numeric_str(s) {
                to_f64(a) == to_f64(b)
            } else {
                to_bytes(a) == to_bytes(b)
            }
        }
        (Array(x), Array(y)) => array_loose_eq(x, y, depth),
        (Object(x), Object(y)) => {
            // same instance, or same class with loosely-equal properties
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let (x, y) = (x.borrow(), y.borrow());
            x.class == y.class
                && x.props.len() == y.props.len()
                && x.props.iter().all(|(k, v)| {
                    y.get(k).map(|w| loose_eq_d(v, w, depth + 1)).unwrap_or(false)
                })
        }
        _ => false,
    }
}

fn is_nullish(v: &Value) -> bool {
    matches!(v, Value::Null)
}

fn array_loose_eq(x: &Arr, y: &Arr, depth: usize) -> bool {
    if depth > 256 || x.len() != y.len() {
        return depth <= 256 && x.len() == y.len();
    }
    for (k, v) in &x.entries {
        match y.get(k) {
            Some(w) if loose_eq_d(v, w, depth + 1) => {}
            _ => return false,
        }
    }
    true
}

pub fn strict_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    if matches!(a, Ref(_)) || matches!(b, Ref(_)) {
        return strict_eq(&a.deref(), &b.deref());
    }
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        (Array(x), Array(y)) => {
            x.len() == y.len()
                && x.entries.iter().zip(&y.entries).all(|((ka, va), (kb, vb))| {
                    ka == kb && strict_eq(va, vb)
                })
        }
        (Object(x), Object(y)) => Rc::ptr_eq(x, y), // strict: same instance
        _ => false,
    }
}

/// Three-way comparison (`<=>`), PHP semantics.
/// Parse a numeric string as an exact i64 when it is integral and in range
/// (no fraction/exponent, no overflow). None → caller falls back to f64.
fn int_str(s: &[u8]) -> Option<i64> {
    let t = std::str::from_utf8(s).ok()?.trim();
    if t.is_empty() || t.contains(['.', 'e', 'E']) {
        return None;
    }
    t.parse::<i64>().ok()
}

pub fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    use Value::*;
    if matches!(a, Ref(_)) || matches!(b, Ref(_)) {
        return compare(&a.deref(), &b.deref());
    }
    match (a, b) {
        // exact integer ordering (f64 round trips collapse near ±2^63)
        (Int(x), Int(y)) => x.cmp(y),
        (Str(x), Str(y)) => {
            if is_numeric_str(x) && is_numeric_str(y) {
                match (int_str(x), int_str(y)) {
                    (Some(ix), Some(iy)) => ix.cmp(&iy),
                    _ => to_f64(a).partial_cmp(&to_f64(b)).unwrap_or(Ordering::Equal),
                }
            } else {
                x.cmp(y)
            }
        }
        (Int(i), Str(s)) if is_numeric_str(s) && int_str(s).is_some() => {
            i.cmp(&int_str(s).unwrap())
        }
        (Str(s), Int(i)) if is_numeric_str(s) && int_str(s).is_some() => {
            int_str(s).unwrap().cmp(i)
        }
        (Array(x), Array(y)) => x.len().cmp(&y.len()),
        (Bool(_), _) | (_, Bool(_)) | (Null, _) | (_, Null) => {
            (to_bool(a) as u8).cmp(&(to_bool(b) as u8))
        }
        _ => to_f64(a).partial_cmp(&to_f64(b)).unwrap_or(Ordering::Equal),
    }
}
