//! The request filter — Milestone 37 (`docs/adr/019-privacy-policies.md` §1).
//!
//! One engine, three list categories (ads, trackers, AI crawlers), in
//! Adblock Plus / EasyList syntax or hosts-file lines. The network layer asks
//! [`FilterEngine::check`] about every hop of every request before it is
//! sent; a [`Verdict::Block`] fails the request.
//!
//! ## What is understood
//!
//! - `||domain^…` (the host or any subdomain), `|` at either end (anchored),
//!   `*` (anything), `^` (a separator: not a letter, digit, `_ - . %`; or
//!   the end of the URL).
//! - `@@` exceptions; `$important` (an exception cannot lift it).
//! - Options: `third-party` / `~third-party`, `domain=a.com|~b.com` (the
//!   page's host), and the request types `script`, `image`, `stylesheet`,
//!   `font`, `xmlhttprequest`, `document`, `other` (and their `~` negations).
//!   A rule without a type applies to every type but `document`: a top-level
//!   navigation is blocked only by a rule that says `$document`.
//! - Hosts-file lines: `0.0.0.0 example.com`, `127.0.0.1 example.com`.
//!
//! Element hiding (`##`), regular-expression rules and options this engine
//! cannot evaluate (`csp=`, `redirect=`, `match-case`, …) are **skipped**,
//! never guessed at; a malformed or oversized rule is **rejected**. Both are
//! counted in the [`ListReport`].
//!
//! ## Bounds
//!
//! A list over [`MAX_LIST_BYTES`], a rule over [`MAX_RULE_BYTES`], and more
//! than [`MAX_RULES`] rules in one engine are refused. A request examines
//! only candidate rules: pure-domain rules by a hash lookup per label of the
//! host, the rest through a token index (each rule is filed under one token
//! of its pattern that must appear whole in a matching URL), plus the few
//! rules with no usable token. Matching one rule is a wildcard match over at
//! most [`MAX_MATCH_URL_BYTES`] of the URL.

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Largest list accepted.
pub const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
/// Longest rule accepted.
pub const MAX_RULE_BYTES: usize = 1024;
/// Most rules one engine holds, over all its lists.
pub const MAX_RULES: usize = 300_000;
/// How much of a URL is matched against (the rest is ignored).
pub const MAX_MATCH_URL_BYTES: usize = 4096;

/// The project's own short list (third-party analytics and advertising
/// hosts). See `built_in.txt`.
pub const BUILT_IN_TRACKERS: &str = include_str!("built_in.txt");

/// What a list blocks — the reason a request was blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Ads,
    Trackers,
    AiCrawlers,
}

impl Category {
    pub const ALL: [Category; 3] = [Category::Ads, Category::Trackers, Category::AiCrawlers];

    /// Stable name: the prefix of list files in a profile's `filters/`.
    pub const fn name(self) -> &'static str {
        match self {
            Category::Ads => "ads",
            Category::Trackers => "trackers",
            Category::AiCrawlers => "ai",
        }
    }
}

/// What a request is for — what `$script`, `$image`, … options test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestType {
    /// A top-level navigation.
    Document,
    Script,
    Image,
    Stylesheet,
    Font,
    /// `fetch()` and `XMLHttpRequest`.
    XmlHttpRequest,
    #[default]
    Other,
}

impl RequestType {
    const fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

const ALL_TYPES: u16 = (1 << 7) - 1;
/// What a rule with no type option applies to: everything but navigations.
const DEFAULT_TYPES: u16 = ALL_TYPES & !RequestType::Document.bit();

/// One request, as the filter sees it.
#[derive(Debug, Clone, Copy)]
pub struct FilterRequest<'a> {
    pub url: &'a str,
    pub kind: RequestType,
    /// The host of the page making the request (what `domain=` matches);
    /// `None` for a request no page made (a typed URL).
    pub page_host: Option<&'a str>,
    /// Whether the request's site differs from the page's.
    pub third_party: bool,
}

/// The filter's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Block { category: Category, rule: String },
}

/// What reading one list did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListReport {
    /// Rules now in force.
    pub rules: usize,
    /// Rules of a kind this engine does not evaluate (element hiding,
    /// regular expressions, unsupported options) — left out on purpose.
    pub skipped: usize,
    /// Malformed or oversized lines.
    pub rejected: usize,
}

/// Why a whole list was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListError {
    TooLarge,
    TooManyRules,
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListError::TooLarge => write!(f, "the list is larger than {MAX_LIST_BYTES} bytes"),
            ListError::TooManyRules => write!(f, "the filter would hold more than {MAX_RULES} rules"),
        }
    }
}

