//! Selector-heavy restyle benchmark (Milestone 40b): a page of 5,000
//! elements of mixed tags, classes and attributes, under a sheet of a few
//! hundred type, class, attribute and descendant rules — the shape of a real
//! site's stylesheet. Toggling a class on `<body>` restyles every element, so
//! each restyle is one selector match per candidate rule per element.
//!
//!     cargo run -p style --release --example restyle_bench
//!
//! Prints the median and best of a full first restyle and of a whole-page
//! restyle after the toggle. Uses only API that existed before M40b, so the
//! same file measures the M40 baseline.

use std::time::{Duration, Instant};
use style::{StyleEngine, ViewportSize};

const ROUNDS: usize = 60;

fn page(cards: usize) -> String {
    let mut body = String::new();
    for i in 0..cards {
        body.push_str(&format!(
            "<article class=\"card c{m}\" data-state=\"{state}\"><header><h2 class=\"title\">Card {i}</h2></header>\
             <ul class=\"links\"><li><a href=\"https://example.com/{i}\" rel=\"nofollow\">out</a></li><li><a href=\"/local/{i}\">in</a></li></ul>\
             <p>text <em>em</em> <x-badge level=\"{m}\">b</x-badge> <span class=\"meta\" title=\"t\">m</span></p></article>",
            m = i % 7,
            state = if i % 3 == 0 { "open" } else { "closed" },
        ));
    }
    format!("<html><head></head><body class=\"light\"><nav><a href=\"/\">home</a></nav><main>{body}</main></body></html>")
}

fn sheet() -> String {
    let mut css = String::from(
        "body { margin: 0 } nav a { color: blue } main article { padding: 4px } article header h2 { font-size: 18px }\
         ul li a { text-decoration: none } a[href^=\"https\"] { color: green } a[rel] { color: gray }\
         [data-state=\"open\"] { border-width: 1px } x-badge { display: inline-block } x-badge[level=\"3\"] { color: red }\
         .dark article { background-color: black } .dark a { color: white } .dark .meta { color: silver }\
         p em { font-style: italic } p span { color: #333 } li + li { margin-left: 4px } h2.title { margin: 0 }",
    );
    // The long tail: rules for elements and classes this page mostly lacks,
    // as real sheets have — each still a candidate where its key matches.
    for (i, tag) in ["div", "span", "section", "aside", "footer", "table", "td", "button", "input", "label", "form", "img"].iter().enumerate() {
        for k in 0..12 {
            css.push_str(&format!("{tag}.u{k} {{ margin-top: {i}px }} main {tag} {{ padding-left: {k}px }} {tag}[data-k{k}] {{ color: red }} "));
        }
    }
    for k in 0..40 {
        css.push_str(&format!(".c{} .title {{ padding-top: {k}px }} article p .meta{k} {{ color: blue }} ", k % 7));
    }
    css
}

fn report(name: &str, mut times: Vec<Duration>) {
    times.sort();
    println!("{name:28} median {:8.3} ms   best {:8.3} ms", times[times.len() / 2].as_secs_f64() * 1e3, times[0].as_secs_f64() * 1e3);
}

fn main() {
    let root = html_parser::parse_document(&page(600)).expect("parses");
    let mut document = dom::Document::new(root);
    let sheet = css_parser::parse_stylesheet(&sheet());
    let viewport = ViewportSize::new(1024.0, 768.0);
    let body = document.root().descendants_and_self().find(|e| e.tag() == "body").expect("a body").node_id();
    println!("{} elements, {} rules", document.element_count(), sheet.rules.len());

    let (mut first, mut toggled) = (Vec::new(), Vec::new());
    for round in 0..ROUNDS {
        let mut engine = StyleEngine::new();
        let none = document.take_mutations();
        let started = Instant::now();
        let report = engine.restyle(&document, &sheet, 1, viewport, &none);
        first.push(started.elapsed());
        assert!(report.full);

        document.set_attribute(body, "class", if round % 2 == 0 { "dark" } else { "light" });
        let mutations = document.take_mutations();
        let started = Instant::now();
        let report = engine.restyle(&document, &sheet, 1, viewport, &mutations);
        toggled.push(started.elapsed());
        assert!(report.restyled >= document.element_count() - 3, "the toggle restyles the page ({} restyled)", report.restyled);
    }
    report("first restyle", first);
    report("class toggle on <body>", toggled);
}
