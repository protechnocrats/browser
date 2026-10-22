//! A parser for the WebIDL subset the script bindings are declared in
//! (Milestone 40, ADR 023). `script`'s build script reads
//! `crates/script/webidl/*.webidl` with it and generates the bindings.
//!
//! ## The subset
//!
//! - `interface Name : Parent { … };` and `partial interface Name { … };`,
//!   with extended attributes (`[Global]`, `[SameObject]`, …) kept as text;
//! - `namespace name { … };`;
//! - `dictionary Name { type member; … };` (members are all optional);
//! - `enum Name { "a", "b" };`;
//! - `callback Name = ReturnType (arguments);`;
//! - members: `[readonly] attribute Type name;`, `constructor(arguments);`,
//!   `ReturnType name(arguments);`;
//! - arguments: `[optional] Type[...] name [= default]`;
//! - types: `undefined`, `boolean`, `short`, `long`, `unsigned short`,
//!   `unsigned long`, `double`, `DOMString`, `USVString`, `ByteString`,
//!   `any`, `object`, `Promise<T>`, `sequence<T>`, a named type, each with an
//!   optional `?`.
//!
//! Anything else is an error with its line, never a guess. The input is the
//! engine's own files, read at build time; it is still bounded (input
//! size, nesting) and fuzzed (`fuzz/fuzz_targets/webidl_parse.rs`), as every
//! parser here is.

#![forbid(unsafe_code)]

use std::fmt;

/// Largest input accepted, in bytes.
pub const MAX_INPUT_BYTES: usize = 1 << 20;

/// Deepest type nesting (`Promise<sequence<…>>`) accepted.
pub const MAX_TYPE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub enum Definition {
    Interface(Interface),
    Namespace(Interface),
    Dictionary(Dictionary),
    Enum(Enum),
    Callback(Callback),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub name: String,
    pub parent: Option<String>,
    pub partial: bool,
    pub ext_attrs: Vec<String>,
    pub members: Vec<Member>,
}

