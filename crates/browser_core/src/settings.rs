//! Typed, versioned, validated settings, persisted under the profile
//! (Milestone 36; network and privacy keys since Milestone 37, ADR 010–012,
//! 018, 019).
//!
//! ```text
//! browser-engine settings 1
//! scroll.invert = false
//! scroll.speed = 1
//! zoom.default = 1
//! privacy.mode = false
//! privacy.timezone = auto
//! network.third_party_cookies = block
//! network.proxy =
//! network.proxy_sites =
//! network.private_proxy =
//! network.vpn_only = false
//! dns.doh =
//! dns.doh_bootstrap =
//! tls.extra_roots =
//! ```
//!
//! A value is either valid or not set: out of range, non-finite, malformed,
//! of the wrong type, or for a key this build does not know, it is ignored
//! and reported, and the default stands. A file of another version (or none
//! at all) gives the defaults; a newer one is not guessed at. [`Settings::set`]
//! is the one way to change a value — the parser, the setters and the
//! command line all go through it. Keys added in M37 keep version 1: an older
//! build reports them as unknown and keeps its defaults.

use std::net::IpAddr;
use std::ops::RangeInclusive;

/// The first line of a settings file: its format and version.
pub const SETTINGS_HEADER: &str = "browser-engine settings 1";

/// Largest settings file read back.
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;

/// Multiplier on every scroll delta.
pub const SCROLL_SPEED_RANGE: RangeInclusive<f32> = 0.1..=10.0;

/// Page zoom: CSS pixels per device pixel.
pub const ZOOM_RANGE: RangeInclusive<f32> = 0.25..=5.0;

/// Most entries in a list-valued setting (sites, addresses, files).
pub const MAX_LIST_ENTRIES: usize = 64;

/// A value a setter refused.
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidSetting {
    pub key: String,
    pub reason: String,
}

impl std::fmt::Display for InvalidSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.reason)
    }
}

impl std::error::Error for InvalidSetting {}

/// `privacy.timezone`: what `Date` reports as local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimezoneSetting {
    /// The real one, or UTC in private mode.
    #[default]
    Auto,
    Real,
    Utc,
}

/// The profile's settings. Every field is valid by construction: the
/// defaults are, and [`Settings::set`] admits only valid values.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    scroll_invert: bool,
    scroll_speed: f32,
    default_zoom: f32,
    privacy_mode: bool,
    timezone: TimezoneSetting,
    third_party_cookies_allowed: bool,
    proxy: Option<String>,
    proxy_sites: Vec<String>,
    private_proxy: Option<String>,
    vpn_only: bool,
    doh: Option<String>,
    doh_bootstrap: Vec<IpAddr>,
    extra_roots: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            scroll_invert: false,
            scroll_speed: 1.0,
            default_zoom: 1.0,
            privacy_mode: false,
            timezone: TimezoneSetting::Auto,
            third_party_cookies_allowed: false,
            proxy: None,
            proxy_sites: Vec::new(),
            private_proxy: None,
            vpn_only: false,
            doh: None,
            doh_bootstrap: Vec::new(),
            extra_roots: Vec::new(),
        }
    }
}

fn in_range(value: f32, range: &RangeInclusive<f32>) -> Result<f32, String> {
    if value.is_finite() && range.contains(&value) {
        Ok(value)
    } else {
        Err(format!("{value} is outside {}–{}", range.start(), range.end()))
    }
}

fn boolean(value: &str) -> Result<bool, String> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("`{value}` is not true or false")),
    }
}

fn number(value: &str) -> Result<f32, String> {
    value.parse::<f32>().map_err(|_| format!("`{value}` is not a number"))
}

/// A comma-separated list (empty: none).
fn list(value: &str) -> Result<Vec<String>, String> {
    let items: Vec<String> = value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if items.len() > MAX_LIST_ENTRIES {
        return Err(format!("more than {MAX_LIST_ENTRIES} entries"));
    }
    Ok(items)
}

