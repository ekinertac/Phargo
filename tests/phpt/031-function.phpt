--TEST--
user-defined function
--FILE--
<?php
function add($a, $b) { return $a + $b; }
echo add(2, 3);
--EXPECT--
5
