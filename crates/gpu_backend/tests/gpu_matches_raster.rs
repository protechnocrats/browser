//! Milestone 34: the GPU path draws what the CPU rasteriser draws — on real
//! hardware. Each scene (clips, rounded clips, rotation, group opacity,
//! images, text, shadows) is drawn headless on the GPU, read back, and
//! compared with `rendering::rasterize`. The two differ in antialiasing and
//! image filtering, not in what goes where: a small share of pixels may
//! differ, by a bounded amount.
//!
//! No GPU (a CI runner, a VM): the test says so and passes, unless
//! `GPU_REQUIRED=1`.

use gpu_backend::GpuRenderer;
use rendering::display_list::GlyphInstance;
use rendering::{Affine, Damage, DisplayItem, GlyphAtlasSnapshot, ImageResource, RasterImage, Scene, TransformLayer};
use std::sync::Arc;
use style::Color;

const W: f32 = 160.0;
const H: f32 = 120.0;

fn renderer() -> Option<GpuRenderer> {
    match pollster::block_on(GpuRenderer::headless(W as u32, H as u32)) {
        Ok(r) => Some(r),
        Err(e) => {
            assert!(std::env::var_os("GPU_REQUIRED").is_none(), "GPU_REQUIRED is set but there is no GPU: {e}");
            eprintln!("no GPU ({e}); skipped");
            None
        }
    }
}

fn color(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color { r, g, b, a }
}

fn rect(x: f32, y: f32, w: f32, h: f32, c: Color) -> DisplayItem {
    DisplayItem::Rect { x, y, width: w, height: h, color: c, radius: 0.0 }
}

fn layer(m: Affine) -> DisplayItem {
    DisplayItem::PushTransform(Box::new(TransformLayer { matrix: m, bounds: [0.0, 0.0, W, H], element: None, origin: [0.0; 2], scroll: None }))
}

fn scene(items: Vec<DisplayItem>, atlas: &text::GlyphAtlas, images: Vec<ImageResource>) -> Scene {
    Scene {
        viewport_width: W,
        viewport_height: H,
        background: color(250, 250, 250, 255),
        items: Arc::new(items),
        glyph_atlas: GlyphAtlasSnapshot::from_atlas(atlas),
        images,
        damage: Damage::Full,
        sequence: Scene::next_sequence(),
        damage_base: None,
    }
}

/// `(share of pixels differing by more than 48 in a channel, the largest
/// difference)`.
fn compare(gpu: &RasterImage, cpu: &RasterImage) -> (f32, u8) {
    assert_eq!((gpu.width, gpu.height), (cpu.width, cpu.height));
    let (mut off, mut max) = (0usize, 0u8);
    for (a, b) in gpu.rgba.chunks(4).zip(cpu.rgba.chunks(4)) {
        let d = (0..3).map(|k| a[k].abs_diff(b[k])).max().unwrap_or(0);
        max = max.max(d);
        if d > 48 {
            off += 1;
        }
    }
    (off as f32 / (gpu.width * gpu.height) as f32, max)
}

fn glyphs(atlas: &mut text::GlyphAtlas, font: &text::Font, text: &str, x: f32, y: f32) -> DisplayItem {
    let mut pen = x;
    let mut instances = Vec::new();
    for ch in text.chars() {
        let e = atlas.get_or_insert(font, ch, 20.0);
        instances.push(GlyphInstance {
            x: pen + e.bearing_x as f32,
            y: y - e.bearing_y as f32,
            width: e.atlas_w as f32,
            height: e.atlas_h as f32,
            atlas_x: e.atlas_x,
            atlas_y: e.atlas_y,
            atlas_w: e.atlas_w,
            atlas_h: e.atlas_h,
            color: color(20, 20, 120, 255),
        });
        pen += e.advance_width;
    }
    DisplayItem::Glyphs { instances, atlas_generation: atlas.generation() }
}

/// A `w` × `h` image of four solid quadrants. Drawn at its own size, so the
/// GPU's bilinear filtering and the CPU's nearest sampling agree.
fn quadrants(id: u64, w: u32, h: u32) -> ImageResource {
    let px = [[220u8, 40, 40, 255], [40, 160, 40, 255], [40, 40, 220, 255], [230, 200, 30, 255]];
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&px[usize::from(x >= w / 2) + 2 * usize::from(y >= h / 2)]);
        }
    }
    ImageResource { id, width: w, height: h, rgba: Arc::new(rgba) }
}

