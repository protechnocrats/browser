//! Milestone 33: CSS lengths — every absolute, font-relative and
//! viewport-relative unit, percentages, and `calc()` / `min()` / `max()` /
//! `clamp()`.
//!
//! Everything except a percentage is resolved to pixels here, at cascade
//! time, from a [`LengthContext`] (the element's font size, the root font
//! size and the viewport). A percentage depends on the containing block,
//! which only layout knows, so it is kept: a [`Length`] is `px + percent%`.
//! That is exactly what `calc()` produces — a `calc()` expression over
//! lengths is always a linear combination of pixels and percentages — so
//! `calc(100% - 2em)` is represented without loss.
//!
//! `min()` / `max()` / `clamp()` are resolved when their arguments are
//! all free of percentages. With a percentage argument the result depends
//! on layout and is not representable as `px + percent`; the declaration
//! is then invalid and ignored, as CSS does for any value an engine cannot
//! parse (graceful degradation, counted by the conformance report).
//!
//! Zero-trust: the input is untrusted CSS. Parsing is a recursive descent
//! bounded in nesting depth ([`MAX_CALC_DEPTH`]) and input length
//! ([`MAX_LENGTH_INPUT`]); a result that is not finite is rejected.

use crate::values::Length;

/// What non-percentage units resolve against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LengthContext {
    /// The element's font size (`em`); for `font-size` itself, the
    /// parent's.
    pub font_size: f32,
    /// The root element's font size (`rem`).
    pub root_font_size: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
}

impl LengthContext {
    /// A 16px font on a 1920×1080 viewport — for resolving outside a
    /// cascade (tests, keyframes with no known viewport).
    pub const DEFAULT: LengthContext =
        LengthContext { font_size: 16.0, root_font_size: 16.0, viewport_width: 1920.0, viewport_height: 1080.0 };
}

/// The viewport, for `vw`/`vh`/`vmin`/`vmax`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSize {
    pub width: f32,
    pub height: f32,
}

impl ViewportSize {
    pub const DEFAULT: ViewportSize = ViewportSize { width: 1920.0, height: 1080.0 };

    pub fn new(width: f32, height: f32) -> ViewportSize {
        ViewportSize { width, height }
    }
}

impl Default for ViewportSize {
    fn default() -> Self {
        ViewportSize::DEFAULT
    }
}

/// Nesting depth of parentheses/functions accepted inside one value.
pub const MAX_CALC_DEPTH: usize = 32;
/// Longest value string parsed as a length (bytes).
pub const MAX_LENGTH_INPUT: usize = 4096;

/// Parses one length-percentage value: a dimension, a percentage, a
/// unitless `0`, or a math function. `None` for anything else.
pub fn parse_length(value: &str, ctx: &LengthContext) -> Option<Length> {
    parse_length_unchecked(value, ctx).filter(|l| l.px.is_finite() && l.percent.is_finite())
}

/// [`parse_length`] without the finiteness check — for a caller that
/// reports a non-finite value itself (`font-size`'s guard).
pub(crate) fn parse_length_unchecked(value: &str, ctx: &LengthContext) -> Option<Length> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_LENGTH_INPUT {
        return None;
    }
    let mut parser = Parser { src: value.as_bytes(), pos: 0, ctx, depth: 0 };
    let result = parser.term_value()?;
    parser.skip_space();
    if parser.pos != parser.src.len() {
        return None;
    }
    match result {
        Value::Length(l) => Some(l),
        // A bare `0` is a length; any other bare number is not.
        Value::Number(n) => (n == 0.0).then_some(Length::ZERO),
    }
}

/// The value of a (sub)expression: a plain number, or a length.
#[derive(Debug, Clone, Copy)]
enum Value {
    Number(f32),
    Length(Length),
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    ctx: &'a LengthContext,
    depth: usize,
}

