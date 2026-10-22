//! The DOM binding cases every runtime must pass (ADR 023 §7). Each is a
//! document, a list of steps — scripts in the subset of JavaScript the stub
//! runtime interprets (`crate::stub`), clicks, animation frames — and what
//! each step and the final document must be. `script`'s tests run them on
//! the stub; `js`'s run them on Boa. Both use only the generated bindings,
//! so passing on both is what "the host side is runtime-independent" means.

use crate::ScriptEnvironment;
use dom::{Document, Element, ElementRef, Node};
use std::cell::RefCell;
use std::rc::Rc;

pub struct Case {
    pub name: &'static str,
    pub document: fn() -> Document,
    pub steps: &'static [Step],
    /// The document after the steps (see [`serialize`]).
    pub expect_document: &'static str,
}

pub enum Step {
    /// Runs a script; its completion value must be the string given.
    Run(&'static str, &'static str),
    /// Runs a script that must throw an error whose message contains the
    /// string given.
    Throws(&'static str, &'static str),
    /// Clicks the first element matching this selector (the hit path: it,
    /// then its ancestors).
    Click(&'static str),
    /// Runs the animation-frame callbacks at this time.
    Frame(f64),
    /// Collects garbage (the runtime's, then the host's).
    Collect,
}

fn element(tag: &str, attributes: &[(&str, &str)], children: Vec<Node>) -> Element {
    let mut el = Element::new(tag);
    for (k, v) in attributes {
        el.set_attr(k, *v);
    }
    el.children = children;
    el
}

fn text(t: &str) -> Node {
    Node::Text(t.to_string())
}

/// `<div><ul id=list><li id=a>A</li><li id=b class=row>B</li><li id=c>C</li></ul>
/// <p class=plain>plain</p><input id=box type=checkbox></div>`
pub fn sample_document() -> Document {
    let list = element(
        "ul",
        &[("id", "list")],
        vec![
            Node::Element(element("li", &[("id", "a")], vec![text("A")])),
            Node::Element(element("li", &[("id", "b"), ("class", "row")], vec![text("B")])),
            Node::Element(element("li", &[("id", "c")], vec![text("C")])),
        ],
    );
    let plain = element("p", &[("class", "plain")], vec![text("plain")]);
    let checkbox = element("input", &[("id", "box"), ("type", "checkbox")], vec![]);
    Document::new(element("div", &[], vec![Node::Element(list), Node::Element(plain), Node::Element(checkbox)]))
}

const SAMPLE: &str = r#"<div><ul id="list"><li id="a">A</li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#;

/// A document as markup: elements with their attributes in order, text as it
/// is.
pub fn serialize(el: ElementRef<'_>) -> String {
    let mut out = format!("<{}", el.tag());
    for (k, v) in el.attributes() {
        out.push_str(&format!(" {k}=\"{v}\""));
    }
    out.push('>');
    for child in el.children() {
        match child {
            dom::ChildRef::Element(e) => out.push_str(&serialize(e)),
            dom::ChildRef::Text(t) => out.push_str(t),
        }
    }
    out.push_str(&format!("</{}>", el.tag()));
    out
}

pub const CASES: &[Case] = &[
    Case {
        name: "getElementById reads and writes text",
        document: sample_document,
        steps: &[
            Step::Run("document.getElementById('a').textContent", "A"),
            Step::Run("document.getElementById('nope') === null", "true"),
            Step::Run("document.getElementById('b').textContent = 'Bee'", "Bee"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><li id="b" class="row">Bee</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "one wrapper per node",
        document: sample_document,
        steps: &[
            Step::Run("document.getElementById('a') === document.getElementById('a')", "true"),
            Step::Run("document.getElementById('a') === document.getElementById('b')", "false"),
            Step::Run("document === document", "true"),
            Step::Run("var a = document.getElementById('a'); a.style === a.style", "true"),
            Step::Collect,
            Step::Run("a === document.getElementById('a')", "true"),
            Step::Run("document.querySelector('.row') === document.getElementById('b')", "true"),
        ],
        expect_document: SAMPLE,
    },
    Case {
        name: "querySelector finds elements without an id",
        document: sample_document,
        steps: &[
            Step::Run("var p = document.querySelector('.plain'); p.textContent", "plain"),
            Step::Run("p.setAttribute('data-x', '1'); p.getAttribute('data-x')", "1"),
            Step::Run("document.querySelector('span') === null", "true"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain" data-x="1">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "a created element's handle works before and after attaching",
        document: sample_document,
        steps: &[
            Step::Run("var x = document.createElement('li'); x.id = 'x'; x.textContent = 'X'; document.getElementById('x') === null", "true"),
            Step::Run("document.getElementById('list').appendChild(x) === x", "true"),
            Step::Run("x === document.getElementById('x')", "true"),
            Step::Run("x.textContent = 'X2'; x.tagName + ':' + x.nodeType", "LI:1"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><li id="b" class="row">B</li><li id="c">C</li><li id="x">X2</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "insertBefore places elements and text nodes",
        document: sample_document,
        steps: &[
            Step::Run("var list = document.getElementById('list'); var t = document.createTextNode('!'); t.nodeType", "3"),
            Step::Run("list.insertBefore(t, null); list.insertBefore(document.createElement('hr'), document.getElementById('b')); t.textContent", "!"),
            Step::Run("t.textContent = '?'; list.textContent", "ABC?"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><hr></hr><li id="b" class="row">B</li><li id="c">C</li>?</ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "removeChild returns the node, which can go back in",
        document: sample_document,
        steps: &[
            Step::Run("var list = document.getElementById('list'); var a = document.getElementById('a'); list.removeChild(a) === a", "true"),
            Step::Run("document.getElementById('a') === null", "true"),
            Step::Run("a.textContent = 'A2'; list.appendChild(a); a === document.getElementById('a')", "true"),
        ],
        expect_document: r#"<div><ul id="list"><li id="b" class="row">B</li><li id="c">C</li><li id="a">A2</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "appendChild of an attached node moves it; never under itself",
        document: sample_document,
        steps: &[
            Step::Run("var list = document.getElementById('list'); list.appendChild(document.getElementById('a')).id", "a"),
            Step::Throws("document.getElementById('a').appendChild(list)", "HierarchyRequestError"),
            Step::Throws("list.removeChild(list)", "NotFoundError"),
        ],
        expect_document: r#"<div><ul id="list"><li id="b" class="row">B</li><li id="c">C</li><li id="a">A</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "a detached subtree is built, then attached whole",
        document: sample_document,
        steps: &[
            Step::Run("var d = document.createElement('div'); var s = document.createElement('span'); d.appendChild(s); s.textContent = 'in'; d.textContent", "in"),
            Step::Throws("s.appendChild(d)", "HierarchyRequestError"),
            Step::Run("d.insertBefore(document.createElement('b'), s); d.id = 'd'; document.getElementById('a').appendChild(d); document.getElementById('d') === d", "true"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A<div id="d"><b></b><span>in</span></div></li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "replaceChild and cloneNode",
        document: sample_document,
        steps: &[
            Step::Run("var list = document.getElementById('list'); var copy = document.getElementById('c').cloneNode(true); copy.id = 'c2'; copy === document.getElementById('c')", "false"),
            Step::Run("var old = list.replaceChild(copy, document.getElementById('b')); old.id", "b"),
            Step::Run("list.appendChild(old); document.getElementById('a').cloneNode().textContent", ""),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><li id="c2">C</li><li id="c">C</li><li id="b" class="row">B</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "attributes, style and checked",
        document: sample_document,
        steps: &[
            Step::Run("var a = document.getElementById('a'); a.getAttribute('class') === null", "true"),
            Step::Run("a.setAttribute('class', 'x'); a.getAttribute('class')", "x"),
            Step::Run("a.removeAttribute('class'); a.style.color = 'red'; a.style.backgroundColor = 'blue'; a.style.color", "red"),
            Step::Run("var box = document.getElementById('box'); box.checked", "false"),
            Step::Run("box.checked = true; box.checked", "true"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a" style="color:red;background-color:blue">A</li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox" checked="checked"></input></div>"#,
    },
    Case {
        name: "arguments are checked against the IDL",
        document: sample_document,
        steps: &[
            Step::Run("var list = document.getElementById('list'); 'ok'", "ok"),
            Step::Throws("list.appendChild()", "1 argument(s) required"),
            Step::Throws("list.insertBefore(null, null)", "TypeError"),
            Step::Throws("list.appendChild('text')", "TypeError"),
            Step::Throws("list.removeChild(document)", "TypeError"),
            Step::Throws("document.createElement()", "TypeError"),
        ],
        expect_document: SAMPLE,
    },
    Case {
        name: "listeners need no id; the target is the node's wrapper",
        document: sample_document,
        steps: &[
            Step::Run(
                "var seen = ''; var p = document.querySelector('.plain'); \
                 p.addEventListener('click', (e) => { seen = seen + e.type + ':' + (e.target === p) + ';' }); 'ok'",
                "ok",
            ),
            Step::Run("document.addEventListener('click', (e) => { seen = seen + 'doc;' }); 'ok'", "ok"),
            Step::Click(".plain"),
            Step::Run("seen", "click:true;doc;"),
        ],
        expect_document: SAMPLE,
    },
    Case {
        name: "capture, target and bubble order; stopPropagation; preventDefault",
        document: sample_document,
        steps: &[
            Step::Run(
                "var order = ''; var list = document.getElementById('list'); \
                 list.addEventListener('click', (e) => { order = order + 'capture,' }, true); \
                 list.addEventListener('click', (e) => { order = order + 'bubble,' }); \
                 document.getElementById('a').addEventListener('click', (e) => { order = order + 'target,' }); 'ok'",
                "ok",
            ),
            Step::Click("#a"),
            Step::Run("order", "capture,target,bubble,"),
            Step::Run("document.getElementById('c').addEventListener('click', (e) => e.stopPropagation()); order = ''", ""),
            Step::Click("#c"),
            Step::Run("order", "capture,"),
            Step::Run("var box = document.getElementById('box'); box.addEventListener('click', (e) => e.preventDefault()); 'ok'", "ok"),
            Step::Click("#box"),
            Step::Run("box.checked", "false"),
        ],
        expect_document: SAMPLE,
    },
    Case {
        name: "a click's default action toggles a checkbox",
        document: sample_document,
        steps: &[Step::Click("#box"), Step::Run("document.getElementById('box').checked", "true")],
        expect_document: r#"<div><ul id="list"><li id="a">A</li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox" checked="checked"></input></div>"#,
    },
    Case {
        name: "requestAnimationFrame runs on the next frame",
        document: sample_document,
        steps: &[
            Step::Run("var at = 'never'; requestAnimationFrame((t) => { at = t; document.getElementById('a').textContent = 'framed' }); at", "never"),
            Step::Frame(16.0),
            Step::Run("at", "16"),
        ],
        expect_document: r#"<div><ul id="list"><li id="a">framed</li><li id="b" class="row">B</li><li id="c">C</li></ul><p class="plain">plain</p><input id="box" type="checkbox"></input></div>"#,
    },
    Case {
        name: "sessionStorage round-trips",
        document: sample_document,
        steps: &[
            Step::Run("sessionStorage.setItem('k', 'v'); sessionStorage.getItem('k')", "v"),
            Step::Run("sessionStorage.removeItem('k'); sessionStorage.getItem('k') === null", "true"),
            Step::Run("sessionStorage === sessionStorage", "true"),
        ],
        expect_document: SAMPLE,
    },
    Case {
        name: "detached nodes no wrapper reaches are freed",
        document: sample_document,
        steps: &[
            Step::Run("var a = document.createElement('i'); var b = document.createElement('b'); a = null; b = null; 'ok'", "ok"),
            Step::Collect,
            Step::Run("var keep = document.createElement('p'); keep.id = 'k'; keep.id", "k"),
        ],
        expect_document: SAMPLE,
    },
];

/// The hit path of the first element matching `selector`: it, then its
/// ancestors.
fn hit_path(document: &Document, selector: &str) -> Vec<dom::NodeId> {
    let Some(node) = document.root().query_selector(selector).map(|e| e.node_id()) else { return Vec::new() };
    let mut path = vec![node];
    path.extend(document.ancestors_of(node).iter().map(|e| e.node_id()));
    path
}

/// Runs `case` on environments `make` builds; `Err` says which step failed.
pub fn run(case: &Case, make: &dyn Fn(Rc<RefCell<Document>>) -> ScriptEnvironment) -> Result<(), String> {
    let document = Rc::new(RefCell::new((case.document)()));
    let mut env = make(Rc::clone(&document));
    for (i, step) in case.steps.iter().enumerate() {
        let at = || format!("{}: step {i}", case.name);
        match step {
            Step::Run(source, expected) => match env.run_script(source) {
                Ok(v) if v == *expected => {}
                Ok(v) => return Err(format!("{}: `{source}` gave {v:?}, expected {expected:?}", at())),
                Err(e) => return Err(format!("{}: `{source}` threw {e}", at())),
            },
            Step::Throws(source, expected) => match env.run_script(source) {
                Err(e) if e.contains(expected) => {}
                Err(e) => return Err(format!("{}: `{source}` threw {e:?}, expected one containing {expected:?}", at())),
                Ok(v) => return Err(format!("{}: `{source}` gave {v:?}, expected it to throw", at())),
            },
            Step::Click(id) => {
                let path = hit_path(&document.borrow(), id);
                env.dispatch_click(&path);
            }
            Step::Frame(t) => env.run_animation_frame_callbacks(*t),
            Step::Collect => env.collect_garbage(),
        }
    }
    let got = serialize(document.borrow().root());
    if got != case.expect_document {
        return Err(format!("{}: the document is\n  {got}\nexpected\n  {}", case.name, case.expect_document));
    }
    if !document.borrow().index_is_consistent() {
        return Err(format!("{}: the document's indexes are inconsistent", case.name));
    }
    Ok(())
}
