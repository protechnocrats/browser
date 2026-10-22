//! A session: the pages (tabs) of one window over one engine (Milestone 36,
//! ADR 010 §8).
//!
//! The session opens every page with the grants its profile decides, sends
//! input to the active page, hands the shell the active page's newest frame
//! (and, on a switch, the last frame of the page switched to), applies the
//! zoom, and writes the profile as it runs.

use crate::permissions::{Permission, PermissionState};
use crate::profile::Profile;
use crate::settings::ZOOM_RANGE;
use capability::CapabilityKind;
use engine::{Engine, EngineConfig, EngineEvent, EngineInput, LoadRequest, PageCommand, PageId, PageParams, Scene, Viewport};
use origin::Origin;
use std::time::{Duration, Instant};

/// How often a running session writes what changed in its profile.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One tab.
struct Tab {
    page: PageId,
    /// The committed document's URL (`None`: none yet, or no address).
    url: Option<String>,
    /// The page's newest frame: shown at once when the tab is switched to.
    last_frame: Option<Scene>,
    crashed: Option<String>,
}

/// What a shell shows about a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    pub page: PageId,
    pub url: Option<String>,
    pub active: bool,
    pub crashed: bool,
}

/// What one [`Session::update`] produced.
#[derive(Debug, Default)]
pub struct SessionUpdate {
    /// The scene to present: the active page's newest, if it changed.
    pub frame: Option<Scene>,
    /// Everything the pages reported (all tabs), for the shell to show.
    pub events: Vec<EngineEvent>,
}

/// The pages of one window, over one engine and one profile.
pub struct Session {
    engine: Engine,
    profile: Profile,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// The window's drawable size, in device pixels.
    window: (f32, f32),
    zoom: f32,
    /// The active tab changed: present its last frame at the next update.
    present_cached: bool,
    last_flush: Option<Instant>,
    /// Whether anyone can see the window (M38b's gap): minimised or fully
    /// covered, even the active tab is hidden.
    window_visible: bool,
}

impl Session {
    /// A session over `profile`. `config.network` is replaced by the
    /// profile's: every page of a profile shares one cookie jar and cache.
    pub fn new(profile: Profile, mut config: EngineConfig) -> Session {
        config.network = profile.network().clone();
        let zoom = profile.settings().default_zoom();
        Session { engine: Engine::new(config), profile, tabs: Vec::new(), active: None, window: (800.0, 600.0), zoom, present_cached: false, last_flush: None, window_visible: true }
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn profile_mut(&mut self) -> &mut Profile {
        &mut self.profile
    }

    /// The engine, for diagnostics (renderer pids, queued commands).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn set_waker(&mut self, waker: engine::Waker) {
        self.engine.set_waker(waker);
    }

    /// The viewport a page sees: the window in CSS pixels at the zoom.
    fn viewport(&self) -> Viewport {
        Viewport::new(self.window.0 / self.zoom, self.window.1 / self.zoom)
    }

    /// Opens a tab, loads `load` into it, and makes it the active one. The
    /// page is granted what the profile grants a web page.
    pub fn open_tab(&mut self, load: LoadRequest) -> PageId {
        let params = PageParams::new(self.viewport()).with_grants(self.profile.grants_for_web_page()).with_fingerprint(self.profile.fingerprint());
        let page = self.engine.create_page(params);
        self.engine.send(page, PageCommand::Load(load));
        self.tabs.push(Tab { page, url: None, last_frame: None, crashed: None });
        self.set_active(self.tabs.len() - 1);
        page
    }

    fn set_active(&mut self, index: usize) {
        if self.active != Some(index) {
            // Milestone 38b (ADR 020): only the active tab is visible; the
            // others do no animation work and build no frames.
            if let Some(old) = self.active.and_then(|a| self.tabs.get(a)) {
                self.engine.send(old.page, PageCommand::SetVisible(false));
            }
            self.active = Some(index);
            self.engine.send(self.tabs[index].page, PageCommand::SetVisible(self.window_visible));
            self.present_cached = true;
        }
    }

