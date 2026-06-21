// Zend-focused failure analysis: scan vendor/php-src/Zend, run each gradeable
// (--EXPECT--) test, and categorize failures into parse error / runtime error /
// output mismatch — with samples — so we can target the core engine.
use std::collections::HashMap;
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
    std::panic::set_hook(Box::new(|_| {}));
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(scan)
        .unwrap()
        .join()
        .unwrap();
}

fn scan() {
    let scratch = std::env::temp_dir().join("phargo_scratch");
    let _ = std::fs::create_dir_all(&scratch);
    let _ = std::env::set_current_dir(&scratch);
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = Path::new(root).join("vendor").join("php-src").join("Zend");
    let mut files = Vec::new();
    collect(&dir, &mut files);

    let filter = std::env::args().nth(1); // optional: "mismatch" | "error" | "MISS:<substr>"
    let mut err_tally: HashMap<String, usize> = HashMap::new();
    let mut miss_samples: Vec<(String, String, String)> = Vec::new(); // file, expect-head, got-head
    let (mut pass, mut parse_err, mut run_err, mut mismatch, mut ungradeable) = (0, 0, 0, 0, 0);

    for f in &files {
        let src = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = match section(&src, "--FILE--") {
            Some(c) => c,
            None => continue,
        };
        let expect = match section(&src, "--EXPECT--") {
            Some(e) => e,
            None => {
                ungradeable += 1;
                continue;
            }
        };
        let res = std::panic::catch_unwind(|| phargo::run(&code));
        match res {
            Ok(Ok(out)) => {
                if out.trim_end() == expect.trim_end() {
                    pass += 1;
                } else {
                    mismatch += 1;
                    if miss_samples.len() < 60 {
                        let name = f.strip_prefix(root).unwrap_or(f).to_string_lossy().into_owned();
                        miss_samples.push((
                            name,
                            head(&expect),
                            head(&String::from_utf8_lossy(out.trim_end().as_bytes())),
                        ));
                    }
                }
            }
            Ok(Err(e)) => {
                let m = format!("{e:?}");
                if m.contains("Parse error") {
                    parse_err += 1;
                } else {
                    run_err += 1;
                }
                *err_tally.entry(normalize(&m)).or_insert(0) += 1;
            }
            Err(_) => {
                run_err += 1;
                *err_tally.entry("PANIC".into()).or_insert(0) += 1;
            }
        }
    }

    println!(
        "Zend: {} files | PASS {pass}  MISMATCH {mismatch}  PARSE_ERR {parse_err}  RUN_ERR {run_err}  (ungradeable {ungradeable})",
        files.len()
    );

    match filter.as_deref() {
        Some("mismatch") => {
            println!("\n--- output mismatches (expect | got) ---");
            for (f, e, g) in miss_samples.iter().take(50) {
                println!("{f}\n  EXP: {e}\n  GOT: {g}");
            }
        }
        Some(s) if s.starts_with("MISS:") => {
            let needle = &s[5..];
            for (f, e, g) in miss_samples.iter() {
                if e.contains(needle) || g.contains(needle) {
                    println!("{f}\n  EXP: {e}\n  GOT: {g}");
                }
            }
        }
        _ => {
            println!("\n--- top error buckets ---");
            let mut v: Vec<_> = err_tally.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            for (k, n) in v.into_iter().take(30) {
                println!("{n:5}  {k}");
            }
        }
    }
}

fn head(s: &str) -> String {
    s.replace(['\n', '\r'], "\\n").chars().take(90).collect()
}

fn normalize(m: &str) -> String {
    let mut out = String::new();
    let mut in_tick = false;
    for c in m.chars() {
        if c == '`' {
            in_tick = !in_tick;
            out.push('X');
            continue;
        }
        if in_tick {
            continue;
        }
        out.push(c);
    }
    out.chars().take(60).collect()
}
