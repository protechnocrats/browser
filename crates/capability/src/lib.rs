//! Capabilities — Milestone 36 (`docs/adr/010-capabilities.md`).
//!
//! Authority is a value. A [`NetworkCapability`] holds the network context
//! it grants; a [`StorageCapability`] holds the profile's storage store, and
//! narrows ([`StorageCapability::scope`]) to an [`OriginStorage`] that holds
//! one area. Code that has not been handed a handle has no way to reach what
//! it guards: the fields are private, and the one minting point, the
//! [`Authority`], is constructed only where `scripts/check-capabilities.sh`
//! allows (the browser's profile, and a renderer re-creating what its host
//! listed — ADR 010 §2).
//!
//! Every handle minted by one grant shares a revocation token: after
//! [`NetworkCapability::revoke`] (or the storage one) every clone, and every
//! handle narrowed from it, refuses with [`CapabilityError`].
//!
//! APIs that are not built yet have capability types that cannot be
//! constructed at all ([`ClipboardCapability`] and the rest hold an
//! uninhabited value), so nothing can be wired to them — ambiently or
//! otherwise — until a mint method exists here (ADR 010 §6).

#![forbid(unsafe_code)]

use network::NetworkContext;
use origin::StorageKey;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use storage::{StorageArea, StorageStore};

/// What a capability grants. Plain data: what crosses the engine API and
/// the renderer protocol when a grant is listed or revoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapabilityKind {
    /// Script-initiated requests: `fetch()` and `XMLHttpRequest`.
    Network,
    /// `localStorage` for the documents' own storage keys.
    Storage,
    Clipboard,
    Geolocation,
    Camera,
    Microphone,
    Notifications,
}

impl CapabilityKind {
    pub const ALL: [CapabilityKind; 7] = [
        CapabilityKind::Network,
        CapabilityKind::Storage,
        CapabilityKind::Clipboard,
        CapabilityKind::Geolocation,
        CapabilityKind::Camera,
        CapabilityKind::Microphone,
        CapabilityKind::Notifications,
    ];

    /// Stable lowercase name (persisted permission files, events).
    pub const fn name(self) -> &'static str {
        match self {
            CapabilityKind::Network => "network",
            CapabilityKind::Storage => "storage",
            CapabilityKind::Clipboard => "clipboard",
            CapabilityKind::Geolocation => "geolocation",
            CapabilityKind::Camera => "camera",
            CapabilityKind::Microphone => "microphone",
            CapabilityKind::Notifications => "notifications",
        }
    }

    pub fn from_name(name: &str) -> Option<CapabilityKind> {
        CapabilityKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Whether this build can mint the capability at all. The rest exist
    /// only as types (ADR 010 §6).
    pub const fn is_implemented(self) -> bool {
        matches!(self, CapabilityKind::Network | CapabilityKind::Storage)
    }

    /// One bit per kind, for the renderer protocol's grant list.
    pub const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Why a capability refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The page was never granted this capability.
    NotGranted,
    /// It was granted and has been revoked.
    Revoked,
    /// The handle's scope is not what was asked for (e.g. another origin's
    /// storage).
    OutOfScope,
}

/// A refusal on capability grounds — never a network or storage error.
/// Displays as the DOM exception script sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityError {
    pub kind: CapabilityKind,
    pub denial: Denial,
}

impl CapabilityError {
    /// A refusal, reported as the `capability_denied` guard.
    pub fn report(kind: CapabilityKind, denial: Denial) -> CapabilityError {
        let detail = match denial {
            Denial::NotGranted => "a page used an API it holds no capability for",
            Denial::Revoked => "a page used a capability that was revoked",
            Denial::OutOfScope => "a page used a capability outside its scope",
        };
        diagnostics::guard(diagnostics::Guard::CapabilityDenied, detail);
        CapabilityError { kind, denial }
    }
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let exception = match self.kind {
            CapabilityKind::Storage => "SecurityError",
            _ => "NotAllowedError",
        };
        let why = match self.denial {
            Denial::NotGranted => "this page has not been granted",
            Denial::Revoked => "this page's grant was revoked for",
            Denial::OutOfScope => "this page's grant does not cover this use of",
        };
        write!(f, "{exception}: {why} the {} capability", self.kind.name())
    }
}

impl std::error::Error for CapabilityError {}

/// The revocation state every handle of one grant shares.
#[derive(Debug, Default)]
struct Token {
    revoked: AtomicBool,
}

impl Token {
    fn check(&self, kind: CapabilityKind) -> Result<(), CapabilityError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(CapabilityError::report(kind, Denial::Revoked))
        } else {
            Ok(())
        }
    }
}

/// The minting point. Whoever holds one can create capabilities, so it is
/// constructed only by the browser's profile and by a renderer re-creating
/// its host's grants (`scripts/check-capabilities.sh`; ADR 010 §2).
#[derive(Debug)]
pub struct Authority {
    _private: (),
}

