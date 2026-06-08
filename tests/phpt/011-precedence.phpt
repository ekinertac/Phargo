--TEST--
arithmetic precedence: * before +
--FILE--
<?php echo 1 + 2 * 3;
--EXPECT--
7
