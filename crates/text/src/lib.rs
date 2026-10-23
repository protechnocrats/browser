//! Text: fonts, shaping, glyph rasterization and a CPU-side glyph atlas.
//!
//! Milestone 35 (ADR 009): fonts are parsed by `ttf-parser` (lazily, with
//! bounded work), shaped by `rustybuzz` (kerning, ligatures, complex
//! scripts), matched per CSS with per-character fallback across a bundled
//! face (DejaVu Sans — `assets/LICENSE_DEJAVU.txt`) and the fonts the host
//! grants ([`FontSource`]), and rasterized by our own safe rasterizer.
//! `fontdue`, which hung on one corrupted `cmap` byte, is gone.
//!
//! This crate does no GPU work — `GlyphAtlas` produces a plain CPU byte
//! buffer; uploading it is `gpu_backend`'s job.

pub mod directory;
pub mod face;
pub mod webfont;
pub mod font;
mod raster;

pub use directory::DirectoryFonts;
pub use webfont::{decode_web_font, MAX_WEB_FONT_BYTES, MAX_WEB_FONT_INPUT};
pub use face::{FaceId, FontError, FontStyle, GlyphRef, MAX_FONT_BYTES, SYNTHETIC_BOLD, SYNTHETIC_OBLIQUE};
pub use font::{CatalogEntry, Font, FontSelector, FontSource, LineMetrics, ShapedGlyph, ShapedRun, TextSpec};

use std::collections::HashMap;

/// Largest font size (px) `Font` will measure or rasterize at; larger
/// requests are clamped. `fontdue` allocates a `width * height` bitmap
/// per glyph — proportional to the square of the size — so an
/// unbounded size is an unbounded allocation (a multi-petabyte request
/// aborts the process rather than panicking). `style` already clamps
/// `font-size` to the same bound; this re-checks at the trust boundary
/// because this crate cannot assume its callers did (architecture doc
/// §1, zero-trust: every boundary validates rather than trusts).
pub const MAX_FONT_PX: f32 = 1024.0;

