--TEST--
callables: array_map / array_filter / array_reduce / call_user_func
--FILE--
<?php
function dbl($x) { return $x * 2; }
function isOdd($x) { return $x % 2 === 1; }
function sum2($a, $b) { return $a + $b; }
$nums = [1, 2, 3, 4, 5];
echo implode(",", array_map('dbl', $nums)), "\n";
echo implode(",", array_filter($nums, 'isOdd')), "\n";
echo array_reduce($nums, 'sum2', 0), "\n";
echo array_search(3, $nums), "\n";
echo call_user_func('dbl', 21), "\n";
echo array_key_exists(2, $nums) ? "yes" : "no";
--EXPECT--
2,4,6,8,10
1,3,5
15
2
42
yes
