//! The second oracle: how far does WordPress get on this engine?
//!
//! Synthesizes a minimal wp-config.php (untracked, inside vendor/wordpress/),
//! sets up a CLI-ish SAPI fixture ($_SERVER etc.), and runs WordPress's
//! bootstrap chain under the engine. Reports the first death: the error, and
//! the tail of whatever output was produced before it.
//!
//! Per docs/ROADMAP.md Phase 0, this harness's blocker ranking outranks the
//! corpus scoreboard when the two disagree about what to build next.

use std::path::PathBuf;

const WP_CONFIG: &str = r#"<?php
define('DB_NAME', 'wordpress');
define('DB_USER', 'wp');
define('DB_PASSWORD', 'wp');
define('DB_HOST', 'localhost');
define('DB_CHARSET', 'utf8');
define('DB_COLLATE', '');
$table_prefix = 'wp_';
define('WP_DEBUG', false);
define('AUTH_KEY', 'k'); define('SECURE_AUTH_KEY', 'k'); define('LOGGED_IN_KEY', 'k');
define('NONCE_KEY', 'k'); define('AUTH_SALT', 'k'); define('SECURE_AUTH_SALT', 'k');
define('LOGGED_IN_SALT', 'k'); define('NONCE_SALT', 'k');
if ( ! defined( 'ABSPATH' ) ) {
    define( 'ABSPATH', __DIR__ . '/' );
}
require_once ABSPATH . 'wp-settings.php';
"#;

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
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wp = root.join("vendor").join("wordpress");
    if !wp.join("wp-settings.php").exists() {
        eprintln!("run scripts/fetch-wp.sh first");
        return;
    }
    // synthesize wp-config.php (vendor/ is untracked)
    let cfg = wp.join("wp-config.php");
    if !cfg.exists() {
        std::fs::write(&cfg, WP_CONFIG).expect("write wp-config");
    }
    // the real db.php drop-in: the SQLite-integration plugin's db.copy with
    // its placeholders substituted (what the plugin's activator does)
    let plug = wp
        .join("wp-content")
        .join("plugins")
        .join("sqlite-database-integration");
    let dbphp = wp.join("wp-content").join("db.php");
    if plug.join("db.copy").exists() {
        let tpl = std::fs::read_to_string(plug.join("db.copy")).expect("read db.copy");
        let filled = tpl
            .replace("{SQLITE_IMPLEMENTATION_FOLDER_PATH}", &plug.display().to_string())
            .replace("{SQLITE_PLUGIN}", "sqlite-database-integration/load.php");
        std::fs::write(&dbphp, filled).expect("write db.php");
    }

    // the driver script: SAPI fixture + the real WP entry chain
    let driver = format!(
        r#"<?php
$_SERVER['HTTP_HOST'] = 'localhost';
$_SERVER['REQUEST_URI'] = '/';
$_SERVER['REQUEST_METHOD'] = 'GET';
$_SERVER['SERVER_PROTOCOL'] = 'HTTP/1.1';
$_SERVER['SERVER_NAME'] = 'localhost';
$_SERVER['SCRIPT_NAME'] = '/index.php';
$_SERVER['SCRIPT_FILENAME'] = '{wp}/index.php';
$_SERVER['PHP_SELF'] = '/index.php';
$_SERVER['DOCUMENT_ROOT'] = '{wp}';
$_SERVER['REMOTE_ADDR'] = '127.0.0.1';
// the real front-controller request lifecycle (mirrors WP's index.php):
// wp-blog-header requires wp-load, runs wp(), then the template loader.
define('WP_USE_THEMES', true);
require '{wp}/wp-blog-header.php';
echo "\n=== WP PAGE RENDER COMPLETED ===\n";
"#,
        wp = wp.display()
    );

    let t = std::time::Instant::now();
    let result = std::panic::catch_unwind(|| {
        phargo::run_with_path(&driver, Some(wp.join("index.php")))
    });
    let ms = t.elapsed().as_millis();

    println!("=== wpscan: WordPress 6.7.x bootstrap under Phargo ===");
    println!("elapsed: {ms} ms");
    match result {
        Ok(Ok(out)) => {
            println!("run returned OK; output {} bytes", out.len());
            // keep the full response for inspection (target/ is untracked)
            let _ = std::fs::write(root.join("target").join("wp_page.html"), &out);
            // the blocker line, untruncated
            if let Some(i) = out.find("Fatal error") {
                let line: String = out[i..].lines().next().unwrap_or("").to_string();
                println!("BLOCKER: {line}");
            } else if out.is_empty() {
                println!(
                    "no output, no fatal: WP exited cleanly — with an empty DB this \
                     is the wp_not_installed() redirect to the installer (bootstrap \
                     chain itself completed; verify with define('WP_INSTALLING', true))"
                );
            }
            let tail: String = out.chars().rev().take(4000).collect::<String>().chars().rev().collect();
            println!("--- output tail ---\n{tail}");
        }
        Ok(Err(e)) => {
            println!("DIED (engine error): {e}");
        }
        Err(_) => println!("DIED (panic)"),
    }
}
