//! Permission decisions as data (Milestone 36, ADR 010 §7).
//!
//! A decision is keyed by (origin, permission): allowed, denied, or not yet
//! asked. Asking with no decision yields a pending prompt — a [`RequestId`]
//! the shell may show and later answer with [`PermissionStore::resolve`].
//! The prompt UI is the shell's; the store never decides on its own.
//!
//! - **Origin-scoped:** an opaque origin is always denied and never stored
//!   (it is unique, so a decision for it could never be found again).
//! - **Revocable:** [`PermissionStore::revoke`] forgets a decision; the next
//!   request asks again.
//! - **Audited:** every request, decision and revocation is an
//!   `engine::permission` event (M27).
//! - **Bounded:** pending prompts per origin and in total, and stored
//!   decisions, are capped; past a cap a request is denied
//!   (`permission_requests_capped`) — a page cannot grow the store.
//! - **Persisted** as text under the profile, read back without trust.

use diagnostics::tracing;
use origin::Origin;
use std::collections::BTreeMap;

/// The first line of a permissions file.
pub const PERMISSIONS_HEADER: &str = "browser-engine permissions 1";

/// Largest permissions file read back.
pub const MAX_PERMISSIONS_BYTES: usize = 1024 * 1024;

/// Pending prompts one origin may have.
pub const MAX_PENDING_PER_ORIGIN: usize = 4;

/// Pending prompts in all.
pub const MAX_PENDING: usize = 64;

/// Decisions kept.
pub const MAX_DECISIONS: usize = 4096;

/// What a site can ask the user for. (The network and storage are not
/// asked for: a profile grants them to every web page it opens — ADR 010
/// §8.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    Clipboard,
    Geolocation,
    Camera,
    Microphone,
    Notifications,
}

impl Permission {
    pub const ALL: [Permission; 5] = [Permission::Clipboard, Permission::Geolocation, Permission::Camera, Permission::Microphone, Permission::Notifications];

    /// The capability a granted permission would mint.
    pub fn kind(self) -> capability::CapabilityKind {
        match self {
            Permission::Clipboard => capability::CapabilityKind::Clipboard,
            Permission::Geolocation => capability::CapabilityKind::Geolocation,
            Permission::Camera => capability::CapabilityKind::Camera,
            Permission::Microphone => capability::CapabilityKind::Microphone,
            Permission::Notifications => capability::CapabilityKind::Notifications,
        }
    }

    pub fn name(self) -> &'static str {
        self.kind().name()
    }

    pub fn from_name(name: &str) -> Option<Permission> {
        Permission::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// A user's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    fn name(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
        }
    }
}

/// A pending prompt's handle. Plain data, never reused within one store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

/// What a request comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    /// No decision yet: the shell may prompt and [`PermissionStore::resolve`].
    Prompt(RequestId),
}

/// A prompt the shell has not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub id: RequestId,
    pub origin: Origin,
    pub permission: Permission,
}

/// The profile's permission decisions and pending prompts.
#[derive(Debug, Default)]
pub struct PermissionStore {
    /// By serialised (tuple) origin.
    decisions: BTreeMap<(String, Permission), Decision>,
    pending: Vec<PendingRequest>,
    next_id: u64,
    /// Something to write since the last save.
    dirty: bool,
}

fn audit(origin: &str, permission: Permission, outcome: &str) {
    tracing::info!(target: "engine::permission", origin = origin, permission = permission.name(), outcome = outcome, "permission");
}

impl PermissionStore {
    pub fn new() -> PermissionStore {
        PermissionStore::default()
    }

    /// `origin` asks for `permission`.
    pub fn request(&mut self, origin: &Origin, permission: Permission) -> PermissionState {
        if origin.is_opaque() {
            audit("null", permission, "denied: opaque origin");
            return PermissionState::Denied;
        }
        let key = origin.serialize();
        if let Some(decision) = self.decisions.get(&(key.clone(), permission)) {
            audit(&key, permission, decision.name());
            return match decision {
                Decision::Allow => PermissionState::Granted,
                Decision::Deny => PermissionState::Denied,
            };
        }
        if let Some(existing) = self.pending.iter().find(|p| p.permission == permission && &p.origin == origin) {
            return PermissionState::Prompt(existing.id);
        }
        let of_origin = self.pending.iter().filter(|p| &p.origin == origin).count();
        if of_origin >= MAX_PENDING_PER_ORIGIN || self.pending.len() >= MAX_PENDING {
            diagnostics::guard(diagnostics::Guard::PermissionRequestsCapped, "too many permission prompts pending; request denied");
            audit(&key, permission, "denied: too many pending prompts");
            return PermissionState::Denied;
        }
        self.next_id += 1;
        let id = RequestId(self.next_id);
        self.pending.push(PendingRequest { id, origin: origin.clone(), permission });
        audit(&key, permission, "prompt");
        PermissionState::Prompt(id)
    }