impl Parser<'_> {
    fn skip_space(&mut self) {
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    /// A single top-level value: a dimension or a math function. (A bare
    /// arithmetic expression is only valid inside `calc()`.)
    fn term_value(&mut self) -> Option<Value> {
        self.skip_space();
        match self.peek()? {
            b'0'..=b'9' | b'.' | b'+' | b'-' => self.dimension(),
            _ => self.function(),
        }
    }

    /// `calc(...)`, `min(...)`, `max(...)`, `clamp(...)` (ASCII
    /// case-insensitive names), or a nested `( ... )` inside `calc`.
    fn function(&mut self) -> Option<Value> {
        let start = self.pos;
        while self.pos < self.src.len() && (self.src[self.pos].is_ascii_alphabetic() || self.src[self.pos] == b'-') {
            self.pos += 1;
        }
        let name = std::str::from_utf8(&self.src[start..self.pos]).ok()?.to_ascii_lowercase();
        if self.peek()? != b'(' {
            return None;
        }
        self.pos += 1;
        self.depth += 1;
        if self.depth > MAX_CALC_DEPTH {
            return None;
        }
        let result = match name.as_str() {
            "" | "calc" => self.sum(),
            "min" | "max" | "clamp" => {
                let mut args = vec![self.sum()?];
                loop {
                    self.skip_space();
                    if self.peek()? != b',' {
                        break;
                    }
                    self.pos += 1;
                    args.push(self.sum()?);
                    if args.len() > 64 {
                        return None;
                    }
                }
                math_function(&name, &args)
            }
            _ => None,
        }?;
        self.skip_space();
        if self.peek()? != b')' {
            return None;
        }
        self.pos += 1;
        self.depth -= 1;
        Some(result)
    }

    /// `product (('+' | '-') product)*`. CSS requires whitespace around
    /// `+`/`-` in `calc()`, which is what keeps `1px -2px` apart from
    /// `1px - 2px`; the sign of a number is handled by `dimension`.
    fn sum(&mut self) -> Option<Value> {
        let mut acc = self.product()?;
        loop {
            let before = self.pos;
            self.skip_space();
            let op = match self.peek() {
                Some(b'+') => 1.0,
                Some(b'-') => -1.0,
                _ => {
                    self.pos = before;
                    return Some(acc);
                }
            };
            let had_space_before = self.pos > before;
            self.pos += 1;
            let had_space_after = self.peek().is_some_and(|c| c.is_ascii_whitespace());
            if !had_space_before || !had_space_after {
                return None;
            }
            let rhs = self.product()?;
            acc = add(acc, rhs, op)?;
        }
    }

    /// `unary (('*' | '/') unary)*`.
    fn product(&mut self) -> Option<Value> {
        let mut acc = self.unary()?;
        loop {
            let before = self.pos;
            self.skip_space();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    let rhs = self.unary()?;
                    acc = multiply(acc, rhs)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    let Value::Number(divisor) = self.unary()? else { return None };
                    if divisor == 0.0 {
                        return None;
                    }
                    acc = multiply(acc, Value::Number(1.0 / divisor))?;
                }
                _ => {
                    self.pos = before;
                    return Some(acc);
                }
            }
        }
    }

    fn unary(&mut self) -> Option<Value> {
        self.skip_space();
        match self.peek()? {
            b'0'..=b'9' | b'.' | b'+' | b'-' => self.dimension(),
            _ => self.function(),
        }
    }

    /// A number with an optional unit or `%`.
    fn dimension(&mut self) -> Option<Value> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.')) {
            self.pos += 1;
        }
        // Exponent (`1e3px`), only when followed by digits — `1em` is
        // a unit, not an exponent.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mut look = self.pos + 1;
            if matches!(self.src.get(look), Some(b'+' | b'-')) {
                look += 1;
            }
            if matches!(self.src.get(look), Some(b'0'..=b'9')) {
                self.pos = look;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
        }
        if self.pos == digits_start {
            return None;
        }
        let number: f32 = std::str::from_utf8(&self.src[start..self.pos]).ok()?.parse().ok()?;
        let unit_start = self.pos;
        if self.peek() == Some(b'%') {
            self.pos += 1;
            return Some(Value::Length(Length::percent(number)));
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        if self.pos == unit_start {
            return Some(Value::Number(number));
        }
        let unit = std::str::from_utf8(&self.src[unit_start..self.pos]).ok()?.to_ascii_lowercase();
        let px = number * unit_in_px(&unit, self.ctx)?;
        Some(Value::Length(Length::px(px)))
    }
}