fn scenes() -> Vec<(&'static str, Scene)> {
    let font = text::Font::dejavu_sans();
    let mut atlas = text::GlyphAtlas::new(512, 128);
    let rot = |deg: f32, cx: f32, cy: f32| Affine::translate(cx, cy).then_after(&Affine::rotate_deg(deg)).then_after(&Affine::translate(-cx, -cy));
    let text_item = glyphs(&mut atlas, &font, "Hello", 20.0, 60.0);
    vec![
        (
            "rects, a rounded rect and a shadow",
            scene(
                vec![
                    DisplayItem::Shadow { x: 20.0, y: 20.0, width: 50.0, height: 30.0, color: color(0, 0, 0, 120), radius: 6.0, blur: 5.0 },
                    DisplayItem::Rect { x: 20.0, y: 20.0, width: 50.0, height: 30.0, color: color(200, 60, 60, 255), radius: 6.0 },
                    rect(90.0, 10.0, 40.0, 90.0, color(60, 60, 200, 180)),
                ],
                &atlas,
                vec![],
            ),
        ),
        (
            "a rotated subtree with text",
            scene(
                vec![layer(rot(30.0, 80.0, 60.0)), rect(40.0, 40.0, 80.0, 40.0, color(60, 180, 90, 255)), text_item.clone(), DisplayItem::PopTransform],
                &atlas,
                vec![],
            ),
        ),
        (
            "a rounded clip around an image",
            scene(
                vec![
                    DisplayItem::PushClip { rect: [30.0, 20.0, 100.0, 80.0], radius: 20.0 },
                    DisplayItem::Image { x: 20.0, y: 10.0, width: 120.0, height: 100.0, image_id: 7, sampling: None },
                    DisplayItem::PopClip,
                ],
                &atlas,
                vec![quadrants(7, 120, 100)],
            ),
        ),
        (
            "cards batched out of order, one overlapped by a later badge",
            scene(
                {
                    let mut items = Vec::new();
                    for i in 0..4 {
                        let x = 5.0 + i as f32 * 38.0;
                        items.push(rect(x, 10.0, 34.0, 50.0, color(200, 200, 220, 255)));
                        items.push(DisplayItem::Image { x: x + 2.0, y: 12.0, width: 30.0, height: 30.0, image_id: 20 + i, sampling: None });
                        items.push(rect(x + 2.0, 46.0, 30.0, 10.0, color(30, 30, 30, 255)));
                    }
                    // Over the second card's image: must stay on top.
                    items.push(rect(50.0, 20.0, 30.0, 12.0, color(230, 20, 20, 200)));
                    items.push(DisplayItem::Image { x: 5.0, y: 70.0, width: 30.0, height: 30.0, image_id: 20, sampling: None });
                    items
                },
                &atlas,
                (0..4).map(|i| quadrants(20 + i, 30, 30)).collect(),
            ),
        ),
        (
            "group opacity over overlapping rects",
            scene(
                vec![
                    DisplayItem::PushEffect { opacity: 0.5, bounds: [10.0, 10.0, 120.0, 90.0] },
                    rect(10.0, 10.0, 80.0, 60.0, color(200, 0, 0, 255)),
                    rect(50.0, 40.0, 80.0, 60.0, color(0, 0, 200, 255)),
                    DisplayItem::PopEffect,
                ],
                &atlas,
                vec![],
            ),
        ),
        (
            "a clip under rotation, inside a group, with text",
            scene(
                vec![
                    DisplayItem::PushEffect { opacity: 0.8, bounds: [0.0, 0.0, W, H] },
                    layer(rot(-20.0, 80.0, 60.0)),
                    DisplayItem::PushClip { rect: [40.0, 30.0, 80.0, 60.0], radius: 0.0 },
                    rect(0.0, 0.0, W, H, color(240, 160, 30, 255)),
                    text_item,
                    DisplayItem::PopClip,
                    DisplayItem::PopTransform,
                    DisplayItem::PopEffect,
                ],
                &atlas,
                vec![],
            ),
        ),
    ]
}

#[test]
fn the_gpu_draws_what_the_cpu_rasteriser_draws() {
    let Some(mut gpu) = renderer() else { return };
    for (name, scene) in scenes() {
        let drawn = gpu.render_to_image(&scene).expect("read back");
        let expected = rendering::rasterize(&scene);
        let (off, max) = compare(&drawn, &expected);
        eprintln!("{name}: {:.2}% of pixels differ by more than 48 (largest difference {max})", off * 100.0);
        assert!(off < 0.02, "{name}: {:.2}% of pixels differ", off * 100.0);
    }
}

/// A frame that redraws only its damage leaves the back buffer exactly as
/// a full redraw of that frame would.
#[test]
fn a_damage_redraw_equals_a_full_redraw() {
    let Some(mut gpu) = renderer() else { return };
    let atlas = text::GlyphAtlas::new(512, 128);
    let before = scene(vec![rect(10.0, 10.0, 30.0, 30.0, color(200, 0, 0, 255)), rect(100.0, 60.0, 30.0, 30.0, color(0, 0, 200, 255))], &atlas, vec![]);
    let _ = gpu.render_to_image(&before);
    let mut after = scene(vec![rect(10.0, 10.0, 30.0, 30.0, color(200, 0, 0, 255)), rect(110.0, 60.0, 30.0, 30.0, color(0, 0, 200, 255))], &atlas, vec![]);
    after.damage = Damage::Rect { x: 100.0, y: 60.0, width: 40.0, height: 30.0 };
    after.damage_base = Some(before.sequence);
    let partial = gpu.render_to_image(&after).expect("read back");
    assert!(gpu.last_frame_stats().partial, "the back buffer held the base frame: only the damage was redrawn");
    let mut full = after.clone();
    full.sequence = Scene::next_sequence();
    full.damage = Damage::Full;
    full.damage_base = None;
    let whole = gpu.render_to_image(&full).expect("read back");
    assert!(!gpu.last_frame_stats().partial);
    assert!(partial.rgba == whole.rgba, "a partial redraw differs from a full one");
}

/// Adjacent images in one atlas page draw in one call.
#[test]
fn images_sharing_an_atlas_page_draw_in_one_call() {
    let Some(mut gpu) = renderer() else { return };
    let atlas = text::GlyphAtlas::new(512, 128);
    let images: Vec<ImageResource> = (0..20).map(|i| quadrants(100 + i, 16, 16)).collect();
    let items = (0..20).map(|i| DisplayItem::Image { x: (i % 10) as f32 * 16.0, y: (i / 10) as f32 * 16.0, width: 16.0, height: 16.0, image_id: 100 + i, sampling: None }).collect();
    let _ = gpu.render_to_image(&scene(items, &atlas, images)).expect("read back");
    // Twenty images: one draw for all of them, plus the copy to the window
    // (headless: none).
    assert_eq!(gpu.last_frame_stats().draw_calls, 1, "{:?}", gpu.last_frame_stats());
}
