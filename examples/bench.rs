//! The Phase 2 stopwatch: measures the engine on the workloads that matter,
//! so "the VM beats the walker" is a number, not a feeling.
//!
//! Two suites:
//!   micro — language-core hot paths (loops, calls, arrays, strings, objects).
//!           These are the shapes the bytecode VM must win first.
//!   wp    — the real gauge: a full WordPress front-page request against the
//!           installed SQLite database (needs fetch-wp.sh + wpinstall).
//!
//! Usage:
//!   cargo run --release --example bench            # micro suite
//!   cargo run --release --example bench -- wp     # WordPress page render
//!   cargo run --release --example bench -- all    # both
//!
//! Each benchmark reports the best of N runs (steady-state, minimizes noise);
//! the goal line for `wp` is < 1000 ms.

use std::path::PathBuf;
use std::time::Instant;

const MICRO: &[(&str, &str)] = &[
    (
        "loop-arith",
        r#"<?php $s = 0; for ($i = 0; $i < 1000000; $i++) { $s += $i * 2 - 1; } echo $s;"#,
    ),
    (
        "fn-calls",
        r#"<?php function f($a, $b) { return $a + $b; } $s = 0; for ($i = 0; $i < 200000; $i++) { $s = f($s, $i); } echo $s;"#,
    ),
    (
        "method-calls",
        r#"<?php class C { private $n = 0; public function add($x) { $this->n += $x; return $this->n; } }
$c = new C; $s = 0; for ($i = 0; $i < 200000; $i++) { $s = $c->add(1); } echo $s;"#,
    ),
    (
        "array-build",
        r#"<?php $a = []; for ($i = 0; $i < 200000; $i++) { $a[] = $i; } echo count($a), ' ', $a[199999];"#,
    ),
    (
        "assoc-rw",
        r#"<?php $a = []; for ($i = 0; $i < 100000; $i++) { $a["k$i"] = $i; } $s = 0; for ($i = 0; $i < 100000; $i++) { $s += $a["k$i"]; } echo $s;"#,
    ),
    (
        "string-concat",
        r#"<?php $s = ''; for ($i = 0; $i < 100000; $i++) { $s .= 'ab'; } echo strlen($s);"#,
    ),
    (
        "foreach-sum",
        r#"<?php $a = range(1, 300000); $s = 0; foreach ($a as $v) { $s += $v; } echo $s;"#,
    ),
    (
        "str-builtins",
        r#"<?php $s = 0; for ($i = 0; $i < 50000; $i++) { $s += strlen(str_replace('a', 'bb', 'banana')) + strpos('hello world', 'world'); } echo $s;"#,
    ),
    (
        "preg",
        r#"<?php $n = 0; for ($i = 0; $i < 20000; $i++) { if (preg_match('/(\d+)-(\d+)/', "order $i-42 shipped", $m)) { $n += (int) $m[2]; } } echo $n;"#,
    ),
    (
        "recursion",
        r#"<?php function fib($n) { return $n < 2 ? $n : fib($n - 1) + fib($n - 2); } echo fib(22);"#,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode: String = args.first().cloned().unwrap_or_else(|| "micro".to_string());
    if std::env::var("PHARGO_STEP_LIMIT").is_err() {
        std::env::set_var("PHARGO_STEP_LIMIT", "3000000000");
    }
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(move || {
            if mode == "micro" || mode == "all" {
                run_micro();
            }
            if mode == "wp" || mode == "all" {
                run_wp();
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

fn best_of(n: usize, mut f: impl FnMut() -> Option<usize>) -> Option<(u128, usize)> {
    let mut best: Option<(u128, usize)> = None;
    for _ in 0..n {
        let t = Instant::now();
        let out_len = f()?;
        let el = t.elapsed().as_millis();
        if best.map(|(b, _)| el < b).unwrap_or(true) {
            best = Some((el, out_len));
        }
    }
    best
}

fn run_micro() {
    println!("== micro suite (best of 3, ms) ==");
    let mut total = 0u128;
    for (name, code) in MICRO {
        match best_of(3, || phargo::run(code).ok().map(|o| o.len())) {
            Some((ms, _)) => {
                total += ms;
                println!("{name:>14}  {ms:>6} ms");
            }
            None => println!("{name:>14}   ERROR"),
        }
    }
    println!("{:>14}  {total:>6} ms", "TOTAL");
}

fn run_wp() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wp = root.join("vendor").join("wordpress");
    if !wp.join("wp-content/database/.ht.sqlite").exists() {
        eprintln!("wp bench needs vendor/wordpress + installed DB (fetch-wp.sh, wpinstall)");
        return;
    }
    let driver = format!(
        r#"<?php
$_SERVER['HTTP_HOST'] = 'localhost';
$_SERVER['REQUEST_URI'] = '/';
$_SERVER['REQUEST_METHOD'] = 'GET';
$_SERVER['SERVER_PROTOCOL'] = 'HTTP/1.1';
$_SERVER['SERVER_NAME'] = 'localhost';
$_SERVER['SCRIPT_NAME'] = '/index.php';
$_SERVER['SCRIPT_FILENAME'] = '{wp}/index.php';
$_SERVER['PHP_SELF'] = '/index.php';
$_SERVER['DOCUMENT_ROOT'] = '{wp}';
$_SERVER['REMOTE_ADDR'] = '127.0.0.1';
define('WP_USE_THEMES', true);
require '{wp}/wp-blog-header.php';
"#,
        wp = wp.display()
    );
    println!("== wp front page (best of 3, goal < 1000 ms) ==");
    match best_of(3, || {
        phargo::run_with_path(&driver, Some(wp.join("index.php")))
            .ok()
            .map(|o| o.len())
    }) {
        Some((ms, bytes)) => println!("{:>14}  {ms:>6} ms   ({bytes} bytes)", "wp-front-page"),
        None => println!("{:>14}   ERROR", "wp-front-page"),
    }
}
