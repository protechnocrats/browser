//! A profile: one user's browser state, and the one authority that grants
//! its pages capabilities (Milestone 36, ADR 010 §8).

use crate::permissions::{PermissionStore, MAX_PERMISSIONS_BYTES};
use crate::settings::{Settings, TimezoneSetting, MAX_SETTINGS_BYTES};
use capability::{Authority, PageGrants};
use engine::{FingerprintPolicy, NetworkContext, TimezonePolicy};
use network::{FetchResponse, FilterCategory, FilterEngine, NetworkError, NetworkRoute, RequestContext};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use storage::{read_capped, write_private_file, StorageStore};

/// The directory of filter lists inside a profile (ADR 019 §1).
pub const FILTERS_DIR: &str = "filters";
/// Most list files read from it.
pub const MAX_FILTER_FILES: usize = 32;

/// What a host supplies when opening a profile (Milestone 37).
#[derive(Clone, Default)]
pub struct ProfileOptions {
    /// Reports whether a system VPN is up; enables the `system-vpn` route
    /// (ADR 011 §4). The host decides how to tell.
    pub vpn_probe: Option<network::VpnProbe>,
    /// Settings for this run only, applied after the file through the same
    /// validation (`key`, `value`) — the command line. Never saved.
    pub overrides: Vec<(String, String)>,
}

/// Refuses every request: the network a profile gets when its network
/// settings cannot be honoured. A configured proxy that is broken must not
/// turn into direct connections (ADR 011 §2).
struct FailClosed(String);

impl NetworkRoute for FailClosed {
    fn fetch(&self, url: &str) -> Result<FetchResponse, NetworkError> {
        self.fetch_with(url, &RequestContext::default())
    }
    fn fetch_with(&self, _url: &str, _ctx: &RequestContext) -> Result<FetchResponse, NetworkError> {
        diagnostics::guard(diagnostics::Guard::RouteFailedClosed, "the profile's network settings are invalid; every request is refused");
        Err(NetworkError::Transport(format!("the network settings cannot be used ({}); nothing is sent", self.0)))
    }
}

/// The filter: the built-in list plus every `ads-*.txt`, `trackers-*.txt` and
/// `ai-*.txt` file in `dir` (at most [`MAX_FILTER_FILES`], each capped).
fn load_filters(dir: Option<&Path>, issues: &mut Vec<String>) -> FilterEngine {
    let mut engine = FilterEngine::with_built_in();
    let Some(dir) = dir else { return engine };
    let Ok(entries) = std::fs::read_dir(dir) else { return engine };
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for path in files.into_iter().take(MAX_FILTER_FILES) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let category = if name.starts_with("ads-") {
            FilterCategory::Ads
        } else if name.starts_with("trackers-") {
            FilterCategory::Trackers
        } else if name.starts_with("ai-") {
            FilterCategory::AiCrawlers
        } else {
            continue;
        };
        if !name.ends_with(".txt") {
            continue;
        }
        match read_capped(&path, filter::MAX_LIST_BYTES) {
            Ok(Some(Ok(text))) => match engine.add_list(category, &text) {
                Ok(report) if report.rejected > 0 => issues.push(format!("filters/{name}: {} malformed rule(s) dropped", report.rejected)),
                Ok(_) => {}
                Err(e) => issues.push(format!("filters/{name}: {e}")),
            },
            Ok(Some(Err(problem))) => issues.push(format!("filters/{name}: {problem}")),
            Ok(None) => {}
            Err(e) => issues.push(format!("filters/{name}: {e}")),
        }
    }
    engine
}

