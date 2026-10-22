//! Web fonts (Milestone 35, ADR 009): the bytes an `@font-face` fetched,
//! checked and turned into a font file before any parser sees them.
//!
//! Zero-trust: the input is sniffed (never trusted by its `format()` hint or
//! content type), its size is bounded, a WOFF/WOFF2 container's declared
//! output size is checked *before* decompression (a small file claiming a
//! huge font is refused without inflating it), and the result is validated
//! as a font ([`crate::Font::add_web_font`]) with the same caps as any.

use crate::face::FontError;

/// The largest web font fetched, compressed.
pub const MAX_WEB_FONT_INPUT: usize = 8 << 20;
/// The largest web font once decompressed.
pub const MAX_WEB_FONT_BYTES: usize = 24 << 20;

/// What a web font's bytes are, by their first four bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFontFormat {
    /// TrueType or OpenType (`\0\1\0\0`, `true`, `OTTO`).
    Sfnt,
    Woff,
    Woff2,
}

pub fn sniff(bytes: &[u8]) -> Option<WebFontFormat> {
    match bytes.get(..4)? {
        [0, 1, 0, 0] | b"true" | b"OTTO" => Some(WebFontFormat::Sfnt),
        b"wOFF" => Some(WebFontFormat::Woff),
        b"wOF2" => Some(WebFontFormat::Woff2),
        _ => None,
    }
}

/// A web font's bytes as a font file (`TooLarge`, `Malformed`: refused).
pub fn decode_web_font(bytes: &[u8]) -> Result<Vec<u8>, FontError> {
    if bytes.len() > MAX_WEB_FONT_INPUT {
        return Err(FontError::TooLarge(bytes.len()));
    }
    let format = sniff(bytes).ok_or_else(|| FontError::Malformed("not a font (unrecognised signature)".into()))?;
    let out = match format {
        WebFontFormat::Sfnt => bytes.to_vec(),
        WebFontFormat::Woff | WebFontFormat::Woff2 => {
            // Both headers declare the decompressed size at offset 16.
            let declared = bytes.get(16..20).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or_else(|| FontError::Malformed("truncated header".into()))?;
            if declared > MAX_WEB_FONT_BYTES {
                return Err(FontError::TooLarge(declared));
            }
            let decoded = if format == WebFontFormat::Woff { wuff::decompress_woff1(bytes) } else { wuff::decompress_woff2(bytes) };
            decoded.map_err(|e| FontError::Malformed(format!("{e:?}")))?
        }
    };
    if out.len() > MAX_WEB_FONT_BYTES {
        return Err(FontError::TooLarge(out.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEJAVU: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

    #[test]
    fn formats_are_sniffed_not_trusted() {
        assert_eq!(sniff(DEJAVU), Some(WebFontFormat::Sfnt));
        assert_eq!(sniff(b"wOF2...."), Some(WebFontFormat::Woff2));
        assert!(decode_web_font(b"<html>not a font</html>").is_err());
        assert_eq!(decode_web_font(DEJAVU).unwrap().len(), DEJAVU.len());
    }

    #[test]
    fn a_container_claiming_a_huge_font_is_refused_before_inflating() {
        let mut woff = b"wOFF\0\x01\0\0".to_vec();
        woff.extend_from_slice(&[0, 0, 0, 44, 0, 1, 0, 0]);
        woff.extend_from_slice(&(u32::MAX).to_be_bytes()); // totalSfntSize
        woff.resize(44, 0);
        assert!(matches!(decode_web_font(&woff), Err(FontError::TooLarge(_))));
        assert!(matches!(decode_web_font(&vec![0u8; MAX_WEB_FONT_INPUT + 1]), Err(FontError::TooLarge(_))));
    }

    #[test]
    fn corrupt_containers_are_errors_not_panics() {
        for n in 0..64 {
            let mut bytes = b"wOF2".to_vec();
            bytes.extend((0..n * 7).map(|i| (i * 31 % 251) as u8));
            let _ = decode_web_font(&bytes);
            bytes[..4].copy_from_slice(b"wOFF");
            let _ = decode_web_font(&bytes);
        }
    }

    #[test]
    fn woff_and_woff2_decode_to_the_font_inside() {
        for bytes in [
            &include_bytes!("../../../fuzz/corpus/web_font/04_dejavu_serif_bold.woff")[..],
            &include_bytes!("../../../fuzz/corpus/web_font/05_dejavu_serif_bold.woff2")[..],
        ] {
            let font = decode_web_font(bytes).expect("decodes");
            assert_eq!(sniff(&font), Some(WebFontFormat::Sfnt));
            let set = crate::Font::dejavu_sans();
            set.add_web_font("brand", font, 700, crate::FontStyle::Normal).expect("a valid font");
            let sel = set.selector(&["brand"], 700, crate::FontStyle::Normal);
            let run = set.shape("Brand", &crate::TextSpec { selector: sel, ..crate::TextSpec::plain(20.0) });
            assert!(run.glyphs.iter().all(|g| g.glyph.face.0 == 1 && g.glyph.id != 0), "drawn with the web font");
        }
    }
}
