--TEST--
list() / [] array destructuring (skips and nesting)
--FILE--
<?php
[$a, $b, $c] = [1, 2, 3];
echo "$a$b$c\n";
list($x, , $z) = [10, 20, 30];
echo "$x$z\n";
[[$p, $q], $r] = [[1, 2], 3];
echo "$p$q$r";
--EXPECT--
123
1030
123
