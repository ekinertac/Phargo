--TEST--
basic class: constructor, properties, method, $this
--FILE--
<?php
class Point {
    public $x = 0;
    public $y = 0;
    public function __construct($x, $y) {
        $this->x = $x;
        $this->y = $y;
    }
    public function sum() {
        return $this->x + $this->y;
    }
}
$p = new Point(3, 4);
echo $p->x, ",", $p->y, "=", $p->sum();
--EXPECT--
3,4=7
