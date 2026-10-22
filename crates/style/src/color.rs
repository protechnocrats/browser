//! Milestone 33: CSS colour values — hex (3/4/6/8 digits), `rgb()`/`rgba()`
//! and `hsl()`/`hsla()` in both the comma and the space-separated syntax
//! (with `/ alpha`), every CSS named colour, `transparent` and
//! `currentcolor`.
//!
//! Zero-trust: the input is untrusted CSS. Parsing is bounded (a value
//! longer than [`MAX_COLOR_INPUT`] is rejected without being scanned),
//! every channel is clamped into range, and a non-finite channel makes the
//! value invalid.

use crate::values::Color;

/// Longest value string parsed as a colour (bytes).
pub const MAX_COLOR_INPUT: usize = 256;

/// Parses one colour. `current` is what `currentcolor` means for the
/// property being set (the element's `color`; for `color` itself, the
/// inherited one).
pub fn parse_color(value: &str, current: Color) -> Option<Color> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_COLOR_INPUT {
        return None;
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = value.to_ascii_lowercase();
    if let Some(open) = lower.find('(') {
        let args = lower[open + 1..].strip_suffix(')')?;
        return match &lower[..open] {
            "rgb" | "rgba" => parse_rgb(args),
            "hsl" | "hsla" => parse_hsl(args),
            _ => None,
        };
    }
    match lower.as_str() {
        "transparent" => Some(Color::TRANSPARENT),
        "currentcolor" => Some(current),
        name => NAMED
            .binary_search_by(|(n, _)| n.cmp(&name))
            .ok()
            .map(|i| {
                let rgb = NAMED[i].1;
                Color { r: (rgb >> 16) as u8, g: (rgb >> 8) as u8, b: rgb as u8, a: 255 }
            }),
    }
}

fn parse_hex(hex: &str) -> Option<Color> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok();
    let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    match hex.len() {
        3 | 4 => {
            let d = |i| digit(i).map(|v| v * 17);
            Some(Color { r: d(0)?, g: d(1)?, b: d(2)?, a: if hex.len() == 4 { d(3)? } else { 255 } })
        }
        6 | 8 => Some(Color { r: pair(0)?, g: pair(2)?, b: pair(4)?, a: if hex.len() == 8 { pair(6)? } else { 255 } }),
        _ => None,
    }
}

/// Splits function arguments: `a, b, c[, d]` or `a b c[ / d]`. Returns the
/// three components and the optional alpha.
fn components(args: &str) -> Option<([&str; 3], Option<&str>)> {
    let (main, alpha) = if args.contains(',') {
        let parts: Vec<&str> = args.split(',').map(str::trim).collect();
        match parts.as_slice() {
            [a, b, c] => return Some(([a, b, c], None)),
            [a, b, c, d] => return Some(([a, b, c], Some(d))),
            _ => return None,
        }
    } else {
        match args.split_once('/') {
            Some((main, alpha)) => (main, Some(alpha.trim())),
            None => (args, None),
        }
    };
    let parts: Vec<&str> = main.split_whitespace().collect();
    match parts.as_slice() {
        [a, b, c] => Some(([a, b, c], alpha)),
        _ => None,
    }
}

