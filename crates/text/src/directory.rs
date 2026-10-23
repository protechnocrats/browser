//! A [`FontSource`] over font files in directories (Milestone 35) — what
//! tests and `conformance` use (a fixed set: goldens must not depend on the
//! machine's fonts), and what a host without fontconfig scans. Hosts grant
//! it; the engine never constructs one on its own.

use crate::face::FontStyle;
use crate::font::{CatalogEntry, FontSource};
use std::path::{Path, PathBuf};

/// Font files scanned at most (a directory tree of thousands costs bounded
/// start-up time).
pub const MAX_SCANNED_FILES: usize = 2048;
/// Directory depth scanned at most.
const MAX_DEPTH: usize = 4;

/// One sample character per script, and the language tags a font covering
/// it serves (`FontSource` fallback hints).
const SCRIPT_SAMPLES: &[(char, &[&str])] = &[
    ('a', &["en"]),
    ('ب', &["ar", "fa", "ur"]),
    ('ש', &["he", "yi"]),
    ('क', &["hi", "mr", "ne", "sa"]),
    ('ক', &["bn", "as"]),
    ('ਕ', &["pa"]),
    ('ક', &["gu"]),
    ('க', &["ta"]),
    ('క', &["te"]),
    ('ಕ', &["kn"]),
    ('ക', &["ml"]),
    ('ก', &["th"]),
    ('中', &["zh-cn", "zh-tw"]),
    ('あ', &["ja"]),
    ('한', &["ko"]),
    ('α', &["el"]),
    ('д', &["ru", "uk", "bg"]),
    ('ა', &["ka"]),
    ('ա', &["hy"]),
];

pub struct DirectoryFonts {
    entries: Vec<CatalogEntry>,
}

impl DirectoryFonts {
    /// Scans `dirs` for `.ttf`/`.otf`/`.ttc` files.
    pub fn scan(dirs: &[PathBuf]) -> DirectoryFonts {
        let mut files = Vec::new();
        for d in dirs {
            collect(d, 0, &mut files);
        }
        files.sort();
        let mut entries = Vec::new();
        for path in files.into_iter().take(MAX_SCANNED_FILES) {
            let Ok(bytes) = std::fs::read(&path) else { continue };
            if bytes.len() > crate::face::MAX_FONT_BYTES || !crate::face::collection_header_fits(&bytes) {
                continue;
            }
            let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1).min(16);
            for index in 0..count {
                if let Some(e) = describe(&bytes, index, &path) {
                    entries.push(e);
                }
            }
        }
        DirectoryFonts { entries }
    }
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || out.len() >= MAX_SCANNED_FILES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, depth + 1, out);
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc")) {
            out.push(path);
        }
    }
}

fn describe(bytes: &[u8], index: u32, path: &Path) -> Option<CatalogEntry> {
    let face = ttf_parser::Face::parse(bytes, index).ok()?;
    let mut families: Vec<String> = Vec::new();
    for name in face.names() {
        if name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY || name.name_id == ttf_parser::name_id::FAMILY {
            if let Some(s) = name.to_string() {
                let s = s.trim().to_lowercase();
                if !s.is_empty() && !families.contains(&s) {
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
    let mut langs = Vec::new();
    for (ch, tags) in SCRIPT_SAMPLES {
        if face.glyph_index(*ch).is_some_and(|g| g.0 != 0) {
            langs.extend(tags.iter().map(|t| t.to_string()));
        }
    }
    Some(CatalogEntry { key: path.to_string_lossy().into_owned(), index, families, weight: face.weight().to_number(), style, langs })
}

impl FontSource for DirectoryFonts {
    fn catalog(&self) -> Vec<CatalogEntry> {
        self.entries.clone()
    }

    fn load(&self, entry: &CatalogEntry) -> Option<Vec<u8>> {
        // Only a file this source listed.
        if !self.entries.iter().any(|e| e.key == entry.key) {
            return None;
        }
        std::fs::read(&entry.key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Font, FontStyle, TextSpec};
    use std::sync::Arc;

    fn test_fonts() -> Font {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fonts"));
        Font::with_source(Arc::new(DirectoryFonts::scan(&[dir])))
    }

    #[test]
    fn the_test_fonts_are_catalogued_with_their_scripts() {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fonts"));
        let fonts = DirectoryFonts::scan(&[dir]);
        let cat = fonts.catalog();
        assert!(cat.len() >= 10, "{}", cat.len());
        let lohit = cat.iter().find(|e| e.families.iter().any(|f| f.contains("lohit"))).expect("Lohit");
        assert!(lohit.langs.contains(&"hi".to_string()));
        let noto = cat.iter().find(|e| e.families.iter().any(|f| f.contains("noto"))).expect("Noto");
        assert!(noto.langs.contains(&"ja".to_string()) && noto.langs.contains(&"zh-cn".to_string()));
        assert!(cat.iter().any(|e| e.weight >= 700 && e.families.contains(&"dejavu sans".to_string())));
    }

    #[test]
    fn real_bold_and_fallback_come_from_the_directory() {
        let font = test_fonts();
        let bold = font.selector(&["dejavu sans"], 700, FontStyle::Normal);
        let run = font.shape("a", &TextSpec { selector: bold, ..TextSpec::plain(16.0) });
        assert_ne!(run.glyphs[0].glyph.face.0, 0, "the real bold face, not the bundled regular");
        assert_eq!(run.glyphs[0].glyph.synthetic, 0);
        // Devanagari and CJK: not in DejaVu; found by fallback.
        for text in ["नमस्ते", "世界"] {
            let run = font.shape(text, &TextSpec::plain(16.0));
            assert!(run.glyphs.iter().all(|g| g.glyph.id != 0), "{text}: no missing glyphs");
            assert!(run.glyphs.iter().all(|g| g.glyph.face.0 != 0), "{text}: from a fallback face");
        }
        // Devanagari shaping: the vowel sign ि is written after क but
        // drawn before it.
        let ka = font.shape("क", &TextSpec::plain(16.0)).glyphs[0].glyph;
        let run = font.shape("कि", &TextSpec::plain(16.0));
        assert_eq!(run.glyphs.len(), 2);
        assert_ne!(run.glyphs[0].glyph, ka, "the vowel sign comes first");
        assert_eq!(run.glyphs[1].glyph, ka);
    }

    /// Regression: matching took two locks in the opposite order to loading
    /// a face, and pages running layout in parallel deadlocked (the corpus
    /// timed out). Many threads matching, loading and falling back at once
    /// must all finish.
    #[test]
    fn concurrent_matching_loading_and_fallback_do_not_deadlock() {
        let font = Arc::new(test_fonts());
        let (tx, rx) = std::sync::mpsc::channel();
        for t in 0..8 {
            let (font, tx) = (Arc::clone(&font), tx.clone());
            std::thread::spawn(move || {
                for i in 0..40 {
                    let families: [&str; 2] = [["dejavu serif", "dejavu sans mono", "dejavu sans"][(i + t) % 3], "sans-serif"];
                    let sel = font.selector(&families, [400, 700][(i / 3 + t) % 2], [FontStyle::Normal, FontStyle::Italic][i % 2]);
                    let _ = font.shape(&format!("a{i} नम 世界 ש"), &TextSpec { selector: sel, ..TextSpec::plain(12.0) });
                }
                let _ = tx.send(());
            });
        }
        for _ in 0..8 {
            rx.recv_timeout(std::time::Duration::from_secs(60)).expect("a thread deadlocked");
        }
    }
}
