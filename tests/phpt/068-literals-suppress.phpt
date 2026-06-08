--TEST--
numeric literals (hex/oct/bin/separators) + @ suppression
--FILE--
<?php
echo 0xFF, "\n";
echo 0b1010, "\n";
echo 0o17, "\n";
echo 0755, "\n";
echo 1_000_000, "\n";
echo 0x1_FF, "\n";
echo 1_234.5, "\n";
$x = @$undefined_var_in_array["missing"];
var_dump($x);
echo @intdiv(1, 0), "done\n";
--EXPECT--
255
10
15
493
1000000
511
1234.5
NULL
done
