//! Milestone 38a (ADR 013), through the public API: a page in a renderer
//! "process" that stops finishing ticks is ended and reported crashed within
//! the unresponsive bound; a page that is only slow is not. The in-process
//! launcher runs the real protocol over OS pipes, but cannot kill its thread
//! (`crates/renderer/tests` checks that a real process is gone).

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, InProcessLauncher, Isolation, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn engine(bound: Duration) -> Engine {
    let mut engine = Engine::new(EngineConfig { isolation: Isolation::Processes(Arc::new(InProcessLauncher)), ..EngineConfig::default() });
    engine.set_unresponsive_after(bound);
    engine
}

fn page(engine: &mut Engine, html: &str) -> PageId {
    let page = engine.create_page(PageParams::new(Viewport { width: 400.0, height: 300.0 }));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: html.into(), url: Some("https://example.test/".into()) }));
    page
}

/// Drives the engine without ever waiting on a page (as a desktop host
/// does) for `duration` of wall-clock time, or until `stop` says so.
fn drive(engine: &mut Engine, duration: Duration, mut stop: impl FnMut(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let end = Instant::now() + duration;
    let mut events = Vec::new();
    while Instant::now() < end {
        events.extend(engine.update(Instant::now()));
        if stop(&events) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    events
}

fn crash_of(events: &[EngineEvent], page: PageId) -> Option<&str> {
    events.iter().find_map(|e| match e {
        EngineEvent::PageCrashed { page: p, reason } if *p == page => Some(reason.as_str()),
        _ => None,
    })
}

fn frames_of(events: &[EngineEvent], page: PageId) -> usize {
    events.iter().filter(|e| matches!(e, EngineEvent::Frame { page: p, .. } if *p == page)).count()
}

/// Animates with rAF, so it keeps asking for frames.
const LIVELY: &str = "<html><body><p id=p>alive</p><script>\
    var n = 0; function f() { n++; document.getElementById('p').textContent = 'frame ' + n; requestAnimationFrame(f); }\
    requestAnimationFrame(f);</script></body></html>";

#[test]
fn an_infinite_loop_is_ended_within_the_bound_and_other_pages_carry_on() {
    let bound = Duration::from_millis(500);
    let mut engine = engine(bound);
    let other = page(&mut engine, LIVELY);
    let before = diagnostics::metrics::snapshot();
    let started = Instant::now();
    // Nested loops: Boa's loop limit counts each loop instance, so this
    // runs for days, where a bare `while (true) {}` stops at the limit.
    let runaway = page(&mut engine, "<html><body><script>for (;;) { for (var i = 0; i < 9000000; i++) {} }</script></body></html>");

    let events = drive(&mut engine, Duration::from_secs(10), |e| crash_of(e, runaway).is_some());
    let elapsed = started.elapsed();
    let reason = crash_of(&events, runaway).unwrap_or_else(|| panic!("the runaway page was never ended; events: {events:?}"));
    assert!(reason.contains("stopped responding"), "{reason}");
    // The bound, plus one watchdog interval (bound / 4), plus slack for a
    // loaded test machine.
    assert!(elapsed >= bound, "ended before the bound: {elapsed:?}");
    assert!(elapsed < bound * 3, "ended too late: {elapsed:?}");
    assert!(diagnostics::metrics::snapshot().delta(&before, "guard.page_unresponsive") >= 1);
    // Reported once.
    assert_eq!(events.iter().filter(|e| matches!(e, EngineEvent::PageCrashed { .. })).count(), 1);

    // The other page keeps drawing after the runaway was ended.
    let after = drive(&mut engine, Duration::from_millis(500), |_| false);
    assert!(frames_of(&after, other) >= 3, "the other page stopped: {} frames", frames_of(&after, other));
    assert!(crash_of(&after, other).is_none());
}

#[test]
fn a_page_that_is_slow_but_keeps_finishing_ticks_is_not_ended() {
    // Each frame is busy for about half the bound; over several bounds of
    // wall-clock time the page never owes one tick for the whole bound. The
    // pointer keeps moving, so the host sends new ticks while one is still
    // owed: the page always owes one, and only its finished ticks count as
    // progress.
    let bound = Duration::from_millis(600);
    let mut engine = engine(bound);
    let slow = page(
        &mut engine,
        "<html><body><p id=p>slow</p><script>\
         var n = 0; function f() { var t = Date.now(); while (Date.now() - t < 300) {} n++;\
         document.getElementById('p').textContent = 'frame ' + n; requestAnimationFrame(f); }\
         requestAnimationFrame(f);</script></body></html>",
    );
    let end = Instant::now() + bound * 4;
    let mut events = Vec::new();
    let mut x = 0.0;
    while Instant::now() < end {
        x = (x + 1.0) % 300.0;
        engine.send(slow, PageCommand::Input(EngineInput::PointerMove { x, y: 10.0 }));
        events.extend(engine.update(Instant::now()));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(crash_of(&events, slow).is_none(), "a slow page was ended: {:?}", crash_of(&events, slow));
    assert!(frames_of(&events, slow) >= 3, "the slow page kept drawing: {} frames", frames_of(&events, slow));
}

#[test]
fn an_idle_page_is_never_ended() {
    // Nothing owed: an idle page is asked for no ticks, however long it sits.
    let bound = Duration::from_millis(200);
    let mut engine = engine(bound);
    let idle = page(&mut engine, "<html><body><p>idle</p></body></html>");
    let events = drive(&mut engine, bound * 5, |_| false);
    assert!(crash_of(&events, idle).is_none());
    assert!(frames_of(&events, idle) >= 1);
}
