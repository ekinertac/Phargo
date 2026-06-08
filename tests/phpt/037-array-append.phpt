--TEST--
array append and explicit index
--FILE--
<?php
$a = [];
$a[] = "first";
$a[] = "second";
$a[5] = "fifth";
$a[] = "sixth";
echo count($a), $a[0], $a[6];
--EXPECT--
4firstsixth
