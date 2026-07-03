--TEST--
function scope is isolated from the caller
--FILE--
<?php
$x = 10;
function f() { return $x ?? 'unset'; }
echo "[", f(), "]";
echo $x;
--EXPECT--
[unset]10
