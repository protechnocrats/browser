//! A second [`ScriptRuntime`]: an interpreter for a small subset of
//! JavaScript, over the same generated bindings Boa uses (ADR 023 §7). It
//! exists to prove the host side is runtime-independent — the binding cases
//! in [`crate::cases`] run on it and on Boa and must agree — and to test the
//! parts of the interface Boa cannot exercise (interrupts).
//!
//! The subset: statements `var x = e`, `e`, `target = e`, separated by `;`;
//! expressions `a.b`, `a.b(args)`, `f(args)`, `new C()`, `a + b`, `a === b`,
//! `a !== b`, `(e)`, string, number, `true`, `false`, `null`, `undefined`,
//! and arrow functions `(a, b) => { statements }` or `(a) => e`. Every
//! variable is global. There are no objects but host objects, and no
//! exceptions inside script: a binding that throws ends the script.

use crate::bindings::{dispatch, InterfaceKind, MemberKind, TypeInfo};
use crate::{
    Call, CallbackId, HostError, HostRef, Interface, InterruptHandle, ObjectRef, PromiseId, ScriptError, ScriptErrorKind,
    ScriptRuntime, Settlement, Value, WrapperGuard, WINDOW,
};
use std::collections::HashMap;
use std::rc::Rc;

/// Most statements one `evaluate` or `call` runs (a stub has no loops, but
/// bounds what it does anyway).
const MAX_STEPS: usize = 100_000;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Number(f64),
    Punct(&'static str),
}

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    const PUNCT: [&str; 13] = ["===", "!==", "=>", "(", ")", "{", "}", ",", ";", ".", "=", "+", "!"];
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let mut s = String::new();
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                s.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return Err("SyntaxError: unterminated string".into());
            }
            i += 1;
            out.push(Token::Str(s));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            out.push(Token::Number(text.parse().map_err(|_| format!("SyntaxError: bad number {text}"))?));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            out.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            let Some(p) = PUNCT.iter().find(|p| rest.starts_with(**p)) else { return Err(format!("SyntaxError: unexpected `{c}`")) };
            out.push(Token::Punct(p));
            i += p.len();
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
enum Expr {
    Literal(SValue),
    Var(String),
    Member(Box<Expr>, String),
    Call(Box<Expr>, Vec<Expr>),
    New(String),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Arrow(Rc<Function>),
}

#[derive(Clone, Debug)]
enum Stmt {
    Var(String, Expr),
    Assign(Expr, Expr),
    Expr(Expr),
}