impl Interface {
    pub fn has_ext_attr(&self, name: &str) -> bool {
        self.ext_attrs.iter().any(|a| a == name || a.starts_with(&format!("{name}=")))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Member {
    Attribute { name: String, ty: Type, readonly: bool, ext_attrs: Vec<String> },
    Operation { name: String, args: Vec<Argument>, returns: Type, ext_attrs: Vec<String> },
    Constructor { args: Vec<Argument> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Argument {
    pub name: String,
    pub ty: Type,
    pub optional: bool,
    pub variadic: bool,
    pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dictionary {
    pub name: String,
    pub members: Vec<(Type, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub name: String,
    pub returns: Type,
    pub args: Vec<Argument>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Undefined,
    Boolean,
    Short,
    Long,
    UnsignedShort,
    UnsignedLong,
    Double,
    DomString,
    UsvString,
    ByteString,
    Any,
    Object,
    Promise(Box<Type>),
    Sequence(Box<Type>),
    Named(String),
    Nullable(Box<Type>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Number(f64),
    String(String),
    Null,
    EmptyList,
    EmptyDictionary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Number(f64),
    Punct(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<(Token, usize)>, Error> {
    const PUNCT: [&str; 14] = ["...", "{", "}", "(", ")", "[", "]", "<", ">", ";", ":", ",", "=", "?"];
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else if src[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if src[i..].starts_with("/*") {
            let Some(end) = src[i + 2..].find("*/") else { return Err(Error { line, message: "unterminated comment".into() }) };
            line += src[i..i + 2 + end].matches('\n').count();
            i += end + 4;
        } else if c == b'"' {
            let Some(end) = src[i + 1..].find('"') else { return Err(Error { line, message: "unterminated string".into() }) };
            let text = &src[i + 1..i + 1 + end];
            if text.contains('\n') {
                return Err(Error { line, message: "a string spans lines".into() });
            }
            out.push((Token::Str(text.to_string()), line));
            i += end + 2;
        } else if c.is_ascii_digit() || (c == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let number = src[start..i].parse().map_err(|_| Error { line, message: format!("bad number `{}`", &src[start..i]) })?;
            out.push((Token::Number(number), line));
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            out.push((Token::Ident(src[start..i].to_string()), line));
        } else if let Some(p) = PUNCT.iter().find(|p| src[i..].starts_with(**p)) {
            out.push((Token::Punct(p), line));
            i += p.len();
        } else {
            let ch = src[i..].chars().next().unwrap_or('?');
            return Err(Error { line, message: format!("unexpected character `{ch}`") });
        }
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

impl Parser {
    fn line(&self) -> usize {
        self.tokens.get(self.pos).or(self.tokens.last()).map_or(1, |t| t.1)
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error { line: self.line(), message: message.into() })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|t| &t.0)
    }

    fn peek_is(&self, punct: &str) -> bool {
        matches!(self.peek(), Some(Token::Punct(p)) if *p == punct)
    }

    fn peek_ident(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(w)) if w == word)
    }

    fn eat(&mut self, punct: &str) -> bool {
        let found = self.peek_is(punct);
        if found {
            self.pos += 1;
        }
        found
    }

    fn eat_ident(&mut self, word: &str) -> bool {
        let found = self.peek_ident(word);
        if found {
            self.pos += 1;
        }
        found
    }

    fn expect(&mut self, punct: &str) -> Result<(), Error> {
        if self.eat(punct) {
            Ok(())
        } else {
            self.error(format!("expected `{punct}`"))
        }
    }

    fn ident(&mut self) -> Result<String, Error> {
        match self.peek() {
            Some(Token::Ident(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => self.error("expected a name"),
        }
    }

    /// `[A, B=C, D(x)]`: each top-level entry as written, spaces removed.
    fn ext_attrs(&mut self) -> Result<Vec<String>, Error> {
        if !self.eat("[") {
            return Ok(Vec::new());
        }
        let mut attrs = Vec::new();
        let mut current = String::new();
        let mut depth = 0usize;
        loop {
            let Some((token, _)) = self.tokens.get(self.pos).cloned() else { return self.error("unterminated extended attributes") };
            self.pos += 1;
            match token {
                Token::Punct("]") if depth == 0 => break,
                Token::Punct(",") if depth == 0 => attrs.push(std::mem::take(&mut current)),
                Token::Punct(p) => {
                    match p {
                        "(" | "[" => depth += 1,
                        ")" | "]" => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    current.push_str(p);
                }
                Token::Ident(w) => current.push_str(&w),
                Token::Str(s) => current.push_str(&format!("\"{s}\"")),
                Token::Number(n) => current.push_str(&n.to_string()),
            }
        }
        if !current.is_empty() {
            attrs.push(current);
        }
        Ok(attrs)
    }

    fn ty(&mut self, depth: usize) -> Result<Type, Error> {
        if depth > MAX_TYPE_DEPTH {
            return self.error("types nested too deeply");
        }
        let name = self.ident()?;
        let base = match name.as_str() {
            "undefined" => Type::Undefined,
            "boolean" => Type::Boolean,
            "short" => Type::Short,
            "long" => Type::Long,
            "double" => Type::Double,
            "unsigned" => {
                if self.eat_ident("short") {
                    Type::UnsignedShort
                } else if self.eat_ident("long") {
                    Type::UnsignedLong
                } else {
                    return self.error("expected `short` or `long` after `unsigned`");
                }
            }
            "DOMString" => Type::DomString,
            "USVString" => Type::UsvString,
            "ByteString" => Type::ByteString,
            "any" => Type::Any,
            "object" => Type::Object,
            "Promise" | "sequence" => {
                self.expect("<")?;
                let inner = Box::new(self.ty(depth + 1)?);
                self.expect(">")?;
                if name == "Promise" { Type::Promise(inner) } else { Type::Sequence(inner) }
            }
            _ => Type::Named(name),
        };
        Ok(if self.eat("?") { Type::Nullable(Box::new(base)) } else { base })
    }

    fn default_value(&mut self) -> Result<DefaultValue, Error> {
        let value = match self.peek().cloned() {
            Some(Token::Ident(w)) if w == "true" || w == "false" => DefaultValue::Bool(w == "true"),
            Some(Token::Ident(w)) if w == "null" => DefaultValue::Null,
            Some(Token::Number(n)) => DefaultValue::Number(n),
            Some(Token::Str(s)) => DefaultValue::String(s),
            Some(Token::Punct("[")) => {
                self.pos += 1;
                if !self.peek_is("]") {
                    return self.error("only `[]` is a list default");
                }
                DefaultValue::EmptyList
            }
            Some(Token::Punct("{")) => {
                self.pos += 1;
                if !self.peek_is("}") {
                    return self.error("only `{}` is a dictionary default");
                }
                DefaultValue::EmptyDictionary
            }
            _ => return self.error("expected a default value"),
        };
        self.pos += 1;
        Ok(value)
    }

    fn arguments(&mut self) -> Result<Vec<Argument>, Error> {
        self.expect("(")?;
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            self.ext_attrs()?;
            let optional = self.eat_ident("optional");
            let ty = self.ty(0)?;
            let variadic = self.eat("...");
            let name = self.ident()?;
            let default = if self.eat("=") { Some(self.default_value()?) } else { None };
            if default.is_some() && !optional {
                return self.error(format!("`{name}` has a default but is not optional"));
            }
            if args.iter().any(|a: &Argument| a.variadic) {
                return self.error("only the last argument may be variadic");
            }
            args.push(Argument { name, ty, optional, variadic, default });
            if self.eat(")") {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    fn members(&mut self) -> Result<Vec<Member>, Error> {
        self.expect("{")?;
        let mut members = Vec::new();
        while !self.eat("}") {
            if self.peek().is_none() {
                return self.error("unterminated body");
            }
            let ext_attrs = self.ext_attrs()?;
            if self.eat_ident("constructor") {
                let args = self.arguments()?;
                members.push(Member::Constructor { args });
            } else {
                let readonly = self.eat_ident("readonly");
                if self.eat_ident("attribute") {
                    let ty = self.ty(0)?;
                    let name = self.ident()?;
                    members.push(Member::Attribute { name, ty, readonly, ext_attrs });
                } else if readonly {
                    return self.error("expected `attribute` after `readonly`");
                } else {
                    let returns = self.ty(0)?;
                    let name = self.ident()?;
                    let args = self.arguments()?;
                    members.push(Member::Operation { name, args, returns, ext_attrs });
                }
            }
            self.expect(";")?;
        }
        Ok(members)
    }

    fn definition(&mut self) -> Result<Definition, Error> {
        let ext_attrs = self.ext_attrs()?;
        let partial = self.eat_ident("partial");
        let keyword = self.ident()?;
        let definition = match keyword.as_str() {
            "interface" => {
                let name = self.ident()?;
                let parent = if self.eat(":") { Some(self.ident()?) } else { None };
                let members = self.members()?;
                Definition::Interface(Interface { name, parent, partial, ext_attrs, members })
            }
            "namespace" if !partial => {
                let name = self.ident()?;
                let members = self.members()?;
                if members.iter().any(|m| !matches!(m, Member::Operation { .. })) {
                    return self.error("a namespace holds operations only");
                }
                Definition::Namespace(Interface { name, parent: None, partial, ext_attrs, members })
            }
            "dictionary" if !partial => {
                let name = self.ident()?;
                self.expect("{")?;
                let mut members = Vec::new();
                while !self.eat("}") {
                    if self.peek().is_none() {
                        return self.error("unterminated dictionary");
                    }
                    self.ext_attrs()?;
                    let ty = self.ty(0)?;
                    let member = self.ident()?;
                    self.expect(";")?;
                    members.push((ty, member));
                }
                Definition::Dictionary(Dictionary { name, members })
            }
            "enum" if !partial => {
                let name = self.ident()?;
                self.expect("{")?;
                let mut values = Vec::new();
                while !self.eat("}") {
                    match self.peek().cloned() {
                        Some(Token::Str(s)) => {
                            self.pos += 1;
                            values.push(s);
                        }
                        _ => return self.error("expected an enum value"),
                    }
                    if !self.eat(",") && !self.peek_is("}") {
                        return self.error("expected `,` or `}`");
                    }
                }
                if values.is_empty() {
                    return self.error("an enum needs a value");
                }
                Definition::Enum(Enum { name, values })
            }
            "callback" if !partial => {
                let name = self.ident()?;
                self.expect("=")?;
                let returns = self.ty(0)?;
                let args = self.arguments()?;
                Definition::Callback(Callback { name, returns, args })
            }
            other => return self.error(format!("unsupported definition `{}{other}`", if partial { "partial " } else { "" })),
        };
        self.expect(";")?;
        Ok(definition)
    }
}

/// Parses `src` into its definitions, in order.
pub fn parse(src: &str) -> Result<Vec<Definition>, Error> {
    if src.len() > MAX_INPUT_BYTES {
        return Err(Error { line: 1, message: format!("input larger than {MAX_INPUT_BYTES} bytes") });
    }
    let tokens = tokenize(src)?;
    let mut parser = Parser { tokens, pos: 0 };
    let mut definitions = Vec::new();
    while parser.peek().is_some() {
        definitions.push(parser.definition()?);
    }
    Ok(definitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_subset() {
        let defs = parse(
            r#"
            // a comment
            [Exposed=Window] interface Node : EventTarget {
              readonly attribute unsigned short nodeType;
              attribute DOMString? textContent;
              Node insertBefore(Node node, Node? child);
              Node cloneNode(optional boolean deep = false);
            };
            [Global] interface Window { Promise<Response> fetch(USVString input, optional RequestInit init = {}); };
            namespace console { undefined log(any... data); };
            dictionary RequestInit { RequestCredentials credentials; };
            enum RequestCredentials { "omit", "same-origin", "include" };
            callback EventListener = undefined (Event event);
            interface XMLHttpRequest { constructor(); };
            "#,
        )
        .unwrap();
        assert_eq!(defs.len(), 7);
        let Definition::Interface(node) = &defs[0] else { panic!() };
        assert_eq!(node.parent.as_deref(), Some("EventTarget"));
        assert!(node.has_ext_attr("Exposed"));
        assert_eq!(
            node.members[1],
            Member::Attribute { name: "textContent".into(), ty: Type::Nullable(Box::new(Type::DomString)), readonly: false, ext_attrs: vec![] }
        );
        let Member::Operation { args, .. } = &node.members[3] else { panic!() };
        assert_eq!(args[0].default, Some(DefaultValue::Bool(false)));
        let Definition::Namespace(console) = &defs[2] else { panic!() };
        let Member::Operation { args, .. } = &console.members[0] else { panic!() };
        assert!(args[0].variadic);
        let Definition::Enum(e) = &defs[4] else { panic!() };
        assert_eq!(e.values, ["omit", "same-origin", "include"]);
    }

    #[test]
    fn errors_name_their_line() {
        let err = parse("interface A {\n  attribute;\n};").unwrap_err();
        assert_eq!(err.line, 2);
        for bad in ["interface", "interface A {", "enum E {};", "namespace n { attribute long x; };", "interface A { long f(long... a, long b); };", "\"x", "/*"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hostile_input_is_bounded() {
        let deep = format!("interface A {{ {}long{} f(); }};", "Promise<".repeat(10_000), ">".repeat(10_000));
        assert!(parse(&deep).is_err());
        assert!(parse(&"a".repeat(MAX_INPUT_BYTES + 1)).is_err());
        let wide = "[".repeat(100_000);
        assert!(parse(&wide).is_err());
    }
}
