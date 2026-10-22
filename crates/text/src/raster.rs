//! Glyph outlines to coverage bitmaps (Milestone 35). Our own rasterizer,
//! in safe code: outline segments accumulate signed area into a buffer
//! whose running sum is the coverage (the classic accumulation method).
//! Work is bounded however hostile the outline: curves are flattened into
//! a capped number of lines, segments per glyph are capped, and the bitmap
//! is at most [`MAX_BITMAP_SIDE`] a side.

use crate::RasterizedGlyph;

/// The largest glyph bitmap side, px — the largest font size ([`crate::MAX_FONT_PX`]): a glyph drawn past its em box is cut there.
pub const MAX_BITMAP_SIDE: usize = 1024;
/// Line segments per glyph, past which the rest of the outline is dropped.
const MAX_SEGMENTS: usize = 50_000;
/// Oblique synthesis: a 14° slant.
const OBLIQUE_SKEW: f32 = 0.25;

/// An outline, in pixels (y down), as line segments.
#[derive(Default)]
struct Path {
    lines: Vec<[f32; 4]>,
    start: (f32, f32),
    last: (f32, f32),
    scale: f32,
    skew: f32,
}

impl Path {
    fn point(&self, x: f32, y: f32) -> (f32, f32) {
        // Font units, y up → px, y down; oblique slants the upper part right.
        (x * self.scale + y * self.scale * self.skew, -y * self.scale)
    }

    fn line_to_px(&mut self, p: (f32, f32)) {
        if self.lines.len() < MAX_SEGMENTS {
            self.lines.push([self.last.0, self.last.1, p.0, p.1]);
        }
        self.last = p;
    }

    /// Flattens a curve with control points `ctrl` (px) into lines.
    fn curve(&mut self, ctrl: &[(f32, f32)]) {
        let p0 = self.last;
        let end = ctrl[ctrl.len() - 1];
        let span = ctrl.iter().fold(0.0f32, |d, p| d.max((p.0 - p0.0).abs() + (p.1 - p0.1).abs()));
        let n = (span.sqrt() * 1.5).ceil().clamp(1.0, 32.0) as usize;
        for i in 1..=n {
            let t = i as f32 / n as f32;
            let u = 1.0 - t;
            let p = match ctrl {
                [c, e] => (u * u * p0.0 + 2.0 * u * t * c.0 + t * t * e.0, u * u * p0.1 + 2.0 * u * t * c.1 + t * t * e.1),
                [c1, c2, e] => (
                    u * u * u * p0.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * e.0,
                    u * u * u * p0.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * e.1,
                ),
                _ => end,
            };
            self.line_to_px(p);
        }
    }
}

impl ttf_parser::OutlineBuilder for Path {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = self.point(x, y);
        self.last = self.start;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.point(x, y);
        self.line_to_px(p);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let c = [self.point(x1, y1), self.point(x, y)];
        self.curve(&c);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c = [self.point(x1, y1), self.point(x2, y2), self.point(x, y)];
        self.curve(&c);
    }
    fn close(&mut self) {
        let s = self.start;
        self.line_to_px(s);
    }
}

