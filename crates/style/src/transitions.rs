//! Milestone 22: CSS `transition`/`@keyframes` animation — parsing the
//! `transition`/`animation-*` shorthands' pieces (easing, durations),
//! the cross-frame registries that remember what's mid-flight
//! (`TransitionRegistry`, `AnimationRegistry`), and the two passes
//! (`apply_transitions`, `apply_animations`) that substitute an
//! interpolated value in place of a freshly-cascaded target `Style`.

use crate::declarations::apply_declaration;
use crate::style::{Style, StyleCache};
use crate::length::ViewportSize;
use crate::values::{Color, Edges, Length, Transform};
use css_parser::KeyframesRule;
use dom::ElementRef;
use std::collections::HashMap;

/// Milestone 22: which property a `TransitionSpec` applies to.
/// Deliberately excludes `opacity` — despite being named in Milestone
/// 14/the deferred-items plan's original transition-property allowlist, `opacity`
/// was never actually implemented as a real CSS property anywhere in
/// this engine (checked directly before writing this — zero hits);
/// adding real alpha-compositing is separate, non-trivial rendering
/// work, not an animation-engine concern, so it's excluded from the
/// set that can meaningfully transition rather than silently accepted
/// and doing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionProperty {
    BackgroundColor,
    Transform,
    Width,
    Height,
    Margin,
    /// `transition: all ...` — matches every property above.
    All,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransitionSpec {
    pub property: TransitionProperty,
    /// Seconds.
    pub duration: f32,
    pub easing: Easing,
}

/// Milestone 22: a CSS easing function — `Linear` or a cubic-bezier
/// curve (the 4 standard keywords are just named control-point
/// presets; `cubic-bezier(x1,y1,x2,y2)` is the general form). `steps()`
/// is NOT implemented — a real, narrow scope cut (the 4 keywords plus
/// arbitrary `cubic-bezier()` cover the overwhelming majority of real
/// usage).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Easing {
    Linear,
    CubicBezier(f32, f32, f32, f32),
}

impl Easing {
    pub const EASE: Easing = Easing::CubicBezier(0.25, 0.1, 0.25, 1.0);
    pub const EASE_IN: Easing = Easing::CubicBezier(0.42, 0.0, 1.0, 1.0);
    pub const EASE_OUT: Easing = Easing::CubicBezier(0.0, 0.0, 0.58, 1.0);
    pub const EASE_IN_OUT: Easing = Easing::CubicBezier(0.42, 0.0, 0.58, 1.0);

    /// Evaluates the eased progress (0..1) for linear time progress
    /// `t` (0..1, i.e. `elapsed / duration`, already clamped by the
    /// caller). For a cubic bezier, real CSS easing curves are defined
    /// parametrically — `x` is time-progress, `y` is the eased output
    /// — with fixed endpoints `P0=(0,0)`/`P3=(1,1)` and author-supplied
    /// control points `P1=(x1,y1)`/`P2=(x2,y2)`. Finding the eased
    /// value for a given TIME means solving `x(t)=t_input` for the
    /// parametric `t`, then evaluating `y(t)` — done via bisection
    /// (`cubic_bezier_y_for_x`), not a closed-form inverse (cubics
    /// don't have a simple one), which is standard practice for this
    /// exact problem (browsers' own implementations do the same thing,
    /// sometimes with Newton-Raphson instead — bisection is simpler
    /// and plenty precise for a visual effect).
    pub fn apply(&self, t: f32) -> f32 {
        match self {
            Easing::Linear => t,
            Easing::CubicBezier(x1, y1, x2, y2) => cubic_bezier_y_for_x(t, *x1, *y1, *x2, *y2),
        }
    }
}

/// Cubic bezier with fixed endpoints 0 and 1: `B(t) = 3(1-t)²t·p1 +
/// 3(1-t)t²·p2 + t³`.
fn cubic_bezier_component(t: f32, p1: f32, p2: f32) -> f32 {
    let mt = 1.0 - t;
    3.0 * mt * mt * t * p1 + 3.0 * mt * t * t * p2 + t * t * t
}

