//! Generates the script bindings from `webidl/*.webidl` (Milestone 40, ADR
//! 023 §3) into `$OUT_DIR/bindings.rs`, included by `src/bindings.rs`:
//!
//! - `Interface`, and a static table of every interface's members and
//!   argument types, which a runtime builds its objects from;
//! - one trait per interface (`NodeImpl`, …) with a typed method per
//!   attribute and operation, which the host implements;
//! - `dispatch`, which checks `this` and every argument against the
//!   declaration, calls the typed method and converts its result.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use webidl::{Argument, DefaultValue, Definition, Interface, Member, Type};

const KEYWORDS: &[&str] = &["type", "match", "move", "ref", "self", "fn", "loop", "use", "mod", "impl", "in", "as", "box"];

struct Model {
    interfaces: Vec<Interface>,
    namespaces: Vec<String>,
    dictionaries: BTreeMap<String, Vec<(Type, String)>>,
    enums: BTreeMap<String, Vec<String>>,
    callbacks: Vec<String>,
}

fn main() {
    let dir = Path::new("webidl");
    println!("cargo:rerun-if-changed=webidl");
    let mut files: Vec<_> = std::fs::read_dir(dir).expect("webidl/ exists").map(|e| e.expect("readable").path()).collect();
    files.retain(|p| p.extension().is_some_and(|e| e == "webidl"));
    files.sort();
    let mut definitions = Vec::new();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let src = std::fs::read_to_string(file).expect("readable IDL");
        match webidl::parse(&src) {
            Ok(defs) => definitions.extend(defs),
            Err(e) => panic!("{}: {e}", file.display()),
        }
    }
    let model = build_model(definitions);
    let code = generate(&model);
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("bindings.rs");
    std::fs::write(out, code).expect("writable OUT_DIR");
}

fn build_model(definitions: Vec<Definition>) -> Model {
    let mut model = Model { interfaces: Vec::new(), namespaces: Vec::new(), dictionaries: BTreeMap::new(), enums: BTreeMap::new(), callbacks: Vec::new() };
    let mut partials = Vec::new();
    for def in definitions {
        match def {
            Definition::Interface(i) if i.partial => partials.push(i),
            Definition::Interface(i) => model.interfaces.push(i),
            Definition::Namespace(n) => {
                model.namespaces.push(n.name.clone());
                model.interfaces.push(n);
            }
            Definition::Dictionary(d) => {
                model.dictionaries.insert(d.name, d.members);
            }
            Definition::Enum(e) => {
                model.enums.insert(e.name, e.values);
            }
            Definition::Callback(c) => model.callbacks.push(c.name),
        }
    }
    for partial in partials {
        let base = model.interfaces.iter_mut().find(|i| i.name == partial.name).unwrap_or_else(|| panic!("partial interface {} has no interface", partial.name));
        base.members.extend(partial.members);
    }
    for i in &model.interfaces {
        if let Some(parent) = &i.parent {
            assert!(model.interfaces.iter().any(|p| &p.name == parent), "{}: unknown parent {parent}", i.name);
        }
        for m in &i.members {
            match m {
                Member::Attribute { ty, .. } => check_type(&model, ty, &i.name),
                Member::Operation { args, returns, .. } => {
                    check_type(&model, returns, &i.name);
                    args.iter().for_each(|a| check_type(&model, &a.ty, &i.name));
                }
                Member::Constructor { args } => args.iter().for_each(|a| check_type(&model, &a.ty, &i.name)),
            }
        }
    }
    model
}

fn check_type(model: &Model, ty: &Type, owner: &str) {
    match ty {
        Type::Named(n) => assert!(
            model.interfaces.iter().any(|i| &i.name == n) || model.dictionaries.contains_key(n) || model.enums.contains_key(n) || model.callbacks.contains(n),
            "{owner}: unknown type {n}"
        ),
        Type::Nullable(t) | Type::Promise(t) => check_type(model, t, owner),
        Type::Sequence(_) => panic!("{owner}: sequence<T> is not supported yet"),
        _ => {}
    }
}

