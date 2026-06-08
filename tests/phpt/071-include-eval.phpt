--TEST--
include / require / require_once / eval
--FILE--
<?php
$ret = include __DIR__ . '/fixtures/helper.inc';
echo $ret, "\n";
echo inc_greet("world"), "\n";
echo INC_CONST, "\n";
var_dump($included_ran);
// require_once should not re-run / re-return
$r2 = require_once __DIR__ . '/fixtures/helper.inc';
var_dump($r2);
// missing include returns false (suppressed)
$bad = @include __DIR__ . '/fixtures/nope.inc';
var_dump($bad);
// eval
$x = eval('return 1 + 2;');
echo $x, "\n";
eval('echo "from eval\n";');
--EXPECT--
RETVAL
hello world
42
bool(true)
bool(true)
bool(false)
3
from eval
