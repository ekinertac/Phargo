--TEST--
enums: pure, backed, methods, cases/from/tryFrom
--FILE--
<?php
enum Suit {
    case Hearts;
    case Spades;
    public function color(): string {
        return match($this) {
            Suit::Hearts => "red",
            Suit::Spades => "black",
        };
    }
}
enum Status: string {
    case Active = 'A';
    case Closed = 'C';
}
$h = Suit::Hearts;
echo $h->name, "\n";
echo $h->color(), "\n";
echo Suit::Spades->color(), "\n";
echo count(Suit::cases()), "\n";
echo Status::Active->value, "\n";
$s = Status::from('C');
echo $s->name, "\n";
var_dump(Status::tryFrom('X'));
var_dump($h instanceof Suit);
--EXPECT--
Hearts
red
black
2
A
Closed
NULL
bool(true)
