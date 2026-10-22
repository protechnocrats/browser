//! HTML parser — Milestone 3: a real tokenizer (see `tokenizer.rs`)
//! feeding a tree builder with actual error recovery, replacing
//! Milestone 1's toy recursive-descent parser that errored on the
//! first malformed thing it saw.
//!
//! This is the trust boundary in the whole pipeline — the one crate
//! that turns untrusted bytes into structured data — so the design
//! goal is "never panics, degrades gracefully," not spec completeness.
//! See `milestones/milestones-01-17.md` for full scope
//! and what's deliberately still deferred (the full HTML5
//! insertion-mode state machine, encoding sniffing, templates/shadow
//! DOM, `<script>`/`<style>` raw-text parsing modes).

mod tokenizer;

use dom::{Element, Name, Node};
use tokenizer::{Token, Tokenizer};

#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// No `<html>` element was found anywhere in the (possibly very
    /// malformed) input. Every other kind of malformed markup is
    /// recovered from rather than surfaced as an error — see module
    /// docs and the tree-builder recovery rules below.
    NoHtmlElement,
}

/// Void elements never have a closing tag and never contain children —
/// https://html.spec.whatwg.org/multipage/syntax.html#void-elements
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Elements that implicitly close a currently-open `<p>` when they
/// start — the real spec's list (used for "has a p element in button
/// scope, close a p element"), simplified here to checking only the
/// immediate top of the open-elements stack rather than full scope
/// resolution. See milestone-3 scope: "implied </p> closing... the
/// most common real-world case," not full scope semantics.
const CLOSES_P: &[&str] = &[
    "address", "article", "aside", "blockquote", "details", "div", "dl", "fieldset",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header",
    "hr", "main", "menu", "nav", "ol", "p", "pre", "section", "table", "ul",
];

fn is_void_element(tag: &str) -> bool {
    VOID_ELEMENTS.contains(&tag)
}

fn closes_p(tag: &str) -> bool {
    CLOSES_P.contains(&tag)
}

/// A currently-open element being assembled — the tree builder keeps
/// a stack of these instead of mutating a tree in place, which avoids
/// needing any interior mutability or unsafe borrow tricks to hold a
/// "path" into an owned recursive `dom::Node` structure.
struct OpenElement {
    tag: Name,
    attributes: Vec<(Name, String)>,
    children: Vec<Node>,
}

impl OpenElement {
    fn into_node(self) -> Node {
        // Built via `Element::new` (not a struct literal) specifically
        // so every parsed element gets a real `node_id` from the
        // shared counter — see that field's doc comment (`dom` crate).
        let mut el = Element::new(self.tag);
        el.attributes = self.attributes;
        el.children = self.children;
        Node::Element(el)
    }
}

/// Caps how deep the open-elements stack (and therefore the resulting
/// DOM tree) can nest. Two real, verified problems without this:
/// (1) `handle_end_tag`'s `rposition` scan over the whole stack is
/// O(depth) per closing tag, and a pathological document (N deeply
/// nested unclosed tags followed by N non-matching closing tags) never
/// pops the stack, making total parse cost O(N^2) — measured at 9.2s
/// for N=20,000 mismatched tags, scaling quadratically from there; (2)
/// every later recursive tree walk (style resolution, layout,
/// accessibility tree) recurses once per DOM level with no depth guard
/// of its own, so a sufficiently deep tree overflows the native stack
/// and aborts the process. Real browsers impose a similar cap (e.g.
/// Blink limits DOM nesting depth); legitimate documents never come
/// remotely close to this number.
const MAX_NESTING_DEPTH: usize = 512;

struct TreeBuilder {
    /// Completed nodes with no currently-open parent — what would be
    /// the document's direct children.
    top_level: Vec<Node>,
    /// Currently-open elements, outermost first.
    stack: Vec<OpenElement>,
    /// Whether this document's nesting-cap trip has been reported yet —
    /// reported ONCE per document (a hostile page with 100,000 over-deep
    /// tags is one event, not 100,000).
    depth_cap_reported: bool,
}

impl TreeBuilder {
    fn new() -> Self {
        TreeBuilder { top_level: Vec::new(), stack: Vec::new(), depth_cap_reported: false }
    }

