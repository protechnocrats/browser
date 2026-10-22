//! The desktop app: a host for the engine (Milestone 29). It owns what a
//! host owns — the window (via `shell_desktop`), command-line flags, the
//! demo page, and presenting frames — and drives the pipeline only through
//! the `engine` crate's commands and events (ADR 005).

/// The built-in demo page shown when no `--url` is given.
pub const DEMO_HTML: &str = include_str!("demo.html");

#[cfg(test)]
mod tests {
    use super::*;
    use engine::{Engine, EngineConfig, EngineEvent, LoadRequest, PageCommand, PageParams, Viewport};

    #[test]
    fn the_demo_page_loads_and_renders_through_the_engine() {
        let mut engine = Engine::new(EngineConfig::default());
        let page = engine.create_page(PageParams::new(Viewport::new(800.0, 700.0)));
        engine.send(page, PageCommand::Load(LoadRequest::Html { html: DEMO_HTML.to_string(), url: None }));
        let events = engine.update_blocking(std::time::Instant::now(), std::time::Duration::from_secs(10));
        assert!(events.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Frame { .. })));
    }
}
