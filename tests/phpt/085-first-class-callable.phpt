--TEST--
first-class callable syntax foo(...)
--FILE--
<?php
$f = strlen(...);
echo $f("hello"), "\n";
$nums = array_map(intval(...), ["1", "2", "3"]);
echo array_sum($nums), "\n";
$up = array_map(strtoupper(...), ["a", "b", "c"]);
echo implode("", $up), "\n";
function dbl($x) { return $x * 2; }
$d = dbl(...);
echo $d(21), "\n";
--EXPECT--
5
6
ABC
42
