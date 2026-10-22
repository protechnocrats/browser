//! Per-stage breakdown (diagnostics spans) of a first frame and a one-DOM-
//! change frame and a scroll frame on a 10k-row page. `cargo run -p engine --release --example frame_profile`
use engine::{Engine, EngineConfig, EngineInput, LoadRequest, PageCommand, PageParams, Viewport};
use std::time::{Duration, Instant};

fn main() {
    let mut body = String::new();
    for i in 0..10_000 {
        body.push_str(&format!("<div class=\"row\"><span>row {i}</span> text</div>"));
    }
    let html = format!("<html><head><style>.row {{ padding: 2px; }} #t {{ width: 80px; height: 20px; }} #f {{ height: 20px; }}</style></head><body><input id=\"f\" type=\"text\"><div id=\"t\">t</div>{body}<script>document.getElementById('t').addEventListener('click', function () {{ document.getElementById('t').textContent = 'clicked'; }});</script></body></html>");
    let collector = diagnostics::collector::Collector::new();
    collector.install_global().expect("no other subscriber");
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(800.0, 600.0)));
    let mut now = Instant::now();
    engine.send(page, PageCommand::Load(LoadRequest::Html { html, url: None }));
    let t = Instant::now();
    engine.update_blocking(now, Duration::from_secs(30));
    println!("first update: {:?}\n{}", t.elapsed(), collector.drain().report("first frame"));
    now += Duration::from_millis(20);
    engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 5.0, y: 30.0 }));
    let t = Instant::now();
    engine.update_blocking(now, Duration::from_secs(30));
    println!("change update: {:?}\n{}", t.elapsed(), collector.drain().report("one DOM change"));
    now += Duration::from_millis(20);
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 300.0 }));
    let t = Instant::now();
    engine.update_blocking(now, Duration::from_secs(30));
    println!("scroll update: {:?}\n{}", t.elapsed(), collector.drain().report("scroll"));
}
