//! CSS parsing.
//!
//! Milestone 1: inline `style="..."` declarations. Milestone 4: full
//! stylesheet syntax — selector lists + rule blocks, type/class/ID/
//! descendant selectors.
//!
//! **Phase A (deferred-items plan)** extends the selector grammar to
//! what real stylesheets actually use:
//!
//! - child (`>`), next-sibling (`+`), subsequent-sibling (`~`) combinators
//! - attribute selectors: `[attr]`, `[attr=v]`, `[attr~=v]`, `[attr|=v]`,
//!   `[attr^=v]`, `[attr$=v]`, `[attr*=v]` (quoted or bare values)
//! - structural pseudo-classes: `:first-child`, `:last-child`,
//!   `:only-child`, `:nth-child(An+B | odd | even | k)`, `:not(<compound>)`
//! - interactive pseudo-classes `:hover`/`:focus`/`:active` — *parsed*
//!   and specificity-counted, but never match yet (no interactivity
//!   state is threaded into style resolution; `style`'s matcher treats
//!   them as always-false and documents it)
//! - pseudo-elements `::before`/`::after` — *parsed* (and the `content`
//!   property is parsed by `style`), but no generated boxes are
//!   produced by `layout` yet
//! - `@media (min-width / max-width / width: <px>)` queries, ANDed,
//!   with `screen`/`all` accepted-and-ignored and `print` never-match
//! - custom properties (`--foo: …`) and `var(--foo, fallback)` — the
//!   parser just carries them through as ordinary declarations/values;
//!   `style` does the cascade + substitution
//!
//! Still out of scope (Phase A notes): cascade layers (`@layer`),
//! container queries, `:nth-of-type`/`:nth-last-child`/`:has()`, `@media`
//! features other than width, calc() in media queries, namespace
//! selectors, case-insensitive attribute flags (`[attr=v i]`).
//!
//! This crate has no `dom` dependency and knows nothing about matching —
//! it only parses CSS *syntax*. Matching a `Selector` against a
//! `dom::Element` is `style`'s job.

pub use names::Name;

#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
}

/// How an attribute selector compares.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrOp {
    /// `[attr]` — present at all.
    Exists,
    /// `[attr=v]`
    Equals(String),
    /// `[attr~=v]` — `v` is one of a whitespace-separated list.
    Includes(String),
    /// `[attr|=v]` — equals `v` or starts with `v-`.
    DashMatch(String),
    /// `[attr^=v]`
    Prefix(String),
    /// `[attr$=v]`
    Suffix(String),
    /// `[attr*=v]`
    Substring(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttrSelector {
    /// Milestone 40b: a [`Name`], so a well-known attribute is matched by
    /// atom (ADR 021 §5).
    pub name: Name,
    pub op: AttrOp,
}

/// `An+B` argument of `:nth-child()`. `odd` -> (2, 1); `even` -> (2, 0);
/// a bare `k` -> (0, k).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Nth {
    pub a: i64,
    pub b: i64,
}

impl Nth {
    /// Does a 1-based child position satisfy this `An+B`? Matches CSS:
    /// there must be a non-negative integer `n` with `position = a*n + b`.
    pub fn matches(&self, position: i64) -> bool {
        if self.a == 0 {
            return position == self.b;
        }
        // `self.b` comes straight from parsing `:nth-child(An+B)` with
        // no range limit (see `parse_nth`), so a crafted stylesheet can
        // set it near `i64::MIN`/`MAX` — `position - self.b` would then
        // overflow for any real (small, positive) sibling position.
        // `checked_sub` turns that into "doesn't match" instead of a
        // panic; a real position can never satisfy an An+B that only
        // "matches" via integer wraparound anyway.
        let Some(diff) = position.checked_sub(self.b) else { return false };
        diff % self.a == 0 && diff / self.a >= 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PseudoClass {
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(Nth),
    /// `:root` — the document's root element (Milestone 33).
    Root,
    /// `:not(<compound>)` — one compound selector, no combinators
    /// inside (matches the level-3 restriction, not level-4's selector
    /// list).
    Not(Box<CompoundSelector>),
    /// `:hover` / `:focus` / `:active` / `:link` / `:visited` — parsed
    /// and specificity-counted, never matched yet.
    Interactive(String),
}

/// One compound selector — a type/class/ID/attribute/pseudo combination
/// with no combinator inside it (e.g. `a.btn[href^="https"]:hover`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompoundSelector {
    /// `None` means "any tag" (no type constraint, or `*`). Milestone 40b: a
    /// [`Name`], so a well-known type is matched by atom.
    pub tag: Option<Name>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<AttrSelector>,
    pub pseudo_classes: Vec<PseudoClass>,
    /// `::before` / `::after` etc. — parsed, not yet acted on.
    pub pseudo_elements: Vec<String>,
}

impl CompoundSelector {
    /// Specificity contribution (ids, classes, types) of this one
    /// compound. Classes, attribute selectors and pseudo-classes each
    /// count as a class; type and pseudo-element count as a type.
    fn specificity(&self) -> (usize, usize, usize) {
        let ids = self.id.is_some() as usize;
        let classes = self.classes.len() + self.attrs.len() + self.pseudo_classes.len();
        let types = self.tag.is_some() as usize + self.pseudo_elements.len();
        (ids, classes, types)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    /// whitespace
    Descendant,
    /// `>`
    Child,
    /// `+`
    NextSibling,
    /// `~`
    SubsequentSibling,
}

/// A selector is a chain of compounds joined by combinators. The
/// left-most compound is the outermost ancestor, the right-most is the
/// element the rule targets (the "subject").
///
/// `compounds` and `combinators` are kept as parallel vecs for
/// backward compatibility with existing code that iterates
/// `selector.compounds`: `combinators[i]` is the combinator sitting
/// *between* `compounds[i]` and `compounds[i + 1]`, so
/// `combinators.len() == compounds.len().saturating_sub(1)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub compounds: Vec<CompoundSelector>,
    pub combinators: Vec<Combinator>,
}

impl Selector {
    /// CSS specificity as (id count, class count, type count), summed
    /// across every compound — comparable via normal tuple ordering.
    pub fn specificity(&self) -> (usize, usize, usize) {
        let mut acc = (0, 0, 0);
        for compound in &self.compounds {
            let (i, c, t) = compound.specificity();
            acc.0 += i;
            acc.1 += c;
            acc.2 += t;
        }
        acc
    }
}

/// `@media` query — a set of width features, all of which must hold
/// (`and`). `never` is set for queries we parse but always reject
/// (`print`, or an unrecognized/malformed feature — failing closed).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaQuery {
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub exact_width: Option<f32>,
    pub never: bool,
}

impl MediaQuery {
    pub fn matches(&self, viewport_width: f32) -> bool {
        if self.never {
            return false;
        }
        if let Some(min) = self.min_width {
            if viewport_width < min {
                return false;
            }
        }
        if let Some(max) = self.max_width {
            if viewport_width > max {
                return false;
            }
        }
        if let Some(w) = self.exact_width {
            if (viewport_width - w).abs() > 0.5 {
                return false;
            }
        }
        true
    }
}

