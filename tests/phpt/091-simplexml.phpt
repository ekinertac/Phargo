--TEST--
SimpleXML: parse, access, attributes, children, asXML
--FILE--
<?php
$xml = <<<XML
<?xml version="1.0"?>
<catalog count="2">
  <book id="b1"><title>PHP</title><price>9.99</price></book>
  <book id="b2"><title>Rust</title><price>19.99</price></book>
</catalog>
XML;
$c = simplexml_load_string($xml);
echo $c->getName(), "\n";
echo $c["count"], "\n";
echo (string)$c->book->title, "\n";
echo count($c->children()), "\n";
foreach ($c->children() as $b) {
    echo $b["id"], ":", (string)$b->title, "=", (string)$b->price, "\n";
}
$first = $c->book;
echo $first["id"], "\n";
var_dump(isset($c["count"]));
var_dump(isset($c["missing"]));
echo simplexml_load_string('<a x="1"><b>hi</b></a>')->asXML();
--EXPECT--
catalog
2
PHP
2
b1:PHP=9.99
b2:Rust=19.99
b1
bool(true)
bool(false)
<?xml version="1.0"?>
<a x="1"><b>hi</b></a>
