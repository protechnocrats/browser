//! Selector matching: does a given compound/selector, evaluated against
//! one element plus its ancestor/sibling context, actually match?
//! Pure predicate logic — no cascade/specificity/declaration-
//! application concerns live here (see `crate::cascade` for those).

use crate::cascade::{Interaction, MatchContext};
use css_parser::{AttrOp, AttrSelector, Combinator, CompoundSelector, PseudoClass, Selector};
use dom::ElementRef;

/// Where an element sits among its siblings — needed for structural
/// pseudo-classes. `None` means "not known in this position" (a
/// compound reached by walking *up* the tree via a descendant/child
/// combinator, where we don't track the ancestor's own siblings).
#[derive(Clone, Copy)]
struct SiblingPos {
    index: usize,
    count: usize,
}

fn matches_attr(attr: &AttrSelector, element: ElementRef<'_>) -> bool {
    let Some(value) = element.attr_named(&attr.name) else { return false };
    match &attr.op {
        AttrOp::Exists => true,
        AttrOp::Equals(v) => value == v,
        AttrOp::Includes(v) => !v.is_empty() && value.split_whitespace().any(|w| w == v),
        AttrOp::DashMatch(v) => value == v || value.starts_with(&format!("{v}-")),
        AttrOp::Prefix(v) => !v.is_empty() && value.starts_with(v.as_str()),
        AttrOp::Suffix(v) => !v.is_empty() && value.ends_with(v.as_str()),
        AttrOp::Substring(v) => !v.is_empty() && value.contains(v.as_str()),
    }
}

/// `is_root`: whether `element` is the document's root (it has no parent
/// in the tree being styled) — for `:root`.
fn matches_compound(state: &Interaction, compound: &CompoundSelector, element: ElementRef<'_>, sibling: Option<SiblingPos>, is_root: bool) -> bool {
    // A rule whose subject is a pseudo-element (`p::before`) must not
    // apply to the real element — `layout` doesn't generate the box
    // that would carry it. Documented Phase A limitation.
    compound.pseudo_elements.is_empty() && matches_compound_parts(state, compound, element, sibling, is_root)
}

