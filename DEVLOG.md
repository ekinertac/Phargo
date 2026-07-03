# Phargo — Dev Log

A running, honest journal of building **Phargo**: a from-scratch, memory-safe PHP
engine in Rust, driven by PHP's own `.phpt` test suite as the oracle. The number
that matters is the pass rate over the **upstream php-src corpus** — tests we did
not write — tracked in [`PROGRESS.md`](./PROGRESS.md).

This file is the *story* behind that number: the decisions, the dead ends, the
bombs, and the lessons. Newest entries first. (Eventual goal: a blog-post series.)

---

## How the work actually goes

Each "rung" of the climb follows the same loop:

1. Run a failure histogram over the whole corpus (`examples/errscan.rs`) to find
   the highest-yield *implementable* cluster — not the biggest, the biggest we can
   actually build without an entire C extension behind it.
2. Implement it.
3. Verify with a focused example (`cargo run --example …`).
4. Run the full ~22k-test scoreboard.
5. Commit + push only if the number went up and nothing regressed.

The corpus is a brutally honest oracle. You cannot fool it, and it surfaces bugs
you would never think to write a test for.

---

## 2026-07-05 (later) — A clean page.

**Zero warnings.** The front page renders 25.7 KB of WordPress with no
diagnostics at all. The last two:

- `preg_replace`'s by-ref 5th `$count` parameter (formatting.php's
  curly-quote "Vulcan logic" branches on it) — same shape as the
  str_replace count fixed two days ago; the builtin core now returns the
  count and a dispatch arm writes it through.
- **foreach-by-ref over an object property didn't write back.**
  `foreach ($this->iterations as &$iteration)` in
  WP_Hook::resort_active_iterations silently iterated a copy — hook
  priorities went stale whenever a filter was added mid-dispatch. The
  by-ref foreach now abstracts over its storage (local variable or object
  property) and mutates the property array in place, same Ref-cell
  promotion, same live-append semantics.

Also promoted the installer to a tracked tool (`examples/wpinstall.rs`):
fetch-wp.sh → wpinstall → wpscan is now the full reproducible pipeline
from nothing to a rendered page.

## 2026-07-05 — Hello world!

**The front page renders its post.** `<a href="http://localhost/?p=1">Hello
world!</a>` followed by "Welcome to WordPress…" — a 26 KB page, twenty-
twentyfive block theme, content out of SQLite, served by a PHP engine with
zero C dependencies beyond the bundled SQLite itself.

The last blocker was exactly where the previous entry left it: the SQLite
plugin's translator converts every SQL literal to a named parameter
(`:param0`) and calls `PDOStatement::execute([':param0' => …])` — an
assoc array. Our PDO bridge flattened parameters positionally and, worse,
`bindValue(':param0')` cast the name to `(int)` — zero for everything.
rusqlite's `raw_bind_parameter` + `parameter_index` now bind named
parameters properly; positional `?` still works by order.

wpscan saves each response to `target/wp_page.html`. Remaining known
noise: a handful of block-tree warnings on serialized template parts.
Next: chase those, then point wpscan at `/?p=1` and wp-admin.

## 2026-07-04 (night) — `<title>Phargo Test Site</title>`

**WordPress serves a page.** The full front-controller lifecycle — index.php →
wp-blog-header → `wp()` → main query → template loader → twentytwentyfive block
theme → `wp_head` — runs to `=== WP PAGE RENDER COMPLETED ===`, emitting a
23 KB HTML document with the site title from the SQLite database, the block
library's inline CSS, rendered template parts, and a proper `</html>`. The
sole gap left on the page: the posts loop says "no results" because the main
query's named SQL parameters (`:param0`) get dropped at our PDO bridge —
rusqlite wants named binding, we forward positional. Next batch's first fix.

The rungs between "installed" and "page":

- **`(array) $object` didn't extract properties** — it wrapped the object in
  `[0 => $obj]`. WP's block parser casts `WP_Block_Parser_Block` to array;
  every block came out with no `blockName`. Now PHP-exact including the
  NUL-mangled `\0Class\0prop` / `\0*\0prop` private/protected keys.
- **`PREG_OFFSET_CAPTURE` and preg_match's 5th `$offset` argument** were
  ignored — the block parser advances through the document by match offset,
  so it saw offsets made of garbage. preg_run now returns byte offsets
  (computed from the char-indexed matcher), honors `PREG_SET_ORDER`, omits
  trailing unmatched groups like PHP, and returns `false` for out-of-range
  offsets. All verified byte-identical against PHP 8.
- **Parameter defaults evaluate before the class scope was set** —
  `WP_Theme_JSON::__construct($theme_json = array('version' =>
  self::LATEST_SCHEMA))` resolved `self::` to the *caller's* class. bind_params
  now runs under the callee's class + definition context.
- **SPL filesystem iterators** (`FilesystemIterator`,
  `RecursiveDirectoryIterator`, `RegexIterator`, `RecursiveRegexIterator`) —
  the theme scanner is a RecursiveIteratorIterator/RegexIterator stack.
- Honest gaps: `strtok` (stateful), `parse_str` (bracket-nested query
  strings), `array_replace(_recursive)`, `hash_equals`, and the
  `ENT_XML1`/`JSON_HEX_*` constant families.

Corpus along the way: 3739 → 3752 (installer batch) → 3784 (+45 total today
so far), smoke 88/93. Not bad for a day that started with an "infinite loop".

## 2026-07-04 (later) — WordPress installs itself.

**`wp_install()` runs end-to-end: admin user created (ID 1), 100 options,
3 posts, all in a real SQLite file. A normal front-page request then
bootstraps against that database to completion.**

The blocker chain after "bootstrap completes" was the installer's, and the
final boss was a beauty: every INSERT the installer issued came out as
`VALUES ,:param0 ), ,:param1 )…` — tables created, zero rows written, and
WordPress soft-continues past failed inserts, so the "installed" site died
later with *"One or more database tables are unavailable."* The trail led
through the SQLite plugin's translator (innocent), its query rewriter
(innocent, verified in isolation), and finally to `wpdb::prepare()` — which
uses `preg_split(..., PREG_SPLIT_DELIM_CAPTURE)` to interleave format
specifiers with query text. Our preg_split ignored DELIM_CAPTURE entirely:
4 pieces instead of 10, and sprintf reassembled a query with the tuples'
guts missing. One flag, the whole installer.

Also in the chain, each verified byte-identical against PHP 8:
- **sha256 + hash_hmac from scratch** — WP's `placeholder_escape()` wants
  `hash_hmac('sha256', …)`; the compat polyfill only does md5/sha1 and
  returns false, which would have poisoned every `%` escape.
- **`var $x;` parsed as a typed property** of type `var` — phpass's PHP4
  declarations made every password hasher write throw TypeError.
