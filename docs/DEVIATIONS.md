# Known deviations from PHP semantics

The tree-walker ships approximations. This is the honest punch list — every
place the current engine knowingly diverges from PHP, promoted out of DEVLOG
war stories into one document, so the Phase 2 VM (docs/ROADMAP.md, "Path C")
has an explicit contract for what "resolve the approximation" means. Items are
grouped by whether the VM's architecture fixes them for free, they need
explicit VM design, or they're policy choices that survive the cutover.

Baseline for the cutover race (examples/bench.rs, walker, 2026-07-06):
micro TOTAL 1092 ms; wp-front-page 7526 ms (goal < 1000 ms).

## Resolved by the VM architecture (free wins)

- **Eager generators.** `yield` bodies pre-run to a buffered array
  (`gen_buf`); infinite generators only survive via a per-generator node cap.
  A VM with explicit frames suspends/resumes real generator state. Also
  unlocks `Fiber` (currently absent entirely).
- **Step-limit granularity.** The walker's `tick()` counts statements, so
  cost accounting is coarse and `PHARGO_STEP_LIMIT` needs per-workload tuning
  (WP needs 3e9). Bytecode ops give uniform accounting.
- **Line-number bookkeeping.** `Stmt::Marked` wrappers + `cur_line` swaps are
  scattered and easy to desync (caller line restored manually after calls).
  Bytecode carries a line table.
- **Recursion depth = Rust stack depth.** Deep PHP recursion needs the 1 GiB
  worker-thread stack; a VM with heap frames removes the coupling (and the
  `MAX_DEPTH` parser guard stays, but runtime depth stops being a Rust stack
  concern).

## Needs explicit VM design (the value model)

- **References are cell-grafted, not zval-native.** `Value::Ref(Rc<RefCell>)`
  cells are *inserted into* arrays/props on demand (`=&`, foreach-by-ref,
  by-ref params). The seams show:
  - plain assignment must remember to deref (fixed 2026-07-06 after
    WP_Query->posts stored a Ref; other strict `matches!(Value::Array(_))`
    readers may still meet a Ref),
  - `unwrap_element` demotes a cell back to a value by `Rc::strong_count`,
    an approximation of PHP's is_ref flag,
  - by-ref parameters alias only when the argument is a plain variable;
    other lvalues (`f($arr[0])`, `f($obj->p)`) still use the
    capture/apply write-back cascade, which loses aliasing during the call,
  - re-bound by-ref parameters are tracked via a NUL-prefixed scope marker
    (`\0rebound\0name`) — functional, but a marker, not a model.
  The VM's zval-like Value with an explicit is_ref bit replaces all four.
- ~~**Copy-on-write arrays.**~~ **RESOLVED 2026-07-08:** `Arr` is now
  `Rc<ArrData>` with `Rc::make_mut` on every mutation path — cloning is an
  Rc bump, the first mutation through a shared handle copies the payload
  once. Visible semantics unchanged (still value semantics); the WP front
  page went 7.3 s → 0.77 s. The historical note stands: bug40261 and the
  48× WP fix were both "stop cloning containers in hot paths" — the eager
  model invited that bug class, COW retires it.
- **`readonly` init-once.** Writes are checked by visibility context, not by
  "exactly one initialization"; one corpus test knowingly lost.
- **Scope maps.** Locals live in `HashMap<String, Value>`; `compact()`,
  `extract()`, `get_defined_vars()` iterate it (and must skip the `\0` marker
  keys). VM uses compile-time slot indices, with a name table for the
  reflection-style builtins.

## Policy choices that survive the cutover (documented, deliberate)

- **Namespace global fallback.** Unqualified class names in a namespace fall
  back to the global/prelude class instead of fataling — keeps prelude
  Exception/SPL reachable from namespaced code without `use`. PHP would
  fatal; we prefer availability.
- **Prelude classes are PHP source, and their internals differ.** DOM,
  SimpleXML, Reflection, DateTime, PDO are pure-PHP emulations whose
  `var_dump` shapes don't match the C extensions' internal-object dumps.
  ext/dom-style exact-serialization tests stay capped.
- **`restore_error_handler` keeps a single slot**, not a stack.
- **Resource guards.** 6 GiB allocator cap, MAX_STR/MAX_ARRAY_NODES/
  MAX_OUTPUT, per-generator caps, MAX_LIVE_APPENDS on by-ref foreach. These
  emulate memory_limit deaths bluntly; tests expecting PHP's exact OOM
  message text fail.
- **The engine emulates PHP-on-Unix** regardless of host (path separators in
  corpus expectations; forcing constants on Windows regressed once — leave).
- **No network.** `fsockopen`/`stream_socket_client` fail like a refused
  connection; `mail()` is a stub. Extension honesty: `extension_loaded`/
  `get_loaded_extensions` report only what genuinely exists.
- **No SAPI header channel.** `header()`, `setcookie()` are accepted and
  dropped; redirects render nothing. Revisit when the WASM/Playground SAPI
  lands (Phase 3), which needs a real header protocol anyway.

## Known-lost corpus tests (accepted)

- Zend autoload/bug78868 (static-init + autoload corner).
- One readonly init-once approximation test.
- ext/dom exact-serialization family (prelude shape mismatch, above).
