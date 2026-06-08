--TEST--
static class properties (self:: and Class::)
--FILE--
<?php
class Counter {
    public static $count = 0;
    public static function inc() {
        self::$count = self::$count + 1;
    }
}
Counter::inc();
Counter::inc();
Counter::$count = Counter::$count + 10;
echo Counter::$count;
--EXPECT--
12
