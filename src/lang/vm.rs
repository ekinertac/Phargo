//! Path C phase 1: a bytecode compiler for the hot subset of PHP.
//!
//! The strategy is MIXED-MODE execution: function bodies (and the top-level
//! program) compile to a stack-machine `Chunk` when every construct they use
//! is in the supported subset; anything else returns `None` and the
//! tree-walker runs that body exactly as before. Correctness comes from
//! sharing semantic kernels with the walker (`apply_bin`, `to_bool`,
//! `stringify`, the builtin table) — the VM changes *dispatch*, never
//! semantics.
//!
//! What the subset buys (and why): locals become compile-time slot indices
//! (no per-access HashMap lookups), literals become constant-pool loads (no
//! per-iteration allocation), control flow becomes jumps (no recursive
//! enum matching). Those three are the walker's measured hot costs on the
//! bench.rs micro suite.
//!
//! The compiler BAILS (returns None) on anything with reference semantics,
//! scope introspection, or dynamic dispatch it can't prove safe: `&`
//! anywhere, `global`/`static`, closures, method/static calls, property
//! access, VarVar, try/catch, switch, match, generators, and calls to
//! builtins with by-ref out-params or scope access (compact/extract/...).
//! The interpreter lives in eval.rs (`Eval::run_chunk`) because it needs the
//! evaluator's state; this module is pure data + compilation.

use super::ast::*;
use std::collections::HashMap;

thread_local! {
    /// Why the most recent compilation bailed (diagnostics only; read by
    /// the evaluator's PHARGO_VM_DEBUG census).
    pub static LAST_BAIL: std::cell::RefCell<&'static str> = const { std::cell::RefCell::new("") };
}

fn bail<T>(reason: &'static str) -> Option<T> {
    LAST_BAIL.with(|b| *b.borrow_mut() = reason);
    None
}

fn stmt_name(s: &Stmt) -> &'static str {
    match s {
        Stmt::Global(_) => "stmt:global",
        Stmt::StaticVar(_) => "stmt:static-var",
        Stmt::Try { .. } => "stmt:try",
        Stmt::Throw(_) => "stmt:throw",
        Stmt::Unset(_) => "stmt:unset",
        Stmt::Func(_) => "stmt:nested-func",
        Stmt::Class(_) => "stmt:nested-class",
        Stmt::Namespace { .. } => "stmt:namespace",
        Stmt::Use(_) => "stmt:use",
        Stmt::ConstDecl(_) => "stmt:const",
        _ => "stmt:other",
    }
}

fn expr_name(e: &Expr) -> &'static str {
    match e {
        Expr::Closure(_) => "expr:closure",
        Expr::ArrowFn(_) => "expr:arrow-fn",
        Expr::Prop(..) => "expr:prop-non-this",
        Expr::MethodCall(..) => "expr:method-call",
        Expr::StaticCall(..) => "expr:static-call",
        Expr::StaticProp(..) => "expr:static-prop",
        Expr::New(..) => "expr:new",
        Expr::NewAnon(..) => "expr:new-anon",
        Expr::Cast(..) => "expr:cast",
        Expr::InstanceOf(..) => "expr:instanceof",
        Expr::Match(..) => "expr:match",
        Expr::VarVar(_) => "expr:varvar",
        Expr::AssignRef(..) => "expr:assign-ref",
        Expr::Assign(..) => "expr:assign-shape",
        Expr::AssignOp(..) => "expr:assign-op-shape",
        Expr::Index(..) => "expr:index-shape",
        Expr::Isset(_) => "expr:isset-shape",
        Expr::Empty(_) => "expr:empty-shape",
        Expr::Call(..) => "expr:call-shape",
        Expr::ConstFetch(_) => "expr:const-fetch",
        Expr::MagicConst(_) => "expr:magic-const",
        Expr::Ternary(..) => "expr:ternary",
        Expr::List(_) => "expr:list",
        Expr::Array(_) => "expr:array-shape",
        Expr::ClassConst(..) => "expr:class-const-shape",
        Expr::Unary(..) => "expr:unary-shape",
        _ => "expr:other",
    }
}

