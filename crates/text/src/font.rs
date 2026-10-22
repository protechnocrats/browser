//! The engine's fonts (Milestone 35, ADR 009): a bundled face plus whatever
//! the host offers through a [`FontSource`], matched per CSS (family, style,
//! weight), with per-character fallback, shaped by `rustybuzz` and cached.
//!
//! The engine never enumerates system fonts: the host grants a source (a
//! capability), which also decides which fonts content may see (M37's
//! fingerprinting policy filters the catalog).

use crate::face::{Face, FaceId, FaceInfo, FontError, FontStyle, GlyphRef, SYNTHETIC_BOLD, SYNTHETIC_OBLIQUE};
use crate::{sanitize_px, RasterizedGlyph, TextMetrics};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

/// The font bundled with the engine: DejaVu Sans (`assets/LICENSE_DEJAVU.txt`).
const DEJAVU_SANS_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

/// Distinct font selectors kept; past it, new ones get the default (a page
/// cannot grow this table without bound).
pub const MAX_SELECTORS: usize = 1024;
/// Faces loaded at most (bundled, system and web together).
pub const MAX_FACES: usize = 512;
/// Web fonts one font set takes at most.
pub const MAX_WEB_FACES: usize = 256;
/// Shaped words kept per cache shard before it starts over.
const SHAPED_PER_SHARD: usize = 8192;
const SHARDS: usize = 16;
/// Catalog fonts one fallback search may load.
const FALLBACK_LOADS: usize = 4;

/// One font the host offers: where it is (an opaque key the source
/// understands), what it is, and which languages it covers (hints for
/// fallback).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub key: String,
    pub index: u32,
    /// Lower-case family names.
    pub families: Vec<String>,
    pub weight: u16,
    pub style: FontStyle,
    /// Language tags it covers (`ar`, `he`, `hi`, `ja`, `zh-cn`, …).
    pub langs: Vec<String>,
}

/// The host's fonts, granted to the engine.
pub trait FontSource: Send + Sync {
    /// Every font content may use.
    fn catalog(&self) -> Vec<CatalogEntry>;
    /// The bytes of one of them.
    fn load(&self, entry: &CatalogEntry) -> Option<Vec<u8>>;
}

/// A CSS `font-family`, weight and style, interned: small and `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FontSelector(pub u32);

/// Everything shaping a run depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextSpec {
    pub selector: FontSelector,
    pub size: f32,
    /// Right-to-left (a bidi run of odd level).
    pub rtl: bool,
}

impl TextSpec {
    /// The default font at `size`, left to right.
    pub fn plain(size: f32) -> TextSpec {
        TextSpec { selector: FontSelector::default(), size, rtl: false }
    }

    fn key(&self) -> (u32, u32, bool) {
        (self.selector.0, sanitize_px(self.size).to_bits(), self.rtl)
    }
}

/// One shaped glyph: `x`/`y` are its origin from the run's start (pen plus
/// offset), px, y down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    pub glyph: GlyphRef,
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    /// Byte offset of the character it came from.
    pub cluster: u32,
}

/// A shaped run, in visual order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
}

/// A font's lines at a size, px: `underline` and `strikeout` are
/// `(offset below the baseline, thickness)` (strikeout's offset is
/// negative: above).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub underline: (f32, f32),
    pub strikeout: (f32, f32),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Family {
    Named(String),
    Generic(Generic),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Generic {
    Serif,
    SansSerif,
    Monospace,
    Cursive,
    Fantasy,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Selector {
    families: Vec<Family>,
    weight: u16,
    style: FontStyle,
}

/// A selector resolved: its faces, best first, with the synthesis each
/// needs — and what it asked for, for fallback faces.
struct Candidates {
    faces: Vec<(Arc<Face>, u8)>,
    weight: u16,
    style: FontStyle,
}

/// FNV-1a: the shaping cache hashes a word per lookup; SipHash's
/// DoS-resistance buys nothing for a bounded, per-page cache.
#[derive(Default)]
pub(crate) struct FnvHasher(u64);

impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        // FNV leaves the top bits of short keys nearly constant; the hash
        // table indexes by them. A splitmix64 finalizer spreads every bit.
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf2_9ce4_8422_2325 } else { self.0 };
        for b in bytes {
            h = (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }
}

pub(crate) type Fnv = std::hash::BuildHasherDefault<FnvHasher>;

/// One shard of the shaping cache: per spec key, the words shaped.
type ShapedShard = HashMap<(u32, u32, bool), HashMap<Box<str>, Arc<ShapedRun>, Fnv>, Fnv>;

/// A face that can be chosen: loaded, or in the catalog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Loaded(FaceId),
    Catalog(usize),
}

