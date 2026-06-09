// End-to-end: lex -> parse -> evaluate a real PHP program, print its output.
use phargo::lang::eval::Eval;
use phargo::lang::lexer::Lexer;
use phargo::lang::parser::Parser;

fn run(src: &[u8]) -> String {
    let toks = Lexer::tokenize(src).expect("lex");
    let ast = Parser::parse(toks).expect("parse");
    let out = Eval::run(&ast).expect("eval");
    String::from_utf8_lossy(&out).into_owned()
}

fn main() {
    let prog = br#"<?php
function fib($n) {
    if ($n < 2) return $n;
    return fib($n - 1) + fib($n - 2);
}
for ($i = 0; $i < 10; $i++) {
    echo fib($i), $i < 9 ? "," : "\n";
}
$a = ['x' => 1, 'y' => 2, 'z' => 3];
$sum = 0;
foreach ($a as $k => $v) { $sum += $v; echo "$k=$v "; }
echo "\nsum=$sum\n";
$names = ["alice", "bob", "carol"];
echo implode(", ", array_map_upper($names)), "\n";
echo strtoupper("hello") . " " . str_repeat("ab", 3) . "\n";
printf("%05d %.2f %s\n", 42, 3.14159, "ok");
var_dump([1, "two", 3.5, true, null]);
echo match(2) { 1 => "one", 2, 3 => "two-three", default => "other" }, "\n";

function array_map_upper($arr) {
    $out = [];
    foreach ($arr as $s) { $out[] = strtoupper($s); }
    return $out;
}
"#;
    print!("{}", run(prog));
}
