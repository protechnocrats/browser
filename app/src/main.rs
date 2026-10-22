//! The desktop browser app: opens a window (`shell_desktop`) and drives its
//! tabs through Browser Core (`browser_core::Session`, Milestone 36) over the
//! `engine` crate — commands in, events out (ADR 005). Each page runs in a
//! sandboxed renderer process — this same executable, started with
//! `--renderer` (ADR 016) — and the GPU on a thread (ADR 006); this, the UI
//! thread, only forwards input and hands frames along. Every page is granted
//! the network and its own origin's storage by the profile (ADR 010).
//! Everything that parses, styles, lays out, runs script or hit-tests lives
//! behind that API; this file is flags, wiring, shortcuts and presenting
//! frames.
//!
//!     app                        # the built-in demo page
//!     app --url https://…/       # fetch a real page (in the background)
//!     app --profile <dir>        # keep settings, storage, cookies and
//!                                # permissions in <dir> (default: memory only)
//!     app --trace                # per-frame stage timings on stderr
//!     app --cookie-jar <path>    # persist cookies to <path>
//!     app --invert-scroll        # reverse the platform's scroll direction
//!     app --single-process       # pages on threads of this process (debugging)
//!
//! Privacy and network (Milestone 37; each is a setting for this run only,
//! applied through the profile's validation — `--set key=value` sets any):
//!
//!     app --private              # private mode: private route, UTC, coarse time
//!     app --proxy socks5://127.0.0.1:9050   # every request through a proxy
//!     app --vpn-only             # only while a system VPN is up (Linux)
//!     app --doh https://…/dns-query          # DNS over HTTPS (no default resolver)
//!     app --extra-roots ca.pem   # trust an extra CA, besides the bundled roots
//!     app --trust-system-store   # also trust the system's CA bundle (Linux)
//!     app --fetch-filter-list ads=https://…/list.txt   # into the profile's filters/
//!
//! Shortcuts: Ctrl+T new tab, Ctrl+W close tab, Ctrl+Tab next tab,
//! Ctrl+= / Ctrl+- zoom, Ctrl+0 the default zoom.

use browser_core::{Profile, ProfileOptions, Session};
use engine::{EngineConfig, EngineEvent, EngineInput, LoadRequest, NetworkContext};
use shell_desktop::{HostUpdate, InputEvent, ScrollSettings, ShellHost};
use std::time::Instant;

/// The app's side of the window: one session (its tabs over one engine).
struct Host {
    session: Session,
    /// `--trace`: a local, in-process collector; its report is printed per
    /// frame. `None` means no subscriber is installed at all, so every span
    /// in the engine is a no-op.
    trace: Option<diagnostics::collector::Collector>,
    frame_number: u64,
}

fn demo() -> LoadRequest {
    LoadRequest::Html { html: app::DEMO_HTML.to_string(), url: None }
}

impl Host {
    /// A browser shortcut; `false` if `key` is not one (it goes to the page).
    fn shortcut(&mut self, key: &str) -> bool {
        match key {
            "t" | "T" => {
                self.session.open_tab(demo());
            }
            "w" | "W" => {
                if let Some(page) = self.session.active_page() {
                    self.session.close_tab(page);
                }
                // The window always shows a tab.
                if self.session.active_page().is_none() {
                    self.session.open_tab(demo());
                }
            }
            "Tab" => self.session.activate_next(false),
            "=" | "+" => self.session.set_zoom(self.session.zoom() * 1.1),
            "-" => self.session.set_zoom(self.session.zoom() / 1.1),
            "0" => {
                let zoom = self.session.profile().settings().default_zoom();
                self.session.set_zoom(zoom);
            }
            _ => return false,
        }
        true
    }
}

impl ShellHost for Host {
    fn set_waker(&mut self, waker: shell_desktop::Waker) {
        // Page threads call it when a frame or event is ready (ADR 006).
        self.session.set_waker(waker);
    }

    fn resized(&mut self, width: f32, height: f32) {
        self.session.resize(width, height);
    }

    fn window_visible(&mut self, visible: bool) {
        self.session.set_window_visible(visible);
    }

