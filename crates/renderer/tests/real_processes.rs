//! ADR 016 with real processes: the `browser-renderer` binary, sandboxed.

use engine::{Engine, EngineConfig, EngineEvent, ImageLoading, Isolation, LoadRequest, NetworkContext, PageCommand, PageId, PageParams, Viewport};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(20);
const RENDERER: &str = env!("CARGO_BIN_EXE_browser-renderer");

fn engine() -> Engine {
    Engine::new(EngineConfig {
        network: NetworkContext::new(),
        waker: None,
        image_loading: ImageLoading::BeforeFirstFrame,
        isolation: Isolation::Processes(Arc::new(renderer::ProcessLauncher::new(RENDERER))),
        fonts: Some(renderer::system_fonts()),
    })
}

fn run_until(engine: &mut Engine, now: &mut Instant, mut done: impl FnMut(&[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let deadline = Instant::now() + WAIT;
    let mut all = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "timed out; events so far: {all:?}");
        *now += Duration::from_millis(16);
        all.extend(engine.update_blocking(*now, WAIT));
        if done(&all) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn drew(events: &[EngineEvent], page: PageId) -> bool {
    events.iter().any(|e| matches!(e, EngineEvent::Frame { page: p, .. } if *p == page))
}

fn load(engine: &mut Engine, page: PageId, text: &str) {
    let html = format!("<html><body><p>{text}</p><script>console.log('scripts run; logs go to stderr')</script></body></html>");
    engine.send(page, PageCommand::Load(LoadRequest::Html { html, url: Some("https://example.test/".into()) }));
}

#[test]
fn a_page_renders_in_its_own_sandboxed_process() {
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let mut now = Instant::now();
    load(&mut engine, page, "hello from a renderer process");
    run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) && drew(e, page));
    let pid = engine.renderer_pid(page).expect("a real process");
    assert_ne!(pid, std::process::id());
    let (sandboxed, detail) = engine.renderer_sandbox(page).expect("the renderer reported");
    if cfg!(target_os = "linux") {
        assert!(sandboxed, "the renderer runs sandboxed on Linux: {detail}");
    }
}

#[test]
fn a_killed_renderer_is_that_pages_crash_and_nothing_else() {
    let mut engine = engine();
    let victim = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let other = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let mut now = Instant::now();
    run_until(&mut engine, &mut now, |e| drew(e, victim) && drew(e, other));
    let pid = engine.renderer_pid(victim).unwrap();
    // As if its memory had been corrupted: the process dies with no warning.
    let killed = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().expect("kill runs");
    assert!(killed.success());
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })));
    let reason = events.iter().find_map(|e| match e {
        EngineEvent::PageCrashed { page, reason } if *page == victim => Some(reason.clone()),
        _ => None,
    });
    assert!(reason.as_deref().is_some_and(|r| r.contains("signal")), "reported with how it ended: {reason:?}");
    load(&mut engine, other, "still here");
    run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { page, .. } if *page == other)));
}

#[test]
fn a_runaway_script_has_its_renderer_ended_by_the_watchdog() {
    // Milestone 38a (ADR 013), with a real process: the watchdog ends it.
    let mut engine = engine();
    let bound = Duration::from_secs(1);
    engine.set_unresponsive_after(bound);
    let runaway = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let other = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let mut now = Instant::now();
    run_until(&mut engine, &mut now, |e| drew(e, runaway) && drew(e, other));
    let pid = engine.renderer_pid(runaway).unwrap();
    // Nested loops: Boa's loop limit counts each loop instance, so this
    // runs for days, where a bare `while (true) {}` stops at the limit.
    let html = "<html><body><script>for (;;) { for (var i = 0; i < 9000000; i++) {} }</script></body></html>".to_string();
    let started = Instant::now();
    engine.send(runaway, PageCommand::Load(LoadRequest::Html { html, url: Some("https://example.test/".into()) }));
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::PageCrashed { .. })));
    let reason = events.iter().find_map(|e| match e {
        EngineEvent::PageCrashed { page, reason } if *page == runaway => Some(reason.clone()),
        _ => None,
    });
    assert!(reason.as_deref().is_some_and(|r| r.contains("stopped responding")), "{reason:?}");
    assert!(started.elapsed() < bound * 4, "ended in {:?}", started.elapsed());
    wait_gone(pid);
    load(&mut engine, other, "still here");
    run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { page, .. } if *page == other)));
}

