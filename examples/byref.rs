use phargo::run;
fn main() {
    let src = r#"<?php
function inc(&$x) { $x++; }
$a = 5; inc($a); echo "inc=$a\n";        // 6

function addItem(&$arr, $v) { $arr[] = $v; }
$list = []; addItem($list, 'a'); addItem($list, 'b');
echo "list=", implode(",", $list), "\n"; // a,b

// recursive accumulator through by-ref
function collect($node, &$out) {
    $out[] = $node['v'];
    foreach ($node['kids'] as $k) { collect($k, $out); }
}
$tree = ['v' => 1, 'kids' => [['v' => 2, 'kids' => []], ['v' => 3, 'kids' => [['v' => 4, 'kids' => []]]]]];
$out = []; collect($tree, $out);
echo "collect=", implode(",", $out), "\n"; // 1,2,3,4

// swap
function swap(&$a, &$b) { $t = $a; $a = $b; $b = $t; }
$x = 1; $y = 2; swap($x, $y); echo "swap=$x,$y\n"; // 2,1

// method by-ref
class Box {
    public function fill(&$arr) { $arr[] = 'filled'; }
}
$b = new Box(); $z = []; $b->fill($z); echo "method=", implode(",", $z), "\n"; // filled
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
