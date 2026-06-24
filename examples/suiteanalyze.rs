// Whole-corpus failure analyzer: one pass over every gradeable .phpt, attributing
// each failing test to its PRIMARY blocker, bucketed by area (Zend / ext/* / core
// / sapi) and category. Analysis only — prints a report.
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

fn lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn area_of(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if let Some(i) = s.find("/ext/") {
        return format!("ext/{}", s[i + 5..].split('/').next().unwrap_or("?"));
    }
    if s.contains("/Zend/") {
        return "Zend".into();
    }
    if let Some(i) = s.find("/sapi/") {
        return format!("sapi/{}", s[i + 6..].split('/').next().unwrap_or("?"));
    }
    "core".into()
}

// category, bucket-key (the primary blocker detail)
fn classify(out_res: &Result<Result<String, phargo::EngineError>, ()>, expect: &str) -> (&'static str, String) {
    match out_res {
        Err(_) => ("PANIC", "panic".into()),
        Ok(Err(e)) => {
            let m = format!("{}", e);
            if m.starts_with("Parse error") {
                ("PARSE", normalize(&m))
            } else if let Some(f) = m.strip_prefix("unknown function ") {
                ("MISSING_FN", f.trim_end_matches("()").to_string())
            } else if m.starts_with("call to undefined method ") {
                let mm = m.trim_start_matches("call to undefined method ");
                ("MISSING_METHOD", mm.trim_end_matches("()").to_string())
            } else if m.starts_with("class ") && m.ends_with(" not found") {
                ("MISSING_CLASS", m.trim_start_matches("class ").trim_end_matches(" not found").to_string())
            } else if m.contains("step limit") || m.contains("output limit") || m.contains("memory") || m.contains("nesting level") {
                ("RESOURCE", normalize(&m))
            } else if m.starts_with("undefined constant") {
                ("MISSING_CONST", m.trim_start_matches("undefined constant ").to_string())
            } else {
                ("RUNTIME_ERR", normalize(&m))
            }
        }
        Ok(Ok(got)) => {
            let got = lf(got);
            let g = got.trim_end();
            let e = lf(expect);
            let e = e.trim_end();
            if g == e {
                ("PASS", String::new())
            } else if g.is_empty() {
                ("MISMATCH_EMPTY", "no output".into())
            } else if g.contains("Fatal error: Uncaught") && !e.contains("Fatal error") && !e.contains("Uncaught") {
                // Extract the underlying cause so we can see WHICH missing piece
                // causes the mid-execution fatal (real second-order leverage).
                let cause = if let Some(i) = g.find("Call to undefined function ") {
                    let rest = &g[i + 27..];
                    format!("fn:{}", rest.split(['(', ' ', '\n']).next().unwrap_or("?"))
                } else if let Some(i) = g.find("Call to undefined method ") {
                    let rest = &g[i + 25..];
                    format!("method:{}", rest.split(['(', ' ', '\n']).next().unwrap_or("?"))
                } else if let Some(i) = g.find("not found") {
                    let pre = &g[..i];
                    let cls = pre.rsplit([' ', '"', '\'']).find(|s| !s.is_empty()).unwrap_or("?");
                    format!("class:{cls}")
                } else if let Some(i) = g.find("Uncaught ") {
                    let rest = &g[i + 9..];
                    format!("throw:{}", rest.split([':', '\n']).next().unwrap_or("?"))
                } else {
                    "other".into()
                };
                return ("MISMATCH_FATAL", cause);
            } else if e.contains("Fatal error") || e.contains("Deprecated") || e.contains("Warning") || e.contains("Notice") {
                ("MISMATCH_DIAG", "expects diagnostic (error/warning)".into())
            } else if g.len().abs_diff(e.len()) <= 4 {
                ("MISMATCH_CLOSE", "near-match (formatting)".into())
            } else {
                ("MISMATCH_OTHER", "behavioral diff".into())
            }
        }
    }
}

fn normalize(m: &str) -> String {
    let mut out = String::new();
    let mut tick = false;
    for c in m.chars() {
        if c == '`' { tick = !tick; out.push('X'); continue; }
        if c == '"' { out.push('"'); continue; }
        if tick { continue; }
        out.push(c);
    }
    out.chars().take(55).collect()
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    std::thread::Builder::new().stack_size(1024 * 1024 * 1024).spawn(run).unwrap().join().unwrap();
}

fn run() {
    let scratch = std::env::temp_dir().join("phargo_scratch");
    let _ = fs::create_dir_all(&scratch);
    let _ = std::env::set_current_dir(&scratch);
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = Path::new(root).join("vendor").join("php-src");
    let mut files = Vec::new();
    collect(&dir, &mut files);

    // area -> category -> count
    let mut by_area: HashMap<String, HashMap<&'static str, usize>> = HashMap::new();
    // category -> bucket -> count (with area split for the report)
    let mut buckets: HashMap<&'static str, HashMap<String, usize>> = HashMap::new();
    let mut cat_total: HashMap<&'static str, usize> = HashMap::new();
    let mut gradeable = 0usize;

    for f in &files {
        let src = match fs::read_to_string(f) { Ok(s) => s, Err(_) => continue };
        let code = match section(&src, "--FILE--") { Some(c) => c, None => continue };
        let expect = match section(&src, "--EXPECT--") { Some(e) => e, None => continue };
        gradeable += 1;
        let res = std::panic::catch_unwind(|| phargo::run(&code)).map_err(|_| ());
        let (cat, bucket) = classify(&res, &expect);
        let area = area_of(f);
        *by_area.entry(area).or_default().entry(cat).or_insert(0) += 1;
        *cat_total.entry(cat).or_insert(0) += 1;
        if cat != "PASS" {
            *buckets.entry(cat).or_default().entry(bucket).or_insert(0) += 1;
        }
    }

    println!("=== GRADEABLE TESTS: {gradeable} ===\n");
    println!("--- category totals (whole corpus) ---");
    let mut cats: Vec<_> = cat_total.iter().collect();
    cats.sort_by(|a, b| b.1.cmp(a.1));
    for (c, n) in &cats { println!("{:>6}  {c}", n); }

    println!("\n--- by area (top 16 areas by failing count) ---");
    let mut areas: Vec<_> = by_area.iter().map(|(a, m)| {
        let total: usize = m.values().sum();
        let pass = *m.get("PASS").unwrap_or(&0);
        (a.clone(), total - pass, pass, total)
    }).collect();
    areas.sort_by(|a, b| b.1.cmp(&a.1));
    println!("{:<22} {:>6} {:>6} {:>6}", "area", "fail", "pass", "total");
    for (a, fail, pass, total) in areas.iter().take(16) {
        println!("{:<22} {:>6} {:>6} {:>6}", a, fail, pass, total);
    }

    for cat in ["MISSING_FN", "MISSING_METHOD", "MISSING_CLASS", "MISMATCH_FATAL", "MISSING_CONST", "PARSE", "RUNTIME_ERR", "RESOURCE"] {
        if let Some(b) = buckets.get(cat) {
            let mut v: Vec<_> = b.iter().collect();
            v.sort_by(|a, c| c.1.cmp(a.1));
            println!("\n--- top {cat} blockers (total {}) ---", cat_total.get(cat).unwrap_or(&0));
            for (k, n) in v.into_iter().take(25) { println!("{:>5}  {k}", n); }
        }
    }
}
