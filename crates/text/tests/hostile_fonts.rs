//! Milestone 35 acceptance: a hostile web font — a huge glyph count, glyphs
//! of absurd size, composite glyphs that include themselves or loop — is
//! loaded, shaped and drawn (or refused) within a bounded time and memory.
//! The fixtures were built with fontTools (`tests/hostile/`).

use std::time::{Duration, Instant};
use text::{Font, FontStyle, GlyphAtlas, TextSpec};

fn within(limit: Duration, what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| panic!("{what}: not done within {limit:?}"));
    eprintln!("{what}: {:?}", started.elapsed());
}

fn exercise(bytes: &'static [u8], text: &'static str) {
    let font = Font::dejavu_sans();
    let decoded = text::decode_web_font(bytes).expect("a font file");
    font.add_web_font("hostile", decoded, 400, FontStyle::Normal).expect("accepted: these fonts are valid, just hostile");
    let sel = font.selector(&["hostile"], 400, FontStyle::Normal);
    let mut atlas = GlyphAtlas::new(512, 128);
    for size in [12.0, 48.0, 1024.0] {
        let run = font.shape(text, &TextSpec { selector: sel, ..TextSpec::plain(size) });
        for g in run.glyphs.iter().take(3) {
            let bitmap = font.rasterize_glyph(g.glyph, size);
            assert!(bitmap.width <= 1024 + 64 && bitmap.height <= 1024, "bitmaps are capped");
        }
        // As the engine draws: the atlas checks a glyph's extent before
        // rasterizing it, and stops rasterizing once it is full.
        atlas.begin_frame();
        for g in &run.glyphs {
            let _ = atlas.get_or_insert_glyph(&font, g.glyph, size);
        }
        assert!(atlas.height() <= text::MAX_ATLAS_HEIGHT);
    }
}

#[test]
fn composite_glyphs_that_include_themselves_are_bounded() {
    within(Duration::from_secs(5), "recursive composites", || {
        exercise(include_bytes!("hostile/recursive-composites.ttf"), "ABCdefghijklmnopqrstuvwxyz");
    });
}

#[test]
fn a_font_with_65000_giant_glyphs_is_bounded() {
    within(Duration::from_secs(5), "65,000 glyphs, every 47th 30 em square", || {
        let text: &'static str = Box::leak((0..400).map(|i| char::from_u32(0x4E00 + i * 47).unwrap()).chain("abcxyz".chars()).collect::<String>().into_boxed_str());
        exercise(include_bytes!("hostile/huge-glyph-count.ttf"), text);
    });
}