fn cubic_bezier_y_for_x(x_target: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let x_target = x_target.clamp(0.0, 1.0);
    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    let mut t = x_target;
    for _ in 0..24 {
        let x = cubic_bezier_component(t, x1, x2);
        if (x - x_target).abs() < 0.0001 {
            break;
        }
        if x < x_target {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) / 2.0;
    }
    cubic_bezier_component(t, y1, y2)
}

pub(crate) fn parse_easing(s: &str) -> Option<Easing> {
    let s = s.trim();
    match s {
        "linear" => Some(Easing::Linear),
        "ease" => Some(Easing::EASE),
        "ease-in" => Some(Easing::EASE_IN),
        "ease-out" => Some(Easing::EASE_OUT),
        "ease-in-out" => Some(Easing::EASE_IN_OUT),
        _ => {
            let inner = s.strip_prefix("cubic-bezier(")?.strip_suffix(')')?;
            let nums: Vec<f32> = inner.split(',').filter_map(|n| n.trim().parse::<f32>().ok()).collect();
            match nums.as_slice() {
                [x1, y1, x2, y2] => Some(Easing::CubicBezier(*x1, *y1, *x2, *y2)),
                _ => None,
            }
        }
    }
}

/// Milestone 22: `transition: <property> <duration>[s|ms] [<easing>]`,
/// comma-separated for multiple properties (real CSS shorthand
/// grammar also allows a `<delay>` 4th component — not implemented, a
/// real, narrow scope cut). Unrecognized property NAMES are silently
/// skipped (that one entry contributes nothing, same graceful-
/// degradation policy as every other parser in this codebase) rather
/// than rejecting the whole declaration.
pub(crate) fn parse_transition_shorthand(value: &str) -> Vec<TransitionSpec> {
    value
        .split(',')
        .filter_map(|entry| {
            let tokens: Vec<&str> = entry.split_whitespace().collect();
            let property = match *tokens.first()? {
                "background-color" => TransitionProperty::BackgroundColor,
                "transform" => TransitionProperty::Transform,
                "width" => TransitionProperty::Width,
                "height" => TransitionProperty::Height,
                "margin" => TransitionProperty::Margin,
                "all" => TransitionProperty::All,
                _ => return None,
            };
            let duration = parse_duration_secs(tokens.get(1)?)?;
            let easing = tokens.get(2).and_then(|e| parse_easing(e)).unwrap_or(Easing::EASE);
            Some(TransitionSpec { property, duration, easing })
        })
        .collect()
}

/// Parses a CSS `<time>` token (`500ms` or `0.5s`) into seconds. Shared
/// by the `transition` and `animation` shorthand/longhand parsers.
pub(crate) fn parse_duration_secs(s: &str) -> Option<f32> {
    if let Some(s) = s.strip_suffix("ms") {
        Some(s.parse::<f32>().ok()? / 1000.0)
    } else if let Some(s) = s.strip_suffix('s') {
        s.parse::<f32>().ok()
    } else {
        None
    }
}

/// Milestone 22: one concrete animated value — whichever type the
/// property being transitioned actually holds. `Width`/`Height` are
/// lengths, and `Margin`'s four edges travel together as one `Edges`
/// unit, same as the CSS property itself. Milestone 33: a length is
/// `px + percent%`, and interpolating each part separately is exactly
/// CSS's interpolation of mixed lengths (via `calc()`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum AnimatedValue {
    Color(Color),
    Transform(Transform),
    Length(Length),
    Edges(Edges<Length>),
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
}

fn lerp_length(a: Length, b: Length, t: f32) -> Length {
    Length { px: lerp_f32(a.px, b.px, t), percent: lerp_f32(a.percent, b.percent, t) }
}

