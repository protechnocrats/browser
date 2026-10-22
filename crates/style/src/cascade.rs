//! The cascade itself: matching an element against a stylesheet's
//! rules (via `crate::selectors`), resolving specificity/source-order/
//! `!important` priority, applying the winning declarations (via
//! `crate::declarations`) in the right order, and the parallel
//! whole-tree entry point (`resolve_style_tree_parallel`) `app`'s
//! render loop actually calls every frame.

use crate::declarations::{apply_declaration, apply_font_size};
use crate::length::{LengthContext, ViewportSize};
use crate::selectors::matches_selector;
use crate::style::{Style, StyleCache};
use css_parser::{parse_inline_style, Declaration, Stylesheet};
use dom::ElementRef;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Viewport width assumed when a `Style` is resolved outside a full
/// layout pass (the back-compat `resolve_style` entry point) — only
/// matters for `@media (min-width/max-width)` evaluation. A layout pass
/// threads the real viewport width through
/// `resolve_style_tree_parallel`.
pub const DEFAULT_VIEWPORT_WIDTH: f32 = 1920.0;
/// Viewport height assumed when none is given (`vh` units).
pub const DEFAULT_VIEWPORT_HEIGHT: f32 = 1080.0;

/// Everything selector matching needs to know about an element beyond
/// the element itself: where it sits among its siblings (for structural
/// pseudo-classes and sibling combinators), its ancestor chain (for
/// descendant/child combinators), and the viewport (for `@media` and
/// viewport units).
#[derive(Clone, Copy)]
pub struct MatchContext<'a> {
    /// Closest parent first, root last.
    pub ancestors: &'a [ElementRef<'a>],
    /// The immediate parent's already-resolved style — for inheritance.
    pub parent_style: Option<&'a Style>,
    /// Element siblings strictly before this one, in document order
    /// (so `.last()` is the immediately-preceding sibling).
    pub previous_siblings: &'a [ElementRef<'a>],
    /// Whether an element sibling follows this one.
    pub next_sibling_exists: bool,
    /// This element's 0-based index among its element siblings.
    pub sibling_index: usize,
    /// Total number of element siblings, including this one.
    pub sibling_count: usize,
    pub viewport_width: f32,
    /// Milestone 33: for `vh`/`vmin`/`vmax`.
    pub viewport_height: f32,
    /// M4's gap: what the user is doing, for `:hover`, `:active`, `:focus`.
    pub interaction: &'a Interaction,
}

/// What the user is doing to the page, for `:hover`, `:active`, `:focus`
/// and `:focus-within` (M4's gap). Each is an element and its ancestors,
/// the element first — an ancestor of a hovered element is hovered too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interaction {
    /// The element under the pointer and its ancestors.
    pub hover: Vec<dom::NodeId>,
    /// The element being pressed and its ancestors.
    pub active: Vec<dom::NodeId>,
    /// The focused element and its ancestors (`:focus` is the first).
    pub focus: Vec<dom::NodeId>,
}

impl Interaction {
    /// Nobody is doing anything.
    pub const NONE: Interaction = Interaction { hover: Vec::new(), active: Vec::new(), focus: Vec::new() };

    /// [`Self::NONE`], for contexts that borrow one.
    pub fn none() -> &'static Interaction {
        static NONE: Interaction = Interaction::NONE;
        &NONE
    }

    /// The elements whose state differs between `self` and `other`.
    pub fn changed(&self, other: &Interaction) -> Vec<dom::NodeId> {
        let differ = |a: &[dom::NodeId], b: &[dom::NodeId]| a.iter().filter(|n| !b.contains(n)).chain(b.iter().filter(|n| !a.contains(n))).copied().collect::<Vec<_>>();
        let mut out = differ(&self.hover, &other.hover);
        out.extend(differ(&self.active, &other.active));
        // `:focus` is the first entry only; `:focus-within` the whole path.
        if self.focus.first() != other.focus.first() {
            out.extend(self.focus.first().into_iter().chain(other.focus.first()).copied());
        }
        out.extend(differ(&self.focus, &other.focus));
        out.sort_unstable();
        out.dedup();
        out
    }
}

impl<'a> MatchContext<'a> {
    /// A context for an element considered in isolation: no siblings,
    /// treated as an only child, default viewport. Used by the
    /// back-compat `resolve_style` entry point.
    pub fn isolated(ancestors: &'a [ElementRef<'a>], parent_style: Option<&'a Style>) -> Self {
        MatchContext {
            ancestors,
            parent_style,
            previous_siblings: &[],
            next_sibling_exists: false,
            sibling_index: 0,
            sibling_count: 1,
            viewport_width: DEFAULT_VIEWPORT_WIDTH,
            viewport_height: DEFAULT_VIEWPORT_HEIGHT,
            interaction: Interaction::none(),
        }
    }
}

