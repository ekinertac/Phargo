//! The token set for the v2 front end.
//!
//! A `Token` is a `Kind` plus its source span (byte offsets, for diagnostics).
//! Keywords are NOT distinguished here — they arrive as `Ident` and the parser
//! matches them case-insensitively, which is how PHP treats them.

/// Byte span into the original source: `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Kind,
    pub span: Span,
}

/// A piece of a double-quoted string / heredoc body: either literal bytes
/// (escapes already resolved) or a span of source holding an embedded
/// expression (`$x`, `$a->b`, `$a[0]`, `{$expr}`) to be parsed later.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Lit(Vec<u8>),
    /// Raw source bytes of an interpolated expression, e.g. `$user->name` or
    /// the inside of `{ … }`. The parser re-lexes/parses this in expression mode.
    Expr(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    // ---- template / structure ------------------------------------------
    InlineHtml(Vec<u8>), // raw bytes outside of `<?php … ?>`
    OpenTag,             // <?php  (or short <?)
    OpenEcho,            // <?=
    CloseTag,            // ?>
    Eof,

    // ---- literals -------------------------------------------------------
    Int(i64),
    Float(f64),
    /// Single-quoted string: escapes resolved, no interpolation.
    Str(Vec<u8>),
    /// Double-quoted / heredoc string: a sequence of literal and expr parts.
    Template(Vec<StrPart>),

    Variable(String), // `$name` → "name"
    Ident(String),    // identifiers + keywords (original case preserved)

    // ---- grouping / punctuation ----------------------------------------
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Semi,      // ;
    Comma,     // ,
    Arrow,     // ->
    NullArrow, // ?->
    DoubleColon, // ::
    Colon,     // :
    FatArrow,  // =>
    Ellipsis,  // ...
    Question,  // ?
    At,        // @
    Backslash, // \
    AttrStart, // #[  (attribute opener)
    Dollar,    // bare $ (variable variables: `$$x`, `${…}`)

    // ---- operators ------------------------------------------------------
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Pow, // **
    Dot, // string concat
    Assign,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    PowEq,
    DotEq,
    AndEq,    // &=
    OrEq,     // |=
    XorEq,    // ^=
    ShlEq,    // <<=
    ShrEq,    // >>=
    CoalesceEq, // ??=
    Inc,      // ++
    Dec,      // --
    EqEq,     // ==
    Identical, // ===
    NotEq,    // != and <>
    NotIdentical, // !==
    Lt,
    Gt,
    Le,
    Ge,
    Spaceship, // <=>
    AndAnd,    // &&
    OrOr,      // ||
    Not,       // !
    Amp,       // &  (bitwise / reference)
    Pipe,      // |
    PipeArrow, // |>  (PHP 8.5 pipe operator)
    Caret,     // ^
    Tilde,     // ~
    Shl,       // <<
    Shr,       // >>
    Coalesce,  // ??
}
