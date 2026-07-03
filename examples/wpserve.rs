//! A tiny HTTP server that serves the vendored WordPress through Phargo, so
//! the rendered site is browsable. Each request runs a FRESH engine instance
//! (PHP's shared-nothing model) against the SQLite database that
//! examples/wpinstall.rs populated.
//!
//! Routing:
//!   /wp-content/*, /wp-includes/* (non-.php)  -> static files from disk
//!   /wp-admin/load-styles.php, load-scripts.php -> run standalone (css/js)
//!   /wp-admin/<page>.php                      -> engine, auto-authenticated
//!                                                as user #1 via forged auth
//!                                                cookies (dev harness!)
//!   /wp-login.php                             -> engine
//!   everything else                           -> front controller (index.php)
//!
//! Limitations (documented, not hidden): the engine has no header() channel,
//! so redirects/Location and Set-Cookie don't reach the browser — wp-admin
//! auth is therefore forged server-side, and form POSTs that end in a
//! redirect render the redirect target's empty body instead. GET browsing is
//! the supported path. Reads $PORT (qwok/portless) or falls back to 8787.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

fn main() {
    if std::env::var("PHARGO_STEP_LIMIT").is_err() {
        // WP page renders legitimately need a huge step budget
        std::env::set_var("PHARGO_STEP_LIMIT", "3000000000");
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wp = root.join("vendor").join("wordpress");
    if !wp.join("wp-settings.php").exists() {
        eprintln!("run scripts/fetch-wp.sh first");
        return;
    }
    if !wp.join("wp-content/database/.ht.sqlite").exists() {
        eprintln!("no installed database — run: PHARGO_STEP_LIMIT=3000000000 cargo run --release --example wpinstall");
        return;
    }
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8787);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("phargo-wp: serving WordPress on http://127.0.0.1:{port} (each page is a fresh engine run; expect ~5-12s per PHP page)");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let wp = wp.clone();
        // one big-stack thread per request, handled sequentially — the
        // evaluator needs deep recursion room, and Rc-based state is
        // single-threaded by design
        let _ = std::thread::Builder::new()
            .stack_size(1024 * 1024 * 1024)
            .spawn(move || {
                let _ = handle(stream, &wp);
            })
            .unwrap()
            .join();
    }
}

fn handle(mut stream: TcpStream, wp: &PathBuf) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut host = format!("127.0.0.1");
    let mut cookies = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "host" => host = v.to_string(),
                "cookie" => cookies = v.to_string(),
                "content-length" => content_length = v.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; content_length.min(2 * 1024 * 1024)];
    if !body.is_empty() {
        reader.read_exact(&mut body)?;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let path = url_decode(&path);

    // ---- static files ------------------------------------------------
    if let Some(resp) = try_static(wp, &path) {
        return write_response(&mut stream, 200, resp.0, &resp.1);
    }
    if path == "/favicon.ico" {
        return write_response(&mut stream, 404, "text/plain", b"no favicon");
    }

    // ---- PHP through the engine ---------------------------------------
    let t = std::time::Instant::now();
    let (script, entry) = route(wp, &path);
    let driver = build_driver(wp, &host, &method, &path, &query, &cookies, &body, &entry);
    let out = std::panic::catch_unwind(|| phargo::run_with_path(&driver, Some(script.clone())));
    let body = match out {
        Ok(Ok(o)) => o.into_bytes(),
        Ok(Err(e)) => format!("<pre>engine error: {e}</pre>").into_bytes(),
        Err(_) => b"<pre>engine panic</pre>".to_vec(),
    };
    eprintln!("{method} {target} -> {} bytes in {} ms", body.len(), t.elapsed().as_millis());
    let ctype = match entry {
        Entry::LoadStyles => "text/css; charset=UTF-8",
        Entry::LoadScripts => "application/javascript; charset=UTF-8",
        _ => "text/html; charset=UTF-8",
    };
    write_response(&mut stream, 200, ctype, &body)
}

enum Entry {
    Front,
    Login,
    Admin(String),
    LoadStyles,
    LoadScripts,
}

fn route(wp: &PathBuf, path: &str) -> (PathBuf, Entry) {
    if path == "/wp-login.php" {
        return (wp.join("wp-login.php"), Entry::Login);
    }
    if path == "/wp-admin/load-styles.php" {
        return (wp.join("wp-admin/load-styles.php"), Entry::LoadStyles);
    }
    if path == "/wp-admin/load-scripts.php" {
        return (wp.join("wp-admin/load-scripts.php"), Entry::LoadScripts);
    }
    if let Some(rest) = path.strip_prefix("/wp-admin") {
        let page = rest.trim_start_matches('/');
        let page = if page.is_empty() { "index.php" } else { page };
        // single safe filename only — no traversal
        if page.ends_with(".php")
            && page
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && wp.join("wp-admin").join(page).exists()
        {
            return (wp.join("wp-admin").join(page), Entry::Admin(page.to_string()));
        }
        return (wp.join("wp-admin/index.php"), Entry::Admin("index.php".into()));
    }
    (wp.join("index.php"), Entry::Front)
}

