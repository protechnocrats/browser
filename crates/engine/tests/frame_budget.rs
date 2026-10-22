//! Milestone 38b (ADR 020), through the public API: an animating page whose
//! frames cannot keep up lowers its frame rate in steps, holds a steady
//! cadence, keeps answering input at once, and recovers when the load goes.

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, InProcessLauncher, Isolation, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use rendering::DisplayItem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A page whose every animation frame runs `busy_ms` of script — for the
/// first `heavy_for_ms` of its life (`Infinity`: always) — and whose box
/// turns green when clicked.
fn page_html(busy_ms: u32, heavy_for_ms: &str) -> String {
    format!(
        "<html><body><div id=b style=\"width: 60px; height: 60px; background-color: #000000\"></div><p id=n>0</p><script>\
         var start = Date.now(), n = 0;\
         function f() {{ if (Date.now() - start < {heavy_for_ms}) {{ var t = Date.now(); while (Date.now() - t < {busy_ms}) {{}} }}\
         n++; document.getElementById('n').textContent = n; requestAnimationFrame(f); }}\
         requestAnimationFrame(f);\
         document.getElementById('b').addEventListener('click', function () {{ document.getElementById('b').style.backgroundColor = '#00ff00'; }});\
         </script></body></html>"
    )
}

struct Host {
    engine: Engine,
    woken: Arc<AtomicUsize>,
    page: PageId,
}

impl Host {
    fn new(isolation: Isolation, html: String) -> Host {
        let mut engine = Engine::new(EngineConfig { isolation, ..EngineConfig::default() });
        let woken = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        engine.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
        engine.send(page, PageCommand::Load(LoadRequest::Html { html, url: Some("https://example.test/".into()) }));
        Host { engine, woken, page }
    }

    /// Runs as a desktop host does — updating when woken or due, with the
    /// real clock — until `stop` or `duration`. Frames come back with the
    /// time they were collected.
    fn run(&mut self, duration: Duration, mut stop: impl FnMut(&EngineEvent) -> bool) -> Vec<(Instant, EngineEvent)> {
        let end = Instant::now() + duration;
        let mut out = Vec::new();
        while Instant::now() < end {
            let due = self.engine.next_wake().is_some_and(|at| at <= Instant::now());
            if self.woken.swap(0, Ordering::SeqCst) == 0 && !due {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            for event in self.engine.update(Instant::now()) {
                let done = stop(&event);
                out.push((Instant::now(), event));
                if done {
                    return out;
                }
            }
        }
        out
    }
}

fn intervals(events: &[(Instant, EngineEvent)]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|(_, e)| match e {
            EngineEvent::Frame { stats, .. } => Some(stats.animation_interval_ms),
            _ => None,
        })
        .collect()
}

fn frame_count(events: &[(Instant, EngineEvent)]) -> usize {
    intervals(events).len()
}

fn a_heavy_page_degrades_and_holds_its_pacing(isolation: Isolation) {
    // 30 ms of script per frame: more than half of 17 and of 33, so the
    // page steps down to 67 ms (15 frames a second) — or, if the machine is
    // loaded enough that a frame takes over 33 ms, one step further, to 133.
    // Either way it settles: one level, and frames paced to it.
    let mut host = Host::new(isolation, page_html(30, "Infinity"));
    let before = diagnostics::metrics::snapshot();
    host.run(Duration::from_millis(1500), |_| false);
    let steady = host.run(Duration::from_secs(1), |_| false);
    let levels = intervals(&steady);
    let level = *levels.first().expect("frames while degraded");
    assert!(level == 67 || level == 133, "degraded at least two steps: {levels:?}");
    assert!(levels.iter().all(|&ms| ms == level), "settled at one level: {levels:?}");
    // Paced: about 1000 / level frames in the second (not 33 at 30 ms back
    // to back).
    let frames = frame_count(&steady) as f32;
    let expected = 1000.0 / level as f32;
    assert!(frames >= expected * 0.6 && frames <= expected * 1.25, "{frames} frames in a second at {level} ms");
    assert!(diagnostics::metrics::snapshot().delta(&before, "guard.frame_budget_exceeded") >= 2, "at least two steps down, reported");
}

#[test]
fn a_heavy_page_degrades_and_holds_its_pacing_on_a_thread() {
    a_heavy_page_degrades_and_holds_its_pacing(Isolation::Threads);
}

#[test]
fn a_heavy_page_degrades_and_holds_its_pacing_in_a_renderer_process() {
    a_heavy_page_degrades_and_holds_its_pacing(Isolation::Processes(Arc::new(InProcessLauncher)));
}

#[test]
fn input_is_answered_at_once_while_the_page_is_degraded() {
    let mut host = Host::new(Isolation::Threads, page_html(30, "Infinity"));
    host.run(Duration::from_millis(1500), |_| false);
    let mut worst = Duration::ZERO;
    for _ in 0..5 {
        let sent = Instant::now();
        let page = host.page;
        host.engine.send(page, PageCommand::Input(EngineInput::PointerDown { x: 20.0, y: 20.0 }));
        let events = host.run(Duration::from_secs(2), |e| match e {
            EngineEvent::Frame { scene, .. } => scene.items.iter().any(|i| matches!(i, DisplayItem::Rect { color, .. } if (color.r, color.g, color.b) == (0, 255, 0))),
            _ => false,
        });
        let (at, last) = events.last().expect("a frame");
        let EngineEvent::Frame { stats, .. } = last else { panic!("the click was never drawn") };
        // Still degraded: input does not reset the budget, nor wait for it.
        assert_eq!(stats.animation_interval_ms, 67);
        worst = worst.max(at.duration_since(sent));
        // Turn it back to black for the next round.
        host.engine.send(page, PageCommand::Load(LoadRequest::Html { html: page_html(30, "Infinity"), url: Some("https://example.test/".into()) }));
        host.run(Duration::from_millis(1500), |_| false);
    }
    // At worst the click waits for the animation frame already running (30
    // ms), never for the next one (67 ms later).
    assert!(worst < Duration::from_millis(60), "a click took {worst:?} to be drawn");
}

#[test]
fn a_page_recovers_its_frame_rate_when_the_load_goes() {
    // 12 ms per frame for the first 800 ms (one step down, to 33 ms), then
    // nothing: 30 fast frames at 33 ms bring it back to 17.
    let mut host = Host::new(Isolation::Threads, page_html(12, "800"));
    let early = host.run(Duration::from_millis(700), |_| false);
    assert!(intervals(&early).contains(&33), "degraded while heavy: {:?}", intervals(&early));
    let late = host.run(Duration::from_millis(3000), |_| false);
    let levels = intervals(&late);
    assert_eq!(levels.last(), Some(&17), "recovered: {levels:?}");
}

#[test]
fn a_light_page_is_never_degraded() {
    let mut host = Host::new(Isolation::Threads, page_html(0, "Infinity"));
    let events = host.run(Duration::from_secs(1), |_| false);
    let levels = intervals(&events);
    assert!(levels.len() >= 40, "{} frames", levels.len());
    assert!(levels.iter().all(|&ms| ms == 17), "{levels:?}");
}
