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
- **2026-07-02/03 session: 3007 → 3150** (13.8% → 14.3%), three rungs:
  (1) **named arguments** (parsed but silently positional — merged onto param
  slots at every call shape) + **foreach-by-ref** (real `Value::Ref` cells in
  array elements, promoted/demoted in place, `&` markers via `Rc::strong_count`)
  + `*_exists` kind checks; (2) **late static binding** (`static::` had been a
  synonym for `self::`; forwarding-call semantics), const initializers scoped to
  declaring class, **undefined constants throw** per PHP 8 (after filling LC_*/
  EXTR_*/PHP_QUERY_*/STREAM_FILTER_*), real `error_reporting()` state, E_ALL=30719;
  (3) **named timezones** — from-scratch TZif reader (`src/tz.rs`) over the host's
  /usr/share/zoneinfo, wired through date()/mktime()/strtotime()/DateTime/
  DateTimeZone (per-object zones, wall-clock add/sub/modify, getTransitions),
  `--INI--` date.timezone honored by the runner, DatePeriod + DATE_* constants,
  DateTime var_dump synthesis — and the discovery that **`clone` was never
  implemented** (evaluated to NULL engine-wide; DateTimeImmutable was silently
  broken everywhere). ext/date went 77 → 155.
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

## Current state (2026-07-04, end of session)

- **3436 / 21970 gradeable (15.6%).** Fourteen batches on 07-02→04: 3007 → 3436
  (+429). Stack traces are PHP-exact now (frame stack at all call shapes,
  snapshot at exception construction, prelude frames filtered). Line numbers are real now (per-token line table → Stmt::Marked →
  cur_line; exceptions capture file+line at construction; __LINE__ works). Batches 8–12 were the error-semantics vein: parameter/return type
  enforcement (weak + strict_types), typed + readonly properties, arithmetic
  TypeErrors/DivisionByZeroError, and **runtime warnings** (undefined
  variable/array key, property-on-scalar, foreach-arg) with the full silence
  map (isset/empty/??/@; by-ref out-params on all call shapes — see the
  builtin table in eval_call; nested index-assign reads; [&$x]; =&; by-ref
  returns; prelude bodies; set_error_handler; eager generators). Warn policy:
  only where the engine is SURE it's user error — null/array prop-bases stay
  silent because our nulls are often unimplemented-API artifacts.
- Known accepted losses: Zend autoload/bug78868 (static-init + autoload
  corner), one readonly init-once approximation test.
- `static` vars, clone, $$x, spl_autoload were all silently-missing features
  found this session; suspect more remain — grep eval.rs for "not yet
  implemented" and check the catch-all arms when output is inexplicably NULL.
- **Realistic ceiling ~40–45%** — the rest is out-of-scope C extensions.
- Landed this session: named args; foreach-by-ref (real Ref cells in elements,
  live-append iteration with cursor+cap guard); LSB; undefined-const Error;
  timezones (TZif reader, createFromFormat, DatePeriod, identifiers list,
  strtotime offsets/abbrevs); `clone` (was missing); variable variables `$$x`
  (were missing); by-ref callback params (array_walk mutates); autoloading
  (spl_autoload_register was a no-op); shutdown fns after fatals; SPL
  IT_MODE/EXTR plumbing; DOM ChildNode API; **harness now trims both sides**
  like run-tests.php (that alone was +84).
- **Orchestration mode works:** well-specified rungs go to Sonnet subagents
  (exact file scope + expected outputs + regression baselines to hold);
  analysis/architecture/commit-verdicts stay with the orchestrator. Five agent
  tasks, all first-try clean. See memory note `phargo-subagent-orchestration`.
- Analysis tools: `suiteanalyze [close]`, `zendscan`, `errscan`,
  `emptyscan` (buckets empty-output failures), `examples/xxx_run.rs <substr>`
  (quick subset PASS/FAIL, EXPECT-only).

## Next targets (by leverage, achievable only)

1. **Fatal-message wording sweep** — with file/line/trace exact, remaining
   fatal tests fail on message TEXT. Build an analyzer diffing expected vs
   actual fatal lines corpus-wide, fix the top wordings (e.g. "Call to
   undefined function x()" — we say "unknown function"), batch them.
   Note: engine errors like RunError("unknown function") should become real
   Error throws with PHP wording.
2. **Tokenizer ext** (`token_get_all`/`PhpToken`, ~55 tests) — raw
   whitespace-preserving scan + PHP's numeric token-ID table. A full session,
   mechanical once designed.
3. **DOM `loadHTML` + saveHTML** (~27+ tests) — lenient HTML parse mode over
   the existing XML tree + HTML serializer.
4. **ReflectionClass::newLazyGhost/newLazyProxy** (~55 tests, PHP 8.4).
5. **Warnings/notices infrastructure** — many EXPECT tests include
   `Warning: ...` lines we never emit (undefined var/index notices, etc.).
   Like type enforcement, "too permissive" fails tests. Needs the same
   careful measure-first approach.
6. Keep re-running `suiteanalyze -- close` after each batch — it found named
   args, LSB, clone, and $$x.

**Avoid:** the Uri/Url 8.5 API (needs IDN/punycode + exact var_dump of internal
objects — PHP uses C libs lexbor/uriparser; much harder than it looks) and all the
C-extension subsystems.
