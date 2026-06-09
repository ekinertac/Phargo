//! v2-engine scoreboard: runs the php-src corpus through the NEW pipeline
//! (lexer -> parser -> Eval::run) and reports the pass count against the same
//! suite the legacy engine scores, plus a histogram of the most common failure
//! reasons so the builtin port can be data-driven.
//!
//! Run: cargo run --release --example lang_scoreboard

use phargo::lang::eval::Eval;
use phargo::lang::lexer::Lexer;
use phargo::lang::parser::Parser;
use std::collections::HashMap;
use std::fs;
use std::panic;
use std::path::{Path, PathBuf};

fn main() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

struct Phpt {
    file: String,
    expect: Option<String>,
    expectf: Option<String>,
    path: PathBuf,
}

thread_local! {
    static PANIC_MSG: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

fn run() {
    panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| {
                let f = l.file();
                let f = f.rsplit(['/', '\\']).next().unwrap_or(f);
                format!("{}:{}", f, l.line())
            })
            .unwrap_or_default();
        PANIC_MSG.with(|m| *m.borrow_mut() = format!("PANIC {loc}"));
    }));
    let root = env!("CARGO_MANIFEST_DIR");
    let corpus_dir = Path::new(root).join("vendor").join("php-src");
    let curated_dir = Path::new(root).join("tests").join("phpt");

    let scratch = std::env::temp_dir().join("phargo_scratch_v2");
    let _ = fs::create_dir_all(&scratch);
    let _ = std::env::set_current_dir(&scratch);

    let mut corpus = Vec::new();
    collect(&corpus_dir, &mut corpus);
    let mut curated = Vec::new();
    collect(&curated_dir, &mut curated);

    let breadcrumb = Path::new(root).join("target").join("current_test_v2.txt");

    let (mut pass, mut fail, mut na) = (0usize, 0usize, 0usize);
    let mut errors: HashMap<String, usize> = HashMap::new();
    let n = corpus.len();
    for (i, path) in corpus.iter().enumerate() {
        let _ = fs::write(&breadcrumb, path.to_string_lossy().as_bytes());
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut t = parse_phpt(&text);
        t.path = path.clone();
        if t.expect.is_none() && t.expectf.is_none() {
            na += 1;
            continue;
        }
        match evaluate(&t) {
            Ok(true) => pass += 1,
            Ok(false) => fail += 1,
            Err(msg) => {
                fail += 1;
                let key = if msg.starts_with("PANIC") { msg.clone() } else { normalize(&msg) };
                *errors.entry(key).or_insert(0) += 1;
            }
        }
        if i % 4000 == 0 && i > 0 {
            eprintln!("  …{i}/{n}");
        }
    }

    let (mut cpass, mut ctotal) = (0usize, 0usize);
    for path in &curated {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut t = parse_phpt(&text);
        t.path = path.clone();
        if t.expect.is_none() && t.expectf.is_none() {
            continue;
        }
        ctotal += 1;
        if matches!(evaluate(&t), Ok(true)) {
            cpass += 1;
        }
    }

    let gradeable = pass + fail;
    println!("\n==================== v2 ENGINE (lang::) ====================");
    println!("  PASS {pass}   FAIL {fail}   N/A {na}   TOTAL {}", pass + fail + na);
    println!(
        "  Pass rate: {pass}/{} ({:.2}% of gradeable)",
        pass + fail + na,
        if gradeable > 0 { pass as f64 * 100.0 / gradeable as f64 } else { 0.0 }
    );
    println!("  (legacy engine baseline on this suite: 1981)");
    println!("  Smoke (curated): {cpass}/{ctotal}");

    let mut ev: Vec<(String, usize)> = errors.into_iter().collect();
    ev.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n  Top failure reasons (errored runs only):");
    for (k, c) in ev.into_iter().take(30) {
        println!("  {c:6}  {k}");
    }
}

fn evaluate(t: &Phpt) -> Result<bool, String> {
    let out = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let toks = Lexer::tokenize(t.file.as_bytes()).map_err(|e| format!("LEX: {}", e.msg))?;
        let ast = Parser::parse(toks).map_err(|e| format!("PARSE: {}", e.msg))?;
        Eval::run_with_path(&ast, Some(t.path.clone())).map_err(|e| e.0)
    }));
    let out = match out {
        Ok(Ok(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(Err(msg)) => return Err(msg),
        Err(_) => return Err(PANIC_MSG.with(|m| {
            let s = m.borrow().clone();
            if s.is_empty() { "PANIC".to_string() } else { s }
        })),
    };
    if let Some(e) = &t.expect {
        return Ok(out.trim_end() == e.trim_end());
    }
    if let Some(p) = &t.expectf {
        return Ok(expectf_matches(p, &out));
    }
    Ok(false)
}

/// Cluster an error message: drop backtick contents and digits, keep a prefix.
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
        if c.is_ascii_digit() {
            continue;
        }
        out.push(c);
    }
    out.chars().take(60).collect()
}

