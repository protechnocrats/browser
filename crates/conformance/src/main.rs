//! `conformance` — record real pages into offline snapshots, and replay the
//! corpus through the engine.
//!
//!     conformance record [--only NAME]...        # live: needs the network
//!     conformance resave                         # re-apply the size budget
//!     conformance run [--bless] [--out DIR] [NAME]...
//!     conformance render FILE.html OUT.png [WIDTH HEIGHT]   # one local file, offline
//!
//! `record` reads `conformance/pages.txt` (tab-separated: name, category,
//! licence, URL, optional WxH viewport) and writes
//! `conformance/snapshots/<name>/`. `run` replays every snapshot (or the
//! named ones), writes `<out>/<name>.scene.json` and `<name>.png`
//! (default out: `target/conformance`), compares deterministic pages with
//! `conformance/golden/<name>.scene.json` (`--bless` rewrites them), prints
//! the report, and exits non-zero on a crash, a timeout, or a golden
//! mismatch.

use conformance::report::Report;
use conformance::routes::RecordingRoute;
use conformance::runner::{self, Status};
use conformance::snapshot::Snapshot;
use engine::{Engine, EngineConfig, EngineEvent, LoadRequest, NetworkContext, PageCommand, PageParams, Viewport};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct PageSpec {
    name: String,
    category: String,
    license: String,
    url: String,
    viewport: (f32, f32),
}

fn read_pages(path: &PathBuf) -> Result<Vec<PageSpec>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut pages = Vec::new();
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            return Err(format!("pages.txt: need name, category, licence, URL: {line:?}"));
        }
        let viewport = fields
            .get(4)
            .and_then(|v| v.split_once('x'))
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .unwrap_or((1024.0, 768.0));
        pages.push(PageSpec { name: fields[0].into(), category: fields[1].into(), license: fields[2].into(), url: fields[3].into(), viewport });
    }
    Ok(pages)
}

