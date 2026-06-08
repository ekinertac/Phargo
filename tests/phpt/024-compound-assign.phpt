--TEST--
compound assignment operators
--FILE--
<?php
$x = 10;
$x += 5;
$x -= 3;
echo $x;
--EXPECT--
12
