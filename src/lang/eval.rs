//! The v2 tree-walking evaluator (stage 3, in progress).
//!
//! Walks the owned AST and runs it over the byte-correct [`Value`] model.
//! This first increment covers scalars, arrays, the full control-flow set,
//! user functions, string interpolation, and a starter library of builtins.
//! Objects/classes, generators, and the rest of the ~250 builtins come next.
#![allow(dead_code)]

use super::ast::*;
use super::value::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug)]
pub struct RunError(pub String);
type R<T> = Result<T, RunError>;

/// Control-flow signal bubbled up from statement execution.
enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(Value),
}

pub struct Eval {
    out: Vec<u8>,
    /// Scope stack; `scopes[0]` is the global scope. Functions get a fresh scope.
    scopes: Vec<HashMap<String, Value>>,
    funcs: HashMap<String, Rc<FuncDecl>>,
    classes: HashMap<String, Rc<ClassDecl>>,
    consts: HashMap<String, Value>,
    /// Static property storage, keyed by (lowercased class, prop name).
    static_props: HashMap<(String, String), Value>,
    /// Class of the currently executing method, for `self`/`parent`/`static`.
    current_class: Option<String>,
    /// In-flight thrown exception (set by `throw`, cleared by a matching `catch`).
    thrown: Option<Value>,
    /// Current function/method call nesting — guards against stack overflow.
    call_depth: usize,
    /// Current expression-evaluation recursion depth (deep AST spines).
    eval_depth: usize,
    /// The file currently executing (for `__FILE__`/`__DIR__` + relative include).
    cur_file: Option<PathBuf>,
    /// Canonical paths already pulled in via `include_once`/`require_once`.
    included: HashSet<String>,
    /// Output-buffering watermarks into `out` (one per active `ob_start`).
    ob_stack: Vec<usize>,
    steps: u64,
}

const MAX_CALL_DEPTH: usize = 2000;

/// A minimal exception/error hierarchy, parsed before every program so that
/// `new Exception(...)`, `getMessage()`, `instanceof`, and `catch` work through
/// the ordinary class machinery.
const PRELUDE: &[u8] = br##"<?php
class stdClass {}
interface Throwable {}
interface Stringable {}
interface Traversable {}
interface Iterator extends Traversable {}
interface IteratorAggregate extends Traversable {}
interface ArrayAccess {}
interface Countable {}
interface JsonSerializable {}
class Exception {
    protected $message = "";
    protected $code = 0;
    protected $previous = null;
    public function __construct($message = "", $code = 0, $previous = null) {
        $this->message = $message; $this->code = $code; $this->previous = $previous;
    }
    public function getMessage() { return $this->message; }
    public function getCode() { return $this->code; }
    public function getPrevious() { return $this->previous; }
    public function getTrace() { return []; }
    public function getTraceAsString() { return "#0 {main}"; }
    public function getFile() { return ""; }
    public function getLine() { return 0; }
    public function __toString() { return $this->message; }
}
class ErrorException extends Exception {}
class Error extends Exception {}
class TypeError extends Error {}
class ValueError extends Error {}
class ArgumentCountError extends TypeError {}
class ArithmeticError extends Error {}
class DivisionByZeroError extends ArithmeticError {}
class UnhandledMatchError extends Error {}
class RuntimeException extends Exception {}
class LogicException extends Exception {}
class InvalidArgumentException extends LogicException {}
class OutOfRangeException extends LogicException {}
class OutOfBoundsException extends RuntimeException {}
class LengthException extends LogicException {}
class DomainException extends LogicException {}
class RangeException extends RuntimeException {}
class UnexpectedValueException extends RuntimeException {}
class UnderflowException extends RuntimeException {}
class OverflowException extends RuntimeException {}
class JsonException extends Exception {}
"##;

const STEP_LIMIT: u64 = 20_000_000;
/// Cap on single string allocations (concat, str_repeat) — stops memory bombs
/// from pathological corpus tests (huge `.=` / `str_repeat` / `range`).
const MAX_STR: usize = 64 * 1024 * 1024;
const MAX_RANGE: usize = 8_000_000;
/// Cap on total nodes in a single array value (memory-bomb guard).
const MAX_ARRAY_NODES: usize = 4_000_000;

impl Eval {
    pub fn new() -> Self {
        Eval {
            out: Vec::new(),
            scopes: vec![HashMap::new()],
            funcs: HashMap::new(),
            classes: HashMap::new(),
            consts: HashMap::new(),
            static_props: HashMap::new(),
            current_class: None,
            thrown: None,
            call_depth: 0,
            eval_depth: 0,
            cur_file: None,
            included: HashSet::new(),
            ob_stack: Vec::new(),
            steps: 0,
        }
    }

