//! Performance audit: frames that only scroll are drawn from the page's
//! display list (`rendering::PageDisplayList`), built once per layout —
//! not by building display items for the whole page every frame. These
//! tests pin that the result is exactly what a per-frame build gives, and
//! that a frame's work is bounded by what is near the viewport.

use rendering::{build_display_list_with_scrolls, cull_to_viewport, DisplayItem, PageDisplayList};

const WIDTH: f32 = 800.0;
const HEIGHT: f32 = 600.0;

/// Every paint feature that changes how items are placed or dropped:
/// fixed boxes (never scrolled), clipping, a scrolled container, opacity,
/// transforms, hidden boxes, shadows, positioned and z-indexed boxes, and
/// a long page of text.
fn page() -> (dom::Element, css_parser::Stylesheet) {
    let mut body = String::from(
        r#"<div id="header">fixed header <b>bold</b></div>
           <div id="clip"><p>clipped text that runs far past its box</p><p>more</p></div>
           <div id="scroller"><p>one</p><p>two</p><p>three</p><p>four</p><p>five</p></div>
           <div id="faded">faded <span>text</span></div>
           <div id="moved">transformed</div>
           <div id="hidden">hidden <span id="shown">but this child is visible</span></div>
           <div id="above">z-indexed</div>"#,
    );
    for i in 0..400 {
        body.push_str(&format!("<div class=\"row\">row {i} with <i>some</i> words and a <u>box</u></div>"));
    }
    let html = format!("<html><body>{body}</body></html>");
    let css = "#header { position: fixed; top: 0; left: 0; width: 300px; height: 30px; background: #336699; box-shadow: 2px 2px 4px #000000; }
               #clip { overflow: hidden; width: 120px; height: 20px; background: #eeeeee; }
               #scroller { overflow: auto; height: 50px; width: 200px; }
               #faded { opacity: 0.5; background: #ff0000; }
               #moved { transform: translate(30px, 5px) scale(1.5); background: #00ff00; }
               #hidden { visibility: hidden; } #shown { visibility: visible; }
               #above { position: relative; z-index: 3; top: -10px; background: #0000ff; }
               .row { height: 22px; border: 1px solid #cccccc; }";
    (html_parser::parse_document(&html).expect("parses"), css_parser::parse_stylesheet(css))
}

fn per_frame(laid: &layout::LayoutBox, font: &text::Font, scroll: f32, scrolls: &layout::ScrollOffsets) -> Vec<DisplayItem> {
    let mut atlas = text::GlyphAtlas::new(512, 128);
    let (mut items, _) = build_display_list_with_scrolls(laid, font, &mut atlas, scroll, scrolls);
    cull_to_viewport(&mut items, WIDTH, HEIGHT);
    items
}

fn same_items(a: &[DisplayItem], b: &[DisplayItem]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| format!("{x:?}") == format!("{y:?}"))
}

#[test]
fn a_frame_from_the_page_list_equals_a_per_frame_build_at_every_scroll() {
    let (root, sheet) = page();
    let font = text::Font::dejavu_sans();
    let laid = layout::layout_document(&root, WIDTH, &font, &sheet, &mut images::ImageCache::new());
    let scroller = root.find_by_id("scroller").expect("scroller").node_id;
    for scrolls in [layout::ScrollOffsets::new(), layout::ScrollOffsets::from([(scroller, (0.0, 37.0))])] {
        let mut atlas = text::GlyphAtlas::new(512, 128);
        let list = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
        assert!(!list.has_sticky() && list.valid());
        // Milestone 34: a list built before the container scrolled, whose
        // scroll layer the compositor then moved, draws the same.
        let mut moved = PageDisplayList::build(&laid, &font, &mut text::GlyphAtlas::new(512, 128), &layout::ScrollOffsets::new());
        assert_eq!(moved.set_container_scrolls(&scrolls), scrolls.len(), "one scroll layer moves per scrolled container");
        for scroll in [0.0, 1.5, 40.0, 255.9, 256.0, 1000.25, 4321.0, 8000.0, 9500.0, 20_000.0] {
            let expected = per_frame(&laid, &font, scroll, &scrolls);
            let drawn = list.visible(scroll, WIDTH, HEIGHT);
            assert!(same_items(&drawn, &expected), "scroll {scroll}: {} items from the list, {} built", drawn.len(), expected.len());
            let geometry = |items: &[DisplayItem]| -> Vec<String> {
                // Atlas coordinates depend on the atlas the list was built with.
                items.iter().map(|i| match i {
                    DisplayItem::Glyphs { instances, .. } => format!("{:?}", instances.iter().map(|g| (g.x, g.y)).collect::<Vec<_>>()),
                    other => format!("{other:?}"),
                }).collect()
            };
            assert_eq!(geometry(&moved.visible(scroll, WIDTH, HEIGHT)), geometry(&expected), "scroll {scroll}, container scrolled by the compositor");
        }
    }
}

