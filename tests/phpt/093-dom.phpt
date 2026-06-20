--TEST--
DOMDocument: loadXML, getElementsByTagName, attributes, textContent, build
--FILE--
<?php
$doc = new DOMDocument();
$doc->loadXML('<catalog><book id="b1"><title>PHP</title></book><book id="b2"><title>Rust</title></book></catalog>');
echo $doc->documentElement->tagName, "\n";
$books = $doc->getElementsByTagName('book');
echo $books->length, "\n";
foreach ($books as $b) {
    echo $b->getAttribute('id'), ":", $b->getElementsByTagName('title')->item(0)->textContent, "\n";
}
echo $doc->getElementsByTagName('title')->length, "\n";
$first = $doc->getElementsByTagName('title')->item(0);
echo $first->nodeName, "=", $first->nodeValue, "\n";

// build a tree
$d2 = new DOMDocument();
$root = $d2->createElement('root');
$child = $d2->createElement('child', 'hello');
$child->setAttribute('x', '1');
$root->appendChild($child);
$d2->appendChild($root);
echo $d2->saveXML();
echo $root->childNodes->length, "\n";
--EXPECT--
catalog
2
b1:PHP
b2:Rust
2
title=PHP
<?xml version="1.0"?>
<root><child x="1">hello</child></root>
1
