//! One-shot WordPress installer under Phargo: runs wp_install() against the
//! vendored WordPress + SQLite-integration plugin, populating
//! vendor/wordpress/wp-content/database/.ht.sqlite (untracked).
//!
//! Run this once after scripts/fetch-wp.sh; then examples/wpscan.rs serves
//! real pages against the installed database. Delete the .ht.sqlite file to
//! reinstall from scratch. Needs a generous step budget:
//!   PHARGO_STEP_LIMIT=3000000000 cargo run --release --example wpinstall

use std::path::PathBuf;

fn main() {
    std::panic::set_hook(Box::new(|i| eprintln!("PANIC: {i}")));
    std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(install)
        .unwrap()
        .join()
        .unwrap();
}

fn install() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wp = root.join("vendor").join("wordpress");
    if !wp.join("wp-settings.php").exists() {
        eprintln!("run scripts/fetch-wp.sh first");
        return;
    }
    let code = format!(
        r#"<?php
$_SERVER['HTTP_HOST'] = 'localhost';
$_SERVER['REQUEST_URI'] = '/wp-admin/install.php';
$_SERVER['REQUEST_METHOD'] = 'GET';
$_SERVER['SERVER_PROTOCOL'] = 'HTTP/1.1';
$_SERVER['SERVER_NAME'] = 'localhost';
$_SERVER['SCRIPT_NAME'] = '/wp-admin/install.php';
$_SERVER['SCRIPT_FILENAME'] = '{wp}/wp-admin/install.php';
$_SERVER['PHP_SELF'] = '/wp-admin/install.php';
$_SERVER['DOCUMENT_ROOT'] = '{wp}';
$_SERVER['REMOTE_ADDR'] = '127.0.0.1';
define('WP_INSTALLING', true);
require '{wp}/wp-load.php';
require '{wp}/wp-admin/includes/upgrade.php';
$r = wp_install('Phargo Test Site', 'admin', 'admin@example.com', true, '', 'phargo-pass-1');
echo "\n=== WP_INSTALL RETURNED ===\n";
var_dump($r);
global $wpdb;
echo "options: "; var_dump($wpdb->get_var("SELECT COUNT(*) FROM wp_options"));
echo "users:   "; var_dump($wpdb->get_var("SELECT COUNT(*) FROM wp_users"));
echo "posts:   "; var_dump($wpdb->get_var("SELECT COUNT(*) FROM wp_posts"));
"#,
        wp = wp.display()
    );
    let t = std::time::Instant::now();
    match phargo::run_with_path(&code, Some(wp.join("wp-admin/install.php"))) {
        Ok(o) => {
            let tail: String = o.chars().rev().take(900).collect::<String>().chars().rev().collect();
            println!("OK {} bytes in {} ms; tail:\n{tail}", o.len(), t.elapsed().as_millis());
        }
        Err(e) => println!("ERR after {} ms: {e}", t.elapsed().as_millis()),
    }
}
