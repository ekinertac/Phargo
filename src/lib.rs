//! Phargo — a from-scratch, memory-safe PHP engine written in Rust.
//!
//! The engine is `lang::` — a proper language implementation: a byte-level
//! lexer → recursive-descent parser → owned AST → tree-walking evaluator. It
//! replaced the original single-pass streaming interpreter (the "Path B"
//! rewrite) after surpassing it on the php-src corpus.
//!
//! Public entry points: [`run`] / [`run_with_path`]. This crate root keeps only
//! the shared, value-independent subsystems the evaluator reuses ([`mod@regex`],
//! [`mod@datetime`]) plus the public error type.

use std::path::PathBuf;

// Shared subsystems reused by the v2 evaluator via `crate::` (char/byte/int —
// no engine-value dependency): the from-scratch regex VM and civil-calendar
// date/time math.
mod datetime;
mod regex;
pub(crate) use datetime::*;
pub(crate) use regex::*;

// The engine.
pub mod lang;

#[derive(Debug)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for EngineError {}

pub(crate) type R<T> = Result<T, EngineError>;

/// Execute PHP `source` and return everything it would have printed to stdout.
pub fn run(source: &str) -> R<String> {
    run_with_path(source, None)
}

/// Like [`run`], but records the script's file path so `__FILE__`/`__DIR__` and
/// relative `include`/`require` resolve against it.
pub fn run_with_path(source: &str, path: Option<PathBuf>) -> R<String> {
    let toks = lang::lexer::Lexer::tokenize(source.as_bytes())
        .map_err(|e| EngineError(format!("Parse error: {}", e.msg)))?;
    let ast = lang::parser::Parser::parse(toks)
        .map_err(|e| EngineError(format!("Parse error: {}", e.msg)))?;
    let out = lang::eval::Eval::run_with_path(&ast, path).map_err(|e| EngineError(e.0))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
