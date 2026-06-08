--TEST--
string built-ins
--FILE--
<?php echo strtoupper("abc"), strtolower("XYZ"), str_repeat("ab", 3);
--EXPECT--
ABCxyzababab
