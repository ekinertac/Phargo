--TEST--
json_encode / json_decode (assoc array and stdClass)
--FILE--
<?php
$data = ["name" => "Rex", "age" => 3, "tags" => ["a", "b"], "active" => true];
echo json_encode($data), "\n";
$back = json_decode(json_encode($data), true);
echo $back["name"], " ", $back["age"], " ", $back["tags"][1], "\n";
$obj = json_decode('{"x": 1, "y": [2, 3]}');
echo $obj->x, " ", $obj->y[0];
--EXPECT--
{"name":"Rex","age":3,"tags":["a","b"],"active":true}
Rex 3 b
1 2
