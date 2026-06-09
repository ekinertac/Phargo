// End-to-end OOP check for the v2 engine.
use phargo::lang::eval::Eval;
use phargo::lang::lexer::Lexer;
use phargo::lang::parser::Parser;

fn run(src: &[u8]) -> String {
    let toks = Lexer::tokenize(src).expect("lex");
    let ast = Parser::parse(toks).expect("parse");
    match Eval::run(&ast) {
        Ok(out) => String::from_utf8_lossy(&out).into_owned(),
        Err(e) => format!("ERROR: {}", e.0),
    }
}

fn main() {
    let prog = br#"<?php
class Animal {
    public const KINGDOM = "Animalia";
    protected string $name;
    public function __construct(string $name) { $this->name = $name; }
    public function speak(): string { return "..."; }
    public function describe(): string {
        return $this->name . " says " . $this->speak();
    }
    public function __toString(): string { return "Animal(" . $this->name . ")"; }
}
class Dog extends Animal {
    public function speak(): string { return "Woof"; }
    public static function species(): string { return "Canis"; }
}
class Counter {
    public function __construct(public int $count = 0, private string $label = "n") {}
    public function inc(): void { $this->count++; }
    public function get(): int { return $this->count; }
}

$d = new Dog("Rex");
echo $d->describe(), "\n";          // polymorphism: Rex says Woof
echo Animal::KINGDOM, "\n";          // class constant via subclass
echo Dog::species(), "\n";           // static method
echo $d, "\n";                       // __toString
var_dump($d instanceof Animal);      // inheritance check
var_dump($d instanceof Counter);

$c = new Counter(5);
$c->inc(); $c->inc();
echo "count=", $c->get(), "\n";      // promoted property mutation -> 7

$nums = [];
for ($i = 1; $i <= 3; $i++) { $nums[] = new Counter($i); }
foreach ($nums as $cc) { echo $cc->get(); }
echo "\n";
"#;
    print!("{}", run(prog));
}