/// Resolves `element`'s computed style via the full cascade.
///
/// - `ancestors`: closest parent first, root last — used for
///   descendant-selector matching. Pass `&[]` for a root element.
/// - `parent_style`: the immediate parent's already-resolved style —
///   used for inheritance. Pass `None` for a root element.
/// - `stylesheet`: from `<style>` blocks — pass `&Stylesheet::default()`
///   for inline-only behavior.
///
/// This entry point considers the element in isolation (see
/// [`MatchContext::isolated`]) — sibling combinators, structural
/// pseudo-classes, and viewport-dependent `@media` need the richer
/// [`resolve_style_in_context`] (which a full layout pass uses via
/// [`resolve_style_tree_parallel`]).
/// For an owned tree (tests, standalone elements); a live document's
/// elements go through [`resolve_style_in_context`].
pub fn resolve_style<'a>(
    element: impl Into<ElementRef<'a>>,
    ancestors: &[&'a dom::Element],
    parent_style: Option<&Style>,
    stylesheet: &Stylesheet,
) -> Style {
    let ancestors: Vec<ElementRef<'a>> = ancestors.iter().map(|a| ElementRef::from(*a)).collect();
    resolve_style_in_context(element, &MatchContext::isolated(&ancestors, parent_style), stylesheet)
}

// How many rule indexes this thread has built (tests).
#[cfg(test)]
thread_local! {
    pub(crate) static RULE_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// An index over a [`Stylesheet`]'s rules, bucketed by each rule's
/// subject compound's id/class/tag (the same "rule hash" technique
/// real engines use) so that resolving one element's style only has
/// to test the small set of rules that could plausibly match it,
/// instead of every rule in the sheet. Building the index is `O(R)`
/// (once per stylesheet); looking up one element's candidates is
/// `O(matching bucket sizes)` instead of `O(R)`.
///
/// A rule lands in a bucket if ANY of its selectors' subject compound
/// has an id, class, or tag — using the most selective key available
/// per selector (id, then a class, then tag). A selector whose subject
/// is bare (`*`, `[attr]`, a lone pseudo-class) can't be bucketed at
/// all, so its rule goes in `universal` and is tested against every
/// element, same as before indexing existed. This narrows the
/// candidate set without changing which rules can match — the exact
/// [`matches_selector`] check still runs on every candidate, so
/// indexing is purely a performance pre-filter, not a second source of
/// truth for matching.
pub(crate) struct RuleIndex {
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    /// Milestone 40b: rules whose subject is a well-known type, at the
    /// type's atom — an index, not a string hash, per element.
    by_well_known_tag: Vec<Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl RuleIndex {
    pub(crate) fn build(stylesheet: &Stylesheet) -> Self {
        #[cfg(test)]
        RULE_INDEX_BUILDS.with(|n| n.set(n.get() + 1));
        let mut index = RuleIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_well_known_tag: Vec::new(),
            by_tag: HashMap::new(),
            universal: Vec::new(),
        };
        for (rule_idx, rule) in stylesheet.rules.iter().enumerate() {
            let mut bucketed = false;
            for sel in &rule.selectors {
                let Some(subject) = sel.compounds.last() else { continue };
                if let Some(id) = &subject.id {
                    index.by_id.entry(id.clone()).or_default().push(rule_idx);
                    bucketed = true;
                } else if let Some(class) = subject.classes.first() {
                    index.by_class.entry(class.clone()).or_default().push(rule_idx);
                    bucketed = true;
                } else if let Some(tag) = &subject.tag {
                    match tag.atom().and_then(dom::Atom::static_index) {
                        Some(i) => {
                            if index.by_well_known_tag.len() <= i {
                                index.by_well_known_tag.resize_with(i + 1, Vec::new);
                            }
                            index.by_well_known_tag[i].push(rule_idx);
                        }
                        None => index.by_tag.entry(tag.to_string()).or_default().push(rule_idx),
                    }
                    bucketed = true;
                }
            }
            if !bucketed {
                index.universal.push(rule_idx);
            }
        }
        index
    }

    /// Candidate rule indices for `element` — a superset of the rules
    /// that can actually match it, POSSIBLY WITH DUPLICATES (a rule
    /// matched via more than one bucket — e.g. a selector list like
    /// `.foo, div { ... }` against `<div class="foo">` — appears once
    /// per bucket it's indexed under). The caller sorts + dedups.
    /// Deliberately a plain `Vec`, not a `HashSet`: this runs once per
    /// element per restyle, and for the typical small candidate count,
    /// hashing into a set is real overhead a sort-and-dedup (which the
    /// caller already needs to do anyway, to get rules in source
    /// order) avoids entirely.
    fn candidates(&self, element: ElementRef<'_>) -> Vec<usize> {
        let mut out = Vec::new();
        out.extend(self.universal.iter().copied());
        if let Some(id) = element.attr_atom(dom::atoms::ID) {
            if let Some(rules) = self.by_id.get(id) {
                out.extend(rules.iter().copied());
            }
        }
        if let Some(classes) = element.attr_atom(dom::atoms::CLASS) {
            for class in classes.split_whitespace() {
                if let Some(rules) = self.by_class.get(class) {
                    out.extend(rules.iter().copied());
                }
            }
        }
        match element.well_known_tag() {
            Some(tag) => {
                let i = tag.static_index().expect("a well-known atom");
                if let Some(rules) = self.by_well_known_tag.get(i) {
                    out.extend(rules.iter().copied());
                }
            }
            // A tag that is not well-known: a selector naming it is not
            // well-known either, so it is in `by_tag`.
            None => {
                if !self.by_tag.is_empty() {
                    if let Some(rules) = self.by_tag.get(element.tag()) {
                        out.extend(rules.iter().copied());
                    }
                }
            }
        }
        out
    }
}

pub fn resolve_style_in_context<'a>(element: impl Into<ElementRef<'a>>, ctx: &MatchContext, stylesheet: &Stylesheet) -> Style {
    let element = element.into();
    // Standalone/test entry point — builds a throwaway index for this
    // one call. The hot bulk path (`resolve_style_tree_parallel`)
    // builds the index once and calls `resolve_style_in_context_indexed`
    // directly instead of going through here.
    let index = RuleIndex::build(stylesheet);
    let rules = Rules { stylesheet, index: &index, inline_styles: &InlineStyleCache::default(), interaction: Interaction::none() };
    cascade_element(element, ctx, &rules, &CascadeSink::new()).element
}