    fn input(&mut self, event: InputEvent) {
        let input = match event {
            InputEvent::MouseMoved { x, y } => EngineInput::PointerMove { x, y },
            InputEvent::MouseClicked { x, y } => EngineInput::PointerDown { x, y },
            InputEvent::MouseReleased { x, y } => EngineInput::PointerUp { x, y },
            InputEvent::Scrolled { delta_x, delta_y } => EngineInput::Scroll { dx: delta_x, dy: delta_y },
            InputEvent::KeyPressed { key, ctrl: true } if self.shortcut(&key) => return,
            InputEvent::KeyPressed { key, .. } => EngineInput::Key { key },
        };
        self.session.input(input);
    }

    fn update(&mut self, now: Instant) -> HostUpdate {
        let update = self.session.update(now);
        for event in update.events {
            match event {
                EngineEvent::PageCrashed { reason, .. } => eprintln!("a page crashed: {reason}"),
                EngineEvent::NavigationFailed { url, error, .. } => eprintln!("navigation to {url} failed: {error}"),
                _ => {}
            }
        }
        if update.frame.is_some() {
            self.frame_number += 1;
            if let Some(collector) = &self.trace {
                eprintln!("{}", collector.drain().report(&format!("frame #{}", self.frame_number)));
            }
        }
        HostUpdate { scene: update.frame, wake_at: self.session.next_wake() }
    }
}

/// Command-line settings for this run (never saved): `--set key=value`, and
/// the convenience flags, each of which is one setting.
fn overrides(args: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let next = args.get(i + 1).cloned();
        match (args[i].as_str(), next) {
            ("--set", Some(kv)) => {
                match kv.split_once('=') {
                    Some((k, v)) => out.push((k.trim().to_string(), v.trim().to_string())),
                    None => eprintln!("--set {kv}: expected key=value"),
                }
                i += 1;
            }
            ("--proxy", Some(url)) => {
                out.push(("network.proxy".into(), url));
                i += 1;
            }
            ("--doh", Some(url)) => {
                out.push(("dns.doh".into(), url));
                i += 1;
            }
            ("--extra-roots", Some(path)) => {
                out.push(("tls.extra_roots".into(), path));
                i += 1;
            }
            ("--private", _) => out.push(("privacy.mode".into(), "true".into())),
            ("--vpn-only", _) => out.push(("network.vpn_only".into(), "true".into())),
            ("--trust-system-store", _) => match system_ca_bundle() {
                Some(path) => out.push(("tls.extra_roots".into(), path)),
                None => eprintln!("--trust-system-store: no system CA bundle found"),
            },
            _ => {}
        }
        i += 1;
    }
    out
}

/// The system's CA bundle, where Linux distributions keep it (ADR 018 §3).
fn system_ca_bundle() -> Option<String> {
    ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/ca-bundle.pem", "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .map(str::to_string)
}

/// Whether a VPN interface (`tun*`, `tap*`, `wg*`, `ppp*`) is up — the
/// `system-vpn` route's probe (ADR 011 §4). Linux reads `/sys/class/net`;
/// elsewhere there is no probe yet, so the route says the VPN is down.
fn vpn_probe() -> browser_core::VpnProbe {
    std::sync::Arc::new(|| {
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else { return false };
        entries.filter_map(Result::ok).any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let vpn = ["tun", "tap", "wg", "ppp"].iter().any(|p| name.starts_with(p));
            // IFF_UP is bit 0 of the interface flags.
            let up = std::fs::read_to_string(e.path().join("flags"))
                .ok()
                .and_then(|f| u32::from_str_radix(f.trim().trim_start_matches("0x"), 16).ok())
                .is_some_and(|flags| flags & 1 == 1);
            vpn && up
        })
    })
}

/// `--fetch-filter-list CATEGORY=URL`: downloads a list, when the user asks,
/// into the profile's `filters/` directory as `CATEGORY-<name>.txt`, and
/// loads it (ADR 019 §1). Needs `--profile`.
fn fetch_filter_list(profile: &Profile, spec: &str) {
    let Some((category, url)) = spec.split_once('=') else {
        eprintln!("--fetch-filter-list: expected ads=URL, trackers=URL or ai=URL");
        return;
    };
    if !matches!(category, "ads" | "trackers" | "ai") {
        eprintln!("--fetch-filter-list: the category is ads, trackers or ai");
        return;
    }
    let Some(dir) = profile.dir() else {
        eprintln!("--fetch-filter-list needs --profile (lists are kept in the profile)");
        return;
    };
    let response = match profile.network().fetch_with(url, &engine::RequestContext::typed_navigation()) {
        Ok(r) if r.status == 200 => r,
        Ok(r) => return eprintln!("--fetch-filter-list: {url} answered {}", r.status),
        Err(e) => return eprintln!("--fetch-filter-list: {e}"),
    };
    let name: String = url.rsplit('/').next().unwrap_or("list").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    let dir = dir.join(browser_core::FILTERS_DIR);
    let path = dir.join(format!("{category}-{}.txt", if name.is_empty() { "list".to_string() } else { name }));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &response.body)) {
        return eprintln!("--fetch-filter-list: could not save {}: {e}", path.display());
    }
    for issue in profile.reload_filters() {
        eprintln!("filters: {issue}");
    }
    eprintln!("filter list saved to {} ({} rules in force)", path.display(), profile.network().store().filter().len());
}

