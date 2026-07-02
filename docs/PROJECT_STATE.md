# Phargo — project state & history

Portable working memory for continuing the project on any machine. Pairs with
`CLAUDE.md` (workflow + rules) at the repo root. This file is the durable
project context: mission, milestone arc, hard-won lessons, current state, and
next targets. Update it at meaningful milestones.

## Mission

Phargo (a coined name — not "PHP + Cargo") is a from-scratch, memory-safe PHP
engine in Rust, built AI-assisted using the **Bun-rewrite methodology**: drive the
AI with the original project's own test suite as the oracle and watch the pass rate
climb in public.

- **Immediate metric:** corpus-only `.phpt` pass rate over upstream php-src.
- **Real goal:** build-in-public attention (radical honesty is the strategy).
- **North star:** run WordPress in [WordPress Playground](https://github.com/WordPress/wordpress-playground)
  via a Rust→**WASM** engine smaller than Playground's Emscripten build. Playground
  also shrinks scope: SQLite (pdo_sqlite), not MySQL; virtual FS, not full SAPI.
- **Public:** https://github.com/ekinertac/Phargo — `origin`, branch `master`.
  Full autonomy: pick the next rung, implement, verify, commit + push, repeat.

## Working conventions

- Public face = `README.md` (`## Status` line) + auto-generated `PROGRESS.md` +
  narrative `DEVLOG.md`. Keep all three current.
- Commit messages **single-quoted** (backticks = shell substitution), ending with
  `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
- Analyzer-driven rung selection (`examples/suiteanalyze.rs` etc.) — don't guess.
- See `CLAUDE.md` for the full rule list (perf traps, resource guards, the
  shared-CWD git trap, etc.).

## The big bet — "Path B" (done)

The original engine was a single-pass streaming interpreter with **no AST** — loops
re-parsed their body every iteration, functions re-parsed every call. That was the
ceiling (speed wall + generators nearly impossible: nowhere to suspend). We rebuilt
the core from scratch as a **proper language implementation** (lexer → parser →
owned AST → tree-walking evaluator) with byte-correct `Vec<u8>` strings and correct
precedence, *in parallel* with the old engine, and cut over only once it surpassed
the shipping number on the same suite. Then deleted the legacy engine (`lib.rs`
11,271 → ~52 lines). Everything since sits on that AST.

## Milestone arc

- Legacy single-pass engine peaked ~**1981** passing.
- v2 (Path B) reached parity, then surpassed it; legacy retired.
- **2026-06-14 → 07-02 grind: 2176 → 3007** (9.98% → 13.8% of gradeable),
  crossing 10/11/12/13% and 3000 tests. Key rungs in order:
  generators (eager `yield`); the **6 GiB capped allocator + generator node caps**
  after a generator OOM hard-restarted the dev machine; file streams (modeled as
  `__Stream` objects, no `Value::Resource` needed); the **8.5 pipe operator `|>`**
  (renumbered the binding-power ladder); real enums (`cases`/`from`/`tryFrom` +
  singleton identity); **real references** (`Value::Ref` — `=&`/`use(&$x)`/by-ref,
  the invariant being "refs only live directly under variable names"); **DOMDocument
  + a from-scratch XML parser** (`src/lang/xml.rs`, hybrid: Rust parser + PHP DOM
  classes) + `__get`; **SimpleXML** (same tree); Reflection (params, return types,
  ReflectionProperty); superglobals + sessions; method-visibility enforcement +
  `Throwable` fix; typed class constants; Closure methods; and — the discovery that
  reframed everything — the **CRLF harness bug** (corpus checked out with CRLF on
  Windows; the scoreboard compared byte-for-byte and was failing ~every multi-line
  test on line endings alone; PHP's run-tests normalizes — we now do too). Then an
  analysis-driven phase: object instance ids, pure-PHP builtins batches
  (`array_chunk`/`pack`/`array_multisort`/the entire html/url encoding family that
  was missing), SPL iterators, ReflectionProperty.

## Recurring lessons (the war stories)

- **Measure your measurement.** The CRLF bug silently suppressed a whole class of
  passes; a one-line normalization unlocked hundreds. Also: build analyzers, don't
  guess which rung to do next.
- **Never clone a container in a hot loop.** `value.deref()` and "eval the argument"
  both clone; inside a loop over a growing array that's O(n²) and hangs
  (bug40261 went from hanging → 265 ms). Peek the type without cloning.
- **The corpus contains accidental resource bombs.** Guards are non-negotiable:
  STEP_LIMIT, MAX_STR/ARRAY_NODES/OUTPUT, generator node caps, the 6 GiB allocator.
  A generator OOM once restarted the whole machine — that's why the ceiling exists.
- **Boring fundamentals hide the points.** The marquee features were all present;
  the mid-game gains were in `unset($arr[$k])` (a total no-op), `__FUNCTION__`
  returning empty, `catch(\Throwable)` matching nothing, `(string)$obj` ignoring
  `__toString`, hardcoded `object(X)#1` ids, `var_dump` float precision. The corpus
  finds these for you.
- **Bash + PowerShell share one CWD** → a Bash `cd` into `vendor/php-src` (a nested
  git repo) once made a `git commit` run in the wrong place. Read corpus by absolute
  path; check `git status --porcelain` before every add.
- **`git add -A` once swept in a corpus-generated junk file.** Always inspect first.

## Current state (2026-07-02)

- **~3007 / 21796 gradeable (~13.8%).** From the whole-corpus analysis: only ~6
  panics in 13.5k tests (engine is robust); failures are missing features.
- **Realistic ceiling ~40–45%** — the rest is out-of-scope C extensions.
- The **pure-PHP-builtins vein is largely mined out**; remaining MISSING_FN are C
  extensions + `crypt`.

## Next targets (by leverage, achievable only)

1. **`ext/date` timezones** — named zones / offsets / DST / abbreviations. The
   larger unbuilt half of date; hundreds of tests. Touches all date formatting →
   watch the scoreboard for regressions. `strtotime` parsing is already expanded.
2. **DOM convenience methods** (`loadHTML`, `remove`/`append`/`before`/`after`) —
   cheap on the existing tree, but ext/dom is capped by exact-serialization matching.
3. **Parser clusters** — small whole-test wins (e.g. lexer "unterminated string").

**Avoid:** the Uri/Url 8.5 API (needs IDN/punycode + exact var_dump of internal
objects — PHP uses C libs lexbor/uriparser; much harder than it looks) and all the
C-extension subsystems.