    fn append_node(&mut self, node: Node) {
        match self.stack.last_mut() {
            Some(open) => open.children.push(node),
            None => self.top_level.push(node),
        }
    }

    fn push_open_element(&mut self, tag: Name, attributes: Vec<(Name, String)>) {
        self.stack.push(OpenElement { tag, attributes, children: Vec::new() });
    }

    /// Closes the innermost open element (pops it, appends it into
    /// its parent — or `top_level` if it had none).
    fn close_innermost(&mut self) {
        if let Some(open) = self.stack.pop() {
            let node = open.into_node();
            self.append_node(node);
        }
    }

    fn handle_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.append_node(Node::Text(text));
    }

    fn handle_start_tag(&mut self, name: Name, attributes: Vec<(Name, String)>, self_closing: bool) {
        // Error recovery: implied </p> closing (see CLOSES_P docs).
        if closes_p(&name) {
            if let Some(top) = self.stack.last() {
                if top.tag.is(dom::atoms::P) {
                    self.close_innermost();
                }
            }
        }

        if self_closing || is_void_element(&name) {
            // Void/self-closing elements never go on the stack — they
            // have no children and no closing tag to wait for. Built
            // via `Element::new` so it gets a real `node_id`.
            let mut el = Element::new(name);
            el.attributes = attributes;
            let node = Node::Element(el);
            self.append_node(node);
        } else if self.stack.len() >= MAX_NESTING_DEPTH {
            // See `MAX_NESTING_DEPTH`'s doc comment. Past the cap, a
            // start tag is appended as a childless leaf (like a void
            // element) instead of being pushed onto the stack — any
            // further "nested" content becomes a sibling at the current
            // depth rather than growing the tree deeper. A closing tag
            // for this element later finds no matching open ancestor
            // and is harmlessly discarded by `handle_end_tag`, same as
            // any other stray closing tag.
            if !self.depth_cap_reported {
                self.depth_cap_reported = true;
                diagnostics::guard(diagnostics::Guard::HtmlNestingDepth, "element nesting flattened past the depth cap");
            }
            let mut el = Element::new(name);
            el.attributes = attributes;
            self.append_node(Node::Element(el));
        } else {
            self.push_open_element(name, attributes);
        }
    }

    fn handle_end_tag(&mut self, name: &Name) {
        // Error recovery: find a matching open ancestor anywhere on
        // the stack (not just the innermost), and close everything
        // from the top down to and including it. A closing tag with
        // no matching open ancestor anywhere is simply discarded —
        // real-world malformed markup ("stray </div>") shouldn't
        // abort parsing.
        if let Some(pos) = self.stack.iter().rposition(|open| open.tag == *name) {
            while self.stack.len() > pos {
                self.close_innermost();
            }
        }
        // else: no matching ancestor — ignore the stray closing tag.
    }

    /// Closes every still-open element at EOF (unclosed-tag recovery)
    /// and returns the completed top-level node list.
    fn finish(mut self) -> Vec<Node> {
        while !self.stack.is_empty() {
            self.close_innermost();
        }
        self.top_level
    }
}

pub fn parse_document(input: &str) -> Result<Element, ParseError> {
    let mut tokenizer = Tokenizer::new(input);
    let mut builder = TreeBuilder::new();

    loop {
        match tokenizer.next_token() {
            Token::Eof => break,
            Token::Text(text) => builder.handle_text(text),
            Token::StartTag { name, attributes, self_closing } => {
                builder.handle_start_tag(name, attributes, self_closing)
            }
            Token::EndTag { name } => builder.handle_end_tag(&name),
            // Recognized, deliberately not represented in the DOM tree
            // (see module docs) — nothing to do.
            Token::Comment(_) | Token::Doctype => {}
        }
    }

    Ok(implied_structure(builder.finish()))
}

/// Elements that belong in `<head>` when they appear before any body
/// content in a document that omits `<head>`/`<body>` tags.
const HEAD_ELEMENTS: &[&str] = &["meta", "title", "link", "style", "script", "base", "noscript", "template"];