    /// The prompts waiting for the user, oldest first.
    pub fn pending(&self) -> &[PendingRequest] {
        &self.pending
    }

    /// The user answered prompt `id`. Returns what was decided, for whom —
    /// `None` if there is no such prompt (answered already, or never asked).
    pub fn resolve(&mut self, id: RequestId, decision: Decision) -> Option<(Origin, Permission, Decision)> {
        let index = self.pending.iter().position(|p| p.id == id)?;
        let request = self.pending.remove(index);
        let key = request.origin.serialize();
        if self.decisions.len() >= MAX_DECISIONS && !self.decisions.contains_key(&(key.clone(), request.permission)) {
            // Full: the answer holds for this prompt only.
            audit(&key, request.permission, "decided (not stored: the store is full)");
        } else {
            self.decisions.insert((key.clone(), request.permission), decision);
            self.dirty = true;
            audit(&key, request.permission, decision.name());
        }
        Some((request.origin, request.permission, decision))
    }

    /// The prompt was closed without an answer: nothing is stored.
    pub fn dismiss(&mut self, id: RequestId) -> bool {
        let before = self.pending.len();
        self.pending.retain(|p| p.id != id);
        before != self.pending.len()
    }

    /// The stored decision, if any.
    pub fn decision(&self, origin: &Origin, permission: Permission) -> Option<Decision> {
        if origin.is_opaque() {
            return None;
        }
        self.decisions.get(&(origin.serialize(), permission)).copied()
    }

    /// Forgets the decision for (`origin`, `permission`): the next request
    /// prompts again. Returns whether there was one.
    pub fn revoke(&mut self, origin: &Origin, permission: Permission) -> bool {
        if origin.is_opaque() {
            return false;
        }
        let key = origin.serialize();
        let had = self.decisions.remove(&(key.clone(), permission)).is_some();
        if had {
            self.dirty = true;
            audit(&key, permission, "revoked");
        }
        had
    }

