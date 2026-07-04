// Run one .phpt FILE section on both engines and show the output diff.
fn main() {
    let path = std::env::args().nth(1).expect("path");
    let src = std::fs::read_to_string(&path).unwrap();
    let file_sec: String = src.split("--FILE--").nth(1).unwrap().split("--EXPECT").next().unwrap().to_string();
    std::thread::Builder::new().stack_size(1024*1024*1024).spawn(move || {
        std::env::remove_var("PHARGO_ENGINE");
        let w = phargo::run(&file_sec);
        std::env::set_var("PHARGO_ENGINE", "vm");
        let v = phargo::run(&file_sec);
        let (w, v) = (format!("{w:?}"), format!("{v:?}"));
        if w == v { println!("IDENTICAL"); return; }
        // print full outputs around the first divergence
        let wb = w.as_bytes(); let vb = v.as_bytes();
        let i = wb.iter().zip(vb.iter()).position(|(a,b)| a != b).unwrap_or(wb.len().min(vb.len()));
        let lo = i.saturating_sub(120);
        println!("diverge at {i} (walker {} bytes, vm {} bytes)", w.len(), v.len());
        println!("== walker ==\n{}", &w[lo..(i+260).min(w.len())]);
        println!("== vm ==\n{}", &v[lo..(i+260).min(v.len())]);
    }).unwrap().join().unwrap();
}
