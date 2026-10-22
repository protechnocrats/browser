//! Loading a snapshot through the engine, offline, and describing what
//! happened.

use crate::routes::ReplayRoute;
use crate::snapshot::Snapshot;
use engine::{Engine, EngineConfig, EngineEvent, FrameStats, LoadRequest, NetworkContext, PageCommand, PageParams, Scene, Viewport};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one page may take (wall clock) before it counts as a timeout.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Simulated frames to run after the document commits, for scripts,
/// fetches and animations to settle. A page still animating after this is
/// captured at this point.
pub const SETTLE_FRAMES: usize = 60;

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Rendered,
    LoadFailed(String),
    /// The page's thread panicked: an engine bug, always a regression test.
    Crashed(String),
    TimedOut,
}

pub struct Outcome {
    pub name: String,
    pub category: String,
    pub status: Status,
    pub scene: Option<Scene>,
    pub stats: Option<FrameStats>,
    /// Unsupported features the page used: (kind, name, occurrences).
    pub unsupported: Vec<(&'static str, String, u64)>,
    /// Requests the snapshot has no response for.
    pub misses: Vec<String>,
    pub elapsed: Duration,
    /// Whether a second, independent load produced the same scene. `None`
    /// if the page did not render.
    pub deterministic: Option<bool>,
}

struct Load {
    status: Status,
    scene: Option<Scene>,
    stats: Option<FrameStats>,
    misses: Vec<String>,
}

fn load(snapshot: &Snapshot) -> Load {
    let replay = Arc::new(ReplayRoute::new(&snapshot.responses));
    let route: Arc<dyn network::NetworkRoute> = replay.clone();
    let network = NetworkContext::with_route(move |_store| route);
    // Milestone 36: pages get what the browser grants a web page.
    let profile = browser_core::Profile::in_memory_with_network(network.clone());
    let mut engine = Engine::new(EngineConfig { network, waker: None, image_loading: engine::ImageLoading::BeforeFirstFrame, isolation: engine::Isolation::Threads, fonts: Some(crate::test_fonts()) });
    let page = engine.create_page(PageParams::new(Viewport::new(snapshot.viewport.0, snapshot.viewport.1)).with_grants(profile.grants_for_web_page()));
    engine.send(page, PageCommand::Load(LoadRequest::Url(snapshot.url.clone())));

    let started = Instant::now();
    let mut clock = Instant::now();
    let (mut scene, mut stats) = (None, None);
    let mut committed = false;
    let mut settle = 0usize;
    let status = loop {
        let remaining = PAGE_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break Status::TimedOut;
        }
        clock += Duration::from_millis(16);
        let mut ended = None;
        for event in engine.update_blocking(clock, remaining) {
            match event {
                EngineEvent::Frame { scene: s, stats: st, .. } => {
                    scene = Some(s);
                    stats = Some(st);
                }
                EngineEvent::NavigationCommitted { .. } => committed = true,
                EngineEvent::NavigationFailed { error, .. } => ended = Some(Status::LoadFailed(error)),
                EngineEvent::PageCrashed { reason, .. } => ended = Some(Status::Crashed(reason)),
                EngineEvent::NavigationStarted { .. } => {}
            }
        }
        if let Some(status) = ended {
            break status;
        }
        if committed {
            settle += 1;
            if engine.next_wake().is_none() || settle >= SETTLE_FRAMES {
                break Status::Rendered;
            }
        } else {
            // The navigation runs on its own thread: give it a moment.
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    Load { status, scene, stats, misses: replay.misses() }
}

/// Loads `snapshot` (twice, to check determinism) and reports the first
/// load, with the unsupported features it used.
pub fn run(snapshot: &Snapshot) -> Outcome {
    diagnostics::unsupported::set_enabled(true);
    diagnostics::unsupported::reset();
    let started = Instant::now();
    let first = load(snapshot);
    let elapsed = started.elapsed();
    let unsupported = diagnostics::unsupported::snapshot();
    diagnostics::unsupported::set_enabled(false);

    let deterministic = match (&first.status, &first.scene) {
        (Status::Rendered, Some(scene)) => {
            let second = load(snapshot);
            Some(second.scene.as_ref().map(engine::scene_to_json) == Some(engine::scene_to_json(scene)))
        }
        _ => None,
    };
    Outcome {
        name: snapshot.name.clone(),
        category: snapshot.category.clone(),
        status: first.status,
        scene: first.scene,
        stats: first.stats,
        unsupported,
        misses: first.misses,
        elapsed,
        deterministic,
    }
}
