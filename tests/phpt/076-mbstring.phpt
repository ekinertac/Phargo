--TEST--
mbstring basics on UTF-8
--FILE--
<?php
$s = "héllo wörld";
echo mb_strlen($s), "\n";
echo strlen($s), "\n";
echo mb_substr($s, 0, 5), "\n";
echo mb_substr($s, -5), "\n";
echo mb_strtoupper("café"), "\n";
echo mb_strtolower("CAFÉ"), "\n";
echo mb_convert_case("hello world", 2), "\n";
echo mb_strpos($s, "wörld"), "\n";
var_dump(mb_strpos($s, "xyz"));
echo mb_ord("A"), "\n";
echo mb_chr(233), "\n";
print_r(mb_str_split("abcd", 2));
echo mb_internal_encoding(), "\n";
--EXPECT--
11
13
héllo
wörld
CAFÉ
café
Hello World
6
bool(false)
65
é
Array
(
    [0] => ab
    [1] => cd
)
UTF-8
