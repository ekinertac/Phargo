use phargo::run;
fn main() {
    let src = r#"<?php
// memory stream round-trip
$fp = fopen('php://memory', 'r+');
var_dump(is_resource($fp));
var_dump(get_resource_type($fp));
fwrite($fp, "Hello, ");
fwrite($fp, "World!\n");
fwrite($fp, "line two\n");
rewind($fp);
echo fgets($fp);
echo "tell=", ftell($fp), "\n";
echo stream_get_contents($fp);
var_dump(feof($fp));
fclose($fp);

// real temp file
$path = sys_get_temp_dir() . '/phargo_stream_test.txt';
$w = fopen($path, 'w');
fwrite($w, "abc\ndef\n");
fclose($w);
echo "file=", file_get_contents($path);
$r = fopen($path, 'r');
echo "c1=", fgetc($r), "\n";
fseek($r, 0, SEEK_END);
echo "size=", ftell($r), "\n";
fclose($r);
unlink($path);

// stdout
fwrite(STDOUT, "via STDOUT\n");
var_dump(gettype($w));
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