/// Rasterizes glyph `id` of `face` at `scale` px per font unit, with
/// synthetic `bold` (widened by `embolden` px) and `oblique` styles.
pub(crate) fn rasterize(face: &ttf_parser::Face<'_>, id: u16, scale: f32, oblique: bool, embolden: usize) -> RasterizedGlyph {
    let advance = f32::from(face.glyph_hor_advance(ttf_parser::GlyphId(id)).unwrap_or(0)) * scale + embolden as f32;
    let mut path = Path { scale, skew: if oblique { OBLIQUE_SKEW } else { 0.0 }, ..Path::default() };
    let empty = RasterizedGlyph { width: 0, height: 0, bitmap: Vec::new(), xmin: 0, ymin: 0, advance_width: advance };
    if face.outline_glyph(ttf_parser::GlyphId(id), &mut path).is_none() || path.lines.is_empty() {
        return empty;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for l in &path.lines {
        for (x, y) in [(l[0], l[1]), (l[2], l[3])] {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
        return empty;
    }
    let (left, top) = (x0.floor(), y0.floor());
    let width = ((x1.ceil() - left) as usize).clamp(1, MAX_BITMAP_SIDE);
    let height = ((y1.ceil() - top) as usize).clamp(1, MAX_BITMAP_SIDE);
    let mut acc = Accumulator { width, height, cells: vec![0.0; width * height + 1] };
    for l in &path.lines {
        acc.line((l[0] - left, l[1] - top), (l[2] - left, l[3] - top));
    }
    let mut bitmap = acc.coverage();
    let mut width = width;
    if embolden > 0 {
        // Synthetic bold: each pixel takes the darkest of it and the
        // `embolden` pixels to its left.
        let w2 = width + embolden;
        let mut out = vec![0u8; w2 * height];
        for y in 0..height {
            for x in 0..w2 {
                let lo = x.saturating_sub(embolden);
                out[y * w2 + x] = (lo..=x.min(width - 1)).map(|sx| bitmap[y * width + sx]).max().unwrap_or(0);
            }
        }
        bitmap = out;
        width = w2;
    }
    RasterizedGlyph {
        width,
        height,
        bitmap,
        xmin: left as i32,
        // Bottom edge of the bitmap above the baseline (y up).
        ymin: -((top as i32) + height as i32),
        advance_width: advance,
    }
}

struct Accumulator {
    width: usize,
    height: usize,
    cells: Vec<f32>,
}

impl Accumulator {
    fn add(&mut self, index: isize, v: f32) {
        if index >= 0 {
            if let Some(c) = self.cells.get_mut(index as usize) {
                *c += v;
            }
        }
    }

    /// Adds the signed area a line segment contributes to each cell.
    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if (p0.1 - p1.1).abs() < f32::EPSILON || !(p0.0.is_finite() && p0.1.is_finite() && p1.0.is_finite() && p1.1.is_finite()) {
            return;
        }
        let (dir, a, b) = if p0.1 < p1.1 { (1.0, p0, p1) } else { (-1.0, p1, p0) };
        // Clamp x into the bitmap: area left of it still counts at column 0.
        let max_x = self.width as f32;
        let clamp = |x: f32| x.clamp(0.0, max_x);
        let dxdy = (b.0 - a.0) / (b.1 - a.1);
        let mut x = a.0;
        if a.1 < 0.0 {
            x -= a.1 * dxdy;
        }
        let first = a.1.max(0.0) as usize;
        let last = (b.1.ceil() as usize).min(self.height);
        for y in first..last {
            let row = (y * self.width) as isize;
            let dy = ((y + 1) as f32).min(b.1) - (y as f32).max(a.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (lo, hi) = if x < xnext { (clamp(x), clamp(xnext)) } else { (clamp(xnext), clamp(x)) };
            let lo_floor = lo.floor();
            let lo_i = lo_floor as isize;
            let hi_i = hi.ceil() as isize;
            if hi_i <= lo_i + 1 {
                // Within one cell.
                let mid = 0.5 * (lo + hi) - lo_floor;
                self.add(row + lo_i, d - d * mid);
                self.add(row + lo_i + 1, d * mid);
            } else {
                let s = (hi - lo).recip();
                let lo_frac = lo - lo_floor;
                let a0 = 0.5 * s * (1.0 - lo_frac) * (1.0 - lo_frac);
                let hi_frac = hi - hi.ceil() + 1.0;
                let am = 0.5 * s * hi_frac * hi_frac;
                self.add(row + lo_i, d * a0);
                if hi_i == lo_i + 2 {
                    self.add(row + lo_i + 1, d * (1.0 - a0 - am));
                } else {
                    let a1 = s * (1.5 - lo_frac);
                    self.add(row + lo_i + 1, d * (a1 - a0));
                    for xi in lo_i + 2..hi_i - 1 {
                        self.add(row + xi, d * s);
                    }
                    let a2 = a1 + (hi_i - lo_i - 3) as f32 * s;
                    self.add(row + hi_i - 1, d * (1.0 - a2 - am));
                }
                self.add(row + hi_i, d * am);
            }
            x = xnext;
        }
    }

    /// The running sum of the cells, as 8-bit coverage.
    fn coverage(&self) -> Vec<u8> {
        let mut sum = 0.0f32;
        self.cells[..self.width * self.height]
            .iter()
            .map(|c| {
                sum += c;
                (sum.abs().min(1.0) * 255.0 + 0.5) as u8
            })
            .collect()
    }
}
