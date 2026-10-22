//! `browser-renderer`: one page's process (ADR 016). Started by the browser
//! with `--renderer`, speaking the engine's protocol on stdin/stdout; it
//! sandboxes itself before reading anything from a page.
//!
//! `--sandbox-self-test` enters the sandbox and tries what it must forbid
//! (exit 0: all refused; 1: something got through; 77: no sandbox here).

#![forbid(unsafe_code)]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--sandbox-self-test") {
        std::process::exit(self_test());
    }
    if !renderer::is_renderer_process() {
        eprintln!("browser-renderer runs pages for the browser; it is not started by hand");
        std::process::exit(64);
    }
    renderer::renderer_main();
}

fn self_test() -> i32 {
    let report = sandbox::enter();
    println!("{}", report.detail);
    if !report.active {
        return 77;
    }
    let mut leaks = Vec::new();
    if std::fs::read("/etc/hostname").is_ok() || std::fs::read_dir("/").is_ok() {
        leaks.push("read a file");
    }
    if std::fs::write(std::env::temp_dir().join("browser-renderer-self-test"), b"x").is_ok() {
        leaks.push("wrote a file");
    }
    if std::net::TcpListener::bind("127.0.0.1:0").is_ok() || std::net::TcpStream::connect("127.0.0.1:9").is_ok() {
        leaks.push("opened a socket");
    }
    if std::process::Command::new("/bin/true").status().is_ok() {
        leaks.push("started a program");
    }
    for leak in &leaks {
        println!("NOT CONTAINED: {leak}");
    }
    i32::from(!leaks.is_empty())
}