    /// The window was covered or minimised (`false`), or can be seen again
    /// (M38b's gap; ADR 020): the active tab is hidden with it — no
    /// animation work, no frames — and shown again, drawing at once.
    pub fn set_window_visible(&mut self, visible: bool) {
        if visible == self.window_visible {
            return;
        }
        self.window_visible = visible;
        if let Some(tab) = self.active.and_then(|a| self.tabs.get(a)) {
            self.engine.send(tab.page, PageCommand::SetVisible(visible));
        }
        if visible {
            self.present_cached = true;
        }
    }

    pub fn window_visible(&self) -> bool {
        self.window_visible
    }

    fn index_of(&self, page: PageId) -> Option<usize> {
        self.tabs.iter().position(|t| t.page == page)
    }

    /// Closes `page`'s tab (its renderer ends). The tab after it becomes
    /// active, or the one before when it was the last.
    pub fn close_tab(&mut self, page: PageId) -> bool {
        let Some(index) = self.index_of(page) else { return false };
        self.engine.close_page(page);
        self.tabs.remove(index);
        self.active = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(active) if active > index => Some(active - 1),
            Some(active) if active == index => {
                self.present_cached = true;
                let next = index.min(self.tabs.len() - 1);
                self.engine.send(self.tabs[next].page, PageCommand::SetVisible(self.window_visible));
                Some(next)
            }
            other => other,
        };
        true
    }

    /// Makes `page`'s tab the active one.
    pub fn activate(&mut self, page: PageId) -> bool {
        match self.index_of(page) {
            Some(index) => {
                self.set_active(index);
                true
            }
            None => false,
        }
    }

    /// Activates the next tab (wrapping); `back` for the previous one.
    pub fn activate_next(&mut self, back: bool) {
        let count = self.tabs.len();
        if let Some(active) = self.active {
            let next = if back { (active + count - 1) % count } else { (active + 1) % count };
            self.set_active(next);
        }
    }

    pub fn active_page(&self) -> Option<PageId> {
        self.active.map(|i| self.tabs[i].page)
    }

    pub fn tabs(&self) -> Vec<TabInfo> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabInfo { page: t.page, url: t.url.clone(), active: self.active == Some(i), crashed: t.crashed.is_some() })
            .collect()
    }

    /// The window is now `width` x `height` device pixels: every tab gets
    /// the new viewport, so a background tab is the right size when shown.
    pub fn resize(&mut self, width: f32, height: f32) {
        if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
            self.window = (width, height);
        }
        let viewport = self.viewport();
        for tab in &self.tabs {
            self.engine.send(tab.page, PageCommand::Resize(viewport));
        }
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// Sets the zoom (clamped to [`ZOOM_RANGE`]): pages lay out at the
    /// window's size divided by it, and the renderer scales their scenes to
    /// the window. (Glyphs are rasterised at CSS size, so text above 100% is
    /// scaled, not re-rasterised.)
    pub fn set_zoom(&mut self, zoom: f32) {
        if zoom.is_finite() {
            self.zoom = zoom.clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
            self.resize(self.window.0, self.window.1);
        }
    }

    /// Input for the active tab, in device pixels — converted to the page's
    /// CSS pixels at the zoom.
    pub fn input(&mut self, input: EngineInput) {
        let Some(page) = self.active_page() else { return };
        let z = self.zoom;
        let input = match input {
            EngineInput::PointerMove { x, y } => EngineInput::PointerMove { x: x / z, y: y / z },
            EngineInput::PointerDown { x, y } => EngineInput::PointerDown { x: x / z, y: y / z },
            EngineInput::PointerUp { x, y } => EngineInput::PointerUp { x: x / z, y: y / z },
            EngineInput::Scroll { dx, dy } => EngineInput::Scroll { dx: dx / z, dy: dy / z },
            key @ EngineInput::Key { .. } => key,
        };
        self.engine.send(page, PageCommand::Input(input));
    }

    /// A command for any tab's page.
    pub fn send(&mut self, page: PageId, command: PageCommand) {
        self.engine.send(page, command);
    }

    /// Takes back `page`'s grant of `kind` (ADR 010 §4).
    pub fn revoke(&mut self, page: PageId, kind: CapabilityKind) {
        self.engine.send(page, PageCommand::Revoke(kind));
    }

    /// Grants `page` the capability of `kind` again, after a revoke (M36's
    /// gap; ADR 010 §4): a fresh handle from the profile, so the revoked one
    /// stays revoked. Kinds a web page is never granted are ignored.
    pub fn grant(&mut self, page: PageId, kind: CapabilityKind) {
        let all = self.profile.grants_for_web_page();
        let grants = capability::PageGrants {
            network: all.network.filter(|_| kind == CapabilityKind::Network),
            storage: all.storage.filter(|_| kind == CapabilityKind::Storage),
        };
        if grants.network.is_some() || grants.storage.is_some() {
            self.engine.send(page, PageCommand::Grant(grants));
        }
    }

    /// Takes back the grant of `kind` from every tab now showing a document
    /// of `origin`.
    pub fn revoke_for_origin(&mut self, origin: &Origin, kind: CapabilityKind) -> usize {
        let pages: Vec<PageId> = self
            .tabs
            .iter()
            .filter(|t| t.url.as_deref().is_some_and(|u| Origin::parse(u).same_origin(origin)))
            .map(|t| t.page)
            .collect();
        for &page in &pages {
            self.revoke(page, kind);
        }
        pages.len()
    }

    /// `page`'s document asks for `permission`, decided by the profile's
    /// store (a `Prompt` is for the shell to show). A granted permission
    /// still mints nothing until its API is built (ADR 010 §6).
    pub fn request_permission(&mut self, page: PageId, permission: Permission) -> PermissionState {
        let origin = self.index_of(page).and_then(|i| self.tabs[i].url.as_deref()).map(Origin::parse).unwrap_or_else(Origin::new_opaque);
        self.profile.permissions_mut().request(&origin, permission)
    }

    /// Advances every page to `now`; returns the active page's newest scene
    /// (or, after a switch, its last one) and every event. Writes the
    /// profile at most every [`FLUSH_INTERVAL`].
    pub fn update(&mut self, now: Instant) -> SessionUpdate {
        let events = self.engine.update(now);
        let active_page = self.active_page();
        let mut frame = None;
        for event in &events {
            match event {
                EngineEvent::Frame { page, scene, .. } => {
                    if let Some(index) = self.index_of(*page) {
                        self.tabs[index].last_frame = Some(scene.clone());
                    }
                    if Some(*page) == active_page {
                        frame = Some(scene.clone());
                    }
                }
                EngineEvent::NavigationCommitted { page, url } => {
                    if let Some(index) = self.index_of(*page) {
                        self.tabs[index].url = url.clone();
                    }
                }
                EngineEvent::PageCrashed { page, reason } => {
                    if let Some(index) = self.index_of(*page) {
                        self.tabs[index].crashed = Some(reason.clone());
                    }
                }
                _ => {}
            }
        }
        if self.present_cached {
            self.present_cached = false;
            if frame.is_none() {
                frame = self.active.and_then(|i| self.tabs[i].last_frame.clone());
            }
        }
        if self.last_flush.is_none_or(|at| now.saturating_duration_since(at) >= FLUSH_INTERVAL) {
            self.last_flush = Some(now);
            if let Err(e) = self.profile.flush() {
                eprintln!("could not save the profile: {e}");
            }
        }
        SessionUpdate { frame, events }
    }

    /// When the host should next call [`Self::update`] (see
    /// [`Engine::next_wake`]); now, when a switched-to tab's frame is
    /// waiting to be presented; and by the next flush, when the profile has
    /// unsaved changes — an idle window must still write what a page stored.
    pub fn next_wake(&self) -> Option<Instant> {
        if self.present_cached {
            return Some(Instant::now());
        }
        let flush = self.profile.has_unsaved_changes().then(|| self.last_flush.map_or_else(Instant::now, |at| at + FLUSH_INTERVAL));
        match (self.engine.next_wake(), flush) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Decision;

    const WAIT: Duration = Duration::from_secs(10);

    fn session() -> Session {
        Session::new(Profile::in_memory(), EngineConfig::default())
    }

    fn html(body: &str, url: &str) -> LoadRequest {
        LoadRequest::Html { html: format!("<html><body>{body}</body></html>"), url: Some(url.into()) }
    }

    /// Updates until `done`, returning every update's frames and events.
    fn run_until(session: &mut Session, now: &mut Instant, mut done: impl FnMut(&Session, &[SessionUpdate]) -> bool) -> Vec<SessionUpdate> {
        let deadline = Instant::now() + WAIT;
        let mut all = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "timed out");
            *now += Duration::from_millis(16);
            all.push(session.update(*now));
            if done(session, &all) {
                return all;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn all_committed(session: &Session) -> bool {
        session.tabs().iter().all(|t| t.url.is_some())
    }

    /// M38b's gap: a window nobody can see hides its active tab — it builds
    /// no frames, as a background tab does — and a tab opened meanwhile
    /// starts hidden; shown again, the active tab draws.
    #[test]
    fn a_hidden_window_hides_its_active_tab() {
        let animated = "<p id=n>0</p><script>var n = 0; function f() { n++; document.getElementById('n').textContent = n; requestAnimationFrame(f); } requestAnimationFrame(f);</script>";
        let mut s = session();
        s.open_tab(html(animated, "https://a.test/"));
        let mut now = Instant::now();
        run_until(&mut s, &mut now, |_, u| u.iter().filter(|u| u.frame.is_some()).count() >= 3);

        s.set_window_visible(false);
        assert!(!s.window_visible());
        for _ in 0..3 {
            now += Duration::from_millis(16);
            s.update(now); // the command lands
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut frames = 0;
        for _ in 0..20 {
            now += Duration::from_millis(16);
            frames += usize::from(s.update(now).frame.is_some());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(frames, 0, "a hidden window's tab builds no frames");
        let b = s.open_tab(html(animated, "https://b.test/"));
        run_until(&mut s, &mut now, |s, _| all_committed(s));
        assert!(s.tabs[1].last_frame.is_none(), "a tab opened in a hidden window starts hidden");

        s.set_window_visible(true);
        run_until(&mut s, &mut now, |s, u| u.iter().filter(|u| u.frame.is_some()).count() >= 3 && s.active_page() == Some(b));
    }

    #[test]
    fn tabs_open_switch_and_close_and_only_the_active_tab_is_presented() {
        let mut s = session();
        let a = s.open_tab(html("<p>first</p>", "https://a.test/"));
        let mut now = Instant::now();
        run_until(&mut s, &mut now, |s, _| s.tabs[0].last_frame.is_some());
        let a_frame = s.tabs[0].last_frame.as_ref().unwrap().sequence;
        let b = s.open_tab(html("<p>second page</p>", "https://b.test/"));
        assert_eq!(s.active_page(), Some(b), "a new tab is active");
        let updates = run_until(&mut s, &mut now, |s, u| all_committed(s) && u.iter().any(|u| u.frame.is_some()));
        // Milestone 38b (ADR 020): the background tab is hidden and drew nothing.
        assert_eq!(s.tabs[0].last_frame.as_ref().unwrap().sequence, a_frame, "a hidden tab builds no frames");
        let presented: Vec<u64> = updates.iter().filter_map(|u| u.frame.as_ref().map(|f| f.sequence)).collect();
        assert!(!presented.is_empty());

        // Switching shows a's last frame at once, before a draws anything new.
        assert!(s.activate(a));
        assert_eq!(s.next_wake().map(|w| w <= Instant::now()), Some(true));
        let update = s.update(now + Duration::from_millis(16));
        let shown = update.frame.expect("the switched-to tab's frame is presented");
        assert_eq!(shown.sequence, s.tabs[0].last_frame.as_ref().unwrap().sequence);

        let tabs = s.tabs();
        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].active && !tabs[1].active);
        assert_eq!(tabs[1].url.as_deref(), Some("https://b.test/"));

        s.activate_next(false);
        assert_eq!(s.active_page(), Some(b));
        s.activate_next(false);
        assert_eq!(s.active_page(), Some(a), "wraps");
        assert!(s.close_tab(a));
        assert_eq!(s.active_page(), Some(b), "the remaining tab becomes active");
        assert!(s.close_tab(b));
        assert_eq!(s.active_page(), None);
        assert!(!s.close_tab(b));
    }

    #[test]
    fn every_tab_is_granted_by_the_profile_and_revocable_on_its_own() {
        let mut s = session();
        let a = s.open_tab(html("", "https://a.test/"));
        let b = s.open_tab(html("", "https://b.test/"));
        assert_eq!(s.engine().page_grants(a), vec![CapabilityKind::Network, CapabilityKind::Storage]);
        s.revoke(a, CapabilityKind::Network);
        assert_eq!(s.engine().page_grants(a), vec![CapabilityKind::Storage]);
        assert_eq!(s.engine().page_grants(b), vec![CapabilityKind::Network, CapabilityKind::Storage], "b keeps its own grant");
        let mut now = Instant::now();
        run_until(&mut s, &mut now, |s, _| all_committed(s));
        assert_eq!(s.revoke_for_origin(&Origin::parse("https://b.test/x"), CapabilityKind::Storage), 1);
        assert_eq!(s.engine().page_grants(b), vec![CapabilityKind::Network]);
        // M36's gap: granted again, from the profile, one kind at a time.
        s.grant(b, CapabilityKind::Storage);
        assert_eq!(s.engine().page_grants(b), vec![CapabilityKind::Network, CapabilityKind::Storage]);
        s.grant(a, CapabilityKind::Camera);
        assert_eq!(s.engine().page_grants(a), vec![CapabilityKind::Storage], "a kind a web page is never granted is ignored");
    }

    #[test]
    fn zoom_scales_the_viewport_and_the_input() {
        let mut s = session();
        s.resize(800.0, 600.0);
        s.set_zoom(2.0);
        let page = s.open_tab(html("<p>x</p>", "https://a.test/"));
        let mut now = Instant::now();
        let updates = run_until(&mut s, &mut now, |_, u| u.iter().any(|u| u.frame.is_some()));
        let frame = updates.iter().rev().find_map(|u| u.frame.as_ref()).unwrap();
        assert_eq!((frame.viewport_width, frame.viewport_height), (400.0, 300.0), "the page lays out at half the window");
        s.set_zoom(100.0);
        assert_eq!(s.zoom(), 5.0, "clamped");
        s.set_zoom(f32::NAN);
        assert_eq!(s.zoom(), 5.0, "ignored");
        assert_eq!(s.active_page(), Some(page));
    }

    #[test]
    fn an_idle_session_wakes_to_write_what_a_page_stored() {
        let dir = std::env::temp_dir().join(format!("be-session-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::new(Profile::open(&dir).unwrap(), EngineConfig::default());
        s.open_tab(html("<script>localStorage.setItem('k', 'v')</script>", "https://a.test/"));
        let mut now = Instant::now();
        run_until(&mut s, &mut now, |s, _| all_committed(s));
        assert!(s.profile().has_unsaved_changes(), "the page's write is not on disk yet");
        let wake = s.next_wake().expect("a session with unsaved changes wakes");
        assert!(wake <= now + FLUSH_INTERVAL, "by the next flush, on the host's clock");
        // The host wakes at `wake`, by its clock, and updates.
        s.update(now + FLUSH_INTERVAL);
        assert!(!s.profile().has_unsaved_changes());
        let file = std::fs::read_to_string(dir.join(crate::LOCAL_STORAGE_FILE)).unwrap();
        assert!(file.contains("https://a.test\tk\tv"), "{file}");
    }

    #[test]
    fn the_default_zoom_comes_from_the_settings() {
        let mut profile = Profile::in_memory();
        let mut settings = profile.settings().clone();
        settings.set_default_zoom(1.5).unwrap();
        profile.set_settings(settings).unwrap();
        let s = Session::new(profile, EngineConfig::default());
        assert_eq!(s.zoom(), 1.5);
    }

    #[test]
    fn permission_requests_are_decided_by_the_origin_the_tab_shows() {
        let mut s = session();
        let page = s.open_tab(html("", "https://a.test/"));
        assert_eq!(s.request_permission(page, Permission::Camera), PermissionState::Denied, "nothing committed yet: an opaque origin");
        let mut now = Instant::now();
        run_until(&mut s, &mut now, |s, _| all_committed(s));
        let PermissionState::Prompt(id) = s.request_permission(page, Permission::Camera) else { panic!("prompt") };
        s.profile_mut().permissions_mut().resolve(id, Decision::Allow);
        assert_eq!(s.request_permission(page, Permission::Camera), PermissionState::Granted);
    }
}