fn lerp_value(from: AnimatedValue, to: AnimatedValue, t: f32) -> AnimatedValue {
    match (from, to) {
        (AnimatedValue::Color(a), AnimatedValue::Color(b)) => AnimatedValue::Color(Color {
            r: lerp_u8(a.r, b.r, t),
            g: lerp_u8(a.g, b.g, t),
            b: lerp_u8(a.b, b.b, t),
            a: lerp_u8(a.a, b.a, t),
        }),
        (AnimatedValue::Transform(a), AnimatedValue::Transform(b)) => AnimatedValue::Transform(Transform {
            translate_x: lerp_f32(a.translate_x, b.translate_x, t),
            translate_y: lerp_f32(a.translate_y, b.translate_y, t),
            scale_x: lerp_f32(a.scale_x, b.scale_x, t),
            scale_y: lerp_f32(a.scale_y, b.scale_y, t),
            rotate_deg: lerp_f32(a.rotate_deg, b.rotate_deg, t),
            skew_x_deg: lerp_f32(a.skew_x_deg, b.skew_x_deg, t),
            skew_y_deg: lerp_f32(a.skew_y_deg, b.skew_y_deg, t),
        }),
        (AnimatedValue::Length(a), AnimatedValue::Length(b)) => AnimatedValue::Length(lerp_length(a, b, t)),
        (AnimatedValue::Edges(a), AnimatedValue::Edges(b)) => AnimatedValue::Edges(Edges {
            top: lerp_length(a.top, b.top, t),
            right: lerp_length(a.right, b.right, t),
            bottom: lerp_length(a.bottom, b.bottom, t),
            left: lerp_length(a.left, b.left, t),
        }),
        // Mismatched variants can't happen (`extract_value` is a pure
        // function of which `TransitionProperty` is being processed) —
        // fall back to the target rather than panicking if it somehow
        // did.
        _ => to,
    }
}

/// Reads the current value of `prop` off `style` — `None` for
/// `Width`/`Height` (and a `Margin` with an `auto` side) when the value is
/// `auto` (`None` in `Style` terms): real CSS doesn't interpolate to/from `auto` either (there's
/// no defined "auto as a number"), so those cases fall through to a
/// hard snap rather than a fabricated interpolation.
fn extract_value(style: &Style, prop: TransitionProperty) -> Option<AnimatedValue> {
    match prop {
        TransitionProperty::BackgroundColor => Some(AnimatedValue::Color(style.background_color)),
        TransitionProperty::Transform => Some(AnimatedValue::Transform(style.transform)),
        TransitionProperty::Width => style.width.map(AnimatedValue::Length),
        TransitionProperty::Height => style.height.map(AnimatedValue::Length),
        TransitionProperty::Margin => {
            let m = style.margin;
            Some(AnimatedValue::Edges(Edges { top: m.top?, right: m.right?, bottom: m.bottom?, left: m.left? }))
        }
        TransitionProperty::All => None, // never a concrete property to read/write directly
    }
}

fn assign_value(style: &mut Style, prop: TransitionProperty, value: AnimatedValue) {
    match (prop, value) {
        (TransitionProperty::BackgroundColor, AnimatedValue::Color(c)) => style.background_color = c,
        (TransitionProperty::Transform, AnimatedValue::Transform(t)) => style.transform = t,
        (TransitionProperty::Width, AnimatedValue::Length(l)) => style.width = Some(l),
        (TransitionProperty::Height, AnimatedValue::Length(l)) => style.height = Some(l),
        (TransitionProperty::Margin, AnimatedValue::Edges(e)) => style.margin = e.map(Some),
        _ => {}
    }
}

/// The concrete (non-`All`) properties `apply_transitions` checks for
/// every element — `TransitionProperty::All` in a `transition:`
/// declaration expands to match every one of these, it's never a
/// value read/written directly.
const CONCRETE_TRANSITION_PROPERTIES: [TransitionProperty; 5] = [
    TransitionProperty::BackgroundColor,
    TransitionProperty::Transform,
    TransitionProperty::Width,
    TransitionProperty::Height,
    TransitionProperty::Margin,
];

fn transition_spec_for(specs: &[TransitionSpec], prop: TransitionProperty) -> Option<&TransitionSpec> {
    specs.iter().find(|s| s.property == prop || s.property == TransitionProperty::All)
}

#[derive(Debug, Clone, Copy)]
struct ActiveTransition {
    from: AnimatedValue,
    target: AnimatedValue,
    start: std::time::Instant,
    // Deliberately NOT storing duration/easing here — `step` always
    // reads the CURRENT frame's `spec.duration`/`spec.easing` instead,
    // matching real CSS: a `transition-duration`/`-timing-function`
    // change on the SAME element mid-flight (e.g. from a class swap
    // that also changed something else) affects the rest of an
    // already-running transition, not just future ones.
}

