// Quick lexer smoke check: tokenize a sample and print the token kinds.
use phargo::lang::lexer::Lexer;

fn main() {
    let sample = br#"<h1>Title</h1>
<?php
$name = "World";
$n = 0x1F + 0b1010 + 1_000 + .5 + 2e3;
echo "Hello, {$name}! count=$n\n";
function add($a, $b = 1, ...$rest) { return $a + $b; }
$arr = ['x' => 1, 'y' => 2];
if ($n >= 10 && $name !== '') { $n ??= 5; }
$txt = <<<EOT
  line $name here
  EOT;
echo strlen($txt) <=> 3;
?>
trailing html
"#;
    match Lexer::tokenize(sample) {
        Ok(toks) => {
            println!("{} tokens", toks.len());
            for t in &toks {
                println!("{:?}", t.kind);
            }
        }
        Err(e) => {
            eprintln!("LEX ERROR at {}: {}", e.pos, e.msg);
            std::process::exit(1);
        }
    }
}