/// What the cascade matches elements against: the stylesheet, its
/// [`RuleIndex`], and inline `style` attributes parsed on earlier passes.
#[derive(Clone, Copy)]
pub(crate) struct Rules<'a> {
    pub stylesheet: &'a Stylesheet,
    pub index: &'a RuleIndex,
    pub inline_styles: &'a InlineStyleCache,
    /// M4's gap: for a whole-tree cascade's root context.
    pub interaction: &'a Interaction,
}

/// Parsed inline `style` attributes, keyed by the attribute's text, kept
/// across restyles by [`crate::StyleEngine`] so an unchanged attribute is
/// not parsed again — and identical attributes (a table's repeated
/// `style="padding: 2px"`) share one parse. Bounded: see
/// [`InlineStyleCache::merge`].
#[derive(Default)]
pub(crate) struct InlineStyleCache {
    parsed: HashMap<Arc<str>, Arc<[Declaration]>>,
}

/// Past this many distinct attribute texts the cache starts over, so a page
/// that writes a new `style` every frame (an animation from script) costs
/// bounded memory.
pub(crate) const MAX_INLINE_STYLE_CACHE: usize = 8192;

impl InlineStyleCache {
    fn get(&self, text: &str) -> Option<&Arc<[Declaration]>> {
        self.parsed.get(text)
    }

    /// Adds what one cascade pass parsed.
    pub(crate) fn merge(&mut self, parsed: Vec<ParsedInlineStyle>) {
        if self.parsed.len() + parsed.len() > MAX_INLINE_STYLE_CACHE {
            self.parsed.clear();
        }
        self.parsed.extend(parsed.into_iter().take(MAX_INLINE_STYLE_CACHE));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.parsed.len()
    }
}

/// An inline `style` attribute's text and its parsed declarations.
pub(crate) type ParsedInlineStyle = (Arc<str>, Arc<[Declaration]>);

/// Where a (parallel) cascade pass puts its results: one shard per `rayon`
/// worker, so workers never wait on each other's inserts (a single shared
/// map behind one lock was taken twice per element by every worker), and
/// one map is built at the end.
pub(crate) struct CascadeSink {
    styles: Vec<Mutex<Vec<(dom::NodeId, Style)>>>,
    inline: Vec<Mutex<Vec<ParsedInlineStyle>>>,
    /// How many elements the cascade ran for (Milestone 31's counters).
    pub cascaded: AtomicUsize,
}

impl CascadeSink {
    pub(crate) fn new() -> CascadeSink {
        // Shard 0 is for callers outside the pool (the page thread).
        let shards = rayon::current_num_threads() + 1;
        CascadeSink {
            styles: (0..shards).map(|_| Mutex::new(Vec::new())).collect(),
            inline: (0..shards).map(|_| Mutex::new(Vec::new())).collect(),
            cascaded: AtomicUsize::new(0),
        }
    }

    fn shard(&self) -> usize {
        rayon::current_thread_index().map_or(0, |i| (i + 1) % self.styles.len())
    }

    fn push(&self, node: dom::NodeId, style: Style) {
        // Uncontended: only this worker writes this shard. Poison-tolerant
        // like every lock in the engine.
        self.styles[self.shard()].lock().unwrap_or_else(PoisonError::into_inner).push((node, style));
    }

    fn push_inline(&self, text: &str, parsed: Arc<[Declaration]>) {
        self.inline[self.shard()].lock().unwrap_or_else(PoisonError::into_inner).push((Arc::from(text), parsed));
    }

