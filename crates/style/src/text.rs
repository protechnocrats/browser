//! Milestone 35: the text properties — `font-family`, `font-weight`,
//! `font-style`, the `font` shorthand, `line-height`, `text-decoration`,
//! `text-transform`, `direction` and `unicode-bidi`.
//!
//! All of them inherit (text decorations propagate to descendants' text,
//! CSS Text Decoration 3 §2), so they live together in one `Arc` that an
//! element shares with its parent until it declares one of them.

use crate::length::{parse_length_unchecked, LengthContext};
use crate::Color;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FontStyleKind {
    #[default]
    Normal,
    Italic,
    Oblique,
}

/// Computed `line-height`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum LineHeight {
    /// The font's own line spacing.
    #[default]
    Normal,
    /// A multiple of the element's font size (inherited as the number).
    Number(f32),
    /// Pixels.
    Px(f32),
}

impl LineHeight {
    /// The used line height for `font_size`, or `None` for `normal`.
    pub fn resolve(self, font_size: f32) -> Option<f32> {
        match self {
            LineHeight::Normal => None,
            LineHeight::Number(n) => Some(n * font_size),
            LineHeight::Px(px) => Some(px),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextTransform {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

impl TextTransform {
    /// `text` transformed (`capitalize`: the first letter of each word;
    /// `at_word_start`: whether `text` starts a word).
    pub fn apply(self, text: &str, at_word_start: bool) -> String {
        match self {
            TextTransform::None => text.to_string(),
            TextTransform::Uppercase => text.to_uppercase(),
            TextTransform::Lowercase => text.to_lowercase(),
            TextTransform::Capitalize => {
                let mut out = String::with_capacity(text.len());
                let mut start = at_word_start;
                for c in text.chars() {
                    if start && c.is_alphabetic() {
                        out.extend(c.to_uppercase());
                        start = false;
                    } else {
                        out.push(c);
                        start = start && !c.is_alphanumeric() || c.is_whitespace();
                    }
                }
                out
            }
        }
    }
}

/// `unicode-bidi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnicodeBidi {
    #[default]
    Normal,
    Embed,
    Isolate,
    BidiOverride,
    IsolateOverride,
    Plaintext,
}

pub const UNDERLINE: u8 = 1;
pub const OVERLINE: u8 = 2;
pub const LINE_THROUGH: u8 = 4;

/// Text decoration lines in effect (the element's own and every
/// ancestor's), and their colour.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Decoration {
    /// [`UNDERLINE`] | [`OVERLINE`] | [`LINE_THROUGH`].
    pub lines: u8,
    /// `None`: the text's own colour.
    pub color: Option<Color>,
}

/// Families a `font-family` list may name, at most (a hostile list costs
/// bounded memory).
pub const MAX_FAMILIES: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// Lower-case names and generic keywords, in order. Empty: the default.
    pub font_family: Arc<[Box<str>]>,
    /// 1–1000.
    pub font_weight: u16,
    pub font_style: FontStyleKind,
    pub line_height: LineHeight,
    pub text_transform: TextTransform,
    pub decoration: Decoration,
    /// The lines inherited from ancestors (an element's own
    /// `text-decoration` adds to them; it cannot remove them).
    pub inherited_lines: u8,
    pub rtl: bool,
    pub unicode_bidi: UnicodeBidi,
}

impl Default for TextStyle {
    fn default() -> Self {
        TextStyle {
            font_family: Arc::from(Vec::new()),
            font_weight: 400,
            font_style: FontStyleKind::Normal,
            line_height: LineHeight::Normal,
            text_transform: TextTransform::None,
            decoration: Decoration::default(),
            inherited_lines: 0,
            rtl: false,
            unicode_bidi: UnicodeBidi::Normal,
        }
    }
}

/// The initial text style, shared (every `Style::default()` would otherwise
/// allocate one).
pub fn default_text() -> Arc<TextStyle> {
    static DEFAULT: std::sync::OnceLock<Arc<TextStyle>> = std::sync::OnceLock::new();
    Arc::clone(DEFAULT.get_or_init(|| Arc::new(TextStyle::default())))
}

/// Parses a `font-family` list.
pub fn parse_font_family(value: &str) -> Option<Arc<[Box<str>]>> {
    let names: Vec<Box<str>> = value
        .split(',')
        .map(|n| n.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_lowercase())
        .filter(|n| !n.is_empty() && n.len() <= 256)
        .take(MAX_FAMILIES)
        .map(String::into_boxed_str)
        .collect();
    (!names.is_empty()).then(|| Arc::from(names))
}

/// Parses `font-weight` against the parent's weight.
pub fn parse_font_weight(value: &str, parent: u16) -> Option<u16> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(400),
        "bold" => Some(700),
        // CSS Fonts 4 §2.2's relative-weight table.
        "bolder" => Some(match parent {
            0..=349 => 400,
            350..=549 => 700,
            _ => 900,
        }),
        "lighter" => Some(match parent {
            0..=549 => 100,
            550..=749 => 400,
            _ => 700,
        }),
        v => v.parse::<f32>().ok().filter(|w| w.is_finite() && (1.0..=1000.0).contains(w)).map(|w| w.round() as u16),
    }
}