#[test]
fn a_scroll_frame_looks_only_at_items_near_the_viewport() {
    let (root, sheet) = page();
    let font = text::Font::dejavu_sans();
    let laid = layout::layout_document(&root, WIDTH, &font, &sheet, &mut images::ImageCache::new());
    let mut atlas = text::GlyphAtlas::new(512, 128);
    let list = PageDisplayList::build(&laid, &font, &mut atlas, &layout::ScrollOffsets::new());
    let near = list.candidates(4000.0, HEIGHT);
    assert!(list.len() > 1500, "a long page: {} items", list.len());
    assert!(near * 5 < list.len(), "a frame examined {near} of {} items", list.len());
}

#[test]
fn a_sticky_box_or_an_overflowed_atlas_means_building_per_frame() {
    let root = html_parser::parse_document("<html><body><div id=s>sticky</div><p>text</p></body></html>").unwrap();
    let sheet = css_parser::parse_stylesheet("#s { position: sticky; top: 0; }");
    let font = text::Font::dejavu_sans();
    let laid = layout::layout_document(&root, WIDTH, &font, &sheet, &mut images::ImageCache::new());
    let mut atlas = text::GlyphAtlas::new(512, 128);
    assert!(PageDisplayList::build(&laid, &font, &mut atlas, &layout::ScrollOffsets::new()).has_sticky());

    // Glyphs far larger than the atlas can hold: some are skipped, so the
    // list must not be reused (the next build clears the atlas).
    let big: String = ('A'..='Z').chain('a'..='z').collect();
    let root = html_parser::parse_document(&format!("<html><body><p>{big}</p></body></html>")).unwrap();
    let sheet = css_parser::parse_stylesheet("p { font-size: 1024px; }");
    let laid = layout::layout_document(&root, WIDTH, &font, &sheet, &mut images::ImageCache::new());
    let mut atlas = text::GlyphAtlas::new(512, 128);
    let scrolls = layout::ScrollOffsets::new();
    assert!(!PageDisplayList::build(&laid, &font, &mut atlas, &scrolls).valid());
}

// ---- Milestone 39b (ADR 022): a page's list updated after a layout change ----

/// Items compared with a float tolerance: an updated list's moved-over items
/// were shifted by a delta, a fresh build's computed directly. A glyph run's
/// `atlas_generation` is ignored, as damage does (`same_pixels`): it records
/// when the run was built, and glyphs never move in a growing atlas.
fn close_items(a: &[DisplayItem], b: &[DisplayItem]) -> Result<(), String> {
    if a.len() != b.len() {
        return Err(format!("{} items vs {}", a.len(), b.len()));
    }
    let split = |item: &DisplayItem| {
        let item = match item {
            DisplayItem::Glyphs { instances, .. } => DisplayItem::Glyphs { instances: instances.clone(), atlas_generation: 0 },
            other => other.clone(),
        };
        let text = format!("{item:?}");
        let mut shape = String::new();
        let mut numbers = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find(|c: char| c.is_ascii_digit() || c == '-') {
            shape.push_str(&rest[..start]);
            let len = rest[start..].find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e')).unwrap_or(rest.len() - start);
            numbers.push(rest[start..start + len].parse::<f64>().unwrap_or(f64::NAN));
            shape.push('#');
            rest = &rest[start + len..];
        }
        shape.push_str(rest);
        (shape, numbers)
    };
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let ((sx, nx), (sy, ny)) = (split(x), split(y));
        let close = nx.len() == ny.len() && nx.iter().zip(&ny).all(|(p, q)| p == q || (p - q).abs() <= 1e-3 * (1.0 + p.abs().max(q.abs())));
        if sx != sy || !close {
            return Err(format!("item {i}: {x:?}\n  vs {y:?}"));
        }
    }
    Ok(())
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
    }
}