/// CPU time a process has used, in clock ticks (user + system), from
/// `/proc/<pid>/stat`: fields 14 and 15, counted after the `)` that ends the
/// command name.
#[cfg(target_os = "linux")]
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("the renderer is running");
    let after_name = &stat[stat.rfind(')').expect("a stat line") + 2..];
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    // After the name: state is field 3, so utime (14) and stime (15) are at
    // 11 and 12.
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn a_hidden_pages_renderer_uses_almost_no_cpu() {
    // Milestone 38b (ADR 020), measured on the real process: a page that
    // spends about 4 ms of script in every animation frame, visible and then
    // hidden.
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let html = "<html><head><style>@keyframes s { from { opacity: 0.2 } to { opacity: 1 } }\
                #a { width: 40px; height: 40px; background-color: #ff0000; animation: s 1s infinite }</style></head>\
                <body><div id=a></div><p id=n>0</p><script>var n = 0;\
                function f() { var t = Date.now(); while (Date.now() - t < 4) {} n++; document.getElementById('n').textContent = n; requestAnimationFrame(f); }\
                requestAnimationFrame(f);</script></body></html>"
        .to_string();
    engine.send(page, PageCommand::Load(LoadRequest::Html { html, url: Some("https://example.test/".into()) }));
    let mut now = Instant::now();
    run_until(&mut engine, &mut now, |e| drew(e, page));
    let pid = engine.renderer_pid(page).unwrap();

    // Drives the engine with the real clock for `duration`.
    let drive = |engine: &mut Engine, duration: Duration| {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            engine.update(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    let measure = Duration::from_secs(2);
    let before = cpu_ticks(pid);
    drive(&mut engine, measure);
    let visible = cpu_ticks(pid) - before;

    engine.send(page, PageCommand::SetVisible(false));
    drive(&mut engine, Duration::from_millis(200)); // it lands
    let before = cpu_ticks(pid);
    drive(&mut engine, measure);
    let hidden = cpu_ticks(pid) - before;

    eprintln!("renderer CPU over {measure:?}: visible {visible} ticks, hidden {hidden} ticks");
    assert!(visible >= 20, "the visible page was busy: {visible} ticks");
    assert!(hidden <= 2, "a hidden page's renderer used {hidden} ticks");
}

#[test]
fn closing_pages_and_dropping_the_engine_end_their_processes() {
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)));
    let kept = engine.create_page(PageParams::new(Viewport::new(100.0, 100.0)));
    let mut now = Instant::now();
    run_until(&mut engine, &mut now, |e| drew(e, page) && drew(e, kept));
    let (closed_pid, kept_pid) = (engine.renderer_pid(page).unwrap(), engine.renderer_pid(kept).unwrap());
    engine.close_page(page);
    wait_gone(closed_pid);
    assert!(alive(kept_pid), "closing one page leaves the other's process");
    drop(engine);
    wait_gone(kept_pid);
}

fn alive(pid: u32) -> bool {
    // A reaped process has no /proc entry (Linux); elsewhere, `kill -0`.
    if cfg!(target_os = "linux") {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    } else {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).status().is_ok_and(|s| s.success())
    }
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "renderer {pid} is still running");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_sandbox_refuses_files_sockets_and_programs() {
    let output = std::process::Command::new(RENDERER).arg("--sandbox-self-test").output().expect("runs");
    let report = String::from_utf8_lossy(&output.stdout);
    match output.status.code() {
        Some(0) => {}
        Some(77) if !cfg!(target_os = "linux") => eprintln!("no sandbox on this platform: {report}"),
        other => panic!("the sandbox let something through ({other:?}):\n{report}"),
    }
}

