--TEST--
__get / __set / __call / __isset magic methods
--FILE--
<?php
class Bag {
    private $data = [];
    public function __get($k) { return $this->data[$k] ?? "unset:$k"; }
    public function __set($k, $v) { $this->data[$k] = $v; }
    public function __isset($k) { return isset($this->data[$k]); }
    public function __call($name, $args) { return "$name(" . implode(",", $args) . ")"; }
    public static function __callStatic($name, $args) { return "static:$name"; }
}
$b = new Bag();
$b->x = 10;
$b->y = 20;
echo $b->x, " ", $b->y, "\n";
echo $b->missing, "\n";
echo $b->doStuff(1, 2, 3), "\n";
$b->x = $b->x + 5;
echo $b->x, "\n";
echo "interp: $b->y\n";
--EXPECT--
10 20
unset:missing
doStuff(1,2,3)
15
interp: 20
