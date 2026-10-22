//! A second, headless host (Milestone 29 acceptance): drives the engine
//! through its PUBLIC API only — this file is compiled as a separate crate,
//! so it cannot reach an engine-internal type even by accident. If a host
//! needed anything beyond `engine::*` to load, render, click, scroll and
//! navigate, it would not compile.

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, LoadRequest, PageCommand, PageParams, Viewport};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

const DEMO_HTML: &str = include_str!("../../../app/src/demo.html");

/// How long a test waits for page threads to finish a tick.
const WAIT: Duration = Duration::from_secs(10);

fn frames(events: &[EngineEvent]) -> Vec<&engine::Scene> {
    events.iter().filter_map(|e| if let EngineEvent::Frame { scene, .. } = e { Some(scene) } else { None }).collect()
}

fn html(html: &str) -> PageCommand {
    PageCommand::Load(LoadRequest::Html { html: html.to_string(), url: None })
}

#[test]
fn load_render_and_dump_a_scene_as_json() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(800.0, 700.0)));
    engine.send(page, html(DEMO_HTML));
    let events = engine.update_blocking(Instant::now(), WAIT);

    assert!(events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { url: None, .. })), "{events:?}");
    let frames = frames(&events);
    assert_eq!(frames.len(), 1, "one update, one frame");
    let json = engine::scene_to_json(frames[0]);
    assert!(json.contains("\"viewport\": [800.00,700.00]"));
    assert!(json.matches("\"glyphs\"").count() > 20, "the demo page's text rendered");
}

#[test]
fn the_same_commands_at_the_same_times_give_the_same_frames() {
    // The clock is injected (ADR 005): nothing on the pipeline path reads
    // the wall clock, so identical input + identical times = identical
    // frames — the demo page's rAF, transition and @keyframes animations
    // included.
    let run = || {
        let start = Instant::now();
        let mut engine = Engine::new(EngineConfig::default());
        // Tall enough that the demo's animated boxes are on screen (the
        // display list is culled to the viewport).
        let page = engine.create_page(PageParams::new(Viewport::new(640.0, 1600.0)));
        engine.send(page, html(DEMO_HTML));
        let mut out = Vec::new();
        for step in 0..5u64 {
            if step == 2 {
                engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 120.0 }));
            }
            // `start` differs between the two runs; only the offsets from it
            // are the same, and only offsets can matter.
            for scene in frames(&engine.update_blocking(start + Duration::from_millis(step * 250), WAIT)) {
                out.push(engine::scene_to_json(scene));
            }
        }
        out
    };
    let (a, b) = (run(), run());
    assert_eq!(a.len(), 5, "the demo page animates, so every update is a frame");
    assert_eq!(a, b);
    assert_ne!(a[0], a[1], "and the animation really advanced between them");
}

#[test]
fn an_idle_page_produces_no_frames_and_asks_for_no_wake_up() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(page, html("<html><body><p>static</p></body></html>"));
    let now = Instant::now();
    assert_eq!(frames(&engine.update_blocking(now, WAIT)).len(), 1);
    assert_eq!(engine.next_wake(), None, "nothing is animating or loading");
    for i in 1..10 {
        assert!(frames(&engine.update_blocking(now + Duration::from_millis(i * 16), WAIT)).is_empty(), "no frame without a change");
    }
}

#[test]
fn a_click_runs_page_script_and_produces_a_new_frame() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(
        page,
        html(
            "<html><head><style>#b { width: 200px; height: 50px; background-color: #336699; }</style></head><body>\
             <div id=\"b\">before</div>\
             <script>document.getElementById('b').addEventListener('click', function () { \
               document.getElementById('b').textContent = 'after, a much longer label'; });</script>\
             </body></html>",
        ),
    );
    let now = Instant::now();
    let first = engine::scene_to_json(frames(&engine.update_blocking(now, WAIT))[0]);
    engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 10.0, y: 10.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    let second = engine::scene_to_json(frames(&events)[0]);
    assert!(second.matches("[").count() > first.matches("[").count(), "the longer label drew more glyphs");
}

