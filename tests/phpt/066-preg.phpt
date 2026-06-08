--TEST--
preg_match / preg_match_all / preg_replace / preg_split
--FILE--
<?php
// basic match + captures
if (preg_match('/(\d+)-(\d+)/', 'order 12-345 done', $m)) {
    echo $m[0], "|", $m[1], "|", $m[2], "\n";
}
// case-insensitive, anchors
echo preg_match('/^hello/i', 'HELLO world'), "\n";
echo preg_match('/world$/', 'hello world'), "\n";
// no match
echo preg_match('/xyz/', 'abc'), "\n";
// match_all pattern order
preg_match_all('/\d+/', 'a1b22c333', $all);
echo implode(",", $all[0]), "\n";
// replace with backrefs
echo preg_replace('/(\w+)\s(\w+)/', '$2 $1', 'hello world'), "\n";
// replace all digits
echo preg_replace('/\d/', '#', 'a1b2c3'), "\n";
// split
print_r(preg_split('/[\s,]+/', "a, b,c  d"));
// alternation + quantifiers
echo preg_match('/colou?r/', 'color'), "\n";
echo preg_match('/(cat|dog)s?/', 'dogs'), "\n";
// char class + boundary
echo preg_replace('/\bfoo\b/', 'BAR', 'foo foobar foo'), "\n";
// callback
echo preg_replace_callback('/\d+/', function($m){ return $m[0]*2; }, 'a3b4'), "\n";
--EXPECT--
12-345|12|345
1
1
0
1,22,333
world hello
a#b#c#
Array
(
    [0] => a
    [1] => b
    [2] => c
    [3] => d
)
1
1
BAR foobar BAR
a6b8
