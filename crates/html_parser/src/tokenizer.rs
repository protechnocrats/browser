//! Real tokenizer states (subset of the WHATWG HTML5 tokenizer state
//! machine — not the full ~80-state spec, but the real shape: named
//! states, one token produced per call, rather than a recursive
//! hand-rolled reader) — Milestone 3 scope. See
//! `milestones/milestones-01-17.md`.
//!
//! Deliberately simplified: comment/DOCTYPE parsing collapse several
//! spec states into "consume until the terminator." Character
//! references (`&amp;`) ARE decoded in text content and attribute
//! values, but only the handful that cover the overwhelming majority
//! of real-world markup (`&lt;` `&gt;` `&amp;` `&quot;` `&apos;`
//! `&nbsp;`, plus numeric `&#NNN;`/`&#xHHHH;`) — not the full HTML5
//! named-character-reference table (2000+ entries); see
//! `decode_entities`. An unrecognized reference is left exactly as
//! written, the same graceful-degradation policy as everything else
//! here. Never panics on any input — every branch is bounds-checked,
//! which is the actual point of this crate for the fuzzer this
//! milestone adds.

use dom::Name;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Milestone 40b: names are born here as [`Name`]s — a well-known name
    /// is an atom, without allocating (ADR 021 §5).
    StartTag {
        name: Name,
        attributes: Vec<(Name, String)>,
        self_closing: bool,
    },
    EndTag {
        name: Name,
    },
    Text(String),
    Comment(String),
    Doctype,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Data,
    TagOpen,
    EndTagOpen,
    TagName,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    MarkupDeclarationOpen,
    CommentStart,
    Comment,
    CommentEndDash,
    CommentEnd,
    Doctype,
    BogusComment,
}

pub struct Tokenizer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    state: State,
    /// Pending tag being assembled across BeforeAttributeName/
    /// AttributeName/... states.
    tag_name: String,
    tag_attributes: Vec<(Name, String)>,
    tag_self_closing: bool,
    is_end_tag: bool,
    current_attr_name: String,
    current_attr_value: String,
    comment_buf: String,
    eof_emitted: bool,
    /// Set right after emitting a StartTag for "style" or "script" —
    /// the HTML5 spec's "raw text" parsing mode: scan literally for
    /// the matching closing tag rather than treating embedded `<`/`>`
    /// as markup. Milestone 3 deferred this ("revisit once CSS/JS
    /// exist"); Milestone 4 needs it for `<style>` content to survive
    /// intact, and `<script>` gets the same fix for free since it's
    /// the identical mechanism (content just isn't executed yet).
    raw_text_tag: Option<Name>,
    /// A token already produced but not yet returned — used when raw
    /// text mode needs to emit both a Text token and the closing
    /// EndTag from the same underlying scan; the Text comes back
    /// first, this holds the EndTag for the very next call.
    pending_token: Option<Token>,
}

impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str) -> Self {
        Tokenizer {
            chars: input.chars().peekable(),
            state: State::Data,
            tag_name: String::new(),
            tag_attributes: Vec::new(),
            tag_self_closing: false,
            is_end_tag: false,
            current_attr_name: String::new(),
            current_attr_value: String::new(),
            comment_buf: String::new(),
            eof_emitted: false,
            raw_text_tag: None,
            pending_token: None,
        }
    }

    fn reset_tag(&mut self) {
        self.tag_name.clear();
        self.tag_attributes.clear();
        self.tag_self_closing = false;
        self.is_end_tag = false;
    }

    fn finish_current_attr(&mut self) {
        if !self.current_attr_name.is_empty() {
            // The name buffer is kept for the next attribute: a
            // well-known name is an atom and allocates nothing.
            self.tag_attributes.push((Name::new(&self.current_attr_name), decode_entities(&std::mem::take(&mut self.current_attr_value))));
        } else {
            self.current_attr_value.clear();
        }
        self.current_attr_name.clear();
    }

    fn emit_start_or_end_tag(&mut self) -> Token {
        self.finish_current_attr();
        let name = Name::new(&self.tag_name);
        let self_closing = self.tag_self_closing;
        let is_end_tag = self.is_end_tag;
        let token = if is_end_tag {
            Token::EndTag { name: name.clone() }
        } else {
            Token::StartTag {
                name: name.clone(),
                attributes: std::mem::take(&mut self.tag_attributes),
                self_closing,
            }
        };
        self.reset_tag();
        self.state = State::Data;

        // Raw-text mode: a non-self-closing <style>/<script> start
        // tag means everything up to the matching closing tag is
        // literal content, not markup — see field doc comment.
        if !is_end_tag && !self_closing && (name.is(dom::atoms::STYLE) || name.is(dom::atoms::SCRIPT)) {
            self.raw_text_tag = Some(name);
        }

        token
    }

    /// Produces the next token. Returns `Token::Eof` exactly once,
    /// after which every subsequent call also returns `Token::Eof`
    /// (never panics, never loops forever — safe for a fuzzer to call
    /// in a tight loop against arbitrary bytes).
    pub fn next_token(&mut self) -> Token {
        if let Some(tok) = self.pending_token.take() {
            return tok;
        }
        if let Some(tag) = self.raw_text_tag.take() {
            return self.consume_raw_text_until_closing_tag(&tag);
        }
        if self.eof_emitted {
            return Token::Eof;
        }

        let mut text_buf = String::new();

        loop {
            let Some(&c) = self.chars.peek() else {
                // EOF mid-token: flush whatever we have rather than
                // silently dropping it or panicking.
                self.eof_emitted = true;
                if !text_buf.is_empty() {
                    return Token::Text(decode_entities(&text_buf));
                }
                return match self.state {
                    State::TagName
                    | State::BeforeAttributeName
                    | State::AttributeName
                    | State::AfterAttributeName
                    | State::BeforeAttributeValue
                    | State::AttributeValueUnquoted
                    | State::AfterAttributeValueQuoted
                    | State::SelfClosingStartTag => self.emit_start_or_end_tag(),
                    _ => Token::Eof,
                };
            };

            match self.state {
                State::Data => {
                    if c == '<' {
                        self.chars.next();
                        self.state = State::TagOpen;
                        if !text_buf.is_empty() {
                            return Token::Text(decode_entities(&text_buf));
                        }
                        // else fall through to tokenizing the tag
                    } else {
                        text_buf.push(c);
                        self.chars.next();
                    }
                }

                State::TagOpen => {
                    if c == '/' {
                        self.chars.next();
                        self.state = State::EndTagOpen;
                    } else if c == '!' {
                        self.chars.next();
                        self.state = State::MarkupDeclarationOpen;
                    } else if c.is_ascii_alphabetic() {
                        self.state = State::TagName;
                        self.is_end_tag = false;
                    } else {
                        // Not a real tag ("<" followed by something
                        // else, e.g. "< foo" or a stray "<") — treat
                        // the "<" as literal text and resume Data.
                        // Malformed-input tolerance, not a panic path.
                        self.state = State::Data;
                        return Token::Text("<".to_string());
                    }
                }

                State::EndTagOpen => {
                    if c.is_ascii_alphabetic() {
                        self.state = State::TagName;
                        self.is_end_tag = true;
                    } else if c == '>' {
                        // "</>"  — spec says ignore. Consume and stay in Data.
                        self.chars.next();
                        self.state = State::Data;
                    } else {
                        // Malformed "</" not followed by a name — bail to bogus comment.
                        self.state = State::BogusComment;
                    }
                }

                State::TagName => {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        self.tag_name.push(c.to_ascii_lowercase());
                        self.chars.next();
                    } else if c.is_whitespace() {
                        self.chars.next();
                        self.state = State::BeforeAttributeName;
                    } else if c == '/' {
                        self.chars.next();
                        self.state = State::SelfClosingStartTag;
                    } else if c == '>' {
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        // Unexpected char in a tag name — consume and
                        // drop it rather than looping forever or
                        // panicking.
                        self.chars.next();
                    }
                }

                State::BeforeAttributeName => {
                    if c.is_whitespace() {
                        self.chars.next();
                    } else if c == '/' {
                        self.chars.next();
                        self.state = State::SelfClosingStartTag;
                    } else if c == '>' {
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        self.state = State::AttributeName;
                    }
                }

                State::AttributeName => {
                    if c.is_whitespace() {
                        self.chars.next();
                        self.state = State::AfterAttributeName;
                    } else if c == '=' {
                        self.chars.next();
                        self.state = State::BeforeAttributeValue;
                    } else if c == '/' {
                        self.finish_current_attr();
                        self.chars.next();
                        self.state = State::SelfClosingStartTag;
                    } else if c == '>' {
                        self.finish_current_attr();
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        self.current_attr_name.push(c.to_ascii_lowercase());
                        self.chars.next();
                    }
                }

                State::AfterAttributeName => {
                    if c.is_whitespace() {
                        self.chars.next();
                    } else if c == '=' {
                        self.chars.next();
                        self.state = State::BeforeAttributeValue;
                    } else if c == '/' {
                        self.finish_current_attr();
                        self.chars.next();
                        self.state = State::SelfClosingStartTag;
                    } else if c == '>' {
                        self.finish_current_attr();
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        self.finish_current_attr();
                        self.state = State::AttributeName;
                    }
                }

                State::BeforeAttributeValue => {
                    if c.is_whitespace() {
                        self.chars.next();
                    } else if c == '"' {
                        self.chars.next();
                        self.state = State::AttributeValueDoubleQuoted;
                    } else if c == '\'' {
                        self.chars.next();
                        self.state = State::AttributeValueSingleQuoted;
                    } else if c == '>' {
                        self.finish_current_attr();
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        self.state = State::AttributeValueUnquoted;
                    }
                }

                State::AttributeValueDoubleQuoted => {
                    if c == '"' {
                        self.chars.next();
                        self.finish_current_attr();
                        self.state = State::AfterAttributeValueQuoted;
                    } else {
                        self.current_attr_value.push(c);
                        self.chars.next();
                    }
                }

                State::AttributeValueSingleQuoted => {
                    if c == '\'' {
                        self.chars.next();
                        self.finish_current_attr();
                        self.state = State::AfterAttributeValueQuoted;
                    } else {
                        self.current_attr_value.push(c);
                        self.chars.next();
                    }
                }

                State::AttributeValueUnquoted => {
                    if c.is_whitespace() {
                        self.chars.next();
                        self.finish_current_attr();
                        self.state = State::BeforeAttributeName;
                    } else if c == '>' {
                        self.finish_current_attr();
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        self.current_attr_value.push(c);
                        self.chars.next();
                    }
                }

                State::AfterAttributeValueQuoted => {
                    if c.is_whitespace() {
                        self.chars.next();
                        self.state = State::BeforeAttributeName;
                    } else if c == '/' {
                        self.chars.next();
                        self.state = State::SelfClosingStartTag;
                    } else if c == '>' {
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        // Missing whitespace between attributes — spec
                        // treats this as an error and reconsumes in
                        // BeforeAttributeName; we do the same.
                        self.state = State::BeforeAttributeName;
                    }
                }

                State::SelfClosingStartTag => {
                    if c == '>' {
                        self.tag_self_closing = true;
                        self.chars.next();
                        return self.emit_start_or_end_tag();
                    } else {
                        // "/" not immediately followed by ">" —
                        // spec-ish recovery: drop the stray "/" and
                        // reconsume as an attribute.
                        self.state = State::BeforeAttributeName;
                    }
                }

                State::MarkupDeclarationOpen => {
                    // Peek ahead without consuming yet, to distinguish
                    // "<!--" (comment) from "<!DOCTYPE" from anything
                    // else (bogus comment) — all without panicking on
                    // short/malformed input.
                    if self.starts_with_and_consume("--") {
                        self.comment_buf.clear();
                        self.state = State::CommentStart;
                    } else if self.consume_doctype_keyword_if_present() {
                        self.state = State::Doctype;
                    } else {
                        self.state = State::BogusComment;
                    }
                }

                State::CommentStart => {
                    self.state = State::Comment;
                }

                State::Comment => {
                    if c == '-' {
                        self.chars.next();
                        self.state = State::CommentEndDash;
                    } else {
                        self.comment_buf.push(c);
                        self.chars.next();
                    }
                }

                State::CommentEndDash => {
                    if c == '-' {
                        self.chars.next();
                        self.state = State::CommentEnd;
                    } else {
                        self.comment_buf.push('-');
                        self.comment_buf.push(c);
                        self.chars.next();
                        self.state = State::Comment;
                    }
                }

                State::CommentEnd => {
                    if c == '>' {
                        self.chars.next();
                        self.state = State::Data;
                        return Token::Comment(std::mem::take(&mut self.comment_buf));
                    } else if c == '-' {
                        self.comment_buf.push('-');
                        self.chars.next();
                    } else {
                        self.comment_buf.push('-');
                        self.comment_buf.push('-');
                        self.comment_buf.push(c);
                        self.chars.next();
                        self.state = State::Comment;
                    }
                }

                State::Doctype => {
                    if c == '>' {
                        self.chars.next();
                        self.state = State::Data;
                        return Token::Doctype;
                    } else {
                        self.chars.next();
                    }
                }

                State::BogusComment => {
                    if c == '>' {
                        self.chars.next();
                        self.state = State::Data;
                        return Token::Comment(std::mem::take(&mut self.comment_buf));
                    } else {
                        self.comment_buf.push(c);
                        self.chars.next();
                    }
                }
            }
        }
    }

    /// Scans literally for the closing tag matching `tag` (e.g.
    /// `</style>`), case-insensitively, treating everything before it
    /// as plain text content — no embedded `<`/`>` is treated as
    /// markup, per HTML5's "raw text" element parsing rule. Never
    /// panics: an unterminated raw-text element (no closing tag
    /// before EOF) just becomes one big Text token, same graceful-
    /// degradation policy as everything else in this tokenizer.
    fn consume_raw_text_until_closing_tag(&mut self, tag: &str) -> Token {
        let mut text = String::new();
        loop {
            if self.chars.peek() == Some(&'<') {
                let mut lookahead = self.chars.clone();
                lookahead.next(); // consume '<'
                if lookahead.peek() == Some(&'/') {
                    lookahead.next(); // consume '/'
                    let mut tag_matches = true;
                    for expected in tag.chars() {
                        match lookahead.next() {
                            Some(c) if c.eq_ignore_ascii_case(&expected) => continue,
                            _ => {
                                tag_matches = false;
                                break;
                            }
                        }
                    }
                    // Require a tag-name boundary right after (not
                    // another alnum char) so "</stylefoo" doesn't
                    // wrongly match "</style".
                    let boundary_ok = matches!(
                        lookahead.peek(),
                        None | Some(' ' | '\t' | '\n' | '\r' | '>' | '/')
                    );
                    if tag_matches && boundary_ok {
                        while let Some(&c) = lookahead.peek() {
                            lookahead.next();
                            if c == '>' {
                                break;
                            }
                        }
                        self.chars = lookahead;
                        self.state = State::Data;
                        self.pending_token = Some(Token::EndTag { name: Name::new(tag) });
                        return Token::Text(text);
                    }
                }
            }

            match self.chars.next() {
                Some(c) => text.push(c),
                None => {
                    self.eof_emitted = true;
                    return Token::Text(text);
                }
            }
        }
    }

    /// If the upcoming characters exactly match `s`, consumes them and
    /// returns true; otherwise consumes nothing and returns false.
    /// Used for the "--" comment-open lookahead.
    fn starts_with_and_consume(&mut self, s: &str) -> bool {
        let mut lookahead = self.chars.clone();
        for expected in s.chars() {
            match lookahead.next() {
                Some(c) if c == expected => continue,
                _ => return false,
            }
        }
        self.chars = lookahead;
        true
    }

    /// Case-insensitive "DOCTYPE" keyword match, consuming on success.
    fn consume_doctype_keyword_if_present(&mut self) -> bool {
        let mut lookahead = self.chars.clone();
        for expected in "DOCTYPE".chars() {
            match lookahead.next() {
                Some(c) if c.eq_ignore_ascii_case(&expected) => continue,
                _ => return false,
            }
        }
        self.chars = lookahead;
        true
    }
}