    /// The styles, as one map, and the inline styles parsed on the way.
    pub(crate) fn finish(self) -> (StyleCache, Vec<ParsedInlineStyle>) {
        let shards: Vec<Vec<(dom::NodeId, Style)>> = self.styles.into_iter().map(|m| m.into_inner().unwrap_or_else(PoisonError::into_inner)).collect();
        let mut cache = StyleCache::with_capacity_and_hasher(shards.iter().map(Vec::len).sum(), Default::default());
        for shard in shards {
            cache.extend(shard);
        }
        let inline = self.inline.into_iter().flat_map(|m| m.into_inner().unwrap_or_else(PoisonError::into_inner)).collect();
        (cache, inline)
    }
}

/// An element's style and, when they generate a box, its `::before` and
/// `::after` styles (Milestone 33).
pub(crate) struct Cascaded {
    pub element: Style,
    // Boxed: this is returned into the recursive tree walk, whose stack
    // frames must stay small (the deep-tree guard test).
    pub before: Option<Box<Style>>,
    pub after: Option<Box<Style>>,
}

/// The inherited starting point of an element (or pseudo-element) whose
/// parent's style is `parent`.
fn inherit_from(parent: &Style) -> Style {
    Style {
        color: parent.color,
        text: inherit_text(&parent.text),
        font_size: parent.font_size,
        root_font_size: parent.root_font_size,
        visibility: parent.visibility,
        table_borders: parent.table_borders,
        text_align: parent.text_align,
        white_space: parent.white_space,
        // Custom properties inherit: share the parent's map (an `Arc`
        // clone, O(1)). It is copied only if this element declares one of
        // its own — cloning it for every element cost O(elements ×
        // custom properties) on sites with hundreds of `--vars` on `:root`.
        custom_properties: Arc::clone(&parent.custom_properties),
        ..Style::default()
    }
}

/// The parent's text style for a child: shared, unless decorations must
/// record what is inherited.
fn inherit_text(parent: &Arc<crate::text::TextStyle>) -> Arc<crate::text::TextStyle> {
    if parent.decoration.lines == parent.inherited_lines {
        return Arc::clone(parent);
    }
    let mut t = (**parent).clone();
    t.inherited_lines = t.decoration.lines;
    Arc::new(t)
}

struct Candidate<'a> {
    origin: css_parser::CascadeOrigin,
    specificity: (usize, usize, usize),
    source_order: usize,
    decl: &'a Declaration,
}

/// Milestone 40b: [`crate::values::initial_display`] for `element` — for a
/// well-known tag, a table read at its atom (the string match runs once per
/// well-known name, not once per element).
fn initial_display_of(element: ElementRef<'_>) -> crate::values::Display {
    static BY_ATOM: std::sync::OnceLock<Vec<crate::values::Display>> = std::sync::OnceLock::new();
    match element.well_known_tag().and_then(dom::Atom::static_index) {
        Some(i) => BY_ATOM.get_or_init(|| dom::WELL_KNOWN_NAMES.iter().map(|name| crate::values::initial_display(name)).collect())[i],
        None => crate::values::initial_display(element.tag()),
    }
}

