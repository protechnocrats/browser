//! The bindings generated from `webidl/*.webidl` (ADR 023 §3) and the types
//! they are written in. `build.rs` writes the generated half; this file
//! holds what it refers to.

use crate::{CallbackId, HostError, ObjectRef, PromiseValue, Value};

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// One member to call, as a runtime names it: an operation, an attribute's
/// getter or setter (by index into [`InterfaceInfo::members`]), or a
/// constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Call {
    Operation(Interface, u16),
    Get(Interface, u16),
    Set(Interface, u16),
    Construct(Interface),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceKind {
    /// Instances are host objects with a prototype.
    Interface,
    /// `[Global]`: its members live on the global object.
    Global,
    /// A namespace: one plain object of operations (`console`).
    Namespace,
}

#[derive(Debug)]
pub struct InterfaceInfo {
    pub name: &'static str,
    pub parent: Option<Interface>,
    pub kind: InterfaceKind,
    pub constructor: Option<&'static [ArgInfo]>,
    pub members: &'static [MemberInfo],
}

#[derive(Debug)]
pub struct MemberInfo {
    pub name: &'static str,
    pub kind: MemberKind,
}

#[derive(Debug)]
pub enum MemberKind {
    Operation { args: &'static [ArgInfo], returns: TypeInfo },
    Attribute { ty: TypeInfo, readonly: bool },
}

#[derive(Debug)]
pub struct ArgInfo {
    pub name: &'static str,
    pub ty: TypeInfo,
    pub optional: bool,
    pub variadic: bool,
}

/// What a runtime converts a script value to, before [`dispatch`] checks it.
#[derive(Debug)]
pub enum TypeInfo {
    Undefined,
    Boolean,
    Number,
    String,
    /// Any value; a runtime passes primitives and host objects as they are,
    /// and any other object as its string (only `console.log` takes `any`).
    Any,
    Object(Interface),
    Callback,
    Dictionary(&'static [(&'static str, TypeInfo)]),
    Promise,
    Nullable(&'static TypeInfo),
}

impl Interface {
    /// Whether `self` is `ancestor` or inherits from it.
    pub fn inherits(self, ancestor: Interface) -> bool {
        let mut current = Some(self);
        while let Some(i) = current {
            if i == ancestor {
                return true;
            }
            current = i.info().parent;
        }
        false
    }

    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// The member `index` of this interface.
    pub fn member(self, index: u16) -> Option<&'static MemberInfo> {
        self.info().members.get(usize::from(index))
    }
}

impl Call {
    /// The argument types a runtime converts this call's arguments to.
    pub fn arg_types(self) -> &'static [ArgInfo] {
        match self {
            Call::Operation(i, m) => match i.member(m).map(|m| &m.kind) {
                Some(MemberKind::Operation { args, .. }) => args,
                _ => &[],
            },
            Call::Set(i, m) => match i.member(m).map(|m| &m.kind) {
                Some(MemberKind::Attribute { ty, .. }) => setter_arg(ty),
                _ => &[],
            },
            Call::Get(..) => &[],
            Call::Construct(i) => i.info().constructor.unwrap_or(&[]),
        }
    }
}

/// A one-argument list for an attribute setter of type `ty` — the table
/// has no `ArgInfo` for it, so the common types have one here.
fn setter_arg(ty: &TypeInfo) -> &'static [ArgInfo] {
    static BOOLEAN: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::Boolean, optional: false, variadic: false }];
    static STRING: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::String, optional: false, variadic: false }];
    static NULLABLE_STRING: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::Nullable(&TypeInfo::String), optional: false, variadic: false }];
    static CALLBACK: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::Nullable(&TypeInfo::Callback), optional: false, variadic: false }];
    static NUMBER: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::Number, optional: false, variadic: false }];
    static ANY: [ArgInfo; 1] = [ArgInfo { name: "value", ty: TypeInfo::Any, optional: false, variadic: false }];
    match ty {
        TypeInfo::Boolean => &BOOLEAN,
        TypeInfo::String => &STRING,
        TypeInfo::Number => &NUMBER,
        TypeInfo::Nullable(TypeInfo::String) => &NULLABLE_STRING,
        TypeInfo::Callback | TypeInfo::Nullable(TypeInfo::Callback) => &CALLBACK,
        _ => &ANY,
    }
}