    /// Enter a call frame; errors if nesting would risk a native stack overflow.
    fn enter_call(&mut self) -> R<()> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return Err(RunError("maximum function nesting level reached".into()));
        }
        Ok(())
    }

    /// Register the exception/error hierarchy (parsed from PRELUDE).
    fn load_prelude(&mut self) {
        if let Ok(toks) = super::lexer::Lexer::tokenize(PRELUDE) {
            if let Ok(stmts) = super::parser::Parser::parse(toks) {
                self.hoist(&stmts);
            }
        }
    }

    /// Run a parsed program and return everything it printed.
    pub fn run(program: &[Stmt]) -> R<Vec<u8>> {
        Self::run_with_path(program, None)
    }

    /// Like [`run`], but records the script's path so `__FILE__`/`__DIR__` and
    /// relative `include`/`require` resolve against it.
    pub fn run_with_path(program: &[Stmt], path: Option<PathBuf>) -> R<Vec<u8>> {
        let mut e = Eval::new();
        e.cur_file = path;
        e.load_prelude();
        e.hoist(program);
        match e.exec_block(program) {
            Ok(_) => Ok(e.out),
            Err(err) => {
                // An uncaught exception becomes a PHP fatal-error message in the
                // output (matching PHP). Other engine errors (step limit, unknown
                // function, parse) propagate with their message so the scoreboard
                // histogram stays useful.
                if let Some(exc) = e.thrown.take() {
                    let (cls, msg) = e.exception_info(&exc);
                    let file = e
                        .cur_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let s = format!(
                        "\nFatal error: Uncaught {cls}: {msg} in {file}:0\nStack trace:\n#0 {{main}}\n  thrown in {file} on line 0\n"
                    );
                    e.out.extend_from_slice(s.as_bytes());
                    Ok(e.out)
                } else {
                    Err(err)
                }
            }
        }
    }

    fn exception_info(&self, v: &Value) -> (String, String) {
        if let Value::Object(rc) = v {
            let o = rc.borrow();
            let msg = o
                .get("message")
                .map(|m| String::from_utf8_lossy(&to_bytes(m)).into_owned())
                .unwrap_or_default();
            (o.class.clone(), msg)
        } else {
            ("Exception".into(), String::new())
        }
    }

    /// Hoist top-level function declarations so call-before-definition works.
    fn hoist(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match s {
                Stmt::Func(f) => {
                    self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f.clone()));
                }
                Stmt::Class(c) => {
                    self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c.clone()));
                }
                _ => {}
            }
        }
    }

    fn vars(&mut self) -> &mut HashMap<String, Value> {
        self.scopes.last_mut().unwrap()
    }

    fn tick(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > STEP_LIMIT {
            return Err(RunError("step limit exceeded".into()));
        }
        Ok(())
    }

    // ---- statements -----------------------------------------------------
    fn exec_block(&mut self, stmts: &[Stmt]) -> R<Flow> {
        for s in stmts {
            match self.exec(s)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec(&mut self, s: &Stmt) -> R<Flow> {
        self.tick()?;
        match s {
            Stmt::InlineHtml(b) => self.out.extend_from_slice(b),
            Stmt::Echo(items) => {
                for e in items {
                    let v = self.eval(e)?;
                    let b = self.stringify(&v)?;
                    self.out.extend_from_slice(&b);
                }
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
            }
            Stmt::Block(b) => return self.exec_block(b),
            Stmt::Nop => {}
            Stmt::Func(f) => {
                self.funcs.insert(f.name.to_ascii_lowercase(), Rc::new(f.clone()));
            }
            Stmt::ConstDecl(decls) => {
                for (name, e) in decls {
                    let v = self.eval(e)?;
                    self.consts.insert(name.clone(), v);
                }
            }
            Stmt::If { cond, then, elseifs, els } => {
                if to_bool(&self.eval(cond)?) {
                    return self.exec_block(then);
                }
                for (c, b) in elseifs {
                    if to_bool(&self.eval(c)?) {
                        return self.exec_block(b);
                    }
                }
                if let Some(b) = els {
                    return self.exec_block(b);
                }
            }
            Stmt::While { cond, body } => {
                while to_bool(&self.eval(cond)?) {
                    self.tick()?;
                    match self.exec_block(body)? {
                        Flow::Break(n) => return self.unwind_break(n),
                        Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                }
            }
            Stmt::DoWhile { body, cond } => loop {
                self.tick()?;
                match self.exec_block(body)? {
                    Flow::Break(n) => return self.unwind_break(n),
                    Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    _ => {}
                }
                if !to_bool(&self.eval(cond)?) {
                    break;
                }
            },
            Stmt::For { init, cond, step, body } => {
                for e in init {
                    self.eval(e)?;
                }
                loop {
                    self.tick()?;
                    let go = if let Some(last) = cond.last() {
                        for e in &cond[..cond.len() - 1] {
                            self.eval(e)?;
                        }
                        to_bool(&self.eval(last)?)
                    } else {
                        true
                    };
                    if !go {
                        break;
                    }
                    match self.exec_block(body)? {
                        Flow::Break(n) => return self.unwind_break(n),
                        Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    for e in step {
                        self.eval(e)?;
                    }
                }
            }
            Stmt::Foreach { array, key, value, by_ref: _, body } => {
                let arr = self.eval(array)?;
                if let Value::Array(a) = arr {
                    for (k, v) in a.entries.clone() {
                        self.tick()?;
                        if let Some(ke) = key {
                            let kv = match k {
                                Key::Int(n) => Value::Int(n),
                                Key::Str(s) => Value::Str(s),
                            };
                            self.assign_to(ke, kv)?;
                        }
                        self.assign_to(value, v)?;
                        match self.exec_block(body)? {
                            Flow::Break(n) => return self.unwind_break(n),
                            Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                            Flow::Return(rv) => return Ok(Flow::Return(rv)),
                            _ => {}
                        }
                    }
                }
            }
            Stmt::Switch { subject, cases } => {
                let subj = self.eval(subject)?;
                let mut matched = false;
                for case in cases {
                    if !matched {
                        match &case.test {
                            Some(t) => {
                                let tv = self.eval(t)?;
                                if loose_eq(&subj, &tv) {
                                    matched = true;
                                }
                            }
                            None => matched = true, // default
                        }
                    }
                    if matched {
                        match self.exec_block(&case.body)? {
                            Flow::Break(n) => return self.unwind_break(n),
                            Flow::Continue(n) if n > 1 => return Ok(Flow::Continue(n - 1)),
                            Flow::Continue(_) => return Ok(Flow::Normal),
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Normal => {}
                        }
                    }
                }
                // if nothing matched and there was a default earlier we'd have run it;
                // a trailing default with no prior match is handled by the loop above.
            }
            Stmt::Break(n) => return Ok(Flow::Break(*n)),
            Stmt::Continue(n) => return Ok(Flow::Continue(*n)),
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e)?,
                    None => Value::Null,
                };
                return Ok(Flow::Return(v));
            }
            Stmt::Unset(items) => {
                for it in items {
                    if let Expr::Var(name) = it {
                        self.vars().remove(name);
                    }
                }
            }
            Stmt::Global(names) => {
                // copy the global into the local scope (simplified: by value)
                for n in names {
                    let v = self.scopes[0].get(n).cloned().unwrap_or(Value::Null);
                    self.vars().insert(n.clone(), v);
                }
            }
            Stmt::Class(c) => {
                self.classes.insert(c.name.to_ascii_lowercase(), Rc::new(c.clone()));
            }
            Stmt::Throw(e) => {
                let v = self.eval(e)?;
                self.thrown = Some(v);
                return Err(RunError("__phargo_throw__".into()));
            }
            Stmt::Try { body, catches, finally } => {
                let outcome = self.exec_block(body);
                let mut result = self.handle_try_outcome(outcome, catches)?;
                if let Some(fin) = finally {
                    match self.exec_block(fin)? {
                        Flow::Normal => {}
                        other => result = other, // finally's flow wins
                    }
                }
                return Ok(result);
            }
            // not yet implemented in this increment — parsed but skipped
            Stmt::StaticVar(_)
            | Stmt::Namespace { .. }
            | Stmt::Use(_)
            | Stmt::Declare => {}
        }
        Ok(Flow::Normal)
    }

    fn unwind_break(&self, n: u32) -> R<Flow> {
        Ok(if n > 1 { Flow::Break(n - 1) } else { Flow::Normal })
    }

    // ---- expressions ----------------------------------------------------
    fn eval(&mut self, e: &Expr) -> R<Value> {
        self.tick()?;
        // Guard native-stack depth: deep left-associative spines (e.g. a long
        // `1+1+...+1`) build a deep AST even though the parser built it iteratively.
        self.eval_depth += 1;
        if self.eval_depth > 6000 {
            self.eval_depth -= 1;
            return Err(RunError("expression too deeply nested".into()));
        }
        let r = self.eval_inner(e);
        self.eval_depth -= 1;
        r
    }

    fn eval_inner(&mut self, e: &Expr) -> R<Value> {
        Ok(match e {
            Expr::Null => Value::Null,
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Int(n) => Value::Int(*n),
            Expr::Float(f) => Value::Float(*f),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Template(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    match p {
                        TplPart::Lit(b) => out.extend_from_slice(b),
                        TplPart::Expr(e) => {
                            let v = self.eval(e)?;
                            let b = self.stringify(&v)?;
                            out.extend_from_slice(&b);
                        }
                    }
                }
                Value::Str(out)
            }
            Expr::Array(items) => {
                let mut a = Arr::new();
                for it in items {
                    if it.spread {
                        if let Value::Array(src) = self.eval(&it.value)? {
                            for (k, v) in src.entries {
                                match k {
                                    Key::Int(_) => a.push(v),
                                    Key::Str(_) => a.insert(k, v),
                                }
                            }
                        }
                        continue;
                    }
                    let val = self.eval(&it.value)?;
                    match &it.key {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            a.insert(Arr::norm_key(&kv), val);
                        }
                        None => a.push(val),
                    }
                }
                Value::Array(a)
            }
            Expr::Var(name) => self.vars().get(name).cloned().unwrap_or(Value::Null),
            Expr::ConstFetch(name) => self.const_fetch(name),
            Expr::MagicConst(name) => match name.to_ascii_uppercase().as_str() {
                "__FILE__" => Value::Str(
                    self.cur_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().as_bytes().to_vec())
                        .unwrap_or_default(),
                ),
                "__DIR__" => Value::Str(
                    self.cur_file
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|d| d.to_string_lossy().as_bytes().to_vec())
                        .unwrap_or_default(),
                ),
                "__CLASS__" => Value::Str(self.current_class.clone().unwrap_or_default().into_bytes()),
                _ => Value::Str(Vec::new()),
            },
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                match op {
                    UnOp::Neg => match to_num(&v) {
                        Num::Int(n) => Value::Int(n.wrapping_neg()),
                        Num::Float(f) => Value::Float(-f),
                    },
                    UnOp::Pos => to_num(&v).to_value(),
                    UnOp::Not => Value::Bool(!to_bool(&v)),
                    UnOp::BitNot => Value::Int(!to_i64(&v)),
                }
            }
            Expr::Binary(op, l, r) => self.binary(*op, l, r)?,
            Expr::Assign(lhs, rhs) => {
                let v = self.eval(rhs)?;
                self.assign_to(lhs, v.clone())?;
                v
            }
            Expr::AssignRef(lhs, rhs) => {
                // references not modeled yet — behave as a value assignment
                let v = self.eval(rhs)?;
                self.assign_to(lhs, v.clone())?;
                v
            }
            Expr::AssignOp(op, lhs, rhs) => {
                // `$v .= expr` in place: append to the existing string instead of
                // cloning it each time (avoids O(n^2) growth on `.=` loops).
                if *op == BinOp::Concat {
                    if let Expr::Var(name) = &**lhs {
                        let rv = self.eval(rhs)?;
                        let rb = self.stringify(&rv)?;
                        let slot = self.vars().entry(name.clone()).or_insert(Value::Str(Vec::new()));
                        if let Value::Str(s) = slot {
                            if s.len() + rb.len() <= MAX_STR {
                                s.extend_from_slice(&rb);
                            }
                            return Ok(Value::Str(s.clone()));
                        } else {
                            let mut s = to_bytes(slot);
                            s.extend_from_slice(&rb);
                            let nv = Value::Str(s);
                            *slot = nv.clone();
                            return Ok(nv);
                        }
                    }
                }
                let cur = self.eval(lhs)?;
                let rv = self.eval(rhs)?;
                let nv = self.apply_bin(*op, &cur, &rv);
                self.assign_to(lhs, nv.clone())?;
                nv
            }
            Expr::PreInc(e) => {
                let v = inc(&self.eval(e)?, 1);
                self.assign_to(e, v.clone())?;
                v
            }
            Expr::PreDec(e) => {
                let v = inc(&self.eval(e)?, -1);
                self.assign_to(e, v.clone())?;
                v
            }
            Expr::PostInc(e) => {
                let old = self.eval(e)?;
                let v = inc(&old, 1);
                self.assign_to(e, v)?;
                old
            }
            Expr::PostDec(e) => {
                let old = self.eval(e)?;
                let v = inc(&old, -1);
                self.assign_to(e, v)?;
                old
            }
            Expr::Ternary(c, mid, els) => {
                let cv = self.eval(c)?;
                if to_bool(&cv) {
                    match mid {
                        Some(m) => self.eval(m)?,
                        None => cv, // a ?: b
                    }
                } else {
                    self.eval(els)?
                }
            }
            Expr::Index(base, idx) => self.read_index(base, idx)?,
            Expr::Call(callee, args) => self.eval_call(callee, args)?,
            Expr::Isset(items) => {
                let mut all = true;
                for it in items {
                    if matches!(self.eval(it)?, Value::Null) {
                        all = false;
                        break;
                    }
                }
                Value::Bool(all)
            }
            Expr::Empty(e) => Value::Bool(!to_bool(&self.eval(e)?)),
            Expr::ErrorSuppress(e) => self.eval(e).unwrap_or(Value::Null),
            Expr::Print(e) => {
                let v = self.eval(e)?;
                self.out.extend_from_slice(&to_bytes(&v));
                Value::Int(1)
            }
            Expr::Cast(ct, e) => {
                let v = self.eval(e)?;
                self.cast(*ct, v)
            }
            Expr::Match(subj, arms) => {
                let s = self.eval(subj)?;
                let mut result = None;
                for arm in arms {
                    match &arm.conditions {
                        Some(conds) => {
                            for c in conds {
                                let cv = self.eval(c)?;
                                if strict_eq(&s, &cv) {
                                    result = Some(self.eval(&arm.body)?);
                                    break;
                                }
                            }
                        }
                        None => {
                            result = Some(self.eval(&arm.body)?);
                        }
                    }
                    if result.is_some() {
                        break;
                    }
                }
                result.ok_or_else(|| RunError("UnhandledMatchError".into()))?
            }
            Expr::New(class, args) => {
                let cname = self.resolve_class_name(class)?;
                let argv = self.eval_args(args)?;
                self.instantiate(&cname, argv)?
            }
            Expr::Prop(obj, name, nullsafe) => {
                let o = self.eval(obj)?;
                if *nullsafe && matches!(o, Value::Null) {
                    return Ok(Value::Null);
                }
                let pname = self.prop_name_str(name)?;
                match &o {
                    Value::Object(rc) => {
                        rc.borrow().get(&pname).cloned().unwrap_or(Value::Null)
                    }
                    _ => Value::Null,
                }
            }
            Expr::MethodCall(obj, name, args, nullsafe) => {
                let o = self.eval(obj)?;
                if *nullsafe && matches!(o, Value::Null) {
                    return Ok(Value::Null);
                }
                let mname = self.prop_name_str(name)?;
                let argv = self.eval_args(args)?;
                self.call_method(o, &mname, argv)?
            }
            Expr::StaticCall(class, name, args) => {
                let cname = self.resolve_class_name(class)?;
                let mname = self.prop_name_str(name)?;
                let argv = self.eval_args(args)?;
                // `parent::`/`self::` keep the current $this if present
                let this = self.vars().get("this").cloned();
                self.call_static(&cname, &mname, argv, this)?
            }
            Expr::ClassConst(class, name) => {
                if name == "class" {
                    let cname = self.resolve_class_name(class)?;
                    Value::Str(cname.into_bytes())
                } else {
                    let cname = self.resolve_class_name(class)?;
                    self.class_const(&cname, name)?
                }
            }
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                self.static_props
                    .get(&(cname.to_ascii_lowercase(), name.clone()))
                    .cloned()
                    .unwrap_or(Value::Null)
            }
            Expr::Throw(inner) => {
                let v = self.eval(inner)?;
                self.thrown = Some(v);
                return Err(RunError("__phargo_throw__".into()));
            }
            Expr::Closure(c) => {
                let mut captures = Vec::new();
                for u in &c.uses {
                    let v = self.vars().get(&u.name).cloned().unwrap_or(Value::Null);
                    captures.push((u.name.clone(), v));
                }
                let bound_this = if c.is_static {
                    None
                } else {
                    self.vars().get("this").cloned()
                };
                Value::Closure(Rc::new(ClosureVal {
                    kind: ClosureKind::Full(Rc::new((**c).clone())),
                    captures,
                    bound_this,
                }))
            }
            Expr::ArrowFn(a) => {
                // arrow fns auto-capture the entire enclosing scope by value
                let captures: Vec<(String, Value)> =
                    self.vars().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let bound_this = if a.is_static {
                    None
                } else {
                    self.vars().get("this").cloned()
                };
                Value::Closure(Rc::new(ClosureVal {
                    kind: ClosureKind::Arrow(Rc::new((**a).clone())),
                    captures,
                    bound_this,
                }))
            }
            Expr::InstanceOf(e, class) => {
                let v = self.eval(e)?;
                let target = self.resolve_class_name(class)?;
                Value::Bool(match &v {
                    Value::Object(rc) => {
                        let c = rc.borrow().class.clone();
                        self.is_subclass(&c, &target)
                    }
                    _ => false,
                })
            }
            // constructs not in this increment
            _ => Value::Null,
        })
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr) -> R<Value> {
        // short-circuit logical operators
        match op {
            BinOp::And => {
                let lv = self.eval(l)?;
                return Ok(Value::Bool(to_bool(&lv) && to_bool(&self.eval(r)?)));
            }
            BinOp::Or => {
                let lv = self.eval(l)?;
                return Ok(Value::Bool(to_bool(&lv) || to_bool(&self.eval(r)?)));
            }
            BinOp::Coalesce => {
                let lv = self.eval(l)?;
                return Ok(if matches!(lv, Value::Null) { self.eval(r)? } else { lv });
            }
            BinOp::Concat => {
                // honor __toString on either operand
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                let mut s = self.stringify(&lv)?;
                let rb = self.stringify(&rv)?;
                if s.len() + rb.len() <= MAX_STR {
                    s.extend_from_slice(&rb);
                }
                return Ok(Value::Str(s));
            }
            _ => {}
        }
        let lv = self.eval(l)?;
        let rv = self.eval(r)?;
        Ok(self.apply_bin(op, &lv, &rv))
    }

    fn apply_bin(&self, op: BinOp, l: &Value, r: &Value) -> Value {
        use BinOp::*;
        match op {
            Add => {
                if let (Value::Array(a), Value::Array(b)) = (l, r) {
                    let mut out = a.clone();
                    for (k, v) in &b.entries {
                        if out.get(k).is_none() {
                            out.insert(k.clone(), v.clone());
                        }
                    }
                    return Value::Array(out);
                }
                num_arith(l, r, |a, b| a.wrapping_add(b), |a, b| a + b)
            }
            Sub => num_arith(l, r, |a, b| a.wrapping_sub(b), |a, b| a - b),
            Mul => num_arith(l, r, |a, b| a.wrapping_mul(b), |a, b| a * b),
            Div => {
                let rf = to_f64(r);
                if rf == 0.0 {
                    return Value::Bool(false); // div-by-zero (legacy-ish); real PHP throws
                }
                match (to_num(l), to_num(r)) {
                    (Num::Int(a), Num::Int(b)) if b != 0 && a % b == 0 => Value::Int(a / b),
                    _ => Value::Float(to_f64(l) / rf),
                }
            }
            Mod => {
                let b = to_i64(r);
                if b == 0 {
                    Value::Bool(false)
                } else {
                    Value::Int(to_i64(l).wrapping_rem(b))
                }
            }
            Pow => {
                match (to_num(l), to_num(r)) {
                    (Num::Int(a), Num::Int(b)) if b >= 0 && b < 64 => {
                        match a.checked_pow(b as u32) {
                            Some(n) => Value::Int(n),
                            None => Value::Float((a as f64).powf(b as f64)),
                        }
                    }
                    _ => Value::Float(to_f64(l).powf(to_f64(r))),
                }
            }
            Concat => {
                let mut s = to_bytes(l);
                let rb = to_bytes(r);
                if s.len() + rb.len() <= MAX_STR {
                    s.extend_from_slice(&rb);
                }
                Value::Str(s)
            }
            Eq => Value::Bool(loose_eq(l, r)),
            NotEq => Value::Bool(!loose_eq(l, r)),
            Identical => Value::Bool(strict_eq(l, r)),
            NotIdentical => Value::Bool(!strict_eq(l, r)),
            Lt => Value::Bool(compare(l, r) == std::cmp::Ordering::Less),
            Gt => Value::Bool(compare(l, r) == std::cmp::Ordering::Greater),
            Le => Value::Bool(compare(l, r) != std::cmp::Ordering::Greater),
            Ge => Value::Bool(compare(l, r) != std::cmp::Ordering::Less),
            Spaceship => Value::Int(match compare(l, r) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }),
            BitAnd => Value::Int(to_i64(l) & to_i64(r)),
            BitOr => Value::Int(to_i64(l) | to_i64(r)),
            BitXor => Value::Int(to_i64(l) ^ to_i64(r)),
            Shl => Value::Int(to_i64(l).wrapping_shl(to_i64(r) as u32)),
            Shr => Value::Int(to_i64(l).wrapping_shr(to_i64(r) as u32)),
            Xor => Value::Bool(to_bool(l) ^ to_bool(r)),
            // logicals handled in `binary`
            And | Or | Coalesce => Value::Null,
        }
    }

    fn cast(&self, ct: CastType, v: Value) -> Value {
        match ct {
            CastType::Int => Value::Int(to_i64(&v)),
            CastType::Float => Value::Float(to_f64(&v)),
            CastType::String => Value::Str(to_bytes(&v)),
            CastType::Bool => Value::Bool(to_bool(&v)),
            CastType::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Arr::new()),
                other => {
                    let mut a = Arr::new();
                    a.push(other);
                    Value::Array(a)
                }
            },
            CastType::Object => match v {
                Value::Object(_) | Value::Closure(_) => v,
                Value::Array(a) => {
                    let mut o = Obj { class: "stdClass".into(), props: Vec::new() };
                    for (k, val) in a.entries {
                        let name = match k {
                            Key::Int(n) => n.to_string(),
                            Key::Str(s) => String::from_utf8_lossy(&s).into_owned(),
                        };
                        o.set(&name, val);
                    }
                    Value::Object(Rc::new(RefCell::new(o)))
                }
                Value::Null => new_obj("stdClass"),
                other => {
                    let o = Rc::new(RefCell::new(Obj { class: "stdClass".into(), props: Vec::new() }));
                    o.borrow_mut().set("scalar", other);
                    Value::Object(o)
                }
            },
            CastType::Unset => Value::Null,
        }
    }

    /// Read `$var[i]`, `$var[i][j]`, ... by navigating the stored container by
    /// reference and cloning only the final element. (Naively evaluating `base`
    /// clones the whole container on every access — O(n^2) in array-heavy loops.)
    fn read_index(&mut self, base: &Expr, idx: &Option<Box<Expr>>) -> R<Value> {
        let first = match idx {
            Some(i) => Arr::norm_key(&self.eval(i)?),
            None => return Ok(Value::Null), // `$a[]` isn't a readable expression
        };
        // Unwind a chain of `Index` nodes down to the root, evaluating each key.
        let mut keys = vec![first];
        let mut node = base;
        loop {
            match node {
                Expr::Index(b, Some(i)) => {
                    keys.push(Arr::norm_key(&self.eval(i)?));
                    node = b;
                }
                Expr::Var(_) => break,
                // root isn't a plain variable (e.g. a call result) — fall back to
                // evaluating it once, then indexing.
                other => {
                    keys.reverse();
                    let mut v = self.eval(other)?;
                    for k in &keys {
                        v = self.index_get_key(&v, k);
                    }
                    return Ok(v);
                }
            }
        }
        keys.reverse();
        let name = match node {
            Expr::Var(n) => n.clone(),
            _ => unreachable!(),
        };
        // Navigate by reference (keys already evaluated — no &mut self needed here).
        let scope = self.scopes.last().unwrap();
        let mut v = match scope.get(&name) {
            Some(v) => v,
            None => return Ok(Value::Null),
        };
        for k in &keys {
            match v {
                Value::Array(a) => {
                    v = match a.get(k) {
                        Some(x) => x,
                        None => return Ok(Value::Null),
                    };
                }
                Value::Str(s) => return Ok(string_char(s, k)),
                _ => return Ok(Value::Null),
            }
        }
        Ok(v.clone())
    }

    fn index_get_key(&self, base: &Value, k: &Key) -> Value {
        match base {
            Value::Array(a) => a.get(k).cloned().unwrap_or(Value::Null),
            Value::Str(s) => string_char(s, k),
            _ => Value::Null,
        }
    }

    fn index_get(&self, base: &Value, idx: &Value) -> Value {
        match base {
            Value::Array(a) => a.get(&Arr::norm_key(idx)).cloned().unwrap_or(Value::Null),
            Value::Str(s) => {
                let i = to_i64(idx);
                let i = if i < 0 { s.len() as i64 + i } else { i };
                if i >= 0 && (i as usize) < s.len() {
                    Value::Str(vec![s[i as usize]])
                } else {
                    Value::Str(Vec::new())
                }
            }
            _ => Value::Null,
        }
    }

    fn const_fetch(&self, name: &Name) -> Value {
        let n = name.last();
        if let Some(v) = self.consts.get(n) {
            return v.clone();
        }
        php_const(n).unwrap_or_else(|| {
            // unknown bareword → its own name as a string (PHP 7 behavior-ish)
            Value::Str(n.as_bytes().to_vec())
        })
    }

    // ---- assignment targets --------------------------------------------
    fn assign_to(&mut self, target: &Expr, val: Value) -> R<()> {
        // Guard against pathological array explosion (e.g. self-referential
        // value-copy where references aren't modeled yet).
        if matches!(val, Value::Array(_)) && value_size(&val, MAX_ARRAY_NODES) > MAX_ARRAY_NODES {
            return Err(self.throw_error("Error", "Allocated array exceeds memory limit"));
        }
        match target {
            Expr::Var(name) => {
                self.vars().insert(name.clone(), val);
            }
            Expr::Index(base, idx) => {
                // ensure base is an array, then set/append
                let key = match idx {
                    Some(i) => Some(Arr::norm_key(&self.eval(i)?)),
                    None => None,
                };
                self.assign_index(base, key, val)?;
            }
            Expr::Prop(obj, name, _) => {
                let o = self.eval(obj)?;
                let pname = self.prop_name_str(name)?;
                if let Value::Object(rc) = o {
                    rc.borrow_mut().set(&pname, val);
                }
            }
            Expr::StaticProp(class, name) => {
                let cname = self.resolve_class_name(class)?;
                self.static_props
                    .insert((cname.to_ascii_lowercase(), name.clone()), val);
            }
            // list/array destructuring: [$a, $b] = ...  and  list($a, $b) = ...
            Expr::Array(_) | Expr::List(_) => {
                self.destructure(target, val)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn assign_index(&mut self, base: &Expr, key: Option<Key>, val: Value) -> R<()> {
        // Read-modify-write the base container. Only simple `$var[...]` (one level)
        // and nested `$var[a][b]` are handled here.
        match base {
            Expr::Var(name) => {
                let entry = self
                    .vars()
                    .entry(name.clone())
                    .or_insert_with(|| Value::Array(Arr::new()));
                if !matches!(entry, Value::Array(_)) {
                    *entry = Value::Array(Arr::new());
                }
                if let Value::Array(a) = entry {
                    match key {
                        Some(k) => a.insert(k, val),
                        None => a.push(val),
                    }
                }
                Ok(())
            }
            Expr::Index(inner, iidx) => {
                // nested: evaluate current, mutate, write back
                let ikey = match iidx {
                    Some(i) => Some(Arr::norm_key(&self.eval(i)?)),
                    None => None,
                };
                let mut cur = self.eval(base).unwrap_or(Value::Array(Arr::new()));
                if !matches!(cur, Value::Array(_)) {
                    cur = Value::Array(Arr::new());
                }
                if let Value::Array(a) = &mut cur {
                    match key {
                        Some(k) => a.insert(k, val),
                        None => a.push(val),
                    }
                }
                self.assign_index(inner, ikey, cur)
            }
            // `$obj->prop[...] = v` — mutate the property array IN PLACE (cloning
            // it on every append is O(n^2); `$this->arr[] = …` loops crawl).
            Expr::Prop(objexpr, name, _) => {
                let o = self.eval(objexpr)?;
                let pname = self.prop_name_str(name)?;
                if let Value::Object(rc) = o {
                    let mut b = rc.borrow_mut();
                    if !matches!(b.get(&pname), Some(Value::Array(_))) {
                        b.set(&pname, Value::Array(Arr::new()));
                    }
                    if let Some(Value::Array(a)) = b.get_mut(&pname) {
                        match key {
                            Some(k) => a.insert(k, val),
                            None => a.push(val),
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn destructure(&mut self, target: &Expr, val: Value) -> R<()> {
        let items: Vec<Option<ArrayItem>> = match target {
            Expr::List(items) => items.clone(),
            Expr::Array(items) => items.iter().cloned().map(Some).collect(),
            _ => return Ok(()),
        };
        if let Value::Array(a) = val {
            let mut idx = 0i64;
            for it in items {
                if let Some(item) = it {
                    let key = match &item.key {
                        Some(ke) => Arr::norm_key(&self.eval(ke)?),
                        None => {
                            let k = Key::Int(idx);
                            idx += 1;
                            k
                        }
                    };
                    let v = a.get(&key).cloned().unwrap_or(Value::Null);
                    self.assign_to(&item.value, v)?;
                } else {
                    idx += 1;
                }
            }
        }
        Ok(())
    }

    // ---- calls ----------------------------------------------------------
    fn eval_call(&mut self, callee: &Expr, args: &[Arg]) -> R<Value> {
        // direct named call: foo(...)
        if let Expr::ConstFetch(n) = callee {
            // first-class callable: foo(...)
            if args.len() == 1 && args[0].name.as_deref() == Some("...") {
                return Ok(Value::Str(n.last().as_bytes().to_vec()));
            }
            let name = n.last().to_ascii_lowercase();
            let argv = self.eval_args(args)?;
            // include/require/eval need the lex->parse->exec machinery
            match name.as_str() {
                "include" | "include_once" | "require" | "require_once" => {
                    let path = String::from_utf8_lossy(&to_bytes(argv.get(0).unwrap_or(&Value::Null)))
                        .into_owned();
                    let require = name.starts_with("require");
                    let once = name.ends_with("_once");
                    return self.do_include(&path, require, once);
                }
                "eval" => {
                    let code = to_bytes(argv.get(0).unwrap_or(&Value::Null));
                    return self.do_eval(&code);
                }
                _ => {}
            }
            if let Some(f) = self.funcs.get(&name).cloned() {
                return self.call_user(&f, argv);
            }
            return self.builtin(&name, argv);
        }
        // dynamic callee: $f(...), expr(...) — evaluate to a callable value
        let cv = self.eval(callee)?;
        let argv = self.eval_args(args)?;
        self.call_value(cv, argv)
    }

    /// `include`/`require`: load a file, lex+parse it, and execute its statements
    /// in the current scope (sharing globals; functions/classes register globally).
    fn do_include(&mut self, path: &str, require: bool, once: bool) -> R<Value> {
        let resolved = self.resolve_include(path);
        let key = resolved.to_string_lossy().to_ascii_lowercase();
        if once && self.included.contains(&key) {
            return Ok(Value::Bool(true));
        }
        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(_) => {
                if require {
                    return Err(self.throw_error(
                        "Error",
                        &format!("require(): Failed opening required '{path}'"),
                    ));
                }
                return Ok(Value::Bool(false)); // include() warns and returns false
            }
        };
        self.included.insert(key);
        self.run_source(&bytes, Some(resolved))
    }

    /// `eval("code")`: lex+parse the string and execute it in the current scope.
    fn do_eval(&mut self, code: &[u8]) -> R<Value> {
        // PHP's eval string has no leading `<?php`; wrap it so the lexer enters code mode.
        let mut full = b"<?php ".to_vec();
        full.extend_from_slice(code);
        let cur = self.cur_file.clone();
        self.run_source(&full, cur)
    }

    fn run_source(&mut self, bytes: &[u8], path: Option<PathBuf>) -> R<Value> {
        let toks = super::lexer::Lexer::tokenize(bytes)
            .map_err(|e| RunError(format!("Parse error: {}", e.msg)))?;
        let ast = super::parser::Parser::parse(toks)
            .map_err(|e| RunError(format!("Parse error: {}", e.msg)))?;
        let prev_file = std::mem::replace(&mut self.cur_file, path);
        self.hoist(&ast);
        let r = self.exec_block(&ast);
        self.cur_file = prev_file;
        match r? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Int(1)), // include/eval default return value
        }
    }

    /// Resolve an include path: absolute as-is, else relative to the current
    /// file's directory, else relative to the working directory.
    fn resolve_include(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() && p.exists() {
            return p;
        }
        if let Some(dir) = self.cur_file.as_ref().and_then(|f| f.parent()) {
            let cand = dir.join(path);
            if cand.exists() {
                return cand;
            }
        }
        p
    }

    /// Invoke any callable value: closure, function-name string, `[obj, "m"]`,
    /// or an object with `__invoke`.
    fn call_value(&mut self, cv: Value, args: Vec<Value>) -> R<Value> {
        match cv {
            Value::Closure(c) => self.call_closure(&c, args),
            Value::Str(s) => {
                let name = String::from_utf8_lossy(&s).to_ascii_lowercase();
                if let Some(f) = self.funcs.get(&name).cloned() {
                    self.call_user(&f, args)
                } else {
                    self.builtin(&name, args)
                }
            }
            Value::Object(rc) => {
                let class = rc.borrow().class.clone();
                if self.find_method(&class, "__invoke").is_some() {
                    self.call_method(Value::Object(rc), "__invoke", args)
                } else {
                    Ok(Value::Null)
                }
            }
            Value::Array(a) if a.len() == 2 => {
                let recv = a.get(&Key::Int(0)).cloned().unwrap_or(Value::Null);
                let m = a.get(&Key::Int(1)).cloned().unwrap_or(Value::Null);
                let mname = String::from_utf8_lossy(&to_bytes(&m)).into_owned();
                match recv {
                    Value::Object(_) => self.call_method(recv, &mname, args),
                    Value::Str(s) => {
                        let cn = String::from_utf8_lossy(&s).into_owned();
                        self.call_static(&cn, &mname, args, None)
                    }
                    _ => Ok(Value::Null),
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn call_closure(&mut self, c: &ClosureVal, args: Vec<Value>) -> R<Value> {
        self.enter_call()?;
        let mut scope = HashMap::new();
        for (k, v) in &c.captures {
            scope.insert(k.clone(), v.clone());
        }
        if let Some(t) = &c.bound_this {
            scope.insert("this".to_string(), t.clone());
        }
        let r = match &c.kind {
            ClosureKind::Full(f) => {
                self.bind_params(&mut scope, &f.params, &args)?;
                self.scopes.push(scope);
                let r = self.exec_block(&f.body);
                self.scopes.pop();
                r.map(|flow| match flow {
                    Flow::Return(v) => v,
                    _ => Value::Null,
                })
            }
            ClosureKind::Arrow(f) => {
                self.bind_params(&mut scope, &f.params, &args)?;
                self.scopes.push(scope);
                let r = self.eval(&f.body);
                self.scopes.pop();
                r
            }
        };
        self.call_depth -= 1;
        r
    }

    fn call_user(&mut self, f: &FuncDecl, args: Vec<Value>) -> R<Value> {
        self.enter_call()?;
        let mut scope = HashMap::new();
        for (i, p) in f.params.iter().enumerate() {
            if p.variadic {
                let mut rest = Arr::new();
                for v in args.iter().skip(i) {
                    rest.push(v.clone());
                }
                scope.insert(p.name.clone(), Value::Array(rest));
                break;
            }
            let v = match args.get(i) {
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                },
            };
            scope.insert(p.name.clone(), v);
        }
        self.scopes.push(scope);
        let r = self.exec_block(&f.body);
        self.scopes.pop();
        self.call_depth -= 1;
        match r? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Null),
        }
    }
}

// ---- objects & classes -------------------------------------------------
impl Eval {
    fn find_class(&self, name: &str) -> Option<Rc<ClassDecl>> {
        self.classes.get(&name.to_ascii_lowercase()).cloned()
    }

    /// Resolve a class reference expression to a class name.
    fn resolve_class_name(&mut self, e: &Expr) -> R<String> {
        match e {
            Expr::ConstFetch(n) => {
                let last = n.last();
                match last.to_ascii_lowercase().as_str() {
                    "self" | "static" => Ok(self
                        .current_class
                        .clone()
                        .unwrap_or_else(|| last.to_string())),
                    "parent" => {
                        let cur = self.current_class.clone().unwrap_or_default();
                        Ok(self
                            .find_class(&cur)
                            .and_then(|c| c.parent.as_ref().map(|p| p.last().to_string()))
                            .unwrap_or(cur))
                    }
                    _ => Ok(last.to_string()),
                }
            }
            _ => {
                let v = self.eval(e)?;
                match v {
                    Value::Str(s) => Ok(String::from_utf8_lossy(&s).into_owned()),
                    Value::Object(rc) => Ok(rc.borrow().class.clone()),
                    _ => Ok(String::new()),
                }
            }
        }
    }

    /// Convert a value to bytes for output, honoring `__toString` on objects.
    fn stringify(&mut self, v: &Value) -> R<Vec<u8>> {
        if let Value::Object(rc) = v {
            let class = rc.borrow().class.clone();
            if self.find_method(&class, "__tostring").is_some() {
                let r = self.call_method(v.clone(), "__toString", vec![])?;
                return Ok(to_bytes(&r));
            }
        }
        Ok(to_bytes(v))
    }

    fn prop_name_str(&mut self, p: &PropName) -> R<String> {
        Ok(match p {
            PropName::Id(s) => s.clone(),
            PropName::Expr(e) => String::from_utf8_lossy(&to_bytes(&self.eval(e)?)).into_owned(),
        })
    }

    fn eval_args(&mut self, args: &[Arg]) -> R<Vec<Value>> {
        let mut out = Vec::new();
        for a in args {
            if a.name.as_deref() == Some("...") {
                continue;
            }
            if a.spread {
                if let Value::Array(arr) = self.eval(&a.value)? {
                    for (_, v) in arr.entries {
                        out.push(v);
                    }
                }
            } else {
                out.push(self.eval(&a.value)?);
            }
        }
        Ok(out)
    }

    /// The ancestor chain (self first, then parents), as class decls.
    fn ancestry(&self, name: &str) -> Vec<Rc<ClassDecl>> {
        let mut out = Vec::new();
        let mut cur = self.find_class(name);
        let mut guard = 0;
        while let Some(c) = cur {
            out.push(c.clone());
            guard += 1;
            if guard > 50 {
                break;
            }
            cur = c.parent.as_ref().and_then(|p| self.find_class(p.last()));
        }
        out
    }

    fn is_subclass(&self, class: &str, target: &str) -> bool {
        let t = target.to_ascii_lowercase();
        for c in self.ancestry(class) {
            if c.name.to_ascii_lowercase() == t {
                return true;
            }
            for i in &c.interfaces {
                if i.last().to_ascii_lowercase() == t {
                    return true;
                }
            }
        }
        false
    }

    /// Find a method (and its declaring class) walking up the hierarchy.
    fn find_method(&self, class: &str, method: &str) -> Option<(String, MethodDecl)> {
        let m = method.to_ascii_lowercase();
        for c in self.ancestry(class) {
            // traits first (declared in the class), then own methods
            for t in &c.uses_traits {
                if let Some(tc) = self.find_class(t.last()) {
                    if let Some(md) = tc.methods.iter().find(|x| x.name.to_ascii_lowercase() == m) {
                        return Some((c.name.clone(), md.clone()));
                    }
                }
            }
            if let Some(md) = c.methods.iter().find(|x| x.name.to_ascii_lowercase() == m) {
                return Some((c.name.clone(), md.clone()));
            }
        }
        None
    }

    fn instantiate(&mut self, class: &str, args: Vec<Value>) -> R<Value> {
        let decl = match self.find_class(class) {
            Some(d) => d,
            None => return Err(RunError(format!("class {class} not found"))),
        };
        let obj = Rc::new(RefCell::new(Obj { class: decl.name.clone(), props: Vec::new() }));
        // initialize declared (instance) properties from the whole hierarchy,
        // base-most first so overrides win.
        let chain = self.ancestry(class);
        for c in chain.iter().rev() {
            for p in &c.props {
                if p.is_static {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                };
                obj.borrow_mut().set(&p.name, v);
            }
        }
        let ov = Value::Object(obj);
        // constructor
        if self.find_method(class, "__construct").is_some() {
            self.call_method(ov.clone(), "__construct", args)?;
        }
        Ok(ov)
    }

    fn call_method(&mut self, recv: Value, method: &str, args: Vec<Value>) -> R<Value> {
        let class = match &recv {
            Value::Object(rc) => rc.borrow().class.clone(),
            _ => return Ok(Value::Null),
        };
        let (decl_class, m) = match self.find_method(&class, method) {
            Some(x) => x,
            None => {
                // __call magic fallback
                if let Some((dc, _)) = self.find_method(&class, "__call") {
                    let mut a = Arr::new();
                    for v in args {
                        a.push(v);
                    }
                    let cargs = vec![Value::Str(method.as_bytes().to_vec()), Value::Array(a)];
                    return self.invoke_method(recv, &dc, &self.find_method(&class, "__call").unwrap().1.clone(), cargs);
                }
                return Err(RunError(format!("call to undefined method {class}::{method}()")));
            }
        };
        self.invoke_method(recv, &decl_class, &m, args)
    }

    fn invoke_method(
        &mut self,
        recv: Value,
        decl_class: &str,
        m: &MethodDecl,
        args: Vec<Value>,
    ) -> R<Value> {
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        let mut scope = HashMap::new();
        if !m.is_static {
            scope.insert("this".to_string(), recv.clone());
        }
        self.bind_params(&mut scope, &m.params, &args)?;
        // constructor property promotion
        if m.name.eq_ignore_ascii_case("__construct") {
            if let Value::Object(rc) = &recv {
                for (i, p) in m.params.iter().enumerate() {
                    if p.promote.is_some() {
                        let v = args
                            .get(i)
                            .cloned()
                            .or_else(|| scope.get(&p.name).cloned())
                            .unwrap_or(Value::Null);
                        rc.borrow_mut().set(&p.name, v);
                    }
                }
            }
        }
        let prev_class = self.current_class.replace(decl_class.to_string());
        self.scopes.push(scope);
        let r = self.exec_block(&body);
        self.scopes.pop();
        self.current_class = prev_class;
        self.call_depth -= 1;
        match r? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Null),
        }
    }

    fn call_static(
        &mut self,
        class: &str,
        method: &str,
        args: Vec<Value>,
        this: Option<Value>,
    ) -> R<Value> {
        let (decl_class, m) = match self.find_method(class, method) {
            Some(x) => x,
            None => return Err(RunError(format!("call to undefined method {class}::{method}()"))),
        };
        let body = match &m.body {
            Some(b) => b.clone(),
            None => return Ok(Value::Null),
        };
        self.enter_call()?;
        let mut scope = HashMap::new();
        // a non-static method reached via parent::/self:: keeps $this
        if !m.is_static {
            if let Some(t) = this {
                scope.insert("this".to_string(), t);
            }
        }
        self.bind_params(&mut scope, &m.params, &args)?;
        let prev_class = self.current_class.replace(decl_class.clone());
        self.scopes.push(scope);
        let r = self.exec_block(&body);
        self.scopes.pop();
        self.current_class = prev_class;
        self.call_depth -= 1;
        match r? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Null),
        }
    }

    fn bind_params(
        &mut self,
        scope: &mut HashMap<String, Value>,
        params: &[Param],
        args: &[Value],
    ) -> R<()> {
        for (i, p) in params.iter().enumerate() {
            if p.variadic {
                let mut rest = Arr::new();
                for v in args.iter().skip(i) {
                    rest.push(v.clone());
                }
                scope.insert(p.name.clone(), Value::Array(rest));
                break;
            }
            let v = match args.get(i) {
                Some(v) => v.clone(),
                None => match &p.default {
                    Some(d) => self.eval(d)?,
                    None => Value::Null,
                },
            };
            scope.insert(p.name.clone(), v);
        }
        Ok(())
    }

    /// Given the result of a `try` body, dispatch to a matching `catch`.
    fn handle_try_outcome(&mut self, outcome: R<Flow>, catches: &[Catch]) -> R<Flow> {
        let err = match outcome {
            Ok(flow) => return Ok(flow),
            Err(e) => e,
        };
        let exc = match self.thrown.take() {
            Some(v) => v,
            None => return Err(err), // not an exception — propagate (e.g. step limit)
        };
        let cls = match &exc {
            Value::Object(rc) => rc.borrow().class.clone(),
            _ => String::new(),
        };
        for c in catches {
            if c.types.iter().any(|t| self.is_subclass(&cls, t.last())) {
                if let Some(var) = &c.var {
                    self.vars().insert(var.clone(), exc.clone());
                }
                return self.exec_block(&c.body);
            }
        }
        // no catch matched — re-throw
        self.thrown = Some(exc);
        Err(err)
    }

    /// Instantiate `class(msg)` and arm it as the pending throw.
    fn throw_error(&mut self, class: &str, msg: &str) -> RunError {
        let v = self
            .instantiate(class, vec![Value::Str(msg.as_bytes().to_vec())])
            .unwrap_or(Value::Null);
        self.thrown = Some(v);
        RunError("__phargo_throw__".into())
    }

    fn class_const(&mut self, class: &str, name: &str) -> R<Value> {
        // enum case?
        if let Some(c) = self.find_class(class) {
            if c.kind == ClassKind::Enum {
                if c.cases.iter().any(|e| e.name == name) {
                    // model an enum case as an object with `name` (+ `value`)
                    let obj = Rc::new(RefCell::new(Obj { class: c.name.clone(), props: Vec::new() }));
                    obj.borrow_mut().set("name", Value::Str(name.as_bytes().to_vec()));
                    if let Some(ec) = c.cases.iter().find(|e| e.name == name) {
                        if let Some(v) = &ec.value {
                            let val = self.eval(v)?;
                            obj.borrow_mut().set("value", val);
                        }
                    }
                    return Ok(Value::Object(obj));
                }
            }
        }
        for c in self.ancestry(class) {
            if let Some(cc) = c.consts.iter().find(|x| x.name == name) {
                return self.eval(&cc.value.clone());
            }
            // interface constants
            for i in &c.interfaces {
                if let Some(ic) = self.find_class(i.last()) {
                    if let Some(cc) = ic.consts.iter().find(|x| x.name == name) {
                        return self.eval(&cc.value.clone());
                    }
                }
            }
        }
        Err(RunError(format!("undefined constant {class}::{name}")))
    }
}

// ---- builtin library (starter set) -------------------------------------
impl Eval {
    fn builtin(&mut self, name: &str, args: Vec<Value>) -> R<Value> {
        let a = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        Ok(match name {
            "strlen" => Value::Int(to_bytes(&a(0)).len() as i64),
            "count" | "sizeof" => match a(0) {
                Value::Array(arr) => Value::Int(arr.len() as i64),
                Value::Null => Value::Int(0),
                _ => Value::Int(1),
            },
            "var_dump" => {
                for v in &args {
                    let mut s = String::new();
                    var_dump(v, 0, &mut s);
                    self.out.extend_from_slice(s.as_bytes());
                }
                Value::Null
            }
            "print_r" => {
                let mut s = String::new();
                print_r(&a(0), 0, &mut s);
                if to_bool(&a(1)) {
                    Value::Str(s.into_bytes())
                } else {
                    self.out.extend_from_slice(s.as_bytes());
                    Value::Bool(true)
                }
            }
            "gettype" => Value::Str(type_name(&a(0)).as_bytes().to_vec()),
            "is_int" | "is_integer" | "is_long" => Value::Bool(matches!(a(0), Value::Int(_))),
            "is_float" | "is_double" => Value::Bool(matches!(a(0), Value::Float(_))),
            "is_string" => Value::Bool(matches!(a(0), Value::Str(_))),
            "is_bool" => Value::Bool(matches!(a(0), Value::Bool(_))),
            "is_array" => Value::Bool(matches!(a(0), Value::Array(_))),
            "is_null" => Value::Bool(matches!(a(0), Value::Null)),
            "is_numeric" => Value::Bool(match a(0) {
                Value::Int(_) | Value::Float(_) => true,
                Value::Str(s) => is_numeric_str(&s),
                _ => false,
            }),
            "is_scalar" => Value::Bool(matches!(
                a(0),
                Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
            )),
            "intval" => Value::Int(to_i64(&a(0))),
            "floatval" | "doubleval" => Value::Float(to_f64(&a(0))),
            "strval" => Value::Str(to_bytes(&a(0))),
            "boolval" => Value::Bool(to_bool(&a(0))),
            "abs" => match to_num(&a(0)) {
                Num::Int(n) => Value::Int(n.abs()),
                Num::Float(f) => Value::Float(f.abs()),
            },
            "max" => self.extreme(&args, true),
            "min" => self.extreme(&args, false),
            "floor" => Value::Float(to_f64(&a(0)).floor()),
            "ceil" => Value::Float(to_f64(&a(0)).ceil()),
            "round" => {
                let p = to_i64(&a(1));
                let m = 10f64.powi(p as i32);
                Value::Float((to_f64(&a(0)) * m).round() / m)
            }
            "sqrt" => Value::Float(to_f64(&a(0)).sqrt()),
            "pow" => self.apply_bin(BinOp::Pow, &a(0), &a(1)),
            "intdiv" => {
                let d = to_i64(&a(1));
                if d == 0 {
                    return Err(self.throw_error("DivisionByZeroError", "Division by zero"));
                }
                Value::Int(to_i64(&a(0)) / d)
            }
            "strtoupper" => Value::Str(to_bytes(&a(0)).to_ascii_uppercase()),
            "strtolower" => Value::Str(to_bytes(&a(0)).to_ascii_lowercase()),
            "ucfirst" => {
                let mut b = to_bytes(&a(0));
                if let Some(c) = b.first_mut() {
                    c.make_ascii_uppercase();
                }
                Value::Str(b)
            }
            "lcfirst" => {
                let mut b = to_bytes(&a(0));
                if let Some(c) = b.first_mut() {
                    c.make_ascii_lowercase();
                }
                Value::Str(b)
            }
            "trim" => Value::Str(trim_bytes(&to_bytes(&a(0)), true, true)),
            "ltrim" => Value::Str(trim_bytes(&to_bytes(&a(0)), true, false)),
            "rtrim" | "chop" => Value::Str(trim_bytes(&to_bytes(&a(0)), false, true)),
            "str_repeat" => {
                let s = to_bytes(&a(0));
                let n = to_i64(&a(1)).max(0) as usize;
                if s.len().saturating_mul(n) > MAX_STR {
                    return Err(self.throw_error("ValueError", "str_repeat result too large"));
                }
                Value::Str(s.repeat(n))
            }
            "strrev" => {
                let mut b = to_bytes(&a(0));
                b.reverse();
                Value::Str(b)
            }
            "ord" => Value::Int(to_bytes(&a(0)).first().copied().unwrap_or(0) as i64),
            "chr" => Value::Str(vec![(to_i64(&a(0)).rem_euclid(256)) as u8]),
            "implode" | "join" => {
                // implode(sep, arr) or implode(arr)
                let (sep, arr) = match (&a(0), &a(1)) {
                    (Value::Array(arr), _) => (Vec::new(), arr.clone()),
                    (_, Value::Array(arr)) => (to_bytes(&a(0)), arr.clone()),
                    _ => (Vec::new(), Arr::new()),
                };
                let mut out = Vec::new();
                for (i, (_, v)) in arr.entries.iter().enumerate() {
                    if i > 0 {
                        out.extend_from_slice(&sep);
                    }
                    out.extend_from_slice(&to_bytes(v));
                }
                Value::Str(out)
            }
            "explode" => {
                let sep = to_bytes(&a(0));
                let s = to_bytes(&a(1));
                let mut arr = Arr::new();
                if sep.is_empty() {
                    arr.push(Value::Str(s));
                } else {
                    let mut start = 0;
                    let mut i = 0;
                    while i + sep.len() <= s.len() {
                        if &s[i..i + sep.len()] == sep.as_slice() {
                            arr.push(Value::Str(s[start..i].to_vec()));
                            i += sep.len();
                            start = i;
                        } else {
                            i += 1;
                        }
                    }
                    arr.push(Value::Str(s[start..].to_vec()));
                }
                Value::Array(arr)
            }
            "substr" => {
                let s = to_bytes(&a(0));
                let len = s.len() as i64;
                let mut start = to_i64(&a(1));
                if start < 0 {
                    start = (len + start).max(0);
                }
                let start = start.min(len) as usize;
                let end = if args.len() > 2 {
                    let l = to_i64(&a(2));
                    if l < 0 {
                        ((len + l).max(start as i64)) as usize
                    } else {
                        (start + l as usize).min(s.len())
                    }
                } else {
                    s.len()
                };
                Value::Str(s[start..end.max(start)].to_vec())
            }
            "str_replace" => {
                let search = to_bytes(&a(0));
                let replace = to_bytes(&a(1));
                let subject = to_bytes(&a(2));
                Value::Str(replace_bytes(&subject, &search, &replace))
            }
            "strpos" => {
                let hay = to_bytes(&a(0));
                let needle = to_bytes(&a(1));
                match find_bytes(&hay, &needle, to_i64(&a(2)).max(0) as usize) {
                    Some(i) => Value::Int(i as i64),
                    None => Value::Bool(false),
                }
            }
            "str_contains" => {
                Value::Bool(find_bytes(&to_bytes(&a(0)), &to_bytes(&a(1)), 0).is_some())
            }
            "str_starts_with" => Value::Bool(to_bytes(&a(0)).starts_with(&to_bytes(&a(1)))),
            "str_ends_with" => Value::Bool(to_bytes(&a(0)).ends_with(&to_bytes(&a(1)))),
            "array_keys" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, _) in arr.entries {
                        out.push(match k {
                            Key::Int(n) => Value::Int(n),
                            Key::Str(s) => Value::Str(s),
                        });
                    }
                }
                Value::Array(out)
            }
            "array_values" => {
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (_, v) in arr.entries {
                        out.push(v);
                    }
                }
                Value::Array(out)
            }
            "array_merge" => {
                let mut out = Arr::new();
                for v in &args {
                    if let Value::Array(arr) = v {
                        for (k, val) in &arr.entries {
                            match k {
                                Key::Int(_) => out.push(val.clone()),
                                Key::Str(_) => out.insert(k.clone(), val.clone()),
                            }
                        }
                    }
                }
                Value::Array(out)
            }
            "array_sum" => {
                let mut fi = 0i64;
                let mut ff = 0f64;
                let mut isf = false;
                if let Value::Array(arr) = a(0) {
                    for (_, v) in &arr.entries {
                        match to_num(v) {
                            Num::Int(n) if !isf => fi += n,
                            n => {
                                if !isf {
                                    ff = fi as f64;
                                    isf = true;
                                }
                                ff += n.as_f64();
                            }
                        }
                    }
                }
                if isf {
                    Value::Float(ff)
                } else {
                    Value::Int(fi)
                }
            }
            "in_array" => {
                let needle = a(0);
                let strict = to_bool(&a(2));
                let mut found = false;
                if let Value::Array(arr) = a(1) {
                    for (_, v) in &arr.entries {
                        if (strict && strict_eq(&needle, v)) || (!strict && loose_eq(&needle, v)) {
                            found = true;
                            break;
                        }
                    }
                }
                Value::Bool(found)
            }
            "call_user_func" => {
                let f = a(0);
                let rest = if args.len() > 1 { args[1..].to_vec() } else { vec![] };
                return self.call_value(f, rest);
            }
            "call_user_func_array" => {
                let f = a(0);
                let mut argv = Vec::new();
                if let Value::Array(arr) = a(1) {
                    for (_, v) in arr.entries {
                        argv.push(v);
                    }
                }
                return self.call_value(f, argv);
            }
            "array_map" => {
                let cb = a(0);
                let mut out = Arr::new();
                if let Value::Array(arr) = a(1) {
                    for (k, v) in arr.entries {
                        let r = if matches!(cb, Value::Null) {
                            v
                        } else {
                            self.call_value(cb.clone(), vec![v])?
                        };
                        out.insert(k, r);
                    }
                }
                Value::Array(out)
            }
            "array_filter" => {
                let cb = a(1);
                let mut out = Arr::new();
                if let Value::Array(arr) = a(0) {
                    for (k, v) in arr.entries {
                        let keep = if matches!(cb, Value::Null) {
                            to_bool(&v)
                        } else {
                            to_bool(&self.call_value(cb.clone(), vec![v.clone()])?)
                        };
                        if keep {
                            out.insert(k, v);
                        }
                    }
                }
                Value::Array(out)
            }
            "array_reduce" => {
                let cb = a(1);
                let mut acc = a(2);
                if let Value::Array(arr) = a(0) {
                    for (_, v) in arr.entries {
                        acc = self.call_value(cb.clone(), vec![acc, v])?;
                    }
                }
                acc
            }
            "is_callable" => Value::Bool(match a(0) {
                Value::Closure(_) => true,
                Value::Str(s) => {
                    let n = String::from_utf8_lossy(&s).to_ascii_lowercase();
                    self.funcs.contains_key(&n) || is_known_builtin(&n)
                }
                Value::Object(rc) => {
                    let c = rc.borrow().class.clone();
                    self.find_method(&c, "__invoke").is_some()
                }
                Value::Array(arr) => arr.len() == 2,
                _ => false,
            }),
            "range" => self.range(&a(0), &a(1), &a(2))?,
            "sprintf" => Value::Str(self.sprintf(&args)),
            "printf" => {
                let s = self.sprintf(&args);
                let n = s.len();
                self.out.extend_from_slice(&s);
                Value::Int(n as i64)
            }
            "define" => {
                if let Value::Str(n) = a(0) {
                    self.consts
                        .insert(String::from_utf8_lossy(&n).into_owned(), a(1));
                }
                Value::Bool(true)
            }
            "function_exists" => {
                let n = String::from_utf8_lossy(&to_bytes(&a(0))).to_ascii_lowercase();
                Value::Bool(self.funcs.contains_key(&n) || is_known_builtin(&n))
            }
            // ---- output buffering ----
            "ob_start" => {
                self.ob_stack.push(self.out.len());
                Value::Bool(true)
            }
            "ob_get_contents" => match self.ob_stack.last() {
                Some(&w) => Value::Str(self.out[w..].to_vec()),
                None => Value::Bool(false),
            },
            "ob_get_clean" => match self.ob_stack.pop() {
                Some(w) => {
                    let s = self.out[w..].to_vec();
                    self.out.truncate(w);
                    Value::Str(s)
                }
                None => Value::Bool(false),
            },
            "ob_end_clean" | "ob_clean" => match self.ob_stack.last().copied() {
                Some(w) => {
                    self.out.truncate(w);
                    if name == "ob_end_clean" {
                        self.ob_stack.pop();
                    }
                    Value::Bool(true)
                }
                None => Value::Bool(false),
            },
            "ob_end_flush" | "ob_flush" | "flush" => {
                if name == "ob_end_flush" {
                    self.ob_stack.pop();
                }
                Value::Bool(true)
            }
            "ob_get_level" => Value::Int(self.ob_stack.len() as i64),
            "ob_get_length" => match self.ob_stack.last() {
                Some(&w) => Value::Int((self.out.len() - w) as i64),
                None => Value::Bool(false),
            },
            // ---- path / filename helpers ----
            "basename" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let s = s.trim_end_matches(['/', '\\']);
                let mut base = s.rsplit(['/', '\\']).next().unwrap_or("").to_string();
                if args.len() > 1 {
                    let suf = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                    if base.len() > suf.len() && base.ends_with(&suf) {
                        base.truncate(base.len() - suf.len());
                    }
                }
                Value::Str(base.into_bytes())
            }
            "dirname" => {
                let s = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let s = s.trim_end_matches(['/', '\\']);
                match s.rfind(['/', '\\']) {
                    Some(0) => Value::Str(b"/".to_vec()),
                    Some(i) => Value::Str(s[..i].as_bytes().to_vec()),
                    None => Value::Str(b".".to_vec()),
                }
            }
            // ---- class / object introspection ----
            "get_class" => match a(0) {
                Value::Object(rc) => Value::Str(rc.borrow().class.clone().into_bytes()),
                _ => match &self.current_class {
                    Some(c) => Value::Str(c.clone().into_bytes()),
                    None => Value::Bool(false),
                },
            },
            "get_parent_class" => {
                let cname = match a(0) {
                    Value::Object(rc) => Some(rc.borrow().class.clone()),
                    Value::Str(s) => Some(String::from_utf8_lossy(&s).into_owned()),
                    _ => self.current_class.clone(),
                };
                match cname.and_then(|c| self.find_class(&c)).and_then(|d| d.parent.clone()) {
                    Some(p) => Value::Str(p.last().as_bytes().to_vec()),
                    None => Value::Bool(false),
                }
            }
            "class_exists" | "interface_exists" | "trait_exists" | "enum_exists" => {
                let n = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(self.find_class(&n).is_some())
            }
            "get_object_vars" => {
                let mut o = Arr::new();
                if let Value::Object(rc) = a(0) {
                    for (k, v) in &rc.borrow().props {
                        o.insert(Key::Str(k.as_bytes().to_vec()), v.clone());
                    }
                }
                Value::Array(o)
            }
            "method_exists" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let m = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                Value::Bool(self.find_method(&cn, &m).is_some())
            }
            "property_exists" => {
                let p = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let in_obj = matches!(&a(0), Value::Object(rc) if rc.borrow().get(&p).is_some());
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let in_class = self.ancestry(&cn).iter().any(|c| c.props.iter().any(|pd| pd.name == p));
                Value::Bool(in_obj || in_class)
            }
            "is_a" | "is_subclass_of" => {
                let cn = match a(0) {
                    Value::Object(rc) => rc.borrow().class.clone(),
                    v => String::from_utf8_lossy(&to_bytes(&v)).into_owned(),
                };
                let t = String::from_utf8_lossy(&to_bytes(&a(1))).into_owned();
                let sub = self.is_subclass(&cn, &t);
                Value::Bool(if name == "is_subclass_of" {
                    sub && !cn.eq_ignore_ascii_case(&t)
                } else {
                    sub
                })
            }
            // ---- filesystem ----
            "file_get_contents" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => Value::Str(b),
                    Err(_) => Value::Bool(false),
                }
            }
            "file_put_contents" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let bytes = match &a(1) {
                    Value::Array(arr) => {
                        let mut v = Vec::new();
                        for (_, e) in &arr.entries {
                            v.extend_from_slice(&to_bytes(e));
                        }
                        v
                    }
                    v => to_bytes(v),
                };
                // FILE_APPEND = 8
                let append = to_i64(&a(2)) & 8 != 0;
                let res = if append {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .and_then(|mut f| f.write_all(&bytes))
                } else {
                    std::fs::write(&path, &bytes)
                };
                match res {
                    Ok(_) => Value::Int(bytes.len() as i64),
                    Err(_) => Value::Bool(false),
                }
            }
            "file" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => {
                        let ignore_nl = to_i64(&a(1)) & 2 != 0;
                        let mut arr = Arr::new();
                        let mut start = 0;
                        for i in 0..b.len() {
                            if b[i] == b'\n' {
                                let end = if ignore_nl { i } else { i + 1 };
                                arr.push(Value::Str(b[start..end].to_vec()));
                                start = i + 1;
                            }
                        }
                        if start < b.len() {
                            arr.push(Value::Str(b[start..].to_vec()));
                        }
                        Value::Array(arr)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "readfile" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read(&path) {
                    Ok(b) => {
                        let n = b.len();
                        self.out.extend_from_slice(&b);
                        Value::Int(n as i64)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "unlink" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(std::fs::remove_file(&path).is_ok())
            }
            "mkdir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let recursive = to_bool(&a(2));
                let r = if recursive {
                    std::fs::create_dir_all(&path)
                } else {
                    std::fs::create_dir(&path)
                };
                Value::Bool(r.is_ok())
            }
            "rmdir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(std::fs::remove_dir(&path).is_ok())
            }
            "rename" => Value::Bool(
                std::fs::rename(
                    String::from_utf8_lossy(&to_bytes(&a(0))).as_ref(),
                    String::from_utf8_lossy(&to_bytes(&a(1))).as_ref(),
                )
                .is_ok(),
            ),
            "copy" => Value::Bool(
                std::fs::copy(
                    String::from_utf8_lossy(&to_bytes(&a(0))).as_ref(),
                    String::from_utf8_lossy(&to_bytes(&a(1))).as_ref(),
                )
                .is_ok(),
            ),
            "file_exists" => {
                Value::Bool(std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).exists())
            }
            "is_file" => Value::Bool(
                std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).is_file(),
            ),
            "is_dir" => Value::Bool(
                std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).is_dir(),
            ),
            "is_readable" | "is_writable" | "is_writeable" => {
                Value::Bool(std::path::Path::new(&String::from_utf8_lossy(&to_bytes(&a(0))).into_owned()).exists())
            }
            "filesize" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::metadata(&path) {
                    Ok(m) => Value::Int(m.len() as i64),
                    Err(_) => Value::Bool(false),
                }
            }
            "scandir" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::read_dir(&path) {
                    Ok(rd) => {
                        let mut names: Vec<Vec<u8>> = vec![b".".to_vec(), b"..".to_vec()];
                        for e in rd.flatten() {
                            names.push(e.file_name().to_string_lossy().as_bytes().to_vec());
                        }
                        names.sort();
                        let mut arr = Arr::new();
                        for n in names {
                            arr.push(Value::Str(n));
                        }
                        Value::Array(arr)
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "realpath" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                match std::fs::canonicalize(&path) {
                    Ok(p) => {
                        let s = p.to_string_lossy();
                        // strip Windows \\?\ prefix
                        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
                        Value::Str(s.as_bytes().to_vec())
                    }
                    Err(_) => Value::Bool(false),
                }
            }
            "getcwd" => match std::env::current_dir() {
                Ok(p) => Value::Str(p.to_string_lossy().as_bytes().to_vec()),
                Err(_) => Value::Bool(false),
            },
            "sys_get_temp_dir" => {
                Value::Str(std::env::temp_dir().to_string_lossy().as_bytes().to_vec())
            }
            "touch" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                Value::Bool(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .write(true)
                        .open(&path)
                        .is_ok(),
                )
            }
            "pathinfo" => {
                let path = String::from_utf8_lossy(&to_bytes(&a(0))).into_owned();
                let p = std::path::Path::new(&path);
                let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| ".".into());
                let base = p.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = p.extension().map(|e| e.to_string_lossy().into_owned());
                let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let mut arr = Arr::new();
                arr.insert(Key::Str(b"dirname".to_vec()), Value::Str(dir.into_bytes()));
                arr.insert(Key::Str(b"basename".to_vec()), Value::Str(base.into_bytes()));
                if let Some(e) = ext {
                    arr.insert(Key::Str(b"extension".to_vec()), Value::Str(e.into_bytes()));
                }
                arr.insert(Key::Str(b"filename".to_vec()), Value::Str(stem.into_bytes()));
                Value::Array(arr)
            }
            // ---- environment / config stubs (setup calls; sane defaults) ----
            "getenv" => {
                if args.is_empty() {
                    Value::Array(Arr::new())
                } else {
                    match std::env::var(String::from_utf8_lossy(&to_bytes(&a(0))).as_ref()) {
                        Ok(v) => Value::Str(v.into_bytes()),
                        Err(_) => Value::Bool(false),
                    }
                }
            }
            "putenv" | "set_time_limit" | "ignore_user_abort" | "setlocale" | "extension_loaded" => {
                Value::Bool(false)
            }
            "ini_get" | "ini_set" => Value::Bool(false),
            "error_reporting" => Value::Int(0),
            "set_error_handler" | "restore_error_handler" | "set_exception_handler"
            | "restore_exception_handler" | "error_clear_last" | "debug_print_backtrace"
            | "gc_enable" | "gc_disable" | "header" | "clearstatcache" | "usleep" | "sleep" => {
                Value::Null
            }
            "trigger_error" | "spl_autoload_register" | "spl_autoload_unregister"
            | "date_default_timezone_set" | "assert" | "gc_enabled" | "headers_sent" => {
                Value::Bool(true)
            }
            "date_default_timezone_get" => Value::Str(b"UTC".to_vec()),
            "debug_backtrace" => Value::Array(Arr::new()),
            "gc_collect_cycles" | "http_response_code" | "getmypid" | "hrtime" => Value::Int(0),
            "memory_get_usage" | "memory_get_peak_usage" => Value::Int(2_000_000),
            "php_sapi_name" => Value::Str(b"cli".to_vec()),
            "phpversion" => Value::Str(b"8.3.0".to_vec()),
            "php_uname" => Value::Str(b"Linux".to_vec()),
            "error_get_last" => Value::Null,
            _ => return Err(RunError(format!("unknown function {name}()"))),
        })
    }

    fn extreme(&self, args: &[Value], want_max: bool) -> Value {
        let items: Vec<Value> = if args.len() == 1 {
            if let Value::Array(a) = &args[0] {
                a.entries.iter().map(|(_, v)| v.clone()).collect()
            } else {
                vec![args[0].clone()]
            }
        } else {
            args.to_vec()
        };
        let mut best: Option<Value> = None;
        for v in items {
            best = Some(match best {
                None => v,
                Some(b) => {
                    let take = if want_max {
                        compare(&v, &b) == std::cmp::Ordering::Greater
                    } else {
                        compare(&v, &b) == std::cmp::Ordering::Less
                    };
                    if take {
                        v
                    } else {
                        b
                    }
                }
            });
        }
        best.unwrap_or(Value::Null)
    }

    fn range(&mut self, start: &Value, end: &Value, step: &Value) -> R<Value> {
        let mut arr = Arr::new();
        // integer range when both bounds are ints and the step is whole — iterate
        // in integer (i128) arithmetic. Doing this in f64 breaks near i64::MIN/MAX
        // (the ulp exceeds 1, so `x += 1.0` is a no-op → infinite loop).
        let step_whole = matches!(step, Value::Null) || to_f64(step).fract() == 0.0;
        if matches!(start, Value::Int(_)) && matches!(end, Value::Int(_)) && step_whole {
            let a = to_i64(start) as i128;
            let b = to_i64(end) as i128;
            let st = if matches!(step, Value::Null) {
                1i128
            } else {
                (to_i64(step).unsigned_abs() as i128).max(1)
            };
            let count = (a - b).abs() / st;
            if count as usize > MAX_RANGE {
                return Ok(Value::Array(arr)); // memory-bomb guard
            }
            let mut x = a;
            if a <= b {
                while x <= b {
                    arr.push(Value::Int(x as i64));
                    x += st;
                }
            } else {
                while x >= b {
                    arr.push(Value::Int(x as i64));
                    x -= st;
                }
            }
            return Ok(Value::Array(arr));
        }
        // float range — iterate by INDEX, never by accumulating `x` (near i64
        // magnitudes the ulp exceeds the step, so `x += st` would be a no-op and
        // loop forever). Compute the element count, cap it BEFORE looping.
        let st = if matches!(step, Value::Null) { 1.0 } else { to_f64(step).abs().max(1e-9) };
        let (a, b) = (to_f64(start), to_f64(end));
        if !a.is_finite() || !b.is_finite() || !st.is_finite() {
            return Err(self.throw_error("ValueError", "range(): Arguments must be finite"));
        }
        let count_f = ((b - a).abs() / st).floor();
        if !count_f.is_finite() || count_f > MAX_RANGE as f64 {
            return Ok(Value::Array(arr)); // memory-bomb guard
        }
        let n = count_f as usize;
        for i in 0..=n {
            let x = if a <= b { a + i as f64 * st } else { a - i as f64 * st };
            arr.push(Value::Float(x));
        }
        Ok(Value::Array(arr))
    }

    fn sprintf(&self, args: &[Value]) -> Vec<u8> {
        let fmt = to_bytes(args.get(0).unwrap_or(&Value::Null));
        let mut out = Vec::new();
        let mut ai = 1;
        let mut i = 0;
        while i < fmt.len() {
            if fmt[i] != b'%' {
                out.push(fmt[i]);
                i += 1;
                continue;
            }
            i += 1;
            if i >= fmt.len() {
                break;
            }
            if fmt[i] == b'%' {
                out.push(b'%');
                i += 1;
                continue;
            }
            // collect flags/width/precision until a conversion char
            let spec_start = i;
            while i < fmt.len() && !fmt[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i >= fmt.len() {
                break;
            }
            let conv = fmt[i];
            let spec = String::from_utf8_lossy(&fmt[spec_start..i]).into_owned();
            i += 1;
            let arg = args.get(ai).cloned().unwrap_or(Value::Null);
            ai += 1;
            let piece = format_spec(conv, &spec, &arg);
            out.extend_from_slice(&piece);
        }
        out
    }
}

