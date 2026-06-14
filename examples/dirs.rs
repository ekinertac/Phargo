use phargo::run;
fn main() {
    let src = r#"<?php
$dir = sys_get_temp_dir() . '/phargo_dirtest';
@mkdir($dir);
file_put_contents($dir . '/a.txt', 'A');
file_put_contents($dir . '/b.txt', 'B');

// opendir/readdir/closedir
$h = opendir($dir);
$names = [];
while (($e = readdir($h)) !== false) { if ($e !== '.' && $e !== '..') $names[] = $e; }
closedir($h);
sort($names);
echo implode(",", $names), "\n";

// DirectoryIterator
$cnt = 0;
foreach (new DirectoryIterator($dir) as $fi) {
    if ($fi->isDot()) continue;
    $cnt++;
}
echo "files=$cnt\n";

// RecursiveArrayIterator
$it = new RecursiveArrayIterator([1, [2, 3], 4]);
$flat = [];
function walk($it, &$flat) {
    foreach ($it as $v) {
        if (is_array($v)) { walk(new RecursiveArrayIterator($v), $flat); }
        else { $flat[] = $v; }
    }
}
walk($it, $flat);
echo implode(",", $flat), "\n";

unlink($dir . '/a.txt'); unlink($dir . '/b.txt'); rmdir($dir);
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
