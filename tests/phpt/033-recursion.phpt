--TEST--
recursive function
--FILE--
<?php
function fact($n) {
    if ($n <= 1) { return 1; }
    return $n * fact($n - 1);
}
echo fact(5);
--EXPECT--
120