// ---- phpt parsing (mirrors src/main.rs) --------------------------------
fn parse_phpt(text: &str) -> Phpt {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current: Option<String> = None;
    let mut buf = String::new();
    for line in text.lines() {
        if let Some(name) = section_header(line) {
            if let Some(c) = current.take() {
                sections.push((c, std::mem::take(&mut buf)));
            }
            current = Some(name);
        } else if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(c) = current.take() {
        sections.push((c, buf));
    }
    let get = |key: &str| sections.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    Phpt {
        file: get("FILE").unwrap_or_default(),
        expect: get("EXPECT"),
        expectf: get("EXPECTF"),
        path: PathBuf::new(),
    }
}

fn section_header(line: &str) -> Option<String> {
    let t = line.trim_end();
    if t.len() > 4 && t.starts_with("--") && t.ends_with("--") {
        let inner = &t[2..t.len() - 2];
        if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return Some(inner.to_string());
        }
    }
    None
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect(&p, out);
            } else if p.extension().map(|e| e == "phpt").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}

// ---- EXPECTF matcher (mirrors src/main.rs) -----------------------------
#[derive(Clone)]
enum Tok {
    Lit(Vec<char>),
    One,
    Var(fn(char) -> bool, usize),
}

fn expectf_matches(pattern: &str, actual: &str) -> bool {
    let toks = parse_expectf(pattern.trim_end());
    let s: Vec<char> = actual.trim_end().chars().collect();
    let mut budget: u64 = 300_000;
    match_toks(&toks, &s, &mut budget)
}

fn match_toks(toks: &[Tok], s: &[char], budget: &mut u64) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match toks.split_first() {
        None => s.is_empty(),
        Some((t, rest)) => match t {
            Tok::Lit(lit) => {
                s.len() >= lit.len()
                    && s[..lit.len()] == lit[..]
                    && match_toks(rest, &s[lit.len()..], budget)
            }
            Tok::One => !s.is_empty() && s[0] != '\n' && match_toks(rest, &s[1..], budget),
            Tok::Var(pred, min) => {
                let mut max = 0;
                while max < s.len() && pred(s[max]) {
                    max += 1;
                }
                if max < *min {
                    return false;
                }
                let mut k = max;
                loop {
                    if match_toks(rest, &s[k..], budget) {
                        return true;
                    }
                    if k == *min {
                        return false;
                    }
                    k -= 1;
                }
            }
        },
    }
}

fn parse_expectf(p: &str) -> Vec<Tok> {
    let chars: Vec<char> = p.chars().collect();
    let mut toks = Vec::new();
    let mut lit: Vec<char> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            if chars[i + 1] == '%' {
                lit.push('%');
                i += 2;
                continue;
            }
            let t = match chars[i + 1] {
                'a' => Some(Tok::Var(|_| true, 1)),
                'A' => Some(Tok::Var(|_| true, 0)),
                'w' => Some(Tok::Var(|c| c.is_whitespace(), 0)),
                's' => Some(Tok::Var(|c| c != '\n' && c != '\r', 1)),
                'd' => Some(Tok::Var(|c| c.is_ascii_digit(), 1)),
                'i' => Some(Tok::Var(|c| c.is_ascii_digit() || c == '+' || c == '-', 1)),
                'f' => Some(Tok::Var(|c| c.is_ascii_digit() || matches!(c, '+'|'-'|'.'|'e'|'E'|'N'|'A'|'I'|'F'), 1)),
                'x' => Some(Tok::Var(|c| c.is_ascii_hexdigit(), 1)),
                'c' => Some(Tok::One),
                'e' => Some(Tok::Var(|c| c == '/' || c == '\\', 1)),
                _ => None,
            };
            if let Some(t) = t {
                if !lit.is_empty() {
                    toks.push(Tok::Lit(std::mem::take(&mut lit)));
                }
                toks.push(t);
                i += 2;
                continue;
            }
        }
        lit.push(chars[i]);
        i += 1;
    }
    if !lit.is_empty() {
        toks.push(Tok::Lit(lit));
    }
    toks
}