impl std::error::Error for ListError {}

/// One step of a compiled pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Byte(u8),
    /// `^`: a separator, or the end of the URL.
    Sep,
    /// `*`, and the implicit ends of an unanchored pattern.
    Star,
    /// The end of the URL (a `|` at the pattern's end).
    End,
}

/// The byte matched for "the end of the URL" (URLs here are ASCII).
const END: u8 = 0;

fn is_separator(b: u8) -> bool {
    b == END || !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'%'))
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'%'
}

impl Tok {
    fn matches(self, b: u8) -> bool {
        match self {
            Tok::Byte(c) => c == b,
            Tok::Sep => is_separator(b),
            Tok::End => b == END,
            Tok::Star => false,
        }
    }
}

/// Whether `toks` matches all of `text`: `Star` matches any run (the
/// classic two-pointer wildcard match, O(text × pattern) at worst).
fn glob(toks: &[Tok], text: &[u8]) -> bool {
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < toks.len() && toks[p] == Tok::Star {
            star = Some((p + 1, t));
            p += 1;
        } else if p < toks.len() && toks[p].matches(text[t]) {
            t += 1;
            p += 1;
        } else if let Some((after, from)) = star {
            p = after;
            t = from + 1;
            star = Some((after, from + 1));
        } else {
            return false;
        }
    }
    toks[p..].iter().all(|&tok| tok == Tok::Star)
}

#[derive(Debug)]
struct Rule {
    text: Box<str>,
    category: Category,
    exception: bool,
    important: bool,
    /// `||`: matched from the start of the host or of any of its labels.
    host_anchor: bool,
    /// Compiled; starts with `Star` unless anchored at the start.
    toks: Box<[Tok]>,
    third_party: Option<bool>,
    types: u16,
    include_domains: Box<[Box<str>]>,
    exclude_domains: Box<[Box<str>]>,
}