/// Loads `spec` live, through a recording route, and saves the snapshot.
fn record(spec: &PageSpec, root: &std::path::Path, today: &str) -> Result<String, String> {
    let slot: Arc<Mutex<Option<Arc<RecordingRoute>>>> = Arc::new(Mutex::new(None));
    let slot_for_route = Arc::clone(&slot);
    let network = NetworkContext::with_route(move |store| {
        let route = Arc::new(RecordingRoute::new(store));
        *slot_for_route.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&route));
        route
    });
    let route = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().ok_or("no route")?;
    // Milestone 36: pages get what the browser grants a web page.
    let profile = browser_core::Profile::in_memory_with_network(network.clone());
    let mut engine = Engine::new(EngineConfig { network, waker: None, image_loading: engine::ImageLoading::BeforeFirstFrame, isolation: engine::Isolation::Threads, fonts: Some(conformance::test_fonts()) });
    let page = engine.create_page(PageParams::new(Viewport::new(spec.viewport.0, spec.viewport.1)).with_grants(profile.grants_for_web_page()));
    engine.send(page, PageCommand::Load(LoadRequest::Url(spec.url.clone())));
    let started = Instant::now();
    let mut clock = Instant::now();
    let (mut committed, mut settle) = (false, 0);
    while started.elapsed() < Duration::from_secs(60) {
        clock += Duration::from_millis(16);
        for event in engine.update_blocking(clock, Duration::from_secs(60)) {
            match event {
                EngineEvent::NavigationCommitted { .. } => committed = true,
                EngineEvent::NavigationFailed { error, .. } => return Err(format!("{}: load failed: {error}", spec.name)),
                EngineEvent::PageCrashed { reason, .. } => return Err(format!("{}: CRASHED while recording: {reason}", spec.name)),
                _ => {}
            }
        }
        if committed {
            settle += 1;
            if engine.next_wake().is_none() || settle >= runner::SETTLE_FRAMES {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if !committed {
        return Err(format!("{}: did not commit within 60 s", spec.name));
    }
    let mut responses = route.take();
    // The document first: replay and the size budget both rely on it.
    if let Some(pos) = responses.iter().position(|r| r.requested_url == spec.url) {
        let document = responses.remove(pos);
        responses.insert(0, document);
    }
    let snapshot = Snapshot {
        name: spec.name.clone(),
        url: spec.url.clone(),
        viewport: spec.viewport,
        category: spec.category.clone(),
        license: spec.license.clone(),
        recorded: today.to_string(),
        responses,
    };
    let count = snapshot.responses.len();
    let skipped = snapshot.save(root).map_err(|e| format!("{}: save: {e}", spec.name))?;
    Ok(format!("{}: {} responses ({} left out for size), status {}", spec.name, count - skipped, skipped, snapshot.responses[0].status))
}

/// Renders a local HTML file (no network: its subresources 404) to a PNG —
/// for looking at what the engine makes of a page or a test case.
fn render(file: &str, out: &str, viewport: (f32, f32)) -> Result<(), String> {
    let html = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let network = NetworkContext::with_route(|_| Arc::new(conformance::routes::ReplayRoute::new(&[])));
    // Milestone 36: pages get what the browser grants a web page.
    let profile = browser_core::Profile::in_memory_with_network(network.clone());
    let mut engine = Engine::new(EngineConfig { network, waker: None, image_loading: engine::ImageLoading::BeforeFirstFrame, isolation: engine::Isolation::Threads, fonts: Some(conformance::test_fonts()) });
    let page = engine.create_page(PageParams::new(Viewport::new(viewport.0, viewport.1)).with_grants(profile.grants_for_web_page()));
    engine.send(page, PageCommand::Load(LoadRequest::Html { html, url: None }));
    let mut clock = Instant::now();
    let mut scene = None;
    for _ in 0..runner::SETTLE_FRAMES {
        clock += Duration::from_millis(16);
        for event in engine.update_blocking(clock, Duration::from_secs(5)) {
            match event {
                EngineEvent::Frame { scene: s, .. } => scene = Some(s),
                EngineEvent::NavigationFailed { error, .. } => return Err(error),
                EngineEvent::PageCrashed { reason, .. } => return Err(format!("crashed: {reason}")),
                _ => {}
            }
        }
        if scene.is_some() && engine.next_wake().is_none() {
            break;
        }
    }
    let scene = scene.ok_or("no frame")?;
    conformance::write_png(std::path::Path::new(out), &engine::rasterize(&scene))
}

fn today() -> String {
    // Days since the epoch → a civil date (no time-zone data needed).
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = conformance::corpus_root();
    let snapshots = root.join("snapshots");
    match args.first().map(String::as_str) {
        Some("record") => {
            let only: Vec<&String> = args.windows(2).filter(|w| w[0] == "--only").map(|w| &w[1]).collect();
            let pages = match read_pages(&root.join("pages.txt")) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            let today = today();
            let mut failed = 0;
            for spec in pages.iter().filter(|p| only.is_empty() || only.iter().any(|n| **n == p.name)) {
                match record(spec, &snapshots, &today) {
                    Ok(line) => println!("{line}"),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{e}");
                    }
                }
            }
            std::process::exit(i32::from(failed > 0));
        }
        Some("resave") => {
            // Re-applies the size budget to every snapshot already on disk.
            for name in Snapshot::list(&snapshots) {
                match Snapshot::load(&snapshots, &name).and_then(|s| s.save(&snapshots).map_err(|e| e.to_string())) {
                    Ok(skipped) => println!("{name}: {skipped} left out"),
                    Err(e) => eprintln!("{name}: {e}"),
                }
            }
        }
        Some("run") => {
            let bless = args.iter().any(|a| a == "--bless");
            let out = args.windows(2).find(|w| w[0] == "--out").map(|w| PathBuf::from(&w[1])).unwrap_or_else(|| root.join("../target/conformance"));
            let named: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--") && Some(*a) != args.windows(2).find(|w| w[0] == "--out").map(|w| &w[1])).collect();
            let _ = std::fs::create_dir_all(&out);
            let golden_dir = root.join("golden");
            let mut outcomes = Vec::new();
            let mut golden_problems = Vec::new();
            for name in Snapshot::list(&snapshots).into_iter().filter(|n| named.is_empty() || named.contains(&n)) {
                let snapshot = match Snapshot::load(&snapshots, &name) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("{e}");
                        golden_problems.push(e);
                        continue;
                    }
                };
                let outcome = runner::run(&snapshot);
                if let Some(scene) = &outcome.scene {
                    let json = engine::scene_to_json(scene);
                    let _ = std::fs::write(out.join(format!("{name}.scene.json")), &json);
                    if let Err(e) = conformance::write_png(&out.join(format!("{name}.png")), &engine::rasterize(scene)) {
                        eprintln!("{name}: png: {e}");
                    }
                    if outcome.status == Status::Rendered && outcome.deterministic == Some(true) {
                        let golden = golden_dir.join(format!("{name}.scene.json"));
                        if bless {
                            let _ = std::fs::write(&golden, &json);
                        } else {
                            match std::fs::read_to_string(&golden) {
                                Ok(expected) if expected == json => {}
                                Ok(_) => golden_problems.push(format!("{name}: scene differs from its golden (diff {} against {})", out.join(format!("{name}.scene.json")).display(), golden.display())),
                                Err(_) => golden_problems.push(format!("{name}: no golden yet (run with --bless)")),
                            }
                        }
                    }
                }
                eprintln!("{name}: {:?} in {} ms", outcome.status, outcome.elapsed.as_millis());
                outcomes.push(outcome);
            }
            let report = Report { outcomes: &outcomes };
            let text = report.render(40);
            let _ = std::fs::write(out.join("report.txt"), &text);
            println!("{text}");
            for p in &golden_problems {
                println!("golden: {p}");
            }
            let failed = !report.failures().is_empty() || (!bless && !golden_problems.is_empty());
            std::process::exit(i32::from(failed));
        }
        Some("render") if args.len() >= 3 => {
            let dimension = |i: usize, default: f32| args.get(i).and_then(|v| v.parse::<f32>().ok()).unwrap_or(default);
            if let Err(e) = render(&args[1], &args[2], (dimension(3, 1024.0), dimension(4, 768.0))) {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: conformance record [--only NAME]... | conformance run [--bless] [--out DIR] [NAME]... | conformance render FILE.html OUT.png [W H]");
            std::process::exit(2);
        }
    }
}