#[derive(Debug)]
struct Function {
    params: Vec<String>,
    body: Vec<Stmt>,
    /// `(a) => e`: the value of `e`.
    returns_last: bool,
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.peek(), Some(Token::Punct(q)) if *q == p)
    }

    fn eat(&mut self, p: &str) -> bool {
        let found = self.is(p);
        if found {
            self.pos += 1;
        }
        found
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) {
            Ok(())
        } else {
            Err(format!("SyntaxError: expected `{p}`"))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.peek().cloned() {
            Some(Token::Ident(w)) => {
                self.pos += 1;
                Ok(w)
            }
            _ => Err("SyntaxError: expected a name".into()),
        }
    }

    fn statements(&mut self, until_brace: bool) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        loop {
            while self.eat(";") {}
            if self.peek().is_none() || (until_brace && self.is("}")) {
                return Ok(out);
            }
            out.push(self.statement()?);
            if !self.eat(";") && self.peek().is_some() && !(until_brace && self.is("}")) {
                return Err("SyntaxError: expected `;`".into());
            }
        }
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if matches!(self.peek(), Some(Token::Ident(w)) if w == "var") {
            self.pos += 1;
            let name = self.ident()?;
            self.expect("=")?;
            return Ok(Stmt::Var(name, self.expr()?));
        }
        let e = self.expr()?;
        if self.eat("=") {
            if !matches!(e, Expr::Var(_) | Expr::Member(..)) {
                return Err("SyntaxError: invalid assignment target".into());
            }
            return Ok(Stmt::Assign(e, self.expr()?));
        }
        Ok(Stmt::Expr(e))
    }

    fn expr(&mut self) -> Result<Expr, String> {
        let mut left = self.sum()?;
        loop {
            let op = if self.eat("===") {
                "==="
            } else if self.eat("!==") {
                "!=="
            } else {
                return Ok(left);
            };
            left = Expr::Binary(op, Box::new(left), Box::new(self.sum()?));
        }
    }

    fn sum(&mut self) -> Result<Expr, String> {
        let mut left = self.postfix()?;
        while self.eat("+") {
            left = Expr::Binary("+", Box::new(left), Box::new(self.postfix()?));
        }
        Ok(left)
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.eat(".") {
                e = Expr::Member(Box::new(e), self.ident()?);
            } else if self.is("(") {
                e = Expr::Call(Box::new(e), self.arguments()?);
            } else {
                return Ok(e);
            }
        }
    }

    fn arguments(&mut self) -> Result<Vec<Expr>, String> {
        self.expect("(")?;
        let mut args = Vec::new();
        if self.eat(")") {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(")") {
                return Ok(args);
            }
            self.expect(",")?;
        }
    }

    /// `(a, b) => …` starts here when the parenthesised list is followed
    /// by `=>`.
    fn arrow_params(&mut self) -> Option<Vec<String>> {
        let start = self.pos;
        let mut params = Vec::new();
        if !self.eat("(") {
            return None;
        }
        if !self.eat(")") {
            loop {
                match self.ident() {
                    Ok(p) => params.push(p),
                    Err(_) => {
                        self.pos = start;
                        return None;
                    }
                }
                if self.eat(")") {
                    break;
                }
                if !self.eat(",") {
                    self.pos = start;
                    return None;
                }
            }
        }
        if self.eat("=>") {
            Some(params)
        } else {
            self.pos = start;
            None
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        if let Some(params) = self.arrow_params() {
            let (body, returns_last) = if self.eat("{") {
                let body = self.statements(true)?;
                self.expect("}")?;
                (body, false)
            } else {
                (vec![Stmt::Expr(self.expr()?)], true)
            };
            return Ok(Expr::Arrow(Rc::new(Function { params, body, returns_last })));
        }
        if self.eat("(") {
            let e = self.expr()?;
            self.expect(")")?;
            return Ok(e);
        }
        match self.peek().cloned() {
            Some(Token::Str(s)) => {
                self.pos += 1;
                Ok(Expr::Literal(SValue::String(s)))
            }
            Some(Token::Number(n)) => {
                self.pos += 1;
                Ok(Expr::Literal(SValue::Number(n)))
            }
            Some(Token::Ident(w)) => {
                self.pos += 1;
                Ok(match w.as_str() {
                    "true" => Expr::Literal(SValue::Bool(true)),
                    "false" => Expr::Literal(SValue::Bool(false)),
                    "null" => Expr::Literal(SValue::Null),
                    "undefined" => Expr::Literal(SValue::Undefined),
                    "new" => {
                        let name = self.ident()?;
                        self.arguments()?;
                        Expr::New(name)
                    }
                    _ => Expr::Var(w),
                })
            }
            _ => Err("SyntaxError: unexpected token".into()),
        }
    }
}

/// A value in the stub.
#[derive(Clone, Debug, PartialEq)]
enum SValue {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Object(ObjectRef),
    Function(u32),
    Namespace(Interface),
    /// A method looked up on an object, not yet called.
    Bound(Call, Option<ObjectRef>),
    Promise,
}

impl SValue {
    fn to_display(&self) -> String {
        match self {
            SValue::Undefined => "undefined".into(),
            SValue::Null => "null".into(),
            SValue::Bool(b) => b.to_string(),
            SValue::Number(n) => crate::number_to_string(*n),
            SValue::String(s) => s.clone(),
            SValue::Object(_) | SValue::Namespace(_) => "[object Object]".into(),
            SValue::Function(_) | SValue::Bound(..) => "function () { [native code] }".into(),
            SValue::Promise => "[object Promise]".into(),
        }
    }
}

/// The stub runtime.
pub struct StubRuntime {
    host: HostRef,
    globals: HashMap<String, SValue>,
    functions: HashMap<u32, Rc<Function>>,
    next_function: u32,
    /// One guard per object the stub has seen; dropped by
    /// [`ScriptRuntime::collect_garbage`] for objects no variable holds.
    wrappers: HashMap<ObjectRef, WrapperGuard>,
    interrupt: InterruptHandle,
    steps: usize,
}

impl StubRuntime {
    pub fn new(host: HostRef) -> StubRuntime {
        StubRuntime { host, globals: HashMap::new(), functions: HashMap::new(), next_function: 0, wrappers: HashMap::new(), interrupt: InterruptHandle::new(), steps: 0 }
    }

    fn error(message: String) -> ScriptError {
        ScriptError { kind: ScriptErrorKind::Thrown, message }
    }