const ORACLE_SHEET: &str = ".p { padding: 3px; margin: 2px; background: #eeeeee; } .bd { border: 2px solid #000000; } .f { display: flex; } \
    .ib { display: inline-block; padding: 4px; background: #ccddee; } .r { position: relative; top: 3px; left: 4px; } \
    .a { position: absolute; top: 5px; right: 6px; width: 40px; background: #ff0000; } .fx { position: fixed; bottom: 0; left: 0; width: 50px; height: 10px; background: #00ff00; } \
    .o { overflow: hidden; height: 30px; } .oa { overflow: auto; max-height: 40px; } .op { opacity: 0.5; background: #123456; } \
    .t { transform: translate(5px, 3px); background: #654321; } .z { position: relative; z-index: 2; } .n { display: none; } \
    .fl { float: left; width: 60px; } .h { height: 13px; } .w { width: 120px; } .sh { box-shadow: 2px 2px 3px #000000; } \
    .tb { display: table; } .trw { display: table-row; } .tc { display: table-cell; padding: 2px; } .big { font-size: 20px; } .c { text-align: center; } \
    .blk { display: block; } .red { color: #ff0000; background: #ffeeee; }";
const ORACLE_CLASSES: &[&str] = &["", "p", "bd", "f", "ib", "r", "a", "fx", "o", "oa", "op", "t", "z", "n", "fl", "h", "w", "sh", "tb", "trw", "tc", "big", "c", "p bd", "r a", "op t", "w o"];
const ORACLE_WORDS: &[&str] = &["alpha", "a much longer run of words that will wrap somewhere", "x", "two words"];

fn random_element(rng: &mut Rng, depth: usize) -> dom::Element {
    let mut el = dom::Element::new(["div", "span", "p", "section"][rng.below(4)]);
    let class = ORACLE_CLASSES[rng.below(ORACLE_CLASSES.len())];
    if !class.is_empty() {
        el.set_attr("class", class);
    }
    for _ in 0..rng.below(if depth < 3 { 5 } else { 2 }) {
        if depth < 4 && rng.below(3) > 0 {
            el.append_child(random_element(rng, depth + 1));
        } else {
            el.children.push(dom::Node::Text(ORACLE_WORDS[rng.below(ORACLE_WORDS.len())].into()));
        }
    }
    el
}

fn element_ids(doc: &dom::Document) -> Vec<dom::NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![doc.root()];
    while let Some(el) = stack.pop() {
        out.push(el.node_id());
        stack.extend(el.element_children());
    }
    out
}

fn mutate(rng: &mut Rng, doc: &mut dom::Document) {
    let ids = element_ids(doc);
    let target = ids[rng.below(ids.len())];
    match rng.below(6) {
        0 => {
            doc.set_attribute(target, "class", ORACLE_CLASSES[rng.below(ORACLE_CLASSES.len())]);
        }
        1 => {
            doc.set_text_content(target, ORACLE_WORDS[rng.below(ORACLE_WORDS.len())]);
        }
        2 | 3 => {
            let children: Vec<dom::NodeId> = doc.find_by_node_id(target).map(|e| e.element_children().map(|c| c.node_id()).collect()).unwrap_or_default();
            let reference = (!children.is_empty()).then(|| children[rng.below(children.len())]);
            let _ = doc.insert_before(target, dom::Fragment::Element(random_element(rng, 3)), reference);
        }
        4 => {
            if let Some(parent) = doc.parent_id(target) {
                doc.remove_child(parent, target);
            }
        }
        _ => {
            let other = ids[rng.below(ids.len())];
            let _ = doc.move_before(other, target, None);
        }
    }
}