/// Matching once, for the element and for its two box-generating
/// pseudo-elements. Never inlined into the recursive walk (stack size).
#[inline(never)]
pub(crate) fn cascade_element(element: ElementRef<'_>, ctx: &MatchContext, rules: &Rules, sink: &CascadeSink) -> Cascaded {
    let Rules { stylesheet, index, inline_styles, .. } = *rules;
    let mut style = match ctx.parent_style {
        Some(parent) => inherit_from(parent),
        None => Style::default(),
    };
    style.display = initial_display_of(element);
    // `<input type=hidden>` is never rendered (HTML §15.3.1), nor is any
    // element with the `hidden` attribute.
    if element.attr_atom(dom::atoms::HIDDEN).is_some() || (element.is(dom::atoms::INPUT) && element.attr_atom(dom::atoms::TYPE).is_some_and(|t| t.eq_ignore_ascii_case("hidden"))) {
        style.display = crate::values::Display::None;
    }

    // [element, ::before, ::after]
    let mut candidates: [Vec<Candidate>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut candidate_rules: Vec<usize> = index.candidates(element);
    candidate_rules.sort_unstable();
    candidate_rules.dedup();
    for source_order in candidate_rules {
        let rule = &stylesheet.rules[source_order];
        if let Some(query) = &rule.media {
            if !query.matches(ctx.viewport_width) {
                continue;
            }
        }
        let mut best: [Option<(usize, usize, usize)>; 3] = [None; 3];
        for selector in &rule.selectors {
            let target = match crate::selectors::subject_pseudo_element(selector) {
                None => 0,
                Some("before") => 1,
                Some("after") => 2,
                Some(_) => continue, // ::first-line and the rest generate no box here
            };
            let matched = if target == 0 {
                matches_selector(selector, element, ctx)
            } else {
                crate::selectors::matches_selector_ignoring_pseudo(selector, element, ctx)
            };
            if matched {
                best[target] = best[target].max(Some(selector.specificity()));
            }
        }
        for (target, specificity) in best.into_iter().enumerate() {
            let Some(specificity) = specificity else { continue };
            for decl in &rule.declarations {
                candidates[target].push(Candidate { origin: rule.origin, specificity, source_order, decl });
            }
        }
    }
    let [own, before, after] = candidates;

    let inline_decls: Arc<[Declaration]> = match element.attr_atom(dom::atoms::STYLE) {
        None => Arc::from(Vec::new()),
        Some(text) => match inline_styles.get(text) {
            Some(parsed) => Arc::clone(parsed),
            None => {
                let parsed: Arc<[Declaration]> = Arc::from(parse_inline_style(text));
                sink.push_inline(text, Arc::clone(&parsed));
                parsed
            }
        },
    };
    let viewport = ViewportSize::new(ctx.viewport_width, ctx.viewport_height);
    apply_cascade(&mut style, own, &inline_decls, viewport, ctx.parent_style.is_none(), Some((element, ctx.ancestors)));

    let pseudo = |candidates: Vec<Candidate>| {
        if candidates.is_empty() {
            return None;
        }
        let mut pseudo = inherit_from(&style);
        pseudo.display = crate::values::Display::Inline;
        pseudo.root_font_size = style.root_font_size;
        apply_cascade(&mut pseudo, candidates, &[], viewport, false, None);
        // Only a pseudo-element with `content` (other than none/normal)
        // generates a box.
        let text = pseudo.content.as_deref().and_then(|raw| crate::content::resolve_content(raw, element))?;
        if pseudo.display == crate::values::Display::None {
            return None;
        }
        pseudo.content = Some(text);
        Some(Box::new(pseudo))
    };
    let (before, after) = (pseudo(before), pseudo(after));
    style.generates_before = before.is_some();
    style.generates_after = after.is_some();
    Cascaded { element: style, before, after }
}

/// Applies the winning declarations, lowest priority first, onto `style`
/// (which holds the inherited values). `presentational` is the element
/// whose size attributes are presentational hints, if any.
fn apply_cascade(
    style: &mut Style,
    candidates: Vec<Candidate>,
    inline_decls: &[Declaration],
    viewport: ViewportSize,
    is_root: bool,
    presentational: Option<(ElementRef<'_>, &[ElementRef<'_>])>,
) {
    let (mut normal, important): (Vec<Candidate>, Vec<Candidate>) = candidates.into_iter().partition(|c| !c.decl.important);
    // Origin first (Milestone 33): for normal declarations a user-agent
    // rule loses to every author rule; for `!important` it wins.
    let by_priority = |a: &Candidate, b: &Candidate| {
        a.origin.cmp(&b.origin).then(a.specificity.cmp(&b.specificity)).then(a.source_order.cmp(&b.source_order))
    };
    normal.sort_by(by_priority);
    // User-agent declarations come first; presentational hints go after
    // them and before the author's (HTML §15.2).
    let user_agent_count = normal.iter().take_while(|c| c.origin == css_parser::CascadeOrigin::UserAgent).count();
    let mut important = important;
    important.sort_by(by_priority);
    let (ua_important, author_important): (Vec<Candidate>, Vec<Candidate>) =
        important.into_iter().partition(|c| c.origin == css_parser::CascadeOrigin::UserAgent);

    // Final application order (lowest to highest priority): sorted
    // stylesheet-normal (user agent, then author), inline-normal, sorted
    // author-important, inline-important, user-agent-important.
    let ordered: Vec<&Declaration> = normal
        .iter()
        .map(|c| c.decl)
        .chain(inline_decls.iter().filter(|d| !d.important))
        .chain(author_important.iter().map(|c| c.decl))
        .chain(inline_decls.iter().filter(|d| d.important))
        .chain(ua_important.iter().map(|c| c.decl))
        .collect();

    // Pass 1 — custom properties. They cascade independently of and
    // before everything else, so a `var()` in a normal declaration can
    // reference a `--foo` set by a higher-priority rule.
    for decl in &ordered {
        if let Some(name) = decl.property.strip_prefix("--") {
            let resolved = substitute_vars(&decl.value, &style.custom_properties).trim().to_string();
            Arc::make_mut(&mut style.custom_properties).insert(name.trim().to_string(), resolved);
        }
    }

    // Pass 2 — `font-size`, before anything that uses `em`. Every
    // `font-size` declaration is relative to the parent's size (the
    // inherited value), not to a lower-priority declaration's result.
    let parent_font = LengthContext {
        font_size: style.font_size,
        root_font_size: style.root_font_size,
        viewport_width: viewport.width,
        viewport_height: viewport.height,
    };
    for decl in ordered.iter().filter(|d| d.property == "font-size" || d.property == "font") {
        let value = substitute_vars(&decl.value, &style.custom_properties);
        if decl.property == "font" {
            // Milestone 35: the shorthand's size, in cascade order with
            // `font-size`; its other parts apply in pass 3.
            if let Some((_, _, size, _, _)) = crate::text::parse_font_shorthand(&value) {
                apply_font_size(style, size, &parent_font);
            }
        } else {
            apply_font_size(style, &value, &parent_font);
        }
    }
    // `color` early too, so `currentcolor` anywhere means the final
    // colour; and the border and outline colours start as `currentcolor`
    // (their initial value) until a declaration below says otherwise.
    for decl in ordered.iter().filter(|d| d.property == "color") {
        let value = substitute_vars(&decl.value, &style.custom_properties);
        apply_declaration(style, "color", &value, viewport);
    }
    style.outline_color = style.color;
    // The root element defines `rem` for the whole document.
    if is_root {
        style.root_font_size = style.font_size;
    }

    // Pass 3 — everything else, with `var()` substituted; presentational
    // hints between the user agent's declarations and the author's.
    for (i, decl) in ordered.iter().enumerate() {
        if i == user_agent_count {
            apply_presentational_hints(style, presentational, viewport);
        }
        if decl.property.starts_with("--") || decl.property == "font-size" || decl.property == "color" {
            continue;
        }
        let value = substitute_vars(&decl.value, &style.custom_properties);
        apply_declaration(style, &decl.property, &value, viewport);
    }
    if user_agent_count >= ordered.len() {
        apply_presentational_hints(style, presentational, viewport);
    }
}

