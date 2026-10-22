//! One font face (Milestone 35, ADR 009): parsed lazily by `ttf-parser`,
//! shaped by `rustybuzz`, validated before it is used.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A face's number within a [`crate::Font`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FaceId(pub u16);

/// CSS `font-style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

/// A glyph to draw: its face, its id in that face, and the synthetic styles
/// applied when the face lacks the real ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlyphRef {
    pub face: FaceId,
    pub id: u16,
    /// [`SYNTHETIC_BOLD`] | [`SYNTHETIC_OBLIQUE`].
    pub synthetic: u8,
}

pub const SYNTHETIC_BOLD: u8 = 1;
pub const SYNTHETIC_OBLIQUE: u8 = 2;

/// Why a font was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontError {
    TooLarge(usize),
    Malformed(String),
    MissingTables,
    NoGlyphs,
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FontError::TooLarge(n) => write!(f, "font of {n} bytes exceeds the cap"),
            FontError::Malformed(e) => write!(f, "malformed font: {e}"),
            FontError::MissingTables => write!(f, "font lacks the tables needed to draw text"),
            FontError::NoGlyphs => write!(f, "font has no glyphs"),
        }
    }
}

/// The largest font file accepted (a CJK system font is about 20 MB).
pub const MAX_FONT_BYTES: usize = 64 << 20;

/// Font bytes kept for the life of the process, so their shaping faces can
/// be cached. Past this, faces are rebuilt per shaping call instead
/// (slower, never wrong) — memory stays bounded however many fonts load.
pub const MAX_RESIDENT_FONT_BYTES: usize = 256 << 20;

/// Web font bytes kept for the process, apart from the system's: a page's
/// web fonts cannot use up the room system fonts need, nor grow without
/// bound (past it, web fonts are rebuilt per call).
pub const MAX_RESIDENT_WEB_FONT_BYTES: usize = 64 << 20;

static RESIDENT: AtomicUsize = AtomicUsize::new(0);
static RESIDENT_WEB: AtomicUsize = AtomicUsize::new(0);

/// Font bytes kept for the process, by key (a file, or a web font's
/// content): every page's font set shares them — a system font loaded by
/// ten pages is in memory once.
fn shared() -> &'static std::sync::Mutex<std::collections::HashMap<String, &'static [u8]>> {
    static SHARED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, &'static [u8]>>> = std::sync::OnceLock::new();
    SHARED.get_or_init(Default::default)
}

/// Whether the bytes of `key` are already kept for the process.
pub(crate) fn is_shared(key: &str) -> bool {
    shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(key)
}

/// Where a face's bytes live.
enum Data {
    /// Kept for the process: its shaping face is built once.
    Resident(Box<rustybuzz::Face<'static>>),
    /// Owned by this face: a shaping face is built per call.
    Transient(Arc<[u8]>),
}

/// What a face is, for matching.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceInfo {
    /// Lower-case family names (the typographic and the legacy ones).
    pub families: Vec<String>,
    pub weight: u16,
    pub style: FontStyle,
}

/// Vertical metrics, in font units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Metrics {
    pub units_per_em: f32,
    pub ascender: f32,
    pub descender: f32,
    pub line_gap: f32,
    pub underline: (f32, f32),
    pub strikeout: (f32, f32),
}

pub(crate) struct Face {
    pub id: FaceId,
    pub info: FaceInfo,
    pub metrics: Metrics,
    index: u32,
    data: Data,
    /// Which ASCII characters it has glyphs for (bit n: character n) — the
    /// coverage question asked for nearly every character laid out.
    ascii: u128,
    /// The ASCII characters whose glyphs no substitution or positioning
    /// lookup touches: a word made only of them shapes to its advances,
    /// exactly — the shaping fast path.
    pub simple_ascii: u128,
    /// Each ASCII character's glyph and advance (font units), looked up once.
    ascii_glyphs: Box<[(u16, f32); 128]>,
}

fn ascii_glyphs(face: &ttf_parser::Face<'_>) -> Box<[(u16, f32); 128]> {
    let mut table = Box::new([(0u16, 0.0f32); 128]);
    for (c, slot) in table.iter_mut().enumerate() {
        if let Some(g) = face.glyph_index(char::from(c as u8)) {
            *slot = (g.0, f32::from(face.glyph_hor_advance(g).unwrap_or(0)));
        }
    }
    table
}

/// Lookup subtables examined for [`simple_ascii`], at most (a hostile font
/// with thousands gets no fast path, not an expensive load).
const MAX_SUBTABLES_EXAMINED: usize = 20_000;