fn variant(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn snake(name: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if i > 0 && (prev_lower || (next_lower && chars[i - 1].is_uppercase())) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    if out == "self" {
        // Not even a raw identifier (`r#self`) may be `self`.
        "self_".to_string()
    } else if KEYWORDS.contains(&out.as_str()) {
        format!("r#{out}")
    } else {
        out
    }
}

/// The Rust type a host method sees for `ty`.
fn rust_type(model: &Model, ty: &Type) -> String {
    match ty {
        Type::Undefined => "()".into(),
        Type::Boolean => "bool".into(),
        Type::Short => "i16".into(),
        Type::Long => "i32".into(),
        Type::UnsignedShort => "u16".into(),
        Type::UnsignedLong => "u32".into(),
        Type::Double => "f64".into(),
        Type::DomString | Type::UsvString | Type::ByteString => "String".into(),
        Type::Any | Type::Object => "Value".into(),
        Type::Promise(_) => "PromiseValue".into(),
        Type::Sequence(t) => format!("Vec<{}>", rust_type(model, t)),
        Type::Nullable(t) => format!("Option<{}>", rust_type(model, t)),
        Type::Named(n) if model.callbacks.contains(n) => "CallbackId".into(),
        Type::Named(n) if model.dictionaries.contains_key(n) || model.enums.contains_key(n) => n.clone(),
        Type::Named(_) => "ObjectRef".into(),
    }
}

/// An expression converting the `Value` expression `v` to `ty`.
fn convert(model: &Model, ty: &Type, v: &str) -> String {
    match ty {
        Type::Boolean => format!("convert::boolean({v})?"),
        Type::Short => format!("convert::number({v})? as i16"),
        Type::Long => format!("convert::number({v})? as i32"),
        Type::UnsignedShort => format!("convert::unsigned({v}, 16)? as u16"),
        Type::UnsignedLong => format!("convert::unsigned({v}, 32)? as u32"),
        Type::Double => format!("convert::number({v})?"),
        Type::DomString | Type::UsvString | Type::ByteString => format!("convert::string({v})?"),
        Type::Any | Type::Object => v.to_string(),
        Type::Nullable(t) => format!("match {v} {{ Value::Null | Value::Undefined => None, v => Some({}) }}", convert(model, t, "v")),
        Type::Named(n) if model.callbacks.contains(n) => format!("convert::callback({v})?"),
        Type::Named(n) if model.dictionaries.contains_key(n) => format!("{n}::from_value({v})?"),
        Type::Named(n) if model.enums.contains_key(n) => format!("{n}::from_value({v})?"),
        Type::Named(n) => format!("convert::object({v}, Interface::{})?", variant(n)),
        Type::Undefined | Type::Promise(_) | Type::Sequence(_) => panic!("{ty:?} is not an argument type"),
    }
}

/// The `TypeInfo` expression for `ty`.
fn type_info(model: &Model, ty: &Type) -> String {
    match ty {
        Type::Undefined => "TypeInfo::Undefined".into(),
        Type::Boolean => "TypeInfo::Boolean".into(),
        Type::Short | Type::Long | Type::UnsignedShort | Type::UnsignedLong | Type::Double => "TypeInfo::Number".into(),
        Type::DomString | Type::UsvString | Type::ByteString => "TypeInfo::String".into(),
        Type::Any | Type::Object => "TypeInfo::Any".into(),
        Type::Promise(_) => "TypeInfo::Promise".into(),
        Type::Sequence(_) => unreachable!("rejected by check_type"),
        Type::Nullable(t) => format!("TypeInfo::Nullable(&{})", type_info(model, t)),
        Type::Named(n) if model.callbacks.contains(n) => "TypeInfo::Callback".into(),
        Type::Named(n) if model.dictionaries.contains_key(n) => {
            let members: Vec<String> = model.dictionaries[n].iter().map(|(t, m)| format!("(\"{m}\", {})", type_info(model, t))).collect();
            format!("TypeInfo::Dictionary(&[{}])", members.join(", "))
        }
        Type::Named(n) if model.enums.contains_key(n) => "TypeInfo::String".into(),
        Type::Named(n) => format!("TypeInfo::Object(Interface::{})", variant(n)),
    }
}

fn default_expr(ty: &Type, default: &DefaultValue) -> String {
    match (default, ty) {
        (DefaultValue::Bool(b), _) => b.to_string(),
        (DefaultValue::Number(n), Type::Double) => format!("{n:?}"),
        (DefaultValue::Number(n), _) => format!("{}", *n as i64),
        (DefaultValue::String(s), _) => format!("String::from({s:?})"),
        (DefaultValue::Null, _) => "None".into(),
        (DefaultValue::EmptyList | DefaultValue::EmptyDictionary, _) => "Default::default()".into(),
    }
}

/// The parameter type of an argument (after optional/variadic).
fn arg_rust_type(model: &Model, a: &Argument) -> String {
    let t = rust_type(model, &a.ty);
    if a.variadic {
        format!("Vec<{t}>")
    } else if a.optional && a.default.is_none() {
        format!("Option<{t}>")
    } else {
        t
    }
}

/// Statements binding each argument from the `args` iterator, and the
/// argument list to pass on.
fn bind_args(model: &Model, args: &[Argument], what: &str) -> (String, String) {
    let mut code = String::new();
    let required = args.iter().filter(|a| !a.optional && !a.variadic).count();
    if required > 0 {
        writeln!(code, "            if len < {required} {{ return Err(HostError::type_error(format!(\"{what}: {required} argument(s) required, but only {{len}} present\"))); }}").unwrap();
    }
    let mut names = Vec::new();
    for a in args {
        let name = format!("arg_{}", snake(&a.name).trim_start_matches("r#"));
        if a.variadic {
            writeln!(code, "            let {name}: Vec<_> = args.by_ref().map(|v| -> Result<_, HostError> {{ Ok({}) }}).collect::<Result<_, _>>()?;", convert(model, &a.ty, "v")).unwrap();
        } else if a.optional {
            let converted = convert(model, &a.ty, "v");
            match &a.default {
                Some(d) => writeln!(code, "            let {name} = match args.next() {{ None | Some(Value::Undefined) => {}, Some(v) => {converted} }};", default_expr(&a.ty, d)).unwrap(),
                None => writeln!(code, "            let {name} = match args.next() {{ None | Some(Value::Undefined) => None, Some(v) => Some({converted}) }};").unwrap(),
            }
        } else {
            writeln!(code, "            let {name} = {{ let v = args.next().unwrap_or(Value::Undefined); {} }};", convert(model, &a.ty, "v")).unwrap();
        }
        names.push(name);
    }
    (code, names.join(", "))
}

fn generate(model: &Model) -> String {
    let mut out = String::new();
    out.push_str("// Generated by crates/script/build.rs from crates/script/webidl/*.webidl. Do not edit.\n\n");

    // Interfaces.
    out.push_str("/// Every interface (and namespace) declared in the IDL.\n#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]\npub enum Interface {\n");
    for i in &model.interfaces {
        writeln!(out, "    {},", variant(&i.name)).unwrap();
    }
    out.push_str("}\n\nimpl Interface {\n    pub const ALL: &'static [Interface] = &[");
    out.push_str(&model.interfaces.iter().map(|i| format!("Interface::{}", variant(&i.name))).collect::<Vec<_>>().join(", "));
    out.push_str("];\n\n    /// What the IDL declares for this interface.\n    pub fn info(self) -> &'static InterfaceInfo {\n        match self {\n");
    for i in &model.interfaces {
        writeln!(out, "            Interface::{} => &INFO_{},", variant(&i.name), snake(&i.name).to_uppercase()).unwrap();
    }
    out.push_str("        }\n    }\n}\n\n");

    // Enums and dictionaries.
    for (name, values) in &model.enums {
        writeln!(out, "#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum {name} {{").unwrap();
        for v in values {
            writeln!(out, "    {},", v.split('-').map(variant).collect::<String>()).unwrap();
        }
        out.push_str("}\n\n");
        writeln!(out, "impl {name} {{\n    pub fn as_str(self) -> &'static str {{\n        match self {{").unwrap();
        for v in values {
            writeln!(out, "            {name}::{} => {v:?},", v.split('-').map(variant).collect::<String>()).unwrap();
        }
        writeln!(out, "        }}\n    }}\n\n    fn from_value(v: Value) -> Result<Self, HostError> {{\n        match convert::string(v)?.as_str() {{").unwrap();
        for v in values {
            writeln!(out, "            {v:?} => Ok({name}::{}),", v.split('-').map(variant).collect::<String>()).unwrap();
        }
        writeln!(out, "            other => Err(HostError::type_error(format!(\"`{{other}}` is not a valid {name}\"))),\n        }}\n    }}\n}}\n").unwrap();
    }
    for (name, members) in &model.dictionaries {
        writeln!(out, "#[derive(Clone, Debug, Default, PartialEq)]\npub struct {name} {{").unwrap();
        for (ty, m) in members {
            writeln!(out, "    pub {}: Option<{}>,", snake(m), rust_type(model, ty)).unwrap();
        }
        writeln!(out, "}}\n\nimpl {name} {{\n    #[allow(unused_mut, clippy::single_match)]\n    fn from_value(v: Value) -> Result<Self, HostError> {{\n        let mut dict = {name}::default();\n        for (key, v) in convert::dictionary(v)? {{\n            match key.as_str() {{").unwrap();
        for (ty, m) in members {
            writeln!(out, "                {m:?} => dict.{} = match v {{ Value::Undefined => None, v => Some({}) }},", snake(m), convert(model, ty, "v")).unwrap();
        }
        writeln!(out, "                _ => {{}}\n            }}\n        }}\n        Ok(dict)\n    }}\n}}\n").unwrap();
    }

    // The member tables.
    for i in &model.interfaces {
        let kind = if model.namespaces.contains(&i.name) {
            "Namespace"
        } else if i.has_ext_attr("Global") {
            "Global"
        } else {
            "Interface"
        };
        let parent = i.parent.as_ref().map_or("None".to_string(), |p| format!("Some(Interface::{})", variant(p)));
        let constructor = i.members.iter().find_map(|m| match m {
            Member::Constructor { args } => Some(format!("Some(&[{}])", args_info(model, args))),
            _ => None,
        });
        let mut members = Vec::new();
        for m in &i.members {
            match m {
                Member::Attribute { name, ty, readonly, .. } => members.push(format!(
                    "MemberInfo {{ name: {name:?}, kind: MemberKind::Attribute {{ ty: {}, readonly: {readonly} }} }}",
                    type_info(model, ty)
                )),
                Member::Operation { name, args, returns, .. } => members.push(format!(
                    "MemberInfo {{ name: {name:?}, kind: MemberKind::Operation {{ args: &[{}], returns: {} }} }}",
                    args_info(model, args),
                    type_info(model, returns)
                )),
                Member::Constructor { .. } => {}
            }
        }
        writeln!(
            out,
            "static INFO_{}: InterfaceInfo = InterfaceInfo {{\n    name: {:?},\n    parent: {parent},\n    kind: InterfaceKind::{kind},\n    constructor: {},\n    members: &[\n        {}\n    ],\n}};\n",
            snake(&i.name).to_uppercase(),
            i.name,
            constructor.unwrap_or_else(|| "None".into()),
            members.join(",\n        ")
        )
        .unwrap();
    }

    // The host traits.
    for i in &model.interfaces {
        let namespace = model.namespaces.contains(&i.name);
        let this = if namespace { "" } else { ", this: ObjectRef" };
        writeln!(out, "/// The host side of `{}` (generated from its IDL).\npub trait {}Impl {{", i.name, variant(&i.name)).unwrap();
        for m in &i.members {
            match m {
                Member::Attribute { name, ty, readonly, .. } => {
                    writeln!(out, "    fn {}(&mut self{this}) -> Result<{}, HostError>;", snake(name), rust_type(model, ty)).unwrap();
                    if !readonly {
                        writeln!(out, "    fn set_{}(&mut self{this}, value: {}) -> Result<(), HostError>;", snake(name).trim_start_matches("r#"), rust_type(model, ty)).unwrap();
                    }
                }
                Member::Operation { name, args, returns, .. } => {
                    let params: String = args.iter().map(|a| format!(", {}: {}", snake(&a.name), arg_rust_type(model, a))).collect();
                    writeln!(out, "    fn {}(&mut self{this}{params}) -> Result<{}, HostError>;", snake(name), rust_type(model, returns)).unwrap();
                }
                Member::Constructor { args } => {
                    let params: String = args.iter().map(|a| format!(", {}: {}", snake(&a.name), arg_rust_type(model, a))).collect();
                    writeln!(out, "    fn constructor(&mut self{params}) -> Result<ObjectRef, HostError>;").unwrap();
                }
            }
        }
        out.push_str("}\n\n");
    }
    let all: Vec<String> = model.interfaces.iter().map(|i| format!("{}Impl", variant(&i.name))).collect();
    writeln!(out, "/// Everything a host implements: one trait per interface.\npub trait Bindings: {} {{}}\n\nimpl<T: {}> Bindings for T {{}}\n", all.join(" + "), all.join(" + ")).unwrap();

    // dispatch.
    out.push_str(
        "/// Calls `call` on the host: checks `this` and the arguments against the\n/// declaration, runs the typed method, converts its result.\n#[allow(unused_variables, unused_mut, clippy::let_unit_value)]\npub fn dispatch<H: Bindings + ?Sized>(host: &mut H, call: Call, this: Option<ObjectRef>, args: Vec<Value>) -> Result<Value, HostError> {\n    let len = args.len();\n    let mut args = args.into_iter();\n    match call {\n",
    );
    for i in &model.interfaces {
        let v = variant(&i.name);
        let namespace = model.namespaces.contains(&i.name);
        let mut index = 0u16;
        for m in &i.members {
            let check_this = |what: &str| if namespace { String::new() } else { format!("            let this = convert::this(this, Interface::{v}, \"{what}\")?;\n") };
            let pass_this = if namespace { "" } else { "this, " };
            match m {
                Member::Attribute { name, readonly, ty, .. } => {
                    let what = format!("{}.{name}", i.name);
                    writeln!(out, "        Call::Get(Interface::{v}, {index}) => {{\n{}            Ok(<H as {v}Impl>::{}(host{}{})?.into_value())\n        }}", check_this(&what), snake(name), if namespace { "" } else { ", " }, if namespace { "" } else { "this" }).unwrap();
                    if !readonly {
                        writeln!(
                            out,
                            "        Call::Set(Interface::{v}, {index}) => {{\n{}            let value = {{ let v = args.next().unwrap_or(Value::Undefined); {} }};\n            <H as {v}Impl>::set_{}(host, {pass_this}value)?;\n            Ok(Value::Undefined)\n        }}",
                            check_this(&what),
                            convert(model, ty, "v"),
                            snake(name).trim_start_matches("r#")
                        )
                        .unwrap();
                    }
                    index += 1;
                }
                Member::Operation { name, args, returns, .. } => {
                    let what = format!("{}.{name}", i.name);
                    let (bind, pass) = bind_args(model, args, &what);
                    let body = format!("{}{bind}            Ok(<H as {v}Impl>::{}(host, {pass_this}{pass})?.into_value())\n", check_this(&what), snake(name));
                    if matches!(returns, Type::Promise(_)) {
                        // WebIDL §3.2.1: an operation that returns a promise
                        // never throws — a bad `this`, a bad argument or the
                        // host's error becomes a rejected promise.
                        writeln!(out, "        Call::Operation(Interface::{v}, {index}) => {{\n            let mut run = || -> Result<Value, HostError> {{\n{body}            }};\n            Ok(run().unwrap_or_else(|e| Value::Promise(crate::PromiseValue::Rejected(Box::new(crate::Rejection::Error(e))))))\n        }}").unwrap();
                    } else {
                        writeln!(out, "        Call::Operation(Interface::{v}, {index}) => {{\n{body}        }}").unwrap();
                    }
                    index += 1;
                }
                Member::Constructor { args } => {
                    let (bind, pass) = bind_args(model, args, &format!("new {}", i.name));
                    writeln!(out, "        Call::Construct(Interface::{v}) => {{\n{bind}            Ok(<H as {v}Impl>::constructor(host, {pass})?.into_value())\n        }}").unwrap();
                }
            }
        }
    }
    out.push_str("        _ => Err(HostError::type_error(\"no such member\")),\n    }\n}\n");
    out
}

fn args_info(model: &Model, args: &[Argument]) -> String {
    args.iter()
        .map(|a| format!("ArgInfo {{ name: {:?}, ty: {}, optional: {}, variadic: {} }}", a.name, type_info(model, &a.ty), a.optional, a.variadic))
        .collect::<Vec<_>>()
        .join(", ")
}
