//! Incremental restyle — Milestone 31 (`docs/adr/008-dirty-tracking-and-identity.md`).
//!
//! [`StyleEngine`] keeps a document's styles across frames and redoes the
//! cascade only where a recorded DOM mutation can have changed its result:
//!
//! - an **attribute** change on E restyles E's subtree, and — if the
//!   stylesheet has sibling combinators (`+`, `~`) — the subtrees of E's
//!   later siblings;
//! - a **child-list** change on P restyles the subtrees of P's new children,
//!   and — if the stylesheet has structural pseudo-classes or sibling
//!   combinators — all of P's children;
//! - a new stylesheet, a viewport width change (when the sheet has `@media`
//!   rules), or overflowed mutation records restyle everything.
//!
//! (The engine's selectors have no `:has()` or `:empty`, so no change below
//! an element can alter the element's own style.)
//!
//! It also owns the frame's **final** styles: the cascade with transitions
//! and `@keyframes` layered on top ([`StyleEngine::animate`]), visiting only
//! the elements that have a `transition` or an `animation-name`.
//!
//! Correctness is checked against the full cascade by a property test over
//! thousands of random mutation sequences (`tests::incremental_*`).

use crate::cascade::{resolve_subtree, CascadeSink, InlineStyleCache, Interaction, MatchContext, RuleIndex, Rules};
use crate::style::StyleCache;
use crate::length::ViewportSize;
use crate::transitions::{apply_animations_to, apply_transitions_to, AnimationRegistry, TransitionRegistry};
use css_parser::{Combinator, CompoundSelector, KeyframesRule, PseudoClass, Stylesheet};
use dom::{Document, ElementRef, MutationKind, Mutations, NodeId};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

/// Which selector features decide how far a mutation's effect can reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SheetFeatures {
    /// `+` or `~` anywhere: an element's style can depend on its earlier
    /// siblings' attributes.
    pub sibling_combinators: bool,
    /// `:first-child`, `:last-child`, `:only-child`, `:nth-child()`: an
    /// element's style can depend on its position among its siblings.
    pub structural_pseudo_classes: bool,
    /// Any `@media` rule: styles can depend on the viewport width.
    pub media_queries: bool,
    /// Milestone 33: a `vw`/`vh`/`vmin`/`vmax` unit in any declaration
    /// (a conservative text match): styles depend on the viewport size.
    pub viewport_units: bool,
    /// M4's gap: `:hover`, `:active`, `:focus` or `:focus-within` anywhere:
    /// styles depend on what the user is doing.
    pub interaction: bool,
}

impl SheetFeatures {
    pub fn of(sheet: &Stylesheet) -> SheetFeatures {
        fn structural(compound: &CompoundSelector) -> bool {
            compound.pseudo_classes.iter().any(|p| match p {
                PseudoClass::FirstChild | PseudoClass::LastChild | PseudoClass::OnlyChild | PseudoClass::NthChild(_) => true,
                PseudoClass::Not(inner) => structural(inner),
                PseudoClass::Interactive(_) | PseudoClass::Root => false,
            })
        }
        fn interactive(compound: &CompoundSelector) -> bool {
            compound.pseudo_classes.iter().any(|p| match p {
                PseudoClass::Interactive(name) => matches!(name.as_str(), "hover" | "active" | "focus" | "focus-visible" | "focus-within"),
                PseudoClass::Not(inner) => interactive(inner),
                _ => false,
            })
        }
        let mut features = SheetFeatures::default();
        for rule in &sheet.rules {
            features.media_queries |= rule.media.is_some();
            features.viewport_units |= rule.declarations.iter().any(|d| mentions_viewport_unit(&d.value));
            for selector in &rule.selectors {
                features.sibling_combinators |=
                    selector.combinators.iter().any(|c| matches!(c, Combinator::NextSibling | Combinator::SubsequentSibling));
                features.structural_pseudo_classes |= selector.compounds.iter().any(structural);
                features.interaction |= selector.compounds.iter().any(interactive);
            }
        }
        features
    }
}

/// What one [`StyleEngine::restyle`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestyleReport {
    /// Everything was restyled (first frame, new sheet, viewport, overflow).
    pub full: bool,
    /// How many elements the cascade ran for.
    pub restyled: usize,
    /// Elements whose FINAL style (what layout sees) changed.
    pub changed: HashSet<NodeId>,
}

