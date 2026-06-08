--TEST--
keyed array literal and index read
--FILE--
<?php
$a = ["x" => 1, "y" => 2];
echo $a["x"], $a["y"];
--EXPECT--
12
