--TEST--
bcmath arbitrary-precision arithmetic
--FILE--
<?php
echo bcadd("1.1", "2.2", 1), "\n";
echo bcmul("12345678901234567890", "98765432109876543210"), "\n";
echo bcsub("5", "8"), "\n";
echo bcdiv("10", "3", 5), "\n";
echo bcmod("10", "3"), "\n";
echo bccomp("1.00001", "1", 5), "\n";
echo bccomp("1", "1", 5), "\n";
echo bcpow("2", "64"), "\n";
echo bcsqrt("152399025"), "\n";
echo bcdiv("1", "7", 20), "\n";
bcscale(3);
echo bcadd("1", "2"), "\n";
--EXPECT--
3.3
1219326311370217952237463801111263526900
-3
3.33333
1
1
0
18446744073709551616
12345
0.14285714285714285714
3.000
