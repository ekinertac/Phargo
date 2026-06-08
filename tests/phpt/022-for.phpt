--TEST--
for loop
--FILE--
<?php
for ($i = 0; $i < 3; $i++) {
    echo $i;
}
--EXPECT--
012
