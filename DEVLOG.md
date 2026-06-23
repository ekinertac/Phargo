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