/// Where a rule came from, for the cascade (CSS Cascade 4 §6.1): a
/// user-agent rule loses to any author rule with a normal declaration,
/// whatever the specificities (and wins for `!important`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum CascadeOrigin {
    UserAgent,
    #[default]
    Author,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Comma-separated selector list — the rule applies if ANY of
    /// these match (using whichever matching selector has the highest
    /// specificity, for cascade purposes).
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
    /// `Some` if this rule came from inside an `@media` block — it only
    /// participates in the cascade when the query matches the viewport.
    /// `None` for a top-level rule (always active).
    pub media: Option<MediaQuery>,
    /// Milestone 33: user agent or author. Parsed rules are author rules.
    pub origin: CascadeOrigin,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    pub keyframes: Vec<KeyframesRule>,
    /// Milestone 32: `@font-face` rules, parsed (their fonts are not loaded
    /// yet — web fonts are Milestone 35, gated on the font fuzz targets).
    pub font_faces: Vec<FontFaceRule>,
}

impl Stylesheet {
    /// Puts `user_agent`'s rules (as user-agent rules) ahead of this
    /// sheet's. Their position does not decide priority — the origin does
    /// — but keeping them first keeps source order meaningful within each
    /// origin.
    pub fn with_user_agent(mut self, user_agent: &Stylesheet) -> Stylesheet {
        let mut rules: Vec<Rule> = user_agent.rules.iter().cloned().map(|r| Rule { origin: CascadeOrigin::UserAgent, ..r }).collect();
        rules.append(&mut self.rules);
        self.rules = rules;
        self
    }
}

/// One `@font-face` rule.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FontFaceRule {
    pub family: Option<String>,
    pub sources: Vec<FontFaceSource>,
    pub weight: Option<String>,
    pub style: Option<String>,
}

/// One entry of an `@font-face` `src` list: `url(…) format(…)` or `local(…)`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FontFaceSource {
    pub url: Option<String>,
    pub local: Option<String>,
    pub format: Option<String>,
}

/// Most `src` entries kept from one `@font-face` (real rules list a handful
/// of formats; a hostile one could list thousands).
pub const MAX_FONT_FACE_SOURCES: usize = 16;

/// The inside of a CSS function argument: trimmed, one level of matching
/// quotes removed.
fn function_argument(s: &str) -> String {
    let s = s.trim();
    let unquoted = s
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(s);
    unquoted.trim().to_string()
}

/// Parses an `@font-face` `src` descriptor: a comma-separated list of
/// `url(<url>) [format(<string>)] [tech(…)]` or `local(<name>)` entries.
/// Malformed entries are skipped; never panics (fuzzed:
/// `fuzz_targets/font_face_src.rs` — the input decides which font bytes the
/// engine will fetch and parse once web fonts exist).
pub fn parse_font_face_src(value: &str) -> Vec<FontFaceSource> {
    let mut out = Vec::new();
    for entry in split_top_level(value, ',') {
        if out.len() >= MAX_FONT_FACE_SOURCES {
            break;
        }
        let mut source = FontFaceSource::default();
        let mut rest = entry.trim();
        while !rest.is_empty() {
            let Some(open) = rest.find('(') else { break };
            let name = rest[..open].trim().to_ascii_lowercase();
            // Matching close paren, respecting quotes.
            let mut depth = 0i32;
            let mut quote: Option<char> = None;
            let mut close = None;
            for (i, c) in rest[open..].char_indices() {
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None => match c {
                        '"' | '\'' => quote = Some(c),
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(open + i);
                                break;
                            }
                        }
                        _ => {}
                    },
                }
            }
            let Some(close) = close else { break };
            let argument = function_argument(&rest[open + 1..close]);
            match name.as_str() {
                "url" if !argument.is_empty() => source.url = Some(argument),
                "local" if !argument.is_empty() => source.local = Some(argument),
                "format" => source.format = Some(argument.to_ascii_lowercase()),
                _ => {} // `tech(…)` and anything unknown: ignored
            }
            rest = rest[close + 1..].trim_start();
        }
        if source.url.is_some() || source.local.is_some() {
            out.push(source);
        }
    }
    out
}

/// A parsed `@keyframes name { ... }` block: every stop, in offset order
/// (M22's gap: Milestone 22 kept only the first and last). A stop's
/// `animation-timing-function` eases the segment that starts at it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KeyframesRule {
    pub name: String,
    pub stops: Vec<Keyframe>,
}

/// One stop of a `@keyframes` rule: its offset in `0..=1` (`from` is 0,
/// `to` 1, `50%` 0.5) and its declarations.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Keyframe {
    pub offset: f32,
    pub declarations: Vec<Declaration>,
}

/// Stops a `@keyframes` rule keeps; more are dropped (a page cannot make
/// each animated frame walk an unbounded list).
pub const MAX_KEYFRAMES: usize = 256;

/// Parses the contents of a `style="..."` attribute (or a
/// declaration-block's interior) into raw (property, value, important)
/// declarations. `:` inside `url(...)`/`var(...)`/quotes does not split
/// a declaration; `;` inside them does not end one.
pub fn parse_inline_style(input: &str) -> Vec<Declaration> {
    split_top_level(input, ';')
        .into_iter()
        .filter_map(|decl| {
            let (property, raw_value) = split_once_top_level(&decl, ':')?;
            let property = property.trim();
            let raw_value = raw_value.trim();
            if property.is_empty() || raw_value.is_empty() {
                return None;
            }
            let (value, important) = strip_important(raw_value);
            Some(Declaration { property: property.to_string(), value: value.to_string(), important })
        })
        .collect()
}

fn strip_important(value: &str) -> (&str, bool) {
    let trimmed = value.trim_end();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(bang_pos) = lower.rfind('!') {
        if lower[bang_pos + 1..].trim() == "important" {
            return (trimmed[..bang_pos].trim_end(), true);
        }
    }
    (trimmed, false)
}

/// Splits `input` on top-level occurrences of `sep`, i.e. those not
/// nested inside `()`, `[]`, or a `'`/`"` string.
fn split_top_level(input: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in input.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' | '[' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' | ']' => {
                    depth -= 1;
                    cur.push(c);
                }
                _ if c == sep && depth <= 0 => {
                    out.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    out.push(cur);
    out
}

fn split_once_top_level(input: &str, sep: char) -> Option<(String, String)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in input.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ if c == sep && depth <= 0 => {
                    return Some((input[..i].to_string(), input[i + c.len_utf8()..].to_string()));
                }
                _ => {}
            },
        }
    }
    None
}

/// Parses a full stylesheet. `/* */` comments are stripped first;
/// `@media` blocks are recognized and their inner rules tagged with the
/// query; unterminated blocks / stray text degrade gracefully.
pub fn parse_stylesheet(input: &str) -> Stylesheet {
    let without_comments = strip_comments(input);
    let mut rules = Vec::new();
    let mut keyframes = Vec::new();
    let mut font_faces = Vec::new();
    parse_rules_into(&without_comments, None, &mut rules, &mut keyframes, &mut font_faces, 0);
    Stylesheet { rules, keyframes, font_faces }
}

/// Same class of bug as `MAX_NOT_NESTING_DEPTH`, in the at-rule path:
/// `parse_rules_into` recurses once per nested `@media { ... }`, and
/// copies the block body at every level (so also O(n²) time/memory).
/// ~25,000 nested `@media` blocks (a 600 KB stylesheet) overflowed the
/// native stack and aborted the process — found by the M25 fuzz
/// harness's structural stress inputs. Real stylesheets nest `@media`
/// two or three deep; a block nested past this cap is consumed and
/// discarded (its rules are dropped), never recursed into.
const MAX_AT_RULE_NESTING_DEPTH: u32 = 32;

/// Most `@font-face` rules kept from one stylesheet.
pub const MAX_FONT_FACE_RULES: usize = 256;