    fn run(&mut self, body: &[Stmt]) -> Result<SValue, ScriptError> {
        let mut last = SValue::Undefined;
        for stmt in body {
            self.steps += 1;
            if self.interrupt.take() {
                return Err(ScriptError { kind: ScriptErrorKind::Interrupted, message: "Interrupted: the script was stopped".into() });
            }
            if self.steps > MAX_STEPS {
                return Err(ScriptError { kind: ScriptErrorKind::RuntimeLimit, message: "RuntimeLimit: too many steps".into() });
            }
            match stmt {
                Stmt::Var(name, e) => {
                    let v = self.eval(e)?;
                    self.globals.insert(name.clone(), v);
                }
                Stmt::Assign(target, e) => {
                    let v = self.eval(e)?;
                    match target {
                        Expr::Var(name) => {
                            self.globals.insert(name.clone(), v.clone());
                        }
                        Expr::Member(object, name) => {
                            let object = self.eval(object)?;
                            let call = self.member(&object, name).and_then(|(m, this)| match m {
                                MemberRef::Attribute(i, index, false) => Some((Call::Set(i, index), this)),
                                _ => None,
                            });
                            if let Some((call, this)) = call {
                                self.invoke(call, this, std::slice::from_ref(&v))?;
                            }
                        }
                        _ => unreachable!("checked by the parser"),
                    }
                    last = v;
                }
                Stmt::Expr(e) => last = self.eval(e)?,
            }
        }
        Ok(last)
    }

    fn eval(&mut self, e: &Expr) -> Result<SValue, ScriptError> {
        match e {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Var(name) => {
                if let Some(v) = self.globals.get(name) {
                    return Ok(v.clone());
                }
                if let Some(&interface) = Interface::ALL.iter().find(|i| i.info().kind == InterfaceKind::Namespace && i.name() == name) {
                    return Ok(SValue::Namespace(interface));
                }
                match find_member(Interface::Window, name) {
                    Some(MemberRef::Attribute(i, index, _)) => self.invoke(Call::Get(i, index), Some(WINDOW), &[]),
                    Some(MemberRef::Operation(i, index)) => Ok(SValue::Bound(Call::Operation(i, index), Some(WINDOW))),
                    None => Err(Self::error(format!("ReferenceError: {name} is not defined"))),
                }
            }
            Expr::Member(object, name) => {
                let object = self.eval(object)?;
                match self.member(&object, name) {
                    Some((MemberRef::Attribute(i, index, _), this)) => self.invoke(Call::Get(i, index), this, &[]),
                    Some((MemberRef::Operation(i, index), this)) => Ok(SValue::Bound(Call::Operation(i, index), this)),
                    None => Ok(SValue::Undefined),
                }
            }
            Expr::Call(callee, args) => {
                let callee = self.eval(callee)?;
                let args = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
                match callee {
                    SValue::Bound(call, this) => self.invoke(call, this, &args),
                    SValue::Function(id) => self.call_function(id, &args),
                    other => Err(Self::error(format!("TypeError: {} is not a function", other.to_display()))),
                }
            }
            Expr::New(name) => match Interface::ALL.iter().find(|i| i.name() == name && i.info().constructor.is_some()) {
                Some(&i) => self.invoke(Call::Construct(i), None, &[]),
                None => Err(Self::error(format!("TypeError: {name} is not a constructor"))),
            },
            Expr::Binary(op, a, b) => {
                let (a, b) = (self.eval(a)?, self.eval(b)?);
                Ok(match *op {
                    "===" => SValue::Bool(a == b),
                    "!==" => SValue::Bool(a != b),
                    _ => match (&a, &b) {
                        (SValue::Number(x), SValue::Number(y)) => SValue::Number(x + y),
                        _ => SValue::String(a.to_display() + &b.to_display()),
                    },
                })
            }
            Expr::Arrow(f) => {
                let id = self.next_function;
                self.next_function += 1;
                self.functions.insert(id, Rc::clone(f));
                Ok(SValue::Function(id))
            }
        }
    }

    fn call_function(&mut self, id: u32, args: &[SValue]) -> Result<SValue, ScriptError> {
        let Some(f) = self.functions.get(&id).cloned() else { return Ok(SValue::Undefined) };
        for (i, p) in f.params.iter().enumerate() {
            self.globals.insert(p.clone(), args.get(i).cloned().unwrap_or(SValue::Undefined));
        }
        let v = self.run(&f.body)?;
        Ok(if f.returns_last { v } else { SValue::Undefined })
    }

    fn member(&self, object: &SValue, name: &str) -> Option<(MemberRef, Option<ObjectRef>)> {
        match object {
            SValue::Object(o) => find_member(o.interface, name).map(|m| (m, Some(*o))),
            SValue::Namespace(i) => find_member(*i, name).map(|m| (m, None)),
            _ => None,
        }
    }

