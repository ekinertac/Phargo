--TEST--
null coalescing operator
--FILE--
<?php
$a = null;
echo $a ?? "default";
echo "\n";
$b = ["x" => 1];
echo $b["y"] ?? "missing";
--EXPECT--
default
missing