fn simple_ascii(face: &ttf_parser::Face<'_>, ascii: u128) -> u128 {
    use ttf_parser::gpos::PositioningSubtable;
    use ttf_parser::gsub::SubstitutionSubtable;
    let t = face.tables();
    // Apple's layout tables are not analysed; nor is a legacy `kern` table,
    // which shapers use only when GPOS has no `kern` feature.
    let gpos_kerns = t.gpos.is_some_and(|g| g.features.into_iter().any(|f| f.tag == ttf_parser::Tag::from_bytes(b"kern")));
    if t.morx.is_some() || t.kerx.is_some() || (t.kern.is_some() && !gpos_kerns) {
        return 0;
    }
    let glyphs: Vec<(u32, ttf_parser::GlyphId)> =
        (0u8..128).filter(|c| ascii & 1 << c != 0).filter_map(|c| face.glyph_index(char::from(c)).map(|g| (u32::from(c), g))).collect();
    let mut touched = 0u128;
    let mut examined = 0usize;
    if let Some(gsub) = t.gsub {
        for lookup in gsub.lookups {
            for sub in lookup.subtables.into_iter::<SubstitutionSubtable>() {
                examined += 1;
                if examined > MAX_SUBTABLES_EXAMINED {
                    return 0;
                }
                let cov = sub.coverage();
                for (c, g) in &glyphs {
                    if cov.contains(*g) {
                        touched |= 1 << c;
                    }
                }
            }
        }
    }
    if let Some(gpos) = t.gpos {
        for lookup in gpos.lookups {
            for sub in lookup.subtables.into_iter::<PositioningSubtable>() {
                examined += 1;
                if examined > MAX_SUBTABLES_EXAMINED {
                    return 0;
                }
                let cov = sub.coverage();
                for (c, g) in &glyphs {
                    if cov.contains(*g) {
                        touched |= 1 << c;
                    }
                }
            }
        }
    }
    ascii & !touched
}

fn ascii_coverage(face: &ttf_parser::Face<'_>) -> u128 {
    (0u8..128).filter(|&c| face.glyph_index(char::from(c)).is_some_and(|g| g.0 != 0)).fold(0u128, |bits, c| bits | 1 << c)
}

impl Face {
    /// Validates `bytes` (face `index`) and makes a face of it. `resident`:
    /// keep the bytes for the process if the cap allows.
    pub fn load(id: FaceId, bytes: Vec<u8>, index: u32, resident: bool) -> Result<Face, FontError> {
        let (info, metrics) = inspect(&bytes, index)?;
        let len = bytes.len();
        let data = if resident && RESIDENT.fetch_add(len, Ordering::Relaxed) + len <= MAX_RESIDENT_FONT_BYTES {
            let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            match rustybuzz::Face::from_slice(leaked, index) {
                Some(face) => Data::Resident(Box::new(face)),
                None => return Err(FontError::Malformed("not a shapeable face".into())),
            }
        } else {
            if resident {
                RESIDENT.fetch_sub(len, Ordering::Relaxed);
            }
            Data::Transient(Arc::from(bytes.into_boxed_slice()))
        };
        let (ascii, simple, glyphs) = match &data {
            Data::Resident(f) => {
                let a = ascii_coverage(f);
                (a, simple_ascii(f, a), ascii_glyphs(f))
            }
            Data::Transient(b) => ttf_parser::Face::parse(b, index).map_or((0, 0, Box::new([(0, 0.0); 128])), |f| {
                let a = ascii_coverage(&f);
                (a, simple_ascii(&f, a), ascii_glyphs(&f))
            }),
        };
        Ok(Face { id, info, metrics, index, data, ascii, simple_ascii: simple, ascii_glyphs: glyphs })
    }

    /// A face over bytes that are already `'static` (the bundled font).
    pub fn from_static(id: FaceId, bytes: &'static [u8]) -> Result<Face, FontError> {
        Self::from_static_index(id, bytes, 0)
    }

    fn from_static_index(id: FaceId, bytes: &'static [u8], index: u32) -> Result<Face, FontError> {
        let (info, metrics) = inspect(bytes, index)?;
        let face = rustybuzz::Face::from_slice(bytes, index).ok_or_else(|| FontError::Malformed("not a shapeable face".into()))?;
        let ascii = ascii_coverage(&face);
        Ok(Face { id, info, metrics, index, ascii, simple_ascii: simple_ascii(&face, ascii), ascii_glyphs: ascii_glyphs(&face), data: Data::Resident(Box::new(face)) })
    }