/// The network a profile's settings describe (ADR 011, 012, 018). Extra CA
/// files are read here (the profile is the host's side); one that cannot be
/// read is reported and skipped.
fn network_config(settings: &Settings, options: &ProfileOptions, issues: &mut Vec<String>) -> network::NetworkConfig {
    use network::{DohConfig, ProxyConfig, RouteRules, TlsTrust};
    let mut config = network::NetworkConfig { private_mode: settings.privacy_mode(), ..network::NetworkConfig::default() };
    if let Some(proxy) = settings.proxy() {
        config.proxies.push(("proxy".into(), ProxyConfig { url: proxy.into() }));
    }
    if let Some(proxy) = settings.private_proxy() {
        config.proxies.push(("private".into(), ProxyConfig { url: proxy.into() }));
    }
    config.system_vpn = options.vpn_probe.clone();
    let mut rules = RouteRules { private_override: settings.private_proxy().map(|_| "private".to_string()), ..RouteRules::default() };
    match (settings.proxy(), settings.proxy_sites().is_empty()) {
        (Some(_), true) => rules.profile_default = Some("proxy".into()),
        (Some(_), false) => rules.site_rules = settings.proxy_sites().iter().map(|d| (d.clone(), "proxy".to_string())).collect(),
        (None, _) => {}
    }
    if settings.vpn_only() && rules.profile_default.is_none() {
        // With no probe the route does not exist, and the rule makes every
        // request fail — VPN-only means that, not "direct".
        rules.profile_default = Some(network::SYSTEM_VPN.into());
    }
    config.rules = rules;
    config.doh = settings.doh().map(|url| DohConfig { resolver_url: url.into(), bootstrap: settings.doh_bootstrap().to_vec() });
    let mut pems = Vec::new();
    for path in settings.extra_roots() {
        match read_capped(Path::new(path), network::tls::MAX_PEM_BYTES) {
            Ok(Some(Ok(text))) => pems.push(text.into_bytes()),
            Ok(Some(Err(problem))) => issues.push(format!("tls.extra_roots: {problem}")),
            Ok(None) => issues.push(format!("tls.extra_roots: {path} does not exist")),
            Err(e) => issues.push(format!("tls.extra_roots: {path}: {e}")),
        }
    }
    config.tls = TlsTrust { extra_roots_pem: pems };
    config
}

/// The profile's network: from its settings, or — if they cannot be honoured
/// — one that refuses everything (fail closed).
fn build_network(settings: &Settings, options: &ProfileOptions, issues: &mut Vec<String>) -> NetworkContext {
    let config = network_config(settings, options, issues);
    let network = match NetworkContext::with_config(&config) {
        Ok((network, report)) => {
            issues.extend(report.issues);
            network
        }
        Err(e) => {
            issues.push(format!("network settings: {e}; every request will be refused until they are fixed"));
            NetworkContext::with_route(move |_| Arc::new(FailClosed(e)))
        }
    };
    apply_request_policy(&network, settings);
    network
}

fn apply_request_policy(network: &NetworkContext, settings: &Settings) {
    let mut policy = network.store().policy();
    policy.third_party_cookies = if settings.third_party_cookies_allowed() { network::ThirdPartyCookies::Allow } else { network::ThirdPartyCookies::Block };
    network.store().set_policy(policy);
    network.set_private_mode(settings.privacy_mode());
}

/// File names inside a profile directory.
pub const SETTINGS_FILE: &str = "settings";
pub const PERMISSIONS_FILE: &str = "permissions";
pub const LOCAL_STORAGE_FILE: &str = "local-storage";
pub const COOKIES_FILE: &str = "cookies";

/// One user's browser state: the network context (cookie jar, HTTP cache),
/// web storage, settings and permission decisions — in a directory, or in
/// memory only — and the [`Authority`] every page's grants are minted from.
pub struct Profile {
    dir: Option<PathBuf>,
    authority: Authority,
    network: NetworkContext,
    storage: StorageStore,
    settings: Settings,
    permissions: PermissionStore,
    /// What could not be read back when the profile was opened.
    issues: Vec<String>,
    /// Settings for this run only (never saved).
    overrides: Vec<(String, String)>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile").field("dir", &self.dir).field("settings", &self.settings).finish_non_exhaustive()
    }
}

/// Reads one profile file: its text, or `None` for "use the defaults" (no
/// file, too big, not UTF-8 — the last two reported).
fn read_profile_file(path: &Path, cap: usize, issues: &mut Vec<String>) -> std::io::Result<Option<String>> {
    match read_capped(path, cap)? {
        None => Ok(None),
        Some(Ok(text)) => Ok(Some(text)),
        Some(Err(problem)) => {
            diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "a profile file could not be read; using defaults");
            issues.push(problem);
            Ok(None)
        }
    }
}

impl Profile {
    /// A profile that keeps nothing once it is dropped.
    pub fn in_memory() -> Profile {
        Profile::in_memory_with_network(NetworkContext::new())
    }

