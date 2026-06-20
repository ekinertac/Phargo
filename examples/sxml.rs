use phargo::run;
fn main() {
    let src = r#"<?php
$xml = simplexml_load_string('<catalog><book id="b1"><title>PHP</title><price>10</price></book><book id="b2"><title>Rust</title><price>20</price></book></catalog>');
echo $xml->getName(), "\n";                         // catalog
echo count($xml->book), "\n";                       // 2
foreach ($xml->book as $b) {
    echo $b['id'], ":", $b->title, "=", $b->price, "\n";
}
echo (string)$xml->book[0]->title, "\n";            // (single access) PHP -- but [0] on element
echo $xml->book->title, "\n";                       // first book's title -> PHP
$first = $xml->book;
echo $first->getName(), "\n";                        // book
foreach ($xml->book[0]->attributes() as $k => $v) { echo "$k=$v "; }
echo "\n";
echo $xml->book[0]->asXML(), "\n";
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
