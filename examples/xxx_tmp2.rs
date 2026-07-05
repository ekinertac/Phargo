fn main() {
    std::thread::Builder::new().stack_size(1024*1024*1024).spawn(|| {
    let wp = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/vendor/wordpress"));
    let driver = format!(r#"<?php
$_SERVER['HTTP_HOST'] = 'localhost';
$_SERVER['REQUEST_URI'] = '/';
$_SERVER['REQUEST_METHOD'] = 'GET';
$_SERVER['SERVER_NAME'] = 'localhost';
$_SERVER['SCRIPT_NAME'] = '/index.php';
$_SERVER['SCRIPT_FILENAME'] = '{}/index.php';
$_SERVER['PHP_SELF'] = '/index.php';
$_SERVER['DOCUMENT_ROOT'] = '{}';
$_SERVER['REMOTE_ADDR'] = '127.0.0.1';
define('WP_USE_THEMES', true);
require '{}/wp-blog-header.php';
"#, wp.display(), wp.display(), wp.display());
    std::env::remove_var("PHARGO_ENGINE");
    let w = phargo::run_with_path(&driver, Some(wp.join("index.php"))).unwrap_or_default();
    std::env::set_var("PHARGO_ENGINE", "vm");
    let v = phargo::run_with_path(&driver, Some(wp.join("index.php"))).unwrap_or_default();
    println!("walker: {} bytes; vm: {} bytes; identical: {}", w.len(), v.len(), w == v);
    if w != v {
        // capture the short render for the cold-start flake diagnosis
        std::fs::write("target/wp_diverge_walker.html", &w).ok();
        std::fs::write("target/wp_diverge_vm.html", &v).ok();
        eprintln!("divergent outputs saved to target/wp_diverge_*.html");
    }
    if w != v {
        let wb = w.as_bytes(); let vb = v.as_bytes();
        let i = wb.iter().zip(vb.iter()).position(|(a,b)| a != b).unwrap_or(wb.len().min(vb.len()));
        println!("first divergence at byte {i}:");
        println!("walker: ...{}", String::from_utf8_lossy(&wb[i.saturating_sub(60)..(i+80).min(wb.len())]));
        println!("vm:     ...{}", String::from_utf8_lossy(&vb[i.saturating_sub(60)..(i+80).min(vb.len())]));
        println!("vm head 400: {}", String::from_utf8_lossy(&vb[..400.min(vb.len())]));
    }
    }).unwrap().join().unwrap();
}
