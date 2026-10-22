//! Milestone 40b (ADR 021 §5, ADR 024): interned tag and attribute names.
//!
//! An [`Atom`] is a `u32`. Below [`STATIC_COUNT`] it names one of the
//! well-known names in [`STATIC`], with the same value everywhere —
//! `atoms::CLASS` can be compared against any element's attributes. Above,
//! it indexes one document's [`AtomTable`], which holds the names that page
//! invented. The table belongs to the document, not the process: a page
//! inventing names cannot grow memory that outlives it.
//!
//! A [`Name`] is what an element holds: a well-known atom (no allocation),
//! or, in a fragment (a parsed tree before it becomes a document, a
//! script-built subtree, a test's tree), the name's own text. Moving a
//! fragment into a document interns its other names in place
//! ([`AtomTable::intern_name`]): the attribute list is not reallocated, and
//! a page of well-known names is not touched at all. M39 measured the
//! opposite order — names born as strings, interned afterwards — as a loss.

#![forbid(unsafe_code)]

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::{Arc, OnceLock};

/// An interned name. Compare atoms of the same document, or a document's
/// atom with a well-known one from [`atoms`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Atom(u32);

impl Atom {
    /// The atom's index in [`STATIC`], if it is a well-known name.
    pub fn static_index(self) -> Option<usize> {
        (self.0 < STATIC_COUNT).then_some(self.0 as usize)
    }
}

macro_rules! static_atoms {
    ($($konst:ident = $text:literal,)*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms, dead_code)]
        #[repr(u32)]
        enum Index { $($konst,)* }

        /// Every well-known name, at its atom's index.
        pub const STATIC: &[&str] = &[$($text,)*];

        /// The well-known names, as atoms equal in every document.
        pub mod atoms {
            use super::{Atom, Index};
            $(pub const $konst: Atom = Atom(Index::$konst as u32);)*
        }
    };
}