/// The merge gate for partial display lists: after every step of random
/// mutation sequences, a page list updated from the previous one (moving
/// over the items of every subtree the incremental layout reused) equals a
/// list built in full from the same layout.
#[test]
fn an_updated_page_list_equals_one_built_in_full_over_random_mutation_sequences() {
    let font = text::Font::dejavu_sans();
    let sheet = css_parser::parse_stylesheet(ORACLE_SHEET);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let scrolls = layout::ScrollOffsets::new();
    let (mut copied, mut built) = (0usize, 0usize);
    for sequence in 0..1_500 {
        let mut doc = dom::Document::new(random_element(&mut rng, 0));
        let mut styles = style::StyleEngine::new();
        let mut reuse = layout::LayoutReuse::new();
        let mut images = images::ImageCache::new();
        let mut atlas = text::GlyphAtlas::new(512, 512);
        let viewport = style::ViewportSize::new(WIDTH, HEIGHT);
        styles.restyle(&doc, &sheet, 1, viewport, &dom::Mutations::default());
        reuse.set_viewport(WIDTH, HEIGHT);
        reuse.begin_frame(None, Default::default());
        let mut laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &[]);
        let mut list = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
        for step in 0..5 {
            for _ in 0..=rng.below(2) {
                mutate(&mut rng, &mut doc);
            }
            let mutations = doc.take_mutations();
            let report = styles.restyle(&doc, &sheet, 1, viewport, &mutations);
            let touched: Vec<dom::NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
            reuse.begin_frame(Some(laid), layout::LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
            laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &touched);
            list = list.update(&laid, &font, &mut atlas, &scrolls);
            copied += list.copied_boxes;
            built += list.built_boxes;
            let fresh = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
            assert_eq!(list.has_sticky(), fresh.has_sticky());
            assert_eq!(list.images().len(), fresh.images().len(), "sequence {sequence}, step {step}: images");
            for scroll in [0.0, 300.0] {
                if let Err(e) = close_items(&list.visible(scroll, WIDTH, 100_000.0), &fresh.visible(scroll, WIDTH, 100_000.0)) {
                    panic!("sequence {sequence}, step {step}, scroll {scroll}: {e}");
                }
            }
        }
    }
    // Meaningful only if moving over really happened, often (these small
    // trees are mutated near the root, and flex items are never reused).
    assert!(copied > 20_000, "boxes moved over {copied}, built {built}");
}

/// Milestone 39b's acceptance criterion: a one-element change on a
/// 10,000-row page builds display items only for the boxes the change
/// touched; every other row's are moved over.
#[test]
fn a_one_row_change_on_a_ten_thousand_row_page_builds_only_what_changed() {
    let mut body = dom::Element::new("body");
    for i in 0..10_000 {
        let mut row = dom::Element::new("div");
        row.set_attr("class", "row");
        let mut span = dom::Element::new("span");
        span.children.push(dom::Node::Text(format!("row {i}")));
        row.append_child(span);
        row.children.push(dom::Node::Text(" text".into()));
        body.append_child(row);
    }
    let mut html = dom::Element::new("html");
    html.append_child(body);
    let mut doc = dom::Document::new(html);
    let sheet = css_parser::parse_stylesheet(".row { padding: 2px; background: #f0f0f0; } span { color: #334455; }");
    let font = text::Font::dejavu_sans();
    let viewport = style::ViewportSize::new(WIDTH, HEIGHT);
    let mut styles = style::StyleEngine::new();
    let mut reuse = layout::LayoutReuse::new();
    let mut images = images::ImageCache::new();
    let mut atlas = text::GlyphAtlas::new(512, 512);
    let scrolls = layout::ScrollOffsets::new();
    styles.restyle(&doc, &sheet, 1, viewport, &dom::Mutations::default());
    reuse.set_viewport(WIDTH, HEIGHT);
    reuse.begin_frame(None, Default::default());
    let first = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &[]);
    let list = PageDisplayList::build(&first, &font, &mut atlas, &scrolls);
    assert!(list.built_boxes > 40_000, "the first list builds the page: {}", list.built_boxes);

    let body_id = doc.root().element_children().next().unwrap().node_id();
    let row = doc.find_by_node_id(body_id).unwrap().element_children().nth(5_000).unwrap().node_id();
    doc.set_text_content(row, "a changed row, longer than before");
    let mutations = doc.take_mutations();
    let report = styles.restyle(&doc, &sheet, 1, viewport, &mutations);
    let touched: Vec<dom::NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
    reuse.begin_frame(Some(first), layout::LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
    let second = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &touched);
    let list = list.update(&second, &font, &mut atlas, &scrolls);
    // The root, the body, the changed row and its words.
    assert!(list.built_boxes <= 12, "built {} boxes", list.built_boxes);
    assert!(list.copied_boxes > 39_000, "moved over {} boxes", list.copied_boxes);
    let fresh = PageDisplayList::build(&second, &font, &mut atlas, &scrolls);
    for scroll in [0.0, 110_000.0, 220_000.0] {
        close_items(&list.visible(scroll, WIDTH, HEIGHT), &fresh.visible(scroll, WIDTH, HEIGHT)).unwrap();
    }
}

