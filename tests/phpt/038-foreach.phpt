--TEST--
foreach over values and key=>value
--FILE--
<?php
$a = [10, 20, 30];
foreach ($a as $v) { echo $v, ","; }
foreach ($a as $k => $v) { echo $k, "=", $v, ";"; }
--EXPECT--
10,20,30,0=10;1=20;2=30;
