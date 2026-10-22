//! Frame-cost benchmark over the public engine API (Milestone 31's "measure
//! first"): for the demo page and synthetic 1k/10k-element pages, the median
//! wall-clock time of a first frame, a scroll frame, a one-element DOM change
//! and a caret/focus frame. Uses only API that existed before Milestone 31,
//! so the same file measures the pre-M31 baseline too.
//!
//!     cargo run -p engine --release --example frame_bench
//!     cargo run -p engine --release --example frame_bench -- --processes
//!
//! `--processes` runs each page behind the renderer-process protocol (ADR
//! 016) — over OS pipes to a renderer on a thread, so it measures the wire
//! (encoding, pipes, decoding), not process start-up.

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, LoadRequest, PageCommand, PageParams, Viewport};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);

fn synthetic(rows: usize) -> String {
    let mut body = String::new();
    for i in 0..rows {
        body.push_str(&format!("<div class=\"row\"><span>row {i}</span> text</div>"));
    }
    format!(
        "<html><head><style>.row {{ padding: 2px; }} span {{ color: #334455; }} #t {{ width: 80px; height: 20px; }} #f {{ height: 20px; }}</style></head>\
         <body><input id=\"f\" type=\"text\"><div id=\"t\">t</div>{body}<script>\
         document.getElementById('t').addEventListener('click', function () {{ document.getElementById('t').textContent = 'clicked'; }});\
         </script></body></html>"
    )
}

/// Time until the frame produced by `command` arrives.
fn time_frame(engine: &mut Engine, page: engine::PageId, command: Option<PageCommand>, now: &mut Instant) -> Option<Duration> {
    if let Some(c) = command {
        engine.send(page, c);
    }
    *now += Duration::from_millis(20);
    let started = Instant::now();
    let events = engine.update_blocking(*now, WAIT);
    let elapsed = started.elapsed();
    events.iter().any(|e| matches!(e, EngineEvent::Frame { .. })).then_some(elapsed)
}

fn median(mut v: Vec<Duration>) -> String {
    if v.is_empty() {
        return "no frame".into();
    }
    v.sort();
    format!("{:>8.3} ms", v[v.len() / 2].as_secs_f64() * 1000.0)
}

/// The 10k-row page with a flex toolbar (any flex container makes the page
/// need intrinsic sizing) and 200 custom properties on `:root` (inherited
/// by every element) — what real sites' stylesheets look like.
fn realistic(rows: usize) -> String {
    let vars: String = (0..200).map(|i| format!("--v{i}: {i}px;")).collect();
    synthetic(rows)
        .replacen("<style>", &format!("<style>:root {{ {vars} }} .bar {{ display: flex; gap: 4px; }} .row {{ margin-left: var(--v3); }}"), 1)
        // After the toggle, so the DOM-change click still lands on it.
        .replacen("<div id=\"t\">t</div>", "<div id=\"t\">t</div><div class=\"bar\"><span>a</span><span>b</span></div>", 1)
}

fn main() {
    let pages = [
        ("demo", include_str!("../../../app/src/demo.html").to_string()),
        ("1k rows", synthetic(1_000)),
        ("10k rows", synthetic(10_000)),
        ("10k + flex/vars", realistic(10_000)),
    ];
    println!("{:<16} {:>14} {:>14} {:>14} {:>14}", "page", "first frame", "scroll", "one DOM change", "caret/focus");
    for (name, html) in pages {
        let (mut first, mut scroll, mut change, mut caret) = (vec![], vec![], vec![], vec![]);
        for _ in 0..5 {
            let isolation = if std::env::args().any(|a| a == "--processes") {
                engine::Isolation::Processes(std::sync::Arc::new(engine::InProcessLauncher))
            } else {
                engine::Isolation::Threads
            };
            let mut engine = Engine::new(EngineConfig { isolation, ..EngineConfig::default() });
            let page = engine.create_page(PageParams::new(Viewport::new(800.0, 600.0)));
            let mut now = Instant::now();
            first.extend(time_frame(&mut engine, page, Some(PageCommand::Load(LoadRequest::Html { html: html.clone(), url: None })), &mut now));
            for _ in 0..3 {
                scroll.extend(time_frame(&mut engine, page, Some(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 40.0 })), &mut now));
            }
            let _ = time_frame(&mut engine, page, Some(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: -10_000.0 })), &mut now);
            // Milestone 39b: on the toggle (`#t`, inside the body's 8px
            // margin) — at x 5 the click hit the body, and the column timed
            // a click that changed nothing.
            change.extend(time_frame(&mut engine, page, Some(PageCommand::Input(EngineInput::PointerDown { x: 20.0, y: 30.0 })), &mut now));
            let _ = time_frame(&mut engine, page, Some(PageCommand::Input(EngineInput::Key { key: "Tab".into() })), &mut now);
            caret.extend(time_frame(&mut engine, page, Some(PageCommand::Input(EngineInput::Key { key: "End".into() })), &mut now));
        }
        println!("{:<16} {:>14} {:>14} {:>14} {:>14}", name, median(first), median(scroll), median(change), median(caret));
    }
}