/// Presentational hints (HTML §15.2): legacy table attributes, and an
/// embedded element's (or table part's) `width` / `height` attributes.
fn apply_presentational_hints(style: &mut Style, presentational: Option<(ElementRef<'_>, &[ElementRef<'_>])>, viewport: ViewportSize) {
    // Presentational hints (HTML §15.2): an embedded element's `width` /
    // `height` attributes are its size unless any CSS rule says otherwise
    // — so they apply before every declaration below.
    if let Some((element, ancestors)) = presentational {
        legacy_table_hints(style, element, ancestors);
        // Milestone 35: `dir="rtl"`/`"ltr"` (HTML §15.3.3).
        if let Some(dir) = element.attr_atom(dom::atoms::DIR) {
            let rtl = if dir.eq_ignore_ascii_case("rtl") {
                Some(true)
            } else if dir.eq_ignore_ascii_case("ltr") {
                Some(false)
            } else {
                None
            };
            if let Some(rtl) = rtl.filter(|r| *r != style.text.rtl) {
                Arc::make_mut(&mut style.text).rtl = rtl;
            }
        }
    }
    use dom::atoms::{CANVAS, EMBED, IFRAME, IMG, OBJECT, SVG, TABLE, TD, TH, VIDEO};
    if let Some(element) = presentational.map(|(e, _)| e).filter(|e| matches!(e.well_known_tag(), Some(IMG | SVG | CANVAS | VIDEO | IFRAME | EMBED | OBJECT | TABLE | TD | TH))) {
        let ctx = LengthContext {
            font_size: style.font_size,
            root_font_size: style.root_font_size,
            viewport_width: viewport.width,
            viewport_height: viewport.height,
        };
        let hint = |name: dom::Atom| {
            let raw = element.attr_atom(name)?.trim();
            // A bare number is pixels (`width="24"`).
            let parsed = match raw.parse::<f32>() {
                Ok(px) => Some(crate::Length::px(px)),
                Err(_) => crate::length::parse_length(raw, &ctx),
            };
            parsed.filter(|l| l.px >= 0.0 && l.percent >= 0.0 && l.px.is_finite())
        };
        if let Some(w) = hint(dom::atoms::WIDTH) {
            style.width = Some(w);
        }
        if let Some(h) = hint(dom::atoms::HEIGHT) {
            style.height = Some(h);
        }
    }

}

