//! [`BoaRuntime`]: Boa as a [`ScriptRuntime`] (Milestone 40, ADR 023).
//!
//! Script objects are built from the generated member table
//! (`script::bindings`): one prototype per interface, chained as the IDL
//! inherits, with one native function per operation and an accessor per
//! attribute, every one of them calling `script::bindings::dispatch`. The
//! global object carries `[Global]`'s members, a namespace is a plain object,
//! and an interface with a constructor is a global constructor.
//!
//! ## Wrappers
//!
//! A host object's wrapper carries its [`ObjectRef`] as native data — an
//! internal slot script cannot forge or read. There is one wrapper per
//! `ObjectRef` while it lives: a cache of weak references finds it again
//! (ADR 023 §4). The wrapper and a small cell point at each other — the
//! wrapper holds the cell, the cell holds the wrapper and the host's
//! [`WrapperGuard`] — and the cache holds the cell weakly, so the pair is an
//! ordinary garbage cycle, collected when script lets go of the wrapper; the
//! guard then tells the host.
//!
//! ## No untraced handles
//!
//! Every native function is a `Copy` closure over a `Call` — it captures no
//! GC handle, and Boa's `from_copy_closure` needs no `unsafe_ignore_trace`.
//! The script functions and promise resolvers the host refers to by
//! [`CallbackId`] and [`PromiseId`] live in [`Tables`], a traced
//! `Gc<GcRefCell<…>>` (ADR 023 §5; ADR 015's sound alternative). The rest of
//! the runtime's state — prototypes, the cache, the host — sits in the
//! context's host-defined data, owned by the context, outside the heap.

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::object::builtins::{JsFunction, JsPromise};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::property::PropertyDescriptor;
use boa_engine::{js_string, Context, Finalize, JsData, JsError, JsNativeError, JsObject, JsResult, JsValue, NativeFunction, Source, Trace};
use boa_gc::{Gc, GcRefCell, WeakGc};
use script::bindings::{dispatch, InterfaceKind, MemberKind, TypeInfo};
use script::{
    Call, CallbackId, ErrorKind, HostError, HostRef, Interface, InterruptHandle, ObjectRef, PromiseId, PromiseValue, Rejection, ScriptError, ScriptErrorKind,
    ScriptRuntime, Settlement, Value, WrapperGuard, WINDOW,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A wrapper's internal slot.
#[derive(Trace, Finalize, JsData)]
struct Wrapper {
    #[unsafe_ignore_trace]
    object: ObjectRef,
    cell: Gc<WrapperCell>,
}

/// The other half of a wrapper's cycle (see the module docs).
#[derive(Trace, Finalize)]
struct WrapperCell {
    wrapper: GcRefCell<Option<JsObject>>,
    #[unsafe_ignore_trace]
    _guard: WrapperGuard,
}

/// Script objects the host names by id. Traced: these handles are inside
/// the heap, marked through the `Gc` that holds this.
#[derive(Trace, Finalize, Default)]
struct Tables {
    callbacks: HashMap<u32, JsFunction>,
    next_callback: u32,
    promises: HashMap<u64, ResolvingFunctions>,
    /// The wrappers of nodes in the document, by node id: kept, so a node's
    /// identity and expando properties last while the document can reach
    /// it. Weak again once the node leaves (see [`keep_document_wrappers`]).
    preserved: HashMap<u64, JsObject>,
}

/// The runtime's state, in the context's host-defined data.
struct RuntimeState {
    host: HostRef,
    tables: Gc<GcRefCell<Tables>>,
    prototypes: HashMap<Interface, JsObject>,
    wrappers: RefCell<HashMap<ObjectRef, WeakGc<WrapperCell>>>,
    /// Live entries when the cache was last pruned of dead ones.
    live_at_prune: RefCell<usize>,
    /// Singletons (the document, the storages): kept, so their identity and
    /// expando properties last as long as the page.
    pinned: RefCell<Vec<JsObject>>,
    /// The original `JSON.parse`, for `Value::Json`.
    json_parse: JsFunction,
    /// The document's link updates when the kept wrappers were last checked.
    preserved_checked_at: RefCell<u64>,
}

struct StateHandle(Rc<RuntimeState>);

fn state(context: &Context) -> JsResult<Rc<RuntimeState>> {
    context.get_data::<StateHandle>().map(|h| Rc::clone(&h.0)).ok_or_else(|| JsNativeError::error().with_message("script runtime is gone").into())
}

/// Boa 0.22 behind the script runtime interface.
pub struct BoaRuntime {
    context: Context,
}

impl BoaRuntime {
    /// A runtime over `host`, its clock and timezone under `fingerprint`
    /// (ADR 019 §3) and its execution limits set.
    pub fn new(host: HostRef, fingerprint: privacy::FingerprintPolicy) -> BoaRuntime {
        let clock = Rc::new(crate::PolicyClock { policy: fingerprint, monotonic: boa_engine::context::time::StdClock::new() });
        let mut context = Context::builder().host_hooks(Rc::new(crate::PolicyHooks(fingerprint))).clock(clock).build().expect("a context with default intrinsics builds");
        {
            // Zero-trust: page script is hostile input. See the constants.
            let limits = context.runtime_limits_mut();
            limits.set_loop_iteration_limit(crate::MAX_LOOP_ITERATIONS);
            limits.set_recursion_limit(crate::MAX_RECURSION_DEPTH);
        }
        let json = context.intrinsics().objects().json();
        let json_parse = json
            .get(js_string!("parse"), &mut context)
            .ok()
            .and_then(|v| v.as_object())
            .and_then(JsFunction::from_object)
            .expect("the intrinsic JSON.parse is a function");
        let mut prototypes = HashMap::new();
        for &interface in Interface::ALL {
            build_prototype(interface, &mut prototypes, &mut context);
        }
        let state = RuntimeState {
            host,
            tables: Gc::new(GcRefCell::new(Tables::default())),
            prototypes,
            wrappers: RefCell::default(),
            live_at_prune: RefCell::new(0),
            pinned: RefCell::default(),
            json_parse,
            preserved_checked_at: RefCell::new(0),
        };
        context.insert_data(StateHandle(Rc::new(state)));
        for &interface in Interface::ALL {
            install_global(interface, &mut context);
        }
        BoaRuntime { context }
    }

    fn state(&self) -> Rc<RuntimeState> {
        state(&self.context).expect("installed in new")
    }
}

/// The prototype of `interface` (and its ancestors', first).
fn build_prototype(interface: Interface, prototypes: &mut HashMap<Interface, JsObject>, context: &mut Context) {
    let info = interface.info();
    if info.kind != InterfaceKind::Interface || prototypes.contains_key(&interface) {
        return;
    }
    let parent = info.parent.map(|p| {
        build_prototype(p, prototypes, context);
        prototypes[&p].clone()
    });
    let proto = JsObject::with_object_proto(context.intrinsics());
    if let Some(parent) = parent {
        proto.set_prototype(Some(parent));
    }
    define_members(&proto, interface, context);
    prototypes.insert(interface, proto);
}

fn function(call: Call, name: &str, length: usize, context: &mut Context) -> JsFunction {
    let native = NativeFunction::from_copy_closure(move |this, args, context| invoke(call, this, args, context));
    FunctionObjectBuilder::new(context.realm(), native).name(js_string!(name)).length(length).build()
}

fn required_args(call: Call) -> usize {
    call.arg_types().iter().filter(|a| !a.optional && !a.variadic).count()
}

/// Defines `interface`'s members on `target`.
fn define_members(target: &JsObject, interface: Interface, context: &mut Context) {
    for (index, member) in interface.info().members.iter().enumerate() {
        let index = index as u16;
        let desc = match &member.kind {
            MemberKind::Operation { .. } => {
                let call = Call::Operation(interface, index);
                let f = function(call, member.name, required_args(call), context);
                PropertyDescriptor::builder().value(f).writable(true).enumerable(true).configurable(true).build()
            }
            MemberKind::Attribute { readonly, .. } => {
                let getter = function(Call::Get(interface, index), member.name, 0, context);
                let mut builder = PropertyDescriptor::builder().get(getter).enumerable(true).configurable(true);
                builder = if *readonly { builder.set(JsValue::undefined()) } else { builder.set(function(Call::Set(interface, index), member.name, 1, context)) };
                builder.build()
            }
        };
        let _ = target.define_property_or_throw(js_string!(member.name), desc, context);
    }
}

/// What `interface` adds to the global object: a namespace object, the
/// global's own members, or a constructor.
fn install_global(interface: Interface, context: &mut Context) {
    let info = interface.info();
    match info.kind {
        InterfaceKind::Namespace => {
            let object = JsObject::with_object_proto(context.intrinsics());
            define_members(&object, interface, context);
            let _ = context.register_global_property(js_string!(info.name), object, boa_engine::property::Attribute::all());
        }
        InterfaceKind::Global => {
            // The global's own members and its ancestors' (`Window` is an
            // `EventTarget`), as a prototype chain would give them.
            let global = context.global_object();
            let mut current = Some(interface);
            while let Some(i) = current {
                define_members(&global, i, context);
                current = i.info().parent;
            }
        }
        InterfaceKind::Interface => {
            if info.constructor.is_none() {
                return;
            }
            let call = Call::Construct(interface);
            let native = NativeFunction::from_copy_closure(move |this, args, context| invoke(call, this, args, context));
            let ctor = FunctionObjectBuilder::new(context.realm(), native).name(js_string!(info.name)).length(required_args(call)).constructor(true).build();
            if let Ok(st) = state(context) {
                let proto = st.prototypes[&interface].clone();
                let _ = ctor.define_property_or_throw(js_string!("prototype"), PropertyDescriptor::builder().value(proto.clone()).writable(false).enumerable(false).configurable(false).build(), context);
                let _ = proto.define_property_or_throw(js_string!("constructor"), PropertyDescriptor::builder().value(ctor.clone()).writable(true).enumerable(false).configurable(true).build(), context);
            }
            let _ = context.register_global_property(js_string!(info.name), ctor, boa_engine::property::Attribute::WRITABLE | boa_engine::property::Attribute::CONFIGURABLE);
        }
    }
}

/// Every native function: convert the arguments as declared, dispatch to
/// the host, convert the result.
fn invoke(call: Call, this: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let st = state(context)?;
    let interface = match call {
        Call::Operation(i, _) | Call::Get(i, _) | Call::Set(i, _) | Call::Construct(i) => i,
    };
    let this_ref = match interface.info().kind {
        InterfaceKind::Global => Some(WINDOW),
        InterfaceKind::Namespace => None,
        InterfaceKind::Interface => object_ref_of(this, context),
    };
    let types = call.arg_types();
    let mut values = Vec::with_capacity(args.len());
    let first_callback = st.tables.borrow().next_callback;
    let mut converted = Ok(());
    for (i, arg) in args.iter().enumerate() {
        let Some(info) = types.get(i).or_else(|| types.last().filter(|t| t.variadic)) else { break };
        match to_value(arg, &info.ty, &st, context) {
            Ok(v) => values.push(v),
            Err(e) => {
                converted = Err(e);
                break;
            }
        }
    }
    // Nodes passed to an operation may be inserted by it.
    let moved: Vec<ObjectRef> = match call {
        Call::Operation(..) => values.iter().filter_map(|v| if let Value::Object(o) = v { Some(*o) } else { None }).filter(|o| o.interface.inherits(Interface::Node)).collect(),
        _ => Vec::new(),
    };
    let result = converted.and_then(|()| {
        let mut host = st.host.borrow_mut();
        host.collect_closed();
        dispatch(&mut *host, call, this_ref, values).map_err(host_error)
    });
    keep_document_wrappers(&moved, &st);
    match result {
        Ok(value) => to_js(value, &st, context),
        Err(e) => {
            // Callbacks registered for a call that failed are nobody's.
            let mut tables = st.tables.borrow_mut();
            let next = tables.next_callback;
            tables.callbacks.retain(|id, _| *id < first_callback || *id >= next);
            Err(e)
        }
    }
}

fn host_error(e: HostError) -> JsError {
    match e.kind {
        ErrorKind::Error => JsNativeError::error().with_message(e.message).into(),
        ErrorKind::TypeError => JsNativeError::typ().with_message(e.message).into(),
    }
}

fn object_ref_of(value: &JsValue, context: &Context) -> Option<ObjectRef> {
    let object = value.as_object()?;
    if object == context.global_object() {
        return Some(WINDOW); // the global object is the host's window
    }
    object.downcast_ref::<Wrapper>().map(|w| w.object)
}

fn js_to_string(v: &JsValue, context: &mut Context) -> JsResult<String> {
    Ok(v.to_string(context)?.to_std_string_escaped())
}

/// `v` as the host expects a value of type `ty` (WebIDL's conversions).
fn to_value(v: &JsValue, ty: &TypeInfo, st: &RuntimeState, context: &mut Context) -> JsResult<Value> {
    Ok(match ty {
        TypeInfo::Undefined | TypeInfo::Promise => Value::Undefined,
        TypeInfo::Boolean => Value::Bool(v.to_boolean()),
        TypeInfo::Number => Value::Number(v.to_number(context)?),
        TypeInfo::String => Value::String(js_to_string(v, context)?),
        TypeInfo::Any => primitive(v).or_else(|| object_ref_of(v, context).map(Value::Object)).unwrap_or_else(|| Value::String(js_to_string(v, context).unwrap_or_else(|_| "[object]".into()))),
        TypeInfo::Object(_) => {
            if v.is_undefined() {
                Value::Undefined
            } else if v.is_null() {
                Value::Null
            } else {
                object_ref_of(v, context).map_or(Value::Other, Value::Object)
            }
        }
        TypeInfo::Callback => match v.as_object().and_then(JsFunction::from_object) {
            Some(f) => Value::Callback(register_callback(st, f)),
            None if v.is_null_or_undefined() => Value::Null,
            None => Value::Other,
        },
        // A nullable callback that is not callable is null
        // (`[LegacyTreatNonObjectAsNull]`, as event handlers are).
        TypeInfo::Nullable(TypeInfo::Callback) => match v.as_object().and_then(JsFunction::from_object) {
            Some(f) => Value::Callback(register_callback(st, f)),
            None => Value::Null,
        },
        TypeInfo::Nullable(inner) => {
            if v.is_null_or_undefined() {
                Value::Null
            } else {
                to_value(v, inner, st, context)?
            }
        }
        TypeInfo::Dictionary(members) => {
            if v.is_null_or_undefined() {
                return Ok(Value::Undefined);
            }
            let Some(object) = v.as_object() else { return Ok(Value::Other) };
            let mut present = Vec::new();
            for (name, member_ty) in members.iter() {
                let member = object.get(js_string!(*name), context)?;
                if !member.is_undefined() {
                    present.push((name.to_string(), to_value(&member, member_ty, st, context)?));
                }
            }
            Value::Dictionary(present)
        }
    })
}

fn primitive(v: &JsValue) -> Option<Value> {
    if v.is_undefined() {
        Some(Value::Undefined)
    } else if v.is_null() {
        Some(Value::Null)
    } else if let Some(b) = v.as_boolean() {
        Some(Value::Bool(b))
    } else if let Some(n) = v.as_number() {
        Some(Value::Number(n))
    } else {
        v.as_string().map(|s| Value::String(s.to_std_string_escaped()))
    }
}

fn register_callback(st: &RuntimeState, f: JsFunction) -> CallbackId {
    let mut tables = st.tables.borrow_mut();
    let id = tables.next_callback;
    tables.next_callback = id.wrapping_add(1);
    tables.callbacks.insert(id, f);
    CallbackId(id)
}

/// The script value for a host value.
fn to_js(value: Value, st: &RuntimeState, context: &mut Context) -> JsResult<JsValue> {
    Ok(match value {
        Value::Undefined | Value::Other => JsValue::undefined(),
        Value::Null => JsValue::null(),
        Value::Bool(b) => JsValue::from(b),
        Value::Number(n) => JsValue::from(n),
        Value::String(s) => JsValue::from(js_string!(s)),
        Value::Object(o) if o == WINDOW => context.global_object().into(),
        Value::Object(o) => wrap(o, st, context).into(),
        Value::Callback(id) => st.tables.borrow().callbacks.get(&id.0).cloned().map_or_else(JsValue::null, JsValue::from),
        Value::Json(text) => st.json_parse.call(&JsValue::undefined(), &[JsValue::from(js_string!(text))], context)?,
        Value::Dictionary(members) => {
            let object = JsObject::with_object_proto(context.intrinsics());
            for (name, v) in members {
                let v = to_js(v, st, context)?;
                object.set(js_string!(name), v, false, context)?;
            }
            object.into()
        }
        Value::Promise(PromiseValue::Pending(id)) => {
            let (promise, resolvers) = JsPromise::new_pending(context);
            st.tables.borrow_mut().promises.insert(id.0, resolvers);
            promise.into()
        }
        Value::Promise(PromiseValue::Resolved(v)) => match to_js(*v, st, context) {
            Ok(v) => JsPromise::resolve(v, context)?.into(),
            // `Response.json()` of text that is not JSON: a rejection.
            Err(e) => JsPromise::reject(JsError::from_opaque(js_string!(e.to_string()).into()), context)?.into(),
        },
        Value::Promise(PromiseValue::Rejected(r)) => {
            let reason = rejection(*r, st, context)?;
            JsPromise::reject(JsError::from_opaque(reason), context)?.into()
        }
    })
}

fn rejection(r: Rejection, st: &RuntimeState, context: &mut Context) -> JsResult<JsValue> {
    match r {
        Rejection::Value(v) => to_js(v, st, context),
        // A native error always becomes an object.
        Rejection::Error(e) => host_error(e).into_opaque(context),
    }
}

/// The one wrapper of `object` (ADR 023 §4).
fn wrap(object: ObjectRef, st: &RuntimeState, context: &mut Context) -> JsObject {
    if let Some(existing) = st.wrappers.borrow().get(&object).and_then(WeakGc::upgrade).and_then(|cell| cell.wrapper.borrow().clone()) {
        return existing;
    }
    let guard = st.host.borrow_mut().open_wrapper(object);
    let cell = Gc::new(WrapperCell { wrapper: GcRefCell::new(None), _guard: guard });
    let proto = st.prototypes.get(&object.interface).cloned();
    let wrapper = JsObject::from_proto_and_data(proto.or_else(|| Some(context.intrinsics().constructors().object().prototype())), Wrapper { object, cell: cell.clone() });
    *cell.wrapper.borrow_mut() = Some(wrapper.clone());
    let mut wrappers = st.wrappers.borrow_mut();
    wrappers.insert(object, WeakGc::new(&cell));
    // Dead entries are dropped once the cache has doubled since the last
    // pruning: amortised O(1) per wrapper.
    let mut live = st.live_at_prune.borrow_mut();
    if wrappers.len() > 2 * *live + 64 {
        wrappers.retain(|_, weak| weak.upgrade().is_some());
        *live = wrappers.len();
    }
    drop(wrappers);
    drop(live);
    if matches!(object.interface, Interface::Document | Interface::Storage) {
        st.pinned.borrow_mut().push(wrapper.clone());
    } else if object.interface.inherits(Interface::Node) && st.host.borrow().is_in_document(object.key) {
        st.tables.borrow_mut().preserved.insert(object.key, wrapper.clone());
    }
    wrapper
}

/// Keeps the wrappers of nodes in the document (M40's gap: expandos on a
/// collected wrapper were lost). A node an operation inserted keeps its
/// wrapper, and so do its descendants' live ones; one an operation removed
/// (`removeChild`, `replaceChild`) lets them go. A node that left the
/// document otherwise (its parent's `textContent` replaced) is found once
/// the document has changed by as many links as there are kept wrappers, so
/// that check is amortised O(1) per tree change.
fn keep_document_wrappers(moved: &[ObjectRef], st: &RuntimeState) {
    let host = st.host.borrow();
    if !moved.is_empty() {
        let cache = st.wrappers.borrow();
        let mut tables = st.tables.borrow_mut();
        for node in moved {
            let attached = host.is_in_document(node.key);
            if attached == tables.preserved.contains_key(&node.key) {
                continue; // kept and in the document, or neither
            }
            let ids = host.subtree(node.key);
            let ids = if ids.is_empty() { vec![node.key] } else { ids }; // a text node
            if !attached {
                for id in ids {
                    tables.preserved.remove(&id);
                }
                continue;
            }
            for id in ids {
                let object = if id == node.key { *node } else { ObjectRef::new(Interface::Element, id) };
                if let Some(wrapper) = cache.get(&object).and_then(WeakGc::upgrade).and_then(|cell| cell.wrapper.borrow().clone()) {
                    tables.preserved.insert(id, wrapper);
                }
            }
        }
    }
    let updates = host.document_link_updates();
    let mut checked = st.preserved_checked_at.borrow_mut();
    let mut tables = st.tables.borrow_mut();
    if updates.saturating_sub(*checked) as usize >= tables.preserved.len().max(64) {
        tables.preserved.retain(|id, _| host.is_in_document(*id));
        *checked = updates;
    }
}

fn script_error(e: &JsError) -> ScriptError {
    let message = e.to_string();
    let kind = if message.starts_with("RuntimeLimit") { ScriptErrorKind::RuntimeLimit } else { ScriptErrorKind::Thrown };
    ScriptError { kind, message }
}

impl ScriptRuntime for BoaRuntime {
    fn evaluate(&mut self, source: &str) -> Result<String, ScriptError> {
        let value = self.context.eval(Source::from_bytes(source)).map_err(|e| script_error(&e))?;
        value.to_string(&mut self.context).map(|s| s.to_std_string_escaped()).map_err(|e| script_error(&e))
    }

    fn run_jobs(&mut self) -> Result<(), ScriptError> {
        self.context.run_jobs().map_err(|e| script_error(&e))
    }

    fn call(&mut self, callback: CallbackId, this: &Value, args: &[Value]) -> Result<Value, ScriptError> {
        let st = self.state();
        let Some(f) = st.tables.borrow().callbacks.get(&callback.0).cloned() else { return Ok(Value::Undefined) };
        let context = &mut self.context;
        let this = to_js(this.clone(), &st, context).map_err(|e| script_error(&e))?;
        let args = args.iter().map(|a| to_js(a.clone(), &st, context)).collect::<JsResult<Vec<_>>>().map_err(|e| script_error(&e))?;
        let result = f.call(&this, &args, context).map_err(|e| script_error(&e))?;
        Ok(primitive(&result).unwrap_or(Value::Other))
    }

    fn settle(&mut self, promise: PromiseId, settlement: Settlement) {
        let st = self.state();
        let Some(resolvers) = st.tables.borrow_mut().promises.remove(&promise.0) else { return };
        let context = &mut self.context;
        let result = match settlement {
            Ok(v) => match to_js(v, &st, context) {
                Ok(v) => resolvers.resolve.call(&JsValue::undefined(), &[v], context),
                Err(e) => {
                    let reason = e.into_opaque(context).unwrap_or_default();
                    resolvers.reject.call(&JsValue::undefined(), &[reason], context)
                }
            },
            Err(r) => {
                let reason = rejection(r, &st, context).unwrap_or_default();
                resolvers.reject.call(&JsValue::undefined(), &[reason], context)
            }
        };
        if let Err(e) = result {
            script::report_script_error("promise settlement", &script_error(&e));
        }
    }

    fn release(&mut self, callbacks: &[CallbackId]) {
        let st = self.state();
        let mut tables = st.tables.borrow_mut();
        for id in callbacks {
            tables.callbacks.remove(&id.0);
        }
    }

    fn interrupt_handle(&self) -> Option<InterruptHandle> {
        // Boa 0.22 has no interrupt (ADR 013); the renderer watchdog bounds
        // a runaway page instead.
        None
    }

    fn collect_garbage(&mut self) {
        boa_gc::force_collect();
    }
}