/// A document's styles, kept across frames.
///
/// One map holds every element's final style (what layout and paint use).
/// For all but animated elements that IS the cascade's result; the cascaded
/// style of an element with a `transition` or `animation-name` — the target
/// the animation moves towards — is kept aside in `targets`. (An earlier
/// version kept a whole second copy of the cascade, cloning every element's
/// `Style` on each full restyle: most of a large page's first-frame cost.)
#[derive(Default)]
pub struct StyleEngine {
    /// Final styles for every element.
    final_styles: StyleCache,
    /// The cascaded style of each animated element.
    targets: StyleCache,
    /// Elements whose cascaded style has a `transition` or an
    /// `animation-name` — the only ones the animation passes visit.
    animated: HashSet<NodeId>,
    features: SheetFeatures,
    /// The stylesheet's rule index, built once per stylesheet (it is
    /// O(rules); rebuilding it on every mutation frame was pure waste).
    index: Option<RuleIndex>,
    /// Inline `style` attributes already parsed.
    inline_styles: InlineStyleCache,
    /// The stylesheet's `@keyframes` by name, built with the index (it
    /// used to be rebuilt — every rule cloned — on every animated frame).
    keyframes: HashMap<String, KeyframesRule>,
    /// The inputs of the last restyle; a change in either means a full one.
    sheet_generation: Option<u64>,
    viewport: Option<ViewportSize>,
    /// M4's gap: what the user is doing, and the elements whose state
    /// changed since the last restyle.
    interaction: Interaction,
    interaction_changed: Vec<NodeId>,
}

impl StyleEngine {
    pub fn new() -> StyleEngine {
        StyleEngine::default()
    }

    /// Whether the stylesheet has a rule that depends on what the user is
    /// doing — if not, hovering and pressing need no restyle (or hit test).
    pub fn uses_interaction(&self) -> bool {
        self.features.interaction
    }

    /// M4's gap: what the user is doing now. The elements whose `:hover`,
    /// `:active` or `:focus` state changed are restyled by the next
    /// [`Self::restyle`] (with whatever their siblings' selectors reach).
    /// Returns whether a restyle is due.
    pub fn set_interaction(&mut self, interaction: Interaction) -> bool {
        if interaction == self.interaction {
            return false;
        }
        let changed = self.interaction.changed(&interaction);
        self.interaction = interaction;
        if !self.features.interaction {
            return false;
        }
        self.interaction_changed.extend(changed);
        !self.interaction_changed.is_empty()
    }

    /// The frame's final styles (for layout).
    pub fn styles(&self) -> &StyleCache {
        &self.final_styles
    }

    /// One element's cascaded style, before transitions and animations.
    pub fn cascaded_style(&self, node: NodeId) -> Option<&crate::Style> {
        self.targets.get(&node).or_else(|| self.final_styles.get(&node))
    }

    /// How many elements have a style.
    pub fn len(&self) -> usize {
        self.final_styles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.final_styles.is_empty()
    }

    /// Whether any element has a `transition` or `animation-name`.
    pub fn has_animated_elements(&self) -> bool {
        !self.animated.is_empty()
    }

    /// Brings the styles up to date with `document` after `mutations`.
    /// `sheet_generation` identifies the stylesheet: pass a new value
    /// whenever the sheet is replaced.
    pub fn restyle(&mut self, document: &Document, sheet: &Stylesheet, sheet_generation: u64, viewport: ViewportSize, mutations: &Mutations) -> RestyleReport {
        let sheet_changed = self.sheet_generation != Some(sheet_generation);
        if sheet_changed || self.index.is_none() {
            self.features = SheetFeatures::of(sheet);
            self.index = Some(RuleIndex::build(sheet));
            self.keyframes = sheet.keyframes.iter().map(|k| (k.name.clone(), k.clone())).collect();
        }
        // A resize matters when a style can depend on the viewport: an
        // `@media` rule, or a viewport unit in the sheet or in any inline
        // style (scanned only on a resize, which is rare).
        let viewport_matters = match self.viewport {
            Some(old) if old == viewport => false,
            None => true,
            Some(old) => {
                (self.features.media_queries && old.width != viewport.width)
                    || self.features.viewport_units
                    || inline_styles_mention_viewport_units(document.root())
            }
        };
        if sheet_changed || viewport_matters || mutations.overflowed {
            return self.full_restyle(document, sheet, sheet_generation, viewport);
        }
        self.viewport = Some(viewport);
        // Elements whose interaction state changed restyle as if their
        // attributes had (M4's gap).
        let interacted;
        let mutations = if self.interaction_changed.is_empty() {
            mutations
        } else {
            let mut all = mutations.clone();
            all.records.extend(self.interaction_changed.drain(..).map(|target| dom::MutationRecord { target, kind: MutationKind::Attributes }));
            interacted = all;
            &interacted
        };
        if mutations.records.is_empty() {
            return RestyleReport::default();
        }

        let roots = self.dirty_roots(document, mutations);
        let Some(index) = self.index.as_ref() else { unreachable!("built above") };
        let rules = Rules { stylesheet: sheet, index, inline_styles: &self.inline_styles, interaction: &self.interaction };
        let sink = CascadeSink::new();
        for root in &roots {
            let Some(element) = document.find_by_node_id(*root) else { continue };
            let ancestors = document.ancestors_of(*root);
            let parent_style = match ancestors.first() {
                Some(parent) => match self.cascaded_style(parent.node_id()) {
                    Some(style) => Some(style),
                    // The parent was never styled (past the depth cap):
                    // nothing below it is styled either.
                    None => continue,
                },
                None => None,
            };
            let siblings: Vec<ElementRef<'_>> = match ancestors.first() {
                Some(parent) => element_children(*parent),
                None => vec![element],
            };
            let index_in_parent = siblings.iter().position(|s| s.node_id() == *root).unwrap_or(0);
            let ctx = MatchContext {
                ancestors: &ancestors,
                parent_style,
                previous_siblings: &siblings[..index_in_parent],
                next_sibling_exists: index_in_parent + 1 < siblings.len(),
                sibling_index: index_in_parent,
                sibling_count: siblings.len(),
                viewport_width: viewport.width,
                viewport_height: viewport.height,
                interaction: &self.interaction,
            };
            resolve_subtree(element, &ctx, &rules, &sink, ancestors.len());
        }
        let restyled = sink.cascaded.load(Ordering::Relaxed);
        let (fresh, parsed_inline) = sink.finish();
        self.inline_styles.merge(parsed_inline);