- **Property defaults now evaluate in definition context** — `self::CONST`
  (PHPMailer) and use-aliased classes (`Port::ACAP` in Requests' Iri) both
  appear in property initializers.
- **find_method now autoloads missing ancestors** — `WP_HTTP_Requests_Hooks
  extends WpOrg\Requests\Hooks` where the parent arrives via PSR-4 later.
- **Stack traces carry per-frame callsite files** — no more "everything
  thrown in wp-settings.php"; the frames finally name the real files.
- Sockets (`fsockopen`/`stream_socket_client`) fail like a refused
  connection with by-ref errno/errstr, so Requests raises its catchable
  exception and WP falls back to WP_Error — no network layer, no fatal.
- Small honest gaps: `stripcslashes`/`addcslashes`, `error_log`,
  `hash_equals`, CRYPT_*/STREAM_CLIENT_* constants.

wpscan now runs the real front-controller lifecycle (wp-blog-header.php →
`wp()` → template loader). Next: make it print a rendered page.

## 2026-07-04 — WordPress boots.

**`=== BOOTSTRAP COMPLETED ===` — the whole wp-load → wp-settings chain runs
to the end under Phargo (installing mode). The Phase 1 gate, hit.**

This was one long day of whack-a-blocker, and almost every mole was a *class* of
bug, not a one-off:

- **The SQL lexer "infinite loop" wasn't a loop bug.** The plugin's lexer reads
  `static::$default_delimiter` — and our engine had never initialized *static
  property defaults*. Every static prop read NULL before its first write, so
  `strlen(null)` = 0 made `parse_delimiter` walk backwards forever. Silent-hole
  count: eight. Fixed with lazy default materialization + PHP's shared-slot
  semantics (a static declared in a parent is one slot for the whole hierarchy).
- **`function_exists` lied about our own builtins.** It knew 33 names; the
  dispatcher handles ~470. WP's compat shim asked "is `mb_strlen` missing?" and
  helpfully installed a pure-PHP polyfill that calls `get_option()` before the
  database exists. Now the list is generated by scanning the dispatch arms.
- **`global $x` bound by value.** WP's `require_wp_db()` does `global $wpdb`,
  includes the drop-in (which sets `$GLOBALS['wpdb']`), then checks
  `isset($wpdb)` — a copy taken *before* the include can't see it. `global` now
  binds through a shared Ref cell, and `$GLOBALS[...]` writes go through it.
- **`__()` parsed as a magic constant.** The prefix test `starts_with("__") &&
  ends_with("__")` matches the two-character name `__` — the same two chars both
  ways. WordPress's most-called function, swallowed by a two-character parsing
  bug. The magic-constant check is now an explicit list.
- **`__DIR__` was the caller's dir.** Definition-site constants (`__FILE__`,
  `__DIR__`), the namespace, and `use` aliases now travel with the function or
  class that declared them — the Requests library's case-sensitive PSR-4
  autoloader was getting lowercase names resolved against the wrong file. This
  also fixed every "thrown in wp-settings.php on line 4413" misattributed stack
  trace this engine has ever printed.
- **`goto`.** WP's HTML-API doctype parser is a goto state machine. Implemented
  as a control error that unwinds until a statement list holding the target
  label catches it — forward, backward, and out-of-loop jumps all work.
- Plus the honest gaps: `strtr`, `parse_url`, `glob`, `substr_replace`,
  `uniqid`, `array_intersect_key`/`array_diff_key`, `str_replace`'s by-ref
  `$count` (WP's `_deep_replace` spins forever without it), `\xNN` inside regex
  character classes (the shortcode-name validator), and template-style
  `switch(): case: ?>…<?php endswitch;`.

Every builtin verified byte-identical against real PHP 8 before moving on. The
non-installing run now exits cleanly with zero output — WordPress correctly
deciding an empty database means "redirect to the installer." Next: run the
installer itself, then render a page.

## 2026-07-08 (later) — 136 seconds to 2.8: the one-line wall

**Pass rate: flat at 3645 (zero regressions). WP bootstrap: 48× faster.**

Profiled the WordPress grind with a native sampler: 81% of all samples were
`Value::clone` and array drops. Three fixes, in escalating impact:

- Method lookups now hand out `Rc<MethodDecl>` from a memo — whole method
  *bodies* had been cloned on every single call since v2 began.
- `func_get_args` storage is Rc-shared instead of deep-copying every call's
  arguments.
- And the wall itself: `arrayaccess_obj` — the "is this an ArrayAccess
  object?" probe on indexed writes — evaluated `$this->arr` to answer,
  **cloning the entire property array on every `$this->arr[$k] = v`**.
  WordPress's SQL translator does exactly that in its hot loop. One
  peek-don't-clone rewrite: 136 s → 2.8 s. It is bug40261's twin, six months
  later, in the property arm — the corpus taught this lesson once; WordPress
  re-taught it at scale.

Step limits are now env-tunable (`PHARGO_STEP_LIMIT`) so the oracle can run
deep without touching corpus guards, and step-limit deaths self-diagnose
(`at file:line in fn()`). Which immediately localized the next blocker: the
SQLite plugin's SQL lexer never net-advances under our engine — an isolated,
reproducible semantic bug, queued with a name and address.

---

## 2026-07-08 — WordPress executes: the wall moves from features to speed

**Pass rate: 3639 → 3645. WP oracle: db_connect → 139 seconds of real execution.**

The blocker-popping cadence the oracle enables: `compact()` (never
implemented — the seventh resurrected staple), honest `extension_loaded()`,
`umask`, the full PDO constant set, and the interesting one —
`PDO::sqliteCreateFunction`. WordPress's SQLite plugin registers ~45 PHP
callbacks as SQL functions (MONTH, FIELD, IF, REGEXP…); instead of building
evaluator-reentrancy for callbacks-inside-queries, the engine pre-registers
the whole MySQL-compat set natively in `src/pdo.rs` — our datetime, hash,
and regex modules answering SQL now — and accepts the registration calls as
no-ops.

Result: WordPress bootstraps through version checks, SAPI fixup, plugin
load, PDO connection, and into live query translation… where it burns the
full 20-million-step budget over 139 seconds. On corpus code the walker does
20M steps in ~4s; on WP's SQL-lexer code, 35× slower. The wall is no longer
missing features — it's the tree-walker itself, arriving precisely where
docs/ROADMAP.md said Phase 2 would find it. Next: profile the grind before
adding surface.

---

## 2026-07-07 (night) — The database decision, and PDO is born

**Pass rate: 3633 → 3639. The dependency count: 0 → 1.**

The WP oracle marched from `version_compare` through `PHP_SAPI` straight to
`wpdb->db_connect()` — the wall the roadmap predicted. Decision made (user
call, recorded in ROADMAP): **rusqlite, bundled, as the one permitted native
dependency**, wrapped by our own PHP API surface. The reasoning: fastest to
a rendered WordPress page, SQLite builds for wasm32 so Phase 3 doesn't
reopen the question, and ext/pdo stops being out-of-scope.

`src/pdo.rs` is the entire native boundary: a thread-local connection
registry and a query call that prepares, binds positionally, and buffers
rows (no statement-lifetime gymnastics). Everything above it is prelude PHP:
`PDO`, `PDOStatement` (fetch modes ASSOC/NUM/BOTH/OBJ/COLUMN, transactions,
quoting, iteration), `PDOException`. First live query, first
`lastInsertId()`, first ext/pdo_sqlite passes (2 → 7). Next: WordPress's
own SQLite-integration plugin as the real db.php drop-in — pure PHP that
translates wpdb's MySQL onto this PDO.

---

## 2026-07-07 (later) — The second oracle flies: WordPress meets Phargo

**Pass rate: 3630 → 3633. WP bootstrap: line 0 → past the version gate.**

Phase 0 of the roadmap landed: `scripts/fetch-wp.sh` vendors WordPress 6.7.1
and `examples/wpscan.rs` runs its real bootstrap chain under the engine with
a synthesized wp-config and a CLI SAPI fixture. First-ever reading: death at
`wp-settings.php:158` on `version_compare()` — a function no analysis had
ever ranked, found by the oracle in 23 ms. Implemented it with PHP's full
canonicalization and special-form ordering (dev < alpha < beta < rc < numbers
< pl), 12/12 on the semantics probes — including the `5.2 vs 5.12` numeric
trap that lexicographic comparison gets wrong.

Second reading: WP now dies trying to *render its own* "MySQL extension
missing" page — which surfaced another parser hole on the way (inline HTML
inside braced blocks: `if (x) { ?>html<?php }` — legal PHP, previously a
parse error). The blocker chain now leads where the roadmap predicted:
the database decision, via WordPress's own `wp-content/db.php` drop-in
mechanism — the same hatch Playground's SQLite integration uses.

---

## 2026-07-07 — Small-bore: asXML declarations, method visibility, honest rulers

**Pass rate: 3629 → 3630.**