pub fn parse_font_style(value: &str) -> Option<FontStyleKind> {
    let v = value.trim().to_ascii_lowercase();
    match v.split_whitespace().next()? {
        "normal" => Some(FontStyleKind::Normal),
        "italic" => Some(FontStyleKind::Italic),
        "oblique" => Some(FontStyleKind::Oblique),
        _ => None,
    }
}

/// Parses `line-height`; percentages and lengths become pixels against the
/// element's `font_size`.
pub fn parse_line_height(value: &str, font_size: f32, ctx: &LengthContext) -> Option<LineHeight> {
    let v = value.trim().to_ascii_lowercase();
    if v == "normal" {
        return Some(LineHeight::Normal);
    }
    if let Ok(n) = v.parse::<f32>() {
        return (n.is_finite() && n >= 0.0).then_some(LineHeight::Number(n.min(1000.0)));
    }
    let ctx = LengthContext { font_size, ..*ctx };
    let px = parse_length_unchecked(&v, &ctx)?.resolve(font_size);
    (px.is_finite() && px >= 0.0).then_some(LineHeight::Px(px.min(100_000.0)))
}

/// Parses `text-decoration` / `text-decoration-line`: the lines, and a
/// colour if one is given.
pub fn parse_text_decoration(value: &str, current: Color) -> Option<(u8, Option<Color>)> {
    let mut lines = 0u8;
    let mut color = None;
    let mut none = false;
    for part in crate::declarations::split_values(value) {
        match part.to_ascii_lowercase().as_str() {
            "none" => none = true,
            "underline" => lines |= UNDERLINE,
            "overline" => lines |= OVERLINE,
            "line-through" => lines |= LINE_THROUGH,
            "solid" | "double" | "dotted" | "dashed" | "wavy" | "auto" | "from-font" => {}
            p => {
                if let Some(c) = crate::parse_color(p, current) {
                    color = Some(c);
                } else if parse_length_unchecked(p, &LengthContext::DEFAULT).is_none() {
                    return None;
                }
            }
        }
    }
    (none || lines != 0 || color.is_some()).then_some((lines, color))
}

/// The `font` shorthand: `[style] [variant] [weight] [stretch] size[/line-height] family`.
/// Returns (style, weight, size value, line-height value, family list).
/// The parts of a `font` shorthand: style, weight, size, line height, family.
pub type FontShorthand<'a> = (FontStyleKind, Option<&'a str>, &'a str, Option<&'a str>, &'a str);

