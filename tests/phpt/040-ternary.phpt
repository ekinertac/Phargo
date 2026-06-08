--TEST--
ternary and short ternary
--FILE--
<?php
echo true ? "yes" : "no";
echo "\n";
echo 0 ? "a" : "b";
echo "\n";
echo "x" ?: "fallback";
echo "\n";
echo "" ?: "fallback";
--EXPECT--
yes
b
x
fallback