/// Milestone 33: the presentational attributes old table markup still
/// relies on (HTML §15.3.8–10): `bgcolor`, `cellspacing`, `cellpadding`,
/// `border`, `align`, `valign`. Like the size hints, they sit below every
/// CSS rule.
fn legacy_table_hints(style: &mut Style, element: ElementRef<'_>, ancestors: &[ElementRef<'_>]) {
    use dom::atoms::{BODY, TABLE, TBODY, TD, TFOOT, TH, THEAD, TR};
    let Some(tag) = element.well_known_tag() else { return }; // every tag below is well-known
    if matches!(tag, BODY | TABLE | TR | TD | TH | THEAD | TBODY | TFOOT) {
        if let Some(c) = element.attr_atom(dom::atoms::BGCOLOR).and_then(|v| crate::color::parse_color(v, style.color)) {
            style.background_color = c;
        }
    }
    let pixels = |v: &str| v.trim().trim_end_matches("px").parse::<f32>().ok().filter(|p| p.is_finite() && *p >= 0.0).map(|p| p.min(1024.0));
    let gray = crate::values::Color { r: 128, g: 128, b: 128, a: 255 };
    match tag {
        TABLE => {
            if let Some(s) = element.attr_atom(dom::atoms::CELLSPACING).and_then(pixels) {
                style.table_borders.spacing = (s, s);
            }
            if let Some(b) = element.attr_atom(dom::atoms::BORDER).map(|v| pixels(v).unwrap_or(1.0)).filter(|b| *b > 0.0) {
                *style.border_mut() = crate::values::Edges::all(crate::values::BorderSpec { width: b, style: crate::values::BorderStyle::Solid, color: Some(gray) });
            }
            if element.attr_atom(dom::atoms::ALIGN).is_some_and(|a| a.eq_ignore_ascii_case("center")) {
                style.margin.left = None;
                style.margin.right = None;
            }
        }
        TD | TH => {
            if let Some(table) = ancestors.iter().find(|a| a.is(dom::atoms::TABLE)) {
                if let Some(p) = table.attr_atom(dom::atoms::CELLPADDING).and_then(pixels) {
                    style.padding = crate::values::Edges::all(crate::Length::px(p));
                }
                if table.attr_atom(dom::atoms::BORDER).is_some_and(|v| pixels(v).unwrap_or(1.0) > 0.0) {
                    *style.border_mut() = crate::values::Edges::all(crate::values::BorderSpec { width: 1.0, style: crate::values::BorderStyle::Solid, color: Some(gray) });
                }
            }
            match element.attr_atom(dom::atoms::ALIGN).map(str::to_ascii_lowercase).as_deref() {
                Some("center") => style.text_align = crate::values::TextAlign::Center,
                Some("right") => style.text_align = crate::values::TextAlign::Right,
                Some("left") => style.text_align = crate::values::TextAlign::Left,
                _ => {}
            }
            match element.attr_atom(dom::atoms::VALIGN).map(str::to_ascii_lowercase).as_deref() {
                Some("top") => style.vertical_align = crate::values::VerticalAlign::Top,
                Some("bottom") => style.vertical_align = crate::values::VerticalAlign::Bottom,
                Some("middle") => style.vertical_align = crate::values::VerticalAlign::Middle,
                _ => {}
            }
        }
        _ => {}
    }
}

/// Replaces `var(--name)` / `var(--name, fallback)` with the custom
/// property's value (or the fallback, itself `var()`-substituted).
/// Bounded iteration guards against a cyclic reference. An unresolved
/// `var()` with no fallback becomes empty (matching CSS's "guaranteed-
/// invalid value" collapsing to nothing for our purposes).
///
/// Borrows the value when it has no `var(` — nearly every declaration —
/// instead of copying every declaration of every element.
fn substitute_vars<'v>(value: &'v str, vars: &HashMap<String, String>) -> Cow<'v, str> {
    if !value.contains("var(") {
        return Cow::Borrowed(value);
    }
    Cow::Owned(substitute_vars_owned(value, vars))
}

fn substitute_vars_owned(value: &str, vars: &HashMap<String, String>) -> String {
    let mut current = value.to_string();
    for _ in 0..16 {
        let Some(start) = current.find("var(") else { break };
        // Match the closing paren for this var(.
        let after = &current[start + 4..];
        let mut depth = 1i32;
        let mut end_rel = None;
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end_rel = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end_rel) = end_rel else { break };
        let inner = &after[..end_rel];
        let (name, fallback) = match inner.split_once(',') {
            Some((n, f)) => (n.trim(), Some(f.trim())),
            None => (inner.trim(), None),
        };
        let replacement = vars
            .get(name.trim_start_matches("--").trim())
            .cloned()
            .or_else(|| fallback.map(|f| substitute_vars(f, vars).into_owned()))
            .unwrap_or_default();
        let end_abs = start + 4 + end_rel + 1;
        current = format!("{}{}{}", &current[..start], replacement, &current[end_abs..]);
    }
    current
}

/// Milestone-postscript concurrency work: precomputes every element's
/// style ONCE, in parallel across CPU cores, before layout runs — a
/// real, textbook "parallel style resolution" pass (the same class of
/// optimization Servo's original headline contribution to browser
/// engines was built around). Keyed by `dom::element_ptr_key` so
/// `layout` can look up a precomputed result by identity instead of
/// re-running the cascade on demand.
///
/// This is genuine multi-core PARALLELISM, not just concurrency: on a
/// machine with N cores, sibling subtrees are resolved on different
/// OS threads simultaneously via `rayon`'s work-stealing thread pool.
/// It also directly fixes a real, previously-documented inefficiency:
/// `layout` used to call `resolve_style` on the same element twice in
/// some paths (once to preview a child's margin for cursor math, once
/// again inside the recursive layout call) — with a shared precomputed
/// cache, that redundancy disappears for free, it isn't a separate fix.
///
/// Structurally: each element's style depends only on its ANCESTORS
/// (for descendant-selector matching and inheritance) and its PRECEDING
/// SIBLINGS (for `+`/`~` combinators and structural pseudo-classes) —
/// both of which are fully known before the element's own subtree is
/// visited, which is what keeps this safe to parallelize. `rayon`'s
/// `par_iter` fans out across each level's children; the recursion into
/// a child's OWN children happens inside that same parallel closure, so
/// the parallelism compounds naturally through the whole tree depth.
///
/// `viewport_width` is threaded through for `@media` evaluation — it's
/// the only sibling/ancestor-independent input to the cascade.
pub fn resolve_style_tree_parallel<'a>(root: impl Into<ElementRef<'a>>, stylesheet: &Stylesheet, viewport_width: f32) -> StyleCache {
    resolve_style_tree_in_viewport(root, stylesheet, ViewportSize::new(viewport_width, DEFAULT_VIEWPORT_HEIGHT))
}

