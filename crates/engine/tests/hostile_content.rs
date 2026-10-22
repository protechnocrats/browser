//! Hostile page content against the GPU-facing resources the page decides
//! (principles audit, 2026-09-24, P1): the glyph atlas. Public API only,
//! like `headless_host.rs`, plus the `text` constant the cap is checked
//! against.

use engine::{Engine, EngineConfig, EngineEvent, EngineInput, LoadRequest, PageCommand, PageParams, Viewport};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);

fn frames(events: &[EngineEvent]) -> Vec<&engine::Scene> {
    events.iter().filter_map(|e| if let EngineEvent::Frame { scene, .. } = e { Some(scene) } else { None }).collect()
}

/// Every letter and digit at the largest `font-size` the engine allows,
/// plus many distinct large sizes. Before the cap, about 20 of these glyphs
/// grew the atlas past 16,384 px — beyond every GPU's texture limit.
fn hostile_page() -> String {
    let alphabet: String = ('A'..='Z').chain('a'..='z').chain('0'..='9').collect();
    let mut html = format!("<html><body><p style=\"font-size: 1024px; margin: 0\">{alphabet}</p>");
    for size in (300..700).step_by(5) {
        html.push_str(&format!("<span style=\"font-size: {size}px\">W</span>"));
    }
    html.push_str("</body></html>");
    html
}

#[test]
fn a_page_of_huge_glyphs_renders_within_the_atlas_cap_and_says_so() {
    let before = diagnostics::metrics::snapshot();
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(800.0, 600.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: hostile_page(), url: None }));

    let start = Instant::now();
    let events = engine.update_blocking(start, WAIT);
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })), "{events:?}");
    let first = frames(&events);
    assert_eq!(first.len(), 1, "the page still produces a frame");
    assert!(first[0].glyph_atlas.height <= text::MAX_ATLAS_HEIGHT, "atlas {}px", first[0].glyph_atlas.height);
    assert!(engine::scene_to_json(first[0]).contains("\"glyphs\""), "the glyphs that fit are drawn");
    assert!(
        diagnostics::metrics::snapshot().delta(&before, "guard.glyph_atlas_full") >= 1,
        "the overflow is reported, not silent"
    );

    // The next frame starts from a cleared atlas and still draws text.
    engine.send(page, PageCommand::Input(EngineInput::Scroll { dx: 0.0, dy: 200.0 }));
    let events = engine.update_blocking(start + Duration::from_millis(16), WAIT);
    let second = frames(&events);
    assert_eq!(second.len(), 1);
    assert!(second[0].glyph_atlas.height <= text::MAX_ATLAS_HEIGHT);
    let glyphs = engine::scene_to_json(second[0]).matches("\"glyphs\"").count();
    assert!(glyphs > 0, "text that fits is still drawn");
}
