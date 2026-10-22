//! A headless host: loads a page through the engine's public API only and
//! prints the first finished frame as JSON (`engine::scene_to_json`).
//!
//!     cargo run -p engine --example engine_headless -- page.html [800 600]
//!     cargo run -p engine --example engine_headless -- https://example.com/ [800 600]
//!
//! A URL is fetched (the command waits up to 30 s for the navigation to
//! commit); anything else is read as a local HTML file and loaded with no
//! address (an opaque origin). The frame is the one built right after the
//! document commits — no later animation frames.

use engine::{Engine, EngineConfig, EngineEvent, LoadRequest, PageCommand, PageParams, Viewport};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(source) = args.first() else {
        eprintln!("usage: engine_headless <file.html | http(s)://url> [width height]");
        std::process::exit(2);
    };
    let dimension = |i: usize, default: f32| args.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(default);
    let viewport = Viewport::new(dimension(1, 800.0), dimension(2, 600.0));

    let request = if source.starts_with("http://") || source.starts_with("https://") {
        LoadRequest::Url(source.clone())
    } else {
        match std::fs::read_to_string(source) {
            Ok(html) => LoadRequest::Html { html, url: None },
            Err(e) => {
                eprintln!("could not read {source}: {e}");
                std::process::exit(1);
            }
        }
    };

    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(viewport));
    engine.send(page, PageCommand::Load(request));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut committed = false;
    while Instant::now() < deadline {
        for event in engine.update(Instant::now()) {
            match event {
                EngineEvent::NavigationCommitted { .. } => committed = true,
                EngineEvent::NavigationFailed { error, .. } => {
                    eprintln!("load failed: {error}");
                    std::process::exit(1);
                }
                EngineEvent::Frame { scene, .. } if committed => {
                    print!("{}", engine::scene_to_json(&scene));
                    return;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("timed out waiting for the page to load");
    std::process::exit(1);
}
