# Phargo — agent working guide

Phargo is a **from-scratch, memory-safe PHP engine in Rust**, driven by PHP's own
`.phpt` test suite as the oracle. The goal is to watch the pass rate climb in
public (build-in-public; north star = run WordPress in WordPress Playground via
Rust→WASM). You are continuing that climb.

## The number that matters

The scoreboard runs every upstream php-src `.phpt` in `vendor/php-src/` and reports
a **corpus-only** pass rate — tests we did **not** write. Curated smoke tests in
`tests/phpt/` are reported separately and do **not** count.

- Current: **~3007 / 21796 gradeable (~13.8%)**. See `PROGRESS.md` (auto-generated).
- Realistic ceiling is ~40–45% — the rest of the corpus is out-of-scope C
  extensions (GD, SOAP, curl, PDO/mysqli/SQLite3, intl/ICU, FFI, Fiber, sockets,
  gmp, zlib, ZipArchive, Phar…). **Do not try to implement those.**

## Architecture

- `src/lang/` is the engine: `token.rs`/`lexer.rs` → `ast.rs`/`parser.rs` →
  `value.rs`/`eval.rs` (tree-walking evaluator). `eval.rs` is large (~8k lines):
  the builtin dispatcher (`fn builtin`) + a PHP `PRELUDE` string holding the class
  library (Exception hierarchy, SPL, DOM, SimpleXML, Reflection, DateTime…).
- `src/lib.rs` — 50-ish-line crate root: `run`/`run_with_path` + a **capped global
  allocator (6 GiB ceiling)** so no test can exhaust host RAM. Also `regex.rs`,
  `datetime.rs`, `hash.rs` — shared subsystems reused by the evaluator.
- `src/main.rs` — the scoreboard. Runs on a 1 GB-stack worker thread; each test
  gets a fresh `Eval`. Normalizes CRLF before comparing (the corpus is checked out
  with CRLF on Windows — that was a huge hidden bug).
- Strings are `Vec<u8>` (byte-correct). PHP objects are `Rc<RefCell<Obj>>`.

## The workflow (how every rung goes)

1. **Analyze** to pick the next rung — never guess. Tools (all `examples/*.rs`):
   - `cargo run --release --example suiteanalyze` — whole-corpus failure categorizer
     (primary blocker per test, by area). `-- close` samples near-misses;
     `-- mismatch` isn't its mode — read the source for arg modes.
   - `cargo run --release --example errscan` — corpus-wide error-message tally;
     `-- "PATHS:<substr>"` lists files hitting an error (slow, whole corpus).
   - `cargo run --release --example zendscan` — Zend-focused PASS/MISMATCH/PARSE/RUN
     split with samples; `-- mismatch`, `-- "ERRFILE:<substr>"` modes.
2. **Implement** the highest-leverage *achievable* cluster (ignore C extensions).
3. **Verify** with a tiny throwaway `examples/xxx_tmp.rs` calling `phargo::run`.
4. **Scoreboard**: `cargo run --release` (~7 min). Watch it with a background
   Monitor (poll `target/current_test.txt` breadcrumb + process memory) so a
   pathological test can't hang unnoticed.
5. **Commit + push** only if the number went up with no regressions.

## Hard-won rules — READ THESE

- **Perf: never clone a container in a hot path.** `value.deref()` and "evaluate
  the argument" both *clone* — inside a loop over a growing array that's O(n²).
  Peek the slot type without cloning. This caused multiple hangs (bug40261).
- **Resource guards are mandatory.** The corpus has accidental memory/CPU bombs.
  Guards exist (STEP_LIMIT, MAX_STR/ARRAY_NODES/OUTPUT, per-generator caps, the
  6 GiB allocator). When a run hangs, check the breadcrumb and add a cap — don't
  lower STEP_LIMIT.
- **Bash and PowerShell share ONE working directory.** Never `cd` into
  `vendor/php-src` in Bash (it's a nested git repo full of corpus junk) — a later
  `git commit` will run in the wrong place. Read corpus files by absolute path;
  `git rev-parse --show-toplevel` if git acts weird. Check `git status --porcelain`
  before every `git add` — never `git add -A`.
- **Methods are case-insensitive**, so a prelude `saveXml` that calls `saveXML`
  is infinite recursion. Don't redefine a method just for letter case.
- **Commit messages: single-quoted** (backticks = shell substitution). End with
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`. Work on `master`.
- The engine emulates **PHP-on-Unix**; the corpus was written for Linux CI. (But
  forcing Unix path *constants* on Windows regressed — reverted. Leave as-is.)
- After a scoreboard run, `git status` may show a stray junk file swept from a
  corpus test — never commit it.

## Where the leverage is now

The pure-PHP-builtins vein is largely mined out. Biggest remaining *achievable*
targets (per `suiteanalyze`):
- **`ext/date` timezones** — the larger unbuilt half of date (named zones/offsets/
  DST/abbreviations). Hundreds of tests but touches all date formatting → run the
  scoreboard carefully for regressions. `strtotime` parsing is already expanded.
- **DOM convenience methods** (`loadHTML`, `remove`/`append`/`before`/`after`) —
  cheap on the existing tree, but ext/dom is capped by exact-serialization matching.
- **Parser clusters** — small, whole-test wins (e.g. lexer "unterminated string").
- Avoid the **Uri/Url 8.5 API**: it needs IDN/punycode + exact var_dump of internal
  objects (PHP uses C libs); much harder than it looks.

## Keep the narrative

`DEVLOG.md` is a public build-in-public journal — append a newest-first entry per
meaningful rung/session (the war stories, not just the number). It's distinct from
`PROGRESS.md` (auto-generated) and commit messages (terse).