        let mut report = RestyleReport { full: false, restyled, changed: HashSet::new() };
        // A pseudo-element that no longer generates a box: its style goes,
        // and its owner counts as changed (its layout has one box fewer).
        for node in fresh.keys().copied().filter(|n| !crate::content::is_pseudo(*n)).collect::<Vec<_>>() {
            for which in [crate::content::PseudoElement::Before, crate::content::PseudoElement::After] {
                let id = crate::content::pseudo_id(node, which);
                if !fresh.contains_key(&id) && self.final_styles.remove(&id).is_some() {
                    report.changed.insert(node);
                }
            }
        }
        for (node, style) in fresh {
            // A pseudo-element's change is its owner's (the owner lays it
            // out); pseudo-elements are not animated.
            if crate::content::is_pseudo(node) {
                if self.final_styles.get(&node) != Some(&style) {
                    report.changed.insert(crate::content::owner_of(node));
                    self.final_styles.insert(node, style);
                }
                continue;
            }
            if is_animated(&style) {
                self.animated.insert(node);
                self.targets.insert(node, style.clone());
            } else {
                self.animated.remove(&node);
                self.targets.remove(&node);
            }
            if self.final_styles.get(&node) != Some(&style) {
                report.changed.insert(node);
                self.final_styles.insert(node, style);
            }
        }
        self.prune(document);
        report
    }

    fn full_restyle(&mut self, document: &Document, sheet: &Stylesheet, sheet_generation: u64, viewport: ViewportSize) -> RestyleReport {
        self.interaction_changed.clear(); // everything is restyled
        let Some(index) = self.index.as_ref() else { unreachable!("built by `restyle`") };
        let rules = Rules { stylesheet: sheet, index, inline_styles: &self.inline_styles, interaction: &self.interaction };
        let (cascade, parsed_inline) = crate::cascade::resolve_tree(document.root(), &rules, viewport).finish();
        self.inline_styles.merge(parsed_inline);
        // Changed: every element whose style (or a pseudo-element's, or a
        // pseudo-element's presence) differs — as element ids.
        let owner = crate::content::owner_of;
        let changed: HashSet<NodeId> = if self.final_styles.is_empty() {
            cascade.keys().map(|n| owner(*n)).collect()
        } else {
            let differs = cascade.iter().filter(|(n, s)| self.final_styles.get(n) != Some(*s)).map(|(n, _)| owner(*n));
            let gone = self.final_styles.keys().filter(|n| crate::content::is_pseudo(**n) && !cascade.contains_key(n)).map(|n| owner(*n));
            differs.chain(gone).collect()
        };
        self.targets = cascade.iter().filter(|(n, s)| !crate::content::is_pseudo(**n) && is_animated(s)).map(|(n, s)| (*n, s.clone())).collect();
        self.animated = self.targets.keys().copied().collect();
        let restyled = cascade.keys().filter(|n| !crate::content::is_pseudo(**n)).count();
        self.final_styles = cascade;
        self.sheet_generation = Some(sheet_generation);
        self.viewport = Some(viewport);
        RestyleReport { full: true, restyled, changed }
    }

    /// The minimal set of subtree roots whose styles a set of mutations can
    /// have changed (no root inside another's subtree).
    fn dirty_roots(&self, document: &Document, mutations: &Mutations) -> Vec<NodeId> {
        let mut roots: HashSet<NodeId> = HashSet::new();
        let reaches_siblings = self.features.sibling_combinators;
        let reaches_positions = self.features.structural_pseudo_classes || self.features.sibling_combinators;
        for record in &mutations.records {
            let Some(target) = document.find_by_node_id(record.target) else { continue };
            match record.kind {
                MutationKind::Attributes => {
                    roots.insert(target.node_id());
                    if reaches_siblings {
                        if let Some(parent) = document.parent_of(target.node_id()) {
                            let siblings = element_children(parent);
                            let at = siblings.iter().position(|s| s.node_id() == target.node_id()).unwrap_or(siblings.len());
                            roots.extend(siblings[at..].iter().map(|s| s.node_id()));
                        }
                    }
                }
                // Milestone 39: an inserted or moved subtree is restyled
                // whole — it may carry styles from where it was before.
                MutationKind::Inserted => {
                    roots.insert(target.node_id());
                }
                MutationKind::ChildList => {
                    for child in element_children(target) {
                        if reaches_positions || !self.final_styles.contains_key(&child.node_id()) {
                            roots.insert(child.node_id());
                        }
                    }
                }
            }
        }
        // Keep only outermost roots: a root inside another root's subtree is
        // restyled with it. O(roots × depth) — no walk across siblings (a
        // child-index path costs one per earlier sibling in the arena, ADR
        // 021). Each root's subtree resolves independently, so the order is
        // only made stable, not meaningful.
        let mut kept: Vec<NodeId> = roots
            .iter()
            .copied()
            .filter(|&node| {
                if !document.contains(node) {
                    return false;
                }
                let mut at = document.parent_id(node);
                while let Some(parent) = at {
                    if roots.contains(&parent) {
                        return false;
                    }
                    at = document.parent_id(parent);
                }
                true
            })
            .collect();
        kept.sort_unstable();
        kept
    }

    /// Drops entries for elements no longer in the document, amortised: only
    /// when the caches have grown well past the document.
    fn prune(&mut self, document: &Document) {
        let live = document.element_count();
        if self.final_styles.len() > live * 3 + 64 {
            self.final_styles.retain(|n, _| document.contains(crate::content::owner_of(*n)));
            self.targets.retain(|n, _| document.contains(*n));
            self.animated.retain(|n| document.contains(*n));
        }
    }

    /// Applies transitions and `@keyframes` at `now` to the final styles,
    /// visiting only animated elements, and reports what changed.
    pub fn animate(
        &mut self,
        transitions: &mut TransitionRegistry,
        animations: &mut AnimationRegistry,
        now: std::time::Instant,
    ) -> AnimationReport {
        if self.animated.is_empty() {
            return AnimationReport::default();
        }
        let mut nodes: Vec<NodeId> = self.animated.iter().copied().collect();
        nodes.sort_unstable(); // deterministic order
        let before: Vec<Option<crate::Style>> = nodes.iter().map(|n| self.final_styles.get(n).cloned()).collect();
        // A node's final style starts from its cascaded target each frame;
        // the passes below then layer the animation on top.
        for node in &nodes {
            if let Some(target) = self.targets.get(node) {
                self.final_styles.insert(*node, target.clone());
            }
        }
        apply_transitions_to(&self.targets, nodes.iter().copied(), &mut self.final_styles, transitions, now);
        let viewport = self.viewport.unwrap_or_default();
        apply_animations_to(nodes.iter().copied(), &self.keyframes, &mut self.final_styles, animations, now, viewport);
        let mut report = AnimationReport::default();
        for (node, old) in nodes.iter().zip(before) {
            let new = self.final_styles.get(node);
            if new == old.as_ref() {
                continue;
            }
            // Milestone 34: only `transform` changed, on an element painted
            // on a transform layer of its own (before and after): the
            // compositor moves the layer; nothing is laid out again.
            let transform_only = match (old, new) {
                (Some(mut old), Some(new)) if old.composited_transform() && new.composited_transform() => {
                    old.transform = new.transform;
                    &old == new
                }
                _ => false,
            };
            match new {
                Some(new) if transform_only => report.composited.push((*node, new.transform)),
                _ => {
                    report.changed.insert(*node);
                }
            }
        }
        report
    }
}

