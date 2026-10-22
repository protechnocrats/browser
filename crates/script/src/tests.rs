use crate::bindings::{dispatch, InterfaceKind, MemberKind};
use crate::stub::StubRuntime;
use crate::*;
use std::cell::RefCell;
use std::rc::Rc;

fn stub_environment(document: Rc<RefCell<dom::Document>>) -> ScriptEnvironment {
    let page = network::Initiator::document(network::Origin::new_opaque(), None);
    ScriptEnvironment::new(document, page, ScriptCapabilities::default(), |host| Box::new(StubRuntime::new(host)))
}

/// ADR 023 §7: the stub runtime drives the DOM binding cases (the same
/// cases `js` runs on Boa).
#[test]
fn the_stub_runtime_passes_every_binding_case() {
    let failures: Vec<String> = cases::CASES.iter().filter_map(|case| cases::run(case, &stub_environment).err()).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_runtime_that_can_be_interrupted_stops_at_its_next_check() {
    let mut env = stub_environment(Rc::new(RefCell::new(cases::sample_document())));
    let handle = env.interrupt_handle().expect("the stub can be interrupted");
    handle.interrupt();
    let err = env.run_script("document.getElementById('a').textContent = 'never'").unwrap_err();
    assert!(err.starts_with("Interrupted"), "{err}");
    assert_eq!(env.run_script("document.getElementById('a').textContent").unwrap(), "A", "the interrupt was taken, and nothing ran");
}

#[test]
fn events_are_freed_after_dispatch_and_xhrs_when_unwrapped() {
    let document = Rc::new(RefCell::new(cases::sample_document()));
    let mut env = stub_environment(Rc::clone(&document));
    let path = vec![document.borrow().node_id_of("a").unwrap()];
    env.dispatch_click(&path);
    assert_eq!(env.host().object_count(), 0, "an event no listener saw is freed at once");
    env.run_script("var e = 0; document.getElementById('a').addEventListener('click', (ev) => { e = ev })").unwrap();
    env.dispatch_click(&path);
    assert_eq!(env.host().object_count(), 1, "a script that keeps the event keeps it alive");
    env.run_script("e = 0").unwrap();
    env.collect_garbage();
    env.run_script("var x = new XMLHttpRequest(); x.withCredentials = true; x.withCredentials").unwrap();
    assert_eq!(env.host().object_count(), 2, "the stub keeps the wrappers of objects it saw (the event and the XHR)");
}

#[test]
fn dispatch_checks_this_and_refuses_values_the_runtime_should_not_pass() {
    let document = Rc::new(RefCell::new(cases::sample_document()));
    let env = stub_environment(Rc::clone(&document));
    let host = &env.host;
    let a = document.borrow().node_id_of("a").unwrap();
    let element = ObjectRef::new(Interface::Element, a);
    let index = |i: Interface, name: &str| i.info().members.iter().position(|m| m.name == name).unwrap() as u16;
    // `appendChild` is declared on `Node`: an `Element` is one, the
    // document is not.
    let append = Call::Operation(Interface::Node, index(Interface::Node, "appendChild"));
    let err = dispatch(&mut *host.borrow_mut(), append, Some(DOCUMENT), vec![Value::Object(element)]).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TypeError);
    for bad in [Value::Other, Value::Number(1.0), Value::Callback(CallbackId(0)), Value::Object(DOCUMENT)] {
        let err = dispatch(&mut *host.borrow_mut(), append, Some(element), vec![bad]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::TypeError);
    }
    // A member index the IDL does not have.
    assert!(dispatch(&mut *host.borrow_mut(), Call::Operation(Interface::Node, 999), Some(element), vec![]).is_err());
}

#[test]
fn the_generated_table_matches_the_idl() {
    assert!(Interface::Element.inherits(Interface::Node) && Interface::Element.inherits(Interface::EventTarget));
    assert!(!Interface::Document.inherits(Interface::Node));
    assert_eq!(Interface::Window.info().kind, InterfaceKind::Global);
    assert_eq!(Interface::Console.info().kind, InterfaceKind::Namespace);
    assert!(Interface::XMLHttpRequest.info().constructor.is_some());
    let node = Interface::Node.info();
    let insert = node.members.iter().find(|m| m.name == "insertBefore").unwrap();
    let MemberKind::Operation { args, .. } = &insert.kind else { panic!("insertBefore is an operation") };
    assert_eq!(args.len(), 2);
    // Every interface's members have distinct names.
    for i in Interface::ALL {
        let mut names: Vec<_> = i.info().members.iter().map(|m| m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), i.info().members.len(), "{}", i.name());
    }
}

#[test]
fn numbers_print_as_script_prints_them() {
    assert_eq!(number_to_string(16.0), "16");
    assert_eq!(number_to_string(-0.0), "0");
    assert_eq!(number_to_string(1.5), "1.5");
    assert_eq!(number_to_string(f64::NAN), "NaN");
    assert_eq!(number_to_string(f64::NEG_INFINITY), "-Infinity");
}
