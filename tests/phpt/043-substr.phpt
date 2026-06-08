--TEST--
substr with positive and negative offsets
--FILE--
<?php
echo substr("Hello, World", 0, 5), "\n";
echo substr("Hello", -3), "\n";
echo substr("Hello", 1, -1);
--EXPECT--
Hello
llo
ell
