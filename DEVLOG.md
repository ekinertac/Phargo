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
