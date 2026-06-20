use phargo::run;
fn main() {
    let src = r#"<?php
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

$d2 = new DOMDocument();
$root = $d2->createElement('root');
$child = $d2->createElement('child', 'hello');
$child->setAttribute('x', '1');
$root->appendChild($child);
$d2->appendChild($root);
echo $d2->saveXML();
echo $root->childNodes->length, "\n";
"#;
    let expect = "catalog\n2\nb1:PHP\nb2:Rust\n2\ntitle=PHP\n<?xml version=\"1.0\"?>\n<root><child x=\"1\">hello</child></root>\n1\n";
    match run(src) {
        Ok(s) => {
            print!("{}", s);
            println!("---\nMATCH: {}", s == expect);
        }
        Err(e) => println!("ERR: {}", e),
    }
}
