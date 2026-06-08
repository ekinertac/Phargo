--TEST--
var_dump of an object
--FILE--
<?php
class C {
    public $a = 1;
    public $b = 2;
}
var_dump(new C());
--EXPECT--
object(C)#1 (2) {
  ["a"]=>
  int(1)
  ["b"]=>
  int(2)
}