// The HTML elements, the SVG elements and the attributes the real-site
// corpus (M32) uses most, ranked by count across its 56 pages (2026-10-21).
// Lowercase: the tokenizer lowercases names. Adding a name changes no
// behaviour, only which names cost an allocation.
static_atoms! {
    // Attributes.
    ID = "id", CLASS = "class", STYLE = "style", TYPE = "type", VALUE = "value", NAME = "name",
    HREF = "href", SRC = "src", ALT = "alt", TITLE = "title", REL = "rel", WIDTH = "width",
    HEIGHT = "height", CHECKED = "checked", DISABLED = "disabled", REQUIRED = "required",
    PATTERN = "pattern", PLACEHOLDER = "placeholder", FOR = "for", ACTION = "action",
    METHOD = "method", TARGET = "target", LANG = "lang", DIR = "dir", HIDDEN = "hidden",
    TABINDEX = "tabindex", ROLE = "role", ARIA_LABEL = "aria-label",
    ARIA_LABELLEDBY = "aria-labelledby", ARIA_DESCRIBEDBY = "aria-describedby",
    ARIA_HIDDEN = "aria-hidden", ARIA_HASPOPUP = "aria-haspopup", ARIA_EXPANDED = "aria-expanded",
    ARIA_CONTROLS = "aria-controls", ARIA_CURRENT = "aria-current",
    CONTENTEDITABLE = "contenteditable", COLSPAN = "colspan", ROWSPAN = "rowspan",
    CELLPADDING = "cellpadding", CELLSPACING = "cellspacing", BORDER = "border",
    BGCOLOR = "bgcolor", ALIGN = "align", VALIGN = "valign", CONTENT = "content",
    CHARSET = "charset", SRCSET = "srcset", SIZES = "sizes", LOADING = "loading",
    DECODING = "decoding", SELECTED = "selected", MULTIPLE = "multiple", READONLY = "readonly",
    AUTOFOCUS = "autofocus", MAXLENGTH = "maxlength", MINLENGTH = "minlength", MIN = "min",
    MAX = "max", STEP = "step", HREFLANG = "hreflang", ABOUT = "about", TYPEOF = "typeof",
    PROPERTY = "property", RESOURCE = "resource", SCOPE = "scope", ACCESSKEY = "accesskey",
    ITEMPROP = "itemprop", ITEMSCOPE = "itemscope", ITEMTYPE = "itemtype", MEDIA = "media",
    XMLNS = "xmlns", VIEWBOX = "viewbox", D = "d", FILL = "fill", STROKE = "stroke",
    ASYNC = "async", DEFER = "defer", CROSSORIGIN = "crossorigin", INTEGRITY = "integrity",
    HTTP_EQUIV = "http-equiv", AUTOCOMPLETE = "autocomplete", ENCTYPE = "enctype",
    START = "start", REVERSED = "reversed", OPEN = "open", DATETIME = "datetime",
    DOWNLOAD = "download", REFERRERPOLICY = "referrerpolicy", NOVALIDATE = "novalidate",
    SPELLCHECK = "spellcheck", TRANSLATE = "translate", DRAGGABLE = "draggable",
    ONCLICK = "onclick", ONLOAD = "onload", POSTER = "poster", CONTROLS = "controls",
    AUTOPLAY = "autoplay", LOOP = "loop", MUTED = "muted", PRELOAD = "preload",
    // Elements.
    HTML = "html", HEAD = "head", BODY = "body", META = "meta", LINK = "link", BASE = "base",
    SCRIPT = "script", NOSCRIPT = "noscript", TEMPLATE = "template", SLOT = "slot", DIV = "div",
    SPAN = "span", P = "p", A = "a", IMG = "img", UL = "ul", OL = "ol", LI = "li", H1 = "h1",
    H2 = "h2", H3 = "h3", H4 = "h4", H5 = "h5", H6 = "h6", TABLE = "table", THEAD = "thead",
    TBODY = "tbody", TFOOT = "tfoot", TR = "tr", TD = "td", TH = "th", CAPTION = "caption",
    COLGROUP = "colgroup", COL = "col", FORM = "form", INPUT = "input", BUTTON = "button",
    TEXTAREA = "textarea", SELECT = "select", OPTION = "option", OPTGROUP = "optgroup",
    LABEL = "label", FIELDSET = "fieldset", LEGEND = "legend", DATALIST = "datalist",
    OUTPUT = "output", PROGRESS = "progress", METER = "meter", SECTION = "section",
    ARTICLE = "article", NAV = "nav", HEADER = "header", FOOTER = "footer", MAIN = "main",
    ASIDE = "aside", ADDRESS = "address", SEARCH = "search", HGROUP = "hgroup",
    FIGURE = "figure", FIGCAPTION = "figcaption", PRE = "pre", CODE = "code", EM = "em",
    STRONG = "strong", I = "i", B = "b", U = "u", S = "s", SMALL = "small", BIG = "big",
    SUB = "sub", SUP = "sup", BR = "br", WBR = "wbr", HR = "hr", BLOCKQUOTE = "blockquote",
    CENTER = "center", FONT = "font", DL = "dl", DT = "dt", DD = "dd", IFRAME = "iframe",
    EMBED = "embed", OBJECT = "object", PARAM = "param", CANVAS = "canvas", VIDEO = "video",
    AUDIO = "audio", SOURCE = "source", TRACK = "track", PICTURE = "picture", MAP = "map",
    AREA = "area", DETAILS = "details", SUMMARY = "summary", DIALOG = "dialog", MENU = "menu",
    ABBR = "abbr", CITE = "cite", Q = "q", DFN = "dfn", BDI = "bdi", BDO = "bdo", TIME = "time",
    DATA = "data", MARK = "mark", DEL = "del", INS = "ins", KBD = "kbd", VAR = "var",
    SAMP = "samp", RUBY = "ruby", RT = "rt", RP = "rp",
    // SVG elements.
    SVG = "svg", G = "g", PATH = "path", RECT = "rect", CIRCLE = "circle", ELLIPSE = "ellipse",
    LINE = "line", POLYLINE = "polyline", POLYGON = "polygon", USE = "use", DEFS = "defs",
    SYMBOL = "symbol", STOP = "stop", MASK = "mask", CLIPPATH = "clippath",
    LINEARGRADIENT = "lineargradient", RADIALGRADIENT = "radialgradient", TEXT = "text",
    TSPAN = "tspan",
}

/// How many well-known names there are; a document's own atoms start here.
pub const STATIC_COUNT: u32 = STATIC.len() as u32;

/// The longest well-known name: longer text is never looked up.
const MAX_STATIC_LEN: usize = 24;

/// FNV-1a. A weak hash is safe here because the table it serves is fixed:
/// page content can only look names up, never add colliding ones.
#[derive(Default)]
struct StaticHasher(u64);

impl Hasher for StaticHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf2_9ce4_8422_2325 } else { self.0 };
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The well-known name `text`'s atom, if it is one. A read-only table built
/// once; its keys are fixed, so no input can make it grow or collide.
pub fn static_atom(text: &str) -> Option<Atom> {
    static TABLE: OnceLock<HashMap<&'static str, u32, BuildHasherDefault<StaticHasher>>> = OnceLock::new();
    if text.len() > MAX_STATIC_LEN {
        return None;
    }
    TABLE.get_or_init(|| STATIC.iter().enumerate().map(|(i, s)| (*s, i as u32)).collect()).get(text).map(|&i| Atom(i))
}

