use phargo::run;
fn main() {
    let src = r#"<?php
class Point {
    public private(set) int $x = 1;
    protected(set) int $y = 2;
    public function __construct(public private(set) int $z = 3) {}
    public function show() { echo $this->x, ",", $this->y, ",", $this->z, "\n"; }
}
$p = new Point(9);
$p->show();
echo $p->x, "\n";
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