/// [`matches_compound`] without the pseudo-element check.
fn matches_compound_parts(state: &Interaction, compound: &CompoundSelector, element: ElementRef<'_>, sibling: Option<SiblingPos>, is_root: bool) -> bool {
    #[cfg(test)]
    tests::count_compound_test();
    // Milestone 40b: a well-known type, `id` and `class` are found by atom
    // — an integer comparison, not a string one (ADR 021 §5).
    if let Some(tag) = &compound.tag {
        if !element.has_tag(tag) {
            return false;
        }
    }
    if let Some(id) = &compound.id {
        match element.attr_atom(dom::atoms::ID) {
            Some(el_id) if el_id == id => {}
            _ => return false,
        }
    }
    if !compound.classes.is_empty() {
        // Performance fix: this used to allocate a fresh `Vec<&str>`
        // (via `.collect()`) on EVERY call — once per candidate rule
        // per element per restyle, for every element with a `class`
        // compound anywhere in the stylesheet. Checking membership
        // directly against `split_whitespace()`'s iterator needs no
        // allocation at all; the tradeoff (re-scanning the class
        // string once per required class instead of once total) only
        // matters for a multi-class compound like `.a.b`, which is
        // rare next to the single-class case this optimizes.
        let class_attr = element.attr_atom(dom::atoms::CLASS).unwrap_or("");
        for class in &compound.classes {
            if !class_attr.split_whitespace().any(|c| c == class) {
                return false;
            }
        }
    }
    for attr in &compound.attrs {
        if !matches_attr(attr, element) {
            return false;
        }
    }
    for pc in &compound.pseudo_classes {
        let ok = match pc {
            // Milestone 20: `:valid`/`:invalid` ARE real now — unlike
            // `:hover`/`:focus`/`:active`/`:link`/`:visited` (still
            // always `false`; those need live interaction/navigation
            // state this cascade doesn't track, a separate, larger
            // piece of work), validity is computable purely from the
            // element's OWN attributes — see `dom::ElementRef::is_valid`.
            // M4's gap: what the user is doing to the page (`Interaction`).
            PseudoClass::Interactive(name) => match name.as_str() {
                "valid" => element.is_valid(),
                "invalid" => !element.is_valid(),
                "hover" => state.hover.contains(&element.node_id()),
                "active" => state.active.contains(&element.node_id()),
                "focus" | "focus-visible" => state.focus.first() == Some(&element.node_id()),
                "focus-within" => state.focus.contains(&element.node_id()),
                _ => false,
            },
            PseudoClass::Not(inner) => !matches_compound(state, inner, element, sibling, is_root),
            PseudoClass::Root => is_root,
            PseudoClass::FirstChild => sibling.is_some_and(|s| s.index == 0),
            PseudoClass::LastChild => sibling.is_some_and(|s| s.index + 1 == s.count),
            PseudoClass::OnlyChild => sibling.is_some_and(|s| s.count == 1),
            PseudoClass::NthChild(nth) => sibling.is_some_and(|s| nth.matches(s.index as i64 + 1)),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The pseudo-element a selector's subject names (`p::before` → `before`),
/// if any.
pub(crate) fn subject_pseudo_element(selector: &Selector) -> Option<&str> {
    selector.compounds.last().and_then(|c| c.pseudo_elements.first()).map(String::as_str)
}

/// Whether `selector` matches `element` when its subject's pseudo-element
/// is set aside — i.e. whether `p::before` applies to this `p`'s
/// `::before`. Matches the selector in place (no clone: resets such as
/// `*::before, *::after { … }` reach this for every element).
pub(crate) fn matches_selector_ignoring_pseudo(selector: &Selector, element: ElementRef<'_>, ctx: &MatchContext) -> bool {
    match selector.compounds.last() {
        Some(last) if last.pseudo_elements.len() == 1 => matches_selector_with(selector, element, ctx, true),
        _ => false,
    }
}

pub(crate) fn matches_selector(selector: &Selector, element: ElementRef<'_>, ctx: &MatchContext) -> bool {
    matches_selector_with(selector, element, ctx, false)
}

/// `ignore_subject_pseudo`: match the subject compound as if its (single)
/// pseudo-element were not there.
fn matches_selector_with(selector: &Selector, element: ElementRef<'_>, ctx: &MatchContext, ignore_subject_pseudo: bool) -> bool {
    let compounds = &selector.compounds;
    let Some(last_idx) = compounds.len().checked_sub(1) else { return false };

    let subject_pos = Some(SiblingPos { index: ctx.sibling_index, count: ctx.sibling_count });
    let subject = &compounds[last_idx];
    let is_root = ctx.ancestors.is_empty();
    let subject_matches = if ignore_subject_pseudo {
        matches_compound_parts(ctx.interaction, subject, element, subject_pos, is_root)
    } else {
        matches_compound(ctx.interaction, subject, element, subject_pos, is_root)
    };
    if !subject_matches {
        return false;
    }
    if last_idx < 2 {
        // One combinator at most: the walk below tries each ancestor or
        // sibling once — already linear, and it allocates nothing.
        return matches_prefix(ctx.interaction, selector, last_idx, ctx.ancestors, Some(ctx.previous_siblings), ctx.sibling_count);
    }
    PrefixMatcher::new(selector, ctx).matches(last_idx, Pos::Subject)
}

/// Match the part of `selector` to the left of `compounds[at]` (which
/// has already matched some element), given that element's `ancestors`
/// (closest first) and `prev_siblings` (document order, `None` when
/// unknown). `total_siblings` is the sibling count of the element at
/// `at`, used to build a `SiblingPos` for a matched previous sibling.
///
/// Plain backtracking: only used for selectors with at most one
/// combinator, where it is linear. Longer selectors go through
/// [`PrefixMatcher`] — backtracking over several descendant or sibling
/// combinators is exponential (`.x div div div div span` against a
/// 100-deep document took 11 s).
fn matches_prefix(
    state: &Interaction,
    selector: &Selector,
    at: usize,
    ancestors: &[ElementRef<'_>],
    prev_siblings: Option<&[ElementRef<'_>]>,
    total_siblings: usize,
) -> bool {
    if at == 0 {
        return true;
    }
    let combinator = selector.combinators[at - 1];
    let target = &selector.compounds[at - 1];

    match combinator {
        Combinator::Descendant => {
            for depth in 0..ancestors.len() {
                if matches_compound(state, target, ancestors[depth], None, depth + 1 == ancestors.len())
                    && matches_prefix(state, selector, at - 1, &ancestors[depth + 1..], None, 0)
                {
                    return true;
                }
            }
            false
        }
        Combinator::Child => {
            let Some(parent) = ancestors.first() else { return false };
            matches_compound(state, target, *parent, None, ancestors.len() == 1)
                && matches_prefix(state, selector, at - 1, &ancestors[1..], None, 0)
        }
        Combinator::NextSibling => {
            let Some(sibs) = prev_siblings else { return false };
            let Some((&prev, rest)) = sibs.split_last() else { return false };
            let pos = SiblingPos { index: rest.len(), count: total_siblings };
            matches_compound(state, target, prev, Some(pos), false)
                && matches_prefix(state, selector, at - 1, ancestors, Some(rest), total_siblings)
        }
        Combinator::SubsequentSibling => {
            let Some(sibs) = prev_siblings else { return false };
            for k in (0..sibs.len()).rev() {
                let pos = SiblingPos { index: k, count: total_siblings };
                if matches_compound(state, target, sibs[k], Some(pos), false)
                    && matches_prefix(state, selector, at - 1, ancestors, Some(&sibs[..k]), total_siblings)
                {
                    return true;
                }
            }
            false
        }
    }
}

/// Where a compound of a selector has been matched: the subject itself,
/// one of the subject's previous siblings (by index), or one of its
/// ancestors (by index, closest first). An ancestor's own siblings are not
/// tracked, so a sibling combinator reached from an ancestor fails — the
/// same limitation [`matches_prefix`] has.
#[derive(Clone, Copy)]
enum Pos {
    Subject,
    Sibling(usize),
    Ancestor(usize),
}

const UNKNOWN: u8 = 0;
const NO: u8 = 1;
const YES: u8 = 2;

/// Selector matching as dynamic programming, for selectors with two or
/// more combinators. Each question — "can compound `c` sit on ancestor
/// `i` with everything left of it matching?" (`a_anc`), and "on any
/// ancestor at or above `i`?" (`h_anc`), and the same for previous
/// siblings — is answered at most once and remembered, so one match costs
/// O(compounds × (ancestors + siblings)) compound tests however the
/// selector and document are shaped. Recursion depth is bounded by the
/// compound count (`css_parser::MAX_SELECTOR_COMPOUNDS`), never by the
/// document: the "any ancestor at or above" scans are loops.
///
/// Memo rows are allocated per compound only when first needed, so a
/// selector that fails at its second compound costs one small row.
struct PrefixMatcher<'s, 'a> {
    selector: &'s Selector,
    state: &'a Interaction,
    ancestors: &'a [ElementRef<'a>],
    siblings: &'a [ElementRef<'a>],
    total_siblings: usize,
    /// `[c][i]`: compound `c` matches ancestor `i` and its prefix matches.
    anc_at: Vec<Vec<u8>>,
    /// `[c][i]`: `anc_at[c][j]` for some `j >= i`.
    anc_from: Vec<Vec<u8>>,
    /// `[c][k]`: compound `c` matches previous sibling `k` and its prefix
    /// matches.
    sib_at: Vec<Vec<u8>>,
    /// `[c][k]`: `sib_at[c][j]` for some `j <= k`.
    sib_upto: Vec<Vec<u8>>,
}

impl<'s, 'a> PrefixMatcher<'s, 'a> {
    fn new(selector: &'s Selector, ctx: &MatchContext<'a>) -> Self {
        let rows = selector.compounds.len();
        PrefixMatcher {
            selector,
            state: ctx.interaction,
            ancestors: ctx.ancestors,
            siblings: ctx.previous_siblings,
            total_siblings: ctx.sibling_count,
            anc_at: vec![Vec::new(); rows],
            anc_from: vec![Vec::new(); rows],
            sib_at: vec![Vec::new(); rows],
            sib_upto: vec![Vec::new(); rows],
        }
    }

    /// Whether `compounds[..c]` matches, given `compounds[c]` matched at
    /// `pos`.
    fn matches(&mut self, c: usize, pos: Pos) -> bool {
        if c == 0 {
            return true;
        }
        let prev = c - 1;
        match (self.selector.combinators[prev], pos) {
            (Combinator::Descendant, Pos::Ancestor(i)) => self.any_ancestor_from(prev, i + 1),
            (Combinator::Descendant, _) => self.any_ancestor_from(prev, 0),
            (Combinator::Child, Pos::Ancestor(i)) => self.at_ancestor(prev, i + 1),
            (Combinator::Child, _) => self.at_ancestor(prev, 0),
            (_, Pos::Ancestor(_)) => false,
            (Combinator::NextSibling, Pos::Sibling(k)) => k > 0 && self.at_sibling(prev, k - 1),
            (Combinator::NextSibling, Pos::Subject) => !self.siblings.is_empty() && self.at_sibling(prev, self.siblings.len() - 1),
            (Combinator::SubsequentSibling, Pos::Sibling(k)) => k > 0 && self.any_sibling_upto(prev, k - 1),
            (Combinator::SubsequentSibling, Pos::Subject) => {
                !self.siblings.is_empty() && self.any_sibling_upto(prev, self.siblings.len() - 1)
            }
        }
    }

    fn at_ancestor(&mut self, c: usize, i: usize) -> bool {
        let depth = self.ancestors.len();
        if i >= depth {
            return false;
        }
        if self.anc_at[c].is_empty() {
            self.anc_at[c] = vec![UNKNOWN; depth];
        }
        match self.anc_at[c][i] {
            YES => return true,
            NO => return false,
            _ => {}
        }
        let result = matches_compound(self.state, &self.selector.compounds[c], self.ancestors[i], None, i + 1 == depth) && self.matches(c, Pos::Ancestor(i));
        self.anc_at[c][i] = if result { YES } else { NO };
        result
    }

    fn any_ancestor_from(&mut self, c: usize, i: usize) -> bool {
        let depth = self.ancestors.len();
        if i >= depth {
            return false;
        }
        if self.anc_from[c].is_empty() {
            self.anc_from[c] = vec![UNKNOWN; depth];
        }
        let mut j = i;
        let found = loop {
            if j == depth {
                break false;
            }
            match self.anc_from[c][j] {
                YES => break true,
                NO => break false,
                _ => {}
            }
            if self.at_ancestor(c, j) {
                j += 1; // `j` itself is a match: record it below with the rest
                break true;
            }
            j += 1;
        };
        // Everything scanned shares the answer: a match at or before `j`'s
        // stopping point, or none from `i` onwards.
        self.anc_from[c][i..j].fill(if found { YES } else { NO });
        found
    }

    fn at_sibling(&mut self, c: usize, k: usize) -> bool {
        if self.sib_at[c].is_empty() {
            self.sib_at[c] = vec![UNKNOWN; self.siblings.len()];
        }
        match self.sib_at[c][k] {
            YES => return true,
            NO => return false,
            _ => {}
        }
        let pos = SiblingPos { index: k, count: self.total_siblings };
        let result = matches_compound(self.state, &self.selector.compounds[c], self.siblings[k], Some(pos), false) && self.matches(c, Pos::Sibling(k));
        self.sib_at[c][k] = if result { YES } else { NO };
        result
    }

    fn any_sibling_upto(&mut self, c: usize, k: usize) -> bool {
        if self.sib_upto[c].is_empty() {
            self.sib_upto[c] = vec![UNKNOWN; self.siblings.len()];
        }
        // Scan down from `k`; `low` ends one past the lowest index scanned.
        let mut low = k + 1;
        let found = loop {
            if low == 0 {
                break false;
            }
            let j = low - 1;
            match self.sib_upto[c][j] {
                YES => break true,
                NO => break false,
                _ => {}
            }
            low = j;
            if self.at_sibling(c, j) {
                break true;
            }
        };
        self.sib_upto[c][low..=k].fill(if found { YES } else { NO });
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::{Element, ElementRef};
    use std::cell::Cell;

    thread_local! {
        /// Compound tests run on this thread — the work a match does.
        static COMPOUND_TESTS: Cell<u64> = const { Cell::new(0) };
        /// Past this many, the test panics instead of running on: an
        /// exponential regression fails at once rather than hanging.
        static COMPOUND_TEST_BUDGET: Cell<u64> = const { Cell::new(u64::MAX) };
    }

    pub(super) fn count_compound_test() {
        let n = COMPOUND_TESTS.with(|n| {
            n.set(n.get() + 1);
            n.get()
        });
        assert!(n <= COMPOUND_TEST_BUDGET.with(Cell::get), "compound-test budget exceeded: matching is not linear");
    }

    /// Runs `f` with at most `budget` compound tests; returns how many ran.
    fn compound_tests<R>(budget: u64, f: impl FnOnce() -> R) -> (R, u64) {
        let before = COMPOUND_TESTS.with(Cell::get);
        COMPOUND_TEST_BUDGET.with(|b| b.set(before + budget));
        let r = f();
        COMPOUND_TEST_BUDGET.with(|b| b.set(u64::MAX));
        (r, COMPOUND_TESTS.with(Cell::get) - before)
    }

    fn selector(text: &str) -> Selector {
        let sheet = css_parser::parse_stylesheet(&format!("{text} {{ color: red }}"));
        sheet.rules.into_iter().next().and_then(|r| r.selectors.into_iter().next()).expect("selector parses")
    }

    fn ctx<'a>(ancestors: &'a [ElementRef<'a>], siblings: &'a [ElementRef<'a>], count: usize) -> MatchContext<'a> {
        MatchContext {
            ancestors,
            parent_style: None,
            previous_siblings: siblings,
            next_sibling_exists: siblings.len() + 1 < count,
            sibling_index: siblings.len(),
            sibling_count: count,
            viewport_width: 800.0,
            viewport_height: 600.0,
            interaction: Interaction::none(),
        }
    }

    /// The old matcher (plain backtracking), kept as the reference.
    fn reference(selector: &Selector, element: ElementRef<'_>, ctx: &MatchContext) -> bool {
        let last = selector.compounds.len() - 1;
        let pos = Some(SiblingPos { index: ctx.sibling_index, count: ctx.sibling_count });
        matches_compound(ctx.interaction, &selector.compounds[last], element, pos, ctx.ancestors.is_empty())
            && matches_prefix(ctx.interaction, selector, last, ctx.ancestors, Some(ctx.previous_siblings), ctx.sibling_count)
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
        }
    }

    fn random_element(rng: &mut Rng) -> Element {
        let mut el = Element::new(["div", "p", "span"][rng.below(3)]);
        let class = ["a", "b", "a b", ""][rng.below(4)];
        if !class.is_empty() {
            el.set_attr("class", class);
        }
        el
    }

    /// The memoized matcher answers exactly as backtracking does, over
    /// random selectors of 3–6 compounds (every combinator, structural
    /// pseudo-classes, `:not`) on random ancestor chains and sibling rows.
    #[test]
    fn the_memoized_matcher_agrees_with_backtracking() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let compounds = ["div", "p", "span", ".a", ".b", "*", "div.a", ":first-child", "p:not(.b)", ":nth-child(2n)"];
        let combinators = [" ", " > ", " + ", " ~ "];
        let mut disagreements = 0;
        let mut matched = 0;
        for _ in 0..20_000 {
            let n = 3 + rng.below(4);
            let mut text = compounds[rng.below(compounds.len())].to_string();
            for _ in 1..n {
                text.push_str(combinators[rng.below(4)]);
                text.push_str(compounds[rng.below(compounds.len())]);
            }
            let sel = selector(&text);
            let ancestors_owned: Vec<Element> = (0..rng.below(8)).map(|_| random_element(&mut rng)).collect();
            let siblings_owned: Vec<Element> = (0..rng.below(8)).map(|_| random_element(&mut rng)).collect();
            let subject = random_element(&mut rng);
            let ancestors: Vec<ElementRef<'_>> = ancestors_owned.iter().map(ElementRef::from).collect();
            let siblings: Vec<ElementRef<'_>> = siblings_owned.iter().map(ElementRef::from).collect();
            let count = siblings.len() + 1 + rng.below(3);
            let c = ctx(&ancestors, &siblings, count);
            let expected = reference(&sel, (&subject).into(), &c);
            matched += usize::from(expected);
            if matches_selector(&sel, (&subject).into(), &c) != expected {
                disagreements += 1;
                eprintln!("disagreement on `{text}`");
            }
        }
        assert_eq!(disagreements, 0);
        assert!(matched > 250, "the random cases must include real matches ({matched})");
    }

    /// Regression (performance audit): `.x div div div div span` against a
    /// 100-deep chain of `div`s backtracked through every combination of
    /// ancestors — 11 s for one element. Matching is now linear in
    /// compounds × ancestors.
    #[test]
    fn a_long_descendant_selector_on_a_deep_document_is_linear() {
        for (depth, ks) in [(100usize, 5usize), (512, 5), (512, 60)] {
            let owned: Vec<Element> = (0..depth).map(|_| Element::new("div")).collect();
            let ancestors: Vec<ElementRef<'_>> = owned.iter().map(ElementRef::from).collect();
            let subject = Element::new("span");
            let sel = selector(&format!(".x {}span", "div ".repeat(ks)));
            let bound = (ks as u64 + 2) * (depth as u64 + 1) + 1;
            let (matched, _) = compound_tests(bound, || matches_selector(&sel, (&subject).into(), &ctx(&ancestors, &[], 1)));
            assert!(!matched);
        }
    }

    /// The same for sibling combinators: `.x ~ p ~ p ~ p ~ span` in a long
    /// row of `p`s.
    #[test]
    fn a_long_sibling_selector_in_a_long_row_is_linear() {
        let owned: Vec<Element> = (0..2000).map(|_| Element::new("p")).collect();
        let siblings: Vec<ElementRef<'_>> = owned.iter().map(ElementRef::from).collect();
        let subject = Element::new("span");
        let sel = selector(".x ~ p ~ p ~ p ~ p + p ~ span");
        let (matched, _) = compound_tests(8 * 2001, || matches_selector(&sel, (&subject).into(), &ctx(&[], &siblings, 2001)));
        assert!(!matched);
    }

    #[test]
    fn a_selector_past_the_compound_cap_is_rejected() {
        let long = format!("{}span", "div ".repeat(css_parser::MAX_SELECTOR_COMPOUNDS));
        let sheet = css_parser::parse_stylesheet(&format!("{long} {{ color: red }}"));
        assert!(sheet.rules.iter().all(|r| r.selectors.is_empty()), "an over-long selector must not match anything");
        let ok = format!("{}span", "div ".repeat(css_parser::MAX_SELECTOR_COMPOUNDS - 1));
        assert!(css_parser::parse_stylesheet(&format!("{ok} {{ color: red }}")).rules.iter().any(|r| !r.selectors.is_empty()));
    }

    /// `::before` rules match through the element without cloning the
    /// selector, with the same answers as before.
    #[test]
    fn pseudo_element_rules_match_the_element_they_belong_to() {
        let mut el = Element::new("p");
        el.set_attr("class", "a");
        let c = ctx(&[], &[], 1);
        assert!(matches_selector_ignoring_pseudo(&selector("p.a::before"), (&el).into(), &c));
        assert!(!matches_selector(&selector("p.a::before"), (&el).into(), &c), "the rule is not the element's own");
        assert!(!matches_selector_ignoring_pseudo(&selector("p.b::before"), (&el).into(), &c));
        assert!(!matches_selector_ignoring_pseudo(&selector("p.a"), (&el).into(), &c), "no pseudo-element: not a ::before rule");
    }
}