#[derive(Debug, Clone)]
pub enum Op {
    /// Push consts[i].
    Const(u16),
    /// Push a clone of slot i, warning once if it was never assigned
    /// (mirrors the walker's undefined-variable warning).
    Load(u16),
    /// Push slot i without the undefined warning (isset/empty/?? sites).
    LoadQuiet(u16),
    /// Pop into slot i.
    Store(u16),
    Dup,
    Pop,
    /// Binary op via Eval::apply_bin — identical semantics to the walker.
    Bin(BinOp),
    /// Logical/unary helpers.
    Not,
    Neg,
    IsNull,
    /// isset($x): slot assigned AND not null → bool.
    IssetSlot(u16),
    UnsetSlot(u16),
    /// Unconditional / conditional jumps (absolute op index). Conditionals pop.
    Jmp(u32),
    JmpIfFalse(u32),
    JmpIfTrue(u32),
    /// Call a named function (user or builtin) with argc stack args
    /// (pushed left-to-right). Name resolved via names[i]; `site` indexes
    /// the chunk's per-call-site memo.
    CallFn { name: u16, argc: u8, site: u16 },
    /// Echo the top n stack values (in push order).
    Echo(u8),
    /// $slot[key]: pop key, push element (Null + no warning if absent — the
    /// compiler only emits this in quiet positions; checked reads use
    /// LoadIndexChecked).
    LoadIndexQuiet(u16),
    /// Checked variant: warns on missing key like the walker.
    LoadIndex(u16),
    /// $slot[key] = v: pop v, pop key, insert into the slot's array in place
    /// (creating the array if the slot is null/unset).
    StoreIndex(u16),
    /// $slot[] = v: pop v, append in place.
    Append(u16),
    /// Casts (subset).
    CastInt,
    CastFloat,
    CastString,
    CastBool,
    /// foreach ($slot as [$k =>] $v) over a snapshot: pushes an iterator.
    /// Jump target = loop end (when exhausted).
    IterInit { slot: u16, end: u32 },
    /// Advance: bind next key/value into slots, or jump to end and pop the
    /// iterator. key_slot == u16::MAX means "no key binding".
    IterNext { val_slot: u16, key_slot: u16, end: u32 },
    /// Pop the innermost iterator (break out of foreach).
    PopIter,
    /// Set current line (error attribution).
    Line(u32),
    /// Return top of stack / null.
    Ret,
    RetNull,
    /// Runtime constant lookup (names[i]): TRUE/FALSE/etc handled as
    /// literals at compile time; this covers user constants.
    ConstLookup(u16),
    /// $slot .= (pop): append bytes in place — the walker's O(n) `.=` fast
    /// path, kept (a Load+Concat+Store round trip clones the whole string).
    ConcatAssign(u16),
    /// Push $this (the receiver object; Null in static contexts).
    LoadThis,
    /// $this->prop read/write (names[i]); missing props read Null silently,
    /// writes run the same typed-property checks as the walker.
    LoadThisProp(u16),
    StoreThisProp(u16),
    /// $this->prop[key] element ops, mutating the property array IN PLACE
    /// (the 48x WordPress lesson: never clone a container on indexed write).
    LoadIndexThisProp(u16),
    StoreIndexThisProp(u16),
    AppendThisProp(u16),
    /// Array literal construction, streaming: NewArr pushes an empty array;
    /// ArrPush appends the popped value to the stack-top array; ArrSet pops
    /// value then key and inserts.
    NewArr,
    ArrPush,
    ArrSet,
    /// isset variants beyond plain slots. IssetIndex pops the key;
    /// IssetThisPropIndex pops the key and checks the property element.
    IssetIndex(u16),
    IssetThisProp(u16),
    IssetThisPropIndex(u16),
    /// new ClassName(args): names[i] resolved through the same namespace
    /// machinery as the walker, then Eval::instantiate.
    NewObj { class: u16, argc: u8 },
    /// ClassName::method(args) — resolution mirrors resolve_class_name for
    /// self/parent/static keywords. Non-lvalue args only (same by-ref rule
    /// as CallMethod).
    CallStatic { class: u16, method: u16, argc: u8 },
    /// Chained index paths: keys are pushed left-to-right; navigation is
    /// in place (never clones intermediate containers — the 48x rule).
    /// depth = number of keys on the stack.
    LoadPath { slot: u16, depth: u8 },
    StorePath { slot: u16, depth: u8 },
    AppendPath { slot: u16, depth: u8 },
    LoadPathThisProp { name: u16, depth: u8 },
    StorePathThisProp { name: u16, depth: u8 },
    AppendPathThisProp { name: u16, depth: u8 },
    /// Class constant CLS::NAME (self/parent/static keywords resolve like
    /// the walker's resolve_class_name).
    ClassConstOp { class: u16, name: u16 },
    /// `global $x`: bind slot to the global variable (registered in the
    /// run's bound list; bound slots sync around out-of-VM calls and exit).
    BindGlobal { slot: u16, name: u16 },
    /// `static $x = init;` — two ops so the initializer runs ONCE (PHP 8.3
    /// allows side-effecting initializers): StaticCheck binds the existing
    /// cell and jumps past the init ops; StaticInit pops the init value,
    /// creates the cell, and binds it. The slot holds the walker's own
    /// per-function Ref cell (Eval::static_vars); slot ops write THROUGH a
    /// Ref-holding slot, so aliasing stays exact (recursion, in-place array
    /// mutation) with no sync choreography. names[key] is
    /// "Class::fn\0varname" (the walker's own static_vars key).
    StaticCheck { slot: u16, key: u16, done: u32 },
    StaticInit { slot: u16, key: u16 },
    /// CLS::$prop read/write through the walker's static_prop_key.
    LoadStaticProp { class: u16, name: u16 },
    StoreStaticProp { class: u16, name: u16 },
    /// `$v instanceof ClassName` (static right-hand name).
    InstanceOfOp { class: u16 },
    /// unset($slot[key]) — pop key; ArrayAccess objects get offsetUnset.
    UnsetIndex(u16),
    UnsetThisPropIndex(u16),
    UnsetThisProp(u16),
    /// Method call: stack holds receiver then argc args. Dispatches through
    /// Eval::call_method (same resolution/visibility path as the walker).
    /// Compile-time restriction keeps by-ref parameter semantics safe: no
    /// argument may be an lvalue (methods can't be resolved statically, so a
    /// by-ref param writing back into a caller variable can't be honored).
    CallMethod { name: u16, argc: u8, site: u16 },
}

pub struct Chunk {
    pub ops: Vec<Op>,
    pub consts: Vec<super::value::Value>,
    pub names: Vec<String>,
    pub slot_names: Vec<String>,
    /// Whether any op touches $this (the interpreter resolves it at entry).
    pub uses_this: bool,
    /// Top-level chunks: slots ARE global variables, so calls that leave the
    /// VM must sync slots out/in (walker callees can mutate globals).
    pub top_level: bool,
    /// Function-body chunks: parameters occupy slots 0..nparams in order.
    /// Defaults here are compile-time constants; anything fancier clears
    /// fast_callable.
    pub nparams: u16,
    pub param_defaults: Vec<Option<super::value::Value>>,
    /// Eligible for the VM-native fast call: untyped by-value params with
    /// constant defaults, no variadics/promotion.
    pub fast_callable: bool,
    /// Any op resolves names through the namespace/use context (const
    /// lookups, new, static calls) — fast calls must swap def-ctx.
    pub needs_ctx: bool,
    /// Contains BindGlobal ops (bound slots need call/exit synchronization).
    pub has_globals: bool,
    /// Per-call-site memo (validated against Eval::funcs_generation).
    pub sites: std::cell::RefCell<Vec<SiteMemo>>,
    /// Owning function name, for diagnostics.
    pub debug_name: String,
}

#[derive(Clone)]
pub struct SiteMemo {
    pub generation: u64,
    pub target: Callee,
}

#[derive(Clone)]
pub enum Callee {
    Unresolved,
    /// Compiled user function: bind args straight into callee slots.
    Fast(std::rc::Rc<FuncDecl>, std::rc::Rc<Chunk>),
    /// User function that must go through the walker's call machinery.
    Slow(std::rc::Rc<FuncDecl>),
    Builtin,
    /// Monomorphic method cache: valid for receivers of exactly this class
    /// (lowercase). frame_name is the precomputed "Class->method" label.
    FastMethod {
        recv_class: String,
        decl_class: String,
        frame_name: String,
        decl: std::rc::Rc<MethodDecl>,
        chunk: std::rc::Rc<Chunk>,
    },
    /// This class+method must use the walker's dispatch (visibility,
    /// magic, typed params…); still keyed to the receiver class.
    SlowMethod { recv_class: String },
}

/// What the compiler needs to know about a callable name, answered by the
/// evaluator at compile time (lazily, at first execution, when the function
/// table is populated).
pub enum CalleeKind {
    /// Safe to call by value: user function without by-ref params, or a
    /// builtin outside the special-dispatch/by-ref table.
    Safe,
    /// Anything else — by-ref params, scope introspection, include/exit,
    /// unknown name. The chunk bails.
    Unsafe,
}

/// Definition-site values for compile-time magic constants.
#[derive(Default)]
pub struct MagicCtx {
    pub file: String,
    pub dir: String,
    pub function: String,
    pub class: String,
    pub namespace: String,
}

pub struct Compiler<'a> {
    ops: Vec<Op>,
    consts: Vec<super::value::Value>,
    names: Vec<String>,
    slots: HashMap<String, u16>,
    slot_names: Vec<String>,
    resolver: &'a dyn Fn(&str) -> CalleeKind,
    magic: &'a MagicCtx,
    /// (continue_target, break_patches, iter_depth_at_entry) per open loop.
    loops: Vec<LoopCtx>,
    iter_depth: usize,
    top_level: bool,
    uses_this: bool,
    needs_ctx: bool,
    has_globals: bool,
    n_sites: u16,
}

