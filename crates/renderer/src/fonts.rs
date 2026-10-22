//! The system's fonts, as a browser grants them to its engine
//! (Milestone 35, ADR 009): a [`text::FontSource`]. Host code — the engine
//! never enumerates fonts or reads font files on its own.
//!
//! fontconfig (`fc-list`) is asked where it exists: it knows every font's
//! families, weight, slant and language coverage from its own cache. Where
//! it does not, the usual font directories are scanned
//! ([`text::DirectoryFonts`]).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use text::{CatalogEntry, DirectoryFonts, FontSource, FontStyle};

/// Language tags kept per font: those fallback asks about.
const USEFUL_LANGS: &[&str] = &[
    "en", "ar", "fa", "ur", "he", "yi", "hi", "mr", "ne", "sa", "bn", "as", "pa", "gu", "or", "ta", "te", "kn", "ml", "th", "zh-cn", "zh-tw", "ja", "ko", "el", "ru", "uk",
    "bg", "hy", "ka", "am",
];

/// The system's fonts: fontconfig's list, or a scan of the font directories.
pub fn system_fonts() -> Arc<dyn FontSource> {
    match fontconfig() {
        Some(fonts) if !fonts.entries.is_empty() => Arc::new(fonts),
        _ => Arc::new(DirectoryFonts::scan(&font_dirs())),
    }
}

struct FontconfigFonts {
    entries: Vec<CatalogEntry>,
}

impl FontSource for FontconfigFonts {
    fn catalog(&self) -> Vec<CatalogEntry> {
        self.entries.clone()
    }

    fn load(&self, entry: &CatalogEntry) -> Option<Vec<u8>> {
        // Only a file fontconfig listed.
        if !self.entries.iter().any(|e| e.key == entry.key) {
            return None;
        }
        let bytes = std::fs::read(&entry.key).ok()?;
        (bytes.len() <= text::MAX_FONT_BYTES).then_some(bytes)
    }
}

fn fontconfig() -> Option<FontconfigFonts> {
    let out = Command::new("fc-list").arg("--format").arg("%{file}\t%{index}\t%{family}\t%{weight}\t%{slant}\t%{lang}\n").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries: Vec<CatalogEntry> = text.lines().filter_map(parse_line).collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key).then(a.index.cmp(&b.index)));
    entries.dedup_by(|a, b| a.key == b.key && a.index == b.index);
    Some(FontconfigFonts { entries })
}

/// One line of `fc-list --format '%{file}\t%{index}\t%{family}\t%{weight}\t%{slant}\t%{lang}'`.
fn parse_line(line: &str) -> Option<CatalogEntry> {
    let mut f = line.split('\t');
    let (file, index, families, weight, slant, langs) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?, f.next().unwrap_or(""));
    let lower = file.to_ascii_lowercase();
    if !(lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".ttc")) {
        return None; // bitmap and Type 1 fonts: not drawable here
    }
    let families: Vec<String> = families.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).take(8).collect();
    let style = match slant.split_whitespace().next()?.parse::<u32>().ok()? {
        0 => FontStyle::Normal,
        110 => FontStyle::Oblique,
        _ => FontStyle::Italic,
    };
    let langs = langs.split('|').filter(|l| USEFUL_LANGS.contains(l)).map(str::to_string).collect();
    Some(CatalogEntry {
        key: file.to_string(),
        index: index.parse().ok()?,
        families,
        weight: css_weight(weight.split_whitespace().next()?.parse().ok()?),
        style,
        langs,
    })
}

/// fontconfig's weight scale to CSS's.
fn css_weight(fc: u32) -> u16 {
    match fc {
        0..=20 => 100,
        21..=45 => 200,
        46..=62 => 300,
        63..=90 => 400,
        91..=140 => 500,
        141..=190 => 600,
        191..=202 => 700,
        203..=207 => 800,
        _ => 900,
    }
}

/// Where fonts usually live.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/fonts"), PathBuf::from("/usr/local/share/fonts"), PathBuf::from("/System/Library/Fonts"), PathBuf::from("/Library/Fonts")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/fonts"));
        dirs.push(PathBuf::from(home).join("Library/Fonts"));
    }
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fontconfig_lines_become_catalog_entries() {
        let e = parse_line("/usr/share/fonts/a/DejaVuSans-Bold.ttf\t0\tDejaVu Sans,DejaVu Sans Bold\t200\t0\ten|ar|xx|he").unwrap();
        assert_eq!(e.families, ["dejavu sans", "dejavu sans bold"]);
        assert_eq!((e.weight, e.style), (700, FontStyle::Normal));
        assert_eq!(e.langs, ["en", "ar", "he"], "only the tags fallback asks about");
        assert!(parse_line("/x/font.pcf.gz\t0\tFixed\t80\t0\ten").is_none(), "bitmap fonts are skipped");
        assert_eq!(parse_line("/x/i.otf\t0\tX\t80\t100\t").unwrap().style, FontStyle::Italic);
    }

    #[test]
    fn a_source_reads_only_what_it_listed() {
        let fonts = FontconfigFonts { entries: vec![] };
        let other = CatalogEntry { key: "/etc/passwd".into(), index: 0, families: vec![], weight: 400, style: FontStyle::Normal, langs: vec![] };
        assert!(fonts.load(&other).is_none());
    }
}
