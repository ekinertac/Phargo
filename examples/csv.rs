use phargo::run;
fn main() {
    let src = r#"<?php
// str_getcsv with quoting
var_dump(str_getcsv('a,b,"c,d",e'));
var_dump(str_getcsv('1;"two ""2""";3', ';'));

// SplTempFileObject round-trip
$f = new SplTempFileObject();
$f->fwrite("line1\nline2\nline3\n");
$f->rewind();
$lines = [];
foreach ($f as $i => $line) { if ($line === '' ) continue; $lines[] = $i . ':' . rtrim($line); }
echo implode(" ", $lines), "\n";

// CSV via SplFileObject on a real temp file
$path = sys_get_temp_dir() . '/phargo_csv.csv';
$w = new SplFileObject($path, 'w');
$w->fputcsv(['name', 'age']);
$w->fputcsv(['Alice', '30']);
unset($w);
$r = new SplFileObject($path, 'r');
$r->setFlags(SplFileObject::READ_CSV);
$rows = [];
while (!$r->eof()) {
    $row = $r->fgetcsv();
    if ($row === false || $row === [null]) break;
    $rows[] = implode("|", $row);
}
echo implode(" / ", $rows), "\n";
unset($r);
unlink($path);
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
