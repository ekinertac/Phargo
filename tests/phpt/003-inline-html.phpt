--TEST--
inline HTML around a php block
--FILE--
Hi <?php echo "there"; ?>!
--EXPECT--
Hi there!
