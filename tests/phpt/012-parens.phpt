--TEST--
parentheses override precedence
--FILE--
<?php echo (1 + 2) * 3;
--EXPECT--
9