fn build_driver(
    wp: &PathBuf,
    host: &str,
    method: &str,
    path: &str,
    query: &str,
    cookies: &str,
    post_body: &[u8],
    entry: &Entry,
) -> String {
    let wpd = wp.display();
    let uri = if query.is_empty() { path.to_string() } else { format!("{path}?{query}") };
    let (script_name, script_file) = match entry {
        Entry::Login => ("/wp-login.php".to_string(), format!("{wpd}/wp-login.php")),
        Entry::Admin(p) => (format!("/wp-admin/{p}"), format!("{wpd}/wp-admin/{p}")),
        Entry::LoadStyles => ("/wp-admin/load-styles.php".into(), format!("{wpd}/wp-admin/load-styles.php")),
        Entry::LoadScripts => ("/wp-admin/load-scripts.php".into(), format!("{wpd}/wp-admin/load-scripts.php")),
        Entry::Front => ("/index.php".to_string(), format!("{wpd}/index.php")),
    };
    // cookies -> $_COOKIE
    let mut cookie_php = String::new();
    for c in cookies.split(';') {
        if let Some((k, v)) = c.split_once('=') {
            cookie_php.push_str(&format!(
                "$_COOKIE[{}] = urldecode({});\n",
                php_str(k.trim()),
                php_str(v.trim())
            ));
        }
    }
    let post_php = if method == "POST" {
        format!(
            "parse_str({}, $_POST);\n",
            php_str(&String::from_utf8_lossy(post_body))
        )
    } else {
        String::new()
    };
    let tail = match entry {
        Entry::Front => format!(
            "define('WP_USE_THEMES', true);\nrequire '{wpd}/wp-blog-header.php';"
        ),
        Entry::Login => format!("require '{wpd}/wp-login.php';"),
        // load-styles/scripts bootstrap themselves with SHORTINIT — run direct
        Entry::LoadStyles => format!("require '{wpd}/wp-admin/load-styles.php';"),
        Entry::LoadScripts => format!("require '{wpd}/wp-admin/load-scripts.php';"),
        // dev harness: wp-admin is auto-authenticated as user #1 with real
        // forged cookies (setcookie can't reach the browser — no header channel)
        Entry::Admin(p) => format!(
            r#"require '{wpd}/wp-load.php';
$__exp = time() + 172800;
$_COOKIE[AUTH_COOKIE] = wp_generate_auth_cookie(1, $__exp, 'auth');
$_COOKIE[LOGGED_IN_COOKIE] = wp_generate_auth_cookie(1, $__exp, 'logged_in');
wp_set_current_user(1);
require '{wpd}/wp-admin/{p}';"#
        ),
    };
    format!(
        r#"<?php
$_SERVER['HTTP_HOST'] = {host_s};
$_SERVER['SERVER_NAME'] = {host_s};
$_SERVER['REQUEST_URI'] = {uri_s};
$_SERVER['QUERY_STRING'] = {query_s};
$_SERVER['REQUEST_METHOD'] = {method_s};
$_SERVER['SERVER_PROTOCOL'] = 'HTTP/1.1';
$_SERVER['SCRIPT_NAME'] = {sn_s};
$_SERVER['SCRIPT_FILENAME'] = {sf_s};
$_SERVER['PHP_SELF'] = {sn_s};
$_SERVER['DOCUMENT_ROOT'] = '{wpd}';
$_SERVER['REMOTE_ADDR'] = '127.0.0.1';
$_SERVER['HTTP_USER_AGENT'] = 'phargo-wpserve';
parse_str($_SERVER['QUERY_STRING'], $_GET);
{post_php}{cookie_php}$_REQUEST = array_merge($_GET, $_POST);
define('WP_HOME', 'http://' . $_SERVER['HTTP_HOST']);
define('WP_SITEURL', 'http://' . $_SERVER['HTTP_HOST']);
{tail}
"#,
        host_s = php_str(host),
        uri_s = php_str(&uri),
        query_s = php_str(query),
        method_s = php_str(method),
        sn_s = php_str(&script_name),
        sf_s = php_str(&script_file),
    )
}

/// Single-quoted PHP string literal.
fn php_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn try_static(wp: &PathBuf, path: &str) -> Option<(&'static str, Vec<u8>)> {
    if !(path.starts_with("/wp-content/") || path.starts_with("/wp-includes/") || path.starts_with("/wp-admin/")) {
        return None;
    }
    if path.contains("..") || path.ends_with(".php") {
        return None;
    }
    let rel = path.trim_start_matches('/');
    let file = wp.join(rel);
    if !file.is_file() {
        return None;
    }
    let mime = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "css" => "text/css",
        "js" => "application/javascript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "json" => "application/json",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "mp4" => "video/mp4",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    };
    std::fs::read(&file).ok().map(|b| (mime, b))
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_response(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}
