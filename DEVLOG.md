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