A deliberately thin batch of sampler follow-ups: SimpleXML's `asXML()` now
round-trips the XML declaration (sniffed at load, emitted only from the
document root), `get_class_methods()` filters to public when called from
outside the class, and suiteanalyze learned the both-sides trim the real
harness uses — which revealed that part of the "332-test truncation cluster"
was the analyzer's own stale ruler, not the engine. The remaining diffs in
that pool are genuine mid-test drifts, one content bug at a time. The
easy-vein era of this climb is visibly ending; what's left is priced in
features, not fixes.

---

## 2026-07-06 (night) — DOMXPath, and the sampler earns its keep

**Pass rate: 3604 → 3629.**

Two subagents: an XPath 1.0 subset (`//tag`, paths, `[N]`/`[@attr="v"]`/
`[last()]` predicates with per-parent position semantics, `@attr` nodes,
`count()`/`string()`) — ext/dom 41 → 53 EXPECT-only — and a MISMATCH_OTHER
sampler mode for suiteanalyze, opening the 2 800-test multi-line-diff pool
the close sampler couldn't see.

The sampler's first run immediately paid twice: `object(bcmath\Number)` —
the namespace-lowercasing in hoist was leaking into *display* names (keys
should be lowercase, names never) — and `<?xml version="1.1"?>` — saveXML
dropped the encoding attribute; DOMDocument now sniffs the declared encoding
from the source and round-trips it. Next investigation it queued: a 332-test
cluster where our output simply *ends early*.

---

## 2026-07-06 (later) — loadHTML, attributes-as-expressions, and hollow builtin #6

**Pass rate: 3595 → 3604.**

Three-part batch. A subagent delivered `DOMDocument::loadHTML`/`saveHTML` —
a lenient pre-pass (void-element closing, bare-`&` escaping, html/body
wrapping) feeding the strict XML parser, plus an HTML-flavored serializer
(`<br>`, never `<tag/>`). The orchestrator fixed `#[Attr]` in expression
position (closures, arrow fns, `new #[C] class` — 11 parse errors gone).

And the agent's best contribution wasn't in its task at all: while tracing
entity escaping it proved that **`str_replace` with array arguments was a
complete no-op** — `to_bytes` on the array made it search for the literal
string "Array". Six hollow builtins found this climb (clone, $$x, static
vars, spl_autoload_register, trim charlists, str_replace arrays); every one
was found by an actual behavioral trace rather than reading the code. Now
str_replace/str_ireplace do full pairwise/mapped PHP semantics.

---

## 2026-07-06 — XMLReader + all eight rounding modes

**Pass rate: 3583 → 3595.**

A subagent delivered `XMLReader` — the pull-parser API implemented by
flattening our parsed XML tree into a linear event stream (ELEMENT /
END_ELEMENT / TEXT / SIGNIFICANT_WHITESPACE / COMMENT with depth and
attributes), with `read()` as a cursor and `next()` as a subtree skip. The
orchestrator finished the bcmath surface: the PHP 8.4 `RoundingMode` enum
with all eight modes genuinely implemented in the decimal core (half-even
parity via the last kept digit), `bcpowmod` argument validation with PHP's
exact ValueError wordings, and the `x^0 mod 1 = 0` identity that bug #54598
exists to check. ext/bcmath 72 → 81.

---

## 2026-07-05 (night) — BcMath\Number, and trim() was ignoring its charlist

**Pass rate: 3549 → 3583.**

PHP 8.4's object bcmath API: `BcMath\Number` lives in the prelude as our
first *namespaced* prelude class (dogfooding the new namespace support), with
immutable add/sub/mul/div/mod/pow/powmod/sqrt/floor/ceil/round/compare and —
the fun part — **operator overloading**: `$a + $b` on Number objects routes
through a dispatch at the top of `apply_bin` into the same decimal-bignum
core. Plus bcfloor/bcceil/bcround/bcpowmod/bcdivmod. ext/bcmath 43 → 72.

The incidental discovery came from Number's output looking wrong:
`rtrim("2.5000", "0")` returned "2.5000" — **trim/ltrim/rtrim had ignored
their charlist argument since forever**, silently trimming whitespace
instead. Now they take real charlists including PHP's `a..z` range syntax.
ext/standard +5 from that alone; the fifth silently-hollow builtin this
climb has surfaced.

---

## 2026-07-05 (later still) — bcmath from scratch; 16% crossed

**Pass rate: 3497 → 3549 — biggest batch since the harness fix.**

`src/bcmath.rs`: arbitrary-precision decimal arithmetic on base-10 digit
vectors — schoolbook add/sub/mul, long division truncating to PHP's scale
semantics (no rounding, trailing zeros kept), Newton's method sqrt,
repeated-squaring pow — wired to bcadd/bcsub/bcmul/bcdiv/bcmod/bccomp/bcpow/
bcsqrt/bcscale with PHP's exact ValueErrors. ext/bcmath went 0 → 43.

The corpus supplied its ritual resource bomb within minutes:
`bcscale(634314234334311)` — with no range validation, the next bcsqrt tried
to materialize 634 trillion zero digits. PHP validates scale to 0..2³¹-1;
now we do too (plus an internal 100k-digit cap, because a "legal" scale of
two billion is still a memory bomb).

