use phargo::run;
fn main() {
    let src = r#"<?php
// =& reference aliasing
$a = 1; $b = &$a; $b = 99; echo "alias: a=$a b=$b\n";   // 99 99
$a = 5; echo "alias2: b=$b\n";                            // b follows a -> 5

// use (&$x) scalar accumulator
$sum = 0;
$add = function($v) use (&$sum) { $sum += $v; };
$add(3); $add(4);
echo "use-scalar: $sum\n";                               // 7

// use (&$out) array accumulator
$out = [];
$push = function($v) use (&$out) { $out[] = $v; };
$push('x'); $push('y'); $push('z');
echo "use-array: ", implode(",", $out), "\n";            // x,y,z

// array_walk_recursive with use(&)
$data = [1, [2, 3], 4]; $total = 0;
array_walk_recursive($data, function($v) use (&$total) { $total += $v; });
echo "walk: $total\n";                                    // 10

// array_map with use(&) counter
$n = 0;
array_map(function($v) use (&$n) { $n++; }, [10, 20, 30]);
echo "map-count: $n\n";                                   // 3

// ref through function param still works
function inc(&$x) { $x++; }
$c = 41; inc($c); echo "param: $c\n";                     // 42
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
