--TEST--
line and block comments
--FILE--
<?php
// a line comment
echo "ok"; # a hash comment
/* a block
   comment */ echo "!";
--EXPECT--
ok!