impl Authority {
    #[allow(clippy::new_without_default)] // a Default would be a second, unchecked way to mint
    pub fn new() -> Authority {
        Authority { _private: () }
    }

    /// Script access to the network through `context`.
    pub fn network(&self, context: NetworkContext) -> NetworkCapability {
        NetworkCapability { context, token: Arc::default() }
    }

    /// `localStorage` in `store`, for whatever storage keys it is scoped to.
    pub fn storage(&self, store: StorageStore) -> StorageCapability {
        StorageCapability { store, token: Arc::default() }
    }
}

/// Script-initiated network access (`fetch()`, `XMLHttpRequest`): holds the
/// network context, so holding one is the only way to reach it.
#[derive(Clone)]
pub struct NetworkCapability {
    context: NetworkContext,
    token: Arc<Token>,
}

impl NetworkCapability {
    /// The network, unless this grant was revoked.
    pub fn context(&self) -> Result<&NetworkContext, CapabilityError> {
        self.token.check(CapabilityKind::Network)?;
        Ok(&self.context)
    }

    /// Takes the grant back: this handle and every clone refuse from now on.
    pub fn revoke(&self) {
        self.token.revoked.store(true, Ordering::Release);
    }

    pub fn is_revoked(&self) -> bool {
        self.token.revoked.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for NetworkCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkCapability").field("revoked", &self.is_revoked()).finish()
    }
}

/// `localStorage` in the profile's store, before it is narrowed to one
/// storage key. Held by the engine for a page, never by script.
#[derive(Clone)]
pub struct StorageCapability {
    store: StorageStore,
    token: Arc<Token>,
}

impl StorageCapability {
    /// The area of one storage key, as a handle that reaches only that area
    /// and shares this grant's revocation. An opaque origin's key gets a
    /// private area.
    pub fn scope(&self, key: &StorageKey) -> OriginStorage {
        OriginStorage { area: self.store.area_for(key), key: key.clone(), token: Arc::clone(&self.token) }
    }

    /// Takes the grant back, from this handle and every area scoped from it.
    pub fn revoke(&self) {
        self.token.revoked.store(true, Ordering::Release);
    }

    pub fn is_revoked(&self) -> bool {
        self.token.revoked.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for StorageCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageCapability").field("revoked", &self.is_revoked()).finish()
    }
}

/// One storage key's area: what script's `localStorage` is handed.
#[derive(Clone)]
pub struct OriginStorage {
    area: StorageArea,
    key: StorageKey,
    token: Arc<Token>,
}

impl OriginStorage {
    /// The area, unless the grant it was scoped from was revoked.
    pub fn area(&self) -> Result<&StorageArea, CapabilityError> {
        self.token.check(CapabilityKind::Storage)?;
        Ok(&self.area)
    }

    /// The storage key this handle reaches (and no other).
    pub fn key(&self) -> &StorageKey {
        &self.key
    }

    pub fn is_revoked(&self) -> bool {
        self.token.revoked.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for OriginStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginStorage").field("key", &self.key).field("revoked", &self.is_revoked()).finish()
    }
}

/// No value of this type exists: what the capabilities of APIs that are not
/// built hold, so they cannot be created (ADR 010 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Never {}

macro_rules! unbuilt_capability {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        ///
        /// Cannot be constructed: the API it would grant is not built.
        #[derive(Debug, Clone)]
        pub struct $name {
            never: Never,
        }

        impl $name {
            /// Unreachable by construction; here so the type is usable in
            /// signatures (a binding that takes one can never be called).
            pub fn never(&self) -> ! {
                match self.never {}
            }
        }
    };
}

unbuilt_capability!(
    /// The system clipboard (copy and paste).
    ClipboardCapability
);
unbuilt_capability!(
    /// The device's location.
    GeolocationCapability
);
unbuilt_capability!(
    /// A camera.
    CameraCapability
);
unbuilt_capability!(
    /// A microphone.
    MicrophoneCapability
);
unbuilt_capability!(
    /// System notifications.
    NotificationsCapability
);

/// Everything one page has been granted. `PageGrants::none()` grants
/// nothing and needs no authority; anything more is minted by the profile.
#[derive(Debug, Clone, Default)]
pub struct PageGrants {
    pub network: Option<NetworkCapability>,
    pub storage: Option<StorageCapability>,
}

/// Two grants are equal when they hold the same handles: the same tokens,
/// so revoking one revokes the other.
impl PartialEq for PageGrants {
    fn eq(&self, other: &PageGrants) -> bool {
        let same = |a: Option<&Arc<Token>>, b: Option<&Arc<Token>>| match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        same(self.network.as_ref().map(|n| &n.token), other.network.as_ref().map(|n| &n.token))
            && same(self.storage.as_ref().map(|s| &s.token), other.storage.as_ref().map(|s| &s.token))
    }
}

impl PageGrants {
    /// No authority at all: script can neither fetch nor store.
    pub fn none() -> PageGrants {
        PageGrants::default()
    }