    /// Every stored decision (a settings page lists these).
    pub fn decisions(&self) -> Vec<(String, Permission, Decision)> {
        self.decisions.iter().map(|((origin, p), d)| (origin.clone(), *p, *d)).collect()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// The permissions file (deterministic). Pending prompts are not saved.
    pub fn serialize(&self) -> String {
        let mut out = format!("{PERMISSIONS_HEADER}\n");
        for ((origin, permission), decision) in &self.decisions {
            out.push_str(&format!("{origin}\t{}\t{}\n", permission.name(), decision.name()));
        }
        out
    }

    /// Reads a permissions file; what is not exactly well formed is dropped
    /// and reported. Never panics.
    pub fn parse(text: &str) -> (PermissionStore, Vec<String>) {
        let mut store = PermissionStore::new();
        let mut issues = Vec::new();
        let mut note = |issue: String| {
            if issues.len() < 16 {
                issues.push(issue);
            }
        };
        let mut lines = text.split('\n');
        if lines.next().map(|l| l.trim_end_matches('\r')) != Some(PERMISSIONS_HEADER) {
            note("not a permissions file of a version this build reads; starting empty".to_string());
            return (store, issues);
        }
        for (number, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.splitn(4, '\t').collect();
            let [origin, permission, decision] = fields[..] else {
                note(format!("line {}: not three fields", number + 2));
                continue;
            };
            let parsed = Origin::parse(origin);
            if parsed.is_opaque() || parsed.serialize() != origin {
                note(format!("line {}: not a serialised origin", number + 2));
                continue;
            }
            let Some(permission) = Permission::from_name(permission) else {
                note(format!("line {}: unknown permission", number + 2));
                continue;
            };
            let decision = match decision {
                "allow" => Decision::Allow,
                "deny" => Decision::Deny,
                _ => {
                    note(format!("line {}: not allow or deny", number + 2));
                    continue;
                }
            };
            if store.decisions.len() >= MAX_DECISIONS {
                note(format!("line {}: more decisions than the store keeps", number + 2));
                break;
            }
            store.decisions.insert((origin.to_string(), permission), decision);
        }
        (store, issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(url: &str) -> Origin {
        Origin::parse(url)
    }

    #[test]
    fn a_request_prompts_until_the_user_decides_then_the_decision_holds_for_that_origin_only() {
        let mut store = PermissionStore::new();
        let a = origin("https://a.example/");
        let PermissionState::Prompt(id) = store.request(&a, Permission::Geolocation) else { panic!("no decision yet: prompt") };
        assert_eq!(store.request(&a, Permission::Geolocation), PermissionState::Prompt(id), "the same prompt, not a second one");
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.resolve(id, Decision::Allow), Some((a.clone(), Permission::Geolocation, Decision::Allow)));
        assert!(store.pending().is_empty());
        assert_eq!(store.request(&origin("https://a.example/other/path"), Permission::Geolocation), PermissionState::Granted);
        // Scoped by origin: scheme, host and port all count.
        for other in ["http://a.example/", "https://b.example/", "https://a.example:8443/", "https://sub.a.example/"] {
            assert!(matches!(store.request(&origin(other), Permission::Geolocation), PermissionState::Prompt(_)), "{other}");
        }
        // And by permission.
        assert!(matches!(store.request(&a, Permission::Camera), PermissionState::Prompt(_)));
        assert_eq!(store.resolve(id, Decision::Deny), None, "a prompt is answered once");
    }

    #[test]
    fn a_revoked_decision_asks_again() {
        let mut store = PermissionStore::new();
        let a = origin("https://a.example/");
        let PermissionState::Prompt(id) = store.request(&a, Permission::Notifications) else { panic!() };
        store.resolve(id, Decision::Deny);
        assert_eq!(store.request(&a, Permission::Notifications), PermissionState::Denied);
        assert!(store.revoke(&a, Permission::Notifications));
        assert!(!store.revoke(&a, Permission::Notifications), "nothing left to revoke");
        assert!(matches!(store.request(&a, Permission::Notifications), PermissionState::Prompt(_)));
    }

    #[test]
    fn opaque_origins_are_always_denied_and_never_stored() {
        let mut store = PermissionStore::new();
        let opaque = Origin::new_opaque();
        assert_eq!(store.request(&opaque, Permission::Clipboard), PermissionState::Denied);
        assert!(store.pending().is_empty());
        assert_eq!(store.decision(&opaque, Permission::Clipboard), None);
    }

    #[test]
    fn a_page_cannot_flood_the_prompt_queue() {
        let mut store = PermissionStore::new();
        let a = origin("https://a.example/");
        for p in Permission::ALL {
            store.request(&a, p);
        }
        assert_eq!(store.pending().len(), MAX_PENDING_PER_ORIGIN, "per origin");
        for i in 0..1000 {
            store.request(&origin(&format!("https://o{i}.example/")), Permission::Camera);
        }
        assert_eq!(store.pending().len(), MAX_PENDING, "in total");
    }

    #[test]
    fn a_dismissed_prompt_stores_nothing() {
        let mut store = PermissionStore::new();
        let a = origin("https://a.example/");
        let PermissionState::Prompt(id) = store.request(&a, Permission::Microphone) else { panic!() };
        assert!(store.dismiss(id));
        assert_eq!(store.decision(&a, Permission::Microphone), None);
        assert!(matches!(store.request(&a, Permission::Microphone), PermissionState::Prompt(_)));
    }

    #[test]
    fn decisions_round_trip_and_hostile_files_load_only_what_is_well_formed() {
        let mut store = PermissionStore::new();
        for (url, p, d) in [("https://a.example/", Permission::Camera, Decision::Allow), ("http://b.example:8080/", Permission::Clipboard, Decision::Deny)] {
            let PermissionState::Prompt(id) = store.request(&origin(url), p) else { panic!() };
            store.resolve(id, d);
        }
        let (loaded, issues) = PermissionStore::parse(&store.serialize());
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(loaded.decisions(), store.decisions());

        let text = format!(
            "{PERMISSIONS_HEADER}\nhttps://ok.example\tcamera\tallow\nnull\tcamera\tallow\nhttps://ok.example/path\tcamera\tallow\n\
             https://ok.example\tfilesystem\tallow\nhttps://ok.example\tcamera\tmaybe\nhttps://ok.example\tcamera\n\
             https://ok.example\tcamera\tallow\textra\n"
        );
        let (loaded, issues) = PermissionStore::parse(&text);
        assert_eq!(loaded.decisions().len(), 1);
        assert_eq!(issues.len(), 6, "{issues:?}");
        for garbage in ["", "browser-engine permissions 9\n", "\u{0}\n\t\t\n"] {
            let (loaded, issues) = PermissionStore::parse(garbage);
            assert!(loaded.decisions().is_empty());
            assert_eq!(issues.len(), 1);
        }
    }
}
