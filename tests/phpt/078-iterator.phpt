--TEST--
foreach over Iterator, IteratorAggregate, and plain object
--FILE--
<?php
class Range implements Iterator {
    private $i;
    public function __construct(private int $lo, private int $hi) { $this->i = $lo; }
    public function rewind(): void { $this->i = $this->lo; }
    public function valid(): bool { return $this->i <= $this->hi; }
    public function current(): mixed { return $this->i * 10; }
    public function key(): mixed { return $this->i; }
    public function next(): void { $this->i++; }
}
foreach (new Range(1, 3) as $k => $v) {
    echo "[$k:$v]";
}
echo "\n";

class Bag implements IteratorAggregate {
    private $items = ["a", "b", "c"];
    public function getIterator(): Iterator { return new ArrayIterator($this->items); }
}
// minimal ArrayIterator for the test
class ArrayIterator implements Iterator {
    private $keys; private $p = 0;
    public function __construct(private array $data) { $this->keys = array_keys($data); }
    public function rewind(): void { $this->p = 0; }
    public function valid(): bool { return $this->p < count($this->keys); }
    public function current(): mixed { return $this->data[$this->keys[$this->p]]; }
    public function key(): mixed { return $this->keys[$this->p]; }
    public function next(): void { $this->p++; }
}
foreach (new Bag() as $x) { echo $x; }
echo "\n";

class Point { public $x = 1; public $y = 2; }
foreach (new Point() as $k => $v) { echo "[$k=$v]"; }
echo "\n";
--EXPECT--
[1:10][2:20][3:30]
abc
[x=1][y=2]
