--TEST--
math built-ins
--FILE--
<?php echo abs(-5), " ", max(3, 7), " ", min(3, 7), " ", intdiv(7, 2);
--EXPECT--
5 7 3 3
