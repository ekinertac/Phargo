// Empty-output failure analysis: scan the ENTIRE vendor/php-src corpus, run
// every gradeable (--EXPECT--) test that expects non-empty output, and isolate
// the subset where the engine ran without error/panic but produced nothing at
// all (MISMATCH_EMPTY). These are silent-failure cases — the engine thinks it
// succeeded but swallowed the output — so they need their own bucketing pass
// distinct from zendscan's parse/runtime/mismatch split.
//
// Modeled directly on examples/zendscan.rs (same section() parser, same 1 GiB
// worker-thread wrapper + panic hook, same scratch-dir chdir before calling
// phargo::run). Read that file alongside this one for the shared scaffolding.
//
// Buckets are cheap substring heuristics on the --FILE-- source, checked in
// priority order: constructs known to suppress/redirect output (ob_start,
// register_shutdown_function, __halt_compiler, goto, declare(ticks) and
// multi-open-tag files) get their own named bucket; everything else falls back
// to a signature bucket keyed on the first 60 chars of code after "<?php".
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

// Collapse a byte-slice of PHP source into a single-line, whitespace-collapsed
// signature so near-identical snippets (differing only in indentation/spacing)
// land in the same bucket.
fn signature(code: &str) -> String {
    let after = match code.find("<?php") {
        Some(i) => &code[i + 5..],
        None => code,
    };
    let collapsed: String = after.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(60).collect()
}

// Does the source contain "?>" followed (anywhere later) by another "<?php"?
// That signals multiple PHP open/close regions — a distinct output pattern
// (e.g. HTML interleaved between tags) worth its own bucket.
fn has_multi_tag(code: &str) -> bool {
    if let Some(close_pos) = code.find("?>") {
        let rest = &code[close_pos + 2..];
        return rest.contains("<?php");
    }
    false
}

fn bucket_for(code: &str) -> String {
    if code.contains("ob_start") {
        "output-buffering".to_string()
    } else if code.contains("register_shutdown_function") {
        "shutdown-func".to_string()
    } else if code.contains("__halt_compiler") {
        "halt-compiler".to_string()
    } else if code.contains("goto ") {
        "goto".to_string()
    } else if code.contains("declare(ticks") {
        "ticks".to_string()
    } else if has_multi_tag(code) {
        "multi-tag".to_string()
    } else {
        format!("sig: {}", signature(code))
    }
}

fn scan() {
    let scratch = std::env::temp_dir().join("phargo_scratch");
    let _ = std::fs::create_dir_all(&scratch);
    let _ = std::env::set_current_dir(&scratch);
    let root = env!("CARGO_MANIFEST_DIR");
    let dir = Path::new(root).join("vendor").join("php-src");
    let mut files = Vec::new();
    collect(&dir, &mut files);

    let mut bucket_tally: HashMap<String, usize> = HashMap::new();
    let mut bucket_sample: HashMap<String, String> = HashMap::new();
    let mut total = 0usize;
    let mut considered = 0usize;

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
            None => continue, // ungradeable (no --EXPECT--, e.g. EXPECTF/EXPECTREGEX)
        };
        if expect.trim().is_empty() {
            continue; // we only care about cases expecting real output
        }
        considered += 1;

        let res = std::panic::catch_unwind(|| phargo::run(&code));
        if let Ok(Ok(out)) = res {
            if out.trim().is_empty() {
                total += 1;
                let name = f.strip_prefix(root).unwrap_or(f).to_string_lossy().into_owned();
                let b = bucket_for(&code);
                *bucket_tally.entry(b.clone()).or_insert(0) += 1;
                bucket_sample.entry(b).or_insert(name);
            }
        }
    }

    println!(
        "emptyscan: {} files scanned | {} gradeable w/ non-empty EXPECT | {} produced EMPTY output (MISMATCH_EMPTY)",
        files.len(),
        considered,
        total
    );

    let mut v: Vec<_> = bucket_tally.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    println!("\n--- bucket histogram ({} buckets) ---", v.len());
    for (bucket, n) in &v {
        println!("{n:5}  {bucket}");
    }

    println!("\n--- sample test per top 12 buckets ---");
    for (bucket, n) in v.iter().take(12) {
        let sample = bucket_sample.get(bucket).map(|s| s.as_str()).unwrap_or("?");
        println!("[{n:4}] {bucket}\n       {sample}");
    }
}
