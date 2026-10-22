// The DOM interfaces script can reach (Milestone 40, ADR 023). An interface
// or member not declared here does not exist for script. Deviations from the
// DOM standard are marked; each is one line to correct when the interface it
// belongs to is built.

callback EventListener = undefined (Event event);

interface EventTarget {
  undefined addEventListener(DOMString type, EventListener? callback, optional boolean capture = false);
};

interface Node : EventTarget {
  readonly attribute unsigned short nodeType;
  attribute DOMString? textContent;
  Node appendChild(Node node);
  Node insertBefore(Node node, Node? child);
  Node removeChild(Node child);
  Node replaceChild(Node node, Node child);
  Node cloneNode(optional boolean deep = false);
};

// Standard: `Document : Node`. There is no document node in the arena
// (ADR 021); the document is reached through its root element.
interface Document : EventTarget {
  Element? getElementById(DOMString elementId);
  Element? querySelector(DOMString selectors);
  Element createElement(DOMString localName);
  Text createTextNode(DOMString data);
};

interface Element : Node {
  readonly attribute DOMString tagName;
  attribute DOMString id;
  DOMString? getAttribute(DOMString qualifiedName);
  undefined setAttribute(DOMString qualifiedName, DOMString value);
  undefined removeAttribute(DOMString qualifiedName);
  [SameObject] readonly attribute CSSStyleDeclaration style;
  // Standard: on `HTMLInputElement`.
  attribute boolean checked;
};

interface Text : Node {
};

// The properties of the `style` crate's that script may set (camelCase), as
// before Milestone 40; each reads and writes the inline `style` attribute.
interface CSSStyleDeclaration {
  attribute DOMString color;
  attribute DOMString backgroundColor;
  attribute DOMString width;
  attribute DOMString height;
  attribute DOMString fontSize;
  attribute DOMString textAlign;
  attribute DOMString transform;
};

interface Event {
  readonly attribute DOMString type;
  readonly attribute EventTarget? target;
  // Standard: on `KeyboardEvent`.
  readonly attribute DOMString? key;
  readonly attribute boolean defaultPrevented;
  undefined stopPropagation();
  undefined preventDefault();
};