/// Non-finite or negative sizes become 0; everything else is clamped
/// to `MAX_FONT_PX`.
pub(crate) fn sanitize_px(px: f32) -> f32 {
    if px.is_finite() && px > 0.0 {
        if px > MAX_FONT_PX {
            diagnostics::guard(diagnostics::Guard::FontSizeClamped, "text: font size clamped at the rasterizer");
        }
        px.min(MAX_FONT_PX)
    } else {
        // A size of exactly 0 is legitimate (`font-size: 0`); only
        // NaN/inf/negative is a guard trip.
        if px != 0.0 {
            diagnostics::guard(diagnostics::Guard::FontSizeClamped, "text: non-finite or negative font size treated as 0");
        }
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMetrics {
    pub width: f32,
    /// Recommended line height (ascent - descent + line gap) at this size.
    pub line_height: f32,
    pub ascent: f32,
}

pub struct RasterizedGlyph {
    pub width: usize,
    pub height: usize,
    /// Single-channel (alpha/coverage) bitmap, row-major, `width * height` bytes.
    pub bitmap: Vec<u8>,
    /// Bitmap's left edge offset from the glyph's pen-origin, in pixels.
    pub xmin: i32,
    /// Bitmap's bottom edge offset from the baseline, in pixels.
    pub ymin: i32,
    pub advance_width: f32,
}

/// Rows per band of [`AtlasPixels`].
pub const ATLAS_BAND_ROWS: usize = 64;

/// A glyph atlas's pixels — `width` × `height` coverage bytes — stored as
/// horizontal bands of [`ATLAS_BAND_ROWS`] rows, each shared
/// copy-on-write.
///
/// A frame's `Scene` holds a clone of this (cheap: one pointer), and the
/// renderer may still hold an older one, so the atlas's next write finds
/// its pixels shared. As one buffer, that write copied the whole atlas (up
/// to 2 MiB) to change a few rows; now it copies the one band it touches.
/// And a renderer that remembers which bands it uploaded (by pointer)
/// re-uploads only the bands that changed — correct even when it skipped
/// some scenes.
#[derive(Debug, Clone, PartialEq)]
pub struct AtlasPixels {
    width: usize,
    height: usize,
    /// `ceil(height / ATLAS_BAND_ROWS)` bands of `width × ATLAS_BAND_ROWS`
    /// bytes (rows past `height` are zero).
    bands: std::sync::Arc<Vec<std::sync::Arc<Vec<u8>>>>,
}

impl AtlasPixels {
    pub fn new(width: usize, height: usize) -> AtlasPixels {
        let bands = height.div_ceil(ATLAS_BAND_ROWS);
        AtlasPixels { width, height, bands: std::sync::Arc::new((0..bands).map(|_| Self::blank_band(width)).collect()) }
    }

    /// Pixels from `width × height` row-major bytes (missing bytes are 0).
    pub fn from_bytes(width: usize, height: usize, bytes: &[u8]) -> AtlasPixels {
        let mut pixels = AtlasPixels::new(width, height);
        for y in 0..height {
            let src = bytes.get(y * width..).unwrap_or(&[]);
            let n = src.len().min(width);
            pixels.row_mut(y)[..n].copy_from_slice(&src[..n]);
        }
        pixels
    }

    /// Pixels from bands (as [`AtlasPixels::bands`] gives them) — how a
    /// process that received an atlas band by band rebuilds it. `None`
    /// unless there are exactly `ceil(height / ATLAS_BAND_ROWS)` bands of
    /// `width × ATLAS_BAND_ROWS` bytes each.
    pub fn from_bands(width: usize, height: usize, bands: Vec<std::sync::Arc<Vec<u8>>>) -> Option<AtlasPixels> {
        let band_bytes = width.checked_mul(ATLAS_BAND_ROWS)?;
        if bands.len() != height.div_ceil(ATLAS_BAND_ROWS) || bands.iter().any(|b| b.len() != band_bytes) {
            return None;
        }
        Some(AtlasPixels { width, height, bands: std::sync::Arc::new(bands) })
    }

    fn blank_band(width: usize) -> std::sync::Arc<Vec<u8>> {
        std::sync::Arc::new(vec![0u8; width * ATLAS_BAND_ROWS])
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// `width × height`.
    pub fn len(&self) -> usize {
        self.width * self.height
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `y` (`None` past the height).
    pub fn row(&self, y: usize) -> Option<&[u8]> {
        if y >= self.height {
            return None;
        }
        let band = &self.bands[y / ATLAS_BAND_ROWS];
        let start = (y % ATLAS_BAND_ROWS) * self.width;
        Some(&band[start..start + self.width])
    }

    /// The coverage at (`x`, `y`); 0 outside.
    pub fn get(&self, x: usize, y: usize) -> u8 {
        if x >= self.width {
            return 0;
        }
        self.row(y).map_or(0, |r| r[x])
    }

    /// Every row in order — the same bytes a single `width × height`
    /// buffer would hold.
    pub fn rows(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.height).filter_map(move |y| self.row(y))
    }

    /// The bands, with the first row each covers.
    pub fn bands(&self) -> impl Iterator<Item = (usize, &std::sync::Arc<Vec<u8>>)> {
        self.bands.iter().enumerate().map(|(i, b)| (i * ATLAS_BAND_ROWS, b))
    }

    /// Row `y`, to write — copying only its band if a snapshot shares it.
    fn row_mut(&mut self, y: usize) -> &mut [u8] {
        let width = self.width;
        let band = std::sync::Arc::make_mut(&mut std::sync::Arc::make_mut(&mut self.bands)[y / ATLAS_BAND_ROWS]);
        let start = (y % ATLAS_BAND_ROWS) * width;
        &mut band[start..start + width]
    }

    /// Grows to `height` rows (new rows are zero). Existing bands are kept
    /// as they are — shared, not copied.
    fn grow_to(&mut self, height: usize) {
        if height <= self.height {
            return;
        }
        let needed = height.div_ceil(ATLAS_BAND_ROWS);
        let bands = std::sync::Arc::make_mut(&mut self.bands);
        while bands.len() < needed {
            bands.push(Self::blank_band(self.width));
        }
        self.height = height;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtlasEntry {
    /// Pixel-space rect within the atlas buffer. Deliberately NOT
    /// normalized (0..1) here — the atlas can grow, which would
    /// silently invalidate a normalized UV computed before the grow.
    /// Normalize against `atlas.width()`/`atlas.height()` at draw
    /// time instead, when both are guaranteed current.
    pub atlas_x: u32,
    pub atlas_y: u32,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub bearing_x: i32,
    pub bearing_y: i32,
    pub advance_width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct GlyphKey {
    glyph: GlyphRef,
    px_size: u32, // quantized to the nearest pixel for cache purposes
}

/// The tallest a [`GlyphAtlas`] may grow. Page content decides which
/// glyphs at which sizes are rasterized, so without a cap a page could
/// grow the atlas without bound: about 20 characters at the largest
/// allowed `font-size` passed 16,384 px, past the texture limit of every
/// GPU (wgpu's default `max_texture_dimension_2d` is 8,192), which is a
/// GPU validation error. 4,096 rows at the production width of 512 is
/// 2 MiB, and holds thousands of glyphs at body-text sizes.
pub const MAX_ATLAS_HEIGHT: usize = 4096;

/// When a single frame's glyphs overflow a freshly cleared atlas, clearing
/// it again next frame would only repeat the same work (and rasterizing
/// huge glyphs every frame is exactly the CPU a hostile page wants to
/// burn), so it is cleared again only after this many frames — about a
/// second at 60 fps — giving newly needed glyphs a turn.
pub const ATLAS_RECLEAR_FRAMES: u32 = 60;

#[derive(Debug, Clone)]
struct CachedGlyph {
    entry: AtlasEntry,
    /// A `Cell`, so the hot lookup stays a shared one (a `get_mut` per
    /// glyph measured 25% slower display-list builds).
    last_used: std::cell::Cell<u64>,
}

/// A growable, single-channel (coverage) glyph atlas. Fixed width,
/// grows in height (doubling) when a shelf-packing pass runs out of
/// room — the simplest growable strategy that never has to reflow
/// already-placed glyphs — up to a hard cap ([`MAX_ATLAS_HEIGHT`]).
///
/// When a glyph does not fit under the cap it is skipped (drawn as its
/// advance only, so layout does not move) and the atlas is marked
/// overflowed; the next [`GlyphAtlas::begin_frame`] **compacts** it
/// (Milestone 34): the glyphs the last frame used are packed afresh,
/// copied, not rasterized again, and the rest are evicted — so a page's
/// glyphs survive while the stale ones of text no longer shown make room.
/// If one frame's glyphs alone overflow the atlas (it overflowed fresh,
/// right after a clear or compaction), compacting would only repeat that;
/// it is then cleared, and only every [`ATLAS_RECLEAR_FRAMES`]. Clearing is safe only between frames: display
/// items kept across frames (a page's display list) are drawn only while
/// [`GlyphAtlas::placement`] is what it was when they were built.
pub struct GlyphAtlas {
    width: usize,
    height: usize,
    initial_height: usize,
    max_height: usize,
    /// A glyph was skipped for lack of room since the last clear.
    overflowed: bool,
    /// `begin_frame` calls since the last clear (0 during the first frame
    /// after it).
    frames_since_clear: u32,
    /// The overflow happened in the first frame after a clear: that
    /// frame's glyphs alone exceed the cap.
    overflowed_when_fresh: bool,
    /// Shared copy-on-write by band, so a snapshot per frame is O(1) and
    /// a write copies at most one band — see [`AtlasPixels`].
    pixels: AtlasPixels,
    shelf_x: usize,
    shelf_y: usize,
    shelf_height: usize,
    cache: HashMap<GlyphKey, CachedGlyph, font::Fnv>,
    /// Counts [`GlyphAtlas::begin_frame`] calls: each cached glyph records
    /// the last one it was used in.
    frame: u64,
    /// Bumped every time the atlas buffer changes (new glyph packed,
    /// or grown). `gpu_backend` compares this to what it last
    /// uploaded to decide whether to re-upload the texture.
    generation: u64,
    /// Milestone 39b: changes whenever glyphs already placed may have moved
    /// or gone — a new atlas, a clear, a compaction. Display items kept
    /// across frames are drawable only while it stays the same.
    placement: u64,
}

/// Source of [`GlyphAtlas::placement`] values: unique per process.
static NEXT_PLACEMENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn new_placement() -> u64 {
    NEXT_PLACEMENT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl GlyphAtlas {
    pub fn new(width: usize, initial_height: usize) -> Self {
        let max_height = MAX_ATLAS_HEIGHT.max(initial_height);
        GlyphAtlas {
            width,
            height: initial_height,
            initial_height,
            max_height,
            overflowed: false,
            frames_since_clear: 0,
            overflowed_when_fresh: false,
            pixels: AtlasPixels::new(width, initial_height),
            shelf_x: 0,
            shelf_y: 0,
            shelf_height: 0,
            cache: HashMap::default(),
            frame: 0,
            generation: 0,
            placement: new_placement(),
        }
    }

    /// Milestone 39b: identifies where the glyphs already in the atlas are.
    /// Equal values: every glyph placed under the first is still where it
    /// was (the atlas has only grown). Display items built under one value
    /// are drawable with this atlas only while it is unchanged.
    pub fn placement(&self) -> u64 {
        self.placement
    }

    /// The same atlas with a lower height cap than [`MAX_ATLAS_HEIGHT`]
    /// (never below its current height) — for tests that need to reach
    /// the cap cheaply.
    pub fn with_max_height(mut self, max_height: usize) -> Self {
        self.max_height = max_height.max(self.height);
        self
    }

    pub fn max_height(&self) -> usize {
        self.max_height
    }

    /// Whether a glyph has been skipped for lack of room since the last
    /// clear.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Call before building each frame's glyph runs. If the atlas has
    /// overflowed, clears it (back to its initial size, releasing the
    /// memory) under a new generation, and returns `true` — at once if it
    /// overflowed after its first frame (the page's text changed), but
    /// only every [`ATLAS_RECLEAR_FRAMES`] if a single frame's glyphs
    /// overflowed it fresh (clearing would just repeat that frame's work).
    pub fn begin_frame(&mut self) -> bool {
        self.frames_since_clear = self.frames_since_clear.saturating_add(1);
        self.frame += 1;
        if !self.overflowed || (self.overflowed_when_fresh && self.frames_since_clear < ATLAS_RECLEAR_FRAMES) {
            return false;
        }
        self.placement = new_placement();
        if !self.overflowed_when_fresh {
            self.compact();
            return true;
        }
        self.frames_since_clear = 0;
        self.overflowed_when_fresh = false;
        self.cache.clear();
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_height = 0;
        self.height = self.initial_height;
        self.pixels = AtlasPixels::new(self.width, self.initial_height);
        self.overflowed = false;
        self.generation += 1;
        diagnostics::metrics::GLYPH_ATLAS_GENERATION_BUMPS.inc();
        true
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The atlas's pixels. Cloning them is O(1) (bands are shared), which
    /// is what a per-frame snapshot does.
    pub fn pixels(&self) -> &AtlasPixels {
        &self.pixels
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether `(ch, px_size)` is in the atlas (tests).
    #[cfg(test)]
    fn contains(&self, font: &Font, ch: char, px_size: f32) -> bool {
        self.cache.contains_key(&GlyphKey { glyph: font.glyph_for(ch), px_size: px_size.round() as u32 })
    }

    /// The entry for a glyph already in the atlas, without marking it used
    /// (tests).
    #[cfg(test)]
    fn clone_entry(&self, font: &Font, ch: char, px_size: f32) -> AtlasEntry {
        self.cache[&GlyphKey { glyph: font.glyph_for(ch), px_size: px_size.round() as u32 }].entry
    }

    /// Milestone 34: keeps the glyphs the last frame used — repacked from
    /// the top, their pixels copied — and evicts the rest.
    fn compact(&mut self) {
        let recent = self.frame - 1;
        let old = std::mem::replace(&mut self.pixels, AtlasPixels::new(self.width, self.initial_height));
        let mut survivors: Vec<(GlyphKey, CachedGlyph)> = self.cache.drain().filter(|(_, c)| c.last_used.get() >= recent).collect();
        // Tallest first packs shelves tightest; ties by key, so the result
        // does not depend on hash order.
        survivors.sort_by(|a, b| b.1.entry.atlas_h.cmp(&a.1.entry.atlas_h).then(a.0.cmp(&b.0)));
        (self.shelf_x, self.shelf_y, self.shelf_height) = (0, 0, 0);
        self.height = self.initial_height;
        self.overflowed = false;
        self.frames_since_clear = 0;
        for (key, cached) in survivors {
            let e = cached.entry;
            let entry = if e.atlas_w == 0 || e.atlas_h == 0 {
                Some(e) // advance only: nothing to copy
            } else {
                let (w, h) = (e.atlas_w as usize, e.atlas_h as usize);
                let mut bitmap = Vec::with_capacity(w * h);
                for row in 0..h {
                    let y = e.atlas_y as usize + row;
                    let x = e.atlas_x as usize;
                    bitmap.extend_from_slice(old.row(y).map_or(&[][..], |r| &r[x..x + w]));
                }
                let glyph = RasterizedGlyph { width: w, height: h, bitmap, xmin: e.bearing_x, ymin: e.bearing_y, advance_width: e.advance_width };
                self.pack(&glyph)
            };
            // They fitted among more glyphs before; a glyph that does not
            // now is simply evicted too.
            if let Some(entry) = entry {
                self.cache.insert(key, CachedGlyph { entry, last_used: cached.last_used });
            }
        }
        self.generation += 1;
        diagnostics::metrics::GLYPH_ATLAS_GENERATION_BUMPS.inc();
    }

    /// Returns the atlas entry for `(font, ch, px_size)`, rasterizing
    /// and packing it if this is the first time it's been requested.
    pub fn get_or_insert(&mut self, font: &Font, ch: char, px_size: f32) -> AtlasEntry {
        self.get_or_insert_glyph(font, font.glyph_for(ch), px_size)
    }

    /// Returns the atlas entry for a shaped glyph at `px_size`, rasterizing
    /// and packing it the first time.
    pub fn get_or_insert_glyph(&mut self, font: &Font, glyph: GlyphRef, px_size: f32) -> AtlasEntry {
        let key = GlyphKey { glyph, px_size: px_size.round() as u32 };
        let frame = self.frame;
        if let Some(cached) = self.cache.get(&key) {
            cached.last_used.set(frame);
            return cached.entry;
        }

        // Metrics first: a glyph that will not fit is never rasterized (a
        // hostile page's glyphs are the largest there are, and a full atlas
        // would otherwise rasterize every one of them every frame).
        let metrics = font.glyph_extent(glyph, px_size);
        let advance_only = AtlasEntry {
            atlas_x: 0,
            atlas_y: 0,
            atlas_w: 0,
            atlas_h: 0,
            bearing_x: metrics.xmin,
            bearing_y: metrics.ymin,
            advance_width: metrics.advance_width,
        };
        let entry = if metrics.width == 0 || metrics.height == 0 {
            // Space and other zero-area glyphs: no bitmap to pack,
            // just an advance-only entry.
            advance_only
        } else {
            let fits = !self.overflowed && self.has_room(metrics.width, metrics.height);
            match fits.then(|| font.rasterize_glyph(glyph, px_size)).and_then(|bitmap| self.pack(&bitmap)) {
                Some(entry) => entry,
                None => {
                    // Full: skip the glyph (its advance still applies) and
                    // leave it uncached, so the next frame, after
                    // `begin_frame` clears the atlas, packs it again.
                    if !self.overflowed {
                        self.overflowed_when_fresh = self.frames_since_clear == 0;
                        diagnostics::guard(
                            diagnostics::Guard::GlyphAtlasFull,
                            &format!("atlas at its {}px cap; glyphs skipped until the next frame", self.max_height),
                        );
                    }
                    self.overflowed = true;
                    return advance_only;
                }
            }
        };

        self.cache.insert(key, CachedGlyph { entry, last_used: std::cell::Cell::new(frame) });
        entry
    }

    /// Whether a `width`×`height` glyph would fit under the cap (the same
    /// shelf arithmetic as [`Self::pack`]).
    fn has_room(&self, width: usize, height: usize) -> bool {
        let width = width.min(self.width);
        let shelf_y = if self.shelf_x + width > self.width { self.shelf_y + self.shelf_height } else { self.shelf_y };
        shelf_y + height <= self.max_height
    }

    /// Packs `glyph`, growing the atlas up to its cap; `None` if it does
    /// not fit under the cap.
    fn pack(&mut self, glyph: &RasterizedGlyph) -> Option<AtlasEntry> {
        // Security/robustness: a glyph wider than the whole atlas is
        // reachable from ordinary, untrusted CSS — nothing clamps
        // `font-size`, and an absurd value (`font-size: 5000px` on a
        // single character) rasterizes to a bitmap wider than this
        // atlas's fixed width. This used to be an `assert!` here,
        // which PANICS on that input — a full process crash from
        // trivial page content, since nothing upstream catches it.
        // Cropping to the atlas width instead degrades to a clipped
        // glyph, matching this project's "never panics, degrades
        // gracefully" discipline for untrusted input elsewhere (see
        // `html_parser`'s module doc comment).
        let width = glyph.width.min(self.width);

        let (mut shelf_x, mut shelf_y, mut shelf_height) = (self.shelf_x, self.shelf_y, self.shelf_height);
        if shelf_x + width > self.width {
            shelf_y += shelf_height;
            shelf_x = 0;
            shelf_height = 0;
        }

        let needed_height = shelf_y + glyph.height;
        if needed_height > self.max_height {
            return None;
        }
        (self.shelf_x, self.shelf_y, self.shelf_height) = (shelf_x, shelf_y, shelf_height);
        if needed_height > self.height {
            self.grow_to_height(needed_height.next_power_of_two().min(self.max_height));
        }

        let (px, py) = (self.shelf_x, self.shelf_y);
        for row in 0..glyph.height {
            let src_start = row * glyph.width;
            self.pixels.row_mut(py + row)[px..px + width].copy_from_slice(&glyph.bitmap[src_start..src_start + width]);
        }

        self.shelf_x += width;
        self.shelf_height = self.shelf_height.max(glyph.height);
        self.generation += 1;
        diagnostics::metrics::GLYPH_ATLAS_GENERATION_BUMPS.inc();

        Some(AtlasEntry {
            atlas_x: px as u32,
            atlas_y: py as u32,
            atlas_w: width as u32,
            atlas_h: glyph.height as u32,
            bearing_x: glyph.xmin,
            bearing_y: glyph.ymin,
            advance_width: glyph.advance_width,
        })
    }

    fn grow_to_height(&mut self, new_height: usize) {
        self.pixels.grow_to(new_height);
        self.height = new_height;
        self.generation += 1;
        diagnostics::metrics::GLYPH_ATLAS_GENERATION_BUMPS.inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_nonzero_width_for_real_text() {
        let font = Font::dejavu_sans();
        let m = font.measure("Hello, engine", 16.0);
        assert!(m.width > 0.0);
        assert!(m.line_height > 0.0);
        assert!(m.ascent > 0.0);
    }

    /// Milestone 35: measuring is shaping — cached per word, and the cache
    /// stays bounded however many sizes and words a page uses.
    #[test]
    fn measuring_is_cached_and_bounded() {
        let font = Font::dejavu_sans();
        for px in [0.0, 7.5, 16.0, 33.3, 1024.0] {
            for text in ["Hello, engine", "tabs\tand ~ symbols {}", "naïve café — ünïcödé", ""] {
                let a = font.measure(text, px).width;
                assert_eq!(a, font.measure(text, px).width, "{text:?} at {px}px");
                assert!(a.is_finite() && a >= 0.0);
            }
        }
        for i in 0..20_000 {
            font.measure(&format!("w{i}"), 1.0 + (i % 50) as f32);
        }
        assert!(font.cached_runs() <= 16 * 8192);
    }

    #[test]
    fn empty_string_measures_to_zero_width() {
        let font = Font::dejavu_sans();
        let m = font.measure("", 16.0);
        assert_eq!(m.width, 0.0);
    }

    /// Security regression: ordinary, untrusted CSS (`font-size:
    /// 5000px` on a single character) rasterizes a glyph wider than a
    /// realistic atlas — this used to be an `assert!` that PANICS the
    /// whole process on that input. Packing it must now degrade to a
    /// clipped glyph instead of crashing.
    #[test]
    fn packing_a_glyph_wider_than_the_atlas_does_not_panic() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(64, 64); // deliberately tiny, real production width is 512
        let entry = atlas.get_or_insert(&font, 'W', 5000.0);
        assert!(entry.atlas_w as usize <= atlas.width(), "the packed width must be clipped to fit the atlas");
        // A second, differently-sized glyph must still pack fine
        // afterward — the atlas isn't left in a broken state.
        let normal = atlas.get_or_insert(&font, 'A', 16.0);
        assert!(normal.atlas_w > 0);
    }

    #[test]
    fn rasterizing_a_visible_character_produces_a_nonempty_bitmap() {
        let font = Font::dejavu_sans();
        let g = font.rasterize('A', 32.0);
        assert!(g.width > 0);
        assert!(g.height > 0);
        assert_eq!(g.bitmap.len(), g.width * g.height);
        // At least one covered pixel — otherwise 'A' would be invisible.
        assert!(g.bitmap.iter().any(|&b| b > 0));
    }

    #[test]
    fn space_rasterizes_to_zero_area_but_nonzero_advance() {
        let font = Font::dejavu_sans();
        let g = font.rasterize(' ', 16.0);
        assert_eq!(g.width * g.height, 0);
        assert!(g.advance_width > 0.0);
    }

    #[test]
    fn atlas_caches_repeated_glyph_requests() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(256, 64);
        let first = atlas.get_or_insert(&font, 'A', 16.0);
        let gen_after_first = atlas.generation();
        let second = atlas.get_or_insert(&font, 'A', 16.0);
        assert_eq!(first, second);
        // Requesting the same glyph again must NOT re-pack (generation unchanged).
        assert_eq!(atlas.generation(), gen_after_first);
    }

    /// A snapshot of an unchanged atlas shares every band with it (a
    /// per-frame `GlyphAtlasSnapshot` is O(1), not a copy of the atlas).
    #[test]
    fn a_snapshot_of_an_unchanged_atlas_shares_its_bands() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(256, 64);
        atlas.get_or_insert(&font, 'A', 16.0);
        let first = atlas.pixels().clone();
        let second = atlas.pixels().clone();
        assert!(first.bands().zip(second.bands()).all(|((_, a), (_, b))| std::sync::Arc::ptr_eq(a, b)));
    }

    /// Correctness under copy-on-write: packing a glyph while an older
    /// snapshot is alive (a previous frame's `Scene`, or the renderer's)
    /// leaves the snapshot untouched, and copies only the band written —
    /// not the whole atlas (performance audit).
    #[test]
    fn packing_while_a_snapshot_is_held_copies_only_the_band_it_writes() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(256, 64);
        atlas.get_or_insert(&font, 'A', 16.0);
        // Grow the atlas to several bands.
        for (i, ch) in "BCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".chars().enumerate() {
            atlas.get_or_insert(&font, ch, 24.0 + i as f32);
        }
        assert!(atlas.height() > 2 * ATLAS_BAND_ROWS, "the test needs several bands");
        let old = atlas.pixels().clone();
        let old_rows: Vec<Vec<u8>> = old.rows().map(<[u8]>::to_vec).collect();

        let entry = atlas.get_or_insert(&font, 'a', 16.0);
        assert!(entry.atlas_w > 0, "the glyph was packed");

        assert_eq!(old.rows().map(<[u8]>::to_vec).collect::<Vec<_>>(), old_rows, "the old snapshot is untouched");
        let written = (entry.atlas_y as usize / ATLAS_BAND_ROWS..=(entry.atlas_y + entry.atlas_h - 1) as usize / ATLAS_BAND_ROWS).collect::<Vec<_>>();
        for ((y, now), (_, before)) in atlas.pixels().bands().zip(old.bands()) {
            let band = y / ATLAS_BAND_ROWS;
            assert_eq!(std::sync::Arc::ptr_eq(now, before), !written.contains(&band), "band {band}: copied iff written");
        }
        assert_ne!(atlas.pixels().rows().map(<[u8]>::to_vec).collect::<Vec<_>>(), old_rows, "the atlas holds the new glyph");
    }

    #[test]
    fn atlas_packs_distinct_glyphs_into_non_overlapping_rects() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(256, 64);
        let a = atlas.get_or_insert(&font, 'A', 16.0);
        let b = atlas.get_or_insert(&font, 'B', 16.0);
        // Different glyphs should not land on the exact same atlas rect.
        assert!(a.atlas_x != b.atlas_x || a.atlas_y != b.atlas_y);
    }

    #[test]
    fn atlas_grows_when_it_runs_out_of_room() {
        let font = Font::dejavu_sans();
        // Deliberately tiny atlas to force a grow quickly.
        let mut atlas = GlyphAtlas::new(32, 8);
        let initial_height = atlas.height();
        // Pack enough distinct glyphs that an 8px-tall atlas must grow.
        for ch in "The quick brown fox jumps over the lazy dog 0123456789".chars() {
            atlas.get_or_insert(&font, ch, 24.0);
        }
        assert!(
            atlas.height() > initial_height,
            "atlas should have grown past its initial height"
        );
        // Buffer length must always match width*height after a grow.
        assert_eq!(atlas.pixels().len(), atlas.width() * atlas.height());
    }

    #[test]
    fn zero_area_glyphs_do_not_bump_generation() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(256, 64);
        atlas.get_or_insert(&font, 'A', 16.0);
        let gen_before = atlas.generation();
        atlas.get_or_insert(&font, ' ', 16.0);
        // Space has no bitmap to pack, so it shouldn't touch the buffer.
        assert_eq!(atlas.generation(), gen_before);
    }

    // ---- Milestone 25 (found by the fuzz harness): unbounded font size ----

    #[test]
    fn rasterizing_at_an_absurd_size_is_bounded_not_an_allocation_abort() {
        // Before the clamp, 99_999_999px asked fontdue for a bitmap of
        // roughly px² bytes (petabytes) and aborted the process.
        let font = Font::dejavu_sans();
        for px in [99_999_999.0_f32, 1e30, f32::INFINITY, f32::NAN, -5.0, f32::NEG_INFINITY] {
            let glyph = font.rasterize('W', px);
            assert!(glyph.width * glyph.height <= (MAX_FONT_PX as usize + 1).pow(2) * 4, "px={px}");
            assert_eq!(glyph.bitmap.len(), glyph.width * glyph.height);
        }
    }

    #[test]
    fn measuring_at_an_absurd_size_is_finite_and_agrees_with_the_clamped_size() {
        let font = Font::dejavu_sans();
        let clamped = font.measure("Hello", MAX_FONT_PX);
        // Finite-but-huge sizes clamp to the maximum...
        for px in [99_999_999.0_f32, 1e30] {
            let m = font.measure("Hello", px);
            assert!(m.width.is_finite() && m.line_height.is_finite() && m.ascent.is_finite());
            assert_eq!(m, clamped);
        }
        // ...while non-finite / negative sizes are treated as 0 (same
        // as `style`, which ignores them), never NaN/inf downstream.
        for px in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN, -1.0] {
            let m = font.measure("Hello", px);
            assert!(m.width.is_finite() && m.line_height.is_finite() && m.ascent.is_finite(), "px={px}");
            assert_eq!(m.width, 0.0);
        }
    }

    // ---- Milestone 27: every silent cap is observable ----

    #[test]
    fn clamped_and_rejected_sizes_report_a_guard_but_legitimate_sizes_do_not() {
        use diagnostics::{collector::capture, Guard};
        let font = Font::dejavu_sans();
        let (_, c) = capture(|| font.measure("x", 99_999_999.0));
        assert!(c.saw_guard(Guard::FontSizeClamped));
        let (_, c) = capture(|| font.rasterize('x', f32::NAN));
        assert!(c.saw_guard(Guard::FontSizeClamped));
        // `font-size: 0` is legal CSS and a real size; only NaN/inf/negative trip.
        let (_, c) = capture(|| (font.measure("x", 0.0), font.measure("x", 16.0), font.rasterize('x', 24.0)));
        assert!(c.events.is_empty(), "{:?}", c.events);
    }

    #[test]
    fn glyph_atlas_generation_bumps_are_counted() {
        let before = diagnostics::metrics::snapshot();
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(128, 128);
        atlas.get_or_insert(&font, 'Q', 18.0);
        assert!(diagnostics::metrics::snapshot().delta(&before, "glyph_atlas_generation_bumps") >= 1);
    }

    /// Regression (fuzz/known-issues/fontdue-cmap-hang.md): one corrupted
    /// `cmap` byte made `fontdue` 0.8 never return from loading. The
    /// parser that replaced it (ADR 009) loads, shapes and draws the same
    /// input promptly.
    #[test]
    fn the_corrupted_cmap_that_hung_fontdue_is_handled_promptly() {
        let mut bytes = include_bytes!("../assets/DejaVuSans.ttf").to_vec();
        bytes[50864] = 0xa8;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok(font) = Font::from_bytes(&bytes) {
                let _ = font.measure("Hg, 世界 ب abc", 16.0);
                let mut atlas = GlyphAtlas::new(256, 64);
                for ch in "Hgab世ب".chars() {
                    let _ = atlas.get_or_insert(&font, ch, 16.0);
                }
            }
            let _ = tx.send(());
        });
        assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(), "the corrupted font must not hang");
    }

    /// Regression (fuzz target `font_parse`): a `ttcf` header claiming
    /// 2^32-1 fonts tripped `ttf-parser`'s length debug-assert when it read
    /// the offset table. It is refused as malformed before parsing; a real
    /// collection header (one font, offset table inside the file) is not.
    #[test]
    fn a_collection_header_claiming_more_fonts_than_the_file_holds_is_refused() {
        let mut bomb = b"ttcf\x00\x01\x00\x00\xff\xff\xff\xff".to_vec();
        bomb.resize(64, 0xff);
        assert!(Font::from_bytes(&bomb).is_err());
        assert!(!face::collection_header_fits(&bomb));
        let mut one = b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x10".to_vec();
        assert!(face::collection_header_fits(&one));
        one.truncate(15);
        assert!(!face::collection_header_fits(&one));
        assert!(face::collection_header_fits(include_bytes!("../assets/DejaVuSans.ttf")), "not a collection");
    }

    /// The hostile input the 2026-09-24 audit measured: every letter and
    /// digit at the largest allowed size. Uncapped, 20 of them grew the
    /// atlas past 16,384 px — beyond every GPU's texture limit.
    #[test]
    fn hostile_glyphs_never_grow_the_atlas_past_its_cap() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(512, 128);
        assert_eq!(atlas.max_height(), MAX_ATLAS_HEIGHT);
        let (_, captured) = diagnostics::collector::capture(|| {
            for ch in ('A'..='Z').chain('a'..='z').chain('0'..='9') {
                let entry = atlas.get_or_insert(&font, ch, MAX_FONT_PX);
                assert!(entry.advance_width > 0.0, "a skipped glyph keeps its advance, so layout does not move");
                assert!(atlas.height() <= MAX_ATLAS_HEIGHT);
            }
        });
        assert!(atlas.overflowed());
        assert_eq!(atlas.pixels().len(), atlas.width() * atlas.height());
        assert!(atlas.pixels().len() <= 512 * MAX_ATLAS_HEIGHT);
        assert_eq!(captured.guard_events(diagnostics::Guard::GlyphAtlasFull).len(), 1, "reported once per overflow, not per glyph");
    }

    /// Continuous `font-size`: every distinct size is its own entry, so
    /// without the cap hundreds of sizes also grew the atlas unbounded.
    #[test]
    fn hundreds_of_distinct_sizes_stay_within_the_cap() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(512, 128);
        for step in 0..400 {
            atlas.get_or_insert(&font, 'W', 200.0 + step as f32);
            assert!(atlas.height() <= MAX_ATLAS_HEIGHT);
        }
        assert!(atlas.overflowed());
    }

    #[test]
    fn a_full_atlas_skips_the_glyph_and_keeps_earlier_ones_valid() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(32, 64).with_max_height(64); // one glyph per shelf
        assert!(!atlas.begin_frame(), "nothing to clear before an overflow");
        let first = atlas.get_or_insert(&font, 'A', 40.0);
        assert!(first.atlas_w > 0, "fits");
        let skipped = atlas.get_or_insert(&font, 'B', 60.0);
        assert_eq!((skipped.atlas_w, skipped.atlas_h), (0, 0), "no room: drawn as its advance only");
        assert!(skipped.advance_width > 0.0);
        assert_eq!(atlas.get_or_insert(&font, 'A', 40.0), first, "glyphs packed before the overflow stay valid for the rest of the frame");
        assert_eq!(atlas.height(), 64, "never grows past the cap");
    }

    fn glyph_pixels(atlas: &GlyphAtlas, e: AtlasEntry) -> Vec<u8> {
        (0..e.atlas_h as usize)
            .flat_map(|r| atlas.pixels().row(e.atlas_y as usize + r).unwrap()[e.atlas_x as usize..(e.atlas_x + e.atlas_w) as usize].to_vec())
            .collect()
    }

    /// Milestone 34: an overflow evicts the glyphs the last frame did not
    /// use and keeps the ones it did — same pixels, packed afresh — instead
    /// of clearing everything.
    #[test]
    fn an_overflow_keeps_the_glyphs_in_use_and_evicts_the_stale_ones() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(64, 64).with_max_height(64);
        // Frame 1: an old page's big glyphs fill the atlas.
        for ch in ['W', 'M', 'Q'] {
            atlas.get_or_insert(&font, ch, 30.0);
        }
        atlas.begin_frame();
        // Frame 2: the text now on screen — small glyphs, one reused big
        // one — overflows it.
        for ch in "abcdefghijklmnopqrstuvwxyz".chars() {
            atlas.get_or_insert(&font, ch, 12.0);
        }
        atlas.get_or_insert(&font, 'W', 30.0);
        atlas.get_or_insert(&font, 'Z', 40.0);
        assert!(atlas.overflowed());
        let kept_before: Vec<(char, Vec<u8>)> =
            [('a', 12.0), ('b', 12.0), ('W', 30.0)].iter().filter(|(ch, px)| atlas.contains(&font, *ch, *px)).map(|&(ch, px)| (ch, glyph_pixels(&atlas, atlas.clone_entry(&font, ch, px)))).collect();
        assert!(kept_before.len() >= 2, "the test needs glyphs packed in the frame that overflowed");

        let generation = atlas.generation();
        assert!(atlas.begin_frame(), "compacted");
        assert!(atlas.generation() > generation && !atlas.overflowed());
        assert!(!atlas.contains(&font, 'M', 30.0) && !atlas.contains(&font, 'Q', 30.0), "unused last frame: evicted");
        for (ch, pixels) in kept_before {
            let px = if ch == 'W' { 30.0 } else { 12.0 };
            assert!(atlas.contains(&font, ch, px), "{ch}: used last frame, kept");
            let e = atlas.get_or_insert(&font, ch, px);
            assert_eq!(glyph_pixels(&atlas, e), pixels, "{ch}: the same pixels, copied");
        }
    }

    /// The text changed (a later frame needs new glyphs): clear at once.
    #[test]
    fn an_overflow_after_the_first_frame_clears_at_the_next_frame() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(32, 64).with_max_height(64);
        atlas.get_or_insert(&font, 'A', 40.0);
        assert!(!atlas.begin_frame(), "frame 1 fitted");
        atlas.get_or_insert(&font, 'B', 60.0); // frame 2: no room
        assert!(atlas.overflowed());

        let generation = atlas.generation();
        assert!(atlas.begin_frame(), "cleared before frame 3");
        assert!(atlas.generation() > generation, "a new generation, so the GPU re-uploads");
        assert!(!atlas.overflowed());
        assert!(atlas.get_or_insert(&font, 'B', 60.0).atlas_w > 0, "after clearing, the skipped glyph is packed");
    }

    /// One frame's glyphs alone exceed the cap: clearing every frame would
    /// re-rasterize the same huge glyphs forever (seen on hardware: every
    /// frame of an animated hostile page). Re-clear only now and then.
    #[test]
    fn an_overflow_on_a_fresh_atlas_is_recleared_only_every_few_frames() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(32, 64).with_max_height(64);
        let (_, captured) = diagnostics::collector::capture(|| {
            for _ in 0..(3 * ATLAS_RECLEAR_FRAMES) {
                atlas.get_or_insert(&font, 'A', 40.0);
                atlas.get_or_insert(&font, 'B', 60.0);
                atlas.begin_frame();
            }
        });
        let reports = captured.guard_events(diagnostics::Guard::GlyphAtlasFull).len();
        assert!((3..=4).contains(&reports), "one report per clear, not per frame: {reports}");
    }

    #[test]
    fn a_glyph_taller_than_the_cap_is_skipped_not_packed() {
        let font = Font::dejavu_sans();
        let mut atlas = GlyphAtlas::new(1024, 64).with_max_height(128);
        let entry = atlas.get_or_insert(&font, 'M', MAX_FONT_PX);
        assert_eq!(entry.atlas_h, 0);
        assert!(atlas.height() <= 128);
    }
}
