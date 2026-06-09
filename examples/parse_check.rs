// Parse a sample and pretty-print the AST to verify the parser.
use phargo::lang::lexer::Lexer;
use phargo::lang::parser::Parser;

fn main() {
    let sample = br#"<?php
$x = 1 + 2 * 3 ** 2;
$y = $a ?? $b ?: $c;
echo "sum=$x\n";
function fib($n) {
    if ($n < 2) return $n;
    return fib($n - 1) + fib($n - 2);
}
$arr = [1, 2, 'k' => 3, ...$rest];
foreach ($arr as $k => &$v) { $v = $v * 2; }
$f = fn($z) => $z + $x;
$r = match($x) { 1, 2 => 'low', default => 'hi' };
class Point extends Base implements Drawable {
    public const ORIGIN = 0;
    private int $x = 0;
    public static function make(int $a, int $b = 1): self { return new self($a); }
}
try { risky(); } catch (\RuntimeException | LogicError $e) { echo $e->getMessage(); } finally { cleanup(); }
$obj?->maybe()->chain[0]::CONST;
?>"#;

    let toks = match Lexer::tokenize(sample) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("LEX ERROR @{}: {}", e.pos, e.msg);
            std::process::exit(1);
        }
    };
    match Parser::parse(toks) {
        Ok(stmts) => {
            println!("{} top-level statements\n", stmts.len());
            for s in &stmts {
                println!("{s:#?}");
            }
        }
        Err(e) => {
            eprintln!("PARSE ERROR @{}: {}", e.pos, e.msg);
            std::process::exit(1);
        }
    }
}
