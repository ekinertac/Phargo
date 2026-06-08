--TEST--
md5 / base64 / bin2hex / crc32
--FILE--
<?php
echo md5("abc"), "\n";
echo base64_encode("Man"), "\n";
echo base64_decode("TWFu"), "\n";
echo bin2hex("AB"), "\n";
echo crc32("abc");
--EXPECT--
900150983cd24fb0d6963f7d28e17f72
TWFu
Man
4142
891568578
