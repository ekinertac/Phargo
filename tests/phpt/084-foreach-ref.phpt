--TEST--
foreach by-reference, list destructuring, call-time &
--FILE--
<?php
$a = [1, 2, 3];
foreach ($a as &$v) { $v *= 10; }
unset($v);
echo implode(",", $a), "\n";

$a2 = ["x" => 1, "y" => 2];
foreach ($a2 as $k => &$v) { $v = strtoupper($k); }
unset($v);
echo implode(",", $a2), "\n";

$pairs = [[1, 2], [3, 4], [5, 6]];
$sums = [];
foreach ($pairs as [$x, $y]) { $sums[] = $x + $y; }
echo implode(",", $sums), "\n";

$labels = [];
foreach ($pairs as $i => [$x, $y]) { $labels[] = "$i:$x$y"; }
echo implode(",", $labels), "\n";

function inc($n) { return $n + 1; }
$z = 41;
echo inc(&$z), "\n";
--EXPECT--
10,20,30
X,Y
3,7,11
0:12,1:34,2:56
42
