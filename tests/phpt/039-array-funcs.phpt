--TEST--
implode and array_keys
--FILE--
<?php
$a = [3, 1, 2];
echo implode("-", $a);
echo "\n";
$b = array_keys(["a" => 1, "b" => 2]);
echo implode(",", $b);
--EXPECT--
3-1-2
a,b
