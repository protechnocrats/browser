//! Milestone 31 acceptance, through the public API: each frame does only
//! the work its changes require, measured with the per-frame `FrameStats`
//! counters (deterministic, not wall-clock).

use engine::{Damage, Engine, EngineConfig, EngineEvent, EngineInput, FrameStats, LayoutWork, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

struct Harness {
    engine: Engine,
    page: PageId,
    now: Instant,
}

impl Harness {
    fn load(html: &str, viewport: Viewport) -> Harness {
        let mut engine = Engine::new(EngineConfig::default());
        let page = engine.create_page(PageParams::new(viewport));
        engine.send(page, PageCommand::Load(LoadRequest::Html { html: html.to_string(), url: None }));
        let mut h = Harness { engine, page, now: Instant::now() };
        h.frame().expect("the first frame");
        h
    }

    /// Advances one frame interval and returns the frame's stats, if a frame
    /// was built.
    fn frame(&mut self) -> Option<FrameStats> {
        self.now += Duration::from_millis(20);
        let events = self.engine.update_blocking(self.now, WAIT);
        events.iter().find_map(|e| match e {
            EngineEvent::Frame { stats, .. } => Some(*stats),
            _ => None,
        })
    }

    fn send(&mut self, command: PageCommand) -> Option<FrameStats> {
        self.engine.send(self.page, command);
        self.frame()
    }
}

/// A 10,000-element page: rows with a clickable toggle that changes ONE
/// row's class, plus a focusable input.
fn big_page() -> String {
    let mut rows = String::new();
    for i in 0..10_000 {
        rows.push_str(&format!("<div class=\"row\">row {i}</div>"));
    }
    format!(
        "<html><head><style>.row {{ height: 20px; }} .hot {{ background-color: #ff0000; }} #field {{ height: 30px; }} #t {{ width: 100px; height: 30px; }}</style></head>\
         <body><input id=\"field\" type=\"text\"><div id=\"t\">toggle</div>{rows}\
         <script>document.getElementById('t').addEventListener('click', function () {{ \
           document.getElementById('t').style.backgroundColor = '#00ff00'; }});</script>\
         </body></html>"
    )
}

#[test]
fn the_first_frame_does_everything_once() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(800.0, 600.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: big_page(), url: None }));
    let events = engine.update_blocking(Instant::now(), WAIT);
    let stats = events.iter().find_map(|e| if let EngineEvent::Frame { stats, .. } = e { Some(*stats) } else { None }).unwrap();
    assert!(stats.full_restyle);
    assert!(stats.restyled > 10_000);
    assert!(matches!(stats.layout, LayoutWork::Full { boxes } if boxes > 10_000));
    assert!(stats.culled > 9_000, "a 600px viewport shows a few dozen of 10,000 rows ({} culled)", stats.culled);
    assert!(stats.display_list_built);
}

#[test]
fn a_scroll_only_frame_does_no_cascade_and_no_layout() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    let stats = h.send(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 250.0 })).expect("a scroll draws a frame");
    assert_eq!(stats.restyled, 0, "no cascade");
    assert_eq!(stats.layout, LayoutWork::Reused, "no layout");
    assert!(stats.display_list_built, "the first frame to reuse a layout builds the page's display list");
    let stats = h.send(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 250.0 })).expect("a scroll draws a frame");
    assert_eq!((stats.restyled, stats.layout), (0, LayoutWork::Reused));
    assert!(!stats.display_list_built, "later ones draw from it, not from the whole layout tree");
    assert!(stats.display_items < 200, "only what is on screen: {}", stats.display_items);
    assert!(matches!(stats.damage, Damage::Rect { .. } | Damage::Full));
    // A page with a sticky box builds per frame (its position depends on
    // the scroll offset) — and still draws correctly.
    let mut sticky = Harness::load(&big_page().replace("#field {", "#t { position: sticky; top: 0; } #field {"), Viewport::new(800.0, 600.0));
    let stats = sticky.send(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 250.0 })).expect("a scroll draws a frame");
    assert_eq!(stats.layout, LayoutWork::Reused);
    assert!(stats.display_list_built);
}

#[test]
fn a_caret_or_focus_only_frame_does_no_cascade_and_no_layout() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    let focus = h.send(PageCommand::Input(EngineInput::Key { key: "Tab".into() })).expect("focus moves: a frame");
    assert_eq!((focus.restyled, focus.layout), (0, LayoutWork::Reused));
    let scrolled = h.send(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 40.0 })).expect("a scroll draws a frame");
    assert!(!scrolled.display_list_built, "the focus ring is an overlay on the page's display list");
    let back = h.send(PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: -40.0 })).expect("a scroll draws a frame");
    assert!(!back.display_list_built);
    // Moving the caret in an empty field changes nothing but the caret.
    let caret = h.send(PageCommand::Input(EngineInput::Key { key: "End".into() }));
    if let Some(caret) = caret {
        assert_eq!((caret.restyled, caret.layout), (0, LayoutWork::Reused));
    }
    // Typing is a real DOM change (the input's value): restyle stays local.
    let typed = h.send(PageCommand::Input(EngineInput::Key { key: "x".into() })).expect("typing draws");
    assert!(typed.restyled <= 2, "restyled {} elements for one keystroke", typed.restyled);
    match typed.damage {
        Damage::Rect { y, height, .. } => assert!(y < 100.0 && height < 100.0, "damage stays around the input: {:?}", typed.damage),
        other => panic!("expected a small damage rect, got {other:?}"),
    }
}

#[test]
fn one_attribute_change_on_a_10k_element_page_restyles_only_that_element() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    // Click the toggle (it sets its own inline style); it sits below the
    // 30px input.
    let stats = h.send(PageCommand::Input(EngineInput::PointerDown { x: 10.0, y: 45.0 })).expect("the click changes the page");
    assert!(!stats.full_restyle);
    assert_eq!(stats.restyled, 1, "one element's cascade, not 10,000");
}

#[test]
fn an_idle_page_builds_no_frames() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    for _ in 0..20 {
        assert!(h.frame().is_none(), "nothing changed: no frame");
    }
    assert_eq!(h.engine.next_wake(), None);
}

#[test]
fn a_resize_relayouts_but_does_not_restyle_without_media_queries() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    let stats = h.send(PageCommand::Resize(Viewport::new(700.0, 600.0))).expect("a resize draws");
    assert_eq!(stats.restyled, 0, "no @media rules: width does not affect the cascade");
    assert!(matches!(stats.layout, LayoutWork::Full { .. }), "but it does affect layout");
    assert_eq!(stats.damage, Damage::Full);
}

#[test]
fn a_dom_change_deep_in_a_big_page_relays_out_only_its_ancestor_chain() {
    let mut h = Harness::load(&big_page(), Viewport::new(800.0, 600.0));
    let stats = h.send(PageCommand::Input(EngineInput::PointerDown { x: 10.0, y: 45.0 })).expect("the click changes the page");
    match stats.layout {
        LayoutWork::Incremental { laid_out, reused_boxes } => {
            // The toggle, its ancestors (html, body), and the text input —
            // an inline-block on body's line, laid out with body's line
            // boxes whenever body is (Milestone 33).
            assert!(laid_out <= 4, "the toggle and its ancestors, not 10,000 rows: {laid_out}");
            assert!(reused_boxes > 20_000, "the rows were moved over: {reused_boxes}");
        }
        other => panic!("expected incremental layout, got {other:?}"),
    }
}
