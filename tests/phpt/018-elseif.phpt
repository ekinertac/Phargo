--TEST--
if / elseif / else chain
--FILE--
<?php
$x = 2;
if ($x === 1) { echo "one"; }
elseif ($x === 2) { echo "two"; }
else { echo "other"; }
--EXPECT--
two
