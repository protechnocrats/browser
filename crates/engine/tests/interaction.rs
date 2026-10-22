//! M4's gap, through the public API: `:hover` follows the pointer, `:active`
//! lasts from a press to its release, and `:focus` follows focus — on a
//! thread and in a renderer process (where the release crosses the wire).

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, InProcessLauncher, Isolation, LoadRequest, PageCommand, PageId, PageParams, Viewport};
use rendering::DisplayItem;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

/// Two 100×40 boxes, the first at the top of the page; each turns green on
/// `:hover`, red on `:focus` and blue on `:active`, in that order of
/// precedence (the sheet's order); `#out`'s border box
/// turns yellow while the first box is hovered (a descendant combinator).
const PAGE: &str = "<html><head><style>\
    body { margin: 0 } .b { width: 100px; height: 40px; background-color: #808080 }\
    .b:hover { background-color: #00ff00 } .b:focus { background-color: #ff0000 } .b:active { background-color: #0000ff }\
    #out { width: 30px; height: 30px; background-color: #000000 } body:hover #out { background-color: #ffff00 }\
    </style></head><body><div class=b id=one></div><div class=b id=two></div><div id=out></div></body></html>";

struct Host {
    engine: Engine,
    page: PageId,
    now: Instant,
}

impl Host {
    fn new(isolation: Isolation) -> Host {
        let mut engine = Engine::new(EngineConfig { isolation, ..EngineConfig::default() });
        let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
        engine.send(page, PageCommand::Load(LoadRequest::Html { html: PAGE.into(), url: Some("https://example.test/".into()) }));
        let mut host = Host { engine, page, now: Instant::now() };
        host.frame_after(None).expect("a first frame");
        host
    }

    /// Sends `input` (if any) and returns the next frame's colours of the
    /// two boxes and `#out`, or `None` if no frame came.
    fn frame_after(&mut self, input: Option<EngineInput>) -> Option<[(u8, u8, u8); 3]> {
        if let Some(input) = input {
            self.engine.send(self.page, PageCommand::Input(input));
        }
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            self.now += Duration::from_millis(20);
            for event in self.engine.update_blocking(self.now, Duration::from_millis(200)) {
                if let EngineEvent::Frame { page, scene, .. } = event {
                    if page == self.page {
                        let colour = |w: f32| {
                            scene.items.iter().filter_map(move |i| match i {
                                DisplayItem::Rect { color, width, .. } if *width == w => Some((color.r, color.g, color.b)),
                                _ => None,
                            })
                        };
                        let boxes: Vec<_> = colour(100.0).collect();
                        let out = colour(30.0).next()?;
                        return Some([*boxes.first()?, *boxes.get(1)?, out]);
                    }
                }
            }
        }
        None
    }
}

const GREY: (u8, u8, u8) = (128, 128, 128);
const GREEN: (u8, u8, u8) = (0, 255, 0);
const BLUE: (u8, u8, u8) = (0, 0, 255);
const RED: (u8, u8, u8) = (255, 0, 0);
const YELLOW: (u8, u8, u8) = (255, 255, 0);

fn hover_active_and_focus_match(isolation: Isolation) {
    let mut host = Host::new(isolation);
    let move_to = |y: f32| Some(EngineInput::PointerMove { x: 50.0, y });

    assert_eq!(host.frame_after(move_to(20.0)), Some([GREEN, GREY, YELLOW]), "hover over the first box, and body:hover reaches #out");
    assert_eq!(host.frame_after(move_to(60.0)), Some([GREY, GREEN, YELLOW]), "hover moves to the second");
    assert_eq!(host.frame_after(move_to(95.0)), Some([GREY, GREY, YELLOW]), "over the body only (beside #out)");
    assert_eq!(host.frame_after(move_to(190.0)).map(|c| c[2]), Some((0, 0, 0)), "below the body: only <html> is hovered");
    assert_eq!(host.frame_after(move_to(95.0)).map(|c| c[2]), Some(YELLOW));

    // A press makes the box active — and, as a click on an element with an
    // id, focuses it (this engine's focus rule).
    assert_eq!(host.frame_after(Some(EngineInput::PointerDown { x: 50.0, y: 20.0 })), Some([BLUE, GREY, YELLOW]), "pressed: active");
    assert_eq!(host.frame_after(Some(EngineInput::PointerUp { x: 50.0, y: 20.0 })), Some([RED, GREY, YELLOW]), "released: focused (over hovered)");
    assert_eq!(host.frame_after(move_to(95.0)), Some([RED, GREY, YELLOW]), "focus stays when the pointer leaves");
    assert_eq!(host.frame_after(Some(EngineInput::PointerDown { x: 50.0, y: 60.0 })), Some([GREY, BLUE, YELLOW]), "focus and activity move to the second");
    assert_eq!(host.frame_after(Some(EngineInput::PointerUp { x: 50.0, y: 60.0 })), Some([GREY, RED, YELLOW]));
}

#[test]
fn hover_active_and_focus_match_on_a_thread() {
    hover_active_and_focus_match(Isolation::Threads);
}

#[test]
fn hover_active_and_focus_match_in_a_renderer_process() {
    hover_active_and_focus_match(Isolation::Processes(Arc::new(InProcessLauncher)));
}

/// A page with no `:hover` rule does no hit testing on a move and builds no
/// frame for it.
#[test]
fn a_move_over_a_page_without_hover_rules_builds_no_frame() {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: "<html><body><div style='height: 40px; background-color: red'></div></body></html>".into(), url: None }));
    let mut now = Instant::now();
    let mut frames = 0;
    for _ in 0..5 {
        now += Duration::from_millis(20);
        frames += engine.update_blocking(now, Duration::from_millis(200)).iter().filter(|e| matches!(e, EngineEvent::Frame { .. })).count();
    }
    assert!(frames >= 1);
    for y in 0..10 {
        engine.send(page, PageCommand::Input(EngineInput::PointerMove { x: 10.0, y: y as f32 * 10.0 }));
        now += Duration::from_millis(20);
        let events = engine.update_blocking(now, Duration::from_millis(100));
        assert!(!events.iter().any(|e| matches!(e, EngineEvent::Frame { .. })), "a move built a frame");
    }
}
