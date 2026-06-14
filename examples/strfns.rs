use phargo::run;
fn main() {
    let src = r#"<?php
var_dump(strstr("user@example.com", "@"));
var_dump(strstr("user@example.com", "@", true));
var_dump(strstr("abc", "z"));
var_dump(stristr("HELLO world", "wor"));
var_dump(strrchr("a/b/c", "/"));
var_dump(strpbrk("This is a test", "st"));
var_dump(strcmp("a", "b"));
var_dump(strcmp("b", "a"));
var_dump(strcmp("a", "a"));
var_dump(strcasecmp("ABC", "abc"));
var_dump(strncmp("hello", "help", 3));
var_dump(strncasecmp("Hello", "HELP", 3));
var_dump(substr_compare("Hello World", "World", 6));
var_dump(strspn("42 is the answer", "1234567890"));
var_dump(strcspn("hello, world", ","));
var_dump(addslashes("O'Reilly \"x\""));
var_dump(stripslashes("O\\'Reilly"));
var_dump(quotemeta("1+1=2"));
"#;
    match run(src) {
        Ok(s) => print!("{}", s),
        Err(e) => println!("ERR: {}", e),
    }
}
