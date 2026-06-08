--TEST--
serialize / unserialize round-trip
--FILE--
<?php
echo serialize(42), "\n";
echo serialize("hi"), "\n";
echo serialize(true), "\n";
echo serialize(null), "\n";
echo serialize(3.5), "\n";
echo serialize([1, 2, "k" => "v"]), "\n";
$o = new stdClass();
$o->a = 1;
$o->b = "x";
echo serialize($o), "\n";
$back = unserialize('a:2:{i:0;s:3:"foo";i:1;i:99;}');
echo $back[0], $back[1], "\n";
$ro = unserialize(serialize($o));
echo $ro->a, $ro->b, "\n";
$arr = unserialize(serialize([5, ["nested" => true]]));
var_dump($arr[1]["nested"]);
--EXPECT--
i:42;
s:2:"hi";
b:1;
N;
d:3.5;
a:3:{i:0;i:1;i:1;i:2;s:1:"k";s:1:"v";}
O:8:"stdClass":2:{s:1:"a";i:1;s:1:"b";s:1:"x";}
foo99
1x
bool(true)
