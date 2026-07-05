# Phargo — project state & history

Portable working memory for continuing the project on any machine. Pairs with
`CLAUDE.md` (workflow + rules) at the repo root and `ROADMAP.md` (the phased
plan to the north star; Phase 0 — the WordPress-progress oracle — is next). This file is the durable
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

- **3630 / 21970 gradeable (16.5%).** Twenty-three batches through 07-07: 3007 → 3630
  (+576). Since batch 14: engine aborts → catchable PHP Errors (b15),
  namespaces v1 with FQ registration + use imports + global fallback (b16),
  error handlers invoked + trigger_error (b17), from-scratch bcmath in
  src/bcmath.rs + ~70 constants + analyzer reads new fatal format (b18),
  BcMath\Number w/ operator overloading + trim charlists — trim had ignored
  charlists forever, fifth hollow builtin found (b19). Line numbers are real now (per-token line table → Stmt::Marked →
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

The easy veins are mined out (b20-23 delivered +12/+9/+25/+1). What's left
is feature-priced:
1. **Tokenizer ext** (~55) — raw whitespace-preserving scan + PHP numeric
   token-id table. Mechanical once designed; a full session.
2. **ReflectionClass lazy objects** (~55, PHP 8.4) — needs
   initialize-on-access hooks in the property model. Design-heavy.
3. **Uri\WhatWg\Url / Uri\Rfc3986** (~200 mentions) — previously marked
   avoid (IDN/punycode + exact var_dump); a subset might pay now that
   namespaces/exceptions/var_dump synthesis exist. Scope carefully first.
4. **MISMATCH_OTHER grind** — `suiteanalyze -- other` histogram: array-count
   drifts (109), bool flips (30), int drifts (33). Singles, ~1-3 tests each.
5. **Perf follow-up**: foreach_016 + phpdbg match grind minutes in the
   live-append/step-limit path.
**WP oracle status (2026-07-04): BOOTSTRAP COMPLETES — Phase 1 gate hit.**
The full wp-load → wp-config → wp-settings chain (plugins, SQLite drop-in,
Requests autoloader, i18n, hooks, widgets) runs to the end under
`define('WP_INSTALLING', true)`; without it WP exits cleanly on the
wp_not_installed() install redirect (empty DB — correct behavior). The
"lexer spin" was static property DEFAULTS never initializing (the eighth
silent hole). The rest of the blocker chain, in order: function_exists
blind to ~440 of our builtins (polyfill shadowing), global-by-value
(require_wp_db's isset($wpdb)), str_replace's by-ref $count
(_deep_replace), `__()` eaten by the magic-constant prefix test, \xNN in
regex classes, definition-site __DIR__/ns/use-alias context for calls
(fixed stack-trace file misattribution engine-wide), goto (HTML-API state
machine), and honest gaps: strtr, parse_url, glob, substr_replace, uniqid,
array_intersect_key/array_diff_key, alt-syntax switch templates.
**Next: run the WP installer (wp-admin/install.php?step=2 equivalent) to
populate the SQLite DB, then render the front page.**

**UPDATE (2026-07-04, night): WordPress SERVES A PAGE.** `wp_install()`
completes (admin user #1, 100 options, 3 posts in SQLite — the corrupting
bug was preg_split ignoring PREG_SPLIT_DELIM_CAPTURE inside wpdb::prepare),
and the full front-controller lifecycle renders a 23 KB HTML page:
`<title>Phargo Test Site</title>`, block-library CSS, twentytwentyfive
template parts, clean `</html>`. wpscan now runs the real index.php path
and saves the response to target/wp_page.html.

**UPDATE (2026-07-06/07, Phase 2 in progress):** the bytecode VM exists
behind PHARGO_ENGINE=vm (src/lang/vm.rs + run_chunk in eval.rs).
Mixed-mode: bodies compile to stack-machine chunks or fall back to the
walker per-body; chunk cache pinned by owning declaration Rcs (pointer
reuse burned us once). Subset so far: slots/consts/jumps, in-place
array+concat ops, int fast paths, $this props (incl. nested paths),
method/static/new calls (non-lvalue args only — by-ref safety), array
literals, isset family, switch, class constants. Verified: A/B harnesses
byte-identical, WP front page byte-identical under the flag, default
scoreboard untouched (3844), VM-mode gap 10 tests (task: close to 0).
Benchmarks: docs/BENCHMARKS.md auto-generated (bench -- cmp): micro
1-3x of PHP 8.5, WP page 55x (7.1s vs 126ms — the Phase 2 number).
**Profile-proven next lever: per-call overhead — VM-native chunk-to-chunk
calls binding args directly into callee slots, skipping the scope
HashMap/def-ctx/frame machinery for compiled callees. Also: compile rate
614/2030 WP bodies; next bail causes worth a census after calls land.**

**UPDATE (2026-07-08, Phase 2 continues):** fast calls + call-site memos
landed; then six feature families: `global`, static props, `instanceof`,
magic constants (definition-site, derived from the chunk's owner +
def-ctx map — NOT evaluator state at compile time), `unset`, and
`static` vars. Statics introduced the aliasing model: slots can hold the
walker's `Value::Ref` cells and every slot op writes THROUGH them —
`global` then became true Ref binding (copy-sync machinery deleted).
Static initializers run ONCE (StaticCheck/StaticInit op pair — PHP 8.3
allows side effects there). Collateral walker bugs found by
engine-racing: `.=` fast path severed Ref cells; VM fast paths skipped
return-type coercion; IssetIndex missed offsetExists. Walker 3867 / VM
3873 (17.6%), WP byte-identical, A/B 16/16. WP bail census 4019 → ~1200
events (top rest: closures 469, try 322, assign-ref 76). Page only
7.6s → 7.3s — **profiler says the wall is clone pressure now:
`Value::deref` deep-clones array elements per read (3.9k/5k samples in
Vec::clone). Next rung: copy-on-write `Arr` (Rc<ArrData> + make_mut,
~110 direct `.entries` sites) — lifts BOTH engines and probably some
OOM/step-limit corpus fails too.**

**UPDATE (2026-07-08, later): COW LANDED — WP FRONT PAGE 772 ms, GOAL
HIT.** `Arr` = `Rc<ArrData>` + `Rc::make_mut` on all mutators; clone =
Rc bump; `entries` privatized behind entries()/entries_mut()/
into_entries()/take_entries() (+ pos()/set_pos). Page 7.3 s → 0.77 s
(walker) / 0.82 s (VM); micro suite 1065 → 813 ms walker-side. Walker
now edges the VM on the page — top-level slot sync is the VM's visible
remaining overhead. `$a[] = $a` still snapshots (pending value holds a
handle → make_mut splits before push; no Rc cycles).

**UPDATE (2026-07-05): "Hello world!" RENDERS.** Named SQL parameters now
bind via rusqlite raw_bind_parameter/parameter_index (the PDO prelude's
bindValue had cast ":param0" to int 0; execute() flattened assoc arrays).
The front page is a full 26 KB twentytwentyfive page with the post title,
permalink (?p=1) and content from SQLite. Remaining noise: ~7 block-tree
warnings (serialized template parts hitting undefined "attrs"/"blockName"
keys in blocks.php render path — likely another small parser/shape gap).
Next candidates: those warnings, single-post view (?p=1), wp-admin pages,
REST API. Repro: PHARGO_STEP_LIMIT=3000000000 cargo run --release
--example wpscan (needs vendor/wordpress + installed DB; installer probe
lives in the session history — wp_install() via wp-admin/includes/
upgrade.php with WP_INSTALLING defined).

Delegation notes: agents run clean off specs with file scope + literal
expected outputs + numeric baselines (12 successful tasks so far, zero
first-try failures). eval.rs is single-writer — never two agents in it.
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