pub struct Font {
    /// Unique in the process: what caches outside the font key on (a font
    /// set freed and another made at the same address are not confused).
    id: u64,
    /// Loaded faces by id. Ids are fixed, not given in load order — the
    /// bundled face is 0, catalog entry `i` is `1 + i`, web fonts follow —
    /// so which thread loads a font first changes nothing drawn.
    faces: RwLock<std::collections::BTreeMap<FaceId, Arc<Face>>>,
    /// Web fonts added so far (their ids follow the catalog's).
    web_fonts: std::sync::atomic::AtomicU16,
    source: Option<Arc<dyn FontSource>>,
    catalog: Vec<CatalogEntry>,
    /// Catalog entry → its face once loaded (`None`: it failed to load).
    loaded: RwLock<HashMap<usize, Option<FaceId>>>,
    /// Lower-case family → catalog entries.
    by_family: HashMap<String, Vec<usize>>,
    selectors: RwLock<(Vec<Selector>, HashMap<Selector, u32>)>,
    /// Per selector: its faces in preference order, with synthesis.
    candidates: RwLock<HashMap<u32, Arc<Candidates>>>,
    /// Per (character, bold, oblique): the fallback face, if any.
    fallback: RwLock<HashMap<(char, bool, bool), Option<FaceId>>>,
    shaped: Vec<Mutex<ShapedShard>>,
    plans: Mutex<HashMap<(FaceId, bool, u32), Arc<rustybuzz::ShapePlan>>>,
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}
fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Font {
    fn with_faces(faces: Vec<Face>, source: Option<Arc<dyn FontSource>>) -> Font {
        let mut catalog = source.as_ref().map(|s| s.catalog()).unwrap_or_default();
        // Room for the web fonts' ids after the catalog's.
        catalog.truncate(usize::from(u16::MAX) - 1 - crate::font::MAX_WEB_FACES);
        let mut by_family: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in catalog.iter().enumerate() {
            for f in &e.families {
                by_family.entry(f.clone()).or_default().push(i);
            }
        }
        let default = Selector { families: Vec::new(), weight: 400, style: FontStyle::Normal };
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Font {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            faces: RwLock::new(faces.into_iter().map(|f| (f.id, Arc::new(f))).collect()),
            web_fonts: std::sync::atomic::AtomicU16::new(0),
            source,
            catalog,
            loaded: RwLock::new(HashMap::new()),
            by_family,
            selectors: RwLock::new((vec![default.clone()], HashMap::from([(default, 0)]))),
            candidates: RwLock::new(HashMap::new()),
            fallback: RwLock::new(HashMap::new()),
            shaped: (0..SHARDS).map(|_| Mutex::new(HashMap::default())).collect(),
            plans: Mutex::new(HashMap::new()),
        }
    }

    /// The bundled font alone.
    pub fn dejavu_sans() -> Font {
        let face = Face::from_static(FaceId(0), DEJAVU_SANS_BYTES).expect("the bundled DejaVu Sans is well-formed");
        Font::with_faces(vec![face], None)
    }

    /// The bundled font, then the host's fonts.
    pub fn with_source(source: Arc<dyn FontSource>) -> Font {
        let face = Face::from_static(FaceId(0), DEJAVU_SANS_BYTES).expect("the bundled DejaVu Sans is well-formed");
        Font::with_faces(vec![face], Some(source))
    }

    /// One font from bytes (untrusted: validated; its bytes are not kept
    /// for the process).
    pub fn from_bytes(bytes: &[u8]) -> Result<Font, String> {
        let face = Face::load(FaceId(0), bytes.to_vec(), 0, false).map_err(|e| e.to_string())?;
        Ok(Font::with_faces(vec![face], None))
    }

    /// Adds a web font (`@font-face`) under `family`. Returns its face.
    pub fn add_web_font(&self, family: &str, bytes: Vec<u8>, weight: u16, style: FontStyle) -> Result<FaceId, FontError> {
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            format!("web:{:016x}:{}", h.finish(), bytes.len())
        };
        let mut faces = write(&self.faces);
        let k = self.web_fonts.load(std::sync::atomic::Ordering::Relaxed);
        if faces.len() >= MAX_FACES || usize::from(k) >= MAX_WEB_FACES {
            return Err(FontError::TooLarge(faces.len()));
        }
        let id = FaceId(1 + self.catalog.len() as u16 + k);
        let mut face = Face::load_shared(id, &key, 0, true, || Some(bytes))?;
        // Content names a web font by its `@font-face` family, not the
        // font's own names; its descriptors decide weight and style.
        face.info = FaceInfo { families: vec![family.trim().to_lowercase()], weight, style };
        faces.insert(id, Arc::new(face));
        self.web_fonts.store(k + 1, std::sync::atomic::Ordering::Relaxed);
        drop(faces);
        // Matching may change: forget what was resolved.
        write(&self.candidates).clear();
        write(&self.fallback).clear();
        for shard in &self.shaped {
            lock(shard).clear();
        }
        Ok(id)
    }

    fn face(&self, id: FaceId) -> Arc<Face> {
        let faces = read(&self.faces);
        Arc::clone(faces.get(&id).or_else(|| faces.get(&FaceId(0))).expect("the bundled face is always loaded"))
    }

    /// The selector for a CSS `font-family` list (names as written, generic
    /// keywords included), weight (1–1000) and style.
    pub fn selector(&self, families: &[&str], weight: u16, style: FontStyle) -> FontSelector {
        let families: Vec<Family> = families
            .iter()
            .take(16)
            .map(|f| {
                let f = f.trim().trim_matches(|c| c == '"' || c == '\'').to_lowercase();
                match f.as_str() {
                    "serif" => Family::Generic(Generic::Serif),
                    "sans-serif" | "system-ui" | "ui-sans-serif" | "-apple-system" | "blinkmacsystemfont" => Family::Generic(Generic::SansSerif),
                    "monospace" | "ui-monospace" => Family::Generic(Generic::Monospace),
                    "cursive" => Family::Generic(Generic::Cursive),
                    "fantasy" => Family::Generic(Generic::Fantasy),
                    _ => Family::Named(f),
                }
            })
            .collect();
        let sel = Selector { families, weight: weight.clamp(1, 1000), style };
        if let Some(id) = read(&self.selectors).1.get(&sel) {
            return FontSelector(*id);
        }
        let mut s = write(&self.selectors);
        if let Some(id) = s.1.get(&sel) {
            return FontSelector(*id);
        }
        if s.0.len() >= MAX_SELECTORS {
            diagnostics::guard(diagnostics::Guard::FontSizeClamped, "text: too many distinct fonts; using the default");
            return FontSelector(0);
        }
        let id = s.0.len() as u32;
        s.0.push(sel.clone());
        s.1.insert(sel, id);
        FontSelector(id)
    }

    fn selector_of(&self, s: FontSelector) -> Selector {
        let sels = read(&self.selectors);
        sels.0.get(s.0 as usize).unwrap_or(&sels.0[0]).clone()
    }

    fn info_of(&self, c: Choice) -> FaceInfo {
        match c {
            Choice::Loaded(id) => self.face(id).info.clone(),
            Choice::Catalog(i) => {
                let e = &self.catalog[i];
                FaceInfo { families: e.families.clone(), weight: e.weight, style: e.style }
            }
        }
    }

    /// Faces (loaded or not) of `family`.
    fn choices_for(&self, family: &str) -> Vec<Choice> {
        let mut out: Vec<Choice> = read(&self.faces).values().filter(|f| f.info.families.iter().any(|n| n == family)).map(|f| Choice::Loaded(f.id)).collect();
        {
            // Never hold one of these locks while taking another (`load_entry`
            // takes `faces`, then `loaded`).
            let loaded = read(&self.loaded);
            for &i in self.by_family.get(family).map(Vec::as_slice).unwrap_or(&[]) {
                match loaded.get(&i) {
                    Some(Some(id)) if out.contains(&Choice::Loaded(*id)) => {}
                    Some(Some(id)) => out.push(Choice::Loaded(*id)),
                    Some(None) => {}
                    None => out.push(Choice::Catalog(i)),
                }
            }
        }
        // A catalog face with the weight and style of a loaded one (the
        // bundled face, and the same font on the system) is not needed.
        // Loaded faces alike are all kept: a web font split into
        // `unicode-range` subsets is several faces of one family.
        let loaded_styles: Vec<(u16, FontStyle)> =
            out.iter().filter(|c| matches!(c, Choice::Loaded(_))).map(|c| self.info_of(*c)).map(|i| (i.weight, i.style)).collect();
        out.retain(|c| match c {
            Choice::Loaded(_) => true,
            Choice::Catalog(_) => {
                let i = self.info_of(*c);
                !loaded_styles.contains(&(i.weight, i.style))
            }
        });
        out
    }

    fn load_choice(&self, c: Choice) -> Option<FaceId> {
        match c {
            Choice::Loaded(id) => Some(id),
            Choice::Catalog(i) => self.load_entry(i),
        }
    }

    fn load_entry(&self, i: usize) -> Option<FaceId> {
        if let Some(r) = read(&self.loaded).get(&i) {
            return *r;
        }
        let entry = &self.catalog[i];
        let source = self.source.as_ref()?;
        let bytes = if crate::face::is_shared(&entry.key) { Some(Vec::new()) } else { source.load(entry) };
        let mut faces = write(&self.faces);
        let mut loaded = write(&self.loaded);
        if let Some(r) = loaded.get(&i) {
            return *r;
        }
        let result = match bytes {
            Some(b) if faces.len() < MAX_FACES => {
                let id = FaceId(1 + i as u16);
                match Face::load_shared(id, &entry.key, entry.index, false, || Some(b)) {
                    Ok(mut face) => {
                        if face.info.families.is_empty() {
                            face.info.families = entry.families.clone();
                        }
                        faces.insert(id, Arc::new(face));
                        Some(id)
                    }
                    Err(_) => None,
                }
            }
            _ => None,
        };
        loaded.insert(i, result);
        result
    }

    /// The faces a selector names, best first, with the synthesis each needs;
    /// the bundled face last.
    fn candidates(&self, s: FontSelector) -> Arc<Candidates> {
        if let Some(c) = read(&self.candidates).get(&s.0) {
            return Arc::clone(c);
        }
        let sel = self.selector_of(s);
        let mut out: Vec<(FaceId, u8)> = Vec::new();
        let mut families: Vec<String> = Vec::new();
        for f in &sel.families {
            match f {
                Family::Named(n) => families.push(n.clone()),
                Family::Generic(g) => families.extend(generic_names(*g).iter().map(|s| s.to_string())),
            }
        }
        families.extend(generic_names(Generic::SansSerif).iter().map(|s| s.to_string()));
        for family in families {
            let choices: Vec<(Choice, FaceInfo)> = self.choices_for(&family).into_iter().map(|c| (c, self.info_of(c))).collect();
            if let Some(best) = best_match(&choices, sel.weight, sel.style) {
                let best_info = self.info_of(best);
                // The best face, and loaded faces just like it (subsets).
                let alike = choices.iter().filter(|(c, i)| *c == best || matches!(c, Choice::Loaded(_)) && i.weight == best_info.weight && i.style == best_info.style);
                for (c, _) in alike {
                    if let Some(id) = self.load_choice(*c) {
                        let synth = synthesis(&self.face(id).info, sel.weight, sel.style);
                        if !out.iter().any(|(f, _)| *f == id) {
                            out.push((id, synth));
                        }
                    }
                }
            }
        }
        if !out.iter().any(|(f, _)| *f == FaceId(0)) {
            let synth = synthesis(&self.face(FaceId(0)).info, sel.weight, sel.style);
            out.push((FaceId(0), synth));
        }
        let out = Arc::new(Candidates { faces: out.into_iter().map(|(id, synth)| (self.face(id), synth)).collect(), weight: sel.weight, style: sel.style });
        write(&self.candidates).insert(s.0, Arc::clone(&out));
        out
    }

    /// The face to draw `ch` with, for a selector's candidates.
    fn face_for(&self, ch: char, cands: &Candidates) -> (FaceId, u8) {
        for (face, synth) in &cands.faces {
            if face.covers(ch) {
                return (face.id, *synth);
            }
        }
        let bold = cands.weight >= 600;
        let oblique = cands.style != FontStyle::Normal;
        let cached = read(&self.fallback).get(&(ch, bold, oblique)).copied();
        let found = match cached {
            Some(f) => f,
            None => {
                let f = self.search_fallback(ch);
                let mut fb = write(&self.fallback);
                if fb.len() > 65_536 {
                    fb.clear();
                }
                fb.insert((ch, bold, oblique), f);
                f
            }
        };
        match found {
            Some(id) => (id, synthesis(&self.face(id).info, cands.weight, cands.style)),
            None => (cands.faces[0].0.id, cands.faces[0].1),
        }
    }

    /// A face with a glyph for `ch`, found the same way whatever happens to
    /// be loaded already (layout runs in parallel: load order varies): the
    /// bundled face, then the first few catalog fonts covering the
    /// character's script (loaded as needed), then the web fonts in order.
    fn search_fallback(&self, ch: char) -> Option<FaceId> {
        if self.face(FaceId(0)).covers(ch) {
            return Some(FaceId(0));
        }
        let langs = script_langs(ch);
        if !langs.is_empty() {
            let candidates = self.catalog.iter().enumerate().filter(|(_, e)| e.style == FontStyle::Normal && e.langs.iter().any(|l| langs.contains(&l.as_str())));
            for (i, _) in candidates.take(FALLBACK_LOADS) {
                if let Some(id) = self.load_entry(i) {
                    if self.face(id).covers(ch) {
                        return Some(id);
                    }
                }
            }
        }
        let web: Vec<Arc<Face>> = read(&self.faces).range(FaceId(1 + self.catalog.len() as u16)..).map(|(_, f)| Arc::clone(f)).collect();
        web.iter().find(|f| f.covers(ch)).map(|f| f.id)
    }

    /// Shapes `text` (one line's worth: a word) — cached.
    pub fn shape(&self, text: &str, spec: &TextSpec) -> Arc<ShapedRun> {
        // A word the fast path takes costs less to shape than to cache.
        if !spec.rtl && text.is_ascii() {
            let cands = self.candidates(spec.selector);
            if let Some(run) = shape_simple(text, spec, sanitize_px(spec.size), &cands) {
                return Arc::new(run);
            }
        }
        let key = spec.key();
        let shard = &self.shaped[shard_of(text, key) % SHARDS];
        if let Some(run) = lock(shard).get(&key).and_then(|m| m.get(text)) {
            return Arc::clone(run);
        }
        let run = Arc::new(self.shape_uncached(text, spec));
        let mut m = lock(shard);
        if m.values().map(HashMap::len).sum::<usize>() >= SHAPED_PER_SHARD {
            m.clear();
        }
        m.entry(key).or_default().insert(text.into(), Arc::clone(&run));
        run
    }

    fn shape_uncached(&self, text: &str, spec: &TextSpec) -> ShapedRun {
        let size = sanitize_px(spec.size);
        let cands = self.candidates(spec.selector);
        if let Some(run) = shape_simple(text, spec, size, &cands) {
            return run;
        }
        self.shape_with_shaper(text, spec, size, &cands)
    }

    #[cfg(test)]
    fn shape_uncached_slow(&self, text: &str, spec: &TextSpec) -> ShapedRun {
        let size = sanitize_px(spec.size);
        let cands = self.candidates(spec.selector);
        self.shape_with_shaper(text, spec, size, &cands)
    }

    fn shape_with_shaper(&self, text: &str, spec: &TextSpec, size: f32, cands: &Candidates) -> ShapedRun {
        // Split into runs of one face; a combining mark stays with its base.
        let mut segs: Vec<(usize, usize, FaceId, u8)> = Vec::new();
        for (i, ch) in text.char_indices() {
            let end = i + ch.len_utf8();
            if let Some(last) = segs.last_mut() {
                if is_continuation(ch) {
                    last.1 = end;
                    continue;
                }
            }
            let (face, synth) = self.face_for(ch, cands);
            match segs.last_mut() {
                Some(last) if (last.2, last.3) == (face, synth) => last.1 = end,
                _ => segs.push((i, end, face, synth)),
            }
        }
        let primary = Arc::clone(&cands.faces[0].0);
        let scale0 = size / primary.metrics.units_per_em;
        let mut run = ShapedRun {
            ascent: primary.metrics.ascender * scale0,
            descent: -primary.metrics.descender * scale0,
            line_gap: primary.metrics.line_gap * scale0,
            ..ShapedRun::default()
        };
        if spec.rtl {
            segs.reverse();
        }
        let mut pen = 0.0f32;
        for (start, end, face_id, synth) in segs {
            let face = self.face(face_id);
            let scale = size / face.metrics.units_per_em;
            run.ascent = run.ascent.max(face.metrics.ascender * scale);
            run.descent = run.descent.max(-face.metrics.descender * scale);
            let embolden = if synth & SYNTHETIC_BOLD != 0 { embolden_px(size) } else { 0.0 };
            let plan_cache = &self.plans;
            let _ = face.with(|rb| {
                let mut buf = rustybuzz::UnicodeBuffer::new();
                buf.push_str(&text[start..end]);
                buf.set_direction(if spec.rtl { rustybuzz::Direction::RightToLeft } else { rustybuzz::Direction::LeftToRight });
                buf.guess_segment_properties();
                let script = buf.script();
                let plan_key = (face_id, spec.rtl, u32::from_be_bytes(script.tag().to_bytes()));
                let plan = {
                    let mut plans = lock(plan_cache);
                    if plans.len() > 4096 {
                        plans.clear();
                    }
                    Arc::clone(plans.entry(plan_key).or_insert_with(|| Arc::new(rustybuzz::ShapePlan::new(rb, buf.direction(), Some(script), None, &[]))))
                };
                let out = rustybuzz::shape_with_plan(rb, &plan, buf);
                for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
                    let advance = pos.x_advance as f32 * scale + embolden;
                    run.glyphs.push(ShapedGlyph {
                        glyph: GlyphRef { face: face_id, id: info.glyph_id as u16, synthetic: synth },
                        x: pen + pos.x_offset as f32 * scale,
                        y: -(pos.y_offset as f32) * scale,
                        advance,
                        cluster: info.cluster + start as u32,
                    });
                    pen += advance;
                }
            });
        }
        run.width = pen;
        run
    }

    /// The advance width of `text` shaped per `spec` — what layout measures
    /// a word with. A word the fast path takes is summed without building a
    /// run.
    pub fn width(&self, text: &str, spec: &TextSpec) -> f32 {
        if !spec.rtl && text.is_ascii() && !text.is_empty() {
            let cands = self.candidates(spec.selector);
            let (face, synth) = &cands.faces[0];
            if text.bytes().all(|b| face.simple_ascii & 1 << b != 0) {
                let size = sanitize_px(spec.size);
                let scale = size / face.metrics.units_per_em;
                let embolden = if synth & SYNTHETIC_BOLD != 0 { embolden_px(size) } else { 0.0 };
                return text.bytes().map(|b| face.ascii_advance(b).1 * scale + embolden).sum();
            }
        }
        self.shape(text, spec).width
    }

    /// Width and line metrics of `text` in the default font at `px`.
    pub fn measure(&self, text: &str, px: f32) -> TextMetrics {
        self.measure_styled(text, &TextSpec::plain(px))
    }

    /// Width and line metrics of `text` shaped per `spec`.
    pub fn measure_styled(&self, text: &str, spec: &TextSpec) -> TextMetrics {
        let run = self.shape(text, spec);
        let lm = self.line_metrics(spec);
        TextMetrics { width: run.width, line_height: lm.ascent + lm.descent + lm.line_gap, ascent: lm.ascent }
    }

    /// The primary face's lines at the spec's size.
    pub fn line_metrics(&self, spec: &TextSpec) -> LineMetrics {
        let size = sanitize_px(spec.size);
        let m = self.candidates(spec.selector).faces[0].0.metrics;
        let s = size / m.units_per_em;
        LineMetrics {
            ascent: m.ascender * s,
            descent: -m.descender * s,
            line_gap: m.line_gap * s,
            underline: (-m.underline.0 * s, m.underline.1 * s),
            strikeout: (-m.strikeout.0 * s, m.strikeout.1 * s),
        }
    }

    /// Rasterizes one glyph at `px`.
    pub fn rasterize_glyph(&self, glyph: GlyphRef, px: f32) -> RasterizedGlyph {
        let size = sanitize_px(px);
        let face = self.face(glyph.face);
        let scale = size / face.metrics.units_per_em;
        let embolden = if glyph.synthetic & SYNTHETIC_BOLD != 0 { embolden_px(size) as usize } else { 0 };
        let oblique = glyph.synthetic & SYNTHETIC_OBLIQUE != 0;
        face.with(|rb| crate::raster::rasterize(rb, glyph.id, scale, oblique, embolden))
            .unwrap_or(RasterizedGlyph { width: 0, height: 0, bitmap: Vec::new(), xmin: 0, ymin: 0, advance_width: 0.0 })
    }

    /// A glyph's bitmap size, bearings and advance at `px` — about what
    /// [`Self::rasterize_glyph`] gives — without rasterizing it.
    pub fn glyph_extent(&self, glyph: GlyphRef, px: f32) -> RasterizedGlyph {
        let size = sanitize_px(px);
        let face = self.face(glyph.face);
        let s = size / face.metrics.units_per_em;
        let embolden = if glyph.synthetic & SYNTHETIC_BOLD != 0 { embolden_px(size) } else { 0.0 };
        face.with(|f| {
            let g = ttf_parser::GlyphId(glyph.id);
            let advance = f32::from(f.glyph_hor_advance(g).unwrap_or(0)) * s + embolden;
            match f.glyph_bounding_box(g) {
                Some(b) => {
                    let slant = if glyph.synthetic & SYNTHETIC_OBLIQUE != 0 { f32::from(b.y_max.max(0)) * s * 0.25 } else { 0.0 };
                    let (x0, x1) = ((f32::from(b.x_min) * s).floor(), (f32::from(b.x_max) * s + slant).ceil() + embolden);
                    let (y0, y1) = ((f32::from(b.y_min) * s).floor(), (f32::from(b.y_max) * s).ceil());
                    RasterizedGlyph {
                        width: (x1 - x0).clamp(0.0, 4096.0) as usize,
                        height: (y1 - y0).clamp(0.0, 4096.0) as usize,
                        bitmap: Vec::new(),
                        xmin: x0 as i32,
                        ymin: y0 as i32,
                        advance_width: advance,
                    }
                }
                None => RasterizedGlyph { width: 0, height: 0, bitmap: Vec::new(), xmin: 0, ymin: 0, advance_width: advance },
            }
        })
        .unwrap_or(RasterizedGlyph { width: 0, height: 0, bitmap: Vec::new(), xmin: 0, ymin: 0, advance_width: 0.0 })
    }

    /// `ch` in the default font: its extent (no bitmap).
    pub fn glyph_metrics(&self, ch: char, px: f32) -> RasterizedGlyph {
        self.glyph_extent(self.glyph_for(ch), px)
    }

    /// The default font's glyph for `ch`.
    pub fn glyph_for(&self, ch: char) -> GlyphRef {
        let face = self.face(FaceId(0));
        let id = face.with(|f| f.glyph_index(ch).map_or(0, |g| g.0)).unwrap_or(0);
        GlyphRef { face: FaceId(0), id, synthetic: 0 }
    }

    /// `ch` in the default font, rasterized.
    pub fn rasterize(&self, ch: char, px: f32) -> RasterizedGlyph {
        self.rasterize_glyph(self.glyph_for(ch), px)
    }

    /// This font set's process-unique id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Faces loaded (bundled, system, web).
    pub fn face_count(&self) -> usize {
        read(&self.faces).len()
    }

    /// Shaped words cached (tests).
    pub fn cached_runs(&self) -> usize {
        self.shaped.iter().map(|s| lock(s).values().map(HashMap::len).sum::<usize>()).sum()
    }
}

