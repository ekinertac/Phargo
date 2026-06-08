// Walk the corpus, run each test's FILE section, and tally engine error prefixes.
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
        .stack_size(256 * 1024 * 1024)
        .spawn(scan)
        .unwrap()
        .join()
        .unwrap();
}

fn scan() {
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = Path::new(root).join("vendor").join("php-src");
    let mut files = Vec::new();
    collect(&dir, &mut files);
    let mut tally: HashMap<String, usize> = HashMap::new();
    let mut samples: HashMap<String, String> = HashMap::new();
    for f in &files {
        let src = match fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = match section(&src, "--FILE--") {
            Some(c) => c,
            None => continue,
        };
        let res = std::panic::catch_unwind(|| ferrophp::run(&code));
        let msg = match res {
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => format!("{e:?}"),
            Err(_) => "PANIC".to_string(),
        };
        // normalize: take a coarse prefix key
        let key = normalize(&msg);
        *tally.entry(key.clone()).or_insert(0) += 1;
        samples.entry(key).or_insert(msg);
    }
    let _ = &samples;
    // Detailed breakdown: tally the backtick token for selected buckets.
    let want = std::env::args().nth(1);
    if let Some(bucket) = want {
        let mut detail: HashMap<String, usize> = HashMap::new();
        for f in &files {
            let src = match fs::read_to_string(f) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let code = match section(&src, "--FILE--") {
                Some(c) => c,
                None => continue,
            };
            let res = std::panic::catch_unwind(|| ferrophp::run(&code));
            let msg = match res {
                Ok(Err(e)) => format!("{e:?}"),
                _ => continue,
            };
            if bucket != "RAW" && !msg.contains(&bucket) {
                continue;
            }
            if bucket == "RAW" {
                let m = msg.replace(['\n', '\r'], " ");
                *detail.entry(m.chars().take(75).collect()).or_insert(0) += 1;
            } else if let (Some(a), Some(b)) = (msg.find('`'), msg.rfind('`')) {
                if b > a {
                    *detail.entry(msg[a + 1..b].to_string()).or_insert(0) += 1;
                }
            }
        }
        let mut v: Vec<_> = detail.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.into_iter().take(50) {
            println!("{n:5}  {k}");
        }
        return;
    }
    let mut v: Vec<_> = tally.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, n) in v.into_iter().take(40) {
        let s = samples.get(&k).map(|s| s.as_str()).unwrap_or("");
        let s = &s[..s.len().min(90)];
        println!("{n:5}  {k:30}  {s}");
    }
}

fn normalize(m: &str) -> String {
    // strip backtick-quoted specifics and snippet tails to cluster errors
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
    out.chars().take(40).collect()
}