fn number(s: &str) -> Option<f32> {
    s.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// An alpha value: a number (0..1) or a percentage.
fn alpha(s: Option<&str>) -> Option<u8> {
    let Some(s) = s else { return Some(255) };
    let a = match s.strip_suffix('%') {
        Some(p) => number(p)? / 100.0,
        None => number(s)?,
    };
    Some((a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

fn parse_rgb(args: &str) -> Option<Color> {
    let ([r, g, b], a) = components(args)?;
    let channel = |s: &str| -> Option<u8> {
        let v = match s.strip_suffix('%') {
            Some(p) => number(p)? * 2.55,
            None if s == "none" => 0.0,
            None => number(s)?,
        };
        Some(v.clamp(0.0, 255.0).round() as u8)
    };
    Some(Color { r: channel(r)?, g: channel(g)?, b: channel(b)?, a: alpha(a)? })
}

fn parse_hsl(args: &str) -> Option<Color> {
    let ([h, s, l], a) = components(args)?;
    let hue = h.strip_suffix("deg").unwrap_or(h);
    let hue = number(hue)?.rem_euclid(360.0) / 360.0;
    let percent = |v: &str| number(v.strip_suffix('%').unwrap_or(v)).map(|p| (p / 100.0).clamp(0.0, 1.0));
    let (s, l) = (percent(s)?, percent(l)?);
    // CSS Color 4 §7.1's hsl → rgb.
    let f = |n: f32| {
        let k = (n + hue * 12.0) % 12.0;
        let a = s * l.min(1.0 - l);
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    let to_u8 = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Some(Color { r: to_u8(f(0.0)), g: to_u8(f(8.0)), b: to_u8(f(4.0)), a: alpha(a)? })
}

/// Every CSS named colour (CSS Color 4 §6.1), sorted for binary search.
const NAMED: [(&str, u32); 148] = [
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };

    fn rgba(r: u8, g: u8, b: u8, a: u8) -> Option<Color> {
        Some(Color { r, g, b, a })
    }

    #[test]
    fn the_named_table_is_sorted_and_complete() {
        assert!(NAMED.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(parse_color("RebeccaPurple", BLACK), rgba(0x66, 0x33, 0x99, 255));
        assert_eq!(parse_color("aliceblue", BLACK), rgba(0xf0, 0xf8, 0xff, 255));
        assert_eq!(parse_color("yellowgreen", BLACK), rgba(0x9a, 0xcd, 0x32, 255));
        assert_eq!(parse_color("notacolor", BLACK), None);
    }

    #[test]
    fn hex_forms() {
        assert_eq!(parse_color("#fff", BLACK), rgba(255, 255, 255, 255));
        assert_eq!(parse_color("#1234", BLACK), rgba(0x11, 0x22, 0x33, 0x44));
        assert_eq!(parse_color("#A0b1C2", BLACK), rgba(0xa0, 0xb1, 0xc2, 255));
        assert_eq!(parse_color("#00000080", BLACK), rgba(0, 0, 0, 0x80));
        for bad in ["#", "#12", "#12345", "#ggg", "#1234567", "#é€"] {
            assert_eq!(parse_color(bad, BLACK), None, "{bad}");
        }
    }

    #[test]
    fn functional_forms() {
        assert_eq!(parse_color("rgb(1, 2, 3)", BLACK), rgba(1, 2, 3, 255));
        assert_eq!(parse_color("rgba(1,2,3,0.5)", BLACK), rgba(1, 2, 3, 128));
        assert_eq!(parse_color("rgb(100% 0% 50% / 25%)", BLACK), rgba(255, 0, 128, 64));
        assert_eq!(parse_color("RGB(300, -5, 1e3)", BLACK), rgba(255, 0, 255, 255), "channels clamp");
        assert_eq!(parse_color("hsl(0, 100%, 50%)", BLACK), rgba(255, 0, 0, 255));
        assert_eq!(parse_color("hsl(120deg 100% 25%)", BLACK), rgba(0, 128, 0, 255));
        assert_eq!(parse_color("hsla(240, 100%, 50%, .5)", BLACK), rgba(0, 0, 255, 128));
        assert_eq!(parse_color("hsl(-120, 100%, 50%)", BLACK), rgba(0, 0, 255, 255), "hue wraps");
        for bad in ["rgb(1, 2)", "rgb(1 2 3 4)", "rgb(1, 2, 3", "rgb(a, b, c)", "rgb(nan, 1, 1)", "rgb(inf, 1, 1)", "lab(1 2 3)", "rgb()"] {
            assert_eq!(parse_color(bad, BLACK), None, "{bad}");
        }
    }

    #[test]
    fn keywords() {
        let current = Color { r: 9, g: 8, b: 7, a: 255 };
        assert_eq!(parse_color("transparent", BLACK), Some(Color::TRANSPARENT));
        assert_eq!(parse_color("currentColor", current), Some(current));
    }

    #[test]
    fn hostile_input_is_bounded() {
        assert_eq!(parse_color(&format!("rgb({})", "1,".repeat(100_000)), BLACK), None);
        assert_eq!(parse_color(&"#f".repeat(MAX_COLOR_INPUT), BLACK), None);
    }
}
