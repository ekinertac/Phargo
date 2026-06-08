--TEST--
isset / empty / unset on variables, array elements
--FILE--
<?php
$a = 5;
$b = null;
$arr = ["x" => 1];
echo isset($a) ? "1" : "0";
echo isset($b) ? "1" : "0";
echo isset($c) ? "1" : "0";
echo isset($arr["x"]) ? "1" : "0";
echo isset($arr["y"]) ? "1" : "0";
echo "\n";
echo empty($a) ? "1" : "0";
echo empty($b) ? "1" : "0";
echo empty($arr["y"]) ? "1" : "0";
echo "\n";
unset($a);
echo isset($a) ? "1" : "0";
unset($arr["x"]);
echo count($arr);
--EXPECT--
10010
011
00