/// The fast path: an ASCII word whose every glyph, in the selector's first
/// face, is untouched by any lookup shapes to exactly its advances — no
/// shaper run. `None`: shape it properly.
fn shape_simple(text: &str, spec: &TextSpec, size: f32, cands: &Candidates) -> Option<ShapedRun> {
    let (face, synth) = &cands.faces[0];
    if spec.rtl || text.is_empty() || !text.bytes().all(|b| b < 128 && face.simple_ascii & 1 << b != 0) {
        return None;
    }
    let scale = size / face.metrics.units_per_em;
    let embolden = if synth & SYNTHETIC_BOLD != 0 { embolden_px(size) } else { 0.0 };
    let mut run = ShapedRun {
        ascent: face.metrics.ascender * scale,
        descent: -face.metrics.descender * scale,
        line_gap: face.metrics.line_gap * scale,
        glyphs: Vec::with_capacity(text.len()),
        width: 0.0,
    };
    let mut pen = 0.0f32;
    for (i, b) in text.bytes().enumerate() {
        let (id, advance) = face.ascii_advance(b);
        let advance = advance * scale + embolden;
        run.glyphs.push(ShapedGlyph { glyph: GlyphRef { face: face.id, id, synthetic: *synth }, x: pen, y: 0.0, advance, cluster: i as u32 });
        pen += advance;
    }
    run.width = pen;
    Some(run)
}

