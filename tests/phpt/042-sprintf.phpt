--TEST--
sprintf format specifiers
--FILE--
<?php
echo sprintf("%05d", 42), "\n";
echo sprintf("%.2f", 3.14159), "\n";
echo sprintf("%-5s|", "hi"), "\n";
echo sprintf("%x", 255), "\n";
--EXPECT--
00042
3.14
hi   |
ff
