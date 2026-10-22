//! The inline formatting context: tokenizing text/inline-element
//! content into words (`InlineWord`), line-breaking them against an
//! available width (`layout_inline_run`), and emitting one `LayoutBox`
//! per visible word with `text-align`/`vertical-align` applied.

use crate::box_model::style_of;
use crate::{LayoutBox, TextBox};
use dom::{ChildRef, ElementRef};
use style::{Color, Style, StyleCache, TextAlign};
use text::Font;

pub(crate) struct InlineWord {
    pub(crate) text: String,
    pub(crate) font_size: f32,
    /// Milestone 35: the element's text properties (font, line height,
    /// decoration), shared.
    pub(crate) text_style: std::sync::Arc<style::TextStyle>,
    pub(crate) color: Color,
    pub(crate) vertical_align: style::VerticalAlign,
    /// Number of spaces immediately before this word. Always 1 for
    /// collapsing white-space (0 is applied implicitly at line start);
    /// the real run length for `pre`/`pre-wrap`.
    pub(crate) leading_spaces: u32,
    /// This token is a forced line break (`<br>`, or a literal newline
    /// under `pre`/`pre-wrap`) — it renders nothing, it just ends the
    /// current line.
    pub(crate) is_break: bool,
    /// `dom::element_ptr_key` of the innermost inline element (e.g.
    /// `<a>`, `<b>`, `<span>`) this word's text is directly inside —
    /// `None` for text that belongs directly to the containing BLOCK
    /// element (which already carries its own key on its own
    /// `LayoutBox`, so a word-level key would be redundant there).
    /// This is what lets `layout_inline_run` give the resulting word
    /// box its own `element_key`, so clicking rendered inline text —
    /// most importantly an `<a href>` inside a paragraph, the single
    /// most common real click target this engine has — resolves to
    /// THAT element, not just "somewhere inside the paragraph." Before
    /// this field existed, every word box's `element_key` was
    /// unconditionally `None`, so `hit_test_path` could never name an
    /// inline element as the click target at all — a real, previously
    /// unverified bug (see `milestone-21-page-navigation.md`'s own
    /// acceptance criteria: "an actual click on a real on-screen link
    /// wasn't manually exercised this pass").
    pub(crate) element_key: Option<dom::NodeId>,
    /// Milestone 33: an atomic inline box (`inline-block`, `inline-flex`,
    /// a replaced element) instead of text.
    pub(crate) atomic: Option<Box<AtomicBox>>,
}

/// An atomic inline-level box, already laid out with its margin box's
/// top-left corner at (0, 0); line layout translates it into place.
pub(crate) struct AtomicBox {
    /// `None` when only measuring (intrinsic sizing).
    pub(crate) layout: Option<LayoutBox>,
    /// Margin-box size.
    pub(crate) width: f32,
    pub(crate) height: f32,
    /// Its baseline, from the margin box's top: the last line box's
    /// baseline inside it, or the bottom margin edge when it has none
    /// (CSS 2 §10.8.1).
    pub(crate) ascent: f32,
    /// Min-content width (for intrinsic sizing; `width` otherwise).
    pub(crate) min_width: f32,
}

impl AtomicBox {
    /// Wraps a laid-out box whose margin box starts at (0, 0).
    pub(crate) fn from_layout(layout: LayoutBox, margin: style::EdgeSizes) -> AtomicBox {
        let width = margin.left + layout.width + margin.right;
        let height = margin.top + layout.height + margin.bottom;
        let ascent = last_baseline(&layout).unwrap_or(height);
        AtomicBox { layout: Some(layout), width, height, ascent, min_width: width }
    }
}

/// The baseline of the last line box in `root`'s subtree, if any, in the
/// frame `root` is in (Milestone 39b: a reused subtree's children may be in
/// a shifted frame).
fn last_baseline(root: &LayoutBox) -> Option<f32> {
    if let Some(text) = &root.text {
        return Some(text.baseline_y);
    }
    let dy = root.subtree.child_offset.1;
    root.children.iter().rev().find_map(last_baseline).map(|b| b + dy)
}

/// Lays out an element's atomic inline child, or `None` to skip it. Given
/// to [`collect_inline_words`] by whoever owns the layout context.
pub(crate) type AtomicLayout<'a> = dyn FnMut(ElementRef<'_>, &Style) -> Option<AtomicBox> + 'a;

impl InlineWord {
    fn word(text: String, style: &Style, leading_spaces: u32, element_key: Option<dom::NodeId>) -> Self {
        InlineWord {
            text,
            font_size: style.font_size,
            text_style: std::sync::Arc::clone(&style.text),
            // Hidden text keeps its place in the line but paints nothing.
            color: if style.visibility == style::Visibility::Hidden { Color::TRANSPARENT } else { style.color },
            vertical_align: style.vertical_align,
            leading_spaces,
            is_break: false,
            element_key,
            atomic: None,
        }
    }

