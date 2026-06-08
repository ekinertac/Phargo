--TEST--
match expression (match(true), multi-condition arms, default)
--FILE--
<?php
function classify($n) {
    return match (true) {
        $n < 0 => "negative",
        $n === 0 => "zero",
        $n < 10 => "small",
        default => "big",
    };
}
echo classify(-5), "\n";
echo classify(0), "\n";
echo classify(7), "\n";
echo classify(100), "\n";
echo match (2) { 1 => "one", 2, 3 => "two-or-three", default => "other" };
--EXPECT--
negative
zero
small
big
two-or-three