/// How many pixels one `unit` is. `None` for an unknown unit.
fn unit_in_px(unit: &str, ctx: &LengthContext) -> Option<f32> {
    Some(match unit {
        "px" => 1.0,
        "em" => ctx.font_size,
        "rem" => ctx.root_font_size,
        // No font metrics for `ex`/`ch` at cascade time: the common
        // approximation (half an em), as engines use when metrics are
        // unavailable.
        "ex" | "ch" => ctx.font_size * 0.5,
        "vw" => ctx.viewport_width / 100.0,
        "vh" => ctx.viewport_height / 100.0,
        "vmin" => ctx.viewport_width.min(ctx.viewport_height) / 100.0,
        "vmax" => ctx.viewport_width.max(ctx.viewport_height) / 100.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "q" => 96.0 / 101.6,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        _ => return None,
    })
}

fn add(a: Value, b: Value, sign: f32) -> Option<Value> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => Some(Value::Number(x + sign * y)),
        (Value::Length(x), Value::Length(y)) => {
            Some(Value::Length(Length { px: x.px + sign * y.px, percent: x.percent + sign * y.percent }))
        }
        // A zero number mixed with lengths is accepted, as engines do
        // for `calc(0 + 10px)` in practice.
        (Value::Length(x), Value::Number(n)) | (Value::Number(n), Value::Length(x)) if n == 0.0 => {
            Some(Value::Length(x))
        }
        _ => None,
    }
}

fn multiply(a: Value, b: Value) -> Option<Value> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => Some(Value::Number(x * y)),
        (Value::Length(l), Value::Number(n)) | (Value::Number(n), Value::Length(l)) => {
            Some(Value::Length(Length { px: l.px * n, percent: l.percent * n }))
        }
        // length × length is not a length.
        (Value::Length(_), Value::Length(_)) => None,
    }
}

/// `min`/`max`/`clamp` over arguments with no percentage part.
fn math_function(name: &str, args: &[Value]) -> Option<Value> {
    let as_px = |v: &Value| match v {
        Value::Length(l) if l.percent == 0.0 => Some(l.px),
        Value::Number(n) if *n == 0.0 => Some(0.0),
        _ => None,
    };
    // All plain numbers: a number result (`calc(2 * min(1, 3))`).
    if args.iter().all(|v| matches!(v, Value::Number(_))) {
        let nums: Vec<f32> = args.iter().map(|v| if let Value::Number(n) = v { *n } else { 0.0 }).collect();
        return pick(name, &nums).map(Value::Number);
    }
    let px: Vec<f32> = args.iter().map(as_px).collect::<Option<_>>()?;
    pick(name, &px).map(|v| Value::Length(Length::px(v)))
}