    pub(crate) fn atomic(atomic: AtomicBox, style: &Style) -> Self {
        InlineWord {
            text: String::new(),
            font_size: style.font_size,
            text_style: std::sync::Arc::clone(&style.text),
            color: style.color,
            vertical_align: style.vertical_align,
            leading_spaces: 1,
            is_break: false,
            element_key: None,
            atomic: Some(Box::new(atomic)),
        }
    }
    fn hard_break() -> Self {
        InlineWord {
            text: String::new(),
            font_size: 0.0,
            text_style: style::text::default_text(),
            color: Color::TRANSPARENT,
            vertical_align: style::VerticalAlign::Baseline,
            leading_spaces: 0,
            is_break: true,
            element_key: None,
            atomic: None,
        }
    }
}

/// Splits a run of text into words, tokenizing according to the
/// element's `white-space`. For `pre`/`pre-wrap`, literal newlines
/// become break tokens and runs of spaces are preserved as
/// `leading_spaces` counts (tabs count as 4); leading/trailing
/// whitespace on a segment is approximated rather than pixel-preserved
/// (a documented Phase B simplification — full preservation needs a
/// different text-box model).
pub(crate) fn collect_words_from_text(text: &str, style: &Style, element_key: Option<dom::NodeId>, out: &mut Vec<InlineWord>) {
    use style::WhiteSpace;
    let preserve = matches!(style.white_space, WhiteSpace::Pre | WhiteSpace::PreWrap);
    let transform = style.text.text_transform;
    if !preserve {
        for word in text.split_whitespace() {
            push_breakable(transform.apply(word, true), style, 1, element_key, out);
        }
        return;
    }
    for (seg_idx, segment) in text.split('\n').enumerate() {
        if seg_idx > 0 {
            out.push(InlineWord::hard_break());
        }
        let mut spaces = 0u32;
        let mut word = String::new();
        let flush = |word: &mut String, spaces: &mut u32, out: &mut Vec<InlineWord>| {
            if !word.is_empty() {
                push_breakable(transform.apply(&std::mem::take(word), true), style, *spaces, element_key, out);
                *spaces = 0;
            }
        };
        for ch in segment.chars() {
            match ch {
                ' ' => {
                    flush(&mut word, &mut spaces, out);
                    spaces += 1;
                }
                '\t' => {
                    flush(&mut word, &mut spaces, out);
                    spaces += 4;
                }
                _ => word.push(ch),
            }
        }
        flush(&mut word, &mut spaces, out);
    }
}

/// Milestone 35: pushes a space-delimited word, split where Unicode line
/// breaking (UAX #14) allows a break inside it — between ideographs, after
/// a hyphen — into pieces joined with no space (`leading_spaces` 0), so a
/// line can end between them. Plain ASCII letters and digits have no such
/// opportunity and skip the analysis.
fn push_breakable(word: String, style: &Style, leading_spaces: u32, element_key: Option<dom::NodeId>, out: &mut Vec<InlineWord>) {
    if word.bytes().all(|b| b.is_ascii_alphanumeric()) {
        out.push(InlineWord::word(word, style, leading_spaces, element_key));
        return;
    }
    let mut start = 0;
    let mut first = true;
    for (at, _) in unicode_linebreak::linebreaks(&word) {
        if at == 0 || at >= word.len() {
            continue;
        }
        out.push(InlineWord::word(word[start..at].to_string(), style, if first { leading_spaces } else { 0 }, element_key));
        first = false;
        start = at;
    }
    out.push(InlineWord::word(word[start..].to_string(), style, if first { leading_spaces } else { 0 }, element_key));
}

/// Milestone 35: each word's bidi embedding level (UAX #9), resolved over
/// the whole run as one paragraph whose base direction is `rtl`. A run
/// with no right-to-left text and a left-to-right base is all level 0
/// without the analysis.
fn word_levels(words: &[InlineWord], rtl: bool) -> Vec<u8> {
    let has_rtl = |t: &str| t.chars().any(|c| matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF | 0x200F | 0x202B | 0x202E | 0x2067));
    if !rtl && !words.iter().any(|w| has_rtl(&w.text)) {
        return vec![0; words.len()];
    }
    let mut para = String::new();
    let mut starts = Vec::with_capacity(words.len());
    for w in words {
        if !para.is_empty() {
            para.push(' ');
        }
        starts.push(para.len());
        if w.atomic.is_some() {
            para.push('\u{FFFC}');
        } else if w.text.is_empty() {
            para.push(' ');
        } else {
            para.push_str(&w.text);
        }
    }
    let base = if rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::BidiInfo::new(&para, Some(base));
    starts.iter().map(|&i| info.levels.get(i).map_or(base.number(), |l| l.number())).collect()
}

