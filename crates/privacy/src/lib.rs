//! The fingerprint policy — Milestone 37 (`docs/adr/019-privacy-policies.md`
//! §3).
//!
//! One [`FingerprintPolicy`] decides what script can learn about the machine
//! through every surface that exists: the clock (`Date`, `requestAnimationFrame`
//! timestamps), the timezone, and which installed fonts content may use. A
//! surface added later (screen size, say) reads its value through here too;
//! [`bucket_px`] is the rule for sizes.

#![forbid(unsafe_code)]

/// What `Date` reports as local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimezonePolicy {
    /// The machine's timezone (what sites showing local times expect).
    #[default]
    Real,
    /// UTC, whatever the machine's: the timezone is not exposed.
    Utc,
}

impl TimezonePolicy {
    pub const fn name(self) -> &'static str {
        match self {
            TimezonePolicy::Real => "real",
            TimezonePolicy::Utc => "utc",
        }
    }

    pub fn from_name(name: &str) -> Option<TimezonePolicy> {
        match name {
            "real" => Some(TimezonePolicy::Real),
            "utc" => Some(TimezonePolicy::Utc),
            _ => None,
        }
    }
}

/// Which of the host's fonts content may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontExposure {
    /// Every font the host grants.
    #[default]
    All,
    /// Only the [`STANDARD_FONT_FAMILIES`] (and the bundled font and web
    /// fonts, which say nothing about the machine).
    Standard,
}

/// Font families common enough across systems to say little about a user
/// (prefixes: `Noto Sans Devanagari` is a `Noto` family).
pub const STANDARD_FONT_FAMILIES: [&str; 3] = ["DejaVu", "Liberation", "Noto"];

/// Bounds of the time resolution, in milliseconds.
pub const MIN_TIME_RESOLUTION_MS: u32 = 1;
pub const MAX_TIME_RESOLUTION_MS: u32 = 1000;

/// What script may observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FingerprintPolicy {
    /// `Date.now()`, `new Date()` and rAF timestamps are multiples of this
    /// (clamped to 1–1000 ms).
    time_resolution_ms: u32,
    pub timezone: TimezonePolicy,
    pub fonts: FontExposure,
}

impl Default for FingerprintPolicy {
    fn default() -> Self {
        FingerprintPolicy::standard()
    }
}

impl FingerprintPolicy {
    /// The default: 1 ms time, the real timezone, every granted font.
    pub const fn standard() -> FingerprintPolicy {
        FingerprintPolicy { time_resolution_ms: 1, timezone: TimezonePolicy::Real, fonts: FontExposure::All }
    }

    /// Private mode: 100 ms time, UTC, the standard fonts only.
    pub const fn private() -> FingerprintPolicy {
        FingerprintPolicy { time_resolution_ms: 100, timezone: TimezonePolicy::Utc, fonts: FontExposure::Standard }
    }

    pub fn time_resolution_ms(&self) -> u32 {
        self.time_resolution_ms
    }

    /// The same policy with another time resolution (clamped to 1–1000 ms).
    pub fn with_time_resolution_ms(self, ms: u32) -> FingerprintPolicy {
        FingerprintPolicy { time_resolution_ms: ms.clamp(MIN_TIME_RESOLUTION_MS, MAX_TIME_RESOLUTION_MS), ..self }
    }

    /// `ms` rounded down to the resolution; a non-finite or negative value
    /// is 0.
    pub fn coarsen_ms(&self, ms: f64) -> f64 {
        if !ms.is_finite() || ms < 0.0 {
            return 0.0;
        }
        let r = f64::from(self.time_resolution_ms);
        (ms / r).floor() * r
    }

    /// The same, for whole milliseconds since the epoch (may be negative).
    pub fn coarsen_epoch_ms(&self, ms: i64) -> i64 {
        let r = i64::from(self.time_resolution_ms);
        ms - ms.rem_euclid(r)
    }

    /// Whether content may use an installed font of `family`.
    pub fn font_allowed(&self, family: &str) -> bool {
        match self.fonts {
            FontExposure::All => true,
            FontExposure::Standard => {
                let family = family.to_ascii_lowercase();
                STANDARD_FONT_FAMILIES.iter().any(|prefix| family.starts_with(&prefix.to_ascii_lowercase()))
            }
        }
    }
}

/// A size as an API exposing it must report it: to the nearest 100 px
/// below, at least 100 (ADR 019 §3). No such API exists today.
pub fn bucket_px(px: f32) -> f32 {
    if !px.is_finite() || px < 100.0 {
        return 100.0;
    }
    (px / 100.0).floor() * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_is_coarsened_to_the_resolution() {
        let standard = FingerprintPolicy::standard();
        assert_eq!(standard.coarsen_ms(1234.567), 1234.0);
        let private = FingerprintPolicy::private();
        assert_eq!(private.coarsen_ms(1234.567), 1200.0);
        assert_eq!(private.coarsen_epoch_ms(1_700_000_000_123), 1_700_000_000_100);
        assert_eq!(private.coarsen_epoch_ms(-1), -100, "rounded down before the epoch too");
        for bad in [f64::NAN, f64::INFINITY, -5.0] {
            assert_eq!(private.coarsen_ms(bad), 0.0);
        }
        assert_eq!(standard.with_time_resolution_ms(0).time_resolution_ms(), 1);
        assert_eq!(standard.with_time_resolution_ms(1_000_000).time_resolution_ms(), 1000);
    }

    #[test]
    fn the_standard_font_set_admits_common_families_only() {
        let private = FingerprintPolicy::private();
        for family in ["DejaVu Sans", "Liberation Serif", "Noto Sans Devanagari", "dejavu sans mono"] {
            assert!(private.font_allowed(family), "{family}");
        }
        for family in ["Comic Sans MS", "Company Corporate Font", "Arial"] {
            assert!(!private.font_allowed(family), "{family}");
            assert!(FingerprintPolicy::standard().font_allowed(family));
        }
    }

    #[test]
    fn sizes_are_bucketed() {
        assert_eq!(bucket_px(1366.0), 1300.0);
        assert_eq!(bucket_px(1920.0), 1900.0);
        assert_eq!(bucket_px(50.0), 100.0);
        assert_eq!(bucket_px(f32::NAN), 100.0);
    }

    #[test]
    fn timezone_names_round_trip() {
        for tz in [TimezonePolicy::Real, TimezonePolicy::Utc] {
            assert_eq!(TimezonePolicy::from_name(tz.name()), Some(tz));
        }
        assert_eq!(TimezonePolicy::from_name("Europe/Berlin"), None);
    }
}
