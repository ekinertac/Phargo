--TEST--
var_dump of an array
--FILE--
<?php
$a = [1, "two", 3.0];
var_dump($a);
--EXPECT--
array(3) {
  [0]=>
  int(1)
  [1]=>
  string(3) "two"
  [2]=>
  float(3)
}
