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

// ---------------------------------------------------------------------------
// Process-wide heap ceiling — the last line of defence.
//
// The evaluator has per-test resource guards (string/array/range/generator
// caps), but those only catch the bombs we've anticipated. A novel runaway
// allocation must NEVER be allowed to exhaust machine RAM — a scoreboard run
// that eats every byte of physical memory + pagefile can hard-restart the host.
// So we cap the whole process at a generous ceiling, far above any legitimate
// peak (normal runs stay well under 1 GB). Past the ceiling, `alloc` returns
// null and Rust aborts THIS process — losing one run, never the machine.
mod capped_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const HEAP_CEILING: usize = 6 * 1024 * 1024 * 1024; // 6 GiB
    static HEAP_USED: AtomicUsize = AtomicUsize::new(0);

    pub struct Capped;

    unsafe impl GlobalAlloc for Capped {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let sz = layout.size();
            if HEAP_USED.fetch_add(sz, Ordering::Relaxed) + sz > HEAP_CEILING {
                HEAP_USED.fetch_sub(sz, Ordering::Relaxed);
                return std::ptr::null_mut();
            }
            let p = System.alloc(layout);
            if p.is_null() {
                HEAP_USED.fetch_sub(sz, Ordering::Relaxed);
            }
            p
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            System.dealloc(ptr, layout);
            HEAP_USED.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let old = layout.size();
            if new_size > old {
                let delta = new_size - old;
                if HEAP_USED.fetch_add(delta, Ordering::Relaxed) + delta > HEAP_CEILING {
                    HEAP_USED.fetch_sub(delta, Ordering::Relaxed);
                    return std::ptr::null_mut();
                }
                let p = System.realloc(ptr, layout, new_size);
                if p.is_null() {
                    HEAP_USED.fetch_sub(delta, Ordering::Relaxed);
                }
                p
            } else {
                let p = System.realloc(ptr, layout, new_size);
                if !p.is_null() {
                    HEAP_USED.fetch_sub(old - new_size, Ordering::Relaxed);
                }
                p
            }
        }
    }
}

#[global_allocator]
static GLOBAL: capped_alloc::Capped = capped_alloc::Capped;

// Shared subsystems reused by the v2 evaluator via `crate::` (char/byte/int —
// no engine-value dependency): the from-scratch regex VM and civil-calendar
// date/time math.
mod bcmath;
mod datetime;
mod hash;
mod regex;
pub(crate) mod tz;
pub(crate) use bcmath as bc;
pub(crate) use datetime::*;
pub(crate) use hash::*;
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

/// Set the default timezone (IANA name) the next run starts with — the harness
/// hook for a .phpt `--INI--` `date.timezone=` line. `None` resets to UTC.
pub fn set_default_timezone(tz: Option<String>) {
    tz::set_default_tz(tz);
}

/// Like [`run`], but records the script's file path so `__FILE__`/`__DIR__` and
/// relative `include`/`require` resolve against it.
pub fn run_with_path(source: &str, path: Option<PathBuf>) -> R<String> {
    let (toks, lines) = lang::lexer::Lexer::tokenize_lines(source.as_bytes())
        .map_err(|e| EngineError(format!("Parse error: {}", e.msg)))?;
    let ast = lang::parser::Parser::parse_with_lines(toks, lines)
        .map_err(|e| EngineError(format!("Parse error: {}", e.msg)))?;
    let out = lang::eval::Eval::run_with_path(&ast, path).map_err(|e| EngineError(e.0))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
