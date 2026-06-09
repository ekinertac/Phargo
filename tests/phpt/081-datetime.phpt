--TEST--
date / mktime / gmdate / checkdate / strtotime / DateTime
--FILE--
<?php
// 2021-03-14 15:09:26 UTC = 1615734566
$ts = 1615734566;
echo date("Y-m-d H:i:s", $ts), "\n";
echo date("D, d M Y", $ts), "\n";
echo date("l N w", $ts), "\n";
echo date("jS \\o\\f F", $ts), "\n";
echo date("g:i A", $ts), "\n";
echo mktime(15, 9, 26, 3, 14, 2021), "\n";
echo gmdate("Y", 0), "\n";
var_dump(checkdate(2, 29, 2020));
var_dump(checkdate(2, 29, 2021));
echo strtotime("2021-03-14 15:09:26"), "\n";
echo strtotime("@1615734566"), "\n";
$d = new DateTime("2021-03-14 15:09:26");
echo $d->format("Y/m/d"), "\n";
echo $d->getTimestamp(), "\n";
$d->setDate(2000, 1, 1);
echo $d->format("Y-m-d H:i:s"), "\n";
--EXPECT--
2021-03-14 15:09:26
Sun, 14 Mar 2021
Sunday 7 0
14th of March
3:09 PM
1615734566
1970
bool(true)
bool(false)
1615734566
1615734566
2021/03/14
1615734566
2000-01-01 15:09:26