/// [`resolve_style_tree_parallel`] with the viewport's height too (for
/// `vh`/`vmin`/`vmax`).
pub fn resolve_style_tree_in_viewport<'a>(root: impl Into<ElementRef<'a>>, stylesheet: &Stylesheet, viewport: ViewportSize) -> StyleCache {
    let root = root.into();
    // Built once per tree resolve (not once per element — see
    // `RuleIndex`'s doc comment) and shared read-only across every
    // parallel task below. `StyleEngine` keeps one across restyles.
    let index = RuleIndex::build(stylesheet);
    let rules = Rules { stylesheet, index: &index, inline_styles: &InlineStyleCache::default(), interaction: Interaction::none() };
    resolve_tree(root, &rules, viewport).finish().0
}

/// The whole tree's cascade, into a [`CascadeSink`].
pub(crate) fn resolve_tree(root: ElementRef<'_>, rules: &Rules, viewport: ViewportSize) -> CascadeSink {
    let ctx = MatchContext {
        ancestors: &[],
        parent_style: None,
        previous_siblings: &[],
        next_sibling_exists: false,
        sibling_index: 0,
        sibling_count: 1,
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        interaction: rules.interaction,
    };
    // Each result goes straight into the sink (O(1) amortized), instead of
    // each level returning its own map for the parent to `.extend()` —
    // that copies every entry once per ancestor, O(n × depth).
    let sink = CascadeSink::new();
    resolve_subtree(root, &ctx, rules, &sink, 0);
    sink
}

/// Caps recursion depth (native stack frames, and — since each level
/// fans out via rayon — worker-thread stack frames too) independent of
/// how deep the DOM actually is. `html_parser::MAX_NESTING_DEPTH` bounds
/// HTML-PARSED trees at 512, but a tree built/mutated via JS
/// `appendChild` isn't bounded there, and this function recurses once
/// per DOM level with no bound of its own. Past the cap, a subtree is
/// simply left with no cache entry — `layout::style_of`'s cache-miss
/// path already falls back to `Style::default()` rather than panicking
/// (see that function's doc comment), so this degrades to "unstyled"
/// for pathologically deep content instead of crashing.
pub(crate) const MAX_STYLE_DEPTH: usize = 512;

/// Cascades `element` and its subtree into `sink`. `ctx` describes
/// `element`'s real position — the incremental restyle starts subtrees
/// mid-document.
pub(crate) fn resolve_subtree(element: ElementRef<'_>, ctx: &MatchContext, rules: &Rules, sink: &CascadeSink, depth: usize) {
    if depth >= MAX_STYLE_DEPTH {
        diagnostics::guard(diagnostics::Guard::StyleDepth, "DOM deeper than the cascade walks; subtree left unstyled");
        return;
    }
    sink.cascaded.fetch_add(1, Ordering::Relaxed);
    let Cascaded { element: style, before, after } = cascade_element(element, ctx, rules, sink);
    if let Some(b) = before {
        sink.push(crate::content::pseudo_id(element.node_id(), crate::content::PseudoElement::Before), *b);
    }
    if let Some(a) = after {
        sink.push(crate::content::pseudo_id(element.node_id(), crate::content::PseudoElement::After), *a);
    }

    let child_elements: Vec<ElementRef<'_>> = element.element_children().collect();
    let sibling_count = child_elements.len();
    if sibling_count > 0 {
        // The children's ancestor chain (closest first). Built only for
        // elements that have element children; its length is bounded by
        // `MAX_STYLE_DEPTH`.
        let mut child_ancestors = Vec::with_capacity(ctx.ancestors.len() + 1);
        child_ancestors.push(element);
        child_ancestors.extend_from_slice(ctx.ancestors);
        let child = |i: usize| {
            let child_ctx = MatchContext {
                ancestors: &child_ancestors,
                parent_style: Some(&style),
                previous_siblings: &child_elements[..i],
                next_sibling_exists: i + 1 < sibling_count,
                sibling_index: i,
                sibling_count,
                viewport_width: ctx.viewport_width,
                viewport_height: ctx.viewport_height,
                interaction: ctx.interaction,
            };
            resolve_subtree(child_elements[i], &child_ctx, rules, sink, depth + 1);
        };
        if sibling_count == 1 {
            // Nothing to split: skip the task machinery.
            child(0);
        } else {
            // The parallel fan-out: rayon's work-stealing pool runs sibling
            // subtrees on as many cores as there are (sequentially on one).
            (0..sibling_count).into_par_iter().for_each(child);
        }
    }

    sink.push(element.node_id(), style);
}
