--TEST--
SPL: ArrayObject, ArrayIterator, SplStack, SplQueue, SplFixedArray, SplObjectStorage
--FILE--
<?php
$ao = new ArrayObject(["a" => 1, "b" => 2]);
$ao["c"] = 3;
$ao[] = 4;
echo count($ao), "\n";
$sum = 0;
foreach ($ao as $k => $v) { $sum += $v; }
echo $sum, "\n";

$st = new SplStack();
$st->push(1); $st->push(2); $st->push(3);
echo $st->top(), " ", $st->count(), "\n";
echo $st->pop(), $st->pop(), "\n";
$out = "";
$st->push(10); $st->push(20);
foreach ($st as $v) { $out .= $v . ","; }   // LIFO
echo $out, "\n";

$q = new SplQueue();
$q->enqueue("x"); $q->enqueue("y");
echo $q->dequeue(), $q->dequeue(), "\n";

$fa = new SplFixedArray(3);
$fa[0] = "p"; $fa[1] = "q";
echo $fa[0], $fa[1], " ", count($fa), "\n";

$s = new SplObjectStorage();
$o1 = new stdClass(); $o2 = new stdClass();
$s->attach($o1, "data1");
$s->attach($o2);
echo $s->count(), " ", $s->contains($o1) ? "yes" : "no", "\n";
echo $s[$o1], "\n";
$s->detach($o1);
echo $s->count(), "\n";

$it = new ArrayIterator([10, 20, 30]);
$t = 0;
foreach ($it as $v) { $t += $v; }
echo $t, "\n";
--EXPECT--
4
10
3 3
32
20,10,1,
xy
pq 3
2 yes
data1
1
60
