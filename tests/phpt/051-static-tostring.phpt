--TEST--
class constants, parent::__construct, self::, static method, ::class, __toString
--FILE--
<?php
class Base {
    const GREETING = "Hello";
    public $name;
    public function __construct($name) { $this->name = $name; }
}
class User extends Base {
    public function __construct($name) {
        parent::__construct($name);
    }
    public function __toString() {
        return self::GREETING . ", " . $this->name;
    }
    public static function make($n) {
        return new User($n);
    }
}
$u = User::make("World");
echo $u;
echo "\n";
echo User::GREETING;
echo "\n";
echo User::class;
--EXPECT--
Hello, World
Hello
User
