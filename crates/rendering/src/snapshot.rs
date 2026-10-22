//! A deterministic, dependency-free JSON serialisation of a [`Scene`] —
//! what a headless host dumps (Milestone 29), and what golden-scene tests
//! compare (Milestone 32).
//!
//! "Deterministic" is the whole point, so three things that vary from run
//! to run are normalised away:
//!
//! - **Image ids** are `Arc` addresses (see `display_list::image_id`); they
//!   are renumbered `0, 1, 2 …` in order of first appearance, and each
//!   image is identified by its size and a hash of its pixels instead.
//! - **Coordinates** are written with two decimals, so a sub-hundredth-of-
//!   a-pixel float difference does not read as a layout change.
//! - **The glyph atlas** is summarised (size, generation, pixel hash)
//!   rather than dumped.

use crate::display_list::DisplayItem;
use crate::scene::Scene;
use std::collections::HashMap;
use std::fmt::Write;

/// FNV-1a, 64-bit: a stable content hash with no dependency. Not
/// cryptographic — it only has to tell "same pixels" from "different
/// pixels" in a test.
pub fn content_hash(bytes: &[u8]) -> u64 {
    content_hash_of_parts(std::iter::once(bytes))
}

/// [`content_hash`] of the concatenation of `parts`.
pub fn content_hash_of_parts<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for bytes in parts {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn num(out: &mut String, v: f32) {
    // `-0.00` and `0.00` are the same position.
    let rounded = (v * 100.0).round() / 100.0;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    let _ = write!(out, "{rounded:.2}");
}

fn color(out: &mut String, c: style::Color) {
    let _ = write!(out, "[{},{},{},{}]", c.r, c.g, c.b, c.a);
}

/// The scene as pretty-enough JSON: one display item per line, so a diff
/// between two snapshots points at the items that changed.
pub fn scene_to_json(scene: &Scene) -> String {
    let mut ordinal: HashMap<u64, usize> = HashMap::new();
    for item in scene.items.iter() {
        if let DisplayItem::Image { image_id, .. } = item {
            let next = ordinal.len();
            ordinal.entry(*image_id).or_insert(next);
        }
    }

    let mut out = String::from("{\n");
    out.push_str("  \"viewport\": [");
    num(&mut out, scene.viewport_width);
    out.push(',');
    num(&mut out, scene.viewport_height);
    out.push_str("],\n");
    out.push_str("  \"background\": ");
    color(&mut out, scene.background);
    out.push_str(",\n");
    let _ = writeln!(
        out,
        "  \"glyph_atlas\": {{\"width\": {}, \"height\": {}, \"hash\": \"{:016x}\"}},",
        scene.glyph_atlas.width,
        scene.glyph_atlas.height,
        content_hash_of_parts(scene.glyph_atlas.pixels.rows())
    );

    let mut images: Vec<(usize, &crate::display_list::ImageResource)> =
        scene.images.iter().filter_map(|r| ordinal.get(&r.id).map(|&n| (n, r))).collect();
    images.sort_by_key(|(n, _)| *n);
    out.push_str("  \"images\": [");
    for (i, (n, r)) in images.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{{\"id\": {n}, \"size\": [{}, {}], \"hash\": \"{:016x}\"}}", r.width, r.height, content_hash(&r.rgba));
    }
    out.push_str("],\n");

    out.push_str("  \"items\": [\n");
    for (i, item) in scene.items.iter().enumerate() {
        out.push_str("    ");
        match item {
            DisplayItem::Shadow { x, y, width, height, color: c, radius, blur } => {
                out.push_str("{\"shadow\": [");
                for (k, v) in [x, y, width, height, radius, blur].into_iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    num(&mut out, *v);
                }
                out.push_str("], \"color\": ");
                color(&mut out, *c);
                out.push('}');
            }
            DisplayItem::Rect { x, y, width, height, color: c, radius } => {
                out.push_str("{\"rect\": [");
                for (k, v) in [x, y, width, height].into_iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    num(&mut out, *v);
                }
                out.push_str("], \"color\": ");
                color(&mut out, *c);
                if *radius != 0.0 {
                    out.push_str(", \"radius\": ");
                    num(&mut out, *radius);
                }
                out.push('}');
            }
            DisplayItem::Glyphs { instances, .. } => {
                out.push_str("{\"glyphs\": [");
                for (k, g) in instances.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    num(&mut out, g.x);
                    out.push(',');
                    num(&mut out, g.y);
                    out.push(',');
                    num(&mut out, g.width);
                    out.push(',');
                    num(&mut out, g.height);
                    out.push(']');
                }
                out.push_str("], \"color\": ");
                match instances.first() {
                    Some(g) => color(&mut out, g.color),
                    None => out.push_str("null"),
                }
                out.push('}');
            }
            DisplayItem::Image { x, y, width, height, image_id, sampling } => {
                let crate::display_list::ImageSampling { uv, alpha } = crate::display_list::ImageSampling::of(sampling);
                let (uv, alpha) = (&uv, &alpha);
                let _ = write!(out, "{{\"image\": {}, \"at\": [", ordinal.get(image_id).copied().unwrap_or(usize::MAX));
                for (k, v) in [x, y, width, height].into_iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    num(&mut out, *v);
                }
                out.push(']');
                // Only a partial or tiled image says which part it draws.
                if *uv != crate::display_list::FULL_UV {
                    out.push_str(", \"uv\": [");
                    for (k, v) in uv.iter().enumerate() {
                        if k > 0 {
                            out.push(',');
                        }
                        num(&mut out, *v);
                    }
                    out.push(']');
                }
                if *alpha < 1.0 {
                    out.push_str(", \"alpha\": ");
                    num(&mut out, *alpha);
                }
                out.push('}');
            }
            DisplayItem::PushClip { rect, radius } => {
                out.push_str("{\"clip\": [");
                for (k, v) in rect.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    num(&mut out, *v);
                }
                out.push(']');
                if *radius != 0.0 {
                    out.push_str(", \"radius\": ");
                    num(&mut out, *radius);
                }
                out.push('}');
            }
            DisplayItem::PushTransform(layer) if layer.scroll.is_some() => {
                out.push_str("{\"scroll\": [");
                num(&mut out, layer.matrix.0[4]);
                out.push(',');
                num(&mut out, layer.matrix.0[5]);
                out.push_str("]}");
            }
            DisplayItem::PushTransform(layer) => {
                out.push_str("{\"transform\": [");
                for (k, v) in layer.matrix.0.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    // Matrix entries to four places: a rotation's sines and
                    // cosines need more than positions do.
                    let rounded = (v * 10_000.0).round() / 10_000.0;
                    let _ = write!(out, "{:.4}", if rounded == 0.0 { 0.0 } else { rounded });
                }
                out.push_str("]}");
            }
            DisplayItem::PushEffect { opacity, .. } => {
                out.push_str("{\"group\": ");
                num(&mut out, *opacity);
                out.push('}');
            }
            DisplayItem::PopClip => out.push_str("{\"end\": \"clip\"}"),
            DisplayItem::PopTransform => out.push_str("{\"end\": \"transform\"}"),
            DisplayItem::PopEffect => out.push_str("{\"end\": \"group\"}"),
        }
        out.push_str(if i + 1 < scene.items.len() { ",\n" } else { "\n" });
    }
    out.push_str("  ]\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::{GlyphInstance, ImageResource};
    use crate::scene::GlyphAtlasSnapshot;
    use std::sync::Arc;

    fn scene_with_image_ids(a: u64, b: u64) -> Scene {
        let red = style::Color { r: 255, g: 0, b: 0, a: 255 };
        Scene {
            viewport_width: 800.0,
            viewport_height: 600.0,
            background: style::Color { r: 255, g: 255, b: 255, a: 255 },
            items: Arc::new(vec![
                DisplayItem::Rect { x: 1.004, y: -0.001, width: 10.0, height: 20.0, color: red, radius: 0.0 },
                DisplayItem::Image { x: 0.0, y: 0.0, width: 4.0, height: 4.0, image_id: b, sampling: crate::display_list::ImageSampling::boxed(crate::display_list::FULL_UV, 1.0) },
                DisplayItem::Image { x: 5.0, y: 0.0, width: 4.0, height: 4.0, image_id: a, sampling: crate::display_list::ImageSampling::boxed(crate::display_list::FULL_UV, 1.0) },
                DisplayItem::Glyphs {
                    instances: vec![GlyphInstance { x: 1.0, y: 2.0, width: 3.0, height: 4.0, atlas_x: 0, atlas_y: 0, atlas_w: 3, atlas_h: 4, color: red }],
                    atlas_generation: 7,
                },
            ]),
            glyph_atlas: GlyphAtlasSnapshot { width: 4, height: 4, generation: 7, pixels: text::AtlasPixels::new(4, 4) },
            images: vec![
                ImageResource { id: a, width: 1, height: 1, rgba: Arc::new(vec![1, 2, 3, 4]) },
                ImageResource { id: b, width: 2, height: 1, rgba: Arc::new(vec![5; 8]) },
            ],
            damage: crate::display_list::Damage::Full,
            sequence: 1,
            damage_base: None,
        }
    }

    #[test]
    fn snapshots_ignore_image_addresses_and_float_noise() {
        let one = scene_to_json(&scene_with_image_ids(0x1000, 0x2000));
        let two = scene_to_json(&scene_with_image_ids(0x9999, 0x7777));
        assert_eq!(one, two, "the same scene with different Arc addresses must serialise identically");
        assert!(one.contains("{\"rect\": [1.00,0.00,10.00,20.00]"), "{one}");
        assert!(one.contains("{\"image\": 0, \"at\": [0.00"), "renumbered by first appearance: {one}");
        assert!(one.contains("\"size\": [2, 1]"), "{one}");
    }

    #[test]
    fn snapshots_see_real_changes() {
        let base = scene_with_image_ids(1, 2);
        let mut moved = base.clone();
        if let DisplayItem::Rect { x, .. } = &mut std::sync::Arc::make_mut(&mut moved.items)[0] {
            *x += 1.0;
        }
        assert_ne!(scene_to_json(&base), scene_to_json(&moved));
        let mut repixeled = base.clone();
        repixeled.images[1].rgba = Arc::new(vec![6; 8]);
        assert_ne!(scene_to_json(&base), scene_to_json(&repixeled));
    }
}
