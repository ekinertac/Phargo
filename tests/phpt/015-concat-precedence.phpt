--TEST--
PHP 8: + binds tighter than . (concatenation)
--FILE--
<?php echo "sum: " . 1 + 2;
--EXPECT--
sum: 3
