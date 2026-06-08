--TEST--
switch with fall-through and default
--FILE--
<?php
function t($x) {
    switch ($x) {
        case 1: echo "one"; break;
        case 2: echo "two"; break;
        case 3:
        case 4: echo "three-or-four"; break;
        default: echo "other";
    }
    echo "\n";
}
t(1);
t(2);
t(3);
t(4);
t(5);
--EXPECT--
one
two
three-or-four
three-or-four
other
