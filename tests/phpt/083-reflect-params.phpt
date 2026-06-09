--TEST--
Reflection params/constants, SplFixedArray::fromArray, ArrayIterator sort
--FILE--
<?php
function f($a, $b = 1, ...$c) {}
$r = new ReflectionFunction("f");
echo $r->getNumberOfParameters(), " ", $r->getNumberOfRequiredParameters(), "\n";
$names = [];
foreach ($r->getParameters() as $p) { $names[] = $p->getName() . ($p->isVariadic() ? "..." : ""); }
echo implode(",", $names), "\n";

class K { const X = 42; const Y = "hi"; public $z = 7; }
$rc = new ReflectionClass("K");
$consts = $rc->getConstants();
echo $consts["X"], " ", $consts["Y"], "\n";
echo $rc->getConstant("X"), "\n";

$fa = SplFixedArray::fromArray([10, 20, 30]);
echo $fa[1], " ", count($fa), "\n";

$ai = new ArrayIterator([3, 1, 2]);
$ai->asort();
echo implode(",", $ai->getArrayCopy()), "\n";

$d = new DateTime("2020-06-15");
echo $d->getTimezone()->getName(), "\n";
--EXPECT--
3 1
a,b,c...
42 hi
42
20 3
1,2,3
UTC
