--TEST--
** is right-associative
--FILE--
<?php echo 2 ** 3 ** 2;
--EXPECT--
512