enum PathRoot {
    Slot(u16),
    ThisProp(u16),
}

struct LoopCtx {
    continue_target: Option<u32>, // None until known (for/while post-parts)
    continue_patches: Vec<usize>,
    break_patches: Vec<usize>,
    iter_depth: usize,
    is_switch: bool,
    is_foreach: bool,
}

/// Builtins that must never be called from a chunk: by-ref out-params,
/// scope introspection, control transfer, or special eval_call dispatch.
const BAIL_CALLS: &[&str] = &[
    "compact", "extract", "get_defined_vars", "func_get_args", "func_get_arg",
    "func_num_args", "eval", "include", "include_once", "require", "require_once",
    "exit", "die", "preg_match", "preg_match_all", "preg_replace", "str_replace",
    "str_ireplace", "similar_text", "parse_str", "sscanf", "fscanf", "settype",
    "array_multisort", "xml_parse_into_struct", "fsockopen", "stream_socket_client",
    "array_push", "array_pop", "array_shift", "array_unshift", "array_splice",
    "sort", "rsort", "asort", "arsort", "ksort", "krsort", "usort", "uasort",
    "uksort", "shuffle", "natsort", "natcasesort", "array_walk",
    "array_walk_recursive", "reset", "end", "next", "prev", "current", "key",
    "each", "pos", "call_user_func", "call_user_func_array", "usleep", "sleep",
    "debug_backtrace", "get_class", "flock",
];

impl<'a> Compiler<'a> {
    pub fn compile(
        body: &[Stmt],
        top_level: bool,
        params: Option<&[Param]>,
        magic: &MagicCtx,
        resolver: &'a dyn Fn(&str) -> CalleeKind,
    ) -> Option<Chunk> {
        let mut c = Compiler {
            ops: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            slots: HashMap::new(),
            slot_names: Vec::new(),
            resolver,
            magic,
            loops: Vec::new(),
            iter_depth: 0,
            top_level,
            uses_this: false,
            needs_ctx: false,
            has_globals: false,
            n_sites: 0,
        };
        // parameters claim slots 0..n in declaration order
        let mut nparams = 0u16;
        let mut param_defaults: Vec<Option<super::value::Value>> = Vec::new();
        let mut fast_callable = params.is_some();
        if let Some(ps) = params {
            for p in ps {
                let s = c.slot(&p.name)?;
                debug_assert_eq!(s as usize, param_defaults.len());
                nparams += 1;
                if p.by_ref || p.variadic || p.type_hint.is_some() || p.promote.is_some() {
                    fast_callable = false;
                }
                let d = match &p.default {
                    None => None,
                    Some(Expr::Null) => Some(super::value::Value::Null),
                    Some(Expr::Bool(b)) => Some(super::value::Value::Bool(*b)),
                    Some(Expr::Int(n)) => Some(super::value::Value::Int(*n)),
                    Some(Expr::Float(f)) => Some(super::value::Value::Float(*f)),
                    Some(Expr::Str(s)) => Some(super::value::Value::Str(s.clone())),
                    Some(Expr::ConstFetch(n)) if n.parts.len() == 1 => {
                        match n.last().to_ascii_lowercase().as_str() {
                            "true" => Some(super::value::Value::Bool(true)),
                            "false" => Some(super::value::Value::Bool(false)),
                            "null" => Some(super::value::Value::Null),
                            _ => {
                                fast_callable = false;
                                None
                            }
                        }
                    }
                    Some(_) => {
                        fast_callable = false;
                        None
                    }
                };
                param_defaults.push(d);
            }
        }
        if top_level {
            // direct top-level declarations were hoisted before execution —
            // safe to skip. NESTED (conditional) declarations register at
            // runtime, which a chunk can't do: stmt() bails on those.
            for s in body {
                let inner = match s {
                    Stmt::Marked(line, i) => {
                        c.ops.push(Op::Line(*line));
                        &**i
                    }
                    other => other,
                };
                if matches!(inner, Stmt::Func(_) | Stmt::Class(_)) {
                    continue;
                }
                c.stmt(inner)?;
            }
        } else {
            c.block(body)?;
        }
        c.ops.push(Op::RetNull);
        let n_sites = c.n_sites as usize;
        Some(Chunk {
            ops: c.ops,
            consts: c.consts,
            names: c.names,
            slot_names: c.slot_names,
            uses_this: c.uses_this,
            top_level,
            nparams,
            param_defaults,
            fast_callable,
            needs_ctx: c.needs_ctx,
            has_globals: c.has_globals,
            sites: std::cell::RefCell::new(vec![
                SiteMemo { generation: 0, target: Callee::Unresolved };
                n_sites
            ]),
            debug_name: String::new(),
        })
    }

    fn slot(&mut self, name: &str) -> Option<u16> {
        // superglobals only behave like locals at top level (functions read
        // them from the global scope — unsupported in slots)
        if !self.top_level && super::eval::is_superglobal(name) {
            return None;
        }
        if name == "GLOBALS" || name == "this" {
            return None;
        }
        if let Some(&i) = self.slots.get(name) {
            return Some(i);
        }
        let i = u16::try_from(self.slot_names.len()).ok()?;
        self.slots.insert(name.to_string(), i);
        self.slot_names.push(name.to_string());
        Some(i)
    }

    fn konst(&mut self, v: super::value::Value) -> Option<u16> {
        let i = u16::try_from(self.consts.len()).ok()?;
        self.consts.push(v);
        Some(i)
    }

    fn name(&mut self, n: &str) -> Option<u16> {
        if let Some(i) = self.names.iter().position(|x| x == n) {
            return u16::try_from(i).ok();
        }
        let i = u16::try_from(self.names.len()).ok()?;
        self.names.push(n.to_string());
        Some(i)
    }

    fn here(&self) -> u32 {
        self.ops.len() as u32
    }

    fn is_this(e: &Expr) -> bool {
        matches!(e, Expr::Var(n) if n == "this")
    }

    /// `$this->prop` with a static name, inside a method chunk.
    fn this_prop(&mut self, e: &Expr) -> Option<u16> {
        if let Expr::Prop(base, PropName::Id(p), false) = e {
            if Self::is_this(base) && !self.top_level {
                self.uses_this = true;
                return self.name(p);
            }
        }
        None
    }

