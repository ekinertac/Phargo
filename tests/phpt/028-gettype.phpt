--TEST--
gettype names
--FILE--
<?php echo gettype(1), " ", gettype("x"), " ", gettype(1.5), " ", gettype(true), " ", gettype(null);
--EXPECT--
integer string double boolean NULL
