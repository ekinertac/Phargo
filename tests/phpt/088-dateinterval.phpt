--TEST--
DateInterval + DateTime add/sub/modify/diff
--FILE--
<?php
$d = new DateTime("2020-01-31 00:00:00");
$d->add(new DateInterval("P1M"));
echo $d->format("Y-m-d"), "\n";          // 2020-02-29 (clamped, leap year)

$d2 = new DateTime("2021-06-15 12:00:00");
$d2->sub(new DateInterval("P1Y2M10D"));
echo $d2->format("Y-m-d"), "\n";          // 2020-04-05

$d3 = new DateTime("2020-01-01 00:00:00");
$d3->modify("+1 day");
$d3->modify("+2 weeks");
$d3->modify("-3 hours");
echo $d3->format("Y-m-d H:i:s"), "\n";

$a = new DateTime("2020-01-01");
$b = new DateTime("2021-03-15");
$diff = $a->diff($b);
echo $diff->format("%y years, %m months, %d days"), "\n";
echo $diff->days, "\n";

$iv = new DateInterval("P1Y2M3DT4H5M6S");
echo $iv->y . "-" . $iv->m . "-" . $iv->d . " " . $iv->h . ":" . $iv->i . ":" . $iv->s, "\n";
--EXPECT--
2020-02-29
2020-04-05
2020-01-15 21:00:00
1 years, 2 months, 14 days
439
1-2-3 4:5:6
