//! Phargo scoreboard.
//!
//! The **official metric is corpus-only**: the pass-rate over the upstream
//! php-src `.phpt` suite in `vendor/php-src/` — tests we did not write. Our own
//! curated tests in `tests/phpt/` are reported separately as "smoke tests" and
//! are NOT counted toward the headline number (writing the test and the
//! implementation proves nothing — independence is the whole point).
//!
//! Results are bucketed by area (per-extension, Zend, core) so the report stays
//! readable across ~22k tests. Writes PROGRESS.md so the climb is always public.

use phargo::run_with_path;
use std::collections::BTreeMap;
use std::panic;
use std::fs;
use std::path::{Path, PathBuf};

const CURATED: &str = "_curated";

struct Phpt {
    file: String,
    expect: Option<String>,
    expectf: Option<String>,
    path: PathBuf,
}

enum Outcome {
    Pass,
    Fail,
    Unsupported, // can't grade yet (e.g. --EXPECTF--, or no expected output)
}

/// [pass, fail, n/a, total]
type Tally = [usize; 4];

fn main() {
    // Deep recursion (engine + EXPECTF matcher) overflows the 1 MB default
    // stack on Windows; give the work a big stack.
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(run_scoreboard)
        .unwrap()
        .join()
        .unwrap();
}

fn run_scoreboard() {
    panic::set_hook(Box::new(|_| {})); // silence per-test panic spam; caught in evaluate()
    let root = env!("CARGO_MANIFEST_DIR");
    let curated_dir = Path::new(root).join("tests").join("phpt");
    let corpus_dir = Path::new(root).join("vendor").join("php-src");

    // Corpus tests do real file I/O with relative paths; run them in a throwaway
    // scratch dir so they can never write junk into the repo working tree.
    let scratch = std::env::temp_dir().join("phargo_scratch");
    let _ = fs::create_dir_all(&scratch);
    let _ = std::env::set_current_dir(&scratch);

    let mut tests: Vec<(String, PathBuf)> = Vec::new();
    let mut cu = Vec::new();
    collect_phpt(&curated_dir, &mut cu);
    for p in cu {
        tests.push((CURATED.to_string(), p));
    }
    if corpus_dir.exists() {
        let mut co = Vec::new();
        collect_phpt(&corpus_dir, &mut co);
        for p in co {
            let g = group_of(&p);
            tests.push((g, p));
        }
    } else {
        eprintln!("(vendor/php-src not found — run scripts/fetch-corpus.sh for the full suite)");
    }

    let mut groups: BTreeMap<String, Tally> = BTreeMap::new();
    let mut corpus: Tally = [0; 4]; // official metric
    let mut curated: Tally = [0; 4]; // smoke tests, not counted

    let n = tests.len();
    let breadcrumb = Path::new(root).join("target").join("current_test.txt");
    for (i, (group, path)) in tests.iter().enumerate() {
        let _ = fs::write(&breadcrumb, path.to_string_lossy().as_bytes());
        let bytes = fs::read(path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let mut t = parse_phpt(&text);
        t.path = path.clone();
        let idx = match evaluate(&t) {
            Outcome::Pass => 0,
            Outcome::Fail => 1,
            Outcome::Unsupported => 2,
        };
        if group == CURATED {
            curated[idx] += 1;
            curated[3] += 1;
        } else {
            let slot = groups.entry(group.clone()).or_insert([0; 4]);
            slot[idx] += 1;
            slot[3] += 1;
            corpus[idx] += 1;
            corpus[3] += 1;
        }
        if n > 2000 && i % 4000 == 0 && i > 0 {
            eprintln!("  …{i}/{n}");
        }
    }

    print_summary(&corpus, &curated, &groups);
    write_progress(root, &corpus, &curated, &groups);
    println!("\nWrote PROGRESS.md");
}

fn evaluate(t: &Phpt) -> Outcome {
    // Tests with neither EXPECT nor EXPECTF (EXPECTREGEX, output-less) aren't gradeable.
    if t.expect.is_none() && t.expectf.is_none() {
        return Outcome::Unsupported;
    }
    // A buggy engine path on one test must never crash the whole run.
    let actual = match panic::catch_unwind(panic::AssertUnwindSafe(|| {
        run_with_path(&t.file, Some(t.path.clone()))
    })) {
        Ok(Ok(s)) => s,
        _ => return Outcome::Fail,
    };
    // Normalize line endings: the corpus is checked out with CRLF on Windows, but
    // PHP output (and run-tests.php's comparison) is LF — so compare LF-to-LF.
    let actual = lf(&actual);
    if let Some(e) = &t.expect {
        if actual.trim_end() == lf(e).trim_end() {
            return Outcome::Pass;
        }
        return Outcome::Fail;
    }
    if let Some(p) = &t.expectf {
        if expectf_matches(&lf(p), &actual) {
            return Outcome::Pass;
        }
    }
    Outcome::Fail
}

/// Normalize CRLF / lone CR to LF (the corpus has CRLF from Windows checkout).
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

// ---- --EXPECTF-- matcher (hand-rolled; no regex dependency) -----------------

#[derive(Clone)]
enum Tok {
    Lit(Vec<char>),               // a run of literal characters
    One,                          // %c — exactly one non-newline char
    Var(fn(char) -> bool, usize), // greedy run of `pred`, at least `usize`
}

fn expectf_matches(pattern: &str, actual: &str) -> bool {
    let toks = parse_expectf(pattern.trim_end());
    let s: Vec<char> = actual.trim_end().chars().collect();
    let mut budget: u64 = 300_000;
    match_toks(&toks, &s, &mut budget)
}

fn match_toks(toks: &[Tok], s: &[char], budget: &mut u64) -> bool {
    if *budget == 0 {
        return false; // give up on pathological backtracking — counts as a non-match
    }
    *budget -= 1;
    match toks.split_first() {
        None => s.is_empty(),
        Some((t, rest)) => match t {
            Tok::Lit(lit) => {
                s.len() >= lit.len()
                    && s[..lit.len()] == lit[..]
                    && match_toks(rest, &s[lit.len()..], budget)
            }
            Tok::One => !s.is_empty() && s[0] != '\n' && match_toks(rest, &s[1..], budget),
            Tok::Var(pred, min) => {
                let mut max = 0;
                while max < s.len() && pred(s[max]) {
                    max += 1;
                }
                if max < *min {
                    return false;
                }
                let mut k = max; // greedy, then backtrack down to the minimum
                loop {
                    if match_toks(rest, &s[k..], budget) {
                        return true;
                    }
                    if k == *min {
                        return false;
                    }
                    k -= 1;
                }
            }
        },
    }
}

fn parse_expectf(p: &str) -> Vec<Tok> {
    let chars: Vec<char> = p.chars().collect();
    let mut toks = Vec::new();
    let mut lit: Vec<char> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            if chars[i + 1] == '%' {
                lit.push('%'); // %% is a literal percent
                i += 2;
                continue;
            }
            let t = match chars[i + 1] {
                'a' => Some(Tok::Var(any_c, 1)),
                'A' => Some(Tok::Var(any_c, 0)),
                'w' => Some(Tok::Var(ws_c, 0)),
                's' => Some(Tok::Var(nonl_c, 1)),
                'd' => Some(Tok::Var(dig_c, 1)),
                'i' => Some(Tok::Var(int_c, 1)),
                'f' => Some(Tok::Var(flt_c, 1)),
                'x' => Some(Tok::Var(hex_c, 1)),
                'c' => Some(Tok::One),
                'e' => Some(Tok::Var(sep_c, 1)),
                _ => None,
            };
            if let Some(t) = t {
                if !lit.is_empty() {
                    toks.push(Tok::Lit(std::mem::take(&mut lit)));
                }
                toks.push(t);
                i += 2;
                continue;
            }
        }
        lit.push(chars[i]);
        i += 1;
    }
    if !lit.is_empty() {
        toks.push(Tok::Lit(lit));
    }
    toks
}

