--TEST--
pre- and post-increment
--FILE--
<?php
$i = 5;
echo $i++;
echo $i;
echo ++$i;
--EXPECT--
567
