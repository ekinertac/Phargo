--TEST--
strstr/stristr/strrchr/fdiv/filter_var/array_walk/class_alias/html decode
--FILE--
<?php
echo strstr("user@example.com", "@"), "\n";
echo strstr("user@example.com", "@", true), "\n";
echo stristr("HELLO world", "world"), "\n";
echo strrchr("a/b/c.txt", "/"), "\n";
var_dump(fdiv(1, 0));
var_dump(fdiv(6, 2));
var_dump(filter_var("42", FILTER_VALIDATE_INT));
var_dump(filter_var("abc", FILTER_VALIDATE_INT));
var_dump(filter_var("yes", FILTER_VALIDATE_BOOLEAN));
var_dump(filter_var("a@b.com", FILTER_VALIDATE_EMAIL));
$a = [1, 2, 3];
array_walk($a, function($v, $k) { echo "[$k=$v]"; });
echo "\n";
class Orig { public function hi() { return "hi"; } }
class_alias("Orig", "Aliased");
$x = new Aliased();
echo $x->hi(), "\n";
echo html_entity_decode("a &lt;b&gt; &amp; c"), "\n";
echo wordwrap("the quick brown fox", 10, "|"), "\n";
--EXPECT--
@example.com
user
world
/c.txt
float(INF)
float(3)
int(42)
bool(false)
bool(true)
string(7) "a@b.com"
[0=1][1=2][2=3]
hi
a <b> & c
the quick|brown fox