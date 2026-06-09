--TEST--
__invoke callable objects + is_callable + similar_text
--FILE--
<?php
class Multiplier {
    public function __construct(private int $factor) {}
    public function __invoke($x) { return $x * $this->factor; }
}
$triple = new Multiplier(3);
echo $triple(7), "\n";
echo array_sum(array_map($triple, [1, 2, 3])), "\n";
var_dump(is_callable($triple));
var_dump(is_callable("strlen"));
var_dump(is_callable(42));
echo call_user_func($triple, 10), "\n";
echo similar_text("World", "word"), "\n";
echo similar_text("Hello World", "Hello PHP World"), "\n";
--EXPECT--
21
18
bool(true)
bool(true)
bool(false)
30
3
11
