--TEST--
var_dump of scalar values
--FILE--
<?php
var_dump(42);
var_dump(1.5);
var_dump(true);
var_dump(false);
var_dump("hi");
var_dump(null);
--EXPECT--
int(42)
float(1.5)
bool(true)
bool(false)
string(2) "hi"
NULL