/// Is `host` `domain` or a subdomain of it?
fn host_within(host: &str, domain: &str) -> bool {
    host == domain || (host.len() > domain.len() && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

impl Rule {
    fn options_allow(&self, request: &FilterRequest) -> bool {
        if self.types & request.kind.bit() == 0 {
            return false;
        }
        if self.third_party.is_some_and(|want| want != request.third_party) {
            return false;
        }
        if !self.include_domains.is_empty() || !self.exclude_domains.is_empty() {
            let Some(page) = request.page_host else { return self.include_domains.is_empty() };
            if !self.include_domains.is_empty() && !self.include_domains.iter().any(|d| host_within(page, d)) {
                return false;
            }
            if self.exclude_domains.iter().any(|d| host_within(page, d)) {
                return false;
            }
        }
        true
    }

    /// The pattern against the URL (`text` = the URL's bytes plus [`END`]),
    /// whose host spans `host`.
    fn pattern_matches(&self, text: &[u8], host: std::ops::Range<usize>) -> bool {
        if !self.host_anchor {
            return glob(&self.toks, text);
        }
        // From the start of the host, and after each dot in it.
        let starts = std::iter::once(host.start).chain((host.start..host.end).filter(|&i| text[i] == b'.').map(|i| i + 1));
        for start in starts {
            if glob(&self.toks, &text[start..]) {
                return true;
            }
        }
        false
    }
}

/// FNV-1a: token hashes for the index (no allocation per token).
fn hash_token(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The token a rule is filed under: the longest run of token bytes in its
/// pattern that is bounded on both sides by something that is not a token
/// byte (so it appears *whole* in any URL the rule matches) — or `None`.
fn index_token(rule: &Rule) -> Option<u64> {
    let toks = &rule.toks;
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < toks.len() {
        let start = i;
        while i < toks.len() && matches!(toks[i], Tok::Byte(b) if is_token_byte(b)) {
            i += 1;
        }
        if i > start {
            let left_bounded = match start.checked_sub(1).map(|j| toks[j]) {
                None => rule.host_anchor, // the start of a host label
                Some(Tok::Byte(b)) => !is_token_byte(b),
                Some(Tok::Sep) => true,
                Some(Tok::Star | Tok::End) => false,
            };
            let right_bounded = match toks.get(i) {
                None => false,
                Some(Tok::Byte(b)) => !is_token_byte(*b),
                Some(Tok::Sep | Tok::End) => true,
                Some(Tok::Star) => false,
            };
            if left_bounded && right_bounded && i - start >= 2 && best.is_none_or(|(s, e)| e - s < i - start) {
                best = Some((start, i));
            }
        } else {
            i += 1;
        }
    }
    let (s, e) = best?;
    let bytes: Vec<u8> = toks[s..e].iter().map(|t| if let Tok::Byte(b) = t { *b } else { 0 }).collect();
    Some(hash_token(&bytes))
}

enum Parsed {
    Rule(Rule),
    /// A rule for a whole domain and nothing else: `||example.com^`, or a
    /// hosts-file line, with options.
    DomainRule(Box<str>, Rule),
    Skip,
    Reject,
    Nothing,
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty() && d.len() <= 253 && d.contains('.') && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-') && !d.starts_with('.') && !d.ends_with('.')
}

fn parse_line(line: &str, category: Category) -> Parsed {
    let line = line.trim();
    if line.is_empty() || line.starts_with('!') || line.starts_with('[') {
        return Parsed::Nothing;
    }
    if line.len() > MAX_RULE_BYTES {
        return Parsed::Reject;
    }
    if line.contains("##") || line.contains("#@#") || line.contains("#?#") || line.contains("#$#") {
        return Parsed::Skip; // element hiding: not a request filter
    }
    if line.starts_with('#') {
        return Parsed::Nothing; // a hosts-file comment
    }
    let text: Box<str> = line.chars().take(200).collect::<String>().into();
    let lower = line.to_ascii_lowercase();

    // Hosts-file line.
    let mut words = lower.split_whitespace();
    if let (Some(ip), Some(domain)) = (words.next(), words.next()) {
        if matches!(ip, "0.0.0.0" | "127.0.0.1" | "::" | "::1" | "::0") {
            let domain = domain.trim_end_matches('.');
            if domain == "localhost" || domain.parse::<std::net::IpAddr>().is_ok() || domain.ends_with(".localdomain") {
                return Parsed::Nothing;
            }
            if !valid_domain(domain) {
                return Parsed::Reject;
            }
            let rule = Rule {
                text,
                category,
                exception: false,
                important: false,
                host_anchor: true,
                toks: Box::new([]),
                third_party: None,
                types: DEFAULT_TYPES,
                include_domains: Box::new([]),
                exclude_domains: Box::new([]),
            };
            return Parsed::DomainRule(domain.into(), rule);
        }
        if lower.split_whitespace().count() > 1 {
            // Patterns hold no spaces; options can (`csp=…`), and those are
            // options this engine does not evaluate.
            return if lower.contains('$') { Parsed::Skip } else { Parsed::Reject };
        }
    }

    let (exception, body) = match lower.strip_prefix("@@") {
        Some(rest) => (true, rest),
        None => (false, lower.as_str()),
    };
    if body.len() > 1 && body.starts_with('/') && body.ends_with('/') {
        return Parsed::Skip; // a regular expression
    }
    // Options follow the last `$`, if what follows looks like options.
    let (pattern, options) = match body.rfind('$') {
        Some(i) if body[i + 1..].bytes().all(|b| b.is_ascii_alphanumeric() || b"~,=|.-_*".contains(&b)) => (&body[..i], Some(&body[i + 1..])),
        _ => (body, None),
    };

    let mut third_party = None;
    let mut important = false;
    let (mut include_types, mut exclude_types) = (0u16, 0u16);
    let (mut include_domains, mut exclude_domains): (Vec<Box<str>>, Vec<Box<str>>) = (Vec::new(), Vec::new());
    for option in options.into_iter().flat_map(|o| o.split(',')) {
        let (negated, name) = match option.strip_prefix('~') {
            Some(n) => (true, n),
            None => (false, option),
        };
        let kind = match name {
            "script" => Some(RequestType::Script),
            "image" => Some(RequestType::Image),
            "stylesheet" => Some(RequestType::Stylesheet),
            "font" => Some(RequestType::Font),
            "xmlhttprequest" => Some(RequestType::XmlHttpRequest),
            "document" => Some(RequestType::Document),
            "other" => Some(RequestType::Other),
            _ => None,
        };
        if let Some(kind) = kind {
            if negated {
                exclude_types |= kind.bit();
            } else {
                include_types |= kind.bit();
            }
            continue;
        }
        match name {
            "third-party" | "3p" => third_party = Some(!negated),
            "first-party" | "1p" => third_party = Some(negated),
            "important" if !negated => important = true,
            _ if !negated && name.starts_with("domain=") => {
                for d in name["domain=".len()..].split('|') {
                    let (neg, d) = match d.strip_prefix('~') {
                        Some(d) => (true, d),
                        None => (false, d),
                    };
                    if !valid_domain(d) {
                        return Parsed::Reject;
                    }
                    if neg {
                        exclude_domains.push(d.into());
                    } else {
                        include_domains.push(d.into());
                    }
                }
            }
            // Types this engine never produces: such a rule cannot apply.
            "subdocument" | "media" | "object" | "ping" | "websocket" | "popup" | "webrtc" => return Parsed::Skip,
            // Anything else (csp=, redirect=, match-case, removeparam=, …)
            // changes what the rule means: never guess.
            _ => return Parsed::Skip,
        }
    }
    let types = if include_types != 0 { include_types } else { DEFAULT_TYPES } & !exclude_types;

    // The pattern.
    let (host_anchor, rest) = match pattern.strip_prefix("||") {
        Some(r) => (true, r),
        None => (false, pattern),
    };
    let (start_anchor, rest) = match (host_anchor, rest.strip_prefix('|')) {
        (false, Some(r)) => (true, r),
        _ => (false, rest),
    };
    let (end_anchor, rest) = match rest.strip_suffix('|') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    if rest.is_empty() && !host_anchor {
        // `*` alone, `$third-party` alone: matches everything — refuse.
        return Parsed::Reject;
    }
    let mut toks = Vec::with_capacity(rest.len() + 2);
    if !start_anchor && !host_anchor {
        toks.push(Tok::Star);
    }
    for b in rest.bytes() {
        match b {
            b'*' => {
                if toks.last() != Some(&Tok::Star) {
                    toks.push(Tok::Star);
                }
            }
            b'^' => toks.push(Tok::Sep),
            b'|' => return Parsed::Reject,
            0 => return Parsed::Reject,
            b if b.is_ascii() => toks.push(Tok::Byte(b)),
            _ => return Parsed::Reject,
        }
    }
    if end_anchor {
        toks.push(Tok::End);
    } else if toks.last() != Some(&Tok::Star) {
        toks.push(Tok::Star);
    }
    if toks.iter().all(|t| *t == Tok::Star) {
        return Parsed::Reject;
    }
    let rule = Rule {
        text,
        category,
        exception,
        important,
        host_anchor,
        toks: toks.into_boxed_slice(),
        third_party,
        types,
        include_domains: include_domains.into_boxed_slice(),
        exclude_domains: exclude_domains.into_boxed_slice(),
    };
    // `||example.com^` and nothing more: a domain rule.
    if host_anchor && !end_anchor {
        if let Some(domain) = rest.strip_suffix('^') {
            if valid_domain(domain) {
                return Parsed::DomainRule(domain.into(), rule);
            }
        }
    }
    Parsed::Rule(rule)
}

/// The filter: every loaded list's rules, indexed for matching.
#[derive(Default)]
pub struct FilterEngine {
    rules: Vec<Rule>,
    /// Pure-domain rules, by domain.
    by_domain: HashMap<Box<str>, Vec<u32>>,
    /// Other rules, by one whole token of their pattern.
    by_token: HashMap<u64, Vec<u32>>,
    /// Rules with no usable token: examined for every request.
    unindexed: Vec<u32>,
}

impl std::fmt::Debug for FilterEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterEngine").field("rules", &self.rules.len()).field("unindexed", &self.unindexed.len()).finish()
    }
}

/// Where the host is in a URL's bytes (after `scheme://` and any userinfo,
/// up to the port, path, query or fragment).
fn host_range(url: &[u8]) -> std::ops::Range<usize> {
    let start = url.windows(3).position(|w| w == b"://").map_or(0, |i| i + 3);
    let end = url[start..].iter().position(|&b| matches!(b, b'/' | b'?' | b'#' | END)).map_or(url.len(), |i| start + i);
    let start = url[start..end].iter().rposition(|&b| b == b'@').map_or(start, |i| start + i + 1);
    let end = url[start..end].iter().position(|&b| b == b':').map_or(end, |i| start + i);
    start..end
}

impl FilterEngine {
    pub fn new() -> FilterEngine {
        FilterEngine::default()
    }

    /// An engine holding the project's built-in tracker list.
    pub fn with_built_in() -> FilterEngine {
        let mut engine = FilterEngine::new();
        engine.add_list(Category::Trackers, BUILT_IN_TRACKERS).expect("the built-in list is within the limits");
        engine
    }

    /// Rules in force.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Rules every request examines (no token to index them by).
    pub fn unindexed(&self) -> usize {
        self.unindexed.len()
    }

    /// Adds a list's rules. Refuses the whole list if it is too large, or if
    /// it would take the engine past [`MAX_RULES`] (nothing is added then).
    pub fn add_list(&mut self, category: Category, text: &str) -> Result<ListReport, ListError> {
        if text.len() > MAX_LIST_BYTES {
            diagnostics::guard(diagnostics::Guard::FilterListRejected, "a filter list larger than the cap was refused");
            return Err(ListError::TooLarge);
        }
        let mut report = ListReport::default();
        let mut parsed = Vec::new();
        for line in text.lines() {
            match parse_line(line, category) {
                Parsed::Rule(rule) => parsed.push((None, rule)),
                Parsed::DomainRule(domain, rule) => parsed.push((Some(domain), rule)),
                Parsed::Skip => report.skipped += 1,
                Parsed::Reject => report.rejected += 1,
                Parsed::Nothing => {}
            }
            if self.rules.len() + parsed.len() > MAX_RULES {
                diagnostics::guard(diagnostics::Guard::FilterListRejected, "a filter list would exceed the rule cap; refused");
                return Err(ListError::TooManyRules);
            }
        }
        report.rules = parsed.len();
        for (domain, rule) in parsed {
            let id = self.rules.len() as u32;
            match domain {
                Some(domain) => self.by_domain.entry(domain).or_default().push(id),
                None => match index_token(&rule) {
                    Some(token) => self.by_token.entry(token).or_default().push(id),
                    None => self.unindexed.push(id),
                },
            }
            self.rules.push(rule);
        }
        if report.rejected > 0 {
            diagnostics::guard(diagnostics::Guard::FilterListRejected, "a filter list had malformed rules; they were dropped");
        }
        Ok(report)
    }

    /// Whether `request` may be sent. An exception lifts a block unless the
    /// block is `$important`.
    pub fn check(&self, request: &FilterRequest) -> Verdict {
        if self.rules.is_empty() {
            return Verdict::Allow;
        }
        let mut url: Vec<u8> = request.url.bytes().take(MAX_MATCH_URL_BYTES).map(|b| b.to_ascii_lowercase()).collect();
        url.push(END);
        let host = host_range(&url);
        let host_str = std::str::from_utf8(&url[host.clone()]).unwrap_or("").trim_end_matches('.');
        let page_host = request.page_host.map(|h| h.to_ascii_lowercase());
        let request = FilterRequest { page_host: page_host.as_deref(), ..*request };

        let mut found = Found::default();

        // Domain rules: the host and each parent domain.
        let mut suffix = host_str;
        loop {
            if let Some(ids) = self.by_domain.get(suffix) {
                for &id in ids {
                    if found.consider(&self.rules[id as usize], true, &request) {
                        return found.verdict();
                    }
                }
            }
            match suffix.find('.') {
                Some(i) => suffix = &suffix[i + 1..],
                None => break,
            }
        }
        // Indexed rules: by each whole token of the URL.
        let text = &url[..url.len() - 1];
        let mut i = 0;
        while i < text.len() {
            if !is_token_byte(text[i]) {
                i += 1;
                continue;
            }
            let start = i;
            while i < text.len() && is_token_byte(text[i]) {
                i += 1;
            }
            if let Some(ids) = self.by_token.get(&hash_token(&text[start..i])) {
                for &id in ids {
                    let rule = &self.rules[id as usize];
                    if found.consider(rule, rule.options_allow(&request) && rule.pattern_matches(&url, host.clone()), &request) {
                        return found.verdict();
                    }
                }
            }
        }
        for &id in &self.unindexed {
            let rule = &self.rules[id as usize];
            if found.consider(rule, rule.options_allow(&request) && rule.pattern_matches(&url, host.clone()), &request) {
                return found.verdict();
            }
        }
        found.verdict()
    }
}

/// What the rules examined so far say.
#[derive(Default)]
struct Found<'r> {
    block: Option<&'r Rule>,
    excepted: bool,
}

impl<'r> Found<'r> {
    /// Takes `rule` into account if it `matched`; `true` once an
    /// `$important` block settles the answer.
    fn consider(&mut self, rule: &'r Rule, matched: bool, request: &FilterRequest) -> bool {
        if !matched || !rule.options_allow(request) {
            return false;
        }
        if rule.exception {
            self.excepted = true;
        } else if rule.important {
            self.block = Some(rule);
            return true;
        } else if self.block.is_none() {
            self.block = Some(rule);
        }
        false
    }

    fn verdict(&self) -> Verdict {
        match self.block {
            Some(rule) if !self.excepted || rule.important => Verdict::Block { category: rule.category, rule: rule.text.to_string() },
            _ => Verdict::Allow,
        }
    }
}

#[cfg(test)]
mod tests;