    /// Decompose `$root[k1][k2]…` into its root and key expressions
    /// (left-to-right). Roots: a local slot or `$this->prop`. The final
    /// Option is None for the append position (`…[] =`).
    fn index_path<'e>(&mut self, e: &'e Expr) -> Option<(PathRoot, Vec<&'e Expr>)> {
        match e {
            Expr::Index(base, Some(k)) => {
                let (root, mut keys) = self.index_path(base)?;
                keys.push(k);
                Some((root, keys))
            }
            Expr::Var(n) if n != "this" => Some((PathRoot::Slot(self.slot(n)?), Vec::new())),
            Expr::Prop(..) => Some((PathRoot::ThisProp(self.this_prop(e)?), Vec::new())),
            _ => None,
        }
    }

    /// An expression that could act as a by-ref write-back target. Method
    /// calls refuse these as arguments (see Op::CallMethod).
    fn is_lvalue(e: &Expr) -> bool {
        matches!(
            e,
            Expr::Var(_)
                | Expr::VarVar(_)
                | Expr::Index(..)
                | Expr::Prop(..)
                | Expr::StaticProp(..)
        )
    }

    fn block(&mut self, stmts: &[Stmt]) -> Option<()> {
        for s in stmts {
            self.stmt(s)?;
        }
        Some(())
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        match s {
            Stmt::Marked(line, inner) => {
                self.ops.push(Op::Line(*line));
                self.stmt(inner)
            }
            Stmt::Nop => Some(()),
            Stmt::Block(b) => self.block(b),
            Stmt::InlineHtml(bytes) => {
                let k = self.konst(super::value::Value::Str(bytes.clone()))?;
                self.ops.push(Op::Const(k));
                self.ops.push(Op::Echo(1));
                Some(())
            }
            Stmt::Echo(items) => {
                if items.len() > u8::MAX as usize {
                    return None;
                }
                for e in items {
                    self.expr(e)?;
                }
                self.ops.push(Op::Echo(items.len() as u8));
                Some(())
            }
            Stmt::Expr(e) => {
                self.expr_stmt(e)?;
                Some(())
            }
            Stmt::Return(e) => {
                match e {
                    Some(e) => {
                        self.expr(e)?;
                        self.ops.push(Op::Ret);
                    }
                    None => self.ops.push(Op::RetNull),
                }
                Some(())
            }
            Stmt::If { cond, then, elseifs, els } => {
                // chain of cond → jmp-false to next arm
                let mut end_patches = Vec::new();
                self.expr(cond)?;
                let mut false_patch = self.ops.len();
                self.ops.push(Op::JmpIfFalse(0));
                self.block(then)?;
                end_patches.push(self.ops.len());
                self.ops.push(Op::Jmp(0));
                for (c, b) in elseifs {
                    let here = self.here();
                    self.patch_jump(false_patch, here);
                    self.expr(c)?;
                    false_patch = self.ops.len();
                    self.ops.push(Op::JmpIfFalse(0));
                    self.block(b)?;
                    end_patches.push(self.ops.len());
                    self.ops.push(Op::Jmp(0));
                }
                let here = self.here();
                self.patch_jump(false_patch, here);
                if let Some(b) = els {
                    self.block(b)?;
                }
                let end = self.here();
                for p in end_patches {
                    self.patch_jump(p, end);
                }
                Some(())
            }
            Stmt::While { cond, body } => {
                let start = self.here();
                self.expr(cond)?;
                let exit_patch = self.ops.len();
                self.ops.push(Op::JmpIfFalse(0));
                self.loops.push(LoopCtx {
                    continue_target: Some(start),
                    continue_patches: Vec::new(),
                    break_patches: Vec::new(),
                    iter_depth: self.iter_depth,
                    is_switch: false,
                    is_foreach: false,
                });
                self.block(body)?;
                self.ops.push(Op::Jmp(start));
                let end = self.here();
                self.patch_jump(exit_patch, end);
                self.finish_loop(end, start);
                Some(())
            }
            Stmt::DoWhile { body, cond } => {
                let start = self.here();
                self.loops.push(LoopCtx {
                    continue_target: None,
                    continue_patches: Vec::new(),
                    break_patches: Vec::new(),
                    iter_depth: self.iter_depth,
                    is_switch: false,
                    is_foreach: false,
                });
                self.block(body)?;
                let cond_at = self.here();
                self.expr(cond)?;
                self.ops.push(Op::JmpIfTrue(start));
                let end = self.here();
                self.finish_loop(end, cond_at);
                Some(())
            }
            Stmt::For { init, cond, step, body } => {
                for e in init {
                    self.expr_stmt(e)?;
                }
                let start = self.here();
                let exit_patch = if cond.is_empty() {
                    None
                } else {
                    // PHP evaluates all cond exprs, last decides
                    for (i, e) in cond.iter().enumerate() {
                        self.expr(e)?;
                        if i + 1 < cond.len() {
                            self.ops.push(Op::Pop);
                        }
                    }
                    let p = self.ops.len();
                    self.ops.push(Op::JmpIfFalse(0));
                    Some(p)
                };
                self.loops.push(LoopCtx {
                    continue_target: None,
                    continue_patches: Vec::new(),
                    break_patches: Vec::new(),
                    iter_depth: self.iter_depth,
                    is_switch: false,
                    is_foreach: false,
                });
                self.block(body)?;
                let step_at = self.here();
                for e in step {
                    self.expr_stmt(e)?;
                }
                self.ops.push(Op::Jmp(start));
                let end = self.here();
                if let Some(p) = exit_patch {
                    self.patch_jump(p, end);
                }
                self.finish_loop(end, step_at);
                Some(())
            }
            Stmt::Foreach { array, key, value, by_ref, body } => {
                if *by_ref {
                    return None;
                }
                // subset: iterate a plain local array variable
                let arr_slot = match array {
                    Expr::Var(n) => self.slot(n)?,
                    _ => return None,
                };
                let val_slot = match value {
                    Expr::Var(n) => self.slot(n)?,
                    _ => return None,
                };
                let key_slot = match key {
                    None => u16::MAX,
                    Some(Expr::Var(n)) => self.slot(n)?,
                    Some(_) => return None,
                };
                let init_patch = self.ops.len();
                self.ops.push(Op::IterInit { slot: arr_slot, end: 0 });
                let start = self.here();
                let next_patch = self.ops.len();
                self.ops.push(Op::IterNext { val_slot, key_slot, end: 0 });
                self.iter_depth += 1;
                self.loops.push(LoopCtx {
                    continue_target: Some(start),
                    continue_patches: Vec::new(),
                    break_patches: Vec::new(),
                    iter_depth: self.iter_depth,
                    is_switch: false,
                    is_foreach: true,
                });
                self.block(body)?;
                self.ops.push(Op::Jmp(start));
                let end = self.here();
                self.iter_depth -= 1;
                match &mut self.ops[init_patch] {
                    Op::IterInit { end: e, .. } => *e = end,
                    _ => unreachable!(),
                }
                match &mut self.ops[next_patch] {
                    Op::IterNext { end: e, .. } => *e = end,
                    _ => unreachable!(),
                }
                self.finish_loop(end, start);
                Some(())
            }
            Stmt::Global(names) => {
                // at top level `global` is a no-op (already global scope);
                // slots there alias globals via the boundary syncs anyway
                if !self.top_level {
                    for n in names {
                        let s = self.slot(n)?;
                        let ni = self.name(n)?;
                        self.has_globals = true;
                        self.ops.push(Op::BindGlobal { slot: s, name: ni });
                    }
                }
                Some(())
            }
            // `static $x = init` — only inside function bodies (a top-level
            // static is a plain var to the walker; keep those on the walker).
            Stmt::StaticVar(vars) => {
                if self.top_level {
                    return bail("stmt:static-var-top");
                }
                let fnkey = format!("{}::{}", self.magic.class, self.magic.function);
                for (name, init) in vars {
                    let s = self.slot(name)?;
                    let ki = self.name(&format!("{fnkey}\u{0}{name}"))?;
                    let check = self.ops.len();
                    self.ops.push(Op::StaticCheck { slot: s, key: ki, done: 0 });
                    match init {
                        Some(e) => self.expr(e)?,
                        None => {
                            let k = self.konst(super::value::Value::Null)?;
                            self.ops.push(Op::Const(k));
                        }
                    }
                    self.ops.push(Op::StaticInit { slot: s, key: ki });
                    let done = self.here();
                    match &mut self.ops[check] {
                        Op::StaticCheck { done: d, .. } => *d = done,
                        _ => unreachable!(),
                    }
                }
                Some(())
            }
            Stmt::Unset(items) => {
                for it in items {
                    match it {
                        Expr::Var(n) => {
                            let s = self.slot(n)?;
                            self.ops.push(Op::UnsetSlot(s));
                        }
                        Expr::Index(base, Some(k)) => {
                            if let Some(p) = self.this_prop(base) {
                                self.expr(k)?;
                                self.ops.push(Op::UnsetThisPropIndex(p));
                            } else {
                                let s = match &**base {
                                    Expr::Var(n) => self.slot(n)?,
                                    _ => return bail("stmt:unset-shape"),
                                };
                                self.expr(k)?;
                                self.ops.push(Op::UnsetIndex(s));
                            }
                        }
                        Expr::Prop(..) => {
                            let p = self.this_prop(it)?;
                            self.ops.push(Op::UnsetThisProp(p));
                        }
                        _ => return bail("stmt:unset-shape"),
                    }
                }
                Some(())
            }
            Stmt::Switch { subject, cases } => {
                // subject into an anonymous temp slot, dispatch via loose Eq,
                // bodies in order with PHP fall-through; break jumps to end
                let tmp = self.slot(&format!("\x00switch{}", self.ops.len()))?;
                self.expr(subject)?;
                self.ops.push(Op::Store(tmp));
                let mut body_patches: Vec<usize> = Vec::new(); // JmpIfTrue per case
                let mut default_idx: Option<usize> = None;
                for (i, c) in cases.iter().enumerate() {
                    match &c.test {
                        Some(t) => {
                            self.ops.push(Op::LoadQuiet(tmp));
                            self.expr(t)?;
                            self.ops.push(Op::Bin(BinOp::Eq));
                            body_patches.push(self.ops.len());
                            self.ops.push(Op::JmpIfTrue(0));
                        }
                        None => {
                            default_idx = Some(i);
                            body_patches.push(usize::MAX); // placeholder
                        }
                    }
                }
                let dispatch_end = self.ops.len();
                self.ops.push(Op::Jmp(0)); // to default body or end
                self.loops.push(LoopCtx {
                    continue_target: None,
                    continue_patches: Vec::new(),
                    break_patches: Vec::new(),
                    iter_depth: self.iter_depth,
                    is_switch: true,
                    is_foreach: false,
                });
                let mut body_starts: Vec<u32> = Vec::new();
                for c in cases {
                    body_starts.push(self.here());
                    self.block(&c.body)?;
                }
                let end = self.here();
                for (i, p) in body_patches.iter().enumerate() {
                    if *p != usize::MAX {
                        self.patch_jump(*p, body_starts[i]);
                    }
                }
                let default_target = default_idx.map(|i| body_starts[i]).unwrap_or(end);
                self.patch_jump(dispatch_end, default_target);
                let ctx = self.loops.pop().unwrap();
                for p in ctx.break_patches {
                    self.patch_jump(p, end);
                }
                if !ctx.continue_patches.is_empty() {
                    return None;
                }
                Some(())
            }
            Stmt::Break(n) => {
                if *n != 1 {
                    return None;
                }
                // leaving a foreach: pop its iterator (only when the loop
                // being broken IS the foreach — a while nested inside one
                // must not pop the outer iterator)
                if self.loops.last()?.is_foreach {
                    self.ops.push(Op::PopIter);
                }
                let p = self.ops.len();
                self.ops.push(Op::Jmp(0));
                self.loops.last_mut()?.break_patches.push(p);
                Some(())
            }
            Stmt::Continue(n) => {
                if *n != 1 {
                    return None;
                }
                if self.loops.last()?.is_switch {
                    return None; // continue-in-switch targets the outer loop
                }
                let target = self.loops.last()?.continue_target;
                match target {
                    Some(t) => self.ops.push(Op::Jmp(t)),
                    None => {
                        let p = self.ops.len();
                        self.ops.push(Op::Jmp(0));
                        self.loops.last_mut()?.continue_patches.push(p);
                    }
                }
                Some(())
            }
            other => bail(stmt_name(other)),
        }
    }

    fn finish_loop(&mut self, end: u32, continue_to: u32) {
        let ctx = self.loops.pop().unwrap();
        for p in ctx.break_patches {
            self.patch_jump(p, end);
        }
        for p in ctx.continue_patches {
            self.patch_jump(p, continue_to);
        }
    }

    fn patch_jump(&mut self, at: usize, target: u32) {
        match &mut self.ops[at] {
            Op::Jmp(t) | Op::JmpIfFalse(t) | Op::JmpIfTrue(t) => *t = target,
            _ => unreachable!("patch target is not a jump"),
        }
    }

    /// Compile an expression evaluated for its side effect (statement
    /// position): avoids pushing values that are immediately popped.
    fn expr_stmt(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Assign(lhs, rhs) => match &**lhs {
                Expr::Var(n) => {
                    let s = self.slot(n)?;
                    self.expr(rhs)?;
                    self.ops.push(Op::Store(s));
                    Some(())
                }
                Expr::Index(base, idx) => {
                    // decompose into root + full key path; idx None = append
                    let (root, keys) = self.index_path(base)?;
                    let mut depth = keys.len();
                    if keys.len() > u8::MAX as usize {
                        return None;
                    }
                    for k in &keys {
                        self.expr(k)?;
                    }
                    if let Some(i) = idx {
                        self.expr(i)?;
                        depth += 1;
                    }
                    self.expr(rhs)?;
                    let d = depth as u8;
                    match (root, idx.is_some(), d) {
                        (PathRoot::Slot(s), true, 1) => self.ops.push(Op::StoreIndex(s)),
                        (PathRoot::ThisProp(p), true, 1) => {
                            self.ops.push(Op::StoreIndexThisProp(p))
                        }
                        (PathRoot::Slot(s), false, 0) => self.ops.push(Op::Append(s)),
                        (PathRoot::ThisProp(p), false, 0) => {
                            self.ops.push(Op::AppendThisProp(p))
                        }
                        (PathRoot::Slot(s), true, d) => {
                            self.ops.push(Op::StorePath { slot: s, depth: d })
                        }
                        (PathRoot::ThisProp(p), true, d) => {
                            self.ops.push(Op::StorePathThisProp { name: p, depth: d })
                        }
                        (PathRoot::Slot(s), false, d) => {
                            self.ops.push(Op::AppendPath { slot: s, depth: d })
                        }
                        (PathRoot::ThisProp(p), false, d) => {
                            self.ops.push(Op::AppendPathThisProp { name: p, depth: d })
                        }
                    }
                    Some(())
                }
                Expr::Prop(..) => {
                    let p = self.this_prop(lhs)?;
                    self.expr(rhs)?;
                    self.ops.push(Op::StoreThisProp(p));
                    Some(())
                }
                Expr::StaticProp(class, pname) => {
                    let raw = match &**class {
                        Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                            n.last().to_string()
                        }
                        _ => return bail("expr:static-prop-shape"),
                    };
                    let ci = self.name(&raw)?;
                    let ni = self.name(pname)?;
                    self.needs_ctx = true;
                    self.expr(rhs)?;
                    self.ops.push(Op::StoreStaticProp { class: ci, name: ni });
                    Some(())
                }
                other => bail(expr_name(other)),
            },
            Expr::AssignOp(op, lhs, rhs) if self.this_prop(lhs).is_some() => {
                let p = self.this_prop(lhs)?;
                self.ops.push(Op::LoadThisProp(p));
                self.expr(rhs)?;
                self.ops.push(Op::Bin(*op));
                self.ops.push(Op::StoreThisProp(p));
                Some(())
            }
            Expr::AssignOp(op, lhs, rhs) => match &**lhs {
                Expr::Var(n) => {
                    let s = self.slot(n)?;
                    if matches!(op, BinOp::Concat) {
                        // the walker's `.=` fast path creates silently
                        self.expr(rhs)?;
                        self.ops.push(Op::ConcatAssign(s));
                        return Some(());
                    }
                    // compound assignment reads CHECKED (undefined warns)
                    self.ops.push(Op::Load(s));
                    self.expr(rhs)?;
                    self.ops.push(Op::Bin(*op));
                    self.ops.push(Op::Store(s));
                    Some(())
                }
                _ => None,
            },
            Expr::PreInc(v) | Expr::PostInc(v) => self.inc_dec_stmt(v, BinOp::Add),
            Expr::PreDec(v) | Expr::PostDec(v) => self.inc_dec_stmt(v, BinOp::Sub),
            _ => {
                self.expr(e)?;
                self.ops.push(Op::Pop);
                Some(())
            }
        }
    }

    fn inc_dec_stmt(&mut self, v: &Expr, op: BinOp) -> Option<()> {
        let s = match v {
            Expr::Var(n) => self.slot(n)?,
            _ => return None,
        };
        let one = self.konst(super::value::Value::Int(1))?;
        self.ops.push(Op::Load(s));
        self.ops.push(Op::Const(one));
        self.ops.push(Op::Bin(op));
        self.ops.push(Op::Store(s));
        Some(())
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        use super::value::Value;
        match e {
            Expr::Null => {
                let k = self.konst(Value::Null)?;
                self.ops.push(Op::Const(k));
            }
            Expr::Bool(b) => {
                let k = self.konst(Value::Bool(*b))?;
                self.ops.push(Op::Const(k));
            }
            Expr::Int(n) => {
                let k = self.konst(Value::Int(*n))?;
                self.ops.push(Op::Const(k));
            }
            Expr::Float(f) => {
                let k = self.konst(Value::Float(*f))?;
                self.ops.push(Op::Const(k));
            }
            Expr::Str(s) => {
                let k = self.konst(Value::Str(s.clone()))?;
                self.ops.push(Op::Const(k));
            }
            Expr::Array(items) => {
                self.ops.push(Op::NewArr);
                for it in items {
                    if it.by_ref || it.spread {
                        return None;
                    }
                    match &it.key {
                        Some(k) => {
                            self.expr(k)?;
                            self.expr(&it.value)?;
                            self.ops.push(Op::ArrSet);
                        }
                        None => {
                            self.expr(&it.value)?;
                            self.ops.push(Op::ArrPush);
                        }
                    }
                }
            }
            Expr::New(class, args) => {
                let cname = match &**class {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_string()
                    }
                    _ => return None,
                };
                if args.len() > u8::MAX as usize {
                    return None;
                }
                for a in args {
                    if a.spread || a.by_ref || a.name.is_some() || Self::is_lvalue(&a.value) {
                        return None;
                    }
                    self.expr(&a.value)?;
                }
                let ci = self.name(&cname)?;
                self.needs_ctx = true;
                self.ops.push(Op::NewObj { class: ci, argc: args.len() as u8 });
            }
            Expr::StaticCall(class, PropName::Id(m), args) => {
                let cname = match &**class {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_string()
                    }
                    _ => return None,
                };
                if args.len() > u8::MAX as usize {
                    return None;
                }
                for a in args {
                    if a.spread || a.by_ref || a.name.is_some() || Self::is_lvalue(&a.value) {
                        return None;
                    }
                    self.expr(&a.value)?;
                }
                let ci = self.name(&cname)?;
                let mi = self.name(m)?;
                self.needs_ctx = true;
                // static calls forward $this when the target isn't static
                // (parent::m()), so the chunk must resolve its receiver
                if !self.top_level {
                    self.uses_this = true;
                }
                self.ops.push(Op::CallStatic { class: ci, method: mi, argc: args.len() as u8 });
            }
            Expr::Var(n) => {
                if n == "this" && !self.top_level {
                    self.uses_this = true;
                    self.ops.push(Op::LoadThis);
                } else {
                    let s = self.slot(n)?;
                    self.ops.push(Op::Load(s));
                }
            }
            Expr::Prop(..) => {
                let p = self.this_prop(e)?;
                self.ops.push(Op::LoadThisProp(p));
            }
            Expr::MethodCall(recv, PropName::Id(m), args, false) => {
                // receiver: a local var, $this, or $this->prop
                match &**recv {
                    Expr::Var(n) if n == "this" => {
                        self.uses_this = true;
                        self.ops.push(Op::LoadThis);
                    }
                    Expr::Var(n) => {
                        let s = self.slot(n)?;
                        self.ops.push(Op::Load(s));
                    }
                    other => {
                        let p = self.this_prop(other)?;
                        self.ops.push(Op::LoadThisProp(p));
                    }
                }
                if args.len() > u8::MAX as usize {
                    return None;
                }
                for a in args {
                    if a.spread || a.by_ref || a.name.is_some() || Self::is_lvalue(&a.value) {
                        return None;
                    }
                    self.expr(&a.value)?;
                }
                let ni = self.name(m)?;
                let site = self.n_sites;
                self.n_sites = self.n_sites.checked_add(1)?;
                self.ops.push(Op::CallMethod { name: ni, argc: args.len() as u8, site });
            }
            Expr::Template(parts) => {
                // concat chain; each part compiles then Concat-folds
                let mut first = true;
                for p in parts {
                    match p {
                        TplPart::Lit(bytes) => {
                            let k = self.konst(Value::Str(bytes.clone()))?;
                            self.ops.push(Op::Const(k));
                        }
                        TplPart::Expr(e) => self.expr(e)?,
                    }
                    if !first {
                        self.ops.push(Op::Bin(BinOp::Concat));
                    }
                    first = false;
                }
                if first {
                    let k = self.konst(Value::Str(Vec::new()))?;
                    self.ops.push(Op::Const(k));
                }
            }
            Expr::Unary(op, inner) => {
                match op {
                    UnOp::Not => {
                        self.expr(inner)?;
                        self.ops.push(Op::Not);
                    }
                    UnOp::Neg => {
                        self.expr(inner)?;
                        self.ops.push(Op::Neg);
                    }
                    UnOp::Pos => {
                        // +$x is an arithmetic no-op after numeric coercion:
                        // 0 + $x preserves PHP semantics
                        let k = self.konst(Value::Int(0))?;
                        self.ops.push(Op::Const(k));
                        self.expr(inner)?;
                        self.ops.push(Op::Bin(BinOp::Add));
                    }
                    _ => return None,
                }
            }
            Expr::Binary(op, l, r) => match op {
                BinOp::And => {
                    self.expr(l)?;
                    let f1 = self.ops.len();
                    self.ops.push(Op::JmpIfFalse(0));
                    self.expr(r)?;
                    let f2 = self.ops.len();
                    self.ops.push(Op::JmpIfFalse(0));
                    let kt = self.konst(Value::Bool(true))?;
                    self.ops.push(Op::Const(kt));
                    let done = self.ops.len();
                    self.ops.push(Op::Jmp(0));
                    let fh = self.here();
                    self.patch_jump(f1, fh);
                    self.patch_jump(f2, fh);
                    let kf = self.konst(Value::Bool(false))?;
                    self.ops.push(Op::Const(kf));
                    let end = self.here();
                    self.patch_jump(done, end);
                }
                BinOp::Or => {
                    self.expr(l)?;
                    let t1 = self.ops.len();
                    self.ops.push(Op::JmpIfTrue(0));
                    self.expr(r)?;
                    let t2 = self.ops.len();
                    self.ops.push(Op::JmpIfTrue(0));
                    let kf = self.konst(Value::Bool(false))?;
                    self.ops.push(Op::Const(kf));
                    let done = self.ops.len();
                    self.ops.push(Op::Jmp(0));
                    let th = self.here();
                    self.patch_jump(t1, th);
                    self.patch_jump(t2, th);
                    let kt = self.konst(Value::Bool(true))?;
                    self.ops.push(Op::Const(kt));
                    let end = self.here();
                    self.patch_jump(done, end);
                }
                BinOp::Coalesce => {
                    // only quiet lvalue shapes on the left
                    match &**l {
                        Expr::Var(n) => {
                            let s = self.slot(n)?;
                            self.ops.push(Op::LoadQuiet(s));
                        }
                        Expr::Index(base, Some(i)) => {
                            let s = match &**base {
                                Expr::Var(n) => self.slot(n)?,
                                _ => return None,
                            };
                            self.expr(i)?;
                            self.ops.push(Op::LoadIndexQuiet(s));
                        }
                        _ => return None,
                    }
                    self.ops.push(Op::Dup);
                    self.ops.push(Op::IsNull);
                    let use_default = self.ops.len();
                    self.ops.push(Op::JmpIfTrue(0));
                    let done = self.ops.len();
                    self.ops.push(Op::Jmp(0));
                    let dh = self.here();
                    self.patch_jump(use_default, dh);
                    self.ops.push(Op::Pop);
                    self.expr(r)?;
                    let end = self.here();
                    self.patch_jump(done, end);
                }
                _ => {
                    self.expr(l)?;
                    self.expr(r)?;
                    self.ops.push(Op::Bin(*op));
                }
            },
            Expr::Ternary(c, mid, els) => {
                self.expr(c)?;
                match mid {
                    Some(m) => {
                        let fp = self.ops.len();
                        self.ops.push(Op::JmpIfFalse(0));
                        self.expr(m)?;
                        let done = self.ops.len();
                        self.ops.push(Op::Jmp(0));
                        let fh = self.here();
                        self.patch_jump(fp, fh);
                        self.expr(els)?;
                        let end = self.here();
                        self.patch_jump(done, end);
                    }
                    None => {
                        // a ?: b — a evaluated once
                        self.ops.push(Op::Dup);
                        let fp = self.ops.len();
                        self.ops.push(Op::JmpIfFalse(0));
                        let done = self.ops.len();
                        self.ops.push(Op::Jmp(0));
                        let fh = self.here();
                        self.patch_jump(fp, fh);
                        self.ops.push(Op::Pop);
                        self.expr(els)?;
                        let end = self.here();
                        self.patch_jump(done, end);
                    }
                }
            }
            Expr::Assign(..) | Expr::AssignOp(..) => {
                // value-position assignment: compile the store, then reload
                match e {
                    Expr::Assign(lhs, _) | Expr::AssignOp(_, lhs, _) => {
                        let n = match &**lhs {
                            Expr::Var(n) => n.clone(),
                            _ => return None,
                        };
                        self.expr_stmt(e)?;
                        let s = self.slot(&n)?;
                        self.ops.push(Op::LoadQuiet(s));
                    }
                    _ => unreachable!(),
                }
            }
            Expr::PreInc(v) | Expr::PreDec(v) => {
                let op = if matches!(e, Expr::PreInc(_)) { BinOp::Add } else { BinOp::Sub };
                let n = match &**v {
                    Expr::Var(n) => n.clone(),
                    _ => return None,
                };
                self.inc_dec_stmt(v, op)?;
                let s = self.slot(&n)?;
                self.ops.push(Op::LoadQuiet(s));
            }
            Expr::PostInc(v) | Expr::PostDec(v) => {
                let op = if matches!(e, Expr::PostInc(_)) { BinOp::Add } else { BinOp::Sub };
                let s = match &**v {
                    Expr::Var(n) => self.slot(n)?,
                    _ => return None,
                };
                let one = self.konst(Value::Int(1))?;
                self.ops.push(Op::LoadQuiet(s)); // old value (result)
                self.ops.push(Op::Dup);
                self.ops.push(Op::Const(one));
                self.ops.push(Op::Bin(op));
                self.ops.push(Op::Store(s));
            }
            Expr::Index(..) => {
                let (root, keys) = self.index_path(e)?;
                if keys.is_empty() || keys.len() > u8::MAX as usize {
                    return None;
                }
                for k in &keys {
                    self.expr(k)?;
                }
                match (root, keys.len()) {
                    (PathRoot::Slot(s), 1) => self.ops.push(Op::LoadIndex(s)),
                    (PathRoot::ThisProp(p), 1) => self.ops.push(Op::LoadIndexThisProp(p)),
                    (PathRoot::Slot(s), d) => {
                        self.ops.push(Op::LoadPath { slot: s, depth: d as u8 })
                    }
                    (PathRoot::ThisProp(p), d) => {
                        self.ops.push(Op::LoadPathThisProp { name: p, depth: d as u8 })
                    }
                }
            }
            Expr::Isset(items) => {
                // each item compiles to a bool; multiple items AND-chain with
                // jumps (subset lvalues have no side effects, so evaluation
                // order nuances don't observable-differ)
                let mut false_patches = Vec::new();
                for (idx, item) in items.iter().enumerate() {
                    match item {
                        Expr::Var(n) => {
                            let s = self.slot(n)?;
                            self.ops.push(Op::IssetSlot(s));
                        }
                        Expr::Index(base, Some(i)) => {
                            if let Some(p) = self.this_prop(base) {
                                self.expr(i)?;
                                self.ops.push(Op::IssetThisPropIndex(p));
                            } else {
                                let s = match &**base {
                                    Expr::Var(n) => self.slot(n)?,
                                    _ => return None,
                                };
                                self.expr(i)?;
                                self.ops.push(Op::IssetIndex(s));
                            }
                        }
                        Expr::Prop(..) => {
                            let p = self.this_prop(item)?;
                            self.ops.push(Op::IssetThisProp(p));
                        }
                        _ => return None,
                    }
                    if idx + 1 < items.len() {
                        let fp = self.ops.len();
                        self.ops.push(Op::JmpIfFalse(0));
                        false_patches.push(fp);
                    }
                }
                if !false_patches.is_empty() {
                    let done = self.ops.len();
                    self.ops.push(Op::Jmp(0));
                    let fh = self.here();
                    for p in false_patches {
                        self.patch_jump(p, fh);
                    }
                    let kf = self.konst(super::value::Value::Bool(false))?;
                    self.ops.push(Op::Const(kf));
                    let end = self.here();
                    self.patch_jump(done, end);
                }
            }
            Expr::Empty(inner) => match &**inner {
                Expr::Var(n) => {
                    let s = self.slot(n)?;
                    self.ops.push(Op::LoadQuiet(s));
                    self.ops.push(Op::Not);
                }
                _ => return None,
            },
            Expr::Cast(t, inner) => {
                self.expr(inner)?;
                match t {
                    CastType::Int => self.ops.push(Op::CastInt),
                    CastType::Float => self.ops.push(Op::CastFloat),
                    CastType::String => self.ops.push(Op::CastString),
                    CastType::Bool => self.ops.push(Op::CastBool),
                    _ => return None,
                }
            }
            Expr::ClassConst(class, cname) if cname != "class" => {
                let raw = match &**class {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_string()
                    }
                    _ => return None,
                };
                let ci = self.name(&raw)?;
                let ni = self.name(cname)?;
                self.needs_ctx = true;
                self.ops.push(Op::ClassConstOp { class: ci, name: ni });
            }
            Expr::StaticProp(class, pname) => {
                let raw = match &**class {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_string()
                    }
                    _ => return bail("expr:static-prop-shape"),
                };
                let ci = self.name(&raw)?;
                let ni = self.name(pname)?;
                self.needs_ctx = true;
                self.ops.push(Op::LoadStaticProp { class: ci, name: ni });
            }
            Expr::InstanceOf(v, class) => {
                let raw = match &**class {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_string()
                    }
                    _ => return bail("expr:instanceof-shape"),
                };
                self.expr(v)?;
                let ci = self.name(&raw)?;
                self.needs_ctx = true;
                self.ops.push(Op::InstanceOfOp { class: ci });
            }
            Expr::MagicConst(m) => {
                use super::value::Value;
                let v = match m.to_ascii_uppercase().as_str() {
                    "__FILE__" => Value::Str(self.magic.file.clone().into_bytes()),
                    "__DIR__" => Value::Str(self.magic.dir.clone().into_bytes()),
                    "__FUNCTION__" => Value::Str(self.magic.function.clone().into_bytes()),
                    "__CLASS__" => Value::Str(self.magic.class.clone().into_bytes()),
                    "__METHOD__" => {
                        let m = if self.magic.class.is_empty() {
                            self.magic.function.clone()
                        } else {
                            format!("{}::{}", self.magic.class, self.magic.function)
                        };
                        Value::Str(m.into_bytes())
                    }
                    "__NAMESPACE__" => Value::Str(self.magic.namespace.clone().into_bytes()),
                    // __LINE__ varies per use site; keep it on the walker
                    _ => return bail("expr:magic-line"),
                };
                let k = self.konst(v)?;
                self.ops.push(Op::Const(k));
            }
            Expr::ConstFetch(n) => {
                if n.parts.len() != 1 {
                    return None;
                }
                let bare = n.last();
                match bare.to_ascii_lowercase().as_str() {
                    "true" => {
                        let k = self.konst(Value::Bool(true))?;
                        self.ops.push(Op::Const(k));
                    }
                    "false" => {
                        let k = self.konst(Value::Bool(false))?;
                        self.ops.push(Op::Const(k));
                    }
                    "null" => {
                        let k = self.konst(Value::Null)?;
                        self.ops.push(Op::Const(k));
                    }
                    _ => {
                        let ni = self.name(bare)?;
                        self.needs_ctx = true;
                self.ops.push(Op::ConstLookup(ni));
                    }
                }
            }
            Expr::Call(callee, args) => {
                let name = match &**callee {
                    Expr::ConstFetch(n) if !n.fully_qualified && n.parts.len() == 1 => {
                        n.last().to_ascii_lowercase()
                    }
                    _ => return None,
                };
                if BAIL_CALLS.contains(&name.as_str()) {
                    return None;
                }
                if !matches!((self.resolver)(&name), CalleeKind::Safe) {
                    return None;
                }
                if args.len() > u8::MAX as usize {
                    return None;
                }
                for a in args {
                    if a.spread || a.by_ref || a.name.is_some() {
                        return None;
                    }
                    self.expr(&a.value)?;
                }
                let ni = self.name(&name)?;
                let site = self.n_sites;
                self.n_sites = self.n_sites.checked_add(1)?;
                self.ops.push(Op::CallFn { name: ni, argc: args.len() as u8, site });
            }
            other => return bail(expr_name(other)),
        }
        Some(())
    }
}