    /// Face `index` of the font known as `key` — its bytes shared across the
    /// process, read with `fetch` the first time. `web`: counted against the
    /// web fonts' cap. Past a cap, the face is transient instead.
    pub fn load_shared(id: FaceId, key: &str, index: u32, web: bool, fetch: impl FnOnce() -> Option<Vec<u8>>) -> Result<Face, FontError> {
        let cached = shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(key).copied();
        if let Some(bytes) = cached {
            return Self::from_static_index(id, bytes, index);
        }
        let bytes = fetch().ok_or_else(|| FontError::Malformed("font unavailable".into()))?;
        inspect(&bytes, index)?;
        let (counter, cap) = if web { (&RESIDENT_WEB, MAX_RESIDENT_WEB_FONT_BYTES) } else { (&RESIDENT, MAX_RESIDENT_FONT_BYTES) };
        let len = bytes.len();
        if counter.fetch_add(len, Ordering::Relaxed) + len > cap {
            counter.fetch_sub(len, Ordering::Relaxed);
            return Face::load(id, bytes, index, false);
        }
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        shared().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.to_string(), leaked);
        Self::from_static_index(id, leaked, index)
    }

    /// Runs `f` with this face's shaping face.
    pub fn with<R>(&self, f: impl FnOnce(&rustybuzz::Face<'_>) -> R) -> Option<R> {
        match &self.data {
            Data::Resident(face) => Some(f(face)),
            Data::Transient(bytes) => rustybuzz::Face::from_slice(bytes, self.index).map(|face| f(&face)),
        }
    }

    /// The horizontal advance of `ch`'s glyph, in font units (fast path).
    pub fn ascii_advance(&self, ch: u8) -> (u16, f32) {
        self.ascii_glyphs[usize::from(ch & 0x7f)]
    }

    /// Whether the face has a glyph for `ch`.
    pub fn covers(&self, ch: char) -> bool {
        if (ch as u32) < 128 {
            return self.ascii & 1 << (ch as u32) != 0;
        }
        match &self.data {
            Data::Resident(f) => f.glyph_index(ch).is_some_and(|g| g.0 != 0),
            // A `cmap` lookup needs only the parser, not a shaping face.
            Data::Transient(b) => ttf_parser::Face::parse(b, self.index).is_ok_and(|f| f.glyph_index(ch).is_some_and(|g| g.0 != 0)),
        }
    }
}

/// Parses what matching and layout need, and rejects what cannot draw text.
fn inspect(bytes: &[u8], index: u32) -> Result<(FaceInfo, Metrics), FontError> {
    if bytes.len() > MAX_FONT_BYTES {
        return Err(FontError::TooLarge(bytes.len()));
    }
    let face = ttf_parser::Face::parse(bytes, index).map_err(|e| FontError::Malformed(e.to_string()))?;
    if face.number_of_glyphs() == 0 {
        return Err(FontError::NoGlyphs);
    }
    let tables = face.tables();
    if tables.cmap.is_none() || tables.hmtx.is_none() || (tables.glyf.is_none() && tables.cff.is_none() && tables.cff2.is_none()) {
        return Err(FontError::MissingTables);
    }
    let units_per_em = f32::from(face.units_per_em()).max(1.0);
    let line = |m: Option<ttf_parser::LineMetrics>, default: (f32, f32)| m.map_or(default, |m| (f32::from(m.position), f32::from(m.thickness).max(1.0)));
    let metrics = Metrics {
        units_per_em,
        ascender: f32::from(face.ascender()),
        descender: f32::from(face.descender()),
        line_gap: f32::from(face.line_gap()),
        underline: line(face.underline_metrics(), (-units_per_em * 0.1, units_per_em * 0.05)),
        strikeout: line(face.strikeout_metrics(), (units_per_em * 0.3, units_per_em * 0.05)),
    };
    let mut families: Vec<String> = Vec::new();
    for name in face.names() {
        // Typographic family (16) first, then the legacy family (1).
        if name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY || name.name_id == ttf_parser::name_id::FAMILY {
            if let Some(s) = name.to_string() {
                let s = s.trim().to_lowercase();
                if !s.is_empty() && s.len() < 256 && !families.contains(&s) && families.len() < 8 {
                    families.push(s);
                }
            }
        }
    }
    let style = if face.is_italic() {
        FontStyle::Italic
    } else if face.is_oblique() {
        FontStyle::Oblique
    } else {
        FontStyle::Normal
    };
    Ok((FaceInfo { families, weight: face.weight().to_number().clamp(1, 1000), style }, metrics))
}
