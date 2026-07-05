//! `lang` — the v2 engine front end, designed as a proper language implementation:
//! a byte-level lexer producing a token stream, then (next) a recursive-descent
//! parser producing an owned AST, then (after that) a tree-walking evaluator.
//!
//! This lives entirely alongside the legacy single-pass engine in `lib.rs`. It is
//! NOT wired into `run()` until it reaches parity on the test suite — so the
//! public scoreboard keeps reflecting the shipping engine while this is built.
//!
//! Design decisions locked at the front (they shape everything downstream):
//!   * Source is `&[u8]`; strings are `Vec<u8>` — PHP strings are byte arrays.
//!   * The lexer owns PHP's HTML/code mode switching (`<?php … ?>` and inline
//!     HTML) so the parser never sees raw template text.
//!   * The AST is owned and built once; loop/function bodies are tree nodes, not
//!     source offsets, so nothing is ever re-parsed.

pub mod ast;
pub mod builtin_sigs;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod token;
pub mod value;
pub mod vm;
pub mod xml;