fn proxy_url(value: &str) -> Result<Option<String>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    network::ProxyConfig { url: value.to_string() }.parse()?;
    Ok(Some(value.to_string()))
}

fn domain(value: &str) -> Result<String, String> {
    let d = value.trim_end_matches('.').to_ascii_lowercase();
    let ok = !d.is_empty() && d.len() <= 253 && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') && !d.starts_with('.');
    if ok {
        Ok(d)
    } else {
        Err(format!("`{value}` is not a domain"))
    }
}

impl Settings {
    pub fn scroll_invert(&self) -> bool {
        self.scroll_invert
    }
    pub fn scroll_speed(&self) -> f32 {
        self.scroll_speed
    }
    pub fn default_zoom(&self) -> f32 {
        self.default_zoom
    }
    /// Private mode: the route policy's private override, and the private
    /// fingerprint policy for new pages (Milestone 37).
    pub fn privacy_mode(&self) -> bool {
        self.privacy_mode
    }
    pub fn timezone(&self) -> TimezoneSetting {
        self.timezone
    }
    pub fn third_party_cookies_allowed(&self) -> bool {
        self.third_party_cookies_allowed
    }
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }
    pub fn proxy_sites(&self) -> &[String] {
        &self.proxy_sites
    }
    pub fn private_proxy(&self) -> Option<&str> {
        self.private_proxy.as_deref()
    }
    pub fn vpn_only(&self) -> bool {
        self.vpn_only
    }
    pub fn doh(&self) -> Option<&str> {
        self.doh.as_deref()
    }
    pub fn doh_bootstrap(&self) -> &[IpAddr] {
        &self.doh_bootstrap
    }
    pub fn extra_roots(&self) -> &[String] {
        &self.extra_roots
    }

    pub fn set_scroll_invert(&mut self, invert: bool) {
        self.scroll_invert = invert;
    }
    pub fn set_scroll_speed(&mut self, speed: f32) -> Result<(), InvalidSetting> {
        self.set("scroll.speed", &speed.to_string())
    }
    pub fn set_default_zoom(&mut self, zoom: f32) -> Result<(), InvalidSetting> {
        self.set("zoom.default", &zoom.to_string())
    }
    pub fn set_privacy_mode(&mut self, on: bool) {
        self.privacy_mode = on;
    }

    /// Sets `key` from its text form — the one validated path every change
    /// takes (the file, the setters, the command line). Refused: nothing
    /// changes.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), InvalidSetting> {
        let value = value.trim();
        let outcome: Result<(), String> = (|| {
            match key {
                "scroll.invert" => self.scroll_invert = boolean(value)?,
                "scroll.speed" => self.scroll_speed = in_range(if value.is_empty() { f32::NAN } else { number(value)? }, &SCROLL_SPEED_RANGE)?,
                "zoom.default" => self.default_zoom = in_range(if value.is_empty() { f32::NAN } else { number(value)? }, &ZOOM_RANGE)?,
                "privacy.mode" => self.privacy_mode = boolean(value)?,
                "privacy.timezone" => {
                    self.timezone = match value {
                        "auto" => TimezoneSetting::Auto,
                        "real" => TimezoneSetting::Real,
                        "utc" => TimezoneSetting::Utc,
                        _ => return Err(format!("`{value}` is not auto, real or utc")),
                    }
                }
                "network.third_party_cookies" => {
                    self.third_party_cookies_allowed = match value {
                        "block" => false,
                        "allow" => true,
                        _ => return Err(format!("`{value}` is not block or allow")),
                    }
                }
                "network.proxy" => self.proxy = proxy_url(value)?,
                "network.private_proxy" => self.private_proxy = proxy_url(value)?,
                "network.proxy_sites" => self.proxy_sites = list(value)?.iter().map(|d| domain(d)).collect::<Result<_, _>>()?,
                "network.vpn_only" => self.vpn_only = boolean(value)?,
                "dns.doh" => {
                    self.doh = if value.is_empty() {
                        None
                    } else {
                        network::DohConfig { resolver_url: value.to_string(), bootstrap: Vec::new() }.validate()?;
                        Some(value.to_string())
                    }
                }
                "dns.doh_bootstrap" => {
                    self.doh_bootstrap = list(value)?.iter().map(|ip| ip.parse::<IpAddr>().map_err(|_| format!("`{ip}` is not an IP address"))).collect::<Result<_, _>>()?
                }
                "tls.extra_roots" => {
                    let paths = list(value)?;
                    if paths.iter().any(|p| p.len() > 4096) {
                        return Err("a path is too long".into());
                    }
                    self.extra_roots = paths;
                }
                _ => return Err("unknown setting".into()),
            }
            Ok(())
        })();
        outcome.map_err(|reason| InvalidSetting { key: key.chars().take(64).collect(), reason })
    }

    /// The settings file (deterministic).
    pub fn serialize(&self) -> String {
        let join = |v: &[String]| v.join(",");
        let ips = self.doh_bootstrap.iter().map(IpAddr::to_string).collect::<Vec<_>>().join(",");
        let timezone = match self.timezone {
            TimezoneSetting::Auto => "auto",
            TimezoneSetting::Real => "real",
            TimezoneSetting::Utc => "utc",
        };
        format!(
            "{SETTINGS_HEADER}\nscroll.invert = {}\nscroll.speed = {}\nzoom.default = {}\nprivacy.mode = {}\nprivacy.timezone = {timezone}\n\
             network.third_party_cookies = {}\nnetwork.proxy = {}\nnetwork.proxy_sites = {}\nnetwork.private_proxy = {}\nnetwork.vpn_only = {}\n\
             dns.doh = {}\ndns.doh_bootstrap = {ips}\ntls.extra_roots = {}\n",
            self.scroll_invert,
            self.scroll_speed,
            self.default_zoom,
            self.privacy_mode,
            if self.third_party_cookies_allowed { "allow" } else { "block" },
            self.proxy.as_deref().unwrap_or(""),
            join(&self.proxy_sites),
            self.private_proxy.as_deref().unwrap_or(""),
            self.vpn_only,
            self.doh.as_deref().unwrap_or(""),
            join(&self.extra_roots),
        )
    }

    /// Reads a settings file. Never fails and never panics: what is valid is
    /// taken, everything else is reported in the second value and left at
    /// its default.
    pub fn parse(text: &str) -> (Settings, Vec<String>) {
        let mut settings = Settings::default();
        let mut issues = Vec::new();
        let mut note = |issue: String| {
            if issues.len() < 16 {
                issues.push(issue);
            }
        };
        let mut lines = text.lines();
        match lines.next().map(str::trim) {
            Some(SETTINGS_HEADER) => {}
            Some(other) if other.starts_with("browser-engine settings ") => {
                note(format!("settings of an unknown version ({:?}); using the defaults", other.chars().take(64).collect::<String>()));
                return (settings, issues);
            }
            _ => {
                note("not a settings file; using the defaults".to_string());
                return (settings, issues);
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for (number, line) in lines.enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                note(format!("line {}: not `key = value`", number + 2));
                continue;
            };
            let key = key.trim();
            if seen.iter().any(|k| k == key) {
                note(format!("line {}: `{}` set twice; the first value stands", number + 2, key.chars().take(64).collect::<String>()));
                continue;
            }
            match settings.set(key, value) {
                Ok(()) => seen.push(key.to_string()),
                Err(e) => note(format!("line {}: {e}; the default stands", number + 2)),
            }
        }
        (settings, issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_round_trip() {
        let (parsed, issues) = Settings::parse(&Settings::default().serialize());
        assert_eq!(parsed, Settings::default());
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn changed_settings_round_trip() {
        let mut s = Settings::default();
        s.set_scroll_invert(true);
        s.set_scroll_speed(2.5).unwrap();
        s.set_default_zoom(1.25).unwrap();
        s.set_privacy_mode(true);
        for (key, value) in [
            ("privacy.timezone", "utc"),
            ("network.third_party_cookies", "allow"),
            ("network.proxy", "socks5://127.0.0.1:9050"),
            ("network.proxy_sites", "Example.com, intranet.example."),
            ("network.private_proxy", "http://127.0.0.1:3128"),
            ("network.vpn_only", "true"),
            ("dns.doh", "https://dns.example/dns-query"),
            ("dns.doh_bootstrap", "192.0.2.1, 2001:db8::1"),
            ("tls.extra_roots", "/etc/corp/ca.pem,/home/u/my-ca.pem"),
        ] {
            s.set(key, value).unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(s.proxy_sites(), ["example.com", "intranet.example"]);
        let (parsed, issues) = Settings::parse(&s.serialize());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(parsed, s);
    }

    #[test]
    fn setters_refuse_out_of_range_and_non_finite_values() {
        let mut s = Settings::default();
        for bad in [0.0, -1.0, 0.09, 10.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(s.set_scroll_speed(bad).is_err(), "{bad}");
        }
        for bad in [0.0, 0.2, 5.5, f32::NAN, 1e30] {
            assert!(s.set_default_zoom(bad).is_err(), "{bad}");
        }
        assert_eq!(s, Settings::default(), "a refused value changes nothing");
    }

    #[test]
    fn network_values_are_checked_when_set() {
        let mut s = Settings::default();
        for (key, bad) in [
            ("network.proxy", "https://proxy.example"),
            ("network.proxy", "nonsense"),
            ("network.proxy_sites", "a b.example"),
            ("dns.doh", "http://dns.example/dns-query"),
            ("dns.doh", "not a url"),
            ("dns.doh_bootstrap", "999.1.1.1"),
            ("network.third_party_cookies", "sometimes"),
            ("privacy.timezone", "Europe/Berlin"),
            ("scroll.speed", ""),
        ] {
            assert!(s.set(key, bad).is_err(), "{key} = {bad}");
        }
        let many = vec!["a.example"; MAX_LIST_ENTRIES + 1].join(",");
        assert!(s.set("network.proxy_sites", &many).is_err());
        assert_eq!(s, Settings::default());
        // An empty value clears a setting.
        s.set("network.proxy", "http://127.0.0.1:3128").unwrap();
        s.set("network.proxy", "").unwrap();
        assert_eq!(s.proxy(), None);
    }

    #[test]
    fn out_of_range_and_malformed_values_fall_back_to_defaults() {
        let text = format!(
            "{SETTINGS_HEADER}\nscroll.speed = 1000\nzoom.default = NaN\nscroll.invert = yes\nprivacy.mode = true\nwhat = 1\nno equals sign\nscroll.speed = 2\nscroll.speed = 3\n"
        );
        let (s, issues) = Settings::parse(&text);
        assert_eq!(s.scroll_speed(), 2.0, "the first valid value stands; a later duplicate is ignored");
        assert_eq!(s.default_zoom(), 1.0);
        assert!(!s.scroll_invert());
        assert!(s.privacy_mode());
        assert_eq!(issues.len(), 6, "{issues:?}");
    }

    #[test]
    fn another_version_or_garbage_gives_the_defaults() {
        for text in ["browser-engine settings 2\nscroll.speed = 2\n", "", "scroll.speed = 2\n", "\u{0}\u{ffff}\n\n", "browser-engine cookies 1\n"] {
            let (s, issues) = Settings::parse(text);
            assert_eq!(s, Settings::default(), "{text:?}");
            assert_eq!(issues.len(), 1, "{text:?}: {issues:?}");
        }
    }

    #[test]
    fn issues_are_bounded() {
        let text = format!("{SETTINGS_HEADER}\n{}", "garbage\n".repeat(10_000));
        let (_, issues) = Settings::parse(&text);
        assert_eq!(issues.len(), 16);
    }
}
