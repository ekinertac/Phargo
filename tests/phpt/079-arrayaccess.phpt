--TEST--
ArrayAccess: offsetGet/Set/Exists/Unset via [], isset, unset
--FILE--
<?php
class Config implements ArrayAccess {
    private $data = [];
    public function offsetExists($k): bool { return isset($this->data[$k]); }
    public function offsetGet($k): mixed { return $this->data[$k] ?? null; }
    public function offsetSet($k, $v): void {
        if ($k === null) { $this->data[] = $v; } else { $this->data[$k] = $v; }
    }
    public function offsetUnset($k): void { unset($this->data[$k]); }
}
$c = new Config();
$c["name"] = "phargo";
$c[] = "appended";
echo $c["name"], "\n";
echo $c[0], "\n";
var_dump(isset($c["name"]));
var_dump(isset($c["missing"]));
$c["n"] = 1;
$c["n"] += 4;
echo $c["n"], "\n";
unset($c["name"]);
var_dump(isset($c["name"]));
--EXPECT--
phargo
appended
bool(true)
bool(false)
5
bool(false)