/// The visual order of a line's words from their levels (UAX #9 rule L2):
/// from the highest level down to the lowest odd one, every run of words
/// at that level or higher is reversed.
fn visual_order(levels: &[u8]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..levels.len()).collect();
    let (Some(&max), Some(min_odd)) = (levels.iter().max(), levels.iter().filter(|l| *l % 2 == 1).min().copied()) else {
        return order;
    };
    for level in (min_odd..=max).rev() {
        let mut i = 0;
        while i < order.len() {
            if levels[order[i]] >= level {
                let start = i;
                while i < order.len() && levels[order[i]] >= level {
                    i += 1;
                }
                order[start..i].reverse();
            } else {
                i += 1;
            }
        }
    }
    order
}

/// Milestone 35: the shaping spec for words of one text style — the last
/// one looked up is kept (consecutive words overwhelmingly share it).
/// Milestone 35: the shaping spec and line metrics for words of one text
/// style — looked up once per thread and font set, not once per line. An
/// entry holds its style (so the pointer it is keyed by cannot be reused
/// while it is cached); the table is bounded.
#[derive(Default)]
pub(crate) struct SpecCache {
    last: Option<(*const style::TextStyle, u32, text::TextSpec, text::LineMetrics)>,
}

type SpecKey = (u64, usize, u32);
type SpecEntry = (std::sync::Arc<style::TextStyle>, text::TextSpec, text::LineMetrics);

