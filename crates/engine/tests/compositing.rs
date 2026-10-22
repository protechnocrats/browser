//! Milestone 34 acceptance: golden scenes for clips, rotation, opacity
//! groups and layers — a page using each, through the whole pipeline, its
//! scene pinned (`tests/golden/compositing.scene.json`; `BLESS=1` rewrites
//! it) and its pixels checked where each feature decides them.

use engine::{Engine, EngineConfig, EngineEvent, LoadRequest, PageCommand, PageParams, RasterImage, Viewport};
use std::time::{Duration, Instant};

const PAGE: &str = r#"<html><head><style>
  body { margin: 0; background-color: #ffffff; }
  div { position: absolute; }
  /* A rounded clip: the child's corner is cut off. */
  #clip { left: 10px; top: 10px; width: 60px; height: 60px; overflow: hidden; border-radius: 30px; }
  #clip div { left: 0; top: 0; width: 60px; height: 60px; background-color: #0000ff; }
  /* A rotated square, its child rotated with it. */
  #spin { left: 100px; top: 10px; width: 60px; height: 60px; background-color: #00aa00; transform: rotate(45deg); }
  #spin div { left: 20px; top: 20px; width: 20px; height: 20px; background-color: #ffff00; }
  /* A group: two overlapping boxes faded together. */
  #group { left: 190px; top: 10px; width: 80px; height: 60px; opacity: 0.5; }
  #a { left: 0; top: 0; width: 50px; height: 50px; background-color: #ff0000; }
  #b { left: 20px; top: 10px; width: 50px; height: 50px; background-color: #ff0000; }
  /* A layer that may animate, and a scroll container. */
  #layer { left: 10px; top: 90px; width: 40px; height: 40px; background-color: #aa00aa; will-change: transform; }
  #scroll { left: 100px; top: 90px; width: 60px; height: 40px; overflow: auto; }
  #scroll p { margin: 0; height: 30px; background-color: #884400; }
</style></head><body>
  <div id="clip"><div></div></div>
  <div id="spin"><div></div></div>
  <div id="group"><div id="a"></div><div id="b"></div></div>
  <div id="layer"></div>
  <div id="scroll"><p></p><p></p><p></p></div>
</body></html>"#;

fn first_frame() -> engine::Scene {
    let mut engine = Engine::new(EngineConfig::default());
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 150.0)));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html: PAGE.to_string(), url: None }));
    let events = engine.update_blocking(Instant::now(), Duration::from_secs(10));
    events
        .into_iter()
        .rev()
        .find_map(|e| if let EngineEvent::Frame { scene, .. } = e { Some(scene) } else { None })
        .expect("a frame")
}

fn px(image: &RasterImage, x: u32, y: u32) -> [u8; 3] {
    let [r, g, b, _] = image.pixel(x, y);
    [r, g, b]
}

#[test]
fn clips_rotation_groups_and_layers_match_their_golden_scene() {
    let scene = first_frame();
    let json = engine::scene_to_json(&scene);
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/compositing.scene.json");
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(path, &json).unwrap();
    }
    let golden = std::fs::read_to_string(path).expect("golden missing: run with BLESS=1 once");
    assert!(json == golden, "the compositing page's scene changed; diff {path} against:\n{json}");
    for marker in ["\"clip\"", "\"radius\"", "\"transform\"", "\"group\"", "\"scroll\""] {
        assert!(json.contains(marker), "the scene has a {marker} marker");
    }
}

#[test]
fn clips_rotation_groups_and_layers_draw_where_they_should() {
    let image = engine::rasterize(&first_frame());
    // The rounded clip: the centre is the child's blue, the corner is cut.
    assert_eq!(px(&image, 40, 40), [0, 0, 255]);
    assert_eq!(px(&image, 12, 12), [255, 255, 255], "outside the rounded corner");
    // The rotated square: its corners swing out past the box's edges...
    assert_eq!(px(&image, 130, 0), [0, 170, 0], "the top corner, above where the box was");
    assert_eq!(px(&image, 102, 12), [255, 255, 255], "the box's own corner is now outside");
    // ...and the child turns with it (its centre is the square's centre).
    assert_eq!(px(&image, 130, 40), [255, 255, 0]);
    // The group: where the boxes overlap, still one layer's 50% — not the
    // 75% two faded boxes on top of each other would give.
    let single = px(&image, 195, 15);
    let overlap = px(&image, 225, 35);
    assert_eq!(single, [255, 128, 128]);
    assert_eq!(overlap, single, "overlapping members of a group do not show through each other");
    // The promoted layer draws at its place (identity transform).
    assert_eq!(px(&image, 30, 110), [170, 0, 170]);
    // The scroll container clips its content to its box.
    assert_eq!(px(&image, 110, 100), [136, 68, 0]);
    assert_eq!(px(&image, 110, 140), [255, 255, 255], "the third paragraph is clipped away");
}