/// Milestone 39c's merge gate for display lists: when a layout was patched
/// in place, the list patched in place (or, where it cannot be, updated)
/// equals a list built in full — over random pages whose elements mostly
/// keep their size through text and class changes.
#[test]
fn a_patched_page_list_equals_one_built_in_full_over_random_mutation_sequences() {
    const BLOCK_CLASSES: &[&str] = &["", "p", "bd", "c", "big", "w", "red", "r", "o", "op", "t", "sh", "p red", "bd c"];
    let font = text::Font::dejavu_sans();
    let sheet = css_parser::parse_stylesheet(ORACLE_SHEET);
    let mut rng = Rng(0x7A3D_C1F0_9E62_B845);
    let scrolls = layout::ScrollOffsets::new();
    let (mut patched, mut frames) = (0usize, 0usize);
    for sequence in 0..1_000 {
        let mut doc = dom::Document::new(random_element(&mut rng, 0));
        for id in element_ids(&doc) {
            let class = if rng.below(10) < 8 { format!("{} blk", BLOCK_CLASSES[rng.below(BLOCK_CLASSES.len())]) } else { ORACLE_CLASSES[rng.below(ORACLE_CLASSES.len())].to_string() };
            doc.set_attribute(id, "class", format!("{class} h"));
        }
        let _ = doc.take_mutations();
        let mut styles = style::StyleEngine::new();
        let mut reuse = layout::LayoutReuse::new();
        let mut images = images::ImageCache::new();
        let mut atlas = text::GlyphAtlas::new(512, 512);
        let viewport = style::ViewportSize::new(WIDTH, HEIGHT);
        styles.restyle(&doc, &sheet, 1, viewport, &dom::Mutations::default());
        reuse.set_viewport(WIDTH, HEIGHT);
        reuse.begin_frame(None, Default::default());
        let mut laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &[]);
        let mut list = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
        for step in 0..5 {
            let ids = element_ids(&doc);
            let target = ids[rng.below(ids.len())];
            match rng.below(10) {
                0..=3 => {
                    doc.set_text_content(target, ORACLE_WORDS[rng.below(ORACLE_WORDS.len())]);
                }
                4..=7 => {
                    doc.set_attribute(target, "class", format!("{} blk h", BLOCK_CLASSES[rng.below(BLOCK_CLASSES.len())]));
                }
                _ => mutate(&mut rng, &mut doc),
            }
            let mutations = doc.take_mutations();
            let report = styles.restyle(&doc, &sheet, 1, viewport, &mutations);
            let touched: Vec<dom::NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
            reuse.begin_frame(Some(laid), layout::LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
            laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &touched);
            frames += 1;
            let path = reuse.patched().map(|(_, path)| path.to_vec());
            let mut how = "update";
            list = match path {
                Some(path) if list.patch(&laid, &path, &font, &mut atlas, &scrolls) => {
                    patched += 1;
                    how = "patch";
                    list
                }
                _ => list.update(&laid, &font, &mut atlas, &scrolls),
            };
            let fresh = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
            assert_eq!(list.images().len(), fresh.images().len(), "sequence {sequence}, step {step}: images");
            for scroll in [0.0, 300.0] {
                if let Err(e) = close_items(&list.visible(scroll, WIDTH, 100_000.0), &fresh.visible(scroll, WIDTH, 100_000.0)) {
                    panic!("sequence {sequence}, step {step} ({how}), scroll {scroll}: {e}");
                }
                // And through a viewport: the bands find the same items.
                if let Err(e) = close_items(&list.visible(scroll, WIDTH, HEIGHT), &fresh.visible(scroll, WIDTH, HEIGHT)) {
                    panic!("sequence {sequence}, step {step}, viewport at {scroll}: {e}");
                }
            }
        }
    }
    assert!(patched > 300, "lists patched in place: {patched} of {frames} frames");
}

