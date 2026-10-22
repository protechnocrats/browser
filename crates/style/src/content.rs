//! Milestone 33: generated content — `::before` / `::after` identities and
//! the `content` property's value.
//!
//! A pseudo-element's style lives in the `StyleCache` under a node id
//! derived from its owner's (a reserved high bit set), so layout, reuse and
//! hit testing handle it like any element; [`owner_of`] maps it back (a
//! click on generated content is a click on its element).

use dom::{ElementRef, NodeId};

// The two bits just above every element id (`dom::MAX_ELEMENT_NODE_ID`).
const AFTER_BIT: NodeId = dom::MAX_ELEMENT_NODE_ID + 1;
const BEFORE_BIT: NodeId = AFTER_BIT << 1;

/// Longest generated text kept (characters).
pub const MAX_CONTENT_CHARS: usize = 4096;

/// Which box-generating pseudo-element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
}

/// The node id a pseudo-element's style is stored under.
pub fn pseudo_id(owner: NodeId, which: PseudoElement) -> NodeId {
    owner | match which {
        PseudoElement::Before => BEFORE_BIT,
        PseudoElement::After => AFTER_BIT,
    }
}

/// Whether `id` names a pseudo-element.
pub fn is_pseudo(id: NodeId) -> bool {
    id & (BEFORE_BIT | AFTER_BIT) != 0
}

/// The element a (pseudo-element) id belongs to; an element's own id is
/// its own owner.
pub fn owner_of(id: NodeId) -> NodeId {
    id & !(BEFORE_BIT | AFTER_BIT)
}

/// The text a `content` value generates for `element`, or `None` when it
/// generates no box (`none`, `normal`, or an invalid value). Strings (with
/// CSS escapes) and `attr()` are concatenated; counters, quotes and images
/// are counted as unsupported and generate nothing (the box itself still
/// exists).
pub fn resolve_content<'a>(value: &str, element: impl Into<ElementRef<'a>>) -> Option<String> {
    let element = element.into();
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("normal") {
        return None;
    }
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                let quote = c;
                loop {
                    match chars.next() {
                        None => break, // unterminated: take what there is
                        Some(q) if q == quote => break,
                        Some('\\') => {
                            if let Some(e) = unescape(&mut chars) {
                                out.push(e);
                            }
                        }
                        Some(ch) => out.push(ch),
                    }
                }
            }
            c if c.is_whitespace() => {}
            c if c.is_ascii_alphabetic() || c == '-' => {
                let mut name = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphanumeric() || n == '-' {
                        name.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let mut arg = String::new();
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    for ch in chars.by_ref() {
                        match ch {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        arg.push(ch);
                    }
                }
                match name.to_ascii_lowercase().as_str() {
                    "attr" => out.push_str(element.attr(arg.trim()).unwrap_or("")),
                    other => diagnostics::unsupported::record("css-value", &format!("content: {other}")),
                }
            }
            _ => return None, // not a valid content value
        }
        if out.chars().count() > MAX_CONTENT_CHARS {
            return Some(out.chars().take(MAX_CONTENT_CHARS).collect());
        }
    }
    Some(out)
}

/// A CSS escape after `\`: up to six hex digits (and one optional space),
/// or the next character itself; an escaped newline is nothing.
fn unescape(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<char> {
    let mut hex = String::new();
    while hex.len() < 6 {
        match chars.peek() {
            Some(h) if h.is_ascii_hexdigit() => {
                hex.push(*h);
                chars.next();
            }
            _ => break,
        }
    }
    if hex.is_empty() {
        return match chars.next() {
            Some('\n') | None => None,
            Some(c) => Some(c),
        };
    }
    if chars.peek() == Some(&' ') {
        chars.next();
    }
    let code = u32::from_str_radix(&hex, 16).ok()?;
    // U+0000, surrogates and beyond U+10FFFF become the replacement character.
    Some(char::from_u32(code).filter(|c| *c != '\0').unwrap_or('\u{FFFD}'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom::Element;

    #[test]
    fn strings_escapes_and_attr() {
        let mut el = Element::new("a");
        el.set_attr("href", "/x");
        assert_eq!(resolve_content(r#""→ " attr(href) ' !'"#, &el).as_deref(), Some("→ /x !"));
        assert_eq!(resolve_content(r#""\2014 \"q\"""#, &el).as_deref(), Some("—\"q\""));
        assert_eq!(resolve_content(r#""""#, &el).as_deref(), Some(""), "an empty string still generates a box");
        assert_eq!(resolve_content("none", &el), None);
        assert_eq!(resolve_content("normal", &el), None);
        assert_eq!(resolve_content("counter(x)", &el).as_deref(), Some(""));
        assert_eq!(resolve_content(r#""\0 \110000 \d800""#, &el).as_deref(), Some("\u{FFFD}\u{FFFD}\u{FFFD}"));
        assert_eq!(resolve_content("12", &el), None);
    }

    #[test]
    fn ids_round_trip_and_generated_text_is_bounded() {
        let id = pseudo_id(42, PseudoElement::After);
        assert!(is_pseudo(id) && !is_pseudo(42));
        assert_eq!(owner_of(id), 42);
        assert_ne!(pseudo_id(42, PseudoElement::Before), id);
        let el = Element::new("p");
        let long = format!("\"{}\"", "x".repeat(10_000));
        assert_eq!(resolve_content(&long, &el).map(|s| s.len()), Some(MAX_CONTENT_CHARS));
    }
}
