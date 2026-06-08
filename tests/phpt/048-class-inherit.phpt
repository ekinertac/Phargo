--TEST--
inheritance, method override, polymorphic $this dispatch
--FILE--
<?php
class Animal {
    public $name;
    public function __construct($name) { $this->name = $name; }
    public function speak() { return "..."; }
    public function describe() { return $this->name . " says " . $this->speak(); }
}
class Dog extends Animal {
    public function speak() { return "Woof"; }
}
$d = new Dog("Rex");
echo $d->describe();
--EXPECT--
Rex says Woof