fn parse_rules_into(
    input: &str,
    media: Option<MediaQuery>,
    out: &mut Vec<Rule>,
    out_keyframes: &mut Vec<KeyframesRule>,
    out_font_faces: &mut Vec<FontFaceRule>,
    nesting: u32,
) {
    let mut chars = input.chars().peekable();

    loop {
        skip_whitespace(&mut chars);
        if chars.peek().is_none() {
            break;
        }

        // Read up to the next `{` (the selector prelude, or an at-rule
        // prelude like `@media ...`).
        let mut prelude = String::new();
        let mut found_brace = false;
        let mut statement_at_rule = false;
        while let Some(&c) = chars.peek() {
            if c == '{' {
                chars.next();
                found_brace = true;
                break;
            }
            // Milestone 32: a statement at-rule (`@charset "UTF-8";`,
            // `@import url(x.css);`, `@namespace …;`) has no block and ends
            // at `;`. Reading on to the next `{` used to swallow the whole
            // FOLLOWING rule as part of its prelude — found by the
            // conformance corpus, where `@charset` opens many stylesheets.
            if c == ';' && prelude.trim_start().starts_with('@') {
                chars.next();
                statement_at_rule = true;
                break;
            }
            if c == '}' {
                // Stray close brace (end of an @media block handled by
                // the caller, or just garbage) — stop.
                chars.next();
                return;
            }
            prelude.push(c);
            chars.next();
        }
        if statement_at_rule {
            let name = prelude.trim().split(|c: char| c.is_whitespace() || c == '"' || c == '\'').next().unwrap_or("@");
            diagnostics::unsupported::record("css-at-rule", name);
            continue;
        }
        if !found_brace {
            break;
        }

        let prelude = prelude.trim().to_string();

        if let Some(rest) = prelude.strip_prefix("@media") {
            // Collect this @media block's body by brace matching.
            let mut body = String::new();
            let mut depth = 1i32;
            for c in chars.by_ref() {
                match c {
                    '{' => {
                        depth += 1;
                        body.push(c);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(c);
                    }
                    _ => body.push(c),
                }
            }
            if nesting >= MAX_AT_RULE_NESTING_DEPTH {
                // Too deep to be real: the body was already consumed
                // above; drop it instead of recursing.
                diagnostics::guard(diagnostics::Guard::CssAtRuleNestingDepth, "@media nested past the depth cap; block dropped");
                continue;
            }
            let query = parse_media_query(rest.trim());
            // Nested @media (rare) is handled the same way as a
            // top-level one — parse the inner rules against the inner
            // query only, without trying to AND them (a documented
            // simplification).
            parse_rules_into(&body, Some(query), out, out_keyframes, out_font_faces, nesting + 1);
            continue;
        }

        if let Some(rest) = prelude.strip_prefix("@keyframes").or_else(|| prelude.strip_prefix("@-webkit-keyframes")) {
            let mut body = String::new();
            let mut depth = 1i32;
            for c in chars.by_ref() {
                match c {
                    '{' => {
                        depth += 1;
                        body.push(c);
                    }
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                        body.push(c);
                    }
                    _ => body.push(c),
                }
            }
            let name = rest.trim().to_string();
            if !name.is_empty() {
                if let Some(rule) = parse_keyframes_body(&name, &body) {
                    out_keyframes.push(rule);
                }
            }
            continue;
        }

        if prelude.starts_with('@') {
            // Some other at-rule (`@font-face`, `@layer`, `@supports`,
            // ...) — skip its block by brace matching so we don't misparse
            // its contents as rules. `@font-face` is parsed (M32) but its
            // font not loaded yet, so it is still counted as unsupported.
            let name = prelude.split(|c: char| c.is_whitespace() || c == '{' || c == ';').next().unwrap_or("@");
            diagnostics::unsupported::record("css-at-rule", name);
            let mut depth = 1i32;
            let mut body = String::new();
            for c in chars.by_ref() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                body.push(c);
            }
            if name.eq_ignore_ascii_case("@font-face") && out_font_faces.len() < MAX_FONT_FACE_RULES {
                let mut rule = FontFaceRule::default();
                for decl in parse_inline_style(&body) {
                    match decl.property.as_str() {
                        "font-family" => rule.family = Some(function_argument(&decl.value)),
                        "src" => rule.sources = parse_font_face_src(&decl.value),
                        "font-weight" => rule.weight = Some(decl.value.clone()),
                        "font-style" => rule.style = Some(decl.value.clone()),
                        _ => {}
                    }
                }
                out_font_faces.push(rule);
            }
            continue;
        }

        // Ordinary style rule. Milestone 33: its block is read by brace
        // matching, because it may contain nested rules (CSS Nesting) —
        // reading to the first `}` used to end the rule inside a nested one
        // and drop every rule after it in the stylesheet (found by the
        // conformance corpus: most of Wikipedia's skin stylesheet).
        let (body, found_close) = read_block(&mut chars);
        parse_style_rule(&prelude, &body, media.as_ref(), out, nesting);

        if !found_close {
            break;
        }
    }
}

/// Reads a `{ … }` block's body (the opening brace already consumed) by
/// brace matching, skipping braces inside strings. Returns the body and
/// whether its closing brace was found.
fn read_block(chars: &mut std::iter::Peekable<std::str::Chars>) -> (String, bool) {
    let mut body = String::new();
    let mut depth = 1u32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in chars.by_ref() {
        if let Some(q) = quote {
            body.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (body, true);
                }
            }
            _ => {}
        }
        body.push(c);
    }
    (body, false)
}

/// Most selectors one nested rule may expand to (a list nested in a list
/// multiplies): past it the nested rule is dropped.
const MAX_NESTED_SELECTORS: usize = 256;

/// One style rule: its own declarations, then its nested rules (CSS
/// Nesting 1) flattened into ordinary rules — `.a { .b {…} }` is
/// `.a .b {…}`, `&` stands for the parent (`.a { &:hover {…} }` is
/// `.a:hover`), a leading combinator relates to it (`.a { > .b {…} }`),
/// and a nested `@media` applies the parent selectors under that query.
/// Rules come out parent first, in source order.
fn parse_style_rule(prelude: &str, body: &str, media: Option<&MediaQuery>, out: &mut Vec<Rule>, nesting: u32) {
    let (declaration_text, nested) = split_nested_rules(body);
    let selectors = parse_selector_list(prelude);
    if !selectors.is_empty() {
        out.push(Rule { selectors, declarations: parse_inline_style(&declaration_text), media: media.cloned(), origin: CascadeOrigin::Author });
    }
    for (nested_prelude, nested_body) in nested {
        if nesting >= MAX_AT_RULE_NESTING_DEPTH {
            diagnostics::guard(diagnostics::Guard::CssAtRuleNestingDepth, "CSS nested past the depth cap; rule dropped");
            continue;
        }
        let nested_prelude = nested_prelude.trim();
        if let Some(query) = nested_prelude.strip_prefix("@media") {
            // A nested `@media`: the parent's selectors, under the query.
            let query = parse_media_query(query.trim());
            parse_style_rule(prelude, &nested_body, Some(&query), out, nesting + 1);
        } else if nested_prelude.starts_with('@') {
            let name = nested_prelude.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or("@");
            diagnostics::unsupported::record("css-at-rule", name);
        } else if let Some(combined) = nest_selectors(prelude, nested_prelude) {
            parse_style_rule(&combined, &nested_body, media, out, nesting + 1);
        }
    }
}