#[test]
fn the_renderer_binary_refuses_to_run_by_hand() {
    let status = std::process::Command::new(RENDERER).status().expect("runs");
    assert_eq!(status.code(), Some(64));
}

/// The sandboxed renderer cannot open a socket, yet the page loads — with its
/// stylesheet and image — because the host makes every request for it.
#[test]
fn a_sandboxed_renderer_loads_network_pages_through_the_host() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let png: &'static [u8] = include_bytes!("../../images/tests/fixtures/quadrants.png");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let body: &[u8] = if request.contains("/img.png") {
                png
            } else if request.contains("/s.css") {
                b"p { background-color: #ff0000; }"
            } else {
                b"<html><head><link rel=stylesheet href=/s.css></head><body><p>styled</p><img src=/img.png></body></html>"
            };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(body);
        }
    });
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let mut now = Instant::now();
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("http://127.0.0.1:{port}/"))));
    let events = run_until(&mut engine, &mut now, |e| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { .. })) && drew(e, page));
    let scene = events.iter().rev().find_map(|e| if let EngineEvent::Frame { scene, .. } = e { Some(scene.clone()) } else { None }).unwrap();
    assert_eq!(scene.images.len(), 1, "the image arrived");
    let red = style_red(&scene);
    assert!(red, "the stylesheet arrived (a red paragraph background)");
    assert!(engine.renderer_sandbox(page).is_some_and(|(on, _)| on || !cfg!(target_os = "linux")));
}

fn style_red(scene: &engine::Scene) -> bool {
    scene.items.iter().any(|i| matches!(i, rendering::DisplayItem::Rect { color, .. } if color.r == 255 && color.g == 0 && color.b == 0))
}

/// Milestone 41 (ADR 025): with real processes, a cross-site navigation
/// moves the page to another sandboxed renderer — another pid — and the one
/// it left is ended; a same-origin link keeps its process.
#[test]
fn a_cross_site_navigation_moves_the_page_to_another_sandboxed_process() {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let link = |to: String| format!("<html><body style=\"margin:0\"><a href=\"{to}\" style=\"display:block;height:100px\">go</a></body></html>");
    let (same, other) = (link(format!("http://127.0.0.1:{port}/same")), link(format!("http://localhost:{port}/other")));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let body = if request.contains("/other") { "<html><body><p>another site</p></body></html>".to_string() } else if request.contains("/same") { other.clone() } else { same.clone() };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    let mut engine = engine();
    let page = engine.create_page(PageParams::new(Viewport::new(300.0, 200.0)));
    let mut now = Instant::now();
    let committed = |url: String| move |e: &[EngineEvent]| e.iter().any(|e| matches!(e, EngineEvent::NavigationCommitted { url: Some(u), .. } if *u == url));
    engine.send(page, PageCommand::Load(LoadRequest::Url(format!("http://127.0.0.1:{port}/"))));
    run_until(&mut engine, &mut now, committed(format!("http://127.0.0.1:{port}/")));
    let first = engine.renderer_pid(page).unwrap();
    let click = |engine: &mut Engine| engine.send(page, PageCommand::Input(engine::EngineInput::PointerDown { x: 20.0, y: 20.0 }));

    click(&mut engine);
    run_until(&mut engine, &mut now, committed(format!("http://127.0.0.1:{port}/same")));
    assert_eq!(engine.renderer_pid(page), Some(first), "same origin: same process");

    click(&mut engine);
    run_until(&mut engine, &mut now, committed(format!("http://localhost:{port}/other")));
    let second = engine.renderer_pid(page).unwrap();
    assert_ne!(second, first, "another site: another process");
    wait_gone(first);
    // The new renderer reports itself sandboxed (once it has started).
    let deadline = Instant::now() + WAIT;
    while engine.renderer_sandbox(page).is_none() {
        assert!(Instant::now() < deadline, "the new renderer never reported");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(engine.renderer_sandbox(page).is_some_and(|(on, _)| on || !cfg!(target_os = "linux")), "the new process is sandboxed too");
}
