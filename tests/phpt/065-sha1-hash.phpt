--TEST--
sha1 and hash() dispatcher
--FILE--
<?php
echo sha1("abc"), "\n";
echo hash("md5", "abc"), "\n";
echo hash("sha1", "abc"), "\n";
echo hash("crc32b", "abc"), "\n";
echo hash_equals("secret", "secret") ? "eq" : "ne";
--EXPECT--
a9993e364706816aba3e25717850c26c9cd0d89d
900150983cd24fb0d6963f7d28e17f72
a9993e364706816aba3e25717850c26c9cd0d89d
352441c2
eq
