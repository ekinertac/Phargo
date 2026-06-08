--TEST--
array_fill/combine/column/product/diff, ctype_*, substr_count
--FILE--
<?php
echo implode(",", array_fill(0, 3, "x")), "\n";
echo implode(",", array_combine(["a", "b"], [1, 2])), "\n";
$rows = [["id" => 1, "name" => "A"], ["id" => 2, "name" => "B"]];
echo implode(",", array_column($rows, "name")), "\n";
echo array_product([2, 3, 4]), "\n";
echo (ctype_digit("12345") ? "y" : "n"), (ctype_alpha("abc") ? "y" : "n"), (ctype_digit("12a") ? "y" : "n"), "\n";
echo substr_count("hello world hello", "hello"), "\n";
echo implode(",", array_diff([1, 2, 3, 4], [2, 4]));
--EXPECT--
x,x,x
1,2
A,B
24
yyn
2
1,3
