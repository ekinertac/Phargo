--TEST--
echo a comma-separated argument list
--FILE--
<?php echo "a", "b", "c";
--EXPECT--
abc