    /// The kinds granted and not revoked.
    pub fn kinds(&self) -> Vec<CapabilityKind> {
        let mut kinds = Vec::new();
        if self.network.as_ref().is_some_and(|n| !n.is_revoked()) {
            kinds.push(CapabilityKind::Network);
        }
        if self.storage.as_ref().is_some_and(|s| !s.is_revoked()) {
            kinds.push(CapabilityKind::Storage);
        }
        kinds
    }

    /// The kinds as one bit each ([`CapabilityKind::bit`]).
    pub fn bits(&self) -> u8 {
        self.kinds().into_iter().fold(0, |bits, k| bits | k.bit())
    }

    /// Takes the kinds `new` holds: each replaces this page's grant of that
    /// kind (a re-grant after a revoke — a fresh token, so handles revoked
    /// before stay revoked). Kinds `new` lacks are left as they are.
    pub fn merge(&mut self, new: PageGrants) {
        if new.network.is_some() {
            self.network = new.network;
        }
        if new.storage.is_some() {
            self.storage = new.storage;
        }
    }

    /// Revokes the grant of `kind`, if there is one: every handle of it, in
    /// this page's documents and their scripts, refuses from now on.
    pub fn revoke(&self, kind: CapabilityKind) {
        match kind {
            CapabilityKind::Network => {
                if let Some(n) = &self.network {
                    n.revoke();
                }
            }
            CapabilityKind::Storage => {
                if let Some(s) = &self.storage {
                    s.revoke();
                }
            }
            // Never granted: nothing to revoke.
            _ => {}
        }
    }
}

// Grants travel to page threads (ADR 006).
const _: () = {
    const fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<PageGrants>();
    assert_send_sync::<OriginStorage>();
    assert_send_sync::<NetworkCapability>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use origin::Origin;

    fn key(url: &str) -> StorageKey {
        StorageKey::first_party(&Origin::parse(url))
    }

    #[test]
    fn a_revoked_network_capability_refuses_through_every_clone() {
        let authority = Authority::new();
        let network = authority.network(NetworkContext::new());
        let copy = network.clone();
        assert!(copy.context().is_ok());
        network.revoke();
        let Err(err) = copy.context() else { panic!("revoked") };
        assert_eq!(err, CapabilityError { kind: CapabilityKind::Network, denial: Denial::Revoked });
        assert!(err.to_string().starts_with("NotAllowedError:"), "{err}");
    }

    #[test]
    fn grants_are_independent_tokens() {
        let authority = Authority::new();
        let one = authority.network(NetworkContext::new());
        let two = authority.network(NetworkContext::new());
        one.revoke();
        assert!(two.context().is_ok(), "revoking one grant leaves another alone");
    }

    #[test]
    fn storage_scopes_to_one_key_and_revocation_reaches_scoped_handles() {
        let authority = Authority::new();
        let store = StorageStore::new();
        let grant = authority.storage(store.clone());
        let a = grant.scope(&key("https://a.example/"));
        let b = grant.scope(&key("https://b.example/"));
        a.area().unwrap().set("k".into(), "a".into()).unwrap();
        assert_eq!(b.area().unwrap().get("k"), None, "a scoped handle reaches only its own key");
        assert_eq!(a.key(), &key("https://a.example/"));
        grant.revoke();
        let err = a.area().expect_err("revoked");
        assert_eq!(err.denial, Denial::Revoked);
        assert!(err.to_string().starts_with("SecurityError:"), "{err}");
        assert!(b.area().is_err());
        // The data is still the profile's; only this grant lost access.
        assert_eq!(store.area_for(&key("https://a.example/")).get("k").as_deref(), Some("a"));
    }

    #[test]
    fn page_grants_list_and_revoke_by_kind() {
        let authority = Authority::new();
        assert!(PageGrants::none().kinds().is_empty());
        assert_eq!(PageGrants::none().bits(), 0);
        let grants = PageGrants { network: Some(authority.network(NetworkContext::new())), storage: Some(authority.storage(StorageStore::new())) };
        assert_eq!(grants.kinds(), vec![CapabilityKind::Network, CapabilityKind::Storage]);
        let held_by_script = grants.network.clone().unwrap();
        grants.revoke(CapabilityKind::Network);
        assert!(held_by_script.context().is_err(), "the copy script holds is revoked too");
        assert_eq!(grants.kinds(), vec![CapabilityKind::Storage]);
        assert_eq!(grants.bits(), CapabilityKind::Storage.bit());
        grants.revoke(CapabilityKind::Clipboard); // never granted: a no-op
    }

    #[test]
    fn kind_names_round_trip_and_bits_are_distinct() {
        let mut bits = 0u8;
        for kind in CapabilityKind::ALL {
            assert_eq!(CapabilityKind::from_name(kind.name()), Some(kind));
            assert_eq!(bits & kind.bit(), 0);
            bits |= kind.bit();
        }
        assert_eq!(CapabilityKind::from_name("filesystem"), None);
        assert!(CapabilityKind::Network.is_implemented() && !CapabilityKind::Clipboard.is_implemented());
    }
}