    /// In memory, with run-time settings and host services (Milestone 37).
    pub fn in_memory_with(options: ProfileOptions) -> Profile {
        let mut issues = Vec::new();
        let (settings, applied) = settings_with_overrides(Settings::default(), &options.overrides, &mut issues);
        let network = build_network(&settings, &options, &mut issues);
        let mut profile = Profile::in_memory_with_network(network);
        profile.settings = settings;
        profile.issues = issues;
        profile.overrides = applied;
        profile
    }

    /// In memory, over a network context the host built (a recording or
    /// replaying route, a test double).
    pub fn in_memory_with_network(network: NetworkContext) -> Profile {
        Profile {
            dir: None,
            authority: Authority::new(),
            network,
            storage: StorageStore::new(),
            settings: Settings::default(),
            permissions: PermissionStore::new(),
            issues: Vec::new(),
            overrides: Vec::new(),
        }
    }

    /// The profile in `dir` (created if missing, owner-only): its settings,
    /// permissions, local storage and persistent cookies are read back now
    /// and written there as they change. A file that cannot be read gives
    /// defaults for what it held — never a failure to start — and is listed
    /// in [`Self::issues`]. `Err` only for I/O on the directory itself.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Profile> {
        Profile::open_with(dir, ProfileOptions::default())
    }

    /// Like [`Self::open`], with run-time settings and host services
    /// (Milestone 37). The network — routes, DNS, TLS trust, filter — is
    /// built from the settings now; changes to those take effect the next
    /// time the profile is opened.
    pub fn open_with(dir: impl Into<PathBuf>, options: ProfileOptions) -> std::io::Result<Profile> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut issues = Vec::new();

        let settings = match read_profile_file(&dir.join(SETTINGS_FILE), MAX_SETTINGS_BYTES, &mut issues)? {
            Some(text) => {
                let (settings, found) = Settings::parse(&text);
                if !found.is_empty() {
                    diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "the settings file had values that could not be used");
                }
                issues.extend(found.into_iter().map(|i| format!("settings: {i}")));
                settings
            }
            None => Settings::default(),
        };
        let permissions = match read_profile_file(&dir.join(PERMISSIONS_FILE), MAX_PERMISSIONS_BYTES, &mut issues)? {
            Some(text) => {
                let (permissions, found) = PermissionStore::parse(&text);
                if !found.is_empty() {
                    diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "the permissions file had lines that could not be read");
                }
                issues.extend(found.into_iter().map(|i| format!("permissions: {i}")));
                permissions
            }
            None => PermissionStore::new(),
        };
        let (storage, report) = StorageStore::open(dir.join(LOCAL_STORAGE_FILE))?;
        issues.extend(report.reasons.into_iter().map(|r| format!("local storage: {r}")));

        let (settings, applied) = settings_with_overrides(settings, &options.overrides, &mut issues);
        let network = build_network(&settings, &options, &mut issues);
        if let Err(e) = network.load_cookies(dir.join(COOKIES_FILE)) {
            issues.push(format!("cookies: {e}"));
        }
        network.set_filter(load_filters(Some(&dir.join(FILTERS_DIR)), &mut issues));
        Ok(Profile { dir: Some(dir), authority: Authority::new(), network, storage, settings, permissions, issues, overrides: applied })
    }

    /// The directory, if this profile has one.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// What could not be read back when the profile was opened.
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    /// The profile's network context: one cookie jar and HTTP cache for
    /// every page (Milestone 26). The engine loads documents through it.
    pub fn network(&self) -> &NetworkContext {
        &self.network
    }

    pub fn storage(&self) -> &StorageStore {
        &self.storage
    }

    /// What a new web page's scripts are granted: the network, and storage
    /// in this profile's store (each document narrowed to its own storage
    /// key by the engine). Fresh tokens per call: each page's grants are
    /// revoked on their own.
    pub fn grants_for_web_page(&self) -> PageGrants {
        PageGrants { network: Some(self.authority.network(self.network.clone())), storage: Some(self.authority.storage(self.storage.clone())) }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the settings (valid by construction) and writes them. Private
    /// mode and the third-party cookie policy apply at once; routes, DNS and
    /// TLS trust the next time the profile is opened.
    pub fn set_settings(&mut self, settings: Settings) -> std::io::Result<()> {
        self.settings = settings;
        apply_request_policy(&self.network, &self.settings);
        self.write_settings()
    }

    /// What new pages' scripts may observe (ADR 019 §3): the private policy
    /// in private mode, else the standard one; `privacy.timezone` overrides
    /// the timezone either way.
    pub fn fingerprint(&self) -> FingerprintPolicy {
        let mut policy = if self.settings.privacy_mode() { FingerprintPolicy::private() } else { FingerprintPolicy::standard() };
        match self.settings.timezone() {
            TimezoneSetting::Auto => {}
            TimezoneSetting::Real => policy.timezone = TimezonePolicy::Real,
            TimezoneSetting::Utc => policy.timezone = TimezonePolicy::Utc,
        }
        policy
    }

    /// Re-reads the filter lists in the profile's `filters/` directory (after
    /// one was added). Returns what could not be read.
    pub fn reload_filters(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let dir = self.dir.as_ref().map(|d| d.join(FILTERS_DIR));
        self.network.set_filter(load_filters(dir.as_deref(), &mut issues));
        issues
    }

    pub fn permissions(&self) -> &PermissionStore {
        &self.permissions
    }

    pub fn permissions_mut(&mut self) -> &mut PermissionStore {
        &mut self.permissions
    }

    /// Writes the settings — without this run's command-line overrides,
    /// which are never saved: the file's own values are re-read and the
    /// changed ones laid over them.
    fn write_settings(&self) -> std::io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let to_save = if self.overrides.is_empty() {
            self.settings.clone()
        } else {
            // Overridden keys keep their file values; everything else is saved.
            let path = dir.join(SETTINGS_FILE);
            let on_disk = match read_capped(&path, MAX_SETTINGS_BYTES)? {
                Some(Ok(text)) => Settings::parse(&text).0,
                _ => Settings::default(),
            };
            let mut merged = self.settings.clone();
            for (key, _) in &self.overrides {
                if let Some(value) = setting_value(&on_disk, key) {
                    let _ = merged.set(key, &value);
                }
            }
            merged
        };
        write_private_file(&dir.join(SETTINGS_FILE), to_save.serialize().as_bytes())
    }

    /// Whether something changed that [`Self::flush`] would write.
    pub fn has_unsaved_changes(&self) -> bool {
        self.dir.is_some() && (self.storage.is_dirty() || self.permissions.is_dirty())
    }

    /// Writes what changed since the last flush: local storage and the
    /// permission decisions. (Settings are written when set; cookies after
    /// every change, by the network context.)
    pub fn flush(&mut self) -> std::io::Result<()> {
        let Some(dir) = self.dir.clone() else { return Ok(()) };
        self.storage.flush()?;
        if self.permissions.take_dirty() {
            if let Err(e) = write_private_file(&dir.join(PERMISSIONS_FILE), self.permissions.serialize().as_bytes()) {
                self.permissions.mark_dirty();
                return Err(e);
            }
        }
        Ok(())
    }
}