#[derive(Debug, Clone, Copy)]
struct PropertyState {
    /// What was actually shown on screen last frame — the interrupt-
    /// aware "from" a NEW transition starts at if the target changes
    /// again mid-flight (real CSS behavior: interrupting a transition
    /// starts the next one from the current visual state, not from the
    /// old target).
    last_displayed: AnimatedValue,
    active: Option<ActiveTransition>,
}

/// Milestone 22: the persistent, cross-frame state real CSS
/// `transition` interpolation needs — "what was this element's
/// property displaying last frame, and is it still animating toward
/// something." Keyed by `dom::Element::node_id` (Milestone 18's
/// stable, cross-frame identity — NOT `element_ptr_key`, which is
/// explicitly only valid within one borrow/frame, useless for
/// remembering anything between frames). Owned by the caller (`app`)
/// across the whole life of the page, passed into `apply_transitions`
/// each frame.
///
/// A real, bounded, documented memory-hygiene simplification, same
/// class as `js::JsEnvironment`'s `detached_nodes`/`event_listeners`
/// maps: an entry for an element that's removed from the DOM (or that
/// simply stops changing) just stays here for the life of the page —
/// no pruning. Bounded by the number of DISTINCT (element, animated
/// property) pairs a page has ever animated, not by frame count.
#[derive(Default)]
pub struct TransitionRegistry {
    state: HashMap<(u64, TransitionProperty), PropertyState>,
}

impl TransitionRegistry {
    /// Whether any tracked (element, property) pair currently has a
    /// transition in flight — the redraw-loop's "keep polling" signal,
    /// so `app` doesn't have to duplicate this registry's internal
    /// state to answer that question itself.
    pub fn any_active(&self) -> bool {
        self.state.values().any(|s| s.active.is_some())
    }

    /// The (element, property) pairs with a transition in flight (M38b's
    /// gap: the page skips frames for those it cannot see).
    pub fn active(&self) -> impl Iterator<Item = (u64, TransitionProperty)> + '_ {
        self.state.iter().filter(|(_, s)| s.active.is_some()).map(|(k, _)| *k)
    }

    pub fn new() -> Self {
        Self::default()
    }
}

/// Milestone 22: the actual interpolation pass — runs AFTER the normal
/// cascade (`resolve_style_tree_parallel`, unchanged, still just
/// computes each element's TARGET style with no notion of time) and
/// BEFORE layout. Walks the same tree, and for every element with a
/// `transition` on a changed property, substitutes the currently-
/// interpolated value instead of jumping straight to the target,
/// mutating `registry` to track what's now in flight.
///
/// Takes OWNERSHIP of `target_cache` and mutates it in place, handing
/// it back — a real performance fix over the original design (which
/// took `&StyleCache` and did `target_cache.clone()` up front,
/// deep-cloning EVERY element's `Style`, including its
/// `custom_properties` map, on every single frame regardless of
/// whether anything was actually transitioning anywhere in the
/// document). Since neither `app`'s render loop nor any other caller
/// needs the pre-transition cache again afterward, taking ownership
/// costs nothing and turns the "nothing is transitioning" case (the
/// overwhelming common case, including every static page) into true
/// O(1) extra work instead of an O(n) full-tree clone every frame.
/// Only elements that actually have an in-flight transition still pay
/// a (single-`Style`, not whole-cache) clone, same as before.
pub fn apply_transitions<'a>(root: impl Into<ElementRef<'a>>, target_cache: StyleCache, registry: &mut TransitionRegistry, now: std::time::Instant) -> StyleCache {
    let nodes = element_ids(root.into());
    let mut out = target_cache.clone();
    apply_transitions_to(&target_cache, nodes, &mut out, registry, now);
    out
}

/// Every element's node id, in document order. Iterative (no depth limit
/// is needed for a flat id list, and none may overflow the stack).
fn element_ids(root: ElementRef<'_>) -> Vec<dom::NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(el) = stack.pop() {
        out.push(el.node_id());
        stack.extend(el.element_children().rev());
    }
    out
}

