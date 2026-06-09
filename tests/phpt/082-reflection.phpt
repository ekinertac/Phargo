--TEST--
introspection + dynamic features + Reflection
--FILE--
<?php
interface Speaks {}
class Animal { public $legs = 4; public function name() { return "animal"; } }
class Dog extends Animal implements Speaks { public $breed = "mutt"; public function speak() { return "woof"; } }

$d = new Dog();
echo get_class($d), "\n";
echo get_parent_class($d), "\n";
var_dump(method_exists($d, "speak"));
var_dump(property_exists($d, "legs"));
var_dump(is_a($d, "Animal"));
var_dump(is_subclass_of($d, "Animal"));
echo implode(",", array_keys(class_implements($d))), "\n";
echo implode(",", array_keys(class_parents($d))), "\n";
print_r(get_object_vars($d));

// dynamic new + spread + variable method/prop
$cls = "Dog";
$d2 = new $cls();
echo $d2->speak(), "\n";
$m = "name";
echo $d2->$m(), "\n";
$p = "breed";
echo $d2->$p, "\n";
$d2->$p = "lab";
echo $d2->breed, "\n";
function sum(...$xs) { return array_sum($xs); }
$args = [1, 2, 3, 4];
echo sum(...$args), "\n";

// Reflection
$rc = new ReflectionClass("Dog");
echo $rc->getName(), "\n";
echo $rc->getParentClass()->getName(), "\n";
var_dump($rc->hasMethod("speak"));
$inst = $rc->newInstance();
echo $inst->speak(), "\n";
$rm = new ReflectionMethod("Dog", "speak");
echo $rm->invoke($d2), "\n";
--EXPECT--
Dog
Animal
bool(true)
bool(true)
bool(true)
bool(true)
speaks
Animal
Array
(
    [legs] => 4
    [breed] => mutt
)
woof
animal
mutt
lab
10
Dog
Animal
bool(true)
woof
woof