fn any_c(_: char) -> bool {
    true
}
fn ws_c(c: char) -> bool {
    c.is_whitespace()
}
fn nonl_c(c: char) -> bool {
    c != '\n' && c != '\r'
}
fn dig_c(c: char) -> bool {
    c.is_ascii_digit()
}
fn int_c(c: char) -> bool {
    c.is_ascii_digit() || c == '+' || c == '-'
}
fn flt_c(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E' | 'N' | 'A' | 'I' | 'F')
}
fn hex_c(c: char) -> bool {
    c.is_ascii_hexdigit()
}
fn sep_c(c: char) -> bool {
    c == '/' || c == '\\'
}

fn pct(part: usize, whole: usize) -> f64 {
    if whole > 0 {
        part as f64 * 100.0 / whole as f64
    } else {
        0.0
    }
}

fn print_summary(corpus: &Tally, curated: &Tally, groups: &BTreeMap<String, Tally>) {
    let [pass, fail, na, total] = *corpus;
    let gradeable = pass + fail;

    let mut sorted: Vec<(&String, &Tally)> = groups.iter().collect();
    sorted.sort_by(|a, b| b.1[3].cmp(&a.1[3]));

    println!("\nBy area (top 30):");
    println!("  {:<22} {:>6} {:>7} {:>7} {:>7}", "area", "pass", "fail", "n/a", "total");
    for (g, t) in sorted.iter().take(30) {
        println!("  {:<22} {:>6} {:>7} {:>7} {:>7}", trunc(g, 22), t[0], t[1], t[2], t[3]);
    }
    if sorted.len() > 30 {
        println!("  …(+{} more areas)", sorted.len() - 30);
    }

    println!("\n====================================");
    println!("OFFICIAL (php-src corpus, tests we did not write):");
    println!("  PASS {pass}   FAIL {fail}   N/A {na}   TOTAL {total}");
    println!(
        "  Pass rate: {pass}/{total} ({:.2}% of all .phpt) | {:.2}% of gradeable ({gradeable})",
        pct(pass, total),
        pct(pass, gradeable),
    );
    println!(
        "Smoke tests (curated, NOT counted): {}/{} passing",
        curated[0], curated[3]
    );
}

