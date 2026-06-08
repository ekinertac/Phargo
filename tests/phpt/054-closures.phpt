--TEST--
closures: anonymous functions, arrow functions, use(), direct invocation
--FILE--
<?php
$nums = [1, 2, 3, 4];
$double = function ($x) { return $x * 2; };
echo implode(",", array_map($double, $nums)), "\n";
$factor = 10;
$scale = fn ($x) => $x * $factor;
echo implode(",", array_map($scale, $nums)), "\n";
$prefix = "#";
$label = function ($x) use ($prefix) { return $prefix . $x; };
echo $label(7), "\n";
echo $double(21);
--EXPECT--
2,4,6,8
10,20,30,40
#7
42
