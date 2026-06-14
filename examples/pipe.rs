use phargo::run;
fn main() {
    let src = r#"<?php
function _test(int $a, int $b = 3) { return $a + $b; }
var_dump(5 |> '_test');                 // _test(5) = 8
var_dump("  hi " |> 'trim' |> 'strtoupper'); // "HI"
$double = fn($x) => $x * 2;
var_dump(10 |> $double |> $double);      // 40
var_dump([3,1,2] |> 'count');           // 3
// precedence: concat higher, comparison lower
var_dump(2 + 3 |> '_test');             // _test(5) = 8 (arith higher than pipe)
echo (1 + 2 * 3 ** 2), "\n";            // 19, sanity on renumbered bp
echo (-2 ** 2), "\n";                   // -4
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