fn write_progress(
    root: &str,
    corpus: &Tally,
    curated: &Tally,
    groups: &BTreeMap<String, Tally>,
) {
    let [pass, fail, na, total] = *corpus;
    let gradeable = pass + fail;

    let mut md = String::new();
    md.push_str("# Phargo — Progress\n\n");
    md.push_str("> Auto-generated by `cargo run`. The whole point of this project is to watch this number climb, in public.\n\n");
    md.push_str("## Scoreboard\n\n");
    md.push_str(&format!(
        "**`.phpt` pass rate: {pass} / {total}  ({:.2}% of the entire PHP test suite)**\n\n",
        pct(pass, total)
    ));
    md.push_str("_This counts only the upstream **php-src** test suite — tests we did **not** write. ");
    md.push_str(&format!(
        "Among tests the runner can currently grade ({gradeable}): {:.2}%. \
         The {na} \"not-yet-gradeable\" tests have no `--EXPECT--`/`--EXPECTF--` (e.g. `--EXPECTREGEX--` or output-less)._\n\n",
        pct(pass, gradeable)
    ));
    md.push_str("| ✓ pass | ✗ fail | • not-yet-gradeable | total |\n");
    md.push_str("|---:|---:|---:|---:|\n");
    md.push_str(&format!("| {pass} | {fail} | {na} | {total} |\n\n"));
    md.push_str(&format!(
        "_Curated smoke tests (dev guards, **not** in the number above): {}/{} passing._\n\n",
        curated[0], curated[3]
    ));

    md.push_str("## By area\n\n| area | ✓ pass | total | % |\n|---|---:|---:|---:|\n");
    let mut sorted: Vec<(&String, &Tally)> = groups.iter().collect();
    sorted.sort_by(|a, b| b.1[3].cmp(&a.1[3]));
    for (g, t) in &sorted {
        md.push_str(&format!("| `{g}` | {} | {} | {:.1}% |\n", t[0], t[3], pct(t[0], t[3])));
    }

    md.push_str("\n## Roadmap (the climb)\n\n");
    md.push_str(ROADMAP);

    let _ = fs::write(Path::new(root).join("PROGRESS.md"), md);
}

