//! A CPU rasteriser for a [`Scene`] — Milestone 32. Headless hosts (the
//! conformance harness) need to *see* a page without a GPU: this turns a
//! scene into RGBA pixels by the same rules the GPU backend draws with —
//! items in order, source-over blending in sRGB space (what browsers do, and
//! what `gpu_backend` does since it renders to a non-sRGB surface), glyph
//! coverage from the atlas, rounded background corners.
//!
//! It is a reference renderer, not a fast one, and it is exact about what it
//! draws rather than pretty: nearest-neighbour image sampling, one-pixel
//! anti-aliasing on rounded corners only.
//!
//! Bounded: at most [`MAX_RASTER_DIMENSION`] pixels a side, so a hostile or
//! corrupt viewport cannot make it allocate without limit.

use crate::display_list::{DisplayItem, ImageResource};
use crate::walk::Clip;
use crate::Affine;
use crate::scene::Scene;
use std::collections::HashMap;

/// Largest raster, per side.
pub const MAX_RASTER_DIMENSION: u32 = 8192;

/// An RGBA8 image, rows top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct RasterImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl RasterImage {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }
}

fn dimension(v: f32) -> u32 {
    if v.is_finite() {
        (v.round().max(1.0) as u32).min(MAX_RASTER_DIMENSION)
    } else {
        1
    }
}

/// Nested groups a raster keeps as layers; deeper ones draw straight into
/// their parent (a hostile scene cannot make it allocate a layer per item).
const MAX_LAYER_DEPTH: usize = 32;

/// A group being drawn: premultiplied RGBA over `rect` (pixel `[x0, y0,
/// x1, y1)`), composited into its parent at `opacity` when it ends.
struct Layer {
    rect: [u32; 4],
    px: Vec<[f32; 4]>,
    opacity: f32,
}

/// What items draw into: the image, or the innermost open layer.
struct Canvas {
    image: RasterImage,
    layers: Vec<Layer>,
    /// Groups opened past `MAX_LAYER_DEPTH`, drawn into their parent.
    flattened: usize,
}

impl Canvas {
    /// Source-over blend of `color` (straight alpha) at `coverage` (0–1) into
    /// pixel (`x`, `y`) of the current target.
    fn blend(&mut self, x: u32, y: u32, color: [u8; 4], coverage: f32) {
        match self.layers.last_mut() {
            None => blend(&mut self.image, x, y, color, coverage),
            Some(layer) => {
                let [x0, y0, x1, y1] = layer.rect;
                if x < x0 || y < y0 || x >= x1 || y >= y1 {
                    return;
                }
                let a = (color[3] as f32 / 255.0) * coverage.clamp(0.0, 1.0);
                if a <= 0.0 {
                    return;
                }
                let p = &mut layer.px[((y - y0) * (x1 - x0) + (x - x0)) as usize];
                for c in 0..3 {
                    p[c] = color[c] as f32 / 255.0 * a + p[c] * (1.0 - a);
                }
                p[3] = a + p[3] * (1.0 - a);
            }
        }
    }

    fn begin_group(&mut self, opacity: f32, bounds: [f32; 4]) {
        if self.layers.len() >= MAX_LAYER_DEPTH {
            self.flattened += 1;
            return;
        }
        let clamp = |v: f32, max: u32| if v.is_finite() { v.max(0.0).min(max as f32) as u32 } else if v > 0.0 { max } else { 0 };
        let (w, h) = (self.image.width, self.image.height);
        let (x0, y0) = (clamp(bounds[0].floor(), w), clamp(bounds[1].floor(), h));
        let (x1, y1) = (clamp(bounds[2].ceil(), w).max(x0), clamp(bounds[3].ceil(), h).max(y0));
        self.layers.push(Layer { rect: [x0, y0, x1, y1], px: vec![[0.0; 4]; ((x1 - x0) * (y1 - y0)) as usize], opacity });
    }

    fn end_group(&mut self) {
        if self.flattened > 0 {
            self.flattened -= 1;
            return;
        }
        let Some(layer) = self.layers.pop() else { return };
        let [x0, y0, x1, _] = layer.rect;
        let width = x1 - x0;
        for (i, p) in layer.px.iter().enumerate() {
            if p[3] <= 0.0 {
                continue;
            }
            let (x, y) = (x0 + i as u32 % width, y0 + i as u32 / width);
            let straight = [(p[0] / p[3] * 255.0).round() as u8, (p[1] / p[3] * 255.0).round() as u8, (p[2] / p[3] * 255.0).round() as u8, 255];
            self.blend(x, y, straight, p[3] * layer.opacity);
        }
    }
}