fn pick(name: &str, v: &[f32]) -> Option<f32> {
    match name {
        "min" => v.iter().copied().reduce(f32::min),
        "max" => v.iter().copied().reduce(f32::max),
        // clamp(MIN, VAL, MAX) = max(MIN, min(VAL, MAX)).
        "clamp" if v.len() == 3 => Some(v[0].max(v[1].min(v[2]))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> LengthContext {
        LengthContext { font_size: 20.0, root_font_size: 10.0, viewport_width: 1000.0, viewport_height: 500.0 }
    }

    fn px(s: &str) -> Option<f32> {
        parse_length(s, &ctx()).filter(|l| l.percent == 0.0).map(|l| l.px)
    }

    fn close(a: Option<f32>, b: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-3)
    }

    #[test]
    fn absolute_units() {
        assert!(close(px("12px"), 12.0));
        assert!(close(px("1in"), 96.0));
        assert!(close(px("72pt"), 96.0));
        assert!(close(px("2.54cm"), 96.0));
        assert!(close(px("25.4mm"), 96.0));
        assert!(close(px("1pc"), 16.0));
        assert!(close(px("1E1PX"), 10.0), "units and exponents are case-insensitive");
        assert!(close(px("-4px"), -4.0));
        assert!(close(px(".5px"), 0.5));
    }

    #[test]
    fn relative_units_resolve_against_the_context() {
        assert!(close(px("1.5em"), 30.0));
        assert!(close(px("2rem"), 20.0));
        assert!(close(px("10vw"), 100.0));
        assert!(close(px("10vh"), 50.0));
        assert!(close(px("10vmin"), 50.0));
        assert!(close(px("10vmax"), 100.0));
        assert!(close(px("1e1em"), 200.0), "an exponent is not confused with the `em` unit");
    }

    #[test]
    fn percent_and_zero() {
        assert_eq!(parse_length("50%", &ctx()), Some(Length::percent(50.0)));
        assert_eq!(parse_length("0", &ctx()), Some(Length::ZERO));
        assert_eq!(parse_length("5", &ctx()), None, "only zero may omit its unit");
    }

    #[test]
    fn calc_is_linear_in_px_and_percent() {
        assert_eq!(parse_length("calc(100% - 2em)", &ctx()), Some(Length { px: -40.0, percent: 100.0 }));
        assert_eq!(parse_length("calc( (10px + 5%) * 2 )", &ctx()), Some(Length { px: 20.0, percent: 10.0 }));
        assert_eq!(parse_length("calc(100%/3)", &ctx()).map(|l| (l.percent * 3.0).round()), Some(100.0));
        assert_eq!(parse_length("CALC(1px + 1px)", &ctx()), Some(Length::px(2.0)));
        assert_eq!(parse_length("calc(2 * 3px)", &ctx()), Some(Length::px(6.0)));
    }

    #[test]
    fn invalid_calc_is_rejected() {
        for bad in [
            "calc(1px+2px)",   // `+` needs whitespace
            "calc(1px * 2px)", // length × length
            "calc(1px / 0)",
            "calc(1px / 2px)",
            "calc(1px",
            "calc()",
            "calc(1px) junk",
            "foo(1px)",
            "calc(1px + 2)", // length + non-zero number
            "10qux",
            "",
            "px",
            "-",
        ] {
            assert_eq!(parse_length(bad, &ctx()), None, "{bad:?}");
        }
    }

    #[test]
    fn min_max_clamp() {
        assert!(close(px("min(10px, 2em)"), 10.0));
        assert!(close(px("max(10px, 2em)"), 40.0));
        assert!(close(px("clamp(1rem, 5vw, 3rem)"), 30.0));
        assert!(close(px("clamp(1rem, 1vw, 3rem)"), 10.0));
        assert!(close(px("calc(min(10px, 20px) + 1px)"), 11.0));
        assert_eq!(parse_length("min(100%, 800px)", &ctx()), None, "percentages inside min() need layout");
        assert_eq!(parse_length("clamp(1px, 2px)", &ctx()), None);
    }

    #[test]
    fn hostile_input_is_bounded() {
        let deep = format!("{}1px{}", "calc(".repeat(10_000), ")".repeat(10_000));
        assert_eq!(parse_length(&deep, &ctx()), None);
        let nested = format!("{}1px{}", "calc(".repeat(MAX_CALC_DEPTH), ")".repeat(MAX_CALC_DEPTH));
        assert_eq!(parse_length(&nested, &ctx()), Some(Length::px(1.0)));
        assert_eq!(parse_length("1e39px", &ctx()), None, "overflow to infinity is rejected");
        assert_eq!(parse_length("calc(1e30px * 1e30)", &ctx()), None);
        let many = format!("min({})", vec!["1px"; 1000].join(","));
        assert_eq!(parse_length(&many, &ctx()), None);
        assert_eq!(parse_length(&"1".repeat(MAX_LENGTH_INPUT + 1), &ctx()), None);
    }
}
