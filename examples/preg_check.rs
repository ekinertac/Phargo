fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    // strip the .phpt --FILE-- section if present
    let code = if let Some(i) = src.find("--FILE--") {
        let rest = &src[i + 8..];
        let end = rest.find("--EXPECT").unwrap_or(rest.len());
        rest[..end].trim().to_string()
    } else {
        src
    };
    let path = std::env::args().nth(1).map(std::path::PathBuf::from);
    match ferrophp::run_with_path(&code, path) {
        Ok(out) => print!("{out}"),
        Err(e) => eprintln!("ERROR: {e:?}"),
    }
}
