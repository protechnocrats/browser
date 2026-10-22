//! Parse benchmark (Milestone 40b): each HTML file in a directory parsed and
//! moved into a `dom::Document`, as a page load does — where names are born
//! and, since M40b, interned. Real pages invent names (`data-*`, custom
//! elements) that the synthetic `frame_bench` pages do not.
//!
//!     cargo run -p html_parser --release --example parse_bench -- <dir>
//!
//! Prints the median and best total over all files, of parsing alone and of
//! parsing plus `Document::new`. Uses only API that existed before M40b.

use std::time::{Duration, Instant};

const ROUNDS: usize = 15;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: parse_bench <dir of .html files>");
    let mut pages: Vec<String> = std::fs::read_dir(&dir)
        .expect("a directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "html"))
        .map(|e| String::from_utf8_lossy(&std::fs::read(e.path()).expect("readable")).into_owned())
        .collect();
    pages.sort();
    let bytes: usize = pages.iter().map(String::len).sum();
    println!("{} pages, {:.1} MB", pages.len(), bytes as f64 / 1e6);

    let (mut parse, mut both) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for page in &pages {
            drop(std::hint::black_box(html_parser::parse_document(page)));
        }
        parse.push(started.elapsed());
        let started = Instant::now();
        for page in &pages {
            if let Ok(root) = html_parser::parse_document(page) {
                drop(std::hint::black_box(dom::Document::new(root)));
            }
        }
        both.push(started.elapsed());
    }
    let show = |name: &str, mut v: Vec<Duration>| {
        v.sort();
        println!("{name:28} median {:8.3} ms   best {:8.3} ms", v[v.len() / 2].as_secs_f64() * 1e3, v[0].as_secs_f64() * 1e3);
    };
    show("parse", parse);
    show("parse + Document::new", both);
}
