--TEST--
chained assignment is an expression
--FILE--
<?php
$a = $b = 4;
echo $a + $b;
--EXPECT--
8
