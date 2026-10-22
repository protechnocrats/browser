//! Browser Core — Milestone 36 (`docs/adr/010-capabilities.md` §8).
//!
//! What is neither engine nor UI: the [`Profile`] (one user's network
//! context, storage, settings and permission decisions, and the one
//! capability `Authority` their pages' grants are minted from), the
//! [`Session`] (the pages of one window over one engine), [`Settings`]
//! (typed, versioned, validated, persisted) and the [`PermissionStore`]
//! (decisions as data; the prompt is the shell's).
//!
//! It sits above `engine` and below the shells, and reaches the pipeline
//! only through the engine's API.

#![forbid(unsafe_code)]

mod permissions;
mod profile;
mod session;
mod settings;

pub use permissions::{
    Decision, PendingRequest, Permission, PermissionState, PermissionStore, RequestId, MAX_DECISIONS, MAX_PENDING, MAX_PENDING_PER_ORIGIN,
    MAX_PERMISSIONS_BYTES, PERMISSIONS_HEADER,
};
/// Milestone 37: what a host hands a profile to enable the `system-vpn` route.
pub use network::VpnProbe;
pub use profile::{Profile, ProfileOptions, COOKIES_FILE, FILTERS_DIR, LOCAL_STORAGE_FILE, MAX_FILTER_FILES, PERMISSIONS_FILE, SETTINGS_FILE};
pub use session::{Session, SessionUpdate, TabInfo, FLUSH_INTERVAL};
pub use settings::{InvalidSetting, Settings, TimezoneSetting, MAX_LIST_ENTRIES, MAX_SETTINGS_BYTES, SCROLL_SPEED_RANGE, SETTINGS_HEADER, ZOOM_RANGE};
