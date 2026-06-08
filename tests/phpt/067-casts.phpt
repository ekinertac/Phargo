--TEST--
type casts
--FILE--
<?php
var_dump((int)"42abc");
var_dump((int)3.9);
var_dump((float)"3.14");
var_dump((bool)0);
var_dump((bool)"x");
var_dump((string)true);
var_dump((string)42);
var_dump((array)"hi");
$o = (object)["a" => 1, "b" => 2];
echo $o->a, $o->b, "\n";
var_dump((int)$o->a + (int)"5");
echo (string)(int)"7.5", "\n";
--EXPECT--
int(42)
int(3)
float(3.14)
bool(false)
bool(true)
string(1) "1"
string(2) "42"
array(1) {
  [0]=>
  string(2) "hi"
}
12
int(6)
7