/// A tag or attribute name, as an element holds it.
///
/// Outside a document a `Name` is a well-known atom or its own text, and
/// reads as a string ([`Name::as_str`], `Deref<Target = str>`). Inside a
/// document's arena every name is an atom, and the document's
/// [`AtomTable`] reads it; `dom` never hands such a name out.
#[derive(Clone)]
pub struct Name(Repr);

#[derive(Clone)]
enum Repr {
    /// A well-known atom anywhere; also a document's own atom, in its arena.
    Atom(Atom),
    /// Boxed twice so a `Name` is two words, not three: owned text is the
    /// rare case (a fragment's invented names), atoms the common one.
    Owned(Box<Box<str>>),
}

impl Name {
    /// The name `text`: a well-known atom, without allocating, if it is one.
    pub fn new(text: &str) -> Name {
        match static_atom(text) {
            Some(atom) => Name(Repr::Atom(atom)),
            None => Name(Repr::Owned(Box::new(text.into()))),
        }
    }

    /// The well-known name `atom`.
    pub const fn well_known(atom: Atom) -> Name {
        assert!(atom.0 < STATIC_COUNT, "not a well-known atom");
        Name(Repr::Atom(atom))
    }

    /// The name as an atom: a well-known one, or (in a document's arena) the
    /// document's own. `None` for a fragment's other names.
    #[inline]
    pub fn atom(&self) -> Option<Atom> {
        match self.0 {
            Repr::Atom(atom) => Some(atom),
            Repr::Owned(_) => None,
        }
    }

    /// Whether this is the well-known name `atom`: one integer comparison.
    #[inline]
    pub fn is(&self, atom: Atom) -> bool {
        matches!(self.0, Repr::Atom(a) if a == atom)
    }

    /// The name's text. Not for names in a document's arena (they read
    /// through the document's table, [`AtomTable::resolve_name`]).
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Atom(atom) => match atom.static_index() {
                Some(i) => STATIC[i],
                None => {
                    debug_assert!(false, "a document's own atom read without its table");
                    ""
                }
            },
            Repr::Owned(text) => text,
        }
    }
}

impl std::ops::Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for Name {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for Name {
    fn from(text: &str) -> Name {
        Name::new(text)
    }
}

impl From<&String> for Name {
    fn from(text: &String) -> Name {
        Name::new(text)
    }
}

impl From<String> for Name {
    fn from(text: String) -> Name {
        match static_atom(&text) {
            Some(atom) => Name(Repr::Atom(atom)),
            None => Name(Repr::Owned(Box::new(text.into_boxed_str()))),
        }
    }
}

impl From<&Name> for Name {
    fn from(name: &Name) -> Name {
        name.clone()
    }
}

impl From<Name> for String {
    fn from(name: Name) -> String {
        name.as_str().to_string()
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Name) -> bool {
        match (&self.0, &other.0) {
            (Repr::Atom(a), Repr::Atom(b)) => a == b,
            // A well-known name is never `Owned` (`Name::new` interns it),
            // so an atom and owned text always differ.
            (Repr::Atom(_), Repr::Owned(_)) | (Repr::Owned(_), Repr::Atom(_)) => false,
            (Repr::Owned(a), Repr::Owned(b)) => a == b,
        }
    }
}

impl Eq for Name {}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<Name> for str {
    fn eq(&self, other: &Name) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<Name> for &str {
    fn eq(&self, other: &Name) -> bool {
        *self == other.as_str()
    }
}

/// Hashes as its text, so a `HashMap<Name, _>` can be read with a `&str`.
impl Hash for Name {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl std::fmt::Debug for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A document's own names — the ones not in [`STATIC`]. Keys come from the
/// page, so the map keeps the standard, collision-resistant hasher. It only
/// grows, for the document's life: a page that invents a name and drops it
/// still holds its entry until the document goes.
#[derive(Debug, Default, Clone)]
pub struct AtomTable {
    names: Vec<Arc<str>>,
    lookup: HashMap<Arc<str>, u32>,
}

impl AtomTable {
    /// `text`'s atom, interning it if new.
    pub fn intern(&mut self, text: &str) -> Atom {
        if let Some(atom) = static_atom(text) {
            return atom;
        }
        if let Some(&i) = self.lookup.get(text) {
            return Atom(STATIC_COUNT + i);
        }
        self.add(Arc::from(text))
    }

    fn add(&mut self, text: Arc<str>) -> Atom {
        let i = u32::try_from(self.names.len()).ok().filter(|i| i.checked_add(STATIC_COUNT).is_some()).expect("atom table exhausted");
        self.names.push(Arc::clone(&text));
        self.lookup.insert(text, i);
        Atom(STATIC_COUNT + i)
    }

    /// Makes `name` an atom of this table, in place (a well-known name, or
    /// one already interned, is left as it is).
    #[inline]
    pub fn intern_name(&mut self, name: &mut Name) {
        if let Repr::Owned(text) = &name.0 {
            let atom = match self.lookup.get(&***text) {
                Some(&i) => Atom(STATIC_COUNT + i),
                None => self.add(Arc::from(&***text)),
            };
            name.0 = Repr::Atom(atom);
        }
    }