fn main() {
    // ADR 016: started by ourselves as a page's renderer — nothing else of
    // the browser runs in that process.
    if renderer::is_renderer_process() {
        renderer::renderer_main();
    }
    let args: Vec<String> = std::env::args().collect();
    let value_of = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();

    // Milestone 27: `--trace` installs a collector BEFORE anything runs, so
    // the startup load is traced too. Local only — stderr, nowhere else.
    let trace = if args.iter().any(|a| a == "--trace") {
        let collector = diagnostics::collector::Collector::new();
        if collector.install_global().is_err() {
            eprintln!("--trace: a tracing subscriber is already installed; continuing without it");
            None
        } else {
            diagnostics::metrics::set_detailed(true);
            Some(collector)
        }
    } else {
        None
    };

    // Milestone 36: the profile — in a directory with `--profile`, else in
    // memory. It holds the network context (Milestone 26: one cookie jar and
    // HTTP cache), storage, settings and permissions, and grants each page
    // its capabilities.
    let options = ProfileOptions { vpn_probe: Some(vpn_probe()), overrides: overrides(&args) };
    let profile = match value_of("--profile") {
        Some(dir) => match Profile::open_with(&dir, options.clone()) {
            Ok(profile) => {
                for issue in profile.issues() {
                    eprintln!("profile {dir}: {issue}");
                }
                profile
            }
            Err(e) => {
                eprintln!("could not open the profile in {dir}: {e} (continuing with a profile in memory)");
                Profile::in_memory_with(options)
            }
        },
        None => {
            let profile = Profile::in_memory_with(options);
            for issue in profile.issues() {
                eprintln!("settings: {issue}");
            }
            profile
        }
    };
    if let Some(spec) = value_of("--fetch-filter-list") {
        fetch_filter_list(&profile, &spec);
    }
    // `--cookie-jar` opts in to persisting cookies at a path of its own.
    if let Some(path) = value_of("--cookie-jar") {
        if let Err(e) = profile.network().load_cookies(&path) {
            eprintln!("could not load cookies from {path}: {e} (continuing without persistence)");
        }
    }

    // The waker arrives from the shell (`set_waker`) once the event loop exists.
    // ADR 016: every page in a sandboxed renderer process of its own (this
    // executable, started with `--renderer`); `--single-process` keeps
    // pages on threads of this process (debugging).
    let isolation = if args.iter().any(|a| a == "--single-process") {
        engine::Isolation::Threads
    } else {
        match renderer::ProcessLauncher::current_exe() {
            Ok(launcher) => engine::Isolation::Processes(std::sync::Arc::new(launcher)),
            Err(e) => {
                eprintln!("cannot find this program to start renderers ({e}); pages run in this process");
                engine::Isolation::Threads
            }
        }
    };
    // Milestone 35: the system's fonts, granted to the engine.
    let fonts = Some(renderer::system_fonts());
    // The session replaces the config's network with the profile's.
    let config = EngineConfig { network: NetworkContext::new(), waker: None, image_loading: engine::ImageLoading::Progressive, isolation, fonts };
    let settings = profile.settings().clone();
    let mut session = Session::new(profile, config);
    session.resize(800.0, 700.0);
    session.open_tab(match value_of("--url") {
        Some(url) => LoadRequest::Url(url),
        None => demo(),
    });

    let scroll = ScrollSettings {
        invert: settings.scroll_invert() || args.iter().any(|a| a == "--invert-scroll"),
        speed: settings.scroll_speed(),
        ..ScrollSettings::default()
    };
    let host = Host { session, trace, frame_number: 0 };
    if let Err(e) = shell_desktop::run("browser-engine", 800, 700, scroll, host) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