/// Splits a style rule's block into its declarations and its nested rules
/// (`prelude { body }` pairs), in order. Strings and parentheses are
/// respected.
fn split_nested_rules(body: &str) -> (String, Vec<(String, String)>) {
    if !body.contains('{') {
        return (body.to_string(), Vec::new());
    }
    let mut declarations = String::new();
    let mut nested = Vec::new();
    let mut segment = String::new();
    let mut chars = body.chars().peekable();
    let mut quote: Option<char> = None;
    let mut parens = 0u32;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            segment.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                segment.push(c);
            }
            '(' => {
                parens += 1;
                segment.push(c);
            }
            ')' => {
                parens = parens.saturating_sub(1);
                segment.push(c);
            }
            ';' if parens == 0 => {
                declarations.push_str(&segment);
                declarations.push(';');
                segment.clear();
            }
            '{' if parens == 0 => {
                let (inner, _) = read_block(&mut chars);
                nested.push((std::mem::take(&mut segment), inner));
            }
            _ => segment.push(c),
        }
    }
    declarations.push_str(&segment);
    (declarations, nested)
}

/// The selector list of a nested rule, written out in full against its
/// parent's (`None` when there is nothing to match or it would expand
/// past [`MAX_NESTED_SELECTORS`]).
fn nest_selectors(parent: &str, nested: &str) -> Option<String> {
    let parents = split_top_level(parent, ',');
    let children = split_top_level(nested, ',');
    if parents.len().saturating_mul(children.len()) > MAX_NESTED_SELECTORS {
        diagnostics::guard(diagnostics::Guard::CssAtRuleNestingDepth, "nested selector list too large; rule dropped");
        return None;
    }
    let mut combined = Vec::new();
    for parent in parents.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        for child in children.iter().map(|c| c.trim()).filter(|c| !c.is_empty()) {
            combined.push(if child.contains('&') { child.replace('&', parent) } else { format!("{parent} {child}") });
        }
    }
    (!combined.is_empty()).then(|| combined.join(", "))
}

/// Parses a `@keyframes` block's interior (`from {...} to {...}`, or
/// `0% {...} 50% {...} 100% {...}`, a selector naming several stops as
/// `0%, 100% {...}`) into stops sorted by offset (stable: stops at one
/// offset keep their order). Offsets outside 0–100% are dropped, as CSS
/// requires. Returns `None` if no stops were found at all.
fn parse_keyframes_body(name: &str, body: &str) -> Option<KeyframesRule> {
    let mut stops: Vec<(f32, Vec<Declaration>)> = Vec::new();
    let mut chars = body.chars().peekable();
    loop {
        skip_whitespace(&mut chars);
        if chars.peek().is_none() {
            break;
        }
        let mut selector = String::new();
        let mut found_brace = false;
        while let Some(&c) = chars.peek() {
            if c == '{' {
                chars.next();
                found_brace = true;
                break;
            }
            if c == '}' {
                chars.next();
                break;
            }
            selector.push(c);
            chars.next();
        }
        if !found_brace {
            break;
        }
        let mut decl_text = String::new();
        let mut found_close = false;
        while let Some(&c) = chars.peek() {
            if c == '}' {
                chars.next();
                found_close = true;
                break;
            }
            decl_text.push(c);
            chars.next();
        }
        let selector = selector.trim().to_ascii_lowercase();
        let declarations = parse_inline_style(&decl_text);
        for part in selector.split(',') {
            let stop = match part.trim() {
                "from" => Some(0.0),
                "to" => Some(1.0),
                s => s.strip_suffix('%').and_then(|n| n.trim().parse::<f32>().ok()).map(|p| p / 100.0),
            };
            if let Some(stop) = stop.filter(|s| (0.0..=1.0).contains(s)) {
                if stops.len() < MAX_KEYFRAMES {
                    stops.push((stop, declarations.clone()));
                }
            }
        }
        if !found_close {
            break;
        }
    }
    if stops.is_empty() {
        return None;
    }
    stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Some(KeyframesRule { name: name.to_string(), stops: stops.into_iter().map(|(offset, declarations)| Keyframe { offset, declarations }).collect() })
}

fn parse_media_query(text: &str) -> MediaQuery {
    let mut query = MediaQuery::default();
    // Split on `and` (case-insensitive), whitespace-delimited.
    let lower = text.to_ascii_lowercase();
    for part in lower.split("and") {
        let part = part.trim().trim_matches(',').trim();
        if part.is_empty() || part == "screen" || part == "all" || part == "only" {
            continue;
        }
        if part == "print" || part == "speech" {
            query.never = true;
            continue;
        }
        let inner = part.trim_start_matches('(').trim_end_matches(')').trim();
        if let Some((feature, value)) = inner.split_once(':') {
            let feature = feature.trim();
            let px = value.trim().trim_end_matches("px").trim().parse::<f32>();
            match (feature, px) {
                ("min-width", Ok(v)) => query.min_width = Some(v),
                ("max-width", Ok(v)) => query.max_width = Some(v),
                ("width", Ok(v)) => query.exact_width = Some(v),
                // A feature we don't model (orientation, resolution,
                // prefers-*, …) — fail closed so we don't apply styles
                // gated on a condition we can't evaluate.
                _ => query.never = true,
            }
        } else if !inner.is_empty() {
            query.never = true;
        }
    }
    query
}

/// Splits a comma-separated selector list, respecting `[]`/`()` nesting
/// (so `[title="a,b"]` and `:not(.x)` are not split mid-selector).
fn parse_selector_list(text: &str) -> Vec<Selector> {
    split_top_level(text, ',').iter().filter_map(|chunk| parse_selector(chunk.trim())).collect()
}

/// The most compounds one selector may have. Real selectors have a
/// handful; the cap bounds what one hostile selector can cost the cascade
/// (matching is linear in compounds × ancestors, see
/// `style::selectors`), and a longer selector is rejected, not truncated
/// (truncating would change what it matches).
pub const MAX_SELECTOR_COMPOUNDS: usize = 64;

/// Parses one full selector (a combinator-joined chain of compounds).
fn parse_selector(text: &str) -> Option<Selector> {
    let tokens = tokenize_selector(text);
    if tokens.is_empty() {
        return None;
    }

    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let mut pending_combinator: Option<Combinator> = None;

    for token in tokens {
        match token {
            SelToken::Combinator(c) => {
                // `>` after a descendant space: the explicit combinator wins.
                pending_combinator = Some(c);
            }
            SelToken::Compound(raw) => {
                let compound = parse_compound_selector(&raw)?;
                if compounds.len() == MAX_SELECTOR_COMPOUNDS {
                    diagnostics::guard(diagnostics::Guard::CssSelectorTooLong, "selector has too many compounds; rejected");
                    return None;
                }
                if !compounds.is_empty() {
                    combinators.push(pending_combinator.unwrap_or(Combinator::Descendant));
                }
                compounds.push(compound);
                pending_combinator = None;
            }
        }
    }

    if compounds.is_empty() {
        None
    } else {
        Some(Selector { compounds, combinators })
    }
}

enum SelToken {
    Compound(String),
    Combinator(Combinator),
}