/// Milestone 39c's acceptance criterion: a one-row change on a 10,000-row
/// page that keeps the row's size is laid out and drawn at the row: one
/// element laid out, nothing moved, only the row's boxes built for the
/// display list — every other item kept where it is.
#[test]
fn a_size_keeping_change_on_a_ten_thousand_row_page_is_patched_in_place() {
    let mut body = dom::Element::new("body");
    for i in 0..10_000 {
        let mut row = dom::Element::new("div");
        row.set_attr("class", "row");
        let mut span = dom::Element::new("span");
        span.children.push(dom::Node::Text(format!("row {i}")));
        row.append_child(span);
        row.children.push(dom::Node::Text(" text".into()));
        body.append_child(row);
    }
    let mut html = dom::Element::new("html");
    html.append_child(body);
    let mut doc = dom::Document::new(html);
    let sheet = css_parser::parse_stylesheet(".row { padding: 2px; background: #f0f0f0; } span { color: #334455; } .hot { background: #ff8800; }");
    let font = text::Font::dejavu_sans();
    let viewport = style::ViewportSize::new(WIDTH, HEIGHT);
    let mut styles = style::StyleEngine::new();
    let mut reuse = layout::LayoutReuse::new();
    let mut images = images::ImageCache::new();
    let mut atlas = text::GlyphAtlas::new(512, 512);
    let scrolls = layout::ScrollOffsets::new();
    styles.restyle(&doc, &sheet, 1, viewport, &dom::Mutations::default());
    reuse.set_viewport(WIDTH, HEIGHT);
    reuse.begin_frame(None, Default::default());
    let mut laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &[]);
    let mut list = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
    let body_id = doc.root().element_children().next().unwrap().node_id();
    let rows: Vec<dom::NodeId> = doc.find_by_node_id(body_id).unwrap().element_children().map(|r| r.node_id()).collect();
    // A text change and a class change, each keeping the row's size.
    for (row, change) in [(rows[5_000], 0), (rows[9_000], 1), (rows[10], 0)] {
        if change == 0 {
            doc.set_text_content(row, "a changed row");
        } else {
            doc.set_attribute(row, "class", "row hot");
        }
        let mutations = doc.take_mutations();
        let report = styles.restyle(&doc, &sheet, 1, viewport, &mutations);
        let touched: Vec<dom::NodeId> = mutations.records.iter().map(|r| r.target).chain(report.changed.iter().copied()).collect();
        reuse.begin_frame(Some(laid), layout::LayoutReuse::with_ancestors(&doc, touched.iter().copied()));
        laid = layout::layout_document_incremental(doc.root(), WIDTH, &font, styles.styles(), &mut images, &mut reuse, &touched);
        let (patched, path) = reuse.patched().expect("the row keeps its size: patched in place");
        assert_eq!(patched, row);
        assert_eq!(path.len(), 2, "html > body > row: the layout went down {path:?}, not across the page");
        assert_eq!((reuse.laid_out, reuse.moved_box_updates), (1, 0), "one element laid out, nothing moved");
        let path = path.to_vec();
        assert!(list.patch(&laid, &path, &font, &mut atlas, &scrolls), "the list is patched in place");
        assert!(list.built_boxes <= 5, "the row and its words: {} boxes built", list.built_boxes);
        let fresh = PageDisplayList::build(&laid, &font, &mut atlas, &scrolls);
        let (top, at_row) = (0.0, 22.0 * (rows.iter().position(|&r| r == row).unwrap() as f32 - 5.0));
        for scroll in [top, at_row, 219_000.0] {
            close_items(&list.visible(scroll, WIDTH, HEIGHT), &fresh.visible(scroll, WIDTH, HEIGHT)).unwrap();
        }
    }
}
