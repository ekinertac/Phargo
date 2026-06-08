--TEST--
heredoc (interpolated) and nowdoc (raw)
--FILE--
<?php
$name = "World";
$x = <<<EOT
Hello, $name!
Line two.
EOT;
echo $x, "\n";
$y = <<<'NOW'
Raw $name no interp
NOW;
echo $y;
--EXPECT--
Hello, World!
Line two.
Raw $name no interp
