--TEST--
echo a single-quoted string with an escaped quote
--FILE--
<?php echo 'It\'s alive';
--EXPECT--
It's alive