/// Decodes character references in text content / attribute values —
/// see this module's doc comment for exactly which ones and why the
/// set is bounded. A reference that doesn't parse (unknown name, no
/// terminating `;`, an out-of-range/invalid numeric code point) is
/// left exactly as written in the output, `&` and all — matching every
/// other malformed-input case in this tokenizer (never panics, never
/// silently drops input).
fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }

        let mut lookahead = chars.clone();
        if lookahead.peek() == Some(&'#') {
            lookahead.next();
            let is_hex = matches!(lookahead.peek(), Some('x') | Some('X'));
            if is_hex {
                lookahead.next();
            }
            let mut digits = String::new();
            while let Some(&d) = lookahead.peek() {
                let keep = if is_hex { d.is_ascii_hexdigit() } else { d.is_ascii_digit() };
                if !keep {
                    break;
                }
                digits.push(d);
                lookahead.next();
            }
            if !digits.is_empty() && lookahead.peek() == Some(&';') {
                lookahead.next();
                let code = u32::from_str_radix(&digits, if is_hex { 16 } else { 10 }).ok();
                if let Some(ch) = code.and_then(char::from_u32) {
                    out.push(ch);
                    chars = lookahead;
                    continue;
                }
            }
            out.push('&');
            continue;
        }

        let mut name = String::new();
        while let Some(&nc) = lookahead.peek() {
            if !nc.is_ascii_alphanumeric() {
                break;
            }
            name.push(nc);
            lookahead.next();
        }
        let decoded = if lookahead.peek() == Some(&';') {
            match name.as_str() {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{00A0}'),
                _ => None,
            }
        } else {
            None
        };
        if let Some(ch) = decoded {
            lookahead.next(); // consume ';'
            out.push(ch);
            chars = lookahead;
        } else {
            out.push('&');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize_all(input: &str) -> Vec<Token> {
        let mut t = Tokenizer::new(input);
        let mut out = Vec::new();
        loop {
            let tok = t.next_token();
            let is_eof = tok == Token::Eof;
            out.push(tok);
            if is_eof {
                break;
            }
        }
        out
    }

    #[test]
    fn simple_start_and_end_tag() {
        let toks = tokenize_all("<div></div>");
        assert_eq!(
            toks,
            vec![
                Token::StartTag { name: "div".into(), attributes: vec![], self_closing: false },
                Token::EndTag { name: "div".into() },
                Token::Eof,
            ]
        );
    }

    #[test]
    fn attributes_double_and_single_quoted_and_unquoted() {
        let toks = tokenize_all(r#"<div a="1" b='2' c=3></div>"#);
        let Token::StartTag { attributes, .. } = &toks[0] else { panic!() };
        assert_eq!(
            *attributes,
            vec![
                ("a".into(), "1".to_string()),
                ("b".into(), "2".to_string()),
                ("c".into(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn self_closing_tag() {
        let toks = tokenize_all("<br/>");
        assert_eq!(
            toks[0],
            Token::StartTag { name: "br".into(), attributes: vec![], self_closing: true }
        );
    }

    #[test]
    fn text_between_tags() {
        let toks = tokenize_all("<p>Hello</p>");
        assert_eq!(toks[1], Token::Text("Hello".to_string()));
    }

    #[test]
    fn comment_is_recognized() {
        let toks = tokenize_all("<!-- a comment --><div></div>");
        assert_eq!(toks[0], Token::Comment(" a comment ".to_string()));
    }

    #[test]
    fn doctype_is_recognized() {
        let toks = tokenize_all("<!DOCTYPE html><html></html>");
        assert_eq!(toks[0], Token::Doctype);
    }

    #[test]
    fn never_panics_on_truncated_tag_at_eof() {
        // Every one of these ends abruptly mid-token — the actual
        // point of this test is that none of them panic.
        for input in ["<div", "<div ", "<div a", "<div a=", "<div a=\"", "</", "<!--", "<!"] {
            let _ = tokenize_all(input);
        }
    }

    #[test]
    fn stray_less_than_is_treated_as_text() {
        let toks = tokenize_all("1 < 2");
        // "<" not followed by a valid tag-open char becomes literal text.
        assert!(toks.iter().any(|t| matches!(t, Token::Text(s) if s.contains('<'))));
    }

    #[test]
    fn style_content_is_raw_text_not_markup() {
        let toks = tokenize_all("<style>div > p { color: red; }</style>");
        assert_eq!(
            toks,
            vec![
                Token::StartTag { name: "style".into(), attributes: vec![], self_closing: false },
                Token::Text("div > p { color: red; }".to_string()),
                Token::EndTag { name: "style".into() },
                Token::Eof,
            ]
        );
    }

    #[test]
    fn style_content_with_embedded_angle_brackets_survives_intact() {
        // The exact case raw-text mode exists for: a literal '<'
        // inside the element's content that would otherwise be
        // misparsed as the start of a new tag.
        let toks = tokenize_all("<style>a[href^=\"x\"] { content: \"<weird>\"; }</style>");
        let Token::Text(content) = &toks[1] else { panic!("expected raw text token") };
        assert!(content.contains("<weird>"));
    }

    #[test]
    fn script_content_is_also_raw_text() {
        let toks = tokenize_all("<script>if (a < b) { x(); }</script>");
        let Token::Text(content) = &toks[1] else { panic!("expected raw text token") };
        assert_eq!(content, "if (a < b) { x(); }");
        assert_eq!(toks[2], Token::EndTag { name: "script".into() });
    }

    #[test]
    fn unterminated_style_tag_does_not_panic() {
        let toks = tokenize_all("<style>div { color: red;");
        // No closing tag before EOF — everything after <style> becomes
        // one Text token, no panic, no infinite loop.
        assert!(matches!(toks.last(), Some(Token::Eof)));
    }

    #[test]
    fn named_entities_decode_in_text_content() {
        let toks = tokenize_all("<p>a &lt;b&gt; c &amp; d &quot;e&quot; &apos;f&apos; &nbsp;g</p>");
        assert_eq!(toks[1], Token::Text("a <b> c & d \"e\" 'f' \u{00A0}g".to_string()));
    }

    #[test]
    fn numeric_entities_decode_decimal_and_hex() {
        let toks = tokenize_all("<p>&#65;&#x42;&#X43;</p>");
        assert_eq!(toks[1], Token::Text("ABC".to_string()));
    }

    #[test]
    fn unrecognized_or_malformed_entities_are_left_literal() {
        let toks = tokenize_all("<p>a &nosuchentity; b &amp c &#zzz; d &</p>");
        assert_eq!(toks[1], Token::Text("a &nosuchentity; b &amp c &#zzz; d &".to_string()));
    }

    #[test]
    fn entities_decode_in_attribute_values() {
        let toks = tokenize_all(r#"<a href="?a=1&amp;b=2">x</a>"#);
        let Token::StartTag { attributes, .. } = &toks[0] else { panic!() };
        assert_eq!(*attributes, vec![("href".into(), "?a=1&b=2".to_string())]);
    }

    #[test]
    fn entities_are_not_decoded_inside_raw_text() {
        // <script>/<style> content is literal per spec — decoding here
        // would be wrong (e.g. it could corrupt real JS/CSS source).
        let toks = tokenize_all("<script>var x = '&amp;';</script>");
        assert_eq!(toks[1], Token::Text("var x = '&amp;';".to_string()));
    }

    #[test]
    fn closing_tag_name_prefix_does_not_false_match() {
        // "</stylefoo>" must NOT be treated as closing </style>.
        let toks = tokenize_all("<style>a</stylefoo>b</style>");
        let Token::Text(content) = &toks[1] else { panic!() };
        assert!(content.contains("</stylefoo>"));
    }
}
