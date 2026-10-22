//! Milestone 33: the deepest document the parser builds (its nesting cap),
//! in every layout mode, is parsed, styled, laid out, painted and hit-tested
//! on HALF of `layout::MIN_STACK_BYTES` — the stack layout documents that it
//! needs — so that contract keeps 2x headroom. (A layout change that grew
//! every recursion frame once overflowed a 2 MiB test thread; the fuzz suite
//! found it.)

fn run_on_small_stack(html: String, css: &'static str) {
    std::thread::Builder::new()
        .stack_size(layout::MIN_STACK_BYTES / 2)
        .spawn(move || {
            let root = html_parser::parse_document(&html).expect("parses");
            let sheet = css_parser::parse_stylesheet(css);
            let font = text::Font::dejavu_sans();
            let laid = layout::layout_document(&root, 800.0, &font, &sheet, &mut images::ImageCache::new());
            let mut atlas = text::GlyphAtlas::new(512, 512);
            let _ = rendering::display_list::build_display_list(&laid, &font, &mut atlas);
            let _ = layout::hit_test_path(&laid, 5.0, 5.0);
        })
        .expect("spawns")
        .join()
        .expect("no stack overflow");
}

/// The fuzz suite's own stress input: 100,000 unclosed `<div>`s (the parser
/// nests the first 512).
fn deep_divs() -> String {
    "<div>".repeat(100_000)
}

#[test]
fn a_maximally_nested_block_document_fits_a_small_stack() {
    run_on_small_stack(deep_divs(), "div { margin: 1px; padding: 1px }");
    run_on_small_stack(deep_divs(), "div { margin: 1px; border: 1px solid; position: relative; overflow: hidden; opacity: 0.99 }");
}

#[test]
fn a_maximally_nested_document_with_floats_and_generated_content_fits_a_small_stack() {
    run_on_small_stack(deep_divs(), "div { float: left; box-shadow: 1px 1px red } div::before { content: 'b' }");
    run_on_small_stack(deep_divs(), "div { display: inline-block }");
    run_on_small_stack(deep_divs(), "div { display: flex; gap: 1px; flex: 1 }");
    run_on_small_stack(deep_divs(), "div { display: table-cell }");
    run_on_small_stack(deep_divs(), "div { display: grid; grid-template-columns: 1fr auto }");
    run_on_small_stack(deep_divs(), "div { display: grid; grid-template-areas: 'a b'; grid-area: b; float: left; position: relative; overflow: hidden }");
}
