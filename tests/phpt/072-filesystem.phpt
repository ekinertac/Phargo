--TEST--
filesystem + path functions
--FILE--
<?php
$dir = sys_get_temp_dir();
$f = $dir . '/ferro_test_072.txt';
file_put_contents($f, "line1\nline2\n");
echo file_get_contents($f);
var_dump(file_exists($f));
echo filesize($f), "\n";
file_put_contents($f, "more\n", FILE_APPEND);
$lines = file($f, FILE_IGNORE_NEW_LINES);
echo count($lines), "\n";
echo $lines[2], "\n";
unlink($f);
var_dump(file_exists($f));
echo basename("/a/b/c.php"), "\n";
echo basename("/a/b/c.php", ".php"), "\n";
echo dirname("/a/b/c.php"), "\n";
$pi = pathinfo("/a/b/file.tar.gz");
echo $pi['dirname'], "|", $pi['basename'], "|", $pi['extension'], "|", $pi['filename'], "\n";
--EXPECT--
line1
line2
bool(true)
12
3
more
bool(false)
c.php
c
/a/b
/a/b|file.tar.gz|gz|file.tar
