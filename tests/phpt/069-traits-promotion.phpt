--TEST--
trait use, constructor promotion, attributes, typed/union members
--FILE--
<?php
trait Greet {
    public function hello() { return "hi " . $this->name; }
}
#[SomeAttr]
class Person {
    use Greet;
    public int|string $tag = "t";
    public function __construct(public string $name, private int $age = 0) {}
    public function age(): int { return $this->age; }
}
$p = new Person("Ann", 30);
echo $p->name, "\n";
echo $p->age(), "\n";
echo $p->hello(), "\n";
echo $p->tag, "\n";
--EXPECT--
Ann
30
hi Ann
t
