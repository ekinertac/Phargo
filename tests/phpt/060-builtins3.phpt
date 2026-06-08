--TEST--
array_chunk, str_ireplace, substr_replace, vsprintf, addslashes
--FILE--
<?php
echo implode("|", array_map(fn ($c) => implode(",", $c), array_chunk([1, 2, 3, 4, 5], 2))), "\n";
echo str_ireplace("WORLD", "PHP", "Hello world"), "\n";
echo substr_replace("Hello World", "PHP", 6), "\n";
echo vsprintf("%s is %d", ["age", 30]), "\n";
echo addslashes("O'Reilly \"x\"");
--EXPECT--
1,2|3,4|5
Hello PHP
Hello PHP
age is 30
O\'Reilly \"x\"