/// What [`StyleEngine::animate`] changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AnimationReport {
    /// Elements whose final style changed in a way layout or paint must see.
    pub changed: HashSet<NodeId>,
    /// Elements on a transform layer of their own whose `transform` alone
    /// changed, and its new value — in document order of their node ids.
    pub composited: Vec<(NodeId, crate::Transform)>,
}

impl AnimationReport {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.composited.is_empty()
    }
}

/// Whether a declaration value may use a viewport unit. Conservative: a
/// false positive only costs a full restyle on resize.
fn mentions_viewport_unit(value: &str) -> bool {
    let v = value.to_ascii_lowercase();
    ["vw", "vh", "vmin", "vmax"].iter().any(|u| v.contains(u))
}

fn inline_styles_mention_viewport_units(root: ElementRef<'_>) -> bool {
    let mut stack = vec![root];
    while let Some(el) = stack.pop() {
        if el.attr_atom(dom::atoms::STYLE).is_some_and(mentions_viewport_unit) {
            return true;
        }
        stack.extend(el.element_children());
    }
    false
}

fn is_animated(style: &crate::Style) -> bool {
    !style.transitions.is_empty() || style.animation_name.is_some()
}

fn element_children(parent: ElementRef<'_>) -> Vec<ElementRef<'_>> {
    parent.element_children().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::{Element, Node};
    use crate::cascade::resolve_style_tree_parallel;

    /// xorshift64*: a deterministic PRNG, so a failing sequence reproduces.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len())]
        }
    }

    const TAGS: &[&str] = &["div", "span", "p", "a", "section", "input"];
    const CLASSES: &[&str] = &["a", "b", "c", "a b", "b c", ""];
    const IDS: &[&str] = &["x", "y", "z"];

    /// Sheets exercising every invalidation path: plain, sibling
    /// combinators, structural pseudo-classes, attribute selectors, `:not`,
    /// inheritance (`color`, custom properties), and `@media`.
    const SHEETS: &[&str] = &[
        "div { color: #111111; } .a { width: 10px; } #x { height: 7px; } span { font-size: 12px; }",
        ".a + .b { width: 20px; } .a ~ p { color: #ff0000; } div > .c { height: 3px; } section .b span { color: #00ff00; }",
        "p:first-child { width: 1px; } span:last-child { height: 2px; } div:nth-child(2n+1) { color: #0000ff; } a:only-child { width: 9px; } .c:not(:first-child) { height: 4px; }",
        "[data-v] { width: 5px; } [data-v=\"1\"] { color: #123456; } input[type=\"checkbox\"] { height: 16px; } .a:not(.b) { width: 30px; }",
        ":root, div { --c: #abcdef; } .b { color: var(--c); } section { color: #999999; } section span { width: 11px; }",
        "@media (min-width: 500px) { .a { width: 50px; } } @media (max-width: 499px) { .a { width: 49px; } } .b + .a { height: 8px; }",
        "div { transition: background-color 1s linear; } .b { background-color: #ff00ff; animation: none; } .c ~ .c { color: #444444; } p:last-child { width: 2px; }",
        // Milestone 33: generated content, appearing and disappearing.
        ".a::before { content: 'x' attr(id); color: #010101; } .b:after { content: none; } .c::after { content: ''; display: block; } :not(.a)::before { content: '-'; }",
    ];

    fn random_element(rng: &mut Rng, depth: usize) -> Element {
        let mut el = Element::new(*rng.pick(TAGS));
        let class = *rng.pick(CLASSES);
        if !class.is_empty() {
            el.set_attr("class", class);
        }
        if rng.below(6) == 0 {
            el.set_attr("id", *rng.pick(IDS));
        }
        if rng.below(4) == 0 {
            el.set_attr("data-v", rng.below(3).to_string());
        }
        if el.tag == "input" && rng.below(2) == 0 {
            el.set_attr("type", "checkbox");
        }
        if depth < 3 {
            for _ in 0..rng.below(4) {
                if rng.below(4) == 0 {
                    el.children.push(Node::Text("t".into()));
                }
                el.append_child(random_element(rng, depth + 1));
            }
        }
        el
    }

    fn all_ids(doc: &Document) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![doc.root()];
        while let Some(el) = stack.pop() {
            out.push(el.node_id());
            stack.extend(element_children(el));
        }
        out
    }

    fn random_mutation(rng: &mut Rng, doc: &mut Document) {
        let nodes = all_ids(doc);
        let target = *rng.pick(&nodes);
        let other = *rng.pick(&nodes);
        let a_child = |doc: &Document, of: NodeId, rng: &mut Rng| -> Option<NodeId> {
            let kids: Vec<NodeId> = doc.find_by_node_id(of)?.element_children().map(|e| e.node_id()).collect();
            (!kids.is_empty()).then(|| *rng.pick(&kids))
        };
        // Milestone 39: the full mutation API (ADR 021 §7).
        match rng.below(14) {
            8 => {
                let reference = a_child(doc, target, rng);
                let _ = doc.insert_before(target, random_element(rng, 2).into(), reference);
            }
            9 => {
                if let Some(child) = a_child(doc, target, rng) {
                    let _ = doc.remove_child(target, child);
                }
            }
            10 => {
                if let Some(child) = a_child(doc, target, rng) {
                    let _ = doc.replace_child(target, random_element(rng, 2).into(), child);
                }
            }
            11 => {
                // Moves (refused where they would make a cycle).
                let reference = a_child(doc, target, rng);
                let _ = doc.move_before(target, other, reference);
            }
            12 => {
                if let Some(dom::Fragment::Element(clone)) = doc.clone_node(other, true) {
                    let _ = doc.insert_before(target, clone.into(), a_child(doc, target, rng));
                }
            }
            13 => {
                let _ = doc.insert_before(target, dom::Fragment::text("t"), a_child(doc, target, rng));
            }
            0 => {
                doc.set_attribute(target, "class", *rng.pick(CLASSES));
            }
            1 => {
                doc.remove_attribute(target, "class");
            }
            2 => {
                doc.set_attribute(target, "id", *rng.pick(IDS));
            }
            3 => {
                doc.set_attribute(target, "data-v", rng.below(3).to_string());
            }
            4 => {
                let child = random_element(rng, 2);
                let _ = doc.append_child(target, child);
            }
            5 => {
                // Never empty the root entirely; any other element may lose
                // its whole subtree.
                if target != doc.root().node_id() {
                    doc.set_text_content(target, "replaced");
                }
            }
            6 => {
                doc.set_inline_style_property(target, "width", &format!("{}px", rng.below(40)));
            }
            _ => {
                let flip = rng.below(2) == 0;
                doc.mutate_elements(|el| {
                    if el.is(dom::atoms::P) {
                        if flip {
                            el.set_attr("class", "c");
                        } else {
                            el.remove_attr("class");
                        }
                        true
                    } else {
                        false
                    }
                });
            }
        }
    }

    fn assert_matches_full(engine: &StyleEngine, doc: &Document, sheet: &Stylesheet, width: f32, context: &str) {
        let full = resolve_style_tree_parallel(doc.root(), sheet, width);
        for node in all_ids(doc) {
            assert_eq!(engine.cascaded_style(node), full.get(&node), "incremental != full for node {node} ({context})");
        }
    }

    /// The Milestone 31 merge gate: incremental restyle equals a full
    /// restyle after every step of 10,000 random mutation sequences.
    #[test]
    fn incremental_restyle_equals_full_restyle_over_random_mutation_sequences() {
        let sheets: Vec<Stylesheet> = SHEETS.iter().map(|s| css_parser::parse_stylesheet(s)).collect();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut total_restyled = 0usize;
        let mut total_full_equivalent = 0usize;
        for sequence in 0..10_000 {
            let sheet_index = sequence % sheets.len();
            let sheet = &sheets[sheet_index];
            let mut doc = Document::new(random_element(&mut rng, 0));
            let width = if rng.below(2) == 0 { 400.0 } else { 800.0 };
            let mut engine = StyleEngine::new();
            let taken = doc.take_mutations();
            engine.restyle(&doc, sheet, 1, ViewportSize::new(width, 600.0), &taken);
            for step in 0..4 {
                for _ in 0..=rng.below(3) {
                    random_mutation(&mut rng, &mut doc);
                }
                let mutations = doc_take(&mut doc);
                let report = engine.restyle(&doc, sheet, 1, ViewportSize::new(width, 600.0), &mutations);
                total_restyled += report.restyled;
                total_full_equivalent += doc.element_count();
                assert_matches_full(&engine, &doc, sheet, width, &format!("sequence {sequence}, step {step}, sheet {sheet_index}"));
            }
        }
        // And it is actually incremental: far less cascade work than
        // restyling everything each step.
        assert!(total_restyled * 2 < total_full_equivalent, "restyled {total_restyled} of {total_full_equivalent}");
    }

    fn doc_take(doc: &mut Document) -> Mutations {
        doc.take_mutations()
    }

    #[test]
    fn a_viewport_change_restyles_everything_only_when_media_queries_exist() {
        let doc = Document::new({
            let mut root = Element::new("div");
            root.set_attr("class", "a");
            root
        });
        let mut engine = StyleEngine::new();
        let plain = css_parser::parse_stylesheet(".a { width: 1px; }");
        engine.restyle(&doc, &plain, 1, ViewportSize::new(800.0, 600.0), &Mutations::default());
        assert_eq!(engine.restyle(&doc, &plain, 1, ViewportSize::new(400.0, 600.0), &Mutations::default()), RestyleReport::default(), "no @media: width is irrelevant");
        let media = css_parser::parse_stylesheet("@media (max-width: 500px) { .a { width: 2px; } }");
        engine.restyle(&doc, &media, 2, ViewportSize::new(800.0, 600.0), &Mutations::default());
        let report = engine.restyle(&doc, &media, 2, ViewportSize::new(400.0, 600.0), &Mutations::default());
        assert!(report.full && report.changed.contains(&doc.root().node_id()));
    }

    #[test]
    fn one_attribute_change_on_a_10k_element_page_restyles_only_its_subtree() {
        let mut root = Element::new("body");
        for i in 0..10_000 {
            let mut item = Element::new("div");
            item.set_attr("class", if i % 2 == 0 { "a" } else { "b" });
            item.append_child(Element::new("span"));
            root.append_child(item);
        }
        let mut doc = Document::new(root);
        let sheet = css_parser::parse_stylesheet(".a { width: 1px; } .b span { color: #ff0000; } .c { height: 5px; }");
        let mut engine = StyleEngine::new();
        let taken = doc.take_mutations();
        let first = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        assert!(first.full && first.restyled == 20_001);

        let target = doc.root().children().nth(1234).and_then(dom::ChildRef::as_element).expect("an element child").node_id();
        doc.set_attribute(target, "class", "c");
        let taken = doc.take_mutations();
        let report = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        assert!(!report.full);
        assert_eq!(report.restyled, 2, "the element and its one child — not 20,001");
        assert!(report.changed.contains(&target));
    }

    /// M4's gap: moving the pointer from one element to another on a
    /// 10k-element page restyles the two (with their subtrees), not the
    /// page; a sheet without `:hover` restyles nothing.
    #[test]
    fn a_hover_change_restyles_only_the_elements_it_changed() {
        let mut root = Element::new("body");
        for _ in 0..10_000 {
            let mut item = Element::new("div");
            item.set_attr("class", "a");
            item.append_child(Element::new("span"));
            root.append_child(item);
        }
        let mut doc = Document::new(root);
        let sheet = css_parser::parse_stylesheet(".a:hover span { color: #ff0000; } .a { width: 1px; }");
        let mut engine = StyleEngine::new();
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        assert!(engine.uses_interaction());
        let body = doc.root().node_id();
        let item = |i: usize| doc.root().children().nth(i).and_then(dom::ChildRef::as_element).expect("an element").node_id();
        let (first, second) = (item(10), item(20));
        let span = |n: dom::NodeId| doc.find_by_node_id(n).and_then(|e| e.element_children().next()).map(|s| s.node_id()).unwrap();

        assert!(engine.set_interaction(Interaction { hover: vec![first, body], ..Interaction::default() }));
        let report = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &Mutations::default());
        assert!(!report.full);
        assert!(report.changed.contains(&span(first)), "its span turns red");
        // `body` entered hover too: it and everything below it are restyled
        // once, as an attribute change on it would be.
        assert!(report.restyled <= 20_002, "{}", report.restyled);

        assert!(engine.set_interaction(Interaction { hover: vec![second, body], ..Interaction::default() }));
        let report = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &Mutations::default());
        assert_eq!(report.restyled, 4, "the two items and their spans — not 20,001");
        assert!(report.changed.contains(&span(first)) && report.changed.contains(&span(second)));

        let plain = css_parser::parse_stylesheet(".a { width: 1px; }");
        let mut engine = StyleEngine::new();
        engine.restyle(&doc, &plain, 1, ViewportSize::new(800.0, 600.0), &Mutations::default());
        assert!(!engine.set_interaction(Interaction { hover: vec![first, body], ..Interaction::default() }), "no :hover rule: nothing to restyle");
    }

    #[test]
    fn sibling_combinators_widen_invalidation_to_later_siblings() {
        let mut root = Element::new("body");
        for _ in 0..5 {
            root.append_child(Element::new("p"));
        }
        let mut doc = Document::new(root);
        let sheet = css_parser::parse_stylesheet(".a ~ p { width: 3px; }");
        let mut engine = StyleEngine::new();
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        let second = doc.root().children().nth(1).and_then(dom::ChildRef::as_element).expect("an element child").node_id();
        doc.set_attribute(second, "class", "a");
        let taken = doc.take_mutations();
        let report = engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        assert_eq!(report.restyled, 4, "the changed element and the three siblings after it");
        assert_eq!(report.changed.len(), 3, "the three later <p>s now match `.a ~ p`");
    }

    #[test]
    fn animation_passes_visit_only_animated_elements_and_report_real_changes() {
        let mut root = Element::new("body");
        let mut boxed = Element::new("div");
        boxed.set_attr("class", "pulse");
        root.append_child(boxed);
        for _ in 0..1000 {
            root.append_child(Element::new("p"));
        }
        let mut doc = Document::new(root);
        let sheet = css_parser::parse_stylesheet(
            "@keyframes k { from { background-color: #000000; } to { background-color: #ffffff; } } .pulse { animation: k 1s infinite; }",
        );
        let mut engine = StyleEngine::new();
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        let (mut t, mut a) = (TransitionRegistry::new(), AnimationRegistry::new());
        let start = std::time::Instant::now();
        engine.animate(&mut t, &mut a, start);
        let changed = engine.animate(&mut t, &mut a, start + std::time::Duration::from_millis(250));
        assert_eq!(changed.changed.len(), 1, "only the animated element changes");
        assert!(changed.composited.is_empty(), "a colour is not a compositor change");
        let again = engine.animate(&mut t, &mut a, start + std::time::Duration::from_millis(250));
        assert!(again.is_empty(), "same time, same style: no change reported");
    }

    fn two_paragraphs() -> (Document, NodeId) {
        let mut root = Element::new("div");
        root.append_child(Element::new("p"));
        root.append_child(Element::new("p"));
        let doc = Document::new(root);
        let first = doc.root().children().next().and_then(dom::ChildRef::as_element).expect("an element child").node_id();
        (doc, first)
    }

    /// Performance audit: the rule index is built once per stylesheet, not
    /// on every mutation frame.
    #[test]
    fn the_rule_index_is_built_once_per_stylesheet() {
        let (mut doc, first) = two_paragraphs();
        let sheet = css_parser::parse_stylesheet("p { color: #111111; } .a { width: 3px; }");
        let mut engine = StyleEngine::new();
        let builds = || crate::cascade::RULE_INDEX_BUILDS.with(std::cell::Cell::get);
        let before = builds();
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        for i in 0..20 {
            doc.set_attribute(first, "class", if i % 2 == 0 { "a" } else { "b" });
            let taken = doc.take_mutations();
            engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        }
        assert_eq!(builds() - before, 1, "one index for one stylesheet");
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 2, ViewportSize::new(800.0, 600.0), &taken);
        assert_eq!(builds() - before, 2, "a new stylesheet gets a new index");
    }

    /// Parsed inline styles are reused across restyles, and a page writing a
    /// new `style` every frame cannot grow the cache without bound.
    #[test]
    fn the_inline_style_cache_is_reused_and_bounded() {
        let (mut doc, first) = two_paragraphs();
        let sheet = css_parser::parse_stylesheet("p { color: #111111; }");
        let mut engine = StyleEngine::new();
        doc.set_attribute(first, "style", "width: 4px");
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        assert_eq!(engine.inline_styles.len(), 1);
        assert_eq!(engine.cascaded_style(first).and_then(|s| s.width), Some(crate::Length::px(4.0)));
        for i in 0..(crate::cascade::MAX_INLINE_STYLE_CACHE * 2) {
            doc.set_attribute(first, "style", format!("width: {i}px"));
            let taken = doc.take_mutations();
            engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
            assert!(engine.inline_styles.len() <= crate::cascade::MAX_INLINE_STYLE_CACHE);
        }
        let last = crate::cascade::MAX_INLINE_STYLE_CACHE * 2 - 1;
        assert_eq!(engine.cascaded_style(first).and_then(|s| s.width), Some(crate::Length::px(last as f32)));
    }

    /// Custom properties are shared with the parent until an element
    /// declares its own — not copied into every element.
    #[test]
    fn custom_properties_are_shared_not_copied() {
        let (mut doc, first) = two_paragraphs();
        let sheet = css_parser::parse_stylesheet(":root { --a: 1px; --b: 2px; } p:first-child { --c: 3px; }");
        let mut engine = StyleEngine::new();
        let taken = doc.take_mutations();
        engine.restyle(&doc, &sheet, 1, ViewportSize::new(800.0, 600.0), &taken);
        let root = engine.cascaded_style(doc.root().node_id()).unwrap();
        let second = doc.root().children().nth(1).and_then(dom::ChildRef::as_element).expect("an element child").node_id();
        let inherited = engine.cascaded_style(second).unwrap();
        assert!(std::sync::Arc::ptr_eq(&root.custom_properties, &inherited.custom_properties), "no declaration: the parent's map is shared");
        let own = engine.cascaded_style(first).unwrap();
        assert!(!std::sync::Arc::ptr_eq(&root.custom_properties, &own.custom_properties));
        assert_eq!(own.custom_properties.len(), 3);
        assert_eq!(root.custom_properties.len(), 2, "copy on write: the parent's map is untouched");
        let _ = doc.take_mutations();
    }
}