/// `settings` with `overrides` applied (each through the validated path); a
/// refused override is reported, never applied.
/// Returns the overrides that applied: only those are kept out of the file.
fn settings_with_overrides(mut settings: Settings, overrides: &[(String, String)], issues: &mut Vec<String>) -> (Settings, Vec<(String, String)>) {
    let mut applied = Vec::new();
    for (key, value) in overrides {
        match settings.set(key, value) {
            Ok(()) => applied.push((key.clone(), value.clone())),
            Err(e) => issues.push(format!("command line: {e}")),
        }
    }
    (settings, applied)
}

/// The text value of `key` in `settings`, as the file writes it.
fn setting_value(settings: &Settings, key: &str) -> Option<String> {
    settings.serialize().lines().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

impl Drop for Profile {
    /// Best effort: what changed since the last flush is written.
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("could not save the profile: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Decision, Permission, PermissionState};
    use origin::{Origin, StorageKey};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("be-profile-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_profile_round_trips_settings_permissions_and_storage_through_its_directory() {
        let dir = temp_dir("roundtrip");
        let a = Origin::parse("https://a.example/");
        {
            let mut profile = Profile::open(&dir).unwrap();
            assert!(profile.issues().is_empty(), "{:?}", profile.issues());
            let mut settings = profile.settings().clone();
            settings.set_scroll_speed(3.0).unwrap();
            settings.set_default_zoom(1.5).unwrap();
            profile.set_settings(settings).unwrap();
            let PermissionState::Prompt(id) = profile.permissions_mut().request(&a, Permission::Camera) else { panic!() };
            profile.permissions_mut().resolve(id, Decision::Allow);
            let grants = profile.grants_for_web_page();
            grants.storage.unwrap().scope(&StorageKey::first_party(&a)).area().unwrap().set("k".into(), "v".into()).unwrap();
            // Dropped: flushed.
        }
        let profile = Profile::open(&dir).unwrap();
        assert!(profile.issues().is_empty(), "{:?}", profile.issues());
        assert_eq!(profile.settings().scroll_speed(), 3.0);
        assert_eq!(profile.settings().default_zoom(), 1.5);
        assert_eq!(profile.permissions().decision(&a, Permission::Camera), Some(Decision::Allow));
        assert_eq!(profile.storage().area_for(&StorageKey::first_party(&a)).get("k").as_deref(), Some("v"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            for file in [SETTINGS_FILE, PERMISSIONS_FILE, LOCAL_STORAGE_FILE] {
                assert_eq!(std::fs::metadata(dir.join(file)).unwrap().permissions().mode() & 0o777, 0o600, "{file}");
            }
        }
    }

    #[test]
    fn corrupt_profile_files_start_safely_with_defaults_and_say_why() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SETTINGS_FILE), "browser-engine settings 1\nscroll.speed = -4\nzoom.default = 99\n").unwrap();
        std::fs::write(dir.join(PERMISSIONS_FILE), [0xff, 0xfe, 0x00]).unwrap(); // not UTF-8
        std::fs::write(dir.join(LOCAL_STORAGE_FILE), "something else entirely").unwrap();
        let big = std::fs::File::create(dir.join(COOKIES_FILE)).unwrap();
        drop(big);
        let before = diagnostics::metrics::snapshot();
        let profile = Profile::open(&dir).unwrap();
        assert_eq!(profile.settings(), &Settings::default(), "out-of-range values fall back to defaults");
        assert!(profile.permissions().decisions().is_empty());
        assert_eq!(profile.storage().origin_count(), 0);
        assert!(profile.issues().len() >= 4, "{:?}", profile.issues());
        assert!(diagnostics::metrics::snapshot().delta(&before, "guard.profile_file_rejected") >= 3);
    }

    #[test]
    fn an_oversized_settings_file_is_not_read() {
        let dir = temp_dir("oversized");
        std::fs::create_dir_all(&dir).unwrap();
        let f = std::fs::File::create(dir.join(SETTINGS_FILE)).unwrap();
        f.set_len(MAX_SETTINGS_BYTES as u64 + 1).unwrap();
        let profile = Profile::open(&dir).unwrap();
        assert_eq!(profile.settings(), &Settings::default());
        assert_eq!(profile.issues().len(), 1, "{:?}", profile.issues());
    }

    #[test]
    fn each_page_gets_its_own_revocable_grants_over_the_one_store() {
        let profile = Profile::in_memory();
        let (one, two) = (profile.grants_for_web_page(), profile.grants_for_web_page());
        let key = StorageKey::first_party(&Origin::parse("https://a.example/"));
        one.storage.as_ref().unwrap().scope(&key).area().unwrap().set("k".into(), "v".into()).unwrap();
        assert_eq!(two.storage.as_ref().unwrap().scope(&key).area().unwrap().get("k").as_deref(), Some("v"), "one store");
        one.revoke(capability::CapabilityKind::Storage);
        assert!(one.storage.as_ref().unwrap().scope(&key).area().is_err());
        assert!(two.storage.as_ref().unwrap().scope(&key).area().is_ok(), "the other page's grant stands");
    }

    #[test]
    fn an_in_memory_profile_writes_nothing() {
        let mut profile = Profile::in_memory();
        profile.set_settings(Settings::default()).unwrap();
        profile.flush().unwrap();
        assert!(profile.dir().is_none());
    }

    // ---- Milestone 37 ----

    #[test]
    fn private_mode_gives_new_pages_the_private_fingerprint_policy() {
        let mut profile = Profile::in_memory();
        assert_eq!(profile.fingerprint(), FingerprintPolicy::standard());
        let mut settings = profile.settings().clone();
        settings.set_privacy_mode(true);
        profile.set_settings(settings.clone()).unwrap();
        assert_eq!(profile.fingerprint(), FingerprintPolicy::private());
        assert!(profile.network().store().private_mode(), "the route policy's private override sees it at once");
        settings.set("privacy.timezone", "real").unwrap();
        profile.set_settings(settings).unwrap();
        assert_eq!(profile.fingerprint().timezone, TimezonePolicy::Real, "the timezone setting overrides the mode");
    }

    #[test]
    fn vpn_only_without_a_vpn_fails_closed_rather_than_going_direct() {
        let options = ProfileOptions { vpn_probe: None, overrides: vec![("network.vpn_only".into(), "true".into())] };
        let profile = Profile::in_memory_with(options);
        assert!(profile.issues().iter().any(|i| i.contains("refused")), "{:?}", profile.issues());
        let result = profile.network().fetch_with("http://127.0.0.1:9/", &RequestContext::typed_navigation());
        assert!(matches!(result, Err(NetworkError::Transport(_))), "{result:?}");
        // With a probe the route exists, and follows it.
        let options = ProfileOptions { vpn_probe: Some(Arc::new(|| false)), overrides: vec![("network.vpn_only".into(), "true".into())] };
        let profile = Profile::in_memory_with(options);
        let down = profile.network().fetch_with("http://127.0.0.1:9/", &RequestContext::typed_navigation());
        assert!(matches!(&down, Err(NetworkError::Transport(why)) if why.contains("VPN is down")), "{down:?}");
    }

    #[test]
    fn command_line_overrides_apply_but_are_never_saved() {
        let dir = temp_dir("overrides");
        {
            let mut profile = Profile::open(&dir).unwrap();
            let mut settings = profile.settings().clone();
            settings.set_scroll_speed(2.0).unwrap();
            profile.set_settings(settings).unwrap();
        }
        let options = ProfileOptions { vpn_probe: None, overrides: vec![("scroll.speed".into(), "4".into()), ("zoom.default".into(), "99".into())] };
        let mut profile = Profile::open_with(&dir, options).unwrap();
        assert_eq!(profile.settings().scroll_speed(), 4.0, "the override applies this run");
        assert!(profile.issues().iter().any(|i| i.starts_with("command line: zoom.default")), "a bad override is reported: {:?}", profile.issues());
        // A change made this run is saved; the override is not.
        let mut settings = profile.settings().clone();
        settings.set_default_zoom(1.5).unwrap();
        profile.set_settings(settings).unwrap();
        drop(profile);
        let profile = Profile::open(&dir).unwrap();
        assert_eq!(profile.settings().scroll_speed(), 2.0);
        assert_eq!(profile.settings().default_zoom(), 1.5);
    }

    #[test]
    fn filter_lists_load_from_the_profile_and_reload() {
        let dir = temp_dir("filters");
        std::fs::create_dir_all(dir.join(FILTERS_DIR)).unwrap();
        std::fs::write(dir.join(FILTERS_DIR).join("ads-test.txt"), "||ads.example^\n||bad domain^\n").unwrap();
        std::fs::write(dir.join(FILTERS_DIR).join("notes.txt"), "||ignored.example^\n").unwrap();
        let profile = Profile::open(&dir).unwrap();
        let built_in = FilterEngine::with_built_in().len();
        assert_eq!(profile.network().store().filter().len(), built_in + 1, "only category-named lists load");
        assert!(profile.issues().iter().any(|i| i.contains("ads-test.txt") && i.contains("malformed")), "{:?}", profile.issues());
        std::fs::write(dir.join(FILTERS_DIR).join("ai-crawlers.txt"), "0.0.0.0 crawler.example\n").unwrap();
        profile.reload_filters();
        assert_eq!(profile.network().store().filter().len(), built_in + 2);
    }

    #[test]
    fn the_third_party_cookie_setting_reaches_the_network_policy() {
        let mut profile = Profile::in_memory();
        assert_eq!(profile.network().store().policy().third_party_cookies, network::ThirdPartyCookies::Block);
        let mut settings = profile.settings().clone();
        settings.set("network.third_party_cookies", "allow").unwrap();
        profile.set_settings(settings).unwrap();
        assert_eq!(profile.network().store().policy().third_party_cookies, network::ThirdPartyCookies::Allow);
    }

    #[test]
    fn an_unreadable_extra_ca_file_is_reported_and_the_bundled_roots_remain() {
        let options = ProfileOptions { vpn_probe: None, overrides: vec![("tls.extra_roots".into(), "/nonexistent/ca.pem".into())] };
        let profile = Profile::in_memory_with(options);
        assert!(profile.issues().iter().any(|i| i.contains("tls.extra_roots")), "{:?}", profile.issues());
        assert!(!profile.issues().iter().any(|i| i.contains("refused")), "the network still works: {:?}", profile.issues());
    }
}