Also this batch: a subagent restored the analyzer's MISSING_* histograms
(the fatal-conversion had collapsed them all into MISMATCH_FATAL — the
measurement tool needed to learn the engine's new output format), and a
second agent added ~70 constants (LIBXML_*, FILTER_*, INPUT_*, XML_OPTION_*,
FILEINFO_*…) — LIBXML_NOERROR alone was blocking 42 tests. ext/filter +6.

---

## 2026-07-05 (later) — Error handlers fire

**Pass rate: 3479 → 3497.**

`set_error_handler` callbacks are actually invoked now — with PHP's
`(errno, errstr, errfile, errline)` signature, a false return falling through
to normal printing, a re-entrancy guard, and exceptions thrown inside the
handler propagating like PHP. `trigger_error` routes through the same path
with all the E_USER_* labels (and E_USER_ERROR halting). A satisfying knock-on:
the object-id off-by-one cluster from the close sampler resolved itself,
because handlers that construct objects now consume instance ids exactly
where PHP does.

---

## 2026-07-05 — Namespaces exist now

**Pass rate: 3450 → 3479.**

The close-mismatch sampler kept circling one test until it got its way:
`interface_exists('IFoo')` inside `namespace foo` must be **false** — and our
engine had been ignoring namespaces since day one. Declarations now register
fully qualified (`foo\Kid`, visible in get_class/var_dump like PHP),
block-form `namespace X { }` bodies actually execute (they were dead code
before — another silent hole), `use` imports resolve, and lookup runs
use-alias → current-namespace → global. The global fallback is a deliberate
deviation from PHP's strict class resolution: it keeps every existing pass
and the entire prelude reachable from inside namespaces, trading a handful of
"Class not found"-expectation tests for hundreds of working ones. Parent /
interface / trait references get qualified at declaration time, so ancestry,
catch-matching, and instanceof all work FQ-aware. One recovery round: modern
DOM tests reach prelude classes via qualified spellings (`Dom\XMLDocument`),
so qualified names that resolve to nothing fall back to the bare last segment.

---

## 2026-07-05 — Engine aborts become PHP fatals

**Pass rate: 3436 → 3450.**

Three engine-internal abort messages became real, catchable PHP `Error`s with
exact wording: `Call to undefined function f()`, `Call to undefined method
C::m()`, `Class "C" not found`. The structural effect is bigger than the
score: Zend's engine-abort bucket collapsed from 351 to 43 — hundreds of
tests that used to die inside the engine now print PHP-style fatal output
(with the new file/line/trace machinery), turning them into near-misses the
close-mismatch sampler can rank next round.

---

## 2026-07-04 (late night) — Stack traces, frame by frame

**Pass rate: 3430 → 3436.**

The second half of what the line infrastructure was built for: real stack
traces. Every call shape pushes a frame (`inner`, `Class->method`,
`Class::method`, `{closure}`) tagged with the caller's line; exceptions
snapshot the stack at construction (prelude-internal frames filtered — the
corpus doesn't expect to see our PHP-emulated internals); and both
`getTraceAsString()` and the uncaught-fatal printer render PHP's format
byte-for-byte: `#0 /file.php(3): inner()` … `#2 {main}`. En route, a real
bug from batch 8 got fixed: a TypeError thrown during argument binding leaked
`cur_fn` entries, quietly corrupting `__FUNCTION__` for the rest of the run.

Honest accounting: +7 and +6 for the two error-infrastructure batches — the
fatal-test cluster demands message, file, line, and trace all exact at once,
so the payoff arrives test by test rather than in a burst. The
infrastructure is the point: every future error-message rung now lands on
correct scaffolding.

---

## 2026-07-04 (night) — Line numbers are real

**Pass rate: 3423 → 3430.**

The engine finally knows what line it's on. The design goal was to avoid the
obvious trap — threading spans through every AST node — and it worked out
surgical: the lexer computes a per-token line table in one two-pointer
post-pass (tokens themselves untouched); the parser stamps each statement
through its existing `statement()` choke point (`Stmt::Marked(line, …)`, one
new variant); the evaluator tracks `cur_line` with a save/restore at the
single shared `run_fn_body` boundary so a caller's line survives calls into
functions. Exceptions capture file+line at construction like PHP; uncaught
fatals print `in /file.php:12 … thrown on line 12`; warnings say `on line 3`;
and `__LINE__` — which had simply never been implemented — returns the truth.

The +7 is honest but modest: it turns out the remaining fatal-test gap is
mostly stack-trace *content* (PHP prints `#0 file(line): func()` frames; we
print `#0 {main}`), not the line numbers. That's the next rung this
infrastructure was built for.

---

## 2026-07-04 (later still) — Property and foreach warnings, with engine-honest limits

**Pass rate: 3414 → 3423.**

Extended the warning surface: `Attempt to read property "x" on string/int/…`
and `foreach() argument must be of type array|object, int given`. The triage
loop surfaced a principle worth writing down: **only warn where the engine is
sure the user erred.** Property reads on null and array stay silent here —
in this engine those bases are as likely artifacts of an unimplemented API
(DOM props we don't model, SimpleXML namespaces) as user mistakes, and one
spurious warning in a passing test costs more than three missed warnings in
failing ones. Closures count as objects; eager generator pre-execution never
warns (PHP's lazy bodies only run when iterated). Bonus correctness:
`DOMElement::setAttribute` now returns the `DOMAttr` like real PHP.

---

## 2026-07-04 (later) — Warnings exist now

**Pass rate: 3387 → 3414.**

The most delicate rung yet: the engine now emits PHP's runtime warnings —
`Warning: Undefined variable $x` and `Undefined array key "k"` — in PHP's
display_errors format. Emitting text is trivial; the hard 90% is everywhere
PHP *stays silent*, because every missed quiet context is a spurious warning
that breaks a passing test:

- `isset()` / `empty()` / `??`-left-hand-side / `@` (the quiet-evaluation contexts)
- by-ref out-params on **every** call shape (`preg_match($p,$s,$m)` must not
  complain about a fresh `$m` — nor may `$obj->method($fresh)` when the method
  declares `&$out`, nor `$closure($fresh)`)
- the read half of nested index *assignments* (`$a['b']['c'] = 1` creates
  dimensions silently)
- `[&$x]` array literals and `=&` (reference creation)
- `return $x` inside `function &f()` (by-ref returns create silently)
- the entire PHP prelude (it emulates C internals — the corpus doesn't expect
  DateTime's internals to warn)
- an active `set_error_handler` (intercepts; we suppress printing)

Method: implement, stash-diff the Zend failure set against the no-warnings
baseline, fix the top cause, repeat — 15 spurious-warning regressions down to
1 accepted corner (an autoload/static-init interaction). One of our own
curated smoke tests turned out to assert PHP-7 silence and got corrected to
PHP 8 reality. The wins live mostly in EXPECTF tests (`in %s on line %d`
tolerates our line-0), which the quick EXPECT-only scans can't even see —
another reminder that each measurement tool has a blind spot.

---

## 2026-07-04 — Arithmetic learns to throw

**Pass rate: 3364 → 3387.**

The last slice of the error-semantics vein: arithmetic/bitwise operators on
array operands now throw PHP 8's `TypeError: Unsupported operand types:
array + int` (with `array + array` union preserved), `/` and `%` by zero
throw `DivisionByZeroError` (previously returned false, PHP-5-style), and
negative shift counts throw `ArithmeticError` — with oversized shifts fixed
to PHP's semantics along the way (`<<` past 63 gives 0, `>>` saturates to the
sign bit; Rust's `wrapping_shl` had been silently wrapping the count).
Zend +16, opcache +7.

---

## 2026-07-04 — Typed and readonly properties

**Pass rate: 3344 → 3364.**

The property half of the type system: writes to declared typed properties now
weak-coerce or throw PHP's exact `Cannot assign string to property P::$n of
type int`, and readonly properties reject writes from outside their declaring
class (`Cannot modify readonly property P::$r`). Because property writes are
hot-path, classes whose hierarchy declares no typed/readonly props skip the
whole check via a per-class cache — the untyped 95% of corpus code pays one
cached hash lookup.

One honest approximation: PHP's readonly is *initialize-once*; our props
default-initialize at instantiation, so "uninitialized" isn't representable
and we approximate with "writable only inside the declaring class". That
traded one ext/standard test for +19 in Zend. Recorded here so future-us
knows where the bodies are buried.

---

## 2026-07-03 (late night) — The type system bites back: TypeError enforcement + static vars

**Pass rate: 3302 → 3344.**

The biggest remaining named cluster ("must be of type" appears in 573 corpus
files) finally got its engine support: **declared parameter and return types
are enforced**. Weak mode does PHP 8's scalar juggling (numeric strings
coerce, int widens to float, Stringable objects satisfy `string`);
`declare(strict_types=1)` — which the parser used to skip entirely, now
captured into the AST — switches to exact-match-only with the int→float
exception. Union types, nullables, class/interface hierarchy checks, and
PHP's exact message shape: `f(): Argument #1 ($x) must be of type int, string
given, called in %s on line %d`. Return types ride the same machinery
(`Return value must be of type X, Y returned`), with a deliberate soft spot:
a Null return never throws, because we can't distinguish `return null` from
falling off the end, and false TypeErrors are worse than missed ones.

Also unearthed while wiring `declare`: **`static $x = 0;` in functions was a
parsed no-op** — the third silently-missing keyword this session, after
`clone` and `$$x`. Statics now live in per-function cells (a Ref into
persistent storage), shared across inherited copies of a method like PHP.

Zend +29, core +10 — and the by-now-standard result that turning on
*enforcement* gains tests rather than losing them: the corpus expects PHP to
throw, and an engine that's too permissive fails those expectations.

---

## 2026-07-03 (night) — Autoloading exists now; 15% crossed

**Pass rate: 3281 → 3302 — 15.0% of gradeable.**

`spl_autoload_register` had been a silent no-op. Now it's real: registered
callbacks fire (with a re-entrancy guard) the first time an unknown class is
touched via `new`, a static call, a class constant, or `class_exists` (whose
`$autoload` parameter defaults to true — several tests check exactly that
flag's behavior). And shutdown functions now also run after an uncaught
exception, matching PHP's lifecycle — that plus autoload converted a chunk of
the "engine printed nothing" bucket the emptyscan tool identified.

Day tally: **3007 → 3302 (+295, 13.8% → 15.0%)** across seven batches — five
of them with cheaper-model subagents doing the well-specified implementation
work in parallel while the analysis, the architecture, and every
commit-or-revert verdict stayed with the orchestrator.

---

## 2026-07-03 (later still) — Measure your measurement, part two

**Pass rate: 3197 → 3281 (+84).**

Second orchestrated wave. An analysis subagent built `examples/emptyscan.rs` to
bucket the ~650 tests that produce empty output (verdict: mostly out-of-scope
SOAP fixtures and a long tail, plus a few real engine-lifecycle gaps now
queued). An implementation subagent added the SPL constants + iterator-mode /
extract-flags plumbing (`IT_MODE_*`, `EXTR_*`, `setIteratorMode`,
`setExtractFlags`). The orchestrator added **variable variables** — `$$x` and
`${expr}` had NO eval, assign, or unset arms; like `clone`, a whole language
keyword silently evaluating to NULL since day one (+10 Zend tests by itself).

But the number that moved the needle was a **harness bug, again**: PHP's
run-tests.php `trim()`s BOTH sides of actual and expected output before
comparing. Our scoreboard only trimmed the end — so every test whose output
legitimately begins with a newline (`echo PHP_EOL, "Done"`) failed on leading
whitespace alone. One-line fix, +dozens of tests across every area
(ext/standard +36, Zend +23, core +9). The CRLF lesson keeps generalizing:
when a failure makes no sense, suspect the ruler before the thing measured.

---

## 2026-07-03 (later) — First orchestrated batch: three subagents + two core fixes

**Pass rate: 3175 → 3197.**

New working mode: well-specified rungs go to cheaper-model subagents in
parallel (each with exact file scope, expected outputs, and regression
baselines to hold); the analysis, the architecture calls, and the regression
verdicts stay with the orchestrator. Three agents ran concurrently on
non-overlapping files and all landed clean on the first try:

- **DOM ChildNode/ParentNode API** (`remove`/`append`/`prepend`/`before`/
  `after`/`replaceWith`, `setAttributeNode`) on the prelude DOM tree.
- **`timezone_identifiers_list()`** + `DateTimeZone::listIdentifiers()` +
  the zone-group class constants, walking /usr/share/zoneinfo.
- **strtotime with explicit zone info** — ISO offsets (`…T22:30:41+02:00`),
  trailing abbreviations (`GMT`, `PST`, 28 more), RFC-2822 dates, and textual
  dates no longer dropping their time-of-day. Architecturally: the parser now
  returns an "absolute" flag so offset-carrying strings skip the wall-clock
  conversion.

Meanwhile the orchestrator took the two semantics fixes the samplers surfaced:
**by-ref callback parameters** (a `Value::Ref` argument now aliases into a
`&$x` param instead of binding a copy — `array_walk` finally mutates, in place,
one writeback) and **live-append iteration in by-ref foreach** (PHP visits
elements appended during the loop). The second one bit back immediately: the
corpus contains tests that append *forever*, relying on PHP's memory_limit to
die — the first implementation hung the whole regression sweep and had an
O(n²) rescan. The fix is a cursor-based tail scan (appends only land at the
end) plus a 100k-visit cap. The old lesson again, from the other direction:
resource guards aren't optional, even inside brand-new code.

---

## 2026-07-03 — Real timezones (a from-scratch TZif reader), and `clone` didn't exist

**Pass rate: 3051 → 3150, then 3175 with the follow-up batch.**

The marquee rung this project has been circling for weeks: **named timezones**.
The engine was UTC-only — `DateTimeZone::getOffset()` returned a hardcoded 0.

- **`src/tz.rs`: a from-scratch RFC 8536 (TZif) reader.** Instead of embedding a
  timezone database, we parse the host's `/usr/share/zoneinfo` files — the same
  IANA tzdata PHP's own timezonedb is generated from, so historical offsets, DST
  transitions, and abbreviations match PHP's answers exactly. ~150 lines, no
  dependencies, cached per thread. `zdump` confirmed macOS ships fat TZif2 files
  with transitions precomputed through 2037 — full coverage for every date the
  corpus tests actually use.
- Everything date-shaped grew a timezone dimension: `date()` vs `gmdate()`,
  `mktime()` (wall-clock inverse mapping with the classic two-pass offset fixup),
  `strtotime()` (parses in the default zone), `DateTime`/`DateTimeImmutable`
  carry a per-object zone, `getTransitions()` streams the real transition table,
  and `DateTime::add/sub/modify` now do calendar math on the **local wall clock**
  — crossing a fall-back transition keeps the wall time, gaining an hour of real
  time, exactly like PHP.
- The scoreboard runner now honors `--INI--` `date.timezone=` lines the way
  run-tests.php does — 157 date tests declare their zone there.
- `var_dump(new DateTime)` prints PHP's synthesized debug props
  (`date`/`timezone_type`/`timezone`), not our internal state.
- The `DATE_*` / `DateTimeInterface::*` format constants, `getdate()`,
  `DatePeriod` (all three constructor forms), `DateInterval::createFromDateString`,
  Swatch beat `date('B')`, ISO week `W`/`o`, and `c`/`r` formats.

And then the stunner, found because `DatePeriod` looped forever: **`clone` was
never implemented.** The parser built `Expr::Clone` nodes; the evaluator had no
arm for them — every `clone $obj` in every test quietly evaluated to NULL. All
of `DateTimeImmutable` was silently broken; so was every OOP test that clones.
One proper eval arm (shallow prop copy, fresh instance id, `__clone()` hook)
later, whole families of tests lit up. The corpus keeps teaching the same
lesson: it's never the fancy features — it's the load-bearing keyword nobody
tested by hand.

Follow-up batch (3150 → 3175): **`DateTime::createFromFormat`** got a real
format matcher — the parsing codes (`d j m n Y y H G h g i s u a A F M D l S U
O P e T z N w`), the reset modifiers `!`/`|` with PHP's exact
position-sensitive semantics (`Y-m-d!` wipes the already-parsed date back to
the epoch; `Y-m-d|` keeps it), `GMT±hh:mm` offsets, and fixed-offset zones
(`+08:00`) synthesized as single-type TzData so they flow through the whole
formatting layer. Plus `setISODate` (Jan-4 rule), `date_isodate_set`, and
`idate()`. ext/date is now 178/689 — it was 76 when the day started.

---

## 2026-07-02 (later) — `static::` was a synonym for `self::`

**Pass rate: 3029 → 3051.**

Second rung of the day, same method: the close-mismatch sampler pointed at core
semantics, not extensions.

- **Late static binding didn't exist.** `static::`, `new static`, and
  `get_called_class()` all resolved to the *defining* class — `static::` was
  literally `self::`. The engine now tracks a separate LSB scope: the runtime
  class of `$this` on instance calls, the named class on `C::m()` calls, and —
  the subtle part — *inherited* through forwarding calls (`self::`/`parent::`/
  `static::`), exactly PHP's forwarding-vs-non-forwarding distinction.
- **Class-const initializers evaluated in the caller's class.** `parent::myDynConst`
  whose initializer says `self::myConst` picked up the *child's* override. Const
  expressions now evaluate scoped to their declaring class.
- **Undefined constants now throw** (`Error: Undefined constant "x"`) instead of
  the PHP-7-ish bareword-to-string fallback. This one needed care: turning it on
  cold broke tests that were passing *because* of the fallback — the corpus used
  constants we'd never defined (`LC_ALL`, `EXTR_*`, `PHP_QUERY_*`,
  `STREAM_FILTER_*`). Filled those in first, then flipped the switch; measured
  before/after failure sets per directory to prove net-positive.
- `error_reporting()` now stores/returns a real level, and `E_ALL` matches
  PHP 8.4's post-E_STRICT value (30719).

---

## 2026-07-02 — Named arguments were silently positional; foreach-by-ref gets real cells

**Pass rate: 3007 → 3029.**

Fresh corpus pull, then the close-mismatch analyzer (`suiteanalyze -- close`) served
up two language-core bugs that had been quietly failing tests across the whole
corpus — not in one extension's directory, but *everywhere*:

- **Named arguments didn't exist.** The parser dutifully recorded `f(b: 2)` names
  into the AST… and the evaluator threw them away, applying every argument
  positionally. `test('A', e: 'E', d: 'D')` bound `d='E', e='D'`. 454 corpus files
  use named-arg syntax somewhere. The fix threads a positional/named split through
  argument evaluation (`eval_args2`) and merges names onto parameter slots — gaps
  filled from parameter defaults — at every call shape: functions, methods, static
  calls, constructors, closures, and string-keyed `...$spread` (the PHP 8.1 named
  form). Zero-cost when no named args are present.
- **`foreach ($a as &$v)` was a no-op.** The loop bound `$v` by value; mutations
  vanished. Worse, passing the var_dump tests needs the `&` refcount marker on
  elements that still have a live alias. So references got their next step: element
  slots can now hold real `Value::Ref` cells. By-ref foreach promotes the visited
  element to a shared cell in place (no array clone — the O(n²) rule), binds `$v`
  to it, and *demotes it back to a plain value* once nothing else holds the cell —
  so, like PHP, only elements still aliased keep the ref. `var_dump` prints `&`
  by consulting `Rc::strong_count`, which tracks PHP's refcount surprisingly well.
  Reads through ref elements deref transparently (index paths, destructuring,
  param binding, comparisons); writes into a ref'd slot write *through* the cell,
  so `$a[1] = 99` is visible via `$v` and vice versa.
- Side dishes: `class_exists`/`interface_exists`/`trait_exists`/`enum_exists` now
  check the declaration *kind* instead of answering "true" for any class-like
  name, and `print_r` learned to see through reference elements.

The lesson repeats: the marquee features were "done" — generators, references,
enums — but the corpus keeps finding the load-bearing 20% we skipped. Named args
shipped in PHP 8.0; we parsed and ignored them for the project's entire life.

---

## 2026-06-23 — "What's taking so long?" — three O(n²) holes and a prelude re-parse

**Pass rate: 2766 → 2810. Scoreboard runtime: ~11 min → ~7 min.**

The user asked why scoreboard runs were dragging. Good question — and chasing it
turned up four separate performance bugs, one of which had been silently making
every run slower for several commits.

- **The prelude was re-parsed 22,000 times.** The scoreboard makes a fresh engine
  per test, and each one lexed + parsed the entire PHP prelude (Exception/SPL/DOM/
  SimpleXML/Reflection/…, now thousands of lines) from scratch. Cached the parsed
  prelude AST in a `thread_local` and just re-hoist from it.
- **`arrayaccess_obj` cloned the whole array on every `$arr[$i] = …`.** Added a
  session or two ago to support object-keyed ArrayAccess, it called `value.deref()`
  to check "is this an ArrayAccess object?" — and `deref()` *clones*. So every
  index assignment cloned the entire (growing) array: O(n²). `bug40261.phpt`
  (100k-element arrays) went from **hanging** to **265 ms** once the check learned
  to peek at the type without cloning. This was the main culprit behind the slow
  runs.
- **Array internal pointers (`reset`/`next`/`current`/…) cloned twice.** First
  `eval_args` cloned the array into the argument list before dispatch; then the
  writeback cloned it again. A 100k-element pointer-iteration loop went from
  **>90 s** to **70 ms** by dispatching before `eval_args` and mutating the
  pointer in place.
- **No output cap.** A `while(true)` that `var_dump`s (gh13178_4) would grind to
  the 20M step limit producing gigabytes. Added a 32 MB output ceiling — a runaway
  echo/var_dump loop now fails fast, like our other resource guards.

The throughline: `.deref()` and "evaluate the argument" both *look* free but clone
the value, and inside a loop over a growing array that's quadratic. The fix in
every case was the same instinct as the in-place `.=` and `$arr[]=` optimizations
from way back — touch the stored value, don't copy it. Worth re-internalizing:
in a tree-walker with value semantics, the question is always "did I just clone a
container in a hot path?"

(Functionally this batch also *added* the array-pointer family — reset/end/next/
prev/current/key/each — which is why the pass count rose while the clock dropped.)

---

## 2026-06-21 — Zend grind: floats, anon classes, and the unset that wasn't

**Pass rate: 2638 → 2706 (Zend area crossed 1000 passing).**

With the harness honest (post-CRLF), the Zend mismatch analyzer became a precise
to-do list. A run of core-language fixes:

- **var_dump float format** — PHP uses shortest-round-trip (serialize_precision
  -1), so `float(2834756759.123123)`, not our `float(2.83…E+9)`.
- **Anonymous classes** — `new class {…}` was never evaluated; it fell through to
  `NULL`. Now registered under a unique internal name (distinct anon classes don't
  collide; stable per declaration so `instanceof` works), displayed as
  `class@anonymous`.
- **var_dump visibility** — `["x":protected]`, `["x":"Class":private]`, resolved
  from the class hierarchy including constructor-promoted params.
- **`unset($arr[$key])` did nothing.** The `unset` statement only handled plain
  `$var` — array-element unset, ArrayAccess `offsetUnset`, and property unset were
  all silently no-ops. This is one of the most common operations in PHP, quietly
  missing. Fixed for all three forms.
- **Object-keyed ArrayAccess** — `$weakmap[$obj] = …` mangled the object offset to
  `int(0)` via key normalization on the *write* path (read was fine). So WeakMap /
  SplObjectStorage-by-`[]` collapsed every key to one slot. Now offsets reach
  offsetSet/offsetUnset raw. Plus WeakMap/WeakReference prelude classes.

Same recurring theme as the CRLF bug: the headline features (classes, closures,
generators) were all there, but a handful of *boring fundamentals* — unset on an
array element, float dump precision — were missing or wrong, each quietly failing
a swathe of tests. The unglamorous stuff is where the mid-game points live.

---

## 2026-06-21 — Pushing on the Zend core, and a CRLF that hid behind everything

**Pass rate: 2581 → 2638 — crossed 12%.**

Decided to aim the climb at the **Zend** test directory — the core language engine
itself, which (unlike extension stubs) is exactly what this project *is*. Built a
Zend-focused analyzer that splits failures into parse-error / runtime-error /
output-mismatch with samples. Two things jumped out.

First, the cheap one: a pile of **basic core functions were simply missing** —
`func_get_args`, `extract`, `fdiv`, `class_alias`, `get_called_class`, `rand`/
`mt_rand`, `random_int`. Added them (func_get_args needed a per-call argument
stack; rand got a small deterministic xorshift).

Second, the one that had been hiding in plain sight for the *entire project*: the
analyzer's mismatch samples showed every expected line ending in `\n\n` where our
output had `\n`. That's not a doubling — it's **CRLF**. The `.phpt` corpus was
checked out on Windows with `autocrlf`, so every `--EXPECT--` block has `\r\n`
terminators, while our engine (correctly) emits `\n`. The scoreboard compared the
two byte-for-byte, so **every multi-line test across the whole corpus had been
failing on line endings alone.** PHP's own run-tests.php normalizes this; we
weren't. One `replace("\r\n", "\n")` on the expected side, and the real number
surfaced — straight past 12%.

The lesson is almost too on-the-nose for a build-in-public log: we spent a dozen
rungs grinding +3s against the long tail, when a single-line harness bug had been
quietly suppressing a whole class of passes the entire time. Measure your
measurement. The oracle is only as honest as your comparison with it.

---

## 2026-06-21 — The long tail, and knowing when you're in it

**Pass rate: 2571 → 2581.**

A cluster of smaller rungs after superglobals: a full `Reflection*` method
build-out, an XML SAX parser (driven off the same `__dom_parse` tree), the
`__serialize`/`__unserialize`/`__wakeup` magic methods, and the PHP 8.4
`Dom\XMLDocument`/`Dom\HTMLDocument` factory API (which, conveniently, resolve by
simple class name, so global prelude classes satisfy the namespaced form).

But the numbers tell a story: +25, +9, +33, +3, +3, +0, +4. The `+33` (superglobals)
was the last big *structural* win — a thing that was missing entirely. After that,
every rung is **precision work**: the feature exists, but matching PHP byte-for-byte
is the bar. `DateTime::__serialize` is the clearest example — I added it, and the
score went *down* two, because PHP's serialized DateTime format has microsecond and
timezone-type details mine didn't reproduce, so tests that had been passing on the
default object serialization now failed on my "better" version. Reverted.

A war story from this stretch worth keeping: I gave `Dom\XMLDocument` a `saveXml()`
method that called `$this->saveXML()`. Instant stack overflow — PHP method names are
case-insensitive, so `saveXml` *is* `saveXML`, and it called itself forever. The fix
was to delete the override entirely and let the inherited (case-insensitive) method
answer both spellings.

The honest read: the project crossed from "missing whole features" into "matching
exact behavior." The first regime is where the big gains live; the second is a
grind of diminishing, uncertain returns. Good place to take stock.

### Session arc (2026-06-14 → 06-21)

From **2176 (9.98%)** to **2581 (11.84%)** of gradeable — **+405 tests**, crossing
10% and 11%. Landed in this stretch: generators, the heap ceiling (after a
generator OOM hard-restarted the dev machine), file streams, the PHP 8.5 pipe
operator, real enums, directory iteration, CSV/SplFileObject, **real references**
(`=&`/`use(&)`/by-ref), **DOMDocument + a from-scratch XML parser**, **SimpleXML**,
Reflection (params + return types), superglobals + sessions, and a pile of
correctness fixes the corpus surfaced along the way (the `(string)` cast that
ignored `__toString` being the most satisfying). Every rung measured against real
php-src tests, committed, and pushed.

---

## 2026-06-21 — Superglobals, and the scope that wasn't global

**Pass rate: 2538 → 2571.**

`$_SERVER`, `$_GET`, `$_SESSION` and friends didn't exist at all. Adding them is
two parts: they must (1) exist as empty arrays so reads don't blow up, and (2)
resolve to the *global* scope from inside any function — that's what makes them
"super". So variable read, variable write, and array-element write all learned to
route a superglobal name straight to `scopes[0]` regardless of the current frame.

The bug that made it interesting: the first cut worked for `$_SESSION['x'] = …`
but `$_SESSION['count']++` inside a function quietly did nothing. The increment's
*read* path (`read_index`, the optimized by-reference array navigator) was still
looking in the current scope, not the global one. So the write went to the
superglobal but the read came back empty every time. Fixed the navigator to honor
superglobal scope — and, while there, to deref reference-backed variables too, so
`$ref[0]` reads correctly. Two scope/aliasing paths, same lesson: every place that
touches a variable has to agree on *which* variable it is.

Session functions came along for the ride as stubs (`session_start` and the dozen
others), since session tests mostly just read and write `$_SESSION`.

---

## 2026-06-20 — Reflection return types, and a cast that forgot to stringify

**Pass rate: 2492 → 2504 (past 2500).**

Chasing the biggest failure cluster — `ReflectionFunction::getReturnType` and
friends — turned up something embarrassing: the parser was *throwing away* return
type declarations. `function f(): int` parsed fine but `: int` went into the void
(`skip_return_type` lived up to its name). So step one was actually keeping them:
store `ret_type` on `FuncDecl`/`MethodDecl`, expose it via a `phargo_func_return_type`
builtin, and wire `getReturnType`/`hasReturnType` plus a fleshed-out
`ReflectionNamedType` (nullable `?int`, `isBuiltin`, `__toString`).

But while testing, both Reflection's and SimpleXML's `(string)$obj` came back
empty — even though `echo $obj` worked. The culprit: `echo` and string
concatenation route through `stringify()` (which honors `__toString`), but the
explicit **`(string)` cast** went straight to `to_bytes()`, whose object fallback
is the empty string. So every `(string)` cast of a stringable object had silently
produced `""` this whole time. One-line fix to route object string-casts through
`stringify`, and a whole category of quiet wrongness disappeared.

That's the recurring shape of this project: you go in to add feature X, and the
test for X exposes a latent bug Y that was wrong all along but never directly
tested. The corpus finds Y for you.

---

## 2026-06-20 — SimpleXML, almost for free

**Pass rate: 2466 → 2490.**

This rung was cheap because the hard part was already done. The DOM work built a
real XML parser (`__dom_parse`) that returns a plain nested-array tree. SimpleXML
is just a *different view* over that same tree, so `simplexml_load_string` is a
one-liner that parses and wraps, and `SimpleXMLElement` is a prelude class that
walks the array.

The fiddly part of SimpleXML is its famous dual nature: `$xml->book` is
simultaneously "the first book" (you can read `$xml->book->title`) and "all the
books" (you can `foreach ($xml->book as $b)`). Modeled it by having `__get('book')`
return an element that carries *both* the first match and the full sibling group,
implementing `Iterator` over the group and the single-element accessors over the
first. Attribute access (`$xml['id']`), `(string)$el` text extraction, `children()`,
`attributes()`, and `asXML()` round it out.

One real bug surfaced and got fixed along the way: indexing the *result of an
expression* — `$xml->book[0]` — wasn't dispatching to `offsetGet`, because the
index-read path only recognized ArrayAccess when the base was a bare `$var`. Now a
property/method/call result that's an ArrayAccess object routes correctly. That
fix helps any ArrayAccess-returning expression, not just SimpleXML.

---

## 2026-06-20 — Real references, without rewriting the value model

**Pass rate: 2451 → 2466, zero regressions.**

The by-ref-param write-back from earlier was a workaround. The real fix —
proper PHP references — landed here, and it turned out to be less scary than
feared.

The textbook approach is to make *every* variable a shared mutable cell. That's a
deep rewrite. Instead: add one new value variant, `Value::Ref(Rc<RefCell<Value>>)`,
and lean on the fact that **references only ever live directly under a variable
name in a scope** — never inside arrays or object properties (PHP allows that too,
but it's rare, and skipping it keeps the blast radius tiny). With that invariant:

- **Reading** a variable derefs (one match).
- **Writing** a variable writes *through* the cell if it's a ref, else replaces it.
- The `&` sites — `$b = &$a`, `use (&$x)` — call one helper, `get_ref_cell`, which
  turns the target variable into a ref-to-cell (if it isn't already) and hands back
  the shared `Rc`. Both names now point at the same cell.

The safety net that made this comfortable: a defensive deref at the top of every
coercion and comparison function (`to_bool`, `to_i64`, `loose_eq`, …). So even if a
`Ref` leaks somewhere I didn't anticipate, arithmetic and comparisons still do the
right thing instead of treating the reference as a weird opaque value. References
are rare, so the extra `matches!` check per call is free in practice.

`use (&$sum)` accumulators — the thing half the `array_map`/`array_walk` callbacks
in the world rely on — now work. So does `$out[] = …` through a captured array
reference, mutating the array *in place* through the cell (no clone, so no O(n²)).

The lesson: a localized invariant ("refs only live in scopes") can turn a
terrifying rewrite into a contained, testable change. +15 undersells it — this is
infrastructure a lot of future tests sit on.

---

## 2026-06-20 — Making `&$ref` parameters actually mean something

**Pass rate: 2435 → 2440.**

The DOM rung had exposed it: by-reference parameters didn't work. `function f(&$x)`
was silently by-value, so any `&$out` accumulator quietly did nothing.

A "proper" fix means real reference cells in the value model — every variable
becomes a shared, aliasable box. That's a deep, invasive change to a tree-walker.
So instead: **write-back**. The engine still passes by value, but for a `&$param`
whose argument is a writable lvalue (a variable, an index, a property), the
parameter's final value is copied back into the caller's variable *after* the call
returns. The trick is timing — capture the value before the callee's scope is
popped, apply it after, when the caller's scope is back on top.

The satisfying part is that this **cascades through recursion for free**. In
`collect($node, &$out)` calling itself, the inner call writes back to the outer
frame's `$out`, then the outer frame writes back to *its* caller, all the way up.
A recursive accumulator just works, even though there's not a real reference
anywhere in the system — just a chain of well-timed copies.

It's not complete (the `call_user_func`-style callable-value path is still
by-value, since there are no argument expressions to write back to), but it covers
functions, methods, static calls, and recursion. The modest +5 undersells it: it's
a foundational correctness fix with zero regressions, and it's the prerequisite for
`array_walk_recursive` and friends.

A small irony worth noting for the eventual blog: I'd *worked around* this exact
bug in the DOM code a day earlier (returning arrays instead of using `&$out`).
Sometimes you fix the symptom to ship, then come back and fix the disease.

---

## 2026-06-20 — DOMDocument, and teaching the engine to read minds (`__get`)

**Pass rate: 2396 → 2435 (past 11%).**

DOM was the biggest non-extension lever left (~360 tests) and it's squarely on
the north-star path — WordPress lives and breathes DOM/HTML. There was no XML
infrastructure in v2 at all (the legacy engine's `xml.rs` was deleted when we
retired it), so this was a from-scratch build.

The design that kept it sane: **a hybrid**. A small Rust XML parser
(`src/lang/xml.rs`) does the one genuinely hard part — turning bytes into a tree —
and hands back a plain nested-array structure via a `__dom_parse` builtin. Then
*all* the DOM semantics (DOMDocument, DOMElement, DOMNodeList, textContent,
saveXML, getElementsByTagName, …) live in PHP, in the engine's prelude. PHP is a
much nicer language than Rust for "walk this tree and concatenate the text nodes,"
and it means the DOM behavior is itself testable PHP.

Two things fell out of this:

- **`__get` had to become a real engine feature.** DOM properties like
  `$node->textContent`, `$node->firstChild`, `$node->nextSibling` are *computed*,
  not stored. PHP models these with the `__get` magic method, which v2 didn't
  support. So I added it: on reading a property that doesn't exist, if the class
  defines `__get`, call it. It's strictly additive (reading an absent property used
  to just yield null), so it can't break anything — and it's useful far beyond DOM.
  I deliberately did *not* add `__set`, because in a tree-walker where every
  property is created on first assignment, `__set` would hijack normal dynamic
  properties.

- **I found a real bug in the engine, the hard way.** My first
  `getElementsByTagName` used the obvious recursive accumulator:
  `__collect($name, &$out)`. It returned zero elements every time. The cause:
  **by-reference parameters don't write back through recursion in v2.** (The same
  bug had quietly broken two earlier test helpers; this is where it finally clicked.)
  Workaround: make the recursion return arrays and merge them instead of mutating a
  shared `&$out`. The proper fix — real reference cells — is now the most valuable
  item on the to-do list, because it also unlocks `array_walk_recursive` and every
  other recursive-accumulator pattern in the corpus.

The curated DOM smoke test now passes byte-for-byte.

---

## 2026-06-14 — A week of breadth: streams, enums, a pipe operator, and a CSV reader

**Pass rate: 2176 → 2396 (+220, crossed 10% and kept going).**

A long, productive stretch of mostly errscan-driven rungs. Highlights and the
stories worth telling:

### File streams modeled as objects
`fopen` and friends were the single biggest lever (~694 test mentions). The
obvious move is a new `Value::Resource` enum variant — but that ripples through
every `match` on values in an 8,000-line evaluator. Instead I modeled a stream as
an ordinary object of a hidden `__Stream` class holding an in-memory byte buffer +
cursor. Because objects in this engine are already `Rc<RefCell<…>>`, a handle
passed *by value* still mutates the same underlying stream — so `fread`/`fwrite`
see each other's effects for free, no by-reference plumbing needed. Real files are
slurped on open and flushed back on every write. `is_resource`, `gettype`, and
`var_dump` were taught to recognize the disguise. +50 tests, zero new value-model
complexity.

### The PHP 8.5 pipe operator `|>`
`$x |> $f` means `$f($x)`. The lexer was splitting `|>` into `|` and `>`, so it
showed up in the histogram as a mysterious "unexpected token: Gt." The fun part
was precedence: the spec puts `|>` *between* concatenation (higher) and comparison
(lower), and the binding-power table had no integer gap there. So I renumbered the
whole upper half of the precedence ladder by +2 to open a clean slot — then proved
the existing precedence was untouched (`1 + 2*3**2` still 19, `-2**2` still -4)
before trusting it.

### Enums were quietly broken
While probing for ReflectionEnum, I discovered `Suit::cases()` failed *literally* —
the enum built-in static methods (`cases`/`from`/`tryFrom`) weren't implemented at
all. Worse, enum cases weren't singletons, so `Suit::from('H') === Suit::Hearts`
would have been false. Fixed both: the static methods, plus a case cache so every
reference to a given case is the same object. `constant()` and `defined()` were
also just… missing, so those went in too.

### A small process scar
At one point a `git commit` failed with "filename too long" and a wall of
unfamiliar untracked files. The cause: I'd `cd`'d into `vendor/php-src` in a Bash
call to read a test file, the Bash and PowerShell tools **share one working
directory**, and `vendor/php-src` is itself a git repo (polluted with corpus
artifacts). My commit had run in the wrong repo. It failed safe — nothing wrong
was committed — but the lesson stuck: read corpus files by absolute path, never
`cd` into the vendored tree.

---

## 2026-06-10 — The night a generator restarted the computer

**Pass rate: 2176 → 2216 (first time past 10%).**

This is the war story of the project so far.

I'd just implemented **generators** (`yield`), the marquee feature the AST rewrite
unlocked. The implementation is *eager*: a function containing `yield` runs its
whole body to completion, collecting yielded values into a buffer, then exposes
them through an iterator. Simple, and it handles the common cases. Its known
limitation is infinite generators — `while (true) yield …` can never "run to
completion."

I had a cap to protect against that: stop after N buffered *values*. It wasn't
enough. The corpus has `bug71297.phpt` — an infinite generator that yields a
**10,000-element array** on every iteration. My cap counted values (one array per
iteration), so it happily allowed millions of giant arrays. The heap climbed to
**25 GB**, exhausted RAM and pagefile, and **hard-restarted the machine.**

The fix came in two layers, because one wasn't enough:

1. **Per-test:** cap the *total node count* in the generator buffer (counting into
   the arrays), not the number of values. The bomb now trips the cap at ~570 MB and
   fails gracefully with "generator buffer limit exceeded."

2. **Process-wide:** a custom global allocator with a hard **6 GiB ceiling**. Past
   it, allocation returns null and the process aborts — losing one test run, never
   the machine. The scoreboard runs all 22k tests in one process and `catch_unwind`
   *cannot* catch an allocation abort, so this ceiling is the only real protection
   against a bomb we didn't anticipate. After a machine restart, you build the
   guardrail that makes it impossible to happen again.

The deeper lesson: when your test oracle is 22,000 programs other people wrote,
some of them *are* adversarial by accident. Resource ceilings aren't optional.

---

## Earlier — "Path B": rebuilding the engine as a real language implementation

Before this log started, the project made its biggest bet. The original engine was
a single-pass streaming interpreter with no AST — loops re-parsed their body every
iteration, functions re-parsed on every call. It worked (it reached ~1981 passing
tests) but its design was a ceiling: that re-parsing was a speed wall, and the
cursor model made generators nearly impossible (nowhere to suspend).

So we rebuilt the core from scratch as a proper language implementation:
byte-level lexer → recursive-descent parser → owned AST → tree-walking evaluator,
with byte-correct strings (`Vec<u8>`) and correct operator precedence baked in. We
built it *in parallel* with the old engine and only cut over once it surpassed the
shipping number on the same suite — so the public scoreboard stayed honest the
whole time. Then we deleted the old engine: `lib.rs` went from 11,271 lines to 52.

The payoff is everything above this line. The AST is what made generators, the
pipe operator, and clean precedence possible at all.
