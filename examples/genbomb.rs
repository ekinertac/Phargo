// Verify the infinite-generator memory bomb (bug71297) is caught by the
// per-test generator node cap — must return an error, not exhaust the heap.
use phargo::run;

fn main() {
    let src = r#"<?php
function foo() {
    yield array_fill(0, 10000, 4);
}
function genLeak() {
    $i = 0;
    while (1) {
        yield from foo();
        print $i++;
    }
}
$x = 0;
foreach (genLeak() as $i) {
    if ($x++ == 3) break;
}
"#;
    match run(src) {
        Ok(s) => println!("OK output={:?}", s),
        Err(e) => println!("ERR (expected, graceful): {}", e),
    }
}
