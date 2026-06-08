<p align="center">
  <img src="assets/banner.webp" alt="Phargo" width="100%">
</p>

# Phargo

**PHP + Cargo.** A from-scratch, **memory-safe PHP engine written in Rust** — built the same way the Bun team rewrote Bun in Rust: drive an AI with the original project's **own test suite as the oracle**, and watch the pass-rate climb *in public*.

## North star: run WordPress in the browser

[WordPress Playground](https://github.com/WordPress/wordpress-playground) runs WordPress entirely in your browser by compiling the C PHP interpreter to WebAssembly via Emscripten. Those WASM builds are big, and the Playground team has been [actively fighting binary size](https://make.wordpress.org/playground/2026/03/11/how-wordpress-playground-cut-php-wasm-binary-sizes-by-122-mb/). Rust → WASM is leaner and memory-safe by construction. So the goal:

> **A PHP engine in Rust that passes PHP's own `.phpt` tests and boots WordPress inside Playground — at a fraction of the WASM size.**

## Honesty

I'm not (yet) a Rust or PHP-internals expert. This is an open experiment in AI-assisted engineering. The wins *and* the failures are in the open, and the scoreboard never lies — see [PROGRESS.md](PROGRESS.md).

## How it works

- `src/lib.rs` — the engine. `run(php_source) -> printed output`.
- `src/main.rs` — the scoreboard. Runs every `.phpt` in `tests/phpt/`, compares output to each test's expected section, writes `PROGRESS.md`.
- `tests/phpt/` — the oracle. Real PHP `.phpt` format: each file bundles the code *and* its expected output, so we can score against it with no PHP runtime installed.

## Run it

```sh
cargo run
```

## Status

**v38: 1561 / 21862 upstream php-src tests passing (7.14%).** The core language is largely there — variables, the full operator/type-juggling model, control flow, functions/closures, arrays + ~200 builtins, classes/interfaces/traits/enums, exceptions, constructor promotion, attributes, `foreach` over user `Iterator`/`IteratorAggregate`, a from-scratch regex engine (`preg_*`), `include`/`require`/`eval`, output buffering, filesystem + path functions, file streams (`fopen` family + `STDIN`/`STDOUT`/`STDERR`), UTF-8 `mbstring` basics, and `serialize`/`unserialize`. The climb continues — see [PROGRESS.md](PROGRESS.md).