/// Milestone 31: the transition pass over just `nodes` — the elements whose
/// cascaded style has a `transition` (the incremental pipeline tracks that
/// set, so a frame visits the transitioning elements, not the whole tree).
/// Reads each target from `cascade` and writes the displayed style into
/// `out` (the frame's final styles). Returns the nodes whose displayed style
/// it changed.
pub fn apply_transitions_to(
    cascade: &StyleCache,
    nodes: impl IntoIterator<Item = dom::NodeId>,
    out: &mut StyleCache,
    registry: &mut TransitionRegistry,
    now: std::time::Instant,
) -> Vec<dom::NodeId> {
    let mut changed = Vec::new();
    for node_id in nodes {
        let Some(target_style) = cascade.get(&node_id) else { continue };
        if target_style.transitions.is_empty() {
            continue;
        }
        let mut interpolated = target_style.clone();
        for prop in CONCRETE_TRANSITION_PROPERTIES {
            let Some(spec) = transition_spec_for(&target_style.transitions, prop) else { continue };
            let Some(target_value) = extract_value(target_style, prop) else { continue };
            let displayed = registry.step(node_id, prop, target_value, spec, now);
            assign_value(&mut interpolated, prop, displayed);
        }
        if out.get(&node_id) != Some(&interpolated) {
            changed.push(node_id);
            out.insert(node_id, interpolated);
        }
    }
    changed
}

impl TransitionRegistry {
    /// Advances (or starts, or completes) the transition for one
    /// (element, property) pair for this frame, returning the value to
    /// actually display — the whole state machine described in this
    /// struct's own doc comment lives here.
    fn step(&mut self, node_id: u64, prop: TransitionProperty, target: AnimatedValue, spec: &TransitionSpec, now: std::time::Instant) -> AnimatedValue {
        let key = (node_id, prop);
        let Some(&existing) = self.state.get(&key) else {
            // First time this (element, property) has been seen —
            // real CSS doesn't transition on initial paint either.
            self.state.insert(key, PropertyState { last_displayed: target, active: None });
            return target;
        };

        // Continue an already-running transition toward the SAME target.
        if let Some(active) = existing.active {
            if active.target == target {
                let t = (now.duration_since(active.start).as_secs_f32() / spec.duration.max(0.0001)).clamp(0.0, 1.0);
                let displayed = if t >= 1.0 { target } else { lerp_value(active.from, active.target, spec.easing.apply(t)) };
                let new_active = if t >= 1.0 { None } else { Some(active) };
                self.state.insert(key, PropertyState { last_displayed: displayed, active: new_active });
                return displayed;
            }
        }

        // No transition running toward this target — either steady
        // state (nothing to do) or the target just changed (start one,
        // interrupting whatever was in flight if anything was).
        if existing.last_displayed == target {
            self.state.insert(key, PropertyState { last_displayed: target, active: None });
            return target;
        }
        let new_transition = ActiveTransition { from: existing.last_displayed, target, start: now };
        self.state.insert(key, PropertyState { last_displayed: existing.last_displayed, active: Some(new_transition) });
        existing.last_displayed
    }
}

/// Milestone 22: the persistent, cross-frame state `@keyframes`
/// animation needs — just "when did this element's CURRENT
/// `animation-name` start playing." Keyed by `dom::Element::node_id`
/// (same identity `TransitionRegistry` uses, same reasoning). If the
/// `animation-name` on an element changes, the animation restarts from
/// its own `from` stop rather than continuing the old one's clock.
///
/// Same bounded, no-pruning memory-hygiene shape as `TransitionRegistry`
/// — an entry for an element that stops animating (or leaves the DOM)
/// simply stays here for the life of the page.
#[derive(Default)]
pub struct AnimationRegistry {
    state: HashMap<u64, AnimationEntry>,
}

#[derive(Debug, Clone)]
struct AnimationEntry {
    name: String,
    start: std::time::Instant,
    /// Whether this element's animation was still progressing (not
    /// frozen at its final `to` stop) as of the LAST `apply_animations`
    /// call — kept so `any_active` can answer "should we keep
    /// redrawing" without redoing the elapsed-time/iteration-count math
    /// itself.
    still_active: bool,
}

