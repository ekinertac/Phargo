use phargo::run;
fn main() {
    let src = r#"<?php
enum Suit: string { case Hearts = 'H'; case Spades = 'S'; }
enum Dir { case N; case S; }
// from / tryFrom / identity
var_dump(Suit::from('H') === Suit::Hearts);   // true (singleton)
var_dump(Suit::tryFrom('Z'));                  // NULL
try { Suit::from('Z'); } catch (\ValueError $e) { echo "ValueError\n"; }
// ReflectionEnum
$re = new ReflectionEnum('Suit');
var_dump($re->isEnum());
var_dump($re->isBacked());
echo "backing=", $re->getBackingType()->getName(), "\n";
foreach ($re->getCases() as $c) {
    echo $c->getName(), "=", $c->getBackingValue(), " ";
}
echo "\n";
$ru = new ReflectionEnum('Dir');
var_dump($ru->isBacked());
var_dump(count($ru->getCases()));
var_dump($re->getCase('Hearts')->getValue() === Suit::Hearts);
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
