--TEST--
SplMinHeap, SplMaxHeap, SplPriorityQueue, custom SplHeap
--FILE--
<?php
$min = new SplMinHeap();
foreach ([5, 1, 3, 2, 4] as $n) { $min->insert($n); }
echo $min->count(), ": ";
$out = [];
while (!$min->isEmpty()) { $out[] = $min->extract(); }
echo implode(",", $out), "\n";

$max = new SplMaxHeap();
foreach ([5, 1, 3, 2, 4] as $n) { $max->insert($n); }
$out = [];
foreach ($max as $v) { $out[] = $v; }
echo implode(",", $out), "\n";

$pq = new SplPriorityQueue();
$pq->insert("low", 1);
$pq->insert("high", 10);
$pq->insert("mid", 5);
$out = [];
while (!$pq->isEmpty()) { $out[] = $pq->extract(); }
echo implode(",", $out), "\n";

class ByLen extends SplHeap {
    protected function compare($a, $b): int { return strlen($a) - strlen($b); }
}
$h = new ByLen();
$h->insert("aa");
$h->insert("zzzz");
$h->insert("c");
echo $h->top(), "\n";
--EXPECT--
5: 1,2,3,4,5
5,4,3,2,1
high,mid,low
zzzz
