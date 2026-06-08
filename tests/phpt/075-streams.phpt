--TEST--
fopen / fwrite / fread / fgets / feof / STDOUT / csv
--FILE--
<?php
$f = sys_get_temp_dir() . '/phargo_stream_075.txt';
$h = fopen($f, 'w');
fwrite($h, "line1\n");
fputs($h, "line2\n");
fclose($h);
echo file_get_contents($f);

$h = fopen($f, 'r');
echo fgets($h);
$rest = fread($h, 100);
echo $rest;
var_dump(feof($h));
fclose($h);

$h = fopen($f, 'r');
echo stream_get_contents($h), "---\n";
fclose($h);

fwrite(STDOUT, "to-stdout\n");
var_dump(is_resource($h));

$c = fopen(sys_get_temp_dir() . '/phargo_075.csv', 'w');
fputcsv($c, ["a", "b,c", 'd"e']);
fclose($c);
$c = fopen(sys_get_temp_dir() . '/phargo_075.csv', 'r');
$row = fgetcsv($c);
echo $row[0], "|", $row[1], "|", $row[2], "\n";
fclose($c);

unlink($f);
unlink(sys_get_temp_dir() . '/phargo_075.csv');
--EXPECT--
line1
line2
line1
line2
bool(true)
line1
line2
---
to-stdout
bool(false)
a|b,c|d"e
