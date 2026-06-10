// Generators / yield — finite cases only (eager generators run body to completion).
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
function gen() { yield 1; yield 2; yield 3; }
$sum = 0;
foreach (gen() as $v) { $sum += $v; }
echo "sum=$sum\n";

function kv() { yield 'a' => 1; yield 'b' => 2; }
foreach (kv() as $k => $v) { echo "$k=$v "; }
echo "\n";

function squares($n) { for ($i = 1; $i <= $n; $i++) { yield $i * $i; } }
echo implode(",", iterator_to_array(squares(5))), "\n";

function withReturn() { yield 1; yield 2; return 99; }
$g = withReturn();
foreach ($g as $v) {}
echo "ret=", $g->getReturn(), "\n";

function delegating() { yield 0; yield from [10, 20, 30]; yield 99; }
echo implode(",", iterator_to_array(delegating(), false)), "\n";
"#;
    print!("{}", run(prog));
}
