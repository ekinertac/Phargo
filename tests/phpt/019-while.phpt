--TEST--
while loop with a counter
--FILE--
<?php
$i = 0;
while ($i < 3) {
    echo $i;
    $i = $i + 1;
}
--EXPECT--
012
