--TEST--
strtr, chunk_split, compact, levenshtein, array_is_list
--FILE--
<?php
echo strtr("hello", "el", "ip"), "\n";
echo strtr("the cat", ["cat" => "dog", "the" => "a"]), "\n";
echo chunk_split("abcdefgh", 3, "-"), "\n";
$a = 1;
$b = 2;
echo implode(",", array_keys(compact("a", "b"))), "\n";
echo levenshtein("kitten", "sitting"), "\n";
echo array_is_list([1, 2, 3]) ? "list" : "map", " ", array_is_list([1 => "x"]) ? "list" : "map";
--EXPECT--
hippo
a dog
abc-def-gh-
a,b
3
list map
