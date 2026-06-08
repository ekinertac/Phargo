--TEST--
PHP_EOL constant and assorted string builtins
--FILE--
<?php
echo "line" . PHP_EOL;
echo str_pad("7", 3, "0", STR_PAD_LEFT), PHP_EOL;
echo ucwords("hello world"), PHP_EOL;
echo number_format(1234567.891, 2), PHP_EOL;
echo (str_contains("hello", "ell") ? "yes" : "no"), PHP_EOL;
echo max([3, 9, 4]), " ", min(5, 2, 8), PHP_EOL;
echo dechex(255);
--EXPECT--
line
007
Hello World
1,234,567.89
yes
9 2
ff
