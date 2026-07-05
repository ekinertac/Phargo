//! The abstract syntax tree — the contract between the parser and the
//! evaluator. The lexer produces tokens that the parser shapes into this tree;
//! the evaluator (next stage) walks it. Designed once, up front: this is the
//! grammar made concrete.
#![allow(dead_code)]

/// A (possibly namespaced) name: `Foo`, `\Foo\Bar`, `namespace\Baz`.
#[derive(Debug, Clone, PartialEq)]
pub struct Name {
    pub parts: Vec<String>,
    pub fully_qualified: bool, // leading `\`
}

impl Name {
    pub fn simple(s: impl Into<String>) -> Self {
        Name { parts: vec![s.into()], fully_qualified: false }
    }
    /// The last segment, lowercased — what the legacy engine keys most things on.
    pub fn last(&self) -> &str {
        self.parts.last().map(|s| s.as_str()).unwrap_or("")
    }
    /// `\`-joined string form.
    pub fn to_string(&self) -> String {
        let body = self.parts.join("\\");
        if self.fully_qualified {
            format!("\\{body}")
        } else {
            body
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod, Pow, Concat,
    Eq, NotEq, Identical, NotIdentical,
    Lt, Gt, Le, Ge, Spaceship,
    And, Or, Xor,
    BitAnd, BitOr, BitXor, Shl, Shr,
    Coalesce,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,    // -x
    Pos,    // +x
    Not,    // !x
    BitNot, // ~x
}

/// A piece of a double-quoted / heredoc string.
#[derive(Debug, Clone, PartialEq)]
pub enum TplPart {
    Lit(Vec<u8>),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrayItem {
    pub key: Option<Expr>,
    pub value: Expr,
    pub by_ref: bool,
    pub spread: bool, // ...$x
}

/// One argument at a call site.
#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub value: Expr,
    pub spread: bool,          // ...$args
    pub by_ref: bool,          // call-time &
    pub name: Option<String>,  // named argument `label: value`
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    // literals
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Template(Vec<TplPart>),
    Array(Vec<ArrayItem>),

    // names & variables
    Var(String),            // $name
    VarVar(Box<Expr>),      // $$x, ${expr}
    ConstFetch(Name),       // bareword constant or function name in value position
    MagicConst(String),     // __LINE__, __FILE__, …

    // operators
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    InstanceOf(Box<Expr>, Box<Expr>), // expr instanceof ClassOrExpr
    Assign(Box<Expr>, Box<Expr>),               // lhs = rhs
    AssignRef(Box<Expr>, Box<Expr>),            // lhs =& rhs
    AssignOp(BinOp, Box<Expr>, Box<Expr>),      // lhs op= rhs
    PreInc(Box<Expr>), PreDec(Box<Expr>),
    PostInc(Box<Expr>), PostDec(Box<Expr>),
    Ternary(Box<Expr>, Option<Box<Expr>>, Box<Expr>), // a ? b : c  (b omitted = a ?: c)

    // access / calls
    Index(Box<Expr>, Option<Box<Expr>>),        // $a[$i] or $a[] (append target)
    Prop(Box<Expr>, PropName, bool),            // $o->p  (bool = nullsafe ?->)
    StaticProp(Box<Expr>, String),              // C::$p   (class as expr/name)
    ClassConst(Box<Expr>, String),              // C::CONST  (name; "class" for ::class)
    Call(Box<Expr>, Vec<Arg>),
    MethodCall(Box<Expr>, PropName, Vec<Arg>, bool), // ->m(...)  (nullsafe?)
    StaticCall(Box<Expr>, PropName, Vec<Arg>),       // C::m(...)
    New(Box<Expr>, Vec<Arg>),                        // new C(...)  / new $cls
    NewAnon(Box<ClassDecl>, Vec<Arg>),               // new class(...) {...}

    // closures
    Closure(Box<Closure>),
    ArrowFn(Box<ArrowFn>),

    // misc constructs that are expressions in PHP
    Cast(CastType, Box<Expr>),
    Isset(Vec<Expr>),
    Empty(Box<Expr>),
    Clone(Box<Expr>),
    Print(Box<Expr>),
    Throw(Box<Expr>),
    ErrorSuppress(Box<Expr>),       // @expr
    Match(Box<Expr>, Vec<MatchArm>),
    List(Vec<Option<ArrayItem>>),   // list(...) / [...] as an assignment target
    ConstFetchExpr(Box<Expr>),      // (rare) dynamic
    FirstClassCallable(Box<Expr>),  // foo(...)
    Yield(Option<Box<Expr>>, Option<Box<Expr>>), // yield [key =>] value
    YieldFrom(Box<Expr>),                        // yield from iterable
}

/// Property/method name after `->` or `::`: usually an identifier, but can be
/// `$var`, `{expr}`, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum PropName {
    Id(String),
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastType {
    Int, Float, String, Bool, Array, Object, Unset,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    /// `None` = the `default` arm.
    pub conditions: Option<Vec<Expr>>,
    pub body: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub default: Option<Expr>,
    pub by_ref: bool,
    pub variadic: bool,
    pub type_hint: Option<String>,   // parsed, kept as text for now
    pub promote: Option<Visibility>, // constructor property promotion
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Visibility {
    Public,
    Protected,
    Private,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClosureUse {
    pub name: String,
    pub by_ref: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    pub params: Vec<Param>,
    pub uses: Vec<ClosureUse>,
    pub body: Vec<Stmt>,
    pub is_static: bool,
    pub by_ref_return: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArrowFn {
    pub params: Vec<Param>,
    pub body: Expr,
    pub is_static: bool,
}

// ---- statements --------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    InlineHtml(Vec<u8>),
    Echo(Vec<Expr>),
    Expr(Expr),
    Block(Vec<Stmt>),
    Nop,
    // goto: the jump propagates up as a control error until a statement list
    // containing the target label catches it (see Eval::exec_block)
    Goto(String),
    Label(String),

    If {
        cond: Expr,
        then: Vec<Stmt>,
        elseifs: Vec<(Expr, Vec<Stmt>)>,
        els: Option<Vec<Stmt>>,
    },
    While { cond: Expr, body: Vec<Stmt> },
    DoWhile { body: Vec<Stmt>, cond: Expr },
    For {
        init: Vec<Expr>,
        cond: Vec<Expr>,
        step: Vec<Expr>,
        body: Vec<Stmt>,
    },
    Foreach {
        array: Expr,
        key: Option<Expr>,
        value: Expr,
        by_ref: bool,
        body: Vec<Stmt>,
    },
    Switch { subject: Expr, cases: Vec<SwitchCase> },
    Break(u32),
    Continue(u32),
    Return(Option<Expr>),

    Func(FuncDecl),
    Class(ClassDecl),

    Global(Vec<String>),
    StaticVar(Vec<(String, Option<Expr>)>),
    Unset(Vec<Expr>),
    ConstDecl(Vec<(String, Expr)>),
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        catches: Vec<Catch>,
        finally: Option<Vec<Stmt>>,
    },
    Namespace { name: Option<Name>, body: Option<Vec<Stmt>> },
    Use(Vec<UseItem>),
    /// declare(...) — only strict_types is meaningful to the evaluator.
    Declare { strict_types: bool },
    /// A statement stamped with its 1-based source line (parser wraps every
    /// statement when line info is available) — powers error/warning lines.
    Marked(u32, Box<Stmt>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// `None` = `default`.
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catch {
    pub types: Vec<Name>,
    pub var: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UseItem {
    pub name: Name,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub by_ref_return: bool,
    pub ret_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClassKind {
    Class,
    Interface,
    Trait,
    Enum,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub kind: ClassKind,
    pub name: String, // empty for anonymous classes
    pub parent: Option<Name>,
    pub interfaces: Vec<Name>,
    pub is_abstract: bool,
    pub is_final: bool,
    pub enum_backing: Option<String>,
    pub consts: Vec<ClassConstDecl>,
    pub props: Vec<PropDecl>,
    pub methods: Vec<MethodDecl>,
    pub uses_traits: Vec<Name>,
    pub cases: Vec<EnumCase>, // for enums
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassConstDecl {
    pub name: String,
    pub value: Expr,
    pub visibility: Visibility,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PropDecl {
    pub name: String,
    pub default: Option<Expr>,
    pub visibility: Visibility,
    pub is_static: bool,
    pub readonly: bool,
    pub is_final: bool,
    pub type_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MethodDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Option<Vec<Stmt>>, // None = abstract / interface method
    pub visibility: Visibility,
    pub is_static: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub by_ref_return: bool,
    pub ret_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumCase {
    pub name: String,
    pub value: Option<Expr>,
}