#[test]
fn scrolling_is_clamped_to_the_document() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(
        page,
        html("<html><head><style>#top { height: 40px; background-color: #ff0000; } #tall { height: 1000px; } \
              #end { height: 40px; background-color: #0000ff; }</style></head>\
              <body><div id=\"top\"></div><div id=\"tall\"></div><div id=\"end\"></div></body></html>"),
    );
    let now = Instant::now();
    engine.update_blocking(now, WAIT);
    // The y of the first item with `color` in the scene, if it was drawn.
    let y_of = |events: &[EngineEvent], color: &str| {
        let json = engine::scene_to_json(frames(events)[0]);
        json.lines()
            .find(|l| l.contains(color))
            .map(|line| line.split('[').nth(1).and_then(|n| n.split(',').nth(1)).and_then(|y| y.parse::<f32>().ok()).unwrap())
    };
    const RED: &str = "[255,0,0,255]";
    const BLUE: &str = "[0,0,255,255]";
    // Far past the top: clamped at 0.
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: -5000.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    assert_eq!(y_of(&events, RED), Some(8.0), "the top of the document, below body's UA margin (8px)");
    assert_eq!(y_of(&events, BLUE), None, "the bottom marker is off screen, so culled");
    // Far past the bottom: clamped so the document's end meets the viewport's.
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 50_000.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(32), WAIT);
    assert_eq!(y_of(&events, BLUE), Some(300.0 - 8.0 - 40.0), "the document's end (after body's 8px bottom margin) meets the viewport's");
    assert_eq!(y_of(&events, RED), None, "the top marker is scrolled away, so culled");
    // Hostile deltas are ignored, not propagated.
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: f32::NAN }));
    assert!(frames(&engine.update_blocking(now + Duration::from_millis(48), WAIT)).is_empty());
}

#[test]
fn hostile_viewports_are_clamped_and_unknown_pages_are_ignored() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(f32::NAN, -3.0)));
    engine.send(page, html("<html><body><p>x</p></body></html>"));
    let now = Instant::now();
    let events = engine.update_blocking(now, WAIT);
    assert_eq!(frames(&events)[0].viewport_width, 1.0);
    engine.send(page, PageCommand::Resize(Viewport { width: 4.0e9, height: f32::INFINITY }));
    let scene = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    assert_eq!(frames(&scene)[0].viewport_width, engine::MAX_VIEWPORT_PX);
    assert_eq!(frames(&scene)[0].viewport_height, 1.0);

    engine.close_page(page);
    engine.send(page, html("<html><body></body></html>"));
    assert!(engine.update_blocking(now + Duration::from_millis(32), WAIT).is_empty(), "a closed page's commands go nowhere");
    assert!(engine.page_ids().is_empty());
}

/// Serves `body` for every request, forever (test helper).
fn serve(body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn loading_a_url_navigates_in_the_background_and_commits() {
    let base = serve("<html><body><p>from the network</p></body></html>");
    let mut engine = Engine::new(EngineConfig::default());
    // Milestone 38b (ADR 020): finished background work wakes the host
    // through its waker; nothing is polled meanwhile.
    let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&woken);
    engine.set_waker(std::sync::Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    let url = format!("{base}/start");
    engine.send(page, PageCommand::Load(LoadRequest::Url(url.clone())));

    let start = Instant::now();
    let events = engine.update_blocking(start, WAIT);
    assert!(events.contains(&EngineEvent::NavigationStarted { page, url: url.clone() }));
    assert!(start.elapsed() < Duration::from_millis(100), "update() must not wait for the network");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut updates = 0;
    loop {
        assert!(Instant::now() < deadline, "the navigation never committed");
        // A host that only updates when woken or when the engine asks.
        let due = engine.next_wake().is_some_and(|at| at <= Instant::now());
        if woken.swap(0, std::sync::atomic::Ordering::SeqCst) == 0 && !due {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        updates += 1;
        let events = engine.update_blocking(Instant::now(), WAIT);
        if events.contains(&EngineEvent::NavigationCommitted { page, url: Some(url.clone()) }) {
            assert_eq!(frames(&events).len(), 1, "the committed document is drawn in the same update");
            break;
        }
    }
    // Woken for the navigation, not polled for it every few milliseconds.
    assert!(updates <= 4, "{updates} updates for one navigation");
}

#[test]
fn a_failed_navigation_keeps_the_old_document() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(page, html("<html><body><p>still here</p></body></html>"));
    engine.update_blocking(Instant::now(), WAIT);
    engine.send(page, PageCommand::Load(LoadRequest::Url("http://127.0.0.1:1/unreachable".into())));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline);
        let events = engine.update_blocking(Instant::now(), WAIT);
        if let Some(EngineEvent::NavigationFailed { error, .. }) = events.iter().find(|e| matches!(e, EngineEvent::NavigationFailed { .. })) {
            assert!(!error.is_empty());
            break;
        }
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })));
        std::thread::sleep(Duration::from_millis(5));
    }
    // The old document is still what gets drawn.
    engine.send(page, PageCommand::Resize(Viewport::new(401.0, 300.0)));
    let events = engine.update_blocking(Instant::now(), WAIT);
    assert!(engine::scene_to_json(frames(&events)[0]).contains("\"glyphs\""), "\"still here\" is still drawn");
}

