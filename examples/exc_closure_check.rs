// End-to-end check for exceptions + closures in the v2 engine.
use phargo::lang::eval::Eval;
use phargo::lang::lexer::Lexer;
use phargo::lang::parser::Parser;

fn run(src: &[u8]) -> String {
    let toks = Lexer::tokenize(src).expect("lex");
    let ast = Parser::parse(toks).expect("parse");
    match Eval::run(&ast) {
        Ok(out) => String::from_utf8_lossy(&out).into_owned(),
        Err(e) => format!("UNCAUGHT: {}", e.0),
    }
}

fn main() {
    let prog = br#"<?php
// --- exceptions ---
function risky($n) {
    if ($n < 0) throw new InvalidArgumentException("negative: $n");
    if ($n == 0) throw new DivisionByZeroError("zero");
    return 100 / $n;
}
foreach ([4, 0, -1] as $n) {
    try {
        echo "risky($n) = ", risky($n), "\n";
    } catch (DivisionByZeroError $e) {
        echo "caught DivByZero: ", $e->getMessage(), "\n";
    } catch (InvalidArgumentException | LogicException $e) {
        echo "caught Logic: ", $e->getMessage(), "\n";
    } finally {
        echo "  (finally for $n)\n";
    }
}
// intdiv throws a catchable engine error
try { intdiv(1, 0); } catch (DivisionByZeroError $e) { echo "intdiv: ", $e->getMessage(), "\n"; }

// --- closures ---
$mul = 3;
$triple = function($x) use ($mul) { return $x * $mul; };
echo $triple(5), "\n";                       // 15

$nums = [1, 2, 3, 4, 5];
$squared = array_map(fn($x) => $x * $x, $nums);
echo implode(",", $squared), "\n";           // 1,4,9,16,25
$even = array_filter($nums, fn($x) => $x % 2 == 0);
echo implode(",", $even), "\n";              // 2,4
echo array_reduce($nums, fn($c, $x) => $c + $x, 0), "\n"; // 15

// closure capturing $this via a method
class Adder {
    private int $base;
    public function __construct(int $b) { $this->base = $b; }
    public function makeAdder() { return fn($x) => $x + $this->base; }
}
$add10 = (new Adder(10))->makeAdder();
echo $add10(7), "\n";                         // 17

// callable string + array callable
echo call_user_func('strtoupper', 'hi'), "\n"; // HI
"#;
    print!("{}", run(prog));
}