pub fn parse_font_shorthand(value: &str) -> Option<FontShorthand<'_>> {
    let value = value.trim();
    // System fonts (`caption`, `menu`, …) are not modelled.
    let mut style = FontStyleKind::Normal;
    let mut weight = None;
    let mut rest = value;
    loop {
        let rest_trim = rest.trim_start();
        let word_end = rest_trim.find(char::is_whitespace).unwrap_or(rest_trim.len());
        let word = &rest_trim[..word_end];
        let lower = word.to_ascii_lowercase();
        match lower.as_str() {
            "normal" | "small-caps" | "condensed" | "expanded" | "semi-condensed" | "semi-expanded" => {}
            "italic" => style = FontStyleKind::Italic,
            "oblique" => style = FontStyleKind::Oblique,
            "bold" | "bolder" | "lighter" => weight = Some(word),
            w if w.parse::<f32>().is_ok_and(|n| (1.0..=1000.0).contains(&n)) && !rest_trim[word_end..].trim().is_empty() => weight = Some(word),
            _ => break,
        }
        rest = &rest_trim[word_end..];
    }
    let rest = rest.trim_start();
    let size_end = rest.find(char::is_whitespace)?;
    let (size_part, family) = (&rest[..size_end], rest[size_end..].trim());
    // `12px/1.5` or `12px / 1.5`.
    let (size, line_height, family) = match size_part.split_once('/') {
        Some((s, lh)) if !lh.is_empty() => (s, Some(lh), family),
        Some((s, _)) => {
            let fam = family.trim_start();
            let lh_end = fam.find(char::is_whitespace)?;
            (s, Some(&fam[..lh_end]), fam[lh_end..].trim())
        }
        None => match family.strip_prefix('/') {
            Some(after) => {
                let after = after.trim_start();
                let lh_end = after.find(char::is_whitespace)?;
                (size_part, Some(&after[..lh_end]), after[lh_end..].trim())
            }
            None => (size_part, None, family),
        },
    };
    if family.is_empty() {
        return None;
    }
    Some((style, weight, size, line_height, family))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_shorthand() {
        let (style, weight, size, lh, family) = parse_font_shorthand("italic bold 12px/1.5 \"Helvetica Neue\", sans-serif").unwrap();
        assert_eq!((style, weight, size, lh, family), (FontStyleKind::Italic, Some("bold"), "12px", Some("1.5"), "\"Helvetica Neue\", sans-serif"));
        let (_, w, size, lh, family) = parse_font_shorthand("600 1em / 20px Arial").unwrap();
        assert_eq!((w, size, lh, family), (Some("600"), "1em", Some("20px"), "Arial"));
        assert!(parse_font_shorthand("caption").is_none());
    }

    #[test]
    fn weights_and_decorations() {
        assert_eq!(parse_font_weight("bolder", 400), Some(700));
        assert_eq!(parse_font_weight("lighter", 700), Some(400));
        assert_eq!(parse_font_weight("1001", 400), None);
        let black = Color { r: 0, g: 0, b: 0, a: 255 };
        assert_eq!(parse_text_decoration("underline dotted red", black).map(|d| d.0), Some(UNDERLINE));
        assert_eq!(parse_text_decoration("none", black), Some((0, None)));
        assert_eq!(parse_text_decoration("bogus", black), None);
    }

    #[test]
    fn transforms_and_line_heights() {
        assert_eq!(TextTransform::Capitalize.apply("hello wide-world", true), "Hello Wide-world");
        assert_eq!(TextTransform::Uppercase.apply("straße", true), "STRASSE");
        let ctx = LengthContext::DEFAULT;
        assert_eq!(parse_line_height("150%", 20.0, &ctx), Some(LineHeight::Px(30.0)));
        assert_eq!(parse_line_height("1.5", 20.0, &ctx), Some(LineHeight::Number(1.5)));
        assert_eq!(LineHeight::Number(1.5).resolve(10.0), Some(15.0));
    }
}
