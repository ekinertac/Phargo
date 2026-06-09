--TEST--
array_splice, array_diff_key/intersect_key, array_count_values, array_find, str_getcsv
--FILE--
<?php
$a = [1, 2, 3, 4, 5];
$r = array_splice($a, 1, 2, ["a", "b", "c"]);
echo implode(",", $a), " / ", implode(",", $r), "\n";

print_r(array_diff_key(["a" => 1, "b" => 2, "c" => 3], ["b" => 9]));
print_r(array_intersect_key(["a" => 1, "b" => 2, "c" => 3], ["b" => 9, "c" => 8]));
print_r(array_count_values([1, 1, 2, "x", "x", "x"]));
echo array_find([1, 3, 5, 8, 9], fn($v) => $v % 2 == 0), "\n";
var_dump(array_all([2, 4, 6], fn($v) => $v % 2 == 0));
var_dump(array_any([1, 3, 4], fn($v) => $v % 2 == 0));
print_r(array_replace(["a" => 1, "b" => 2], ["b" => 9, "c" => 3]));
print_r(str_getcsv('a,"b,c",d'));
--EXPECT--
1,a,b,c,4,5 / 2,3
Array
(
    [a] => 1
    [c] => 3
)
Array
(
    [b] => 2
    [c] => 3
)
Array
(
    [1] => 2
    [2] => 1
    [x] => 3
)
8
bool(true)
bool(true)
Array
(
    [a] => 1
    [b] => 9
    [c] => 3
)
Array
(
    [0] => a
    [1] => b,c
    [2] => d
)