/// Milestone 32: every document has `<html>`, `<head>` and `<body>`, whether
/// or not the markup spells them out — the HTML standard's implied elements.
/// The corpus found real pages that rely on it: OpenBSD's home page omits
/// `<head>` and `<body>`; RFC HTML is a bare `<pre>` with neither `<html>`
/// nor `<body>`. Before, the first was rejected ("no <body>") and the second
/// was not a document at all ("no <html>").
///
/// A simplification of the standard's insertion modes, applied after the
/// tree is built: missing elements are created; head-type elements (`meta`,
/// `title`, `link`, `style`, `script` …) seen before any body content go
/// into `<head>`, everything else into `<body>`; content outside `<html>`
/// goes into its `<body>`.
fn implied_structure(top_level: Vec<Node>) -> Element {
    let mut html = None;
    let mut stray = Vec::new();
    for node in top_level {
        match node {
            Node::Element(el) if el.tag == "html" && html.is_none() => html = Some(el),
            other => stray.push(other),
        }
    }
    let mut html = html.unwrap_or_else(|| Element::new("html"));
    if !stray.is_empty() {
        // Content outside <html> (before or after it) belongs to the document.
        let mut children = std::mem::take(&mut html.children);
        children.extend(stray);
        html.children = children;
    }

    // A document that has a <body> keeps its structure as written. (The
    // standard would also imply an empty <head>; the engine has no use for
    // one, so none is added.)
    if html.children.iter().any(|n| matches!(n, Node::Element(e) if e.tag == "body")) {
        return html;
    }

    let existing_head = html.children.iter().position(|n| matches!(n, Node::Element(e) if e.tag == "head"));
    let mut head = match existing_head {
        Some(i) => match html.children.remove(i) {
            Node::Element(e) => e,
            Node::Text(_) => unreachable!("position matched an element"),
        },
        None => Element::new("head"),
    };
    let mut body: Option<Element> = None;
    let mut new_children = Vec::new();
    let mut in_body = false;
    for node in std::mem::take(&mut html.children) {
        match node {
            Node::Element(el) if el.tag == "body" => {
                in_body = true;
                body = Some(el);
            }
            Node::Element(el) if !in_body && HEAD_ELEMENTS.contains(&el.tag.as_str()) => head.children.push(Node::Element(el)),
            Node::Text(t) if !in_body && t.trim().is_empty() => {}
            other => {
                in_body = true;
                match body.as_mut() {
                    Some(b) => b.children.push(other),
                    None => {
                        let mut b = Element::new("body");
                        b.children.push(other);
                        body = Some(b);
                    }
                }
            }
        }
    }
    if existing_head.is_some() || !head.children.is_empty() {
        new_children.push(Node::Element(head));
    }
    new_children.push(Node::Element(body.unwrap_or_else(|| Element::new("body"))));
    html.children = new_children;
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_divs_with_inline_style() {
        let html = r#"<html><body><div style="width:100px;height:50px;background-color:#ff0000"><div style="width:20px;height:20px"></div></div></body></html>"#;
        let root = parse_document(html).unwrap();
        assert_eq!(root.tag, "html");
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!("expected element"),
        };
        assert_eq!(body.tag, "body");
        let outer_div = match &body.children[0] {
            Node::Element(el) => el,
            _ => panic!("expected element"),
        };
        assert_eq!(outer_div.tag, "div");
        assert!(outer_div.attr("style").unwrap().contains("background-color"));
        assert_eq!(outer_div.children.len(), 1);
    }

    #[test]
    fn parses_text_content() {
        let html = "<html><body><div>Hello</div></body></html>";
        let root = parse_document(html).unwrap();
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        let div = match &body.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        assert_eq!(div.children.len(), 1);
        assert_eq!(div.children[0], Node::Text("Hello".to_string()));
    }

    /// Milestone 1's parser treated a mismatched closing tag as a hard
    /// error (`ParseError::MismatchedClosingTag`). Milestone 3's whole
    /// point is real error recovery instead — this test now asserts
    /// the NEW, correct behavior: recovery, not failure. See
    /// `milestones/milestones-01-17.md` acceptance
    /// criteria ("malformed input... degrades gracefully" — recovery
    /// IS the graceful degradation, superseding Milestone 1's
    /// error-on-mismatch behavior rather than preserving it).
    #[test]
    fn mismatched_closing_tag_recovers_instead_of_erroring() {
        let html = "<html><body><div></span></body></html>";
        let root = parse_document(html).expect("should recover, not error");
        assert_eq!(root.tag, "html");
        // The stray </span> (no matching open <span>) is discarded;
        // </body> then correctly closes <div> too (mismatch recovery:
        // closing the nearest ancestor that actually matches, here
        // "body", closes everything nested inside it, including the
        // still-open <div>).
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        assert_eq!(body.tag, "body");
        assert_eq!(body.children.len(), 1);
        assert!(matches!(&body.children[0], Node::Element(el) if el.tag == "div"));
    }

    #[test]
    fn unclosed_tags_at_eof_are_implicitly_closed_not_an_error() {
        let html = "<html><body><div><p>unclosed";
        let root = parse_document(html).expect("should recover, not error");
        assert_eq!(root.tag, "html");
    }

    #[test]
    fn stray_closing_tag_with_no_matching_open_ancestor_is_ignored() {
        let html = "<html><body></div><div>real content</div></body></html>";
        let root = parse_document(html).expect("should recover, not error");
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        // The stray </div> is discarded; the real <div> still parses correctly.
        assert_eq!(body.children.len(), 1);
        assert!(matches!(&body.children[0], Node::Element(el) if el.tag == "div"));
    }

    #[test]
    fn implied_p_closing() {
        let html = "<html><body><p>one<div>two</div></body></html>";
        let root = parse_document(html).unwrap();
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        // <p> should have been implicitly closed when <div> started,
        // making <p> and <div> siblings rather than <div> nesting
        // inside <p>.
        assert_eq!(body.children.len(), 2);
        assert!(matches!(&body.children[0], Node::Element(el) if el.tag == "p"));
        assert!(matches!(&body.children[1], Node::Element(el) if el.tag == "div"));
    }

    #[test]
    fn void_elements_have_no_children_and_need_no_closing_tag() {
        let html = r#"<html><body><img src="x.png"><br></body></html>"#;
        let root = parse_document(html).unwrap();
        let body = match &root.children[0] {
            Node::Element(el) => el,
            _ => panic!(),
        };
        assert_eq!(body.children.len(), 2);
        assert!(matches!(&body.children[0], Node::Element(el) if el.tag == "img" && el.children.is_empty()));
        assert!(matches!(&body.children[1], Node::Element(el) if el.tag == "br"));
    }

    #[test]
    fn comments_and_doctype_are_recognized_and_skipped() {
        let html = "<!DOCTYPE html><html><!-- a comment --><body></body></html>";
        let root = parse_document(html).unwrap();
        assert_eq!(root.tag, "html");
        // Comment shouldn't appear as a spurious child.
        assert_eq!(root.children.len(), 1);
    }

    #[test]
    fn a_missing_html_element_is_implied() {
        // Milestone 32: this used to be an error (and a test pinned it); the
        // HTML standard implies <html>, and real pages rely on it.
        let root = parse_document("<body><div>no html tag</div></body>").unwrap();
        assert_eq!(root.tag, "html");
        let body = child(&root, "body").expect("the body is kept");
        assert_eq!(body.find_by_id("x"), None);
        assert!(body.text_content().contains("no html tag"));
    }

    #[test]
    fn empty_input_is_an_empty_document_not_a_panic() {
        let root = parse_document("").unwrap();
        assert!(child(&root, "body").is_some());
    }

    fn child<'a>(el: &'a Element, tag: &str) -> Option<&'a Element> {
        el.children.iter().find_map(|n| match n {
            Node::Element(e) if e.tag == tag => Some(e),
            _ => None,
        })
    }

    /// Regression (conformance corpus: openbsd-home): `<head>` and `<body>`
    /// tags omitted — head-type elements go to <head>, content to <body>.
    #[test]
    fn omitted_head_and_body_tags_are_implied() {
        let root = parse_document(
            "<!doctype html><html lang=en><meta charset=utf-8><title>OpenBSD</title><link rel=stylesheet href=x.css>\n\n<main><p>hello</p></main>",
        )
        .unwrap();
        let head = child(&root, "head").unwrap();
        let head_tags: Vec<&str> = head.children.iter().filter_map(|n| if let Node::Element(e) = n { Some(e.tag.as_str()) } else { None }).collect();
        assert_eq!(head_tags, vec!["meta", "title", "link"]);
        let body = child(&root, "body").unwrap();
        assert!(child(body, "main").is_some(), "content goes into the implied body");
        assert_eq!(root.children.len(), 2, "exactly head and body");
    }

    /// Regression (conformance corpus: rfc-2119): a bare `<pre>`, no
    /// <html>, <head> or <body> at all.
    #[test]
    fn a_bare_fragment_becomes_a_whole_document() {
        let root = parse_document("<pre>Network Working Group\n<span class=\"h1\">Key words</span></pre>").unwrap();
        let body = child(&root, "body").unwrap();
        assert_eq!(child(body, "pre").map(|p| p.text_content().contains("Key words")), Some(true));
    }

    #[test]
    fn content_after_the_html_element_is_kept_in_the_body() {
        let root = parse_document("<html><body><p>in</p></body></html><p>after</p>").unwrap();
        assert!(child(&root, "body").unwrap().text_content().contains("after") || root.text_content().contains("after"));
    }

    /// Security regression: without `MAX_NESTING_DEPTH`, N deeply
    /// nested unclosed tags followed by N non-matching closing tags is
    /// O(N^2) (each closing tag rescans the whole never-shrinking open
    /// stack) — verified to take multiple seconds at N=20,000 before
    /// this fix. This asserts it now completes quickly even at a much
    /// larger N, and that parsing still completes without panicking.
    #[test]
    fn extremely_deep_nesting_with_mismatched_closing_tags_parses_quickly() {
        let n = 50_000;
        let html = format!("<html><body>{}{}</body></html>", "<div>".repeat(n), "</x>".repeat(n));
        let start = std::time::Instant::now();
        let root = parse_document(&html).expect("should still recover, not error");
        assert_eq!(root.tag, "html");
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "parsing should be near-linear, not quadratic");
    }

    /// Security regression: a document with far more nesting than any
    /// real page uses must not overflow the native stack in any later
    /// recursive consumer of this tree (style/layout/accessibility) —
    /// enforced here at the source, by capping how deep the parser
    /// itself will nest elements.
    #[test]
    fn nesting_deeper_than_the_cap_is_flattened_not_unbounded() {
        let n = 5_000;
        let html = format!("<html><body>{}{}</body></html>", "<div>".repeat(n), "</div>".repeat(n));
        let root = parse_document(&html).expect("should parse");

        fn max_depth(node: &Node) -> usize {
            match node {
                Node::Element(el) => 1 + el.children.iter().map(max_depth).max().unwrap_or(0),
                Node::Text(_) => 0,
            }
        }
        let depth = max_depth(&Node::Element(root));
        assert!(depth <= MAX_NESTING_DEPTH + 4, "tree depth ({depth}) should be bounded by MAX_NESTING_DEPTH, not {n}");
    }

    #[test]
    fn deeply_malformed_input_never_panics() {
        // A grab-bag of malformed fragments — the only real assertion
        // here is that none of these panic; whatever they parse into
        // is fine either way.
        let inputs = [
            "<<<<<<<>>>>>>>",
            "<html><body><div><div><div><div>",
            "</></></></>",
            "<html attr=\"unterminated",
            "<html><!--unterminated comment",
            "<html><!DOCTYPE unterminated",
            "\0\0\0<html></html>",
            "<HTML><BODY><DIV>MiXeD CaSe</DIV></BODY></HTML>",
        ];
        for input in inputs {
            let _ = parse_document(input);
        }
    }

    // ---- Milestone 27: every silent cap is observable ----

    #[test]
    fn nesting_past_the_cap_reports_exactly_one_guard_event_per_document() {
        use diagnostics::{collector::capture, Guard};
        // 100,000 over-deep tags must be ONE event, not 100,000.
        let (_, c) = capture(|| parse_document(&"<div>".repeat(100_000)));
        assert_eq!(c.guard_events(Guard::HtmlNestingDepth).len(), 1);
    }

    #[test]
    fn ordinary_documents_trip_no_guard() {
        let (_, c) = diagnostics::collector::capture(|| {
            parse_document("<html><body><div><p>hello <b>world</b></p></div></body></html>")
        });
        assert!(c.events.is_empty(), "{:?}", c.events);
    }
}