fn parse_phpt(text: &str) -> Phpt {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<String> = None;
    let mut buf = String::new();
    for line in text.lines() {
        if let Some(name) = section_header(line) {
            if let Some(c) = current.take() {
                sections.push((c, std::mem::take(&mut buf)));
            }
            current = Some(name);
        } else if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(c) = current.take() {
        sections.push((c, buf));
    }
    let get = |key: &str| {
        sections
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    Phpt {
        file: get("FILE").unwrap_or_default(),
        expect: get("EXPECT"),
        expectf: get("EXPECTF"),
        path: PathBuf::new(),
    }
}

/// A section header is a line that is exactly `--NAME--` (uppercase + `_`).
fn section_header(line: &str) -> Option<String> {
    let t = line.trim_end();
    if t.len() > 4 && t.starts_with("--") && t.ends_with("--") {
        let inner = &t[2..t.len() - 2];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return Some(inner.to_string());
        }
    }
    None
}

/// Bucket a corpus test by area, mirroring how PHP itself organises tests.
fn group_of(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if let Some(i) = s.find("/ext/") {
        let name = s[i + 5..].split('/').next().unwrap_or("?");
        return format!("ext/{name}");
    }
    if s.contains("/Zend/") {
        return "Zend".to_string();
    }
    if let Some(i) = s.find("/sapi/") {
        let name = s[i + 6..].split('/').next().unwrap_or("?");
        return format!("sapi/{name}");
    }
    "core".to_string()
}

fn collect_phpt(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_phpt(&p, out);
            } else if p.extension().map(|e| e == "phpt").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

const ROADMAP: &str = r#"The ladder to **"WordPress boots in the browser"**. Each rung is measured against real php-src tests.

- [x] **v0–v3b — Core language.** Inline HTML/`echo`, the zval value model + `"$var"` interpolation, the full operator/precedence/type-juggling engine, `if`/`while`/`do`/`for`/`foreach`/`switch`, `break`/`continue`, `++`/`--`, assignment-as-expression (incl. compound), comments.
- [x] **v4 — Functions.** Built-in calls (`var_dump`/`is_*`/string+math) and user `function`s (params/defaults/`return`/recursion/scope).
- [x] **v5 — Arrays.** Ordered-map arrays, `[...]`/`array()` literals, index r/w/append, `foreach`, and the core array/string builtins.
- [x] **v6 — Ternary `?:` / null-coalescing `??`** + `sprintf`/`substr`/`str_replace`.
- [x] **v8 — Classes.** `class`/`interface`/`trait`, properties, methods, `$this`, `new`, `->`, `__construct`, single inheritance + polymorphic dispatch.
- [x] **v9 — `isset` / `empty` / `unset`.**
- [x] **v10 — `::` + `__toString`.** Class constants, `self::`/`parent::`/`Class::`, `::class`.
- [x] **v11 — Exceptions + `instanceof`.** `throw`/`try`/`catch`/`finally`, the Exception/Error/SPL hierarchy.
- [x] **v12 — Callables + higher-order builtins.** `array_map`/`array_filter`/`array_reduce`, `call_user_func`(`_array`), `array_search`/`array_slice`/…
- [x] **v13 — Closures.** `function(...) use(...) {}` (by-value capture) + arrow `fn(...) => expr`.
- [x] **v14 — JSON.** `json_encode` / `json_decode`.
- [x] **v15 — By-reference array fns.** `sort` family + `array_push`/`array_pop`/`array_shift`/`array_unshift`.
- [x] **v16–v20 — More builtins + language.** array_* / ctype_*, `match`, static class properties, `strtr`, `compact`, `levenshtein`, …
- [x] **v21 — Heredoc / nowdoc.**
- [x] **v22 — Array destructuring.** `[$a, $b] = …` / `list(...)`, skipped slots + nesting.
- [x] **v23–v24 — Hashing / encoding.** `md5`/`sha1`/`crc32`/`base64`/`bin2hex`, `hash()` dispatcher (from-scratch algorithms).
- [x] **v25 — Regex engine.** From-scratch backtracking VM: `preg_match`/`preg_match_all`/`preg_replace`/`preg_replace_callback`/`preg_split`/`preg_quote`.
- [x] **v26 — Type casts** (`(int)`/`(string)`/…) + PHP leading-numeric string coercion.
- [x] **v27 — `@` error-suppression + full numeric literals** (hex/bin/oct/`_` separators).
- [x] **v28 — Traits, constructor promotion, `#[attributes]`, robust class bodies.**
- [x] **v29 — User constants, `print`/`clone`, `declare`/`namespace`, output buffering (`ob_*`), setup stubs.**
- [x] **v30 — ~70 predefined constants** (`DIRECTORY_SEPARATOR`/`SORT_*`/`ENT_*`/`FILTER_*`/…).
- [x] **v31 — `include` / `require` / `eval`** with real file loading (and real `__FILE__`/`__DIR__`).
- [x] **v32 — Filesystem + path fns.** `file_put_contents`/`file_get_contents`/`scandir`/`mkdir`/`basename`/`dirname`/`pathinfo`/…
- [x] **v33 — `serialize` / `unserialize`.**
- [x] **v34 — Enums** (pure + backed: `::cases`/`from`/`tryFrom`) + postfix chaining on expressions.
- [x] **v35 — File streams.** `fopen` family (`fread`/`fwrite`/`fgets`/`fgetcsv`/…) + `STDIN`/`STDOUT`/`STDERR`.
- [x] **v36 — `mbstring`** (UTF-8 basics: `mb_strlen`/`mb_substr`/`mb_*`).
- [x] **v37 — More standard functions.** `strstr`/`fdiv`/`filter_var`/`array_walk`/`class_alias`/`wordwrap`/…
- [x] **v38 — `foreach` over `Iterator`/`IteratorAggregate`** + post-inc/dec on property/index lvalues.

### 🎯 North star — WordPress boots in the browser
- [x] pcre (regex) + mbstring (Unicode) basics.
- [ ] **SPL** — `ArrayObject`/`ArrayIterator`/`SplStack`/`SplQueue`/… via `ArrayAccess` + the iterator support.
- [ ] **Date/time** — `date()` / `mktime()` / `strtotime()` / `DateTime`.
- [ ] **Reflection.**
- [ ] **Request lifecycle** — superglobals, the Playground host interface.
- [ ] **pdo_sqlite** — WordPress-in-Playground runs on SQLite, not MySQL.
- [ ] **WordPress boots in the browser** — compiled to WASM, smaller than the Emscripten build.

### Runner TODO
- [x] `--EXPECTF--` matcher (hand-rolled; makes ~8k more tests gradeable)
- [ ] honor `--SKIPIF--` / `--EXTENSIONS--`
- [ ] verify curated smoke tests against a reference PHP (Docker `php:8.3-cli`)
"#;