/// A host method's result, as a [`Value`].
pub trait IntoValue {
    fn into_value(self) -> Value;
}

impl IntoValue for () {
    fn into_value(self) -> Value {
        Value::Undefined
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}

macro_rules! numbers {
    ($($t:ty),*) => {$(
        impl IntoValue for $t {
            fn into_value(self) -> Value {
                Value::Number(f64::from(self))
            }
        }
    )*};
}
numbers!(i16, i32, u16, u32, f64);

impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::String(self)
    }
}

impl IntoValue for ObjectRef {
    fn into_value(self) -> Value {
        Value::Object(self)
    }
}

impl IntoValue for CallbackId {
    fn into_value(self) -> Value {
        Value::Callback(self)
    }
}

impl IntoValue for PromiseValue {
    fn into_value(self) -> Value {
        Value::Promise(self)
    }
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        self.map_or(Value::Null, IntoValue::into_value)
    }
}

/// Checks and converts values the runtime passed (WebIDL's conversions,
/// after the runtime's own; a runtime that passed the wrong kind of value is
/// answered with a `TypeError`, never trusted).
pub(crate) mod convert {
    use super::Interface;
    use crate::{CallbackId, HostError, ObjectRef, Value};

    pub fn this(this: Option<ObjectRef>, interface: Interface, what: &str) -> Result<ObjectRef, HostError> {
        match this {
            Some(o) if o.interface.inherits(interface) => Ok(o),
            _ => Err(HostError::type_error(format!("{what}: illegal invocation (`this` is not a {})", interface.name()))),
        }
    }

    pub fn boolean(v: Value) -> Result<bool, HostError> {
        Ok(match v {
            Value::Bool(b) => b,
            Value::Undefined | Value::Null => false,
            Value::Number(n) => n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
            _ => true,
        })
    }

    // `number` and `unsigned` are used by generated code when the IDL
    // declares an argument of a numeric type.
    #[allow(dead_code)]
    pub fn number(v: Value) -> Result<f64, HostError> {
        match v {
            Value::Number(n) => Ok(n),
            Value::Bool(b) => Ok(f64::from(u8::from(b))),
            Value::Null => Ok(0.0),
            Value::Undefined => Ok(f64::NAN),
            _ => Err(HostError::type_error("a number was expected")),
        }
    }

    /// WebIDL's `ToUint16`/`ToUint32`: modulo 2^bits, NaN and infinities 0.
    #[allow(dead_code)]
    pub fn unsigned(v: Value, bits: u32) -> Result<f64, HostError> {
        let n = number(v)?;
        if !n.is_finite() {
            return Ok(0.0);
        }
        let modulus = 2f64.powi(bits as i32);
        Ok(n.trunc().rem_euclid(modulus))
    }

    pub fn string(v: Value) -> Result<String, HostError> {
        match v {
            Value::String(s) => Ok(s),
            Value::Undefined => Ok("undefined".into()),
            Value::Null => Ok("null".into()),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Number(n) => Ok(crate::number_to_string(n)),
            _ => Err(HostError::type_error("a string was expected")),
        }
    }

    pub fn object(v: Value, interface: Interface) -> Result<ObjectRef, HostError> {
        match v {
            Value::Object(o) if o.interface.inherits(interface) => Ok(o),
            _ => Err(HostError::type_error(format!("the argument is not a {}", interface.name()))),
        }
    }

    pub fn callback(v: Value) -> Result<CallbackId, HostError> {
        match v {
            Value::Callback(c) => Ok(c),
            _ => Err(HostError::type_error("the argument is not a function")),
        }
    }

    pub fn dictionary(v: Value) -> Result<Vec<(String, Value)>, HostError> {
        match v {
            Value::Dictionary(members) => Ok(members),
            Value::Undefined | Value::Null => Ok(Vec::new()),
            _ => Err(HostError::type_error("an object was expected")),
        }
    }
}