/// Draws `scene` over its (opaque) canvas colour.
pub fn rasterize(scene: &Scene) -> RasterImage {
    let (width, height) = (dimension(scene.viewport_width), dimension(scene.viewport_height));
    let c = scene.background;
    let background = [c.r, c.g, c.b, 255];
    let image = RasterImage { width, height, rgba: background.repeat((width * height) as usize) };
    let mut canvas = Canvas { image, layers: Vec::new(), flattened: 0 };
    let images: HashMap<u64, &ImageResource> = scene.images.iter().map(|r| (r.id, r)).collect();
    crate::walk::walk(&scene.items, |step| match step {
        crate::walk::Step::Draw { item, transform, clip } => draw_item(&mut canvas, scene, &images, item, &transform, &clip),
        crate::walk::Step::BeginGroup { opacity, bounds, .. } => canvas.begin_group(opacity, bounds),
        crate::walk::Step::EndGroup => canvas.end_group(),
    });
    while !canvas.layers.is_empty() {
        canvas.end_group();
    }
    canvas.image
}

/// One drawing item, through `transform`, under `clip`. Every pixel whose
/// centre falls in the item's transformed bounds is mapped back into the
/// item's own coordinates and covered by the same rules as before
/// Milestone 34 (with no transform, exactly those rules).
fn draw_item(canvas: &mut Canvas, scene: &Scene, images: &HashMap<u64, &ImageResource>, item: &DisplayItem, transform: &Affine, clip: &Clip) {
    let Some(inverse) = transform.inverse() else { return };
    let (w, h) = (canvas.image.width, canvas.image.height);
    // Visit the pixels of `local` (x, y, w, h) once transformed, calling
    // `cover(lx, ly)` for the coverage at the local point a pixel maps to.
    let paint = |canvas: &mut Canvas, local: (f32, f32, f32, f32), color: [u8; 4], cover: &dyn Fn(f32, f32) -> f32| {
        let (x, y, bw, bh) = transform.map_rect(local);
        let (x0, x1) = span(x.max(clip.rect[0]), (x + bw).min(clip.rect[2]) - x.max(clip.rect[0]), w);
        let (y0, y1) = span(y.max(clip.rect[1]), (y + bh).min(clip.rect[3]) - y.max(clip.rect[1]), h);
        for py in y0..y1 {
            for px in x0..x1 {
                let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                let clip_cover = if clip.is_none() { 1.0 } else { clip.coverage(cx, cy) };
                if clip_cover <= 0.0 {
                    continue;
                }
                let (lx, ly) = inverse.apply(cx, cy);
                let coverage = cover(lx, ly) * clip_cover;
                if coverage > 0.0 {
                    canvas.blend(px, py, color, coverage);
                }
            }
        }
    };
    match item {
        DisplayItem::Rect { x, y, width: rw, height: rh, color, radius } => {
            if !(x.is_finite() && y.is_finite() && rw.is_finite() && rh.is_finite()) || *rw <= 0.0 || *rh <= 0.0 {
                return;
            }
            let r = if radius.is_finite() { radius.max(0.0).min(rw.min(*rh) / 2.0) } else { 0.0 };
            let (x, y, rw, rh) = (*x, *y, *rw, *rh);
            let cover = move |lx: f32, ly: f32| {
                if lx < x || ly < y || lx >= x + rw || ly >= y + rh {
                    0.0
                } else if r > 0.0 {
                    rounded_coverage(lx, ly, x, y, rw, rh, r)
                } else {
                    1.0
                }
            };
            paint(canvas, (x, y, rw, rh), [color.r, color.g, color.b, color.a], &cover);
        }
        DisplayItem::Shadow { x, y, width: sw, height: sh, color, radius, blur } => {
            if !(x.is_finite() && y.is_finite() && sw.is_finite() && sh.is_finite() && blur.is_finite()) {
                return;
            }
            let b = blur.max(0.75);
            let r = radius.max(0.0).min(sw.min(*sh) / 2.0);
            let (hx, hy) = (sw / 2.0, sh / 2.0);
            let (cx, cy) = (x + hx, y + hy);
            let cover = move |lx: f32, ly: f32| {
                let (qx, qy) = ((lx - cx).abs() - (hx - r), (ly - cy).abs() - (hy - r));
                let d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
                let t = ((d + b) / (2.0 * b)).clamp(0.0, 1.0);
                1.0 - t * t * (3.0 - 2.0 * t)
            };
            paint(canvas, (x - b, y - b, sw + 2.0 * b, sh + 2.0 * b), [color.r, color.g, color.b, color.a], &cover);
        }
        DisplayItem::Glyphs { instances, .. } => {
            let atlas = &scene.glyph_atlas;
            for g in instances {
                if g.atlas_w == 0 || g.atlas_h == 0 || g.width <= 0.0 || g.height <= 0.0 {
                    continue;
                }
                let g = *g;
                let cover = move |lx: f32, ly: f32| {
                    if lx < g.x || ly < g.y || lx >= g.x + g.width || ly >= g.y + g.height {
                        return 0.0;
                    }
                    // Nearest texel of the glyph's atlas rectangle.
                    let u = ((lx - g.x) / g.width * g.atlas_w as f32).floor().clamp(0.0, g.atlas_w as f32 - 1.0) as usize;
                    let v = ((ly - g.y) / g.height * g.atlas_h as f32).floor().clamp(0.0, g.atlas_h as f32 - 1.0) as usize;
                    let (ax, ay) = (g.atlas_x as usize + u, g.atlas_y as usize + v);
                    if ax >= atlas.width || ay >= atlas.height {
                        return 0.0;
                    }
                    atlas.pixels.get(ax, ay) as f32 / 255.0
                };
                paint(canvas, (g.x, g.y, g.width, g.height), [g.color.r, g.color.g, g.color.b, g.color.a], &cover);
            }
        }
        DisplayItem::Image { x, y, width: iw, height: ih, image_id, sampling } => {
            let Some(resource) = images.get(image_id) else { return };
            if resource.width == 0 || resource.height == 0 || resource.rgba.len() < (resource.width * resource.height * 4) as usize || *iw <= 0.0 || *ih <= 0.0 {
                return;
            }
            let s = crate::display_list::ImageSampling::of(sampling);
            let (x, y, iw, ih, uv, alpha) = (*x, *y, *iw, *ih, s.uv, s.alpha.clamp(0.0, 1.0));
            let tiles = crate::display_list::uv_tiles(uv);
            let texel = |t: f32, size: u32| {
                let t = if tiles { t - t.floor() } else { t };
                (t * size as f32).floor().clamp(0.0, size as f32 - 1.0) as u32
            };
            // Images blend texel by texel: paint pixel by pixel here.
            let (bx, by, bw, bh) = transform.map_rect((x, y, iw, ih));
            let (x0, x1) = span(bx.max(clip.rect[0]), (bx + bw).min(clip.rect[2]) - bx.max(clip.rect[0]), w);
            let (y0, y1) = span(by.max(clip.rect[1]), (by + bh).min(clip.rect[3]) - by.max(clip.rect[1]), h);
            for py in y0..y1 {
                for px in x0..x1 {
                    let (cx, cy) = (px as f32 + 0.5, py as f32 + 0.5);
                    let clip_cover = if clip.is_none() { 1.0 } else { clip.coverage(cx, cy) };
                    let (lx, ly) = inverse.apply(cx, cy);
                    if clip_cover <= 0.0 || lx < x || ly < y || lx >= x + iw || ly >= y + ih {
                        continue;
                    }
                    let u = texel(uv[0] + (lx - x) / iw * (uv[2] - uv[0]), resource.width);
                    let v = texel(uv[1] + (ly - y) / ih * (uv[3] - uv[1]), resource.height);
                    let i = ((v * resource.width + u) * 4) as usize;
                    let t = [resource.rgba[i], resource.rgba[i + 1], resource.rgba[i + 2], resource.rgba[i + 3]];
                    canvas.blend(px, py, [t[0], t[1], t[2], 255], t[3] as f32 / 255.0 * alpha * clip_cover);
                }
            }
        }
        _ => {}
    }
}

