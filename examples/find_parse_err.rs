// Find corpus tests whose v2 parse error mentions a given substring; print the
// path and the offending source region.
use phargo::lang::{lexer::Lexer, parser::Parser};
use std::fs;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.extension().map(|x| x == "phpt").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

fn section(src: &str, tag: &str) -> Option<String> {
    let start = src.find(tag)? + tag.len();
    let rest = &src[start..];
    let end = rest.find("\n--").unwrap_or(rest.len());
    Some(rest[..end].trim_start_matches('\n').to_string())
}

fn main() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(work)
        .unwrap()
        .join()
        .unwrap();
}

fn work() {
    let needle = std::env::args().nth(1).unwrap_or_else(|| "require_once".into());
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = Path::new(root).join("vendor").join("php-src");
    let mut files = Vec::new();
    collect(&dir, &mut files);
    let mut shown = 0;
    for f in &files {
        let src = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = match section(&src, "--FILE--") {
            Some(c) => c,
            None => continue,
        };
        let toks = match Lexer::tokenize(code.as_bytes()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if let Err(e) = Parser::parse(toks) {
            if e.msg.contains(&needle) {
                println!("=== {} @pos {} : {}", f.display(), e.pos, e.msg);
                let s = code.as_bytes();
                let lo = e.pos.saturating_sub(60).min(s.len());
                let hi = (e.pos + 40).min(s.len());
                println!("    …{}…", String::from_utf8_lossy(&s[lo..hi]).replace('\n', "\\n"));
                shown += 1;
                if shown >= 8 {
                    break;
                }
            }
        }
    }
    println!("({shown} shown)");
}
