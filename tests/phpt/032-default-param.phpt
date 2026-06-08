--TEST--
default parameter value
--FILE--
<?php
function greet($name, $greeting = "Hello") {
    return $greeting . ", " . $name;
}
echo greet("World");
echo "\n";
echo greet("World", "Hi");
--EXPECT--
Hello, World
Hi, World