/// How much wider synthetic bold draws, px.
fn embolden_px(size: f32) -> f32 {
    (size / 24.0).round().max(1.0)
}

fn shard_of(text: &str, key: (u32, u32, bool)) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = FnvHasher::default();
    text.hash(&mut h);
    key.hash(&mut h);
    h.finish() as usize
}

/// A character that belongs to the cluster before it: combining marks,
/// joiners, variation selectors.
fn is_continuation(ch: char) -> bool {
    use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
    matches!(ch.general_category(), GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark | GeneralCategory::SpacingMark)
        || matches!(ch, '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Families a generic keyword prefers, in order.
fn generic_names(g: Generic) -> &'static [&'static str] {
    match g {
        Generic::SansSerif | Generic::Fantasy => &["dejavu sans", "noto sans", "liberation sans", "open sans", "arial", "helvetica"],
        Generic::Serif => &["dejavu serif", "noto serif", "liberation serif", "times new roman", "c059", "tinos"],
        Generic::Monospace => &["dejavu sans mono", "noto sans mono", "liberation mono", "courier new", "cousine"],
        Generic::Cursive => &["z003", "comic sans ms", "dejavu sans"],
    }
}

/// The synthesis a face needs to look like `weight`/`style`.
fn synthesis(info: &FaceInfo, weight: u16, style: FontStyle) -> u8 {
    let mut s = 0;
    if weight >= 600 && info.weight < 600 {
        s |= SYNTHETIC_BOLD;
    }
    if style != FontStyle::Normal && info.style == FontStyle::Normal {
        s |= SYNTHETIC_OBLIQUE;
    }
    s
}

/// CSS Fonts 4 §5.2 font matching, for style then weight.
fn best_match(choices: &[(Choice, FaceInfo)], weight: u16, style: FontStyle) -> Option<Choice> {
    if choices.is_empty() {
        return None;
    }
    let order: &[FontStyle] = match style {
        FontStyle::Italic => &[FontStyle::Italic, FontStyle::Oblique, FontStyle::Normal],
        FontStyle::Oblique => &[FontStyle::Oblique, FontStyle::Italic, FontStyle::Normal],
        FontStyle::Normal => &[FontStyle::Normal, FontStyle::Oblique, FontStyle::Italic],
    };
    let group: Vec<&(Choice, FaceInfo)> = order.iter().map(|s| choices.iter().filter(|(_, i)| i.style == *s).collect::<Vec<_>>()).find(|g| !g.is_empty())?;
    let w = i32::from(weight);
    // Rank: exact, then the direction the spec prefers, then the other.
    let rank = |fw: i32| -> (u8, i32) {
        if fw == w {
            return (0, 0);
        }
        if (400..=500).contains(&w) {
            if fw > w && fw <= 500 {
                (1, fw - w)
            } else if fw < w {
                (2, w - fw)
            } else {
                (3, fw - w)
            }
        } else if w < 400 {
            if fw < w { (1, w - fw) } else { (2, fw - w) }
        } else if fw > w {
            (1, fw - w)
        } else {
            (2, w - fw)
        }
    };
    group.into_iter().min_by_key(|(_, i)| rank(i32::from(i.weight))).map(|(c, _)| *c)
}

/// Language tags whose fonts may cover `ch`'s script (fallback hints).
fn script_langs(ch: char) -> &'static [&'static str] {
    use unicode_script::{Script, UnicodeScript};
    match ch.script() {
        Script::Arabic => &["ar", "fa", "ur"],
        Script::Hebrew => &["he", "yi"],
        Script::Devanagari => &["hi", "mr", "ne", "sa"],
        Script::Bengali => &["bn", "as"],
        Script::Gurmukhi => &["pa"],
        Script::Gujarati => &["gu"],
        Script::Oriya => &["or"],
        Script::Tamil => &["ta"],
        Script::Telugu => &["te"],
        Script::Kannada => &["kn"],
        Script::Malayalam => &["ml"],
        Script::Thai => &["th"],
        Script::Han => &["zh-cn", "zh-tw", "ja", "ko"],
        Script::Hiragana | Script::Katakana => &["ja"],
        Script::Hangul => &["ko"],
        Script::Greek => &["el"],
        Script::Cyrillic => &["ru", "uk", "bg"],
        Script::Armenian => &["hy"],
        Script::Georgian => &["ka"],
        Script::Ethiopic => &["am"],
        Script::Latin => &["en"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deja() -> Font {
        Font::dejavu_sans()
    }

    #[test]
    fn shaping_applies_kerning() {
        let f = deja();
        let spec = TextSpec::plain(40.0);
        let pair = f.shape("AV", &spec).width;
        let apart = f.shape("A", &spec).width + f.shape("V", &spec).width;
        assert!(pair < apart - 0.5, "kerned {pair} < {apart}");
    }

    /// The fast path gives exactly what the shaper gives, for every word it
    /// takes — and kerned pairs still go to the shaper.
    #[test]
    fn the_fast_path_equals_shaping() {
        let f = deja();
        let face = &f.candidates(FontSelector(0)).faces[0].0;
        let simple: String = (0u8..128).filter(|c| face.simple_ascii & 1 << c != 0).map(char::from).collect();
        eprintln!("simple ASCII in DejaVu Sans: {simple}");
        assert!(face.simple_ascii != 0, "DejaVu has glyphs no lookup touches");
        let words: Vec<String> = (0..2000).map(|i| format!("{i}")).chain(["the", "row", "Hello", "AV", "fi", "Te", "x/y", "wo", "()"].map(String::from)).collect();
        let mut fast = 0;
        for w in &words {
            let spec = TextSpec::plain(16.0);
            let cands = f.candidates(spec.selector);
            if let Some(run) = shape_simple(w, &spec, 16.0, &cands) {
                fast += 1;
                let mut shaped = f.shape_uncached_slow(w, &spec);
                shaped.glyphs.iter_mut().for_each(|g| g.x = (g.x * 1000.0).round() / 1000.0);
                let mut run = run;
                run.glyphs.iter_mut().for_each(|g| g.x = (g.x * 1000.0).round() / 1000.0);
                assert_eq!(run.glyphs, shaped.glyphs, "{w}");
            }
        }
        assert!(fast > 0, "some words take the fast path");
        let spec = TextSpec::plain(16.0);
        assert!(shape_simple("AV", &spec, 16.0, &f.candidates(spec.selector)).is_none(), "a kerned pair is shaped");
    }

    #[test]
    fn shaped_words_are_cached() {
        let f = deja();
        // "hello" has kerned letters: it goes to the shaper, and is cached.
        let a = f.shape("hello", &TextSpec::plain(16.0));
        let b = f.shape("hello", &TextSpec::plain(16.0));
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(f.cached_runs(), 1);
    }

    #[test]
    fn right_to_left_runs_come_out_in_visual_order() {
        let f = deja();
        // Hebrew "shalom": DejaVu covers Hebrew. Visual order is the reverse
        // of logical: the first glyph drawn is the last letter.
        let run = f.shape("שלום", &TextSpec { rtl: true, ..TextSpec::plain(20.0) });
        assert_eq!(run.glyphs.len(), 4);
        assert!(run.glyphs[0].cluster > run.glyphs[3].cluster, "{:?}", run.glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>());
        assert!(run.glyphs.windows(2).all(|w| w[0].x < w[1].x));
    }

    #[test]
    fn arabic_is_joined() {
        let f = deja();
        let spec = TextSpec { rtl: true, ..TextSpec::plain(20.0) };
        // Beh alone, and beh in the middle of a word: different glyph forms.
        let alone = f.shape("ب", &spec).glyphs[0].glyph.id;
        let medial = f.shape("ببب", &spec).glyphs[1].glyph.id;
        assert_ne!(alone, medial, "contextual forms were applied");
    }

    #[test]
    fn bold_and_italic_are_synthesized_without_the_faces() {
        let f = deja();
        let s = f.selector(&["sans-serif"], 700, FontStyle::Italic);
        let run = f.shape("a", &TextSpec { selector: s, ..TextSpec::plain(20.0) });
        assert_eq!(run.glyphs[0].glyph.synthetic, SYNTHETIC_BOLD | SYNTHETIC_OBLIQUE);
        let plain = f.shape("a", &TextSpec::plain(20.0)).width;
        assert!(run.width > plain, "synthetic bold is wider");
        let bitmap = f.rasterize_glyph(run.glyphs[0].glyph, 20.0);
        assert!(bitmap.width > 0 && bitmap.bitmap.iter().any(|&c| c > 200));
    }

    #[test]
    fn selectors_are_interned_and_bounded() {
        let f = deja();
        let a = f.selector(&["Foo", "serif"], 400, FontStyle::Normal);
        assert_eq!(a, f.selector(&["foo", "serif"], 400, FontStyle::Normal));
        for i in 0..MAX_SELECTORS + 10 {
            f.selector(&[&format!("f{i}")], 400, FontStyle::Normal);
        }
        assert_eq!(f.selector(&["one more"], 400, FontStyle::Normal), FontSelector(0));
    }

    #[test]
    fn css_weight_matching() {
        let c = |w: u16| (Choice::Catalog(w as usize), FaceInfo { families: vec![], weight: w, style: FontStyle::Normal });
        let all = [c(300), c(400), c(600), c(700)];
        let pick = |w| match best_match(&all, w, FontStyle::Normal) {
            Some(Choice::Catalog(x)) => x,
            _ => 0,
        };
        assert_eq!(pick(400), 400);
        assert_eq!(pick(500), 400, "500 falls back below first");
        assert_eq!(pick(800), 700);
        assert_eq!(pick(650), 700);
        assert_eq!(pick(200), 300);
    }

    struct Files(Vec<(CatalogEntry, Vec<u8>)>);
    impl FontSource for Files {
        fn catalog(&self) -> Vec<CatalogEntry> {
            self.0.iter().map(|(e, _)| e.clone()).collect()
        }
        fn load(&self, entry: &CatalogEntry) -> Option<Vec<u8>> {
            self.0.iter().find(|(e, _)| e.key == entry.key).map(|(_, b)| b.clone())
        }
    }

    #[test]
    fn a_named_family_from_the_source_is_used_and_loaded_only_when_needed() {
        let entry = CatalogEntry { key: "k".into(), index: 0, families: vec!["test sans".into()], weight: 400, style: FontStyle::Normal, langs: vec!["en".into()] };
        let f = Font::with_source(Arc::new(Files(vec![(entry, DEJAVU_SANS_BYTES.to_vec())])));
        assert_eq!(f.face_count(), 1, "nothing loaded up front");
        let s = f.selector(&["Test Sans"], 400, FontStyle::Normal);
        let run = f.shape("a", &TextSpec { selector: s, ..TextSpec::plain(16.0) });
        assert_eq!(f.face_count(), 2);
        assert_eq!(run.glyphs[0].glyph.face, FaceId(1));
        assert_eq!(run.glyphs[0].glyph.synthetic, 0, "the face has the weight asked for: no synthesis");
    }

}
