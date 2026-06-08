--TEST--
throw / try / catch / finally, exception hierarchy, instanceof
--FILE--
<?php
function risky($x) {
    if ($x < 0) {
        throw new InvalidArgumentException("negative: " . $x);
    }
    return $x * 2;
}
try {
    echo risky(5), "\n";
    echo risky(-1), "\n";
} catch (InvalidArgumentException $e) {
    echo "caught: " . $e->getMessage() . "\n";
} finally {
    echo "done\n";
}
$e = new RuntimeException("oops");
echo ($e instanceof Exception) ? "is-exception\n" : "no\n";
echo ($e instanceof Throwable) ? "is-throwable\n" : "no\n";
echo ($e instanceof Error) ? "is-error\n" : "no\n";
--EXPECT--
10
caught: negative: -1
done
is-exception
is-throwable
no