thread_local! {
    static SPECS: std::cell::RefCell<std::collections::HashMap<SpecKey, SpecEntry>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

impl SpecCache {
    fn lookup(&mut self, font: &Font, t: &std::sync::Arc<style::TextStyle>, size: f32) -> (text::TextSpec, text::LineMetrics) {
        let ptr = std::sync::Arc::as_ptr(t);
        if let Some((p, s, spec, lm)) = self.last {
            if p == ptr && s == size.to_bits() {
                return (spec, lm);
            }
        }
        let key: SpecKey = (font.id(), ptr as usize, size.to_bits());
        let (spec, lm) = SPECS.with(|specs| {
            if let Some((_, spec, lm)) = specs.borrow().get(&key) {
                return (*spec, *lm);
            }
            let families: Vec<&str> = t.font_family.iter().map(|f| &**f).collect();
            let style = match t.font_style {
                style::FontStyleKind::Normal => text::FontStyle::Normal,
                style::FontStyleKind::Italic => text::FontStyle::Italic,
                style::FontStyleKind::Oblique => text::FontStyle::Oblique,
            };
            let spec = text::TextSpec { selector: font.selector(&families, t.font_weight, style), size, rtl: t.rtl };
            let lm = font.line_metrics(&spec);
            let mut specs = specs.borrow_mut();
            if specs.len() >= 256 {
                specs.clear();
            }
            specs.insert(key, (std::sync::Arc::clone(t), spec, lm));
            (spec, lm)
        });
        self.last = Some((ptr, size.to_bits(), spec, lm));
        (spec, lm)
    }

    pub(crate) fn spec(&mut self, font: &Font, t: &std::sync::Arc<style::TextStyle>, size: f32) -> text::TextSpec {
        self.lookup(font, t, size).0
    }

    /// A word's width, ascent and line height: shaped per its style, with
    /// `line-height` applied (half the leading above, half below).
    pub(crate) fn measure(&mut self, font: &Font, w: &InlineWord, rtl: bool) -> text::TextMetrics {
        let (spec, lm) = self.lookup(font, &w.text_style, w.font_size);
        let width = font.width(&w.text, &text::TextSpec { rtl, ..spec });
        match w.text_style.line_height.resolve(w.font_size) {
            None => text::TextMetrics { width, ascent: lm.ascent, line_height: lm.ascent + lm.descent + lm.line_gap },
            Some(lh) => {
                let leading = lh - (lm.ascent + lm.descent);
                text::TextMetrics { width, ascent: lm.ascent + leading / 2.0, line_height: lh }
            }
        }
    }
}

/// Same rationale as `crate::MAX_LAYOUT_DEPTH` (see its doc comment),
/// applied to inline-element nesting specifically
/// (`<span><span><span>...`) — a separate recursion with no depth
/// bound of its own.
const MAX_INLINE_DEPTH: usize = 512;

/// The inline content of `element` (an inline box): its text, its inline
/// descendants' text, and its atomic inline descendants (via `atomic`).
/// `display: none` descendants contribute nothing.
pub(crate) fn collect_inline_words(
    element: ElementRef<'_>,
    resolved_style: &Style,
    style_cache: &StyleCache,
    out: &mut Vec<InlineWord>,
    depth: usize,
    atomic: &mut AtomicLayout,
) {
    if element.is(dom::atoms::BR) {
        out.push(InlineWord::hard_break());
        return;
    }
    if depth >= MAX_INLINE_DEPTH {
        diagnostics::guard(diagnostics::Guard::InlineDepth, "inline content nested past the inline depth cap; skipped");
        return;
    }
    let own_key = Some(element.node_id());
    let generated = crate::box_model::with_generated(element, style_cache);
    for child in generated.iter() {
        match child {
            ChildRef::Text(text) => collect_words_from_text(text, resolved_style, own_key, out),
            ChildRef::Element(child_el) if child_el.is(dom::atoms::BR) => out.push(InlineWord::hard_break()),
            ChildRef::Element(child_el) => {
                let child_style = style_of(style_cache, child_el);
                push_inline_child(child_el, child_style, style_cache, out, depth + 1, atomic);
            }
        }
    }
}

/// One inline-level child: its words if it is `inline`, one atomic item if
/// it is atomic, nothing if it is `display: none` or out of flow. (A block-level element
/// inside an inline one is flowed as inline content — block-in-inline
/// splitting is not modelled.)
pub(crate) fn push_inline_child(
    child: ElementRef<'_>,
    child_style: &Style,
    style_cache: &StyleCache,
    out: &mut Vec<InlineWord>,
    depth: usize,
    atomic: &mut AtomicLayout,
) {
    match child_style.display {
        style::Display::None => {}
        // Out of flow: not on the line at all. The callback records where
        // it would have been (its static position) and returns `None`.
        _ if child_style.position.is_out_of_flow() => {
            let _ = atomic(child, child_style);
        }
        // A float in inline content: the callback places it (layout) or
        // measures it as one item (intrinsic sizing).
        d if d.is_atomic_inline() || child_style.float != style::Float::None => {
            if let Some(a) = atomic(child, child_style) {
                out.push(InlineWord::atomic(a, child_style));
            }
        }
        _ => collect_inline_words(child, child_style, style_cache, out, depth, atomic),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn flush_pending_inline(
    pending: &mut Vec<InlineWord>,
    cursor_y: &mut f32,
    children: &mut Vec<LayoutBox>,
    content_x: f32,
    content_width: f32,
    text_align: TextAlign,
    white_space: style::WhiteSpace,
    font: &Font,
    exclusions: &[crate::floats::Exclusion],
    rtl: bool,
) {
    if pending.is_empty() {
        return;
    }
    let (boxes, new_cursor_y) = layout_inline_run(
        std::mem::take(pending),
        content_x,
        *cursor_y,
        content_width,
        text_align,
        white_space,
        font,
        exclusions,
        rtl,
    );
    *cursor_y = new_cursor_y;
    if children.is_empty() {
        *children = boxes; // the common case: no copy of the boxes
    } else {
        children.extend(boxes);
    }
}

#[derive(Default)]
struct Line {
    /// (word index, x within container, width, ascent, line_height).
    /// The trailing ascent/line_height are the SAME `font.measure`
    /// result already computed during line-breaking below — carried
    /// through so the box-emission pass doesn't call `font.measure` on
    /// the same word a second time (see `layout_inline_run`'s doc
    /// comment for why that mattered).
    words: Vec<(usize, f32, f32, f32, f32)>,
    /// Milestone 35: the space before each word (0 for the first, and
    /// between the pieces of a word split at a break opportunity).
    gaps: Vec<f32>,
    content_width: f32,
    ascent: f32,
    height: f32,
    ended_by_hard_break: bool,
    /// Milestone 33: the line's top, and the room beside any floats — its
    /// left edge (from the container's) and width.
    y: f32,
    left: f32,
    room: f32,
}

fn vertical_align_offset(
    va: style::VerticalAlign,
    word_ascent: f32,
    word_line_height: f32,
    line_ascent: f32,
    line_height: f32,
    font_size: f32,
) -> f32 {
    use style::VerticalAlign::*;
    // Positive = shift this word's baseline downward from the line's
    // shared baseline (`line top + line_ascent`).
    match va {
        Baseline => 0.0,
        Sub => font_size * 0.2,
        Super => -font_size * 0.3,
        Top | TextTop => word_ascent - line_ascent,
        Bottom | TextBottom => (line_height - (word_line_height - word_ascent)) - line_ascent,
        Middle => (line_height - word_line_height) / 2.0 + word_ascent - line_ascent,
    }
}

/// Performance note: `font.measure` (an uncached per-character
/// glyph-metrics walk — see `text::Font::measure`) used to be called
/// TWICE per word on every restyle of any text-bearing page: once
/// here during line-breaking, and again in the box-emission loop below
/// just to re-derive ascent/line-height for `vertical_align`. The
/// line-breaking pass now carries its own measurement through in
/// `Line.words` so the emission loop reuses it instead of re-measuring
/// — cutting `font.measure` calls on the hot text-layout path roughly
/// in half. `space_width` is also cached per `font_size` rather than
/// re-measured for every single word (consecutive words overwhelmingly
/// share the same size).
#[allow(clippy::too_many_arguments)]
fn layout_inline_run(
    mut words: Vec<InlineWord>,
    start_x: f32,
    start_y: f32,
    container_width: f32,
    text_align: TextAlign,
    white_space: style::WhiteSpace,
    font: &Font,
    exclusions: &[crate::floats::Exclusion],
    rtl: bool,
) -> (Vec<LayoutBox>, f32) {
    use style::WhiteSpace;
    let levels = word_levels(&words, rtl);
    let wrap_allowed = matches!(white_space, WhiteSpace::Normal | WhiteSpace::PreWrap);

    let mut lines: Vec<Line> = Vec::new();
    let mut cursor_x = 0.0f32;
    let mut space_width_cache: Option<(f32, f32)> = None; // (font_size, space_width)
    // Milestone 33: where the next line starts, and the room there beside
    // any floats. A line whose first item does not fit beside the floats
    // moves down below them (CSS 2 §9.5).
    let mut next_y = start_y;
    let room_at = |y: f32, h: f32, need: f32| -> (f32, f32, f32) {
        if exclusions.is_empty() {
            return (y, 0.0, container_width);
        }
        let mut y = y;
        for _ in 0..=exclusions.len() {
            let (l, r) = crate::floats::line_interval(exclusions, y, h, start_x, start_x + container_width);
            if r - l >= need || crate::floats::next_bottom(exclusions, y, h).is_none() {
                return (y, l - start_x, (r - l).max(0.0));
            }
            y = crate::floats::next_bottom(exclusions, y, h).unwrap_or(y);
        }
        (y, 0.0, container_width)
    };
    let mut current = Line { y: start_y, room: container_width, ..Line::default() };
    let mut specs = SpecCache::default();

    let push_line = |lines: &mut Vec<Line>, current: &mut Line, next_y: &mut f32, hard: bool| {
        current.ended_by_hard_break = hard;
        if current.ascent == 0.0 {
            // An empty line (e.g. a leading <br>) still occupies one
            // line's worth of vertical space — use a default metric.
            let m = font.measure("x", 16.0);
            current.ascent = m.ascent;
            current.height = m.line_height;
            if current.words.is_empty() {
                current.y = *next_y;
            }
        }
        *next_y = current.y + current.height;
        lines.push(std::mem::take(current));
    };

    for (i, w) in words.iter().enumerate() {
        if w.is_break {
            push_line(&mut lines, &mut current, &mut next_y, true);
            cursor_x = 0.0;
            continue;
        }
        let metrics = match &w.atomic {
            Some(a) => text::TextMetrics { width: a.width, ascent: a.ascent, line_height: a.height },
            None => specs.measure(font, w, levels[i] % 2 == 1),
        };
        let space_width = match space_width_cache {
            Some((sz, sw)) if sz == w.font_size => sw,
            _ => {
                let spec = specs.spec(font, &w.text_style, w.font_size);
                let sw = font.measure_styled(" ", &spec).width;
                space_width_cache = Some((w.font_size, sw));
                sw
            }
        };

        let start_line = |current: &mut Line, y: f32| {
            let (y, left, room) = room_at(y, metrics.line_height, metrics.width);
            (current.y, current.left, current.room) = (y, left, room);
        };
        if current.words.is_empty() {
            start_line(&mut current, next_y);
            current.words.push((i, 0.0, metrics.width, metrics.ascent, metrics.line_height));
            current.gaps.push(0.0);
            cursor_x = metrics.width;
        } else {
            // Milestone 35: 0 between the pieces of a word split at a
            // line-break opportunity.
            let gap = w.leading_spaces as f32 * space_width;
            let tentative_x = cursor_x + gap;
            if wrap_allowed && tentative_x + metrics.width > current.room {
                push_line(&mut lines, &mut current, &mut next_y, false);
                start_line(&mut current, next_y);
                current.words.push((i, 0.0, metrics.width, metrics.ascent, metrics.line_height));
                current.gaps.push(0.0);
                cursor_x = metrics.width;
            } else {
                current.words.push((i, tentative_x, metrics.width, metrics.ascent, metrics.line_height));
                current.gaps.push(gap);
                cursor_x = tentative_x + metrics.width;
            }
        }

        current.content_width = cursor_x;
        current.ascent = current.ascent.max(metrics.ascent);
        current.height = current.height.max(metrics.line_height);
    }
    if !current.words.is_empty() {
        push_line(&mut lines, &mut current, &mut next_y, false);
    }
    if lines.is_empty() {
        return (Vec::new(), start_y);
    }

    let line_count = lines.len();
    let mut boxes = Vec::with_capacity(words.len());
    for (line_idx, line) in lines.iter().enumerate() {
        let y = line.y;
        let container_width = line.room;
        let start_x = start_x + line.left;
        let is_last_line = line_idx + 1 == line_count;
        // Justification stretches real spaces, not the joins inside a word.
        let gap_count = line.gaps.iter().skip(1).filter(|g| **g > 0.0).count();
        // Milestone 35: right-to-left words in visual order, and where each
        // word then sits on the line.
        let reordered = line.words.iter().any(|w| levels[w.0] % 2 == 1);
        let mut placed: Vec<(f32, usize)> = Vec::new();
        if reordered {
            let line_levels: Vec<u8> = line.words.iter().map(|w| levels[w.0]).collect();
            let order = visual_order(&line_levels);
            placed = vec![(0.0f32, 0usize); line.words.len()];
            let (mut x, mut spaces) = (0.0f32, 0usize);
            for (k, &p) in order.iter().enumerate() {
                if k > 0 {
                    let prev = order[k - 1];
                    // The space between two logical neighbours; after a
                    // reordered run, a space's width.
                    let gap = if prev.abs_diff(p) == 1 { line.gaps[prev.max(p)] } else { line.gaps.iter().copied().fold(0.0, f32::max) };
                    x += gap;
                    if gap > 0.0 {
                        spaces += 1;
                    }
                }
                placed[p] = (x, spaces);
                x += line.words[p].2;
            }
        }
        // In logical order: where line breaking put each word, and the
        // spaces before it (for justification).
        let mut spaces_so_far = 0usize;

        // text-align: justify stretches inter-word gaps on every line
        // except the last and any line ended by a forced break.
        let justify_extra = if text_align == TextAlign::Justify
            && !is_last_line
            && !line.ended_by_hard_break
            && gap_count > 0
        {
            ((container_width - line.content_width) / gap_count as f32).max(0.0)
        } else {
            0.0
        };

        let effective_width = line.content_width + justify_extra * gap_count as f32;
        let right = (container_width - effective_width).max(0.0);
        let align_offset = match text_align {
            TextAlign::Left => 0.0,
            TextAlign::Right => right,
            TextAlign::Start | TextAlign::Justify => if rtl { right } else { 0.0 },
            TextAlign::End => if rtl { 0.0 } else { right },
            TextAlign::Center => ((container_width - effective_width) / 2.0).max(0.0),
        };

        for (word_pos, &(word_index, _, word_width, word_ascent, word_line_height)) in line.words.iter().enumerate() {
            let (line_x, spaces_before) = if reordered {
                placed[word_pos]
            } else {
                if word_pos > 0 && line.gaps[word_pos] > 0.0 {
                    spaces_so_far += 1;
                }
                (line.words[word_pos].1, spaces_so_far)
            };
            let w = &mut words[word_index];
            let valign = vertical_align_offset(
                w.vertical_align,
                word_ascent,
                word_line_height,
                line.ascent,
                line.height,
                w.font_size,
            );
            let x = start_x + align_offset + line_x + justify_extra * spaces_before as f32;
            if let Some(atomic) = w.atomic.as_mut().and_then(|a| a.layout.take()) {
                let mut atomic = atomic;
                crate::reuse::translate(&mut atomic, x, y + line.ascent - word_ascent + valign);
                boxes.push(atomic);
                continue;
            }
            boxes.push(LayoutBox {
                x,
                y,
                width: word_width,
                height: line.height,
                background_color: Color::TRANSPARENT,
                border: None,
                border_radius: 0.0,
                overflow: crate::OverflowClip::default(),
                opacity: 1.0,
                visible: true,
                image: None,
                extras: None,
                element_key: w.element_key,
                text: Some(TextBox {
                    // Each word is emitted once: move its text, don't copy it.
                    content: std::mem::take(&mut w.text),
                    font_size: w.font_size,
                    color: w.color,
                    baseline_y: y + line.ascent + valign,
                    spec: text::TextSpec { rtl: levels[word_index] % 2 == 1, ..specs.spec(font, &w.text_style, w.font_size) },
                    decoration: w.text_style.decoration,
                }),
                children: vec![],
                subtree: Default::default(),
            });
        }
    }

    let end = lines.last().map_or(start_y, |l| l.y + l.height);
    (boxes, end)
}

#[cfg(test)]
mod tests {
    use crate::*;
    use css_parser::Stylesheet;
    use dom::{Element, Node};
    use images::ImageCache;
    use text::Font;

    fn test_font() -> Font {
        Font::dejavu_sans()
    }



    fn no_sheet() -> Stylesheet {
        Stylesheet::default()
    }



    // ---- Milestone 35: bidi and line breaking ----

    fn text_of(b: &LayoutBox) -> &str {
        &b.text.as_ref().unwrap().content
    }

    #[test]
    fn a_right_to_left_paragraph_reads_from_the_right() {
        let words = inline_words("direction: rtl", "שלום עולם", 400.0);
        assert_eq!(words.len(), 2);
        let (first, second) = (&words[0], &words[1]);
        assert_eq!(text_of(first), "שלום");
        assert!(first.x > second.x, "the first word is on the right: {} vs {}", first.x, second.x);
        assert!((first.x + first.width - 400.0).abs() < 1.0, "start-aligned: flush right, ends at {}", first.x + first.width);
        assert!(first.text.as_ref().unwrap().spec.rtl, "shaped right to left");
    }

    #[test]
    fn a_hebrew_phrase_inside_english_is_reversed_as_a_run() {
        let words = inline_words("", "go שלום עולם now", 800.0);
        let x = |t: &str| words.iter().find(|w| text_of(w) == t).unwrap().x;
        assert!(x("go") < x("עולם") && x("עולם") < x("שלום") && x("שלום") < x("now"), "LTR base: go [עולם שלום] now");
    }

    #[test]
    fn cjk_text_wraps_between_characters() {
        let text = "世界世界世界世界世界世界世界世界";
        let words = inline_words("", text, 60.0);
        let ys: std::collections::BTreeSet<i32> = words.iter().map(|w| w.y as i32).collect();
        assert!(ys.len() > 1, "wrapped onto several lines, not one overflowing word: {} pieces", words.len());
        assert!(words.iter().all(|w| w.x + w.width <= 60.5));
    }

    #[test]
    fn a_line_may_end_after_a_hyphen() {
        let words = inline_words("", "extraordinarily-long-compound-word", 120.0);
        assert!(words.len() > 1 && text_of(&words[0]).ends_with('-'), "{:?}", words.iter().map(text_of).collect::<Vec<_>>());
    }

    // ---- Phase B: text & inline layout completeness ----

    /// A `<p>` with `style` and a single long text child, laid out in
    /// `width` px — returns the inline word boxes.
    fn inline_words(style: &str, text: &str, width: f32) -> Vec<LayoutBox> {
        let mut p = Element::new("p");
        if !style.is_empty() {
            p.attributes.push(("style".into(), style.into()));
        }
        p.children.push(Node::Text(text.into()));
        layout_document(&p, width, &test_font(), &no_sheet(), &mut ImageCache::new()).children
    }



    #[test]
    fn text_node_becomes_one_leaf_box_per_word() {
        let mut el = Element::new("div");
        el.children.push(Node::Text("Hello, engine".to_string()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 2);
        assert_eq!(laid_out.children[0].text.as_ref().unwrap().content, "Hello,");
        assert_eq!(laid_out.children[1].text.as_ref().unwrap().content, "engine");
    }


    #[test]
    fn whitespace_only_text_node_is_skipped() {
        let mut el = Element::new("div");
        el.children.push(Node::Text("   \n  ".to_string()));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 0);
    }


    #[test]
    fn long_text_wraps_into_multiple_lines_within_container_width() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:150px".into()));
        el.children.push(Node::Text(
            "The quick brown fox jumps over the lazy dog many times today".to_string(),
        ));
        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert!(laid_out.children.len() > 1);
    }


    #[test]
    fn text_align_center_shifts_line_within_container() {
        let mut left_el = Element::new("div");
        left_el.attributes.push(("style".into(), "width:400px".into()));
        left_el.children.push(Node::Text("Hi".to_string()));
        let left_box = layout_document(&left_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());

        let mut center_el = Element::new("div");
        center_el.attributes.push(("style".into(), "width:400px;text-align:center".into()));
        center_el.children.push(Node::Text("Hi".to_string()));
        let center_box = layout_document(&center_el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());

        assert!(center_box.children[0].x > left_box.children[0].x);
    }


    #[test]
    fn inline_element_words_flow_with_surrounding_text_on_same_line() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:800px".into()));
        el.children.push(Node::Text("before ".to_string()));
        let mut span = Element::new("span");
        span.children.push(Node::Text("middle".to_string()));
        el.children.push(Node::Element(span));
        el.children.push(Node::Text(" after".to_string()));

        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 3);
    }


    #[test]
    fn block_element_interrupts_and_resumes_inline_flow_correctly() {
        let mut el = Element::new("div");
        el.attributes.push(("style".into(), "width:800px".into()));
        el.children.push(Node::Text("before block".to_string()));
        let mut block_child = Element::new("div");
        block_child.attributes.push(("style".into(), "height:20px".into()));
        el.children.push(Node::Element(block_child));
        el.children.push(Node::Text("after block".to_string()));

        let laid_out = layout_document(&el, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        assert_eq!(laid_out.children.len(), 5);
    }


    #[test]
    fn text_wraps_at_the_container_edge_by_default() {
        let words = inline_words("width: 120px", "one two three four five six seven eight", 800.0);
        // More than one distinct y => it wrapped onto multiple lines.
        let ys: std::collections::BTreeSet<i32> = words.iter().map(|b| b.y as i32).collect();
        assert!(ys.len() >= 2, "expected wrapping onto multiple lines");
    }


    #[test]
    fn white_space_nowrap_keeps_everything_on_one_line() {
        let words = inline_words("width: 120px; white-space: nowrap", "one two three four five six seven eight", 800.0);
        let ys: std::collections::BTreeSet<i32> = words.iter().map(|b| b.y as i32).collect();
        assert_eq!(ys.len(), 1, "nowrap must not wrap");
        // And the run overflows its container (that's expected for nowrap).
        let rightmost = words.iter().map(|b| b.x + b.width).fold(0.0_f32, f32::max);
        assert!(rightmost > 120.0);
    }


    #[test]
    fn white_space_pre_preserves_newlines_as_line_breaks() {
        let words = inline_words("white-space: pre", "line one\nline two\nline three", 800.0);
        let contents: Vec<&str> = words.iter().map(|b| b.text.as_ref().unwrap().content.as_str()).collect();
        assert_eq!(contents, vec!["line", "one", "line", "two", "line", "three"]);
        // Three distinct y values — one per source line.
        let ys: std::collections::BTreeSet<i32> = words.iter().map(|b| b.y as i32).collect();
        assert_eq!(ys.len(), 3);
    }


    #[test]
    fn white_space_pre_preserves_multiple_spaces() {
        let normal = inline_words("", "a     b", 800.0);
        let pre = inline_words("white-space: pre", "a     b", 800.0);
        let gap = |ws: &[LayoutBox]| ws[1].x - (ws[0].x + ws[0].width);
        assert!(gap(&pre) > gap(&normal) * 3.0, "pre should keep the 5-space gap much wider than a collapsed single space");
    }


    #[test]
    fn br_forces_a_line_break() {
        let mut p = Element::new("p");
        p.children.push(Node::Text("before".into()));
        p.children.push(Node::Element(Element::new("br")));
        p.children.push(Node::Text("after".into()));
        let words = layout_document(&p, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new()).children;
        let contents: Vec<&str> = words.iter().map(|b| b.text.as_ref().unwrap().content.as_str()).collect();
        assert_eq!(contents, vec!["before", "after"]);
        assert!(words[1].y > words[0].y, "text after <br> must be on a lower line");
    }


    #[test]
    fn text_align_justify_stretches_gaps_on_non_last_lines() {
        let width = 160.0;
        let words = inline_words(
            &format!("width: {width}px; text-align: justify"),
            "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll",
            800.0,
        );
        // Group words by line.
        let mut by_line: std::collections::BTreeMap<i32, Vec<&LayoutBox>> = Default::default();
        for w in &words {
            by_line.entry(w.y as i32).or_default().push(w);
        }
        assert!(by_line.len() >= 2, "need at least two lines to test justify");
        let (first_y, _) = by_line.iter().next().unwrap();
        let first_line = &by_line[first_y];
        // A justified non-last line should reach (near) the container's
        // right edge.
        let line_right = first_line.iter().map(|b| b.x + b.width).fold(0.0_f32, f32::max);
        assert!(line_right >= width - 2.0, "justified line should fill the width, got right edge {line_right}");

        // The LAST line must NOT be justified — its right edge is short of full width.
        let (last_y, _) = by_line.iter().next_back().unwrap();
        let last_line = &by_line[last_y];
        if last_line.len() < 3 {
            let last_right = last_line.iter().map(|b| b.x + b.width).fold(0.0_f32, f32::max);
            assert!(last_right < width - 5.0, "last line should not be stretched");
        }
    }


    #[test]
    fn vertical_align_shifts_the_baseline_within_a_line() {
        let mut p = Element::new("p");
        p.children.push(Node::Text("base".into()));
        let mut sup = Element::new("span");
        sup.attributes.push(("style".into(), "vertical-align: super".into()));
        sup.children.push(Node::Text("hi".into()));
        p.children.push(Node::Element(sup));

        let words = layout_document(&p, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new()).children;
        let base = words.iter().find(|b| b.text.as_ref().unwrap().content == "base").unwrap();
        let raised = words.iter().find(|b| b.text.as_ref().unwrap().content == "hi").unwrap();
        assert!(
            raised.text.as_ref().unwrap().baseline_y < base.text.as_ref().unwrap().baseline_y,
            "vertical-align: super should raise the baseline (smaller y)"
        );
    }


    #[test]
    fn white_space_and_vertical_align_default_and_inherit() {
        let mut parent = Element::new("div");
        parent.attributes.push(("style".into(), "white-space: pre".into()));
        let mut child = Element::new("p");
        child.children.push(Node::Text("x\ny".into()));
        parent.children.push(Node::Element(child));
        let root = layout_document(&parent, 800.0, &test_font(), &no_sheet(), &mut ImageCache::new());
        // white-space inherited into <p>: the newline still breaks.
        let p_box = &root.children[0];
        let ys: std::collections::BTreeSet<i32> = p_box.children.iter().map(|b| b.y as i32).collect();
        assert_eq!(ys.len(), 2);
    }

}
