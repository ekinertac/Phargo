--TEST--
by-reference sort family and array_push/pop/shift
--FILE--
<?php
$a = [3, 1, 2];
sort($a);
echo implode(",", $a), "\n";
rsort($a);
echo implode(",", $a), "\n";
$b = [3, 1, 2];
usort($b, fn ($x, $y) => $x - $y);
echo implode(",", $b), "\n";
$c = [1, 2];
array_push($c, 3, 4);
echo implode(",", $c), " count=", count($c), "\n";
echo array_pop($c), "\n";
echo array_shift($c), "\n";
echo implode(",", $c), "\n";
$d = ["b" => 2, "a" => 1, "c" => 3];
ksort($d);
echo implode(",", array_keys($d));
--EXPECT--
1,2,3
3,2,1
1,2,3
1,2,3,4 count=4
4
1
2,3
a,b,c