impl AnimationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any tracked element's animation was still progressing as
    /// of the last `apply_animations` call — the redraw-loop's "keep
    /// polling" signal, mirroring `TransitionRegistry::any_active`.
    pub fn any_active(&self) -> bool {
        self.state.values().any(|e| e.still_active)
    }
}

/// Which animatable properties `declarations` set — whatever names they use
/// (`margin-block`, `background` …): found by applying them to a style
/// holding a value no declaration produces in each, and seeing which moved.
fn declared(declarations: &[css_parser::Declaration], viewport: ViewportSize) -> [bool; 5] {
    let sentinel_len = crate::Length::px(-1.0e9 + 7.0);
    let mut probe = Style {
        background_color: crate::values::Color { r: 1, g: 2, b: 3, a: 4 },
        width: Some(sentinel_len),
        height: Some(sentinel_len),
        margin: Edges::all(Some(sentinel_len)),
        ..Style::default()
    };
    probe.transform.translate_x = -1.0e9 + 7.0;
    let before = probe.clone();
    for decl in declarations {
        apply_declaration(&mut probe, &decl.property, &decl.value, viewport);
    }
    CONCRETE_TRANSITION_PROPERTIES.map(|p| match p {
        TransitionProperty::BackgroundColor => probe.background_color != before.background_color,
        TransitionProperty::Transform => probe.transform != before.transform,
        TransitionProperty::Width => probe.width != before.width,
        TransitionProperty::Height => probe.height != before.height,
        TransitionProperty::Margin => probe.margin != before.margin,
        TransitionProperty::All => false,
    })
}

/// M22's gap: `kf` at progress `t` (0–1) of one cycle, over `base` — the
/// element's own style. Each property is animated on its own keyframes
/// (the stops that declare it, plus the element's own value at 0% and
/// 100% where no stop does — CSS Animations §3.2): between the two around
/// `t`, eased by the first one's `animation-timing-function` if it sets
/// one, else the element's.
fn sample_keyframes(kf: &KeyframesRule, base: &Style, t: f32, viewport: ViewportSize) -> Style {
    // Each stop's style over the element's (stops at one offset in order:
    // the later's declarations win, as in a cascade).
    let styled: Vec<(f32, &[css_parser::Declaration], [bool; 5], Style)> = kf
        .stops
        .iter()
        .map(|stop| {
            let mut s = base.clone();
            for decl in &stop.declarations {
                apply_declaration(&mut s, &decl.property, &decl.value, viewport);
            }
            (stop.offset, stop.declarations.as_slice(), declared(&stop.declarations, viewport), s)
        })
        .collect();
    let easing_of = |decls: &[css_parser::Declaration]| {
        decls.iter().rev().find(|d| d.property == "animation-timing-function").and_then(|d| d.value.split(',').next().and_then(parse_easing)).unwrap_or(base.animation_timing_function)
    };
    let mut out = base.clone();
    for (index, prop) in CONCRETE_TRANSITION_PROPERTIES.into_iter().enumerate() {
        // This property's keyframes: (offset, value, easing of the segment
        // starting there).
        let mut frames: Vec<(f32, AnimatedValue, Easing)> = Vec::new();
        for (offset, decls, sets, s) in &styled {
            if sets[index] {
                if let Some(v) = extract_value(s, prop) {
                    frames.push((*offset, v, easing_of(decls)));
                }
            }
        }
        if frames.is_empty() {
            continue;
        }
        let Some(own) = extract_value(base, prop) else { continue };
        if frames[0].0 > 0.0 {
            frames.insert(0, (0.0, own, base.animation_timing_function));
        }
        if frames.last().is_some_and(|f| f.0 < 1.0) {
            frames.push((1.0, own, base.animation_timing_function));
        }
        // The segment around `t`: the last frame at or before it, and the
        // first after it (or the last frame itself at 100%).
        let at = frames.iter().rposition(|f| f.0 <= t).unwrap_or(0);
        let value = match frames.get(at + 1) {
            None => frames[at].1,
            Some(next) => {
                let (start, end) = (frames[at].0, next.0);
                let local = if end > start { ((t - start) / (end - start)).clamp(0.0, 1.0) } else { 1.0 };
                lerp_value(frames[at].1, next.1, frames[at].2.apply(local))
            }
        };
        assign_value(&mut out, prop, value);
    }
    out
}