    fn invoke(&mut self, call: Call, this: Option<ObjectRef>, args: &[SValue]) -> Result<SValue, ScriptError> {
        let types = call.arg_types();
        let mut values = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let Some(info) = types.get(i).or_else(|| types.last().filter(|t| t.variadic)) else { break };
            values.push(self.to_value(a, &info.ty));
        }
        let result = {
            let mut host = self.host.borrow_mut();
            host.collect_closed();
            dispatch(&mut *host, call, this, values)
        };
        result.map(|v| self.stub_value(v)).map_err(|e: HostError| Self::error(e.to_string()))
    }

    fn to_value(&self, v: &SValue, ty: &TypeInfo) -> Value {
        match (v, ty) {
            (SValue::Undefined, _) => Value::Undefined,
            (SValue::Null, _) => Value::Null,
            (SValue::Function(id), TypeInfo::Callback | TypeInfo::Nullable(TypeInfo::Callback)) => Value::Callback(CallbackId(*id)),
            (_, TypeInfo::Nullable(TypeInfo::Callback)) => Value::Null,
            (_, TypeInfo::Nullable(inner)) => self.to_value(v, inner),
            (_, TypeInfo::Boolean) => Value::Bool(!matches!(v, SValue::Bool(false) | SValue::Number(0.0)) && *v != SValue::String(String::new())),
            (SValue::Number(n), TypeInfo::Number) => Value::Number(*n),
            (SValue::Bool(b), TypeInfo::Number) => Value::Number(f64::from(u8::from(*b))),
            (_, TypeInfo::String) => Value::String(v.to_display()),
            (SValue::Object(o), _) => Value::Object(*o),
            (SValue::Bool(b), _) => Value::Bool(*b),
            (SValue::Number(n), _) => Value::Number(*n),
            (SValue::String(s), TypeInfo::Any) => Value::String(s.clone()),
            _ => Value::Other,
        }
    }

    fn stub_value(&mut self, v: Value) -> SValue {
        match v {
            Value::Undefined | Value::Other | Value::Dictionary(_) => SValue::Undefined,
            Value::Null => SValue::Null,
            Value::Bool(b) => SValue::Bool(b),
            Value::Number(n) => SValue::Number(n),
            Value::String(s) | Value::Json(s) => SValue::String(s),
            Value::Object(o) => {
                if !self.wrappers.contains_key(&o) {
                    let guard = self.host.borrow_mut().open_wrapper(o);
                    self.wrappers.insert(o, guard);
                }
                SValue::Object(o)
            }
            Value::Callback(id) => SValue::Function(id.0),
            Value::Promise(_) => SValue::Promise,
        }
    }

    fn to_host(&self, v: &SValue) -> Value {
        self.to_value(v, &TypeInfo::Any)
    }
}

/// A member found on an interface or its ancestors.
#[derive(Clone, Copy)]
enum MemberRef {
    /// Interface, index, read-only.
    Attribute(Interface, u16, bool),
    Operation(Interface, u16),
}

fn find_member(interface: Interface, name: &str) -> Option<MemberRef> {
    let mut current = Some(interface);
    while let Some(i) = current {
        if let Some((index, m)) = i.info().members.iter().enumerate().find(|(_, m)| m.name == name) {
            return Some(match m.kind {
                MemberKind::Attribute { readonly, .. } => MemberRef::Attribute(i, index as u16, readonly),
                MemberKind::Operation { .. } => MemberRef::Operation(i, index as u16),
            });
        }
        current = i.info().parent;
    }
    None
}

impl ScriptRuntime for StubRuntime {
    fn evaluate(&mut self, source: &str) -> Result<String, ScriptError> {
        self.steps = 0;
        let tokens = tokenize(source).map_err(Self::error)?;
        let program = Parser { tokens, pos: 0 }.statements(false).map_err(Self::error)?;
        self.run(&program).map(|v| v.to_display())
    }

    fn run_jobs(&mut self) -> Result<(), ScriptError> {
        Ok(())
    }

    fn call(&mut self, callback: CallbackId, _this: &Value, args: &[Value]) -> Result<Value, ScriptError> {
        self.steps = 0;
        let args: Vec<SValue> = args.iter().cloned().map(|a| self.stub_value(a)).collect();
        let result = self.call_function(callback.0, &args)?;
        Ok(self.to_host(&result))
    }

    fn settle(&mut self, _promise: PromiseId, _settlement: Settlement) {}

    fn release(&mut self, callbacks: &[CallbackId]) {
        for id in callbacks {
            self.functions.remove(&id.0);
        }
    }

    fn interrupt_handle(&self) -> Option<InterruptHandle> {
        Some(self.interrupt.clone())
    }

    fn collect_garbage(&mut self) {
        let held: Vec<ObjectRef> = self.globals.values().filter_map(|v| if let SValue::Object(o) = v { Some(*o) } else { None }).collect();
        self.wrappers.retain(|o, _| held.contains(o) || !o.interface.inherits(Interface::Node));
    }
}
