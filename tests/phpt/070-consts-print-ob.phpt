--TEST--
const/define, print, clone, declare, output buffering, magic constants
--FILE--
<?php
declare(strict_types=1);
const FOO = 10;
define('BAR', 20);
echo FOO + BAR, "\n";
var_dump(defined('BAR'));
var_dump(defined('NOPE'));
echo constant('FOO'), "\n";
$r = print "hello\n";
echo $r, "\n";
class Box { public $v = 1; }
$a = new Box();
$a->v = 5;
$b = clone $a;
$b->v = 9;
echo $a->v, $b->v, "\n";
ob_start();
echo "buffered";
$c = ob_get_clean();
echo strtoupper($c), "\n";
echo __NAMESPACE__ === "" ? "ns-empty\n" : "ns?\n";
class C { public function m() { return __CLASS__; } }
$cc = new C();
echo $cc->m(), "\n";
--EXPECT--
30
bool(true)
bool(false)
10
hello
1
59
BUFFERED
ns-empty
C