    /// The name `text` as this table names it: its atom (interned if new).
    pub fn name(&mut self, text: &str) -> Name {
        Name(Repr::Atom(self.intern(text)))
    }

    /// `text`'s atom, if it is well-known or this document has seen it.
    pub fn find(&self, text: &str) -> Option<Atom> {
        static_atom(text).or_else(|| self.lookup.get(text).map(|&i| Atom(STATIC_COUNT + i)))
    }

    /// The text of `atom` (a well-known atom or one of this table's).
    #[inline]
    pub fn resolve(&self, atom: Atom) -> &str {
        match atom.0.checked_sub(STATIC_COUNT) {
            None => STATIC[atom.0 as usize],
            Some(i) => &self.names[i as usize],
        }
    }

    /// The text of a name held in this table's document.
    #[inline]
    pub fn resolve_name<'a>(&'a self, name: &'a Name) -> &'a str {
        match &name.0 {
            Repr::Atom(atom) => self.resolve(*atom),
            Repr::Owned(text) => text,
        }
    }

    /// A copy of a name held in this table's document, for a fragment taken
    /// out of it (a fragment has no table, so the name carries its text).
    pub fn detach(&self, name: &Name) -> Name {
        match &name.0 {
            Repr::Atom(atom) if atom.0 >= STATIC_COUNT => Name(Repr::Owned(Box::new(self.resolve(*atom).into()))),
            _ => name.clone(),
        }
    }

    /// How many names the page invented.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_names_have_fixed_atoms_and_others_are_per_table() {
        assert_eq!(static_atom("class"), Some(atoms::CLASS));
        assert_eq!(STATIC[atoms::DIV.0 as usize], "div");
        assert!(static_atom("data-x").is_none());
        let (mut a, mut b) = (AtomTable::default(), AtomTable::default());
        assert_eq!(a.intern("class"), atoms::CLASS, "well-known: the same everywhere");
        let x = a.intern("data-x");
        assert_eq!(a.intern("data-x"), x, "interned once");
        assert_eq!(a.resolve(x), "data-x");
        assert_eq!(a.find("data-y"), None);
        b.intern("other");
        assert_eq!((a.len(), b.len()), (1, 1));
    }

    #[test]
    fn every_static_name_is_unique_lowercase_and_short() {
        let mut seen = std::collections::HashSet::new();
        for name in STATIC {
            assert!(seen.insert(*name), "{name} listed twice");
            assert_eq!(*name, name.to_ascii_lowercase());
            assert!(name.len() <= MAX_STATIC_LEN, "{name} is longer than MAX_STATIC_LEN");
            assert_eq!(static_atom(name).map(|a| STATIC[a.0 as usize]), Some(*name));
        }
    }

    #[test]
    fn a_well_known_name_allocates_nothing_and_compares_as_an_integer() {
        let div = Name::new("div");
        assert_eq!(div.atom(), Some(atoms::DIV));
        assert!(div.is(atoms::DIV));
        assert_eq!(Name::from(String::from("div")), div);
        assert_eq!(div, "div");
        assert_eq!(&*div, "div");
        let custom = Name::new("my-widget");
        assert_eq!(custom.atom(), None);
        assert_eq!(custom, Name::from("my-widget"));
        assert_ne!(custom, div);
        assert_eq!(format!("{custom} {div:?}"), "my-widget \"div\"");
    }

    #[test]
    fn a_name_is_two_words() {
        assert_eq!(std::mem::size_of::<Name>(), 16);
        assert_eq!(std::mem::size_of::<Option<Name>>(), 16);
    }

    #[test]
    fn interning_in_place_and_detaching_round_trip() {
        let mut table = AtomTable::default();
        let mut names = [Name::new("id"), Name::new("data-a"), Name::new("data-a")];
        for n in &mut names {
            table.intern_name(n);
        }
        assert!(names.iter().all(|n| n.atom().is_some()));
        assert_eq!(names[1].atom(), names[2].atom());
        assert_eq!(table.len(), 1);
        assert_eq!(table.resolve_name(&names[1]), "data-a");
        let detached = table.detach(&names[1]);
        assert_eq!(detached.atom(), None);
        assert_eq!(detached, "data-a");
        assert!(table.detach(&names[0]).is(atoms::ID));
    }

    #[test]
    fn hashing_matches_str_so_maps_can_be_read_by_text() {
        let mut map = HashMap::new();
        map.insert(Name::new("div"), 1);
        map.insert(Name::new("x-y"), 2);
        assert_eq!(map.get("div"), Some(&1));
        assert_eq!(map.get("x-y"), Some(&2));
    }
}