fn is_known_builtin(n: &str) -> bool {
    matches!(
        n,
        "strlen" | "count" | "var_dump" | "print_r" | "implode" | "explode" | "sprintf"
            | "printf" | "in_array" | "array_keys" | "array_values" | "array_merge" | "range"
    )
}

fn trim_bytes(s: &[u8], left: bool, right: bool) -> Vec<u8> {
    let ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b);
    let mut start = 0;
    let mut end = s.len();
    if left {
        while start < end && ws(s[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && ws(s[end - 1]) {
            end -= 1;
        }
    }
    s[start..end].to_vec()
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    if from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn replace_bytes(subject: &[u8], search: &[u8], replace: &[u8]) -> Vec<u8> {
    if search.is_empty() {
        return subject.to_vec();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < subject.len() {
        if i + search.len() <= subject.len() && &subject[i..i + search.len()] == search {
            out.extend_from_slice(replace);
            i += search.len();
        } else {
            out.push(subject[i]);
            i += 1;
        }
    }
    out
}

fn format_spec(conv: u8, spec: &str, arg: &Value) -> Vec<u8> {
    // parse: flags ([-0 +]) then width then .precision
    let mut chars = spec.chars().peekable();
    let mut left = false;
    let mut zero = false;
    let mut plus = false;
    while let Some(&c) = chars.peek() {
        match c {
            '-' => left = true,
            '0' => zero = true,
            '+' => plus = true,
            ' ' => {}
            _ => break,
        }
        chars.next();
    }
    let mut width = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            width.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let width: usize = width.parse().unwrap_or(0);
    let mut prec: Option<usize> = None;
    if chars.peek() == Some(&'.') {
        chars.next();
        let mut p = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() {
                p.push(c);
                chars.next();
            } else {
                break;
            }
        }
        prec = Some(p.parse().unwrap_or(0));
    }
    let body: Vec<u8> = match conv {
        b'd' | b'i' => {
            let n = to_i64(arg);
            let mut s = n.abs().to_string();
            if n < 0 {
                s = format!("-{s}");
            } else if plus {
                s = format!("+{s}");
            }
            s.into_bytes()
        }
        b'u' => (to_i64(arg) as u64).to_string().into_bytes(),
        b'f' | b'F' => {
            let p = prec.unwrap_or(6);
            format!("{:.*}", p, to_f64(arg)).into_bytes()
        }
        b's' => {
            let mut b = to_bytes(arg);
            if let Some(p) = prec {
                b.truncate(p);
            }
            b
        }
        b'x' => format!("{:x}", to_i64(arg)).into_bytes(),
        b'X' => format!("{:X}", to_i64(arg)).into_bytes(),
        b'o' => format!("{:o}", to_i64(arg)).into_bytes(),
        b'b' => format!("{:b}", to_i64(arg)).into_bytes(),
        b'c' => vec![to_i64(arg) as u8],
        b'e' => format!("{:e}", to_f64(arg)).into_bytes(),
        _ => Vec::new(),
    };
    if body.len() >= width {
        return body;
    }
    let pad = width - body.len();
    let padch = if zero && !left { b'0' } else { b' ' };
    let mut out = Vec::with_capacity(width);
    if left {
        out.extend_from_slice(&body);
        out.extend(std::iter::repeat(b' ').take(pad));
    } else {
        out.extend(std::iter::repeat(padch).take(pad));
        out.extend_from_slice(&body);
    }
    out
}

// ---- var_dump / print_r formatting -------------------------------------
fn var_dump(v: &Value, indent: usize, out: &mut String) {
    var_dump_seen(v, indent, out, &mut Vec::new());
}

/// `seen` holds the Rc addresses of objects on the current path, so circular
/// object graphs print `*RECURSION*` instead of recursing into a memory bomb.
fn var_dump_seen(v: &Value, indent: usize, out: &mut String, seen: &mut Vec<usize>) {
    // Depth cap: deeply nested structures otherwise produce quadratic-sized output
    // (indentation grows per level), a memory bomb on e.g. a 50k-deep object chain.
    if indent > 256 {
        out.push_str(&format!("{}*MAX DEPTH*\n", "  ".repeat(indent)));
        return;
    }
    let pad = "  ".repeat(indent);
    match v {
        Value::Null => out.push_str(&format!("{pad}NULL\n")),
        Value::Bool(b) => out.push_str(&format!("{pad}bool({})\n", if *b { "true" } else { "false" })),
        Value::Int(n) => out.push_str(&format!("{pad}int({n})\n")),
        Value::Float(f) => out.push_str(&format!("{pad}float({})\n", format_float(*f))),
        Value::Str(s) => out.push_str(&format!(
            "{pad}string({}) \"{}\"\n",
            s.len(),
            String::from_utf8_lossy(s)
        )),
        Value::Array(a) => {
            out.push_str(&format!("{pad}array({}) {{\n", a.len()));
            for (k, val) in &a.entries {
                match k {
                    Key::Int(n) => out.push_str(&format!("{pad}  [{n}]=>\n")),
                    Key::Str(s) => {
                        out.push_str(&format!("{pad}  [\"{}\"]=>\n", String::from_utf8_lossy(s)))
                    }
                }
                var_dump_seen(val, indent + 1, out, seen);
            }
            out.push_str(&format!("{pad}}}\n"));
        }
        Value::Object(o) => {
            let id = Rc::as_ptr(o) as *const () as usize;
            if seen.contains(&id) {
                out.push_str(&format!("{pad}*RECURSION*\n"));
                return;
            }
            let ob = o.borrow();
            out.push_str(&format!("{pad}object({})#1 ({}) {{\n", ob.class, ob.props.len()));
            seen.push(id);
            for (k, val) in &ob.props {
                out.push_str(&format!("{pad}  [\"{k}\"]=>\n"));
                var_dump_seen(val, indent + 1, out, seen);
            }
            seen.pop();
            out.push_str(&format!("{pad}}}\n"));
        }
        Value::Closure(_) => out.push_str(&format!("{pad}object(Closure)#1 (0) {{\n{pad}}}\n")),
    }
}

fn print_r(v: &Value, indent: usize, out: &mut String) {
    if indent > 256 {
        out.push_str("*MAX DEPTH*");
        return;
    }
    match v {
        Value::Array(a) => {
            let pad = "    ".repeat(indent);
            out.push_str("Array\n");
            out.push_str(&format!("{pad}(\n"));
            for (k, val) in &a.entries {
                let ks = match k {
                    Key::Int(n) => n.to_string(),
                    Key::Str(s) => String::from_utf8_lossy(s).into_owned(),
                };
                out.push_str(&format!("{pad}    [{ks}] => "));
                print_r(val, indent + 2, out);
                out.push('\n');
            }
            out.push_str(&format!("{pad})\n"));
        }
        other => out.push_str(&String::from_utf8_lossy(&to_bytes(other))),
    }
}

// ---- helpers -----------------------------------------------------------

/// Count nodes in a value (arrays only — the explosion vector), iteratively and
/// bounded by `limit`. Guards against pathological array growth, e.g. value-copy
/// fallback for `$a[$i] =& $a` (references not yet modeled) blowing up
/// exponentially. Iterative so it can't itself overflow the stack.
fn value_size(v: &Value, limit: usize) -> usize {
    let mut count = 0usize;
    let mut stack: Vec<&Value> = vec![v];
    while let Some(cur) = stack.pop() {
        count += 1;
        if count > limit {
            return count;
        }
        if let Value::Array(a) = cur {
            for (_, e) in &a.entries {
                stack.push(e);
            }
        }
    }
    count
}

/// The predefined PHP constants commonly used in the corpus.
fn php_const(n: &str) -> Option<Value> {
    use Value::*;
    let sep = if cfg!(windows) { "\\" } else { "/" };
    Some(match n {
        "PHP_EOL" => Str(b"\n".to_vec()),
        "PHP_INT_MAX" => Int(i64::MAX),
        "PHP_INT_MIN" => Int(i64::MIN),
        "PHP_INT_SIZE" => Int(8),
        "PHP_FLOAT_EPSILON" => Float(f64::EPSILON),
        "PHP_FLOAT_MAX" => Float(f64::MAX),
        "PHP_FLOAT_MIN" => Float(f64::MIN_POSITIVE),
        "PHP_FLOAT_DIG" => Int(15),
        "PHP_VERSION" => Str(b"8.3.0".to_vec()),
        "PHP_MAJOR_VERSION" => Int(8),
        "PHP_MINOR_VERSION" => Int(3),
        "PHP_RELEASE_VERSION" => Int(0),
        "PHP_VERSION_ID" => Int(80300),
        "PHP_OS" => Str(if cfg!(windows) { b"WINNT".to_vec() } else { b"Linux".to_vec() }),
        "PHP_OS_FAMILY" => Str(if cfg!(windows) { b"Windows".to_vec() } else { b"Linux".to_vec() }),
        "DIRECTORY_SEPARATOR" => Str(sep.as_bytes().to_vec()),
        "PATH_SEPARATOR" => Str(if cfg!(windows) { b";".to_vec() } else { b":".to_vec() }),
        "NULL" | "null" => Null,
        "TRUE" | "true" => Bool(true),
        "FALSE" | "false" => Bool(false),
        // math
        "M_PI" => Float(std::f64::consts::PI),
        "M_E" => Float(std::f64::consts::E),
        "M_SQRT2" => Float(std::f64::consts::SQRT_2),
        "M_SQRT1_2" => Float(std::f64::consts::FRAC_1_SQRT_2),
        "M_SQRT3" => Float(1.7320508075688772),
        "M_LN2" => Float(std::f64::consts::LN_2),
        "M_LN10" => Float(std::f64::consts::LN_10),
        "M_LOG2E" => Float(std::f64::consts::LOG2_E),
        "M_LOG10E" => Float(std::f64::consts::LOG10_E),
        "M_PI_2" => Float(std::f64::consts::FRAC_PI_2),
        "M_PI_4" => Float(std::f64::consts::FRAC_PI_4),
        "M_2_PI" => Float(std::f64::consts::FRAC_2_PI),
        "M_1_PI" => Float(std::f64::consts::FRAC_1_PI),
        "M_EULER" => Float(0.5772156649015329),
        "INF" => Float(f64::INFINITY),
        "NAN" => Float(f64::NAN),
        // error levels
        "E_ERROR" => Int(1),
        "E_WARNING" => Int(2),
        "E_PARSE" => Int(4),
        "E_NOTICE" => Int(8),
        "E_CORE_ERROR" => Int(16),
        "E_CORE_WARNING" => Int(32),
        "E_COMPILE_ERROR" => Int(64),
        "E_COMPILE_WARNING" => Int(128),
        "E_USER_ERROR" => Int(256),
        "E_USER_WARNING" => Int(512),
        "E_USER_NOTICE" => Int(1024),
        "E_STRICT" => Int(2048),
        "E_RECOVERABLE_ERROR" => Int(4096),
        "E_DEPRECATED" => Int(8192),
        "E_USER_DEPRECATED" => Int(16384),
        "E_ALL" => Int(32767),
        // sort flags
        "SORT_REGULAR" => Int(0),
        "SORT_NUMERIC" => Int(1),
        "SORT_STRING" => Int(2),
        "SORT_DESC" => Int(3),
        "SORT_ASC" => Int(4),
        "SORT_LOCALE_STRING" => Int(5),
        "SORT_NATURAL" => Int(6),
        "SORT_FLAG_CASE" => Int(8),
        // count / str_pad / array_filter
        "COUNT_NORMAL" => Int(0),
        "COUNT_RECURSIVE" => Int(1),
        "STR_PAD_RIGHT" => Int(1),
        "STR_PAD_LEFT" => Int(0),
        "STR_PAD_BOTH" => Int(2),
        "ARRAY_FILTER_USE_KEY" => Int(2),
        "ARRAY_FILTER_USE_BOTH" => Int(1),
        // file flags
        "FILE_USE_INCLUDE_PATH" => Int(1),
        "FILE_IGNORE_NEW_LINES" => Int(2),
        "FILE_SKIP_EMPTY_LINES" => Int(4),
        "FILE_APPEND" => Int(8),
        "FILE_NO_DEFAULT_CONTEXT" => Int(16),
        "FILE_TEXT" => Int(0),
        "FILE_BINARY" => Int(0),
        "SEEK_SET" => Int(0),
        "SEEK_CUR" => Int(1),
        "SEEK_END" => Int(2),
        "LOCK_SH" => Int(1),
        "LOCK_EX" => Int(2),
        "LOCK_UN" => Int(3),
        // htmlspecialchars / ent
        "ENT_QUOTES" => Int(3),
        "ENT_COMPAT" => Int(2),
        "ENT_NOQUOTES" => Int(0),
        "ENT_HTML401" => Int(0),
        "ENT_HTML5" => Int(48),
        // json
        "JSON_PRETTY_PRINT" => Int(128),
        "JSON_UNESCAPED_SLASHES" => Int(64),
        "JSON_UNESCAPED_UNICODE" => Int(256),
        "JSON_THROW_ON_ERROR" => Int(4194304),
        "JSON_ERROR_NONE" => Int(0),
        // preg
        "PREG_PATTERN_ORDER" => Int(1),
        "PREG_SET_ORDER" => Int(2),
        "PREG_OFFSET_CAPTURE" => Int(256),
        "PREG_SPLIT_NO_EMPTY" => Int(1),
        "PREG_SPLIT_DELIM_CAPTURE" => Int(2),
        // misc
        "PHP_ROUND_HALF_UP" => Int(1),
        "PHP_ROUND_HALF_DOWN" => Int(2),
        "PHP_ROUND_HALF_EVEN" => Int(3),
        "PHP_ROUND_HALF_ODD" => Int(4),
        "ENT_SUBSTITUTE" => Int(8),
        _ => return None,
    })
}

fn string_char(s: &[u8], k: &Key) -> Value {
    let i = match k {
        Key::Int(n) => *n,
        Key::Str(b) => leading_number(b).as_i64(),
    };
    let i = if i < 0 { s.len() as i64 + i } else { i };
    if i >= 0 && (i as usize) < s.len() {
        Value::Str(vec![s[i as usize]])
    } else {
        Value::Str(Vec::new())
    }
}

fn inc(v: &Value, by: i64) -> Value {
    match v {
        Value::Int(n) => Value::Int(n + by),
        Value::Float(f) => Value::Float(f + by as f64),
        Value::Null if by > 0 => Value::Int(1),
        Value::Null => Value::Null, // PHP: --$null stays null
        _ => match to_num(v) {
            Num::Int(n) => Value::Int(n + by),
            Num::Float(f) => Value::Float(f + by as f64),
        },
    }
}

fn num_arith(l: &Value, r: &Value, fi: fn(i64, i64) -> i64, ff: fn(f64, f64) -> f64) -> Value {
    match (to_num(l), to_num(r)) {
        (Num::Int(a), Num::Int(b)) => {
            // detect overflow for + - * by checking against float
            let res = fi(a, b);
            Value::Int(res)
        }
        (a, b) => Value::Float(ff(a.as_f64(), b.as_f64())),
    }
}
