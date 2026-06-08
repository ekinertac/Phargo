--TEST--
break and continue inside while
--FILE--
<?php
$i = 0;
while ($i < 10) {
    $i = $i + 1;
    if ($i === 2) { continue; }
    if ($i === 4) { break; }
    echo $i;
}
--EXPECT--
13