/// Splits a selector string into compound-selector chunks and explicit
/// combinator tokens, respecting `[]`/`()` nesting.
fn tokenize_selector(text: &str) -> Vec<SelToken> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut pending_ws = false;

    let flush = |cur: &mut String, tokens: &mut Vec<SelToken>| {
        if !cur.trim().is_empty() {
            tokens.push(SelToken::Compound(std::mem::take(cur).trim().to_string()));
        } else {
            cur.clear();
        }
    };

    for c in text.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            _ if depth > 0 => cur.push(c),
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    flush(&mut cur, &mut tokens);
                    pending_ws = true;
                } else if !tokens.is_empty() {
                    pending_ws = true;
                }
            }
            '>' | '+' | '~' => {
                if !cur.is_empty() {
                    flush(&mut cur, &mut tokens);
                }
                let combinator = match c {
                    '>' => Combinator::Child,
                    '+' => Combinator::NextSibling,
                    _ => Combinator::SubsequentSibling,
                };
                tokens.push(SelToken::Combinator(combinator));
                pending_ws = false;
            }
            _ => {
                if pending_ws {
                    // A run of whitespace is only a descendant
                    // combinator when it sits between two compounds —
                    // not after an explicit `>`/`+`/`~` we just emitted.
                    if matches!(tokens.last(), Some(SelToken::Compound(_))) {
                        tokens.push(SelToken::Combinator(Combinator::Descendant));
                    }
                    pending_ws = false;
                }
                cur.push(c);
            }
        }
    }
    flush(&mut cur, &mut tokens);
    tokens
}

/// `:not()` can nest (`:not(:not(:not(...)))`), and each level recurses
/// through `parse_compound_selector` -> `parse_pseudo_class` ->
/// `parse_compound_selector` with no base-case bound on the input
/// itself. Without a depth cap, a stylesheet containing on the order
/// of 10^5 nested `:not()`s exhausts the native call stack and ABORTS
/// THE WHOLE PROCESS (a stack overflow is not a catchable panic) —
/// verified against this exact code path. No real selector nests
/// anywhere close to this deep, so the cap only ever rejects
/// adversarial input.
const MAX_NOT_NESTING_DEPTH: u32 = 20;

fn parse_compound_selector(token: &str) -> Option<CompoundSelector> {
    parse_compound_selector_at_depth(token, 0)
}

fn parse_compound_selector_at_depth(token: &str, depth: u32) -> Option<CompoundSelector> {
    if token.is_empty() {
        return None;
    }
    let mut compound = CompoundSelector::default();
    let chars: Vec<char> = token.chars().collect();
    let mut i = 0;

    // Optional leading type selector (or `*`).
    if i < chars.len() && !matches!(chars[i], '.' | '#' | '[' | ':' | '*') {
        let start = i;
        while i < chars.len() && !matches!(chars[i], '.' | '#' | '[' | ':') {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        if !name.is_empty() {
            compound.tag = Some(Name::from(name.to_ascii_lowercase()));
        }
    } else if i < chars.len() && chars[i] == '*' {
        i += 1;
    }

    while i < chars.len() {
        match chars[i] {
            '.' | '#' => {
                let marker = chars[i];
                i += 1;
                let start = i;
                while i < chars.len() && !matches!(chars[i], '.' | '#' | '[' | ':') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    return None;
                }
                if marker == '.' {
                    compound.classes.push(name);
                } else {
                    compound.id = Some(name);
                }
            }
            '[' => {
                let start = i + 1;
                let mut end = start;
                let mut quote: Option<char> = None;
                while end < chars.len() {
                    let c = chars[end];
                    if let Some(q) = quote {
                        if c == q {
                            quote = None;
                        }
                    } else if c == '"' || c == '\'' {
                        quote = Some(c);
                    } else if c == ']' {
                        break;
                    }
                    end += 1;
                }
                if end >= chars.len() {
                    return None; // unterminated `[`
                }
                let inner: String = chars[start..end].iter().collect();
                compound.attrs.push(parse_attr_selector(&inner)?);
                i = end + 1;
            }
            ':' => {
                let is_element = i + 1 < chars.len() && chars[i + 1] == ':';
                i += if is_element { 2 } else { 1 };
                let start = i;
                // Name, then optional `(...)`.
                while i < chars.len() && !matches!(chars[i], '.' | '#' | '[' | ':' | '(') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect::<String>().to_ascii_lowercase();
                let mut arg = String::new();
                if i < chars.len() && chars[i] == '(' {
                    let mut depth = 1i32;
                    i += 1;
                    let arg_start = i;
                    while i < chars.len() && depth > 0 {
                        match chars[i] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        if depth == 0 {
                            break;
                        }
                        i += 1;
                    }
                    arg = chars[arg_start..i].iter().collect();
                    if i < chars.len() {
                        i += 1; // consume ')'
                    }
                }
                if name.is_empty() {
                    return None;
                }
                // Milestone 33: CSS 2's single-colon forms of the four
                // original pseudo-elements are still pseudo-elements.
                if is_element || matches!(name.to_ascii_lowercase().as_str(), "before" | "after" | "first-line" | "first-letter") {
                    compound.pseudo_elements.push(name.to_ascii_lowercase());
                } else {
                    compound.pseudo_classes.push(parse_pseudo_class(&name, &arg, depth)?);
                }
            }
            _ => return None,
        }
    }

    Some(compound)
}

fn parse_attr_selector(inner: &str) -> Option<AttrSelector> {
    let inner = inner.trim();
    // Check two-char ops first, then a plain `=`, then bare existence.
    let two_char = ["~=", "|=", "^=", "$=", "*="];
    for op_str in two_char {
        if let Some(pos) = inner.find(op_str) {
            let name = Name::new(inner[..pos].trim());
            let value = unquote(inner[pos + 2..].trim());
            if name.is_empty() {
                return None;
            }
            let op = match op_str {
                "~=" => AttrOp::Includes(value),
                "|=" => AttrOp::DashMatch(value),
                "^=" => AttrOp::Prefix(value),
                "$=" => AttrOp::Suffix(value),
                "*=" => AttrOp::Substring(value),
                _ => unreachable!(),
            };
            return Some(AttrSelector { name, op });
        }
    }
    if let Some(pos) = inner.find('=') {
        let name = Name::new(inner[..pos].trim());
        let value = unquote(inner[pos + 1..].trim());
        if name.is_empty() {
            return None;
        }
        return Some(AttrSelector { name, op: AttrOp::Equals(value) });
    }
    if inner.is_empty() {
        return None;
    }
    Some(AttrSelector { name: Name::new(inner), op: AttrOp::Exists })
}

fn parse_pseudo_class(name: &str, arg: &str, depth: u32) -> Option<PseudoClass> {
    match name {
        "first-child" => Some(PseudoClass::FirstChild),
        "last-child" => Some(PseudoClass::LastChild),
        "only-child" => Some(PseudoClass::OnlyChild),
        "root" => Some(PseudoClass::Root),
        "nth-child" => parse_nth(arg).map(PseudoClass::NthChild),
        "not" => {
            if depth >= MAX_NOT_NESTING_DEPTH {
                diagnostics::guard(diagnostics::Guard::CssNotNestingDepth, ":not() nested past the depth cap; selector rejected");
                return None;
            }
            let compound = parse_compound_selector_at_depth(arg.trim(), depth + 1)?;
            Some(PseudoClass::Not(Box::new(compound)))
        }
        // Milestone 20: `:valid`/`:invalid` really match now (see
        // `style::matches_compound`'s `Interactive` arm — real
        // constraint-validation, not always-false like the rest of
        // this bucket).
        "hover" | "focus" | "active" | "link" | "visited" | "focus-visible" | "focus-within" | "valid" | "invalid" => {
            Some(PseudoClass::Interactive(name.to_string()))
        }
        // An unrecognized pseudo-class makes the whole compound
        // un-matchable rather than silently ignored (so we don't apply
        // a rule that was gated on something we don't understand).
        _ => {
            diagnostics::unsupported::record("css-selector", &format!(":{name}"));
            None
        }
    }
}