#[test]
fn animation_runs_at_most_once_per_frame_interval_however_often_the_host_asks() {
    // The demo page animates forever (rAF + @keyframes). A host calling
    // `update` in a tight loop must not get more than ~60 frames a second.
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(page, html(DEMO_HTML));
    let start = Instant::now();
    let mut frame_count = 0;
    // One simulated second, updated every millisecond.
    for ms in 0..1000u64 {
        frame_count += frames(&engine.update_blocking(start + Duration::from_millis(ms), WAIT)).len();
    }
    assert!((55..=62).contains(&frame_count), "{frame_count} frames in one second of 1 ms updates");
    let wake = engine.next_wake().expect("still animating");
    assert!(wake > start + Duration::from_millis(999), "the next wake is a frame interval out, not now");
}

#[test]
fn a_fixed_bar_stays_on_screen_and_takes_clicks_after_scrolling() {
    // Milestone 33: `position: fixed` does not scroll, for painting and for
    // hit testing alike.
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(
        page,
        html("<html><head><style>body { margin: 0 } #bar { position: fixed; top: 0; left: 0; width: 400px; height: 30px; background-color: #ff0000; } \
              #tall { height: 3000px; }</style></head><body><div id=\"bar\"></div><div id=\"tall\"></div>\
              <script>document.getElementById('bar').addEventListener('click', function () { \
                document.getElementById('bar').style.backgroundColor = '#00ff00'; });</script></body></html>"),
    );
    let now = Instant::now();
    engine.update_blocking(now, WAIT);
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 1000.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    let json = engine::scene_to_json(frames(&events)[0]);
    let red = json.lines().find(|l| l.contains("[255,0,0,255]")).expect("the bar is still drawn after scrolling");
    assert!(red.contains("\"rect\": [0.00,0.00,"), "at the top of the viewport: {red}");

    engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 10.0, y: 10.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(32), WAIT);
    let json = engine::scene_to_json(frames(&events)[0]);
    assert!(json.contains("[0,255,0,255]"), "the click hit the fixed bar, not the content scrolled under it");
}

#[test]
fn the_wheel_scrolls_the_container_under_the_pointer_then_the_page() {
    // Milestone 33: `overflow: auto` containers scroll on their own.
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(400.0, 300.0)));
    engine.send(
        page,
        html("<html><head><style>body { margin: 0 } #box { overflow: auto; height: 100px; } \
              #a { height: 150px; background-color: #ff0000; } #b { height: 20px; background-color: #0000ff; } \
              #tall { height: 2000px; }</style></head><body><div id=\"box\"><div id=\"a\"></div><div id=\"b\"></div></div>\
              <div id=\"tall\"></div></body></html>"),
    );
    let now = Instant::now();
    engine.update_blocking(now, WAIT);
    // Where #b is drawn: its y, plus the scroll layer's offset it is on
    // (Milestone 34: a container's content moves on a layer of its own).
    let blue_y = |events: &[EngineEvent]| {
        let json = engine::scene_to_json(frames(events)[0]);
        let second = |l: &str| l.split('[').nth(1).and_then(|n| n.split([',', ']']).nth(1)).and_then(|y| y.parse::<f32>().ok()).unwrap();
        let mut layer_dy = 0.0;
        for l in json.lines() {
            if l.contains("\"scroll\"") {
                layer_dy = second(l);
            } else if l.contains("\"end\": \"transform\"") {
                layer_dy = 0.0;
            } else if l.contains("[0,0,255,255]") {
                return Some(second(l) + layer_dy);
            }
        }
        None
    };
    engine.send(page, PageCommand::Input(EngineInput::PointerMove { x: 10.0, y: 50.0 }));
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 500.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(16), WAIT);
    // The box scrolled to its end (170 - 100 = 70): #b is at 150 - 70 = 80.
    assert_eq!(blue_y(&events), Some(80.0), "the container scrolled, clamped to its content");
    // At its end, the next wheel scrolls the page.
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 30.0 }));
    let events = engine.update_blocking(now + Duration::from_millis(32), WAIT);
    assert_eq!(blue_y(&events), Some(50.0), "the page moved by 30");
}
