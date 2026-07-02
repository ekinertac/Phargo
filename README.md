<p align="center">
  <img src="assets/banner.webp" alt="Phargo" width="100%">
</p>

# Phargo

A from-scratch, **memory-safe PHP engine written in Rust** — built the same way the Bun team rewrote Bun in Rust: drive an AI with the original project's **own test suite as the oracle**, and watch the pass-rate climb *in public*.

## North star: run WordPress in the browser

[WordPress Playground](https://github.com/WordPress/wordpress-playground) runs WordPress entirely in your browser by compiling the C PHP interpreter to WebAssembly via Emscripten. Those WASM builds are big, and the Playground team has been [actively fighting binary size](https://make.wordpress.org/playground/2026/03/11/how-wordpress-playground-cut-php-wasm-binary-sizes-by-122-mb/). Rust → WASM is leaner and memory-safe by construction. So the goal:

> **A PHP engine in Rust that passes PHP's own `.phpt` tests and boots WordPress inside Playground — at a fraction of the WASM size.**

## Honesty

I'm not (yet) a Rust or PHP-internals expert. This is an open experiment in AI-assisted engineering. The wins *and* the failures are in the open, and the scoreboard never lies — see [PROGRESS.md](PROGRESS.md).

## How it works

- `src/lang/` — **the engine**: a proper language implementation — byte-level lexer (`lexer.rs`/`token.rs`) → recursive-descent parser → owned AST (`ast.rs`/`parser.rs`) → tree-walking evaluator (`value.rs`/`eval.rs`). `run(php_source) -> printed output`.
- `src/lib.rs` — a 52-line crate root: the public `run`/`run_with_path` entry points + the shared subsystems the evaluator reuses (`regex.rs`, `datetime.rs`).
- `src/main.rs` — the scoreboard. Runs every `.phpt` in `vendor/php-src/`, compares output to each test's expected section, writes `PROGRESS.md`.
- `tests/phpt/` — curated smoke tests. Real PHP `.phpt` format: each file bundles the code *and* its expected output, so we can score against it with no PHP runtime installed.

> **Note:** the engine was rebuilt from scratch (the original single-pass interpreter is retired). The from-scratch lexer→parser→AST→evaluator now passes *more* corpus tests than the engine it replaced — see the git history.

## Run it

```sh
cargo run
```

## Status

**v2 engine: 3007 / 21862 upstream php-src tests passing (13.8% of gradeable).** Rebuilt from scratch as a real lexer→parser→AST→evaluator, the engine passes far more corpus tests than the original single-pass interpreter it replaced (1,981), with **byte-correct strings** (`Vec<u8>`), correct operator precedence, and an AST (parse once, no re-parsing). The core language is there — variables + real references (`&$x`/`=&`/`use(&$x)`), the full operator/type-juggling model, control flow, generators (`yield`), functions/closures (variadics, argument unpacking, named args, `Closure::bind`/`bindTo`/`fromCallable`), arrays + ~300 builtins, classes/interfaces/traits/enums (+ typed constants, asymmetric visibility, method-visibility enforcement), exceptions, attributes, the 8.5 pipe operator `|>`, dynamic dispatch, Reflection, the SPL iterator/container family, `ArrayAccess`, superglobals + sessions, date/time (`DateTime`/`strtotime`), a from-scratch regex engine (`preg_*`), a from-scratch **XML parser powering DOMDocument + SimpleXML**, file streams (`fopen`), `include`/`require`/`eval`, `serialize`/`unserialize` (+ `__serialize`), and PHP-faithful `var_dump`/`var_export`. The remaining failures are overwhelmingly **out-of-scope C extensions** (GD, SOAP, curl, PDO/mysqli, intl, FFI, Fiber…). The climb continues — see [PROGRESS.md](PROGRESS.md).
