--TEST--
function scope is isolated from the caller
--FILE--
<?php
$x = 10;
function f() { return $x; }
echo "[", f(), "]";
echo $x;
--EXPECT--
[]10