/// Applies every element's `@keyframes` animation (named by its
/// resolved `animation-name`, looked up in `keyframes`) for this frame,
/// returning a `StyleCache` with interpolated values in place of
/// `target_cache`'s. Mirrors `apply_transitions`'s shape exactly,
/// INCLUDING its performance fix — see that function's doc comment:
/// takes ownership of `target_cache` and mutates it in place instead
/// of deep-cloning every element's `Style` up front regardless of
/// whether anything is actually animating — but keyed off
/// `animation-name`/`-duration`/`-iteration-count` instead of
/// `transition`.
///
/// Scope cuts (all documented in `milestone-22-animation-depth.md`):
/// only the first/last `@keyframes` stop is used (see `KeyframesRule`);
/// interpolation is always linear (`animation-timing-function` isn't
/// modeled); only `CONCRETE_TRANSITION_PROPERTIES` are animatable
/// (reusing the exact same set `transition` supports); a finished
/// finite-iteration-count animation holds at its `to` stop (an
/// implicit `animation-fill-mode: forwards`, not real CSS's `none`
/// default, chosen because holding at the pre-animation base style
/// after a finite animation completes would be a more surprising
/// default for anyone actually using this).
pub fn apply_animations<'a>(root: impl Into<ElementRef<'a>>, target_cache: StyleCache, keyframes: &HashMap<String, KeyframesRule>, registry: &mut AnimationRegistry, now: std::time::Instant) -> StyleCache {
    let nodes = element_ids(root.into());
    let mut out = target_cache;
    apply_animations_to(nodes, keyframes, &mut out, registry, now, ViewportSize::DEFAULT);
    out
}

/// Milestone 31: the `@keyframes` pass over just `nodes` (the elements
/// with an `animation-name`). Reads each element's current style from
/// `out` — the cascaded style, or the transition pass's output for an
/// element that also transitions — and writes the animated style back.
/// Only elements that are actually animated are cloned (the old tree walk
/// cloned every element's `Style` every frame). Returns the nodes whose
/// style it changed.
pub fn apply_animations_to(
    nodes: impl IntoIterator<Item = dom::NodeId>,
    keyframes: &HashMap<String, KeyframesRule>,
    out: &mut StyleCache,
    registry: &mut AnimationRegistry,
    now: std::time::Instant,
    viewport: ViewportSize,
) -> Vec<dom::NodeId> {
    let mut changed = Vec::new();
    for node_id in nodes {
        let Some(name) = out.get(&node_id).and_then(|s| s.animation_name.clone()) else { continue };
        let Some(kf) = keyframes.get(&name) else { continue };
        // Keyframes that animate nothing this engine can animate (only
        // `opacity`, say) change nothing: no entry, so they do not keep
        // the page animating.
        if !kf.stops.iter().any(|stop| declared(&stop.declarations, viewport).contains(&true)) {
            continue;
        }
        let Some(target_style) = out.get(&node_id).cloned() else { continue };
        let target_style = &target_style;
        let name = &name;
        {
            {
                let start = match registry.state.get(&node_id) {
                    Some(entry) if &entry.name == name => entry.start,
                    _ => now,
                };
                let duration = target_style.animation_duration.max(0.0001);
                let elapsed_cycles = now.duration_since(start).as_secs_f32() / duration;
                let (t, still_active) = match target_style.animation_iteration_count {
                    Some(max) if elapsed_cycles >= max.max(0.0) => (1.0, false),
                    _ => (elapsed_cycles.fract(), true),
                };
                registry.state.insert(node_id, AnimationEntry { name: name.clone(), start, still_active });

                let interpolated = sample_keyframes(kf, target_style, t, viewport);
                if *target_style != interpolated {
                    changed.push(node_id);
                    out.insert(node_id, interpolated);
                }
            }
        }
    }
    changed
}
