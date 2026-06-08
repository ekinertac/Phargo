--TEST--
if / else
--FILE--
<?php if (false) { echo "a"; } else { echo "b"; }
--EXPECT--
b
