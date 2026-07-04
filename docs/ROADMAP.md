# Phargo — Roadmap

The strategic plan from here to the north star (WordPress running in
Playground on a Rust→WASM engine). Written 2026-07-07 at 3630/21970 corpus
tests (16.5%), after the 23-batch climb documented in `DEVLOG.md`. Pairs with
`PROJECT_STATE.md` (current state) and `CLAUDE.md` (working rules).

The core admission behind this plan: **the .phpt pass rate is a proxy metric,
and the current tree-walking engine is not the final engine.** The corpus
climb built (and keeps building) the safety net that makes the real
destination reachable. This plan sequences the remaining work so every phase
banks value the next phase stands on.

---

## Phase 0 — The second oracle (immediate)

The scoreboard measures PHP-compat in general; the north star needs
WordPress-compat in particular. When the two disagree, WordPress wins.

- [ ] `scripts/fetch-wp.sh` — vendor a pinned WordPress release into
      `vendor/wordpress/` (same pattern as the php-src corpus).
- [ ] `examples/wpscan.rs` — the WordPress-progress harness: execute
      WP's entry points (start with `wp-load.php` / `wp-settings.php`
      bootstrap chain) under the engine with a minimal SAPI shim
      (superglobals, `$_SERVER` fixture, output capture) and report:
      * how far the bootstrap gets (file + line of first death),
      * a ranked histogram of blockers (missing function / class /
        feature) across repeated runs, suiteanalyze-style.
- [ ] Adopt the **two-oracle policy** in `CLAUDE.md`: rung selection
      consults both; WP-blocker rank outranks corpus-cluster size.

**Gate:** a WP-blockers histogram exists and drives the next batch.

## Phase 1 — WP-informed feature climb (weeks)

Continue the batch loop (implement → verify → both oracles → commit), but
reprioritized by the Phase-0 histogram. Predictable candidates, to be
confirmed by measurement rather than assumed:

- Output buffering fidelity (WP nests buffers), include-path/dir semantics,
  superglobal/session/cookie fidelity, `define()`-heavy config loading.
- `preg_*` completeness against WP's regex diet (our from-scratch engine's
  gaps get their own analyzer pass).
- mbstring subset WP actually calls; `sprintf`/`number_format` exactness.
- **Database decision — RESOLVED 2026-07-07 (user call): Option A.**
  `rusqlite` (bundled SQLite) is the one permitted native dependency,
  behind our own PDO/pdo_sqlite/SQLite3 PHP API surface. Rationale:
  fastest to a rendered WP page; SQLite builds for wasm32 so the choice
  survives Phase 3; unlocks ext/pdo + ext/sqlite3 corpus tests. The wpdb
  translation layer comes free via WordPress's own SQLite-integration
  plugin (pure PHP) through the db.php drop-in.
- Corpus rungs continue **only** where cheap or WP-overlapping. The
  tokenizer ext (~55 tests) is explicitly *deprioritized*: zero WP value.

**Gate:** WP bootstrap reaches `wp-settings.php` completion (or the
measured equivalent milestone the histogram exposes).

## Phase 2 — Path C: the real execution model (the big one)

The known ceilings that no amount of tree-walker patching removes:

- **Eager generators** (bodies pre-run; a semantic lie) and **no Fiber**.
- **Approximated value model** (cells + write-backs + `Rc::strong_count`
  instead of zvals with COW and real reference identity).
- **Perf wall**: clone-pressure of the walker; WP needs an order of
  magnitude more than the corpus does.

Path C = bytecode compiler + VM loop with explicit frames → lazy
generators and Fiber fall out of the architecture; line-accurate errors
and real backtraces come free; COW arrays + a zval-like `Value` replace
the approximation stack.

**Method — the proven Path B playbook, verbatim:** build the VM in
parallel behind a flag; run BOTH engines against BOTH oracles; cut over
only when the VM beats the tree-walker on each; then delete the walker.

**Preconditions before starting:**
- [x] `docs/DEVIATIONS.md` — landed 2026-07-06: the punch list of
      approximations, split into "VM fixes for free" / "needs VM design" /
      "policy that survives".
- [ ] Corpus ≥ ~20% and the Phase-1 gate passed (gate passed 2026-07-04 —
      WordPress installs, renders pages, and serves wp-admin; corpus at
      17.5% and climbing alongside).
- [x] `examples/bench.rs` — landed 2026-07-06, including a `cmp` mode that
      races real PHP and writes `docs/BENCHMARKS.md`. Walker baseline:
      micro 1065 ms, wp-front-page 7526 ms (PHP 8.5: 125 ms).

**Status 2026-07-06: STARTED.** `src/lang/vm.rs` — mixed-mode stack VM
behind `PHARGO_ENGINE=vm`: slot-indexed locals, const pool, jumps,
in-place array/concat ops, integer fast paths; per-body fallback to the
walker; chunk cache pinned by owning declaration Rcs. First wins: micro
suite 1065 → ~760 ms; WordPress renders byte-identical under the flag.
Next subset targets: $this/property ops, method calls, array literals —
WordPress is objects all the way down.

**Gate:** cut-over merged; legacy walker deleted (second engine funeral).

## Phase 3 — WASM + Playground (the destination)

- `wasm32-wasip1` target; virtual FS; embed a tz-data subset (no
  `/usr/share/zoneinfo` in WASM — the TZif reader already isolates this
  behind `tz::lookup`).
- SAPI shim matching Playground's protocol; size budget benchmarked
  against Playground's Emscripten PHP as the headline number.

**Gate:** WordPress serves a page in Playground from the Rust engine.

---

## Cross-cutting (all phases)

- **Maintainability:** split the ~10k-line `eval.rs` into modules
  (builtins by family, prelude out of the string constant where possible)
  as mechanical, behavior-gated agent batches.
- **Delegation model** (proven: 12/12 first-try agent tasks): specs carry
  file scope + literal expected outputs + numeric baselines; `eval.rs` is
  single-writer; orchestrator owns diagnosis, architecture, and every
  commit verdict.
- **Honesty artifacts:** DEVLOG per meaningful rung; deviations recorded
  the day they're made, not archaeologically.

## Sequencing summary

```
now ──► P0 WP oracle ──► P1 WP-informed climb ──► P2 Path C VM ──► P3 WASM
        (days)           (to WP-settings gate)    (parallel build,   (target)
                                                   cut-over gate)
```