fn parse_nth(arg: &str) -> Option<Nth> {
    let arg = arg.trim().to_ascii_lowercase();
    match arg.as_str() {
        "odd" => return Some(Nth { a: 2, b: 1 }),
        "even" => return Some(Nth { a: 2, b: 0 }),
        _ => {}
    }
    if !arg.contains('n') {
        return arg.parse::<i64>().ok().map(|b| Nth { a: 0, b });
    }
    let (a_part, b_part) = arg.split_once('n')?;
    let a = match a_part.trim() {
        "" | "+" => 1,
        "-" => -1,
        s => s.parse::<i64>().ok()?,
    };
    let b_part = b_part.trim().replace(' ', "");
    let b = if b_part.is_empty() { 0 } else { b_part.parse::<i64>().ok()? };
    Some(Nth { a, b })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev_star = false;
            for c in chars.by_ref() {
                if prev_star && c == '/' {
                    break;
                }
                prev_star = c == '*';
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn font_face_rules_and_src_lists_are_parsed() {
        let sheet = parse_stylesheet(
            "@font-face { font-family: 'Open Sans'; font-weight: 400;\n src: local(\"Open Sans\"), url(/f/open.woff2) format(\"woff2\"), url('/f/a,b.woff') format('woff'); }\np { color: red; }",
        );
        assert_eq!(sheet.rules.len(), 1, "the rule after @font-face survives");
        let face = &sheet.font_faces[0];
        assert_eq!(face.family.as_deref(), Some("Open Sans"));
        assert_eq!(face.weight.as_deref(), Some("400"));
        assert_eq!(face.sources.len(), 3);
        assert_eq!(face.sources[0].local.as_deref(), Some("Open Sans"));
        assert_eq!((face.sources[1].url.as_deref(), face.sources[1].format.as_deref()), (Some("/f/open.woff2"), Some("woff2")));
        assert_eq!(face.sources[2].url.as_deref(), Some("/f/a,b.woff"), "a comma inside quotes is not a separator");
    }

    #[test]
    fn hostile_font_face_src_is_bounded_and_never_panics() {
        for value in ["", "url(", "url())", "format(woff)", "local()", "url('unterminated)", ",,,", "url(a) url(b) url(c)", "\u{0}("] {
            let _ = parse_font_face_src(value);
        }
        let many = vec!["url(x.woff)"; 10_000].join(",");
        assert_eq!(parse_font_face_src(&many).len(), MAX_FONT_FACE_SOURCES);
        let faces = "@font-face{src:url(x)}".repeat(1000);
        assert_eq!(parse_stylesheet(&faces).font_faces.len(), MAX_FONT_FACE_RULES);
    }

    /// Regression (conformance corpus): a statement at-rule has no block and
    /// ends at `;`. The rule after it used to be swallowed into its prelude.
    #[test]
    fn statement_at_rules_do_not_swallow_the_next_rule() {
        let sheet = parse_stylesheet("@charset \"UTF-8\";\n@import url(\"other.css\");\n@namespace svg url(http://www.w3.org/2000/svg);\nbody { color: red; }\np{margin:0}");
        assert_eq!(sheet.rules.len(), 2, "{:?}", sheet.rules);
        assert_eq!(sheet.rules[0].selectors[0].compounds[0].tag.as_deref(), Some("body"));
        assert_eq!(sheet.rules[0].declarations[0].property, "color");
        // A `;` inside an ordinary rule's prelude is not an at-rule end.
        assert_eq!(parse_stylesheet("a { color: red; } @media (min-width: 1px) { b { color: blue; } }").rules.len(), 2);
    }
    use super::*;

    /// Security regression: deeply-nested `:not()` must be rejected
    /// past a bounded depth instead of recursing until the native stack
    /// overflows (which aborts the whole process, unlike a panic).
    #[test]
    fn deeply_nested_not_is_rejected_instead_of_overflowing_the_stack() {
        let nesting = 100_000;
        let selector_text = format!("{}a{}", ":not(".repeat(nesting), ")".repeat(nesting));
        // Must return without crashing; a selector this deep is
        // correctly treated as unmatchable (`None` compound / css_parser
        // as a whole may report zero rules for that stylesheet), not a
        // hard requirement on the exact `Option` shape.
        let _ = parse_compound_selector(&selector_text);
        let stylesheet = parse_stylesheet(&format!("{selector_text} {{ color: red; }}"));
        let _ = stylesheet.rules.len();
    }

    /// Regression (found by the M25 fuzz harness): deeply nested
    /// `@media` blocks recursed once per level and overflowed the stack.
    #[test]
    fn deeply_nested_media_blocks_are_bounded_not_a_stack_overflow() {
        let nesting = 100_000;
        let css = format!("{}p{{x:y}}{}", "@media (min-width:1px){".repeat(nesting), "}".repeat(nesting));
        let sheet = parse_stylesheet(&css);
        // Past the cap the inner rule is dropped, not recursed into.
        assert!(sheet.rules.is_empty());
    }

    #[test]
    fn media_nesting_within_the_cap_still_parses() {
        let css = format!("{}p{{color:red}}{}", "@media (min-width:1px){".repeat(5), "}".repeat(5));
        assert_eq!(parse_stylesheet(&css).rules.len(), 1);
    }

    /// Security regression: `:nth-child(n-9223372036854775808)` (`b`
    /// near `i64::MIN`) must not panic on subtract-overflow inside
    /// `Nth::matches`.
    #[test]
    fn nth_child_with_extreme_b_does_not_panic_on_overflow() {
        let nth = parse_nth("n-9223372036854775808").expect("should parse as a valid An+B");
        assert!(!nth.matches(1));
        assert!(!nth.matches(i64::MAX));
    }

    #[test]
    fn parses_multiple_declarations() {
        let decls = parse_inline_style("width: 100px; height:50px ;background-color:#ff0000");
        assert_eq!(decls.len(), 3);
        assert_eq!(decls[2].value, "#ff0000");
    }

    #[test]
    fn declaration_split_respects_url_and_var_parens() {
        let decls = parse_inline_style("background-image: url(a;b.png); color: var(--x, rgb(1,2,3))");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].value, "url(a;b.png)");
        assert_eq!(decls[1].value, "var(--x, rgb(1,2,3))");
    }

    #[test]
    fn important_suffix_is_detected_and_stripped() {
        let decls = parse_inline_style("color: red !important; width: 10px");
        assert_eq!(decls[0].value, "red");
        assert!(decls[0].important);
        assert!(!decls[1].important);
    }

    #[test]
    fn parses_type_class_id_and_compound_selectors() {
        let sheet = parse_stylesheet("div.foo#bar { color: red; }");
        let sel = &sheet.rules[0].selectors[0];
        assert_eq!(sel.compounds.len(), 1);
        assert_eq!(sel.compounds[0].tag, Some("div".into()));
        assert_eq!(sel.compounds[0].id, Some("bar".to_string()));
        assert_eq!(sel.compounds[0].classes, vec!["foo".to_string()]);
    }

    #[test]
    fn parses_descendant_combinator() {
        let sheet = parse_stylesheet("div.outer span { color: blue; }");
        let sel = &sheet.rules[0].selectors[0];
        assert_eq!(sel.compounds.len(), 2);
        assert_eq!(sel.combinators, vec![Combinator::Descendant]);
    }

    #[test]
    fn parses_child_and_sibling_combinators() {
        let sheet = parse_stylesheet("ul > li + li ~ span { color: blue; }");
        let sel = &sheet.rules[0].selectors[0];
        assert_eq!(sel.compounds.len(), 4);
        assert_eq!(
            sel.combinators,
            vec![Combinator::Child, Combinator::NextSibling, Combinator::SubsequentSibling]
        );
    }

    #[test]
    fn combinators_without_surrounding_spaces_parse() {
        let sheet = parse_stylesheet("a>b+c{color:red}");
        let sel = &sheet.rules[0].selectors[0];
        assert_eq!(sel.compounds.len(), 3);
        assert_eq!(sel.combinators, vec![Combinator::Child, Combinator::NextSibling]);
    }

    #[test]
    fn parses_attribute_selectors() {
        let sheet = parse_stylesheet(
            r#"a[href] [data-x=y] [class~="a"] [lang|=en] [href^="https"] [href$=".pdf"] [href*="site"] { color: red; }"#,
        );
        let sel = &sheet.rules[0].selectors[0];
        let all_attrs: Vec<&AttrSelector> = sel.compounds.iter().flat_map(|c| c.attrs.iter()).collect();
        assert_eq!(all_attrs.len(), 7);
        assert_eq!(all_attrs[0], &AttrSelector { name: "href".into(), op: AttrOp::Exists });
        assert_eq!(all_attrs[1], &AttrSelector { name: "data-x".into(), op: AttrOp::Equals("y".into()) });
        assert_eq!(all_attrs[2], &AttrSelector { name: "class".into(), op: AttrOp::Includes("a".into()) });
        assert_eq!(all_attrs[3], &AttrSelector { name: "lang".into(), op: AttrOp::DashMatch("en".into()) });
        assert_eq!(all_attrs[4], &AttrSelector { name: "href".into(), op: AttrOp::Prefix("https".into()) });
        assert_eq!(all_attrs[5], &AttrSelector { name: "href".into(), op: AttrOp::Suffix(".pdf".into()) });
        assert_eq!(all_attrs[6], &AttrSelector { name: "href".into(), op: AttrOp::Substring("site".into()) });
    }

    #[test]
    fn attribute_value_with_comma_is_not_split_as_a_selector_list() {
        let sheet = parse_stylesheet(r#"[title="a, b"] { color: red; }"#);
        assert_eq!(sheet.rules[0].selectors.len(), 1);
        assert_eq!(
            sheet.rules[0].selectors[0].compounds[0].attrs[0].op,
            AttrOp::Equals("a, b".into())
        );
    }

    #[test]
    fn parses_structural_pseudo_classes() {
        let sheet = parse_stylesheet("li:first-child, li:last-child, li:only-child, li:nth-child(2n+1) { x: y; }");
        let sels = &sheet.rules[0].selectors;
        assert_eq!(sels[0].compounds[0].pseudo_classes, vec![PseudoClass::FirstChild]);
        assert_eq!(sels[1].compounds[0].pseudo_classes, vec![PseudoClass::LastChild]);
        assert_eq!(sels[2].compounds[0].pseudo_classes, vec![PseudoClass::OnlyChild]);
        assert_eq!(sels[3].compounds[0].pseudo_classes, vec![PseudoClass::NthChild(Nth { a: 2, b: 1 })]);
    }

    #[test]
    fn parses_nth_child_keywords_and_bare_numbers() {
        assert_eq!(parse_nth("odd"), Some(Nth { a: 2, b: 1 }));
        assert_eq!(parse_nth("even"), Some(Nth { a: 2, b: 0 }));
        assert_eq!(parse_nth("3"), Some(Nth { a: 0, b: 3 }));
        assert_eq!(parse_nth("n"), Some(Nth { a: 1, b: 0 }));
        assert_eq!(parse_nth("-n+3"), Some(Nth { a: -1, b: 3 }));
        assert_eq!(parse_nth("2n - 1"), Some(Nth { a: 2, b: -1 }));
    }

    #[test]
    fn nth_matches_positions_correctly() {
        let odd = Nth { a: 2, b: 1 };
        assert!(odd.matches(1) && odd.matches(3) && !odd.matches(2));
        let first_three = Nth { a: -1, b: 3 };
        assert!(first_three.matches(1) && first_three.matches(3) && !first_three.matches(4));
    }

    #[test]
    fn parses_not_pseudo_class() {
        let sheet = parse_stylesheet("div:not(.excluded) { color: red; }");
        let pc = &sheet.rules[0].selectors[0].compounds[0].pseudo_classes[0];
        match pc {
            PseudoClass::Not(inner) => assert_eq!(inner.classes, vec!["excluded".to_string()]),
            _ => panic!("expected :not"),
        }
    }

    #[test]
    fn parses_pseudo_elements_and_interactive_pseudo_classes() {
        let sheet = parse_stylesheet("a:hover { color: red; } p::before { content: 'x'; }");
        assert_eq!(
            sheet.rules[0].selectors[0].compounds[0].pseudo_classes,
            vec![PseudoClass::Interactive("hover".to_string())]
        );
        assert_eq!(sheet.rules[1].selectors[0].compounds[0].pseudo_elements, vec!["before".to_string()]);
    }

    #[test]
    fn unrecognized_pseudo_class_drops_the_selector() {
        // `:nth-of-type` is out of scope — the selector shouldn't match anything.
        let sheet = parse_stylesheet("p:nth-of-type(2) { color: red; } p { color: blue; }");
        // The unsupported rule is dropped entirely; the plain `p` rule survives.
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].declarations[0].value, "blue");
    }

    #[test]
    fn specificity_counts_attributes_and_pseudo_classes_as_classes() {
        let a = parse_selector("a[href]:hover").unwrap();
        let b = parse_selector("a.link.link").unwrap();
        assert_eq!(a.specificity(), (0, 2, 1));
        assert_eq!(a.specificity(), b.specificity());
    }

    #[test]
    fn parses_media_query_block() {
        let sheet = parse_stylesheet(
            "div { color: red; } @media (max-width: 600px) { div { color: blue; } .x { color: green; } }",
        );
        assert_eq!(sheet.rules.len(), 3);
        assert!(sheet.rules[0].media.is_none());
        let q = sheet.rules[1].media.as_ref().unwrap();
        assert_eq!(q.max_width, Some(600.0));
        assert!(sheet.rules[2].media.is_some());
    }

    #[test]
    fn media_query_min_and_max_and_and() {
        let sheet =
            parse_stylesheet("@media screen and (min-width: 400px) and (max-width: 900px) { a { x: y; } }");
        let q = sheet.rules[0].media.as_ref().unwrap();
        assert!(!q.never);
        assert_eq!(q.min_width, Some(400.0));
        assert_eq!(q.max_width, Some(900.0));
        assert!(q.matches(500.0));
        assert!(!q.matches(1000.0));
        assert!(!q.matches(300.0));
    }

    #[test]
    fn media_print_never_matches() {
        let sheet = parse_stylesheet("@media print { a { x: y; } }");
        assert!(sheet.rules[0].media.as_ref().unwrap().never);
    }

    #[test]
    fn unknown_media_feature_fails_closed() {
        let sheet = parse_stylesheet("@media (orientation: landscape) { a { x: y; } }");
        assert!(sheet.rules[0].media.as_ref().unwrap().never);
    }

    #[test]
    fn unsupported_at_rule_block_is_skipped_cleanly() {
        let sheet = parse_stylesheet("@font-face { font-family: x; src: url(y); } div { color: red; }");
        assert_eq!(sheet.rules.len(), 1);
        assert_eq!(sheet.rules[0].selectors[0].compounds[0].tag, Some("div".into()));
    }

    #[test]
    fn keyframes_from_to_are_parsed_and_do_not_produce_a_rule() {
        let sheet = parse_stylesheet("@keyframes spin { from { transform: rotate(0deg); } to { transform: rotate(360deg); } } div { color: red; }");
        assert_eq!(sheet.rules.len(), 1, "the @keyframes block must not be misparsed as a style rule");
        assert_eq!(sheet.keyframes.len(), 1);
        let kf = &sheet.keyframes[0];
        assert_eq!(kf.name, "spin");
        assert_eq!(kf.stops[0].declarations[0].value, "rotate(0deg)");
        assert_eq!(kf.stops.last().unwrap().declarations[0].value, "rotate(360deg)");
    }

    /// A rule with 100,000 stops keeps `MAX_KEYFRAMES` of them (an animated
    /// frame walks the list per element) and parses in bounded time.
    #[test]
    fn a_keyframes_rule_with_huge_numbers_of_stops_is_capped() {
        let body: String = (0..100_000).map(|i| format!("{}% {{ width: {i}px }} ", i % 101)).collect();
        let started = std::time::Instant::now();
        let sheet = parse_stylesheet(&format!("@keyframes big {{ {body} }}"));
        assert_eq!(sheet.keyframes[0].stops.len(), MAX_KEYFRAMES);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// M22's gap: every stop is kept, in offset order, a selector list
    /// names several, and offsets outside 0–100% are dropped.
    #[test]
    fn keyframes_keep_every_stop_in_offset_order() {
        let sheet = parse_stylesheet("@keyframes fade { 100% { opacity: 1; } 0%, 75% { opacity: 0; } 50% { opacity: 0.5; } 150% { opacity: 9; } }");
        let kf = &sheet.keyframes[0];
        let stops: Vec<(f32, &str)> = kf.stops.iter().map(|k| (k.offset, k.declarations[0].value.as_str())).collect();
        assert_eq!(stops, vec![(0.0, "0"), (0.5, "0.5"), (0.75, "0"), (1.0, "1")]);
    }

    #[test]
    fn keyframes_with_no_stops_is_skipped_not_a_panic() {
        let sheet = parse_stylesheet("@keyframes empty { } div { color: blue; }");
        assert_eq!(sheet.keyframes.len(), 0);
        assert_eq!(sheet.rules.len(), 1);
    }

    #[test]
    fn comments_are_stripped_before_parsing() {
        let sheet = parse_stylesheet("/* a */ div { /* b */ color: red; }");
        assert_eq!(sheet.rules[0].declarations[0].value, "red");
    }

    #[test]
    fn garbage_input_does_not_panic() {
        for input in ["{}", "}{", "###...{}", "div{{{color:red}}}", ".{color:red}", "@media {", "[unterminated"] {
            let _ = parse_stylesheet(input);
        }
    }

    #[test]
    fn empty_stylesheet_does_not_panic() {
        assert_eq!(parse_stylesheet(""), Stylesheet::default());
        assert_eq!(parse_stylesheet("   \n\t "), Stylesheet::default());
    }

    // ---- Milestone 27: every silent cap is observable ----

    #[test]
    fn hostile_nesting_reports_which_guard_fired() {
        use diagnostics::{collector::capture, Guard};
        let nesting = 100_000;
        let deep_not = format!("{}a{}{{color:red}}", ":not(".repeat(nesting), ")".repeat(nesting));
        let (_, c) = capture(|| parse_stylesheet(&deep_not));
        assert!(c.saw_guard(Guard::CssNotNestingDepth), "a 100,000-deep :not() must report its depth cap");
        assert!(!c.saw_guard(Guard::CssAtRuleNestingDepth), "and only the guard that actually fired");

        let deep_media = format!("{}p{{x:y}}{}", "@media (min-width:1px){".repeat(nesting), "}".repeat(nesting));
        let (_, c) = capture(|| parse_stylesheet(&deep_media));
        assert!(c.saw_guard(Guard::CssAtRuleNestingDepth), "deep @media must report its depth cap");
    }

    #[test]
    fn benign_stylesheets_trip_no_guard() {
        let (_, c) = diagnostics::collector::capture(|| {
            parse_stylesheet("p:not(.a){color:red} @media (min-width:1px){@media (max-width:9px){p{x:y}}}")
        });
        assert!(c.events.is_empty(), "no guard should fire on ordinary CSS: {:?}", c.events);
    }

    // ---- Milestone 33: CSS Nesting ----

    fn selectors_of(sheet: &Stylesheet) -> Vec<String> {
        sheet.rules.iter().map(|r| format!("{:?}", r.selectors.iter().map(|s| s.compounds.len()).collect::<Vec<_>>())).collect()
    }

    #[test]
    fn a_nested_rule_does_not_swallow_the_rules_after_it() {
        let sheet = parse_stylesheet(".outer { color: red; .inner { color: blue } } .after { color: green } .last { width: 1px }");
        assert_eq!(sheet.rules.len(), 4, "{:?}", selectors_of(&sheet));
        assert_eq!(sheet.rules[0].declarations.len(), 1);
        assert_eq!(sheet.rules[1].selectors[0].compounds.len(), 2, ".outer .inner");
        assert_eq!(sheet.rules[1].declarations[0].value, "blue");
        assert_eq!(sheet.rules[3].declarations[0].property, "width");
    }

    #[test]
    fn nested_selectors_expand_against_the_parent() {
        let sheet = parse_stylesheet(".a, .b { &:hover { x: 1 } > .c { x: 2 } &.d .e { x: 3 } }");
        let hover = &sheet.rules[1];
        assert_eq!(hover.selectors.len(), 2, "one per parent");
        assert_eq!(hover.selectors[0].compounds.len(), 1, "&:hover is the same compound");
        assert_eq!(hover.selectors[0].compounds[0].classes, vec!["a".to_string()]);
        assert_eq!(sheet.rules[2].selectors[0].combinators, vec![Combinator::Child]);
        assert_eq!(sheet.rules[3].selectors[1].compounds[0].classes, vec!["b".to_string(), "d".to_string()]);
    }

    #[test]
    fn a_nested_media_rule_applies_the_parent_selectors_under_the_query() {
        let sheet = parse_stylesheet(".a { color: red; @media (min-width: 600px) { color: blue } }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[1].media.as_ref().and_then(|m| m.min_width), Some(600.0));
        assert_eq!(sheet.rules[1].selectors[0].compounds[0].classes, vec!["a".to_string()]);
    }

    #[test]
    fn braces_inside_strings_do_not_end_a_block() {
        let sheet = parse_stylesheet(".a::before { content: '}' } .b { color: red }");
        assert_eq!(sheet.rules.len(), 2);
        assert_eq!(sheet.rules[1].selectors[0].compounds[0].classes, vec!["b".to_string()]);
    }

    #[test]
    fn hostile_nesting_is_bounded() {
        let deep = format!("{}color: red{}", ".a {".repeat(10_000), "}".repeat(10_000));
        let sheet = parse_stylesheet(&(deep + " .after { x: 1 }"));
        assert!(sheet.rules.len() <= MAX_AT_RULE_NESTING_DEPTH as usize + 2);
        assert!(sheet.rules.iter().any(|r| r.selectors[0].compounds[0].classes == vec!["after".to_string()]));
        let wide = format!("{} {{ {} {{ x: 1 }} }}", vec![".p"; 100].join(","), vec![".c"; 100].join(","));
        assert_eq!(parse_stylesheet(&wide).rules.len(), 1, "100 x 100 selectors: the nested rule is dropped");
    }
}