/// Source-over blend of `color` at `coverage` (0–1) into pixel (`x`, `y`).
fn blend(image: &mut RasterImage, x: u32, y: u32, color: [u8; 4], coverage: f32) {
    let alpha = (color[3] as f32 / 255.0) * coverage.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    let i = ((y * image.width + x) * 4) as usize;
    for (c, &channel) in color.iter().take(3).enumerate() {
        let dst = image.rgba[i + c] as f32;
        image.rgba[i + c] = (channel as f32 * alpha + dst * (1.0 - alpha)).round() as u8;
    }
    let dst_a = image.rgba[i + 3] as f32 / 255.0;
    image.rgba[i + 3] = ((alpha + dst_a * (1.0 - alpha)) * 255.0).round() as u8;
}

/// The pixel range [start, end) covered by the span [a, a + len) on an axis
/// of `size` pixels (pixel centres inside the span).
fn span(a: f32, len: f32, size: u32) -> (u32, u32) {
    if !(a.is_finite() && len.is_finite()) || len <= 0.0 {
        return (0, 0);
    }
    let start = (a - 0.5).ceil().max(0.0).min(size as f32) as u32;
    let end = (a + len - 0.5).ceil().max(0.0).min(size as f32) as u32;
    (start, end.max(start))
}
/// Coverage of a pixel centred at (`cx`, `cy`) by a rounded rectangle: 1
/// inside, 0 outside, a one-pixel ramp across the corner arcs.
fn rounded_coverage(cx: f32, cy: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let nearest_x = cx.clamp(x + r, x + w - r);
    let nearest_y = cy.clamp(y + r, y + h - r);
    let (dx, dy) = (cx - nearest_x, cy - nearest_y);
    let distance = (dx * dx + dy * dy).sqrt();
    (r + 0.5 - distance).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::{Damage, GlyphInstance};
    use crate::scene::GlyphAtlasSnapshot;
    use std::sync::Arc;

    fn scene(items: Vec<DisplayItem>, images: Vec<ImageResource>, atlas: GlyphAtlasSnapshot) -> Scene {
        Scene { viewport_width: 20.0, viewport_height: 10.0, background: style::Color { r: 255, g: 255, b: 255, a: 255 }, items: std::sync::Arc::new(items), glyph_atlas: atlas, images, damage: Damage::Full, sequence: Scene::next_sequence(), damage_base: None }
    }

    fn no_atlas() -> GlyphAtlasSnapshot {
        GlyphAtlasSnapshot { width: 1, height: 1, generation: 0, pixels: text::AtlasPixels::new(1, 1) }
    }

    const WHITE: [u8; 4] = [255, 255, 255, 255];

    #[test]
    fn rects_fill_exactly_their_pixels_and_blend_in_order() {
        let red = style::Color { r: 255, g: 0, b: 0, a: 255 };
        let half_blue = style::Color { r: 0, g: 0, b: 255, a: 128 };
        let s = scene(
            vec![
                DisplayItem::Rect { x: 2.0, y: 1.0, width: 4.0, height: 3.0, color: red, radius: 0.0 },
                DisplayItem::Rect { x: 4.0, y: 1.0, width: 4.0, height: 3.0, color: half_blue, radius: 0.0 },
            ],
            vec![],
            no_atlas(),
        );
        let img = rasterize(&s);
        assert_eq!((img.width, img.height), (20, 10));
        assert_eq!(img.pixel(1, 1), WHITE, "left of the rect");
        assert_eq!(img.pixel(2, 1), [255, 0, 0, 255], "first covered pixel");
        assert_eq!(img.pixel(3, 3), [255, 0, 0, 255], "last covered row");
        assert_eq!(img.pixel(3, 4), WHITE, "below the rect");
        assert_eq!(img.pixel(5, 2), [127, 0, 128, 255], "blue over red, source-over in sRGB");
        assert_eq!(img.pixel(7, 2), [127, 127, 255, 255], "blue over white");
    }

    #[test]
    fn rounded_corners_leave_the_corner_pixel_uncovered() {
        let black = style::Color { r: 0, g: 0, b: 0, a: 255 };
        let s = scene(vec![DisplayItem::Rect { x: 0.0, y: 0.0, width: 10.0, height: 10.0, color: black, radius: 5.0 }], vec![], no_atlas());
        let img = rasterize(&s);
        assert_eq!(img.pixel(0, 0), WHITE, "the corner is cut");
        assert_eq!(img.pixel(5, 5), [0, 0, 0, 255], "the middle is filled");
    }

    #[test]
    fn glyphs_use_atlas_coverage_and_images_are_sampled() {
        let atlas = GlyphAtlasSnapshot { width: 2, height: 1, generation: 1, pixels: text::AtlasPixels::from_bytes(2, 1, &[255, 0]) };
        let glyph = GlyphInstance { x: 0.0, y: 0.0, width: 2.0, height: 1.0, atlas_x: 0, atlas_y: 0, atlas_w: 2, atlas_h: 1, color: style::Color { r: 0, g: 0, b: 0, a: 255 } };
        let checker = ImageResource { id: 7, width: 1, height: 1, rgba: Arc::new(vec![0, 255, 0, 255]) };
        let s = scene(
            vec![
                DisplayItem::Glyphs { instances: vec![glyph], atlas_generation: 1 },
                DisplayItem::Image { x: 10.0, y: 5.0, width: 2.0, height: 2.0, image_id: 7, sampling: crate::display_list::ImageSampling::boxed(crate::display_list::FULL_UV, 1.0) },
            ],
            vec![checker],
            atlas,
        );
        let img = rasterize(&s);
        assert_eq!(img.pixel(0, 0), [0, 0, 0, 255], "full coverage texel");
        assert_eq!(img.pixel(1, 0), WHITE, "zero coverage texel");
        assert_eq!(img.pixel(11, 6), [0, 255, 0, 255], "image scaled up");
    }

    #[test]
    fn hostile_scenes_are_bounded_and_never_panic() {
        let color = style::Color { r: 1, g: 2, b: 3, a: 255 };
        let mut s = scene(
            vec![
                DisplayItem::Rect { x: f32::NAN, y: 0.0, width: 5.0, height: 5.0, color, radius: f32::INFINITY },
                DisplayItem::Rect { x: -1e30, y: -1e30, width: 2e30, height: 2e30, color, radius: 0.0 },
                DisplayItem::Image { x: 0.0, y: 0.0, width: 5.0, height: 5.0, image_id: 404, sampling: crate::display_list::ImageSampling::boxed(crate::display_list::FULL_UV, 1.0) },
                DisplayItem::Glyphs {
                    instances: vec![GlyphInstance { x: 0.0, y: 0.0, width: 5.0, height: 5.0, atlas_x: 1_000_000, atlas_y: 9, atlas_w: 3, atlas_h: 3, color }],
                    atlas_generation: 0,
                },
            ],
            vec![],
            no_atlas(),
        );
        s.viewport_width = 1e12;
        s.viewport_height = f32::NAN;
        let img = rasterize(&s);
        assert_eq!((img.width, img.height), (MAX_RASTER_DIMENSION, 1));
    }

    #[test]
    fn tiled_uvs_wrap_and_partial_uvs_select_a_sub_rectangle() {
        // A 2x1 image: red then blue.
        let resource = ImageResource { id: 1, width: 2, height: 1, rgba: Arc::new(vec![255, 0, 0, 255, 0, 0, 255, 255]) };
        let s = scene(
            vec![
                DisplayItem::Image { x: 0.0, y: 0.0, width: 8.0, height: 1.0, image_id: 1, sampling: crate::display_list::ImageSampling::boxed([0.0, 0.0, 4.0, 1.0], 1.0) },
                DisplayItem::Image { x: 0.0, y: 1.0, width: 4.0, height: 1.0, image_id: 1, sampling: crate::display_list::ImageSampling::boxed([0.5, 0.0, 1.0, 1.0], 1.0) },
            ],
            vec![resource],
            no_atlas(),
        );
        let img = rasterize(&s);
        let px = |x: u32, y: u32| &img.rgba[((y * img.width + x) * 4) as usize..][..3];
        let row0: Vec<&[u8]> = (0..8).map(|x| px(x, 0)).collect();
        // Four 2px tiles across 8px: one pixel per texel, red/blue alternating.
        assert_eq!(row0, vec![&[255, 0, 0][..], &[0, 0, 255], &[255, 0, 0], &[0, 0, 255], &[255, 0, 0], &[0, 0, 255], &[255, 0, 0], &[0, 0, 255]]);
        assert!((0..4).all(|x| px(x, 1) == [0, 0, 255]), "only the right half (blue) is drawn");
    }

    #[test]
    fn a_shadow_fades_out_over_its_blur() {
        let black = style::Color { r: 0, g: 0, b: 0, a: 255 };
        let s = scene(vec![DisplayItem::Shadow { x: 5.0, y: 0.0, width: 10.0, height: 10.0, color: black, radius: 0.0, blur: 4.0 }], vec![], no_atlas());
        let img = rasterize(&s);
        let r = |x: u32| img.rgba[((5 * img.width + x) * 4) as usize];
        // Inside darker than the edge, the edge darker than outside.
        assert!(r(10) < r(5) && r(5) < r(2) && r(2) < 255, "{} {} {}", r(10), r(5), r(2));
        assert_eq!(r(0), 255, "past the blur: untouched");
    }
}
