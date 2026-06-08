--TEST--
str_replace
--FILE--
<?php echo str_replace("world", "PHP", "hello world");
--EXPECT--
hello PHP
