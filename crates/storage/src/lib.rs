//! Web storage for one profile — Milestone 36 (`docs/adr/010-capabilities.md`).
//!
//! A [`StorageStore`] holds the profile's `localStorage`: one
//! [`StorageArea`] per storage key (an origin under its top-level site,
//! Milestone 28), shared by every page with that key and by no other. It
//! enforces the per-area quota, bounds how many origins it keeps, and — when
//! opened on a file — persists itself there.
//!
//! Script never reaches a store: it is handed one area, through a
//! `capability::OriginStorage` (ADR 010). That is why this lives outside
//! `js`.
//!
//! ## The file
//!
//! ```text
//! browser-engine local-storage 1
//! https://example.com<TAB>key<TAB>value
//! ```
//!
//! One line per entry; tabs, newlines, carriage returns and backslashes in
//! keys and values are escaped (`\t`, `\n`, `\r`, `\\`). Only first-party,
//! shareable keys are written (no frames exist yet, so no other kind does).
//! Written atomically and owner-only ([`write_private_file`]). Read without
//! trust: the file is size-capped, every line is checked, quotas and the
//! origin cap apply as if the entries had been written by script, and what
//! cannot be read is dropped and reported (`profile_file_rejected`).

#![forbid(unsafe_code)]

use origin::{Origin, StorageKey};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Most bytes (keys + values, UTF-8) one storage area may hold — the same
/// order as browsers' ~5 MiB quota. Without it a page could grow the store
/// until the process ran out of memory (hardening pass: 64 MiB stored in
/// under 3 s).
pub const MAX_STORAGE_BYTES: usize = 5 * 1024 * 1024;

/// Most storage keys one profile keeps areas for. A page cannot mint
/// origins, but a long session could accumulate them; past the cap a key
/// gets a private, unshared area instead of growing the map without bound.
pub const MAX_STORAGE_ORIGINS: usize = 512;

/// Largest storage file read back. Far above anything script can write
/// under the quota for a typical profile, and far below what would hurt.
pub const MAX_STORAGE_FILE_BYTES: usize = 64 * 1024 * 1024;

/// The first line of a storage file: its format and version.
pub const FILE_HEADER: &str = "browser-engine local-storage 1";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A write refused because it would take its area past
/// [`MAX_STORAGE_BYTES`]. The area is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaExceeded;

/// One storage area: the key/value map behind `localStorage` for one storage
/// key, or behind one page's `sessionStorage`. A cheap handle: clones are
/// the same area.
///
/// `Arc<Mutex<..>>` (Milestone 30): pages live on their own threads, and two
/// tabs of one origin share one area, as in browsers. The lock recovers
/// from poisoning — a page that panicked mid-write must not take every
/// other page's storage with it (ADR 007).
#[derive(Clone)]
pub struct StorageArea {
    backing: Backing,
}

#[derive(Clone)]
enum Backing {
    /// In this process.
    Local {
        entries: Arc<Mutex<HashMap<String, String>>>,
        /// The owning store's "something changed" flag (`None`: an area no
        /// store persists).
        dirty: Option<Arc<AtomicBool>>,
    },
    /// Milestone 41: in another process, which checks and serves every
    /// operation (a renderer's `localStorage` is the browser process's).
    Remote { areas: Arc<dyn RemoteAreas>, key: StorageKey },
}

/// Storage areas kept by another process — what a renderer's store is
/// (Milestone 41, ADR 025 §4): every operation goes to the browser process,
/// which holds the profile's store, checks that the renderer may reach the
/// key, and answers. A remote that cannot answer reads as empty and refuses
/// writes (as a full quota would).
pub trait RemoteAreas: Send + Sync {
    fn get(&self, key: &StorageKey, name: &str) -> Option<String>;
    fn set(&self, key: &StorageKey, name: String, value: String) -> Result<(), QuotaExceeded>;
    fn remove(&self, key: &StorageKey, name: &str);
    fn clear(&self, key: &StorageKey);
    fn len(&self, key: &StorageKey) -> usize;
}

impl std::fmt::Debug for StorageArea {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.backing {
            Backing::Local { entries, .. } => f.debug_struct("StorageArea").field("entries", &lock(entries).len()).finish(),
            Backing::Remote { key, .. } => f.debug_struct("StorageArea").field("remote", key).finish(),
        }
    }
}

impl StorageArea {
    /// An area of its own: shared with nothing, persisted nowhere (an
    /// opaque origin's `localStorage`, a page's `sessionStorage`).
    pub fn private() -> StorageArea {
        StorageArea::local(None)
    }

    fn local(dirty: Option<Arc<AtomicBool>>) -> StorageArea {
        StorageArea { backing: Backing::Local { entries: Arc::default(), dirty } }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        match &self.backing {
            Backing::Local { entries, .. } => lock(entries).get(key).cloned(),
            Backing::Remote { areas, key: area } => areas.get(area, key),
        }
    }

    /// Stores `value` under `key`, unless that takes the area past
    /// [`MAX_STORAGE_BYTES`] — then nothing changes, the
    /// `storage_quota_exceeded` guard is reported, and `Err` is returned
    /// (script sees a `QuotaExceededError`).
    pub fn set(&self, key: String, value: String) -> Result<(), QuotaExceeded> {
        let (entries, dirty) = match &self.backing {
            Backing::Local { entries, dirty } => (entries, dirty),
            Backing::Remote { areas, key: area } => return areas.set(area, key, value),
        };
        let mut map = lock(entries);
        let replaced = map.get(&key).map_or(0, |old| key.len() + old.len());
        if bytes_of(&map) - replaced + key.len() + value.len() > MAX_STORAGE_BYTES {
            diagnostics::guard(diagnostics::Guard::StorageQuotaExceeded, "storage write refused: quota exceeded");
            return Err(QuotaExceeded);
        }
        map.insert(key, value);
        drop(map);
        touched(dirty);
        Ok(())
    }

    pub fn remove(&self, key: &str) {
        match &self.backing {
            Backing::Local { entries, dirty } => {
                if lock(entries).remove(key).is_some() {
                    touched(dirty);
                }
            }
            Backing::Remote { areas, key: area } => areas.remove(area, key),
        }
    }

    pub fn clear(&self) {
        match &self.backing {
            Backing::Local { entries, dirty } => {
                let mut map = lock(entries);
                if !map.is_empty() {
                    map.clear();
                    drop(map);
                    touched(dirty);
                }
            }
            Backing::Remote { areas, key: area } => areas.clear(area),
        }
    }

    pub fn len(&self) -> usize {
        match &self.backing {
            Backing::Local { entries, .. } => lock(entries).len(),
            Backing::Remote { areas, key: area } => areas.len(area),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keys + values, in bytes — what the quota counts (0 for a remote
    /// area: its quota is kept where it lives).
    pub fn bytes(&self) -> usize {
        match &self.backing {
            Backing::Local { entries, .. } => bytes_of(&lock(entries)),
            Backing::Remote { .. } => 0,
        }
    }

    /// Whether `self` and `other` are the same area (not equal contents).
    pub fn same_area(&self, other: &StorageArea) -> bool {
        match (&self.backing, &other.backing) {
            (Backing::Local { entries: a, .. }, Backing::Local { entries: b, .. }) => Arc::ptr_eq(a, b),
            (Backing::Remote { areas: a, key: x }, Backing::Remote { areas: b, key: y }) => Arc::ptr_eq(a, b) && x == y,
            _ => false,
        }
    }

    fn entries_sorted(&self) -> Vec<(String, String)> {
        let Backing::Local { entries, .. } = &self.backing else { return Vec::new() };
        let mut entries: Vec<(String, String)> = lock(entries).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort();
        entries
    }
}

fn touched(dirty: &Option<Arc<AtomicBool>>) {
    if let Some(dirty) = dirty {
        dirty.store(true, Ordering::Release);
    }
}

fn bytes_of(map: &HashMap<String, String>) -> usize {
    map.iter().map(|(k, v)| k.len() + v.len()).sum()
}

/// What reading a storage file found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// Entries now in the store.
    pub entries: usize,
    /// Lines (or whole files) that were not read, and why — the first few.
    pub rejected: usize,
    pub reasons: Vec<String>,
}

impl LoadReport {
    fn reject(&mut self, reason: impl Into<String>) {
        self.rejected += 1;
        if self.reasons.len() < 8 {
            self.reasons.push(reason.into());
        }
    }
}

/// The profile's `localStorage`, partitioned by storage key: one
/// [`StorageArea`] per key, shared by every page with that key in this
/// profile and by no other. A cheap `Clone` handle, like
/// `network::NetworkContext`.
#[derive(Clone, Default)]
pub struct StorageStore {
    inner: Arc<StoreInner>,
}

#[derive(Default)]
struct StoreInner {
    areas: Mutex<HashMap<StorageKey, StorageArea>>,
    /// Set by any area of this store when it changes; cleared by a flush.
    dirty: Arc<AtomicBool>,
    /// Where [`StorageStore::flush`] writes (`None`: memory only).
    path: Mutex<Option<PathBuf>>,
    /// Milestone 41: the process that keeps this store's areas, if not this
    /// one.
    remote: Option<Arc<dyn RemoteAreas>>,
}

impl StorageStore {
    /// A store that lives as long as its handles do, in memory only.
    pub fn new() -> StorageStore {
        StorageStore::default()
    }

    /// A store whose shareable areas are kept by another process (Milestone
    /// 41: a renderer's, served by the browser process). An opaque origin's
    /// area is still private and local — it is shared with nothing, so there
    /// is nothing to ask for.
    pub fn remote(areas: Arc<dyn RemoteAreas>) -> StorageStore {
        StorageStore { inner: Arc::new(StoreInner { remote: Some(areas), ..StoreInner::default() }) }
    }

    /// A store persisted at `path`: what the file holds is loaded now (a
    /// missing file is a first run), and [`Self::flush`] writes back to it.
    /// A file that cannot be read in part or at all gives what could be
    /// read — never an error for content; `Err` only for I/O.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<(StorageStore, LoadReport)> {
        let path = path.into();
        let store = StorageStore::new();
        let report = match read_capped(&path, MAX_STORAGE_FILE_BYTES)? {
            None => LoadReport::default(),
            Some(Err(too_big)) => {
                let mut report = LoadReport::default();
                report.reject(too_big);
                diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "local storage file over its size cap; starting empty");
                report
            }
            Some(Ok(text)) => store.load_from_str(&text),
        };
        *lock(&store.inner.path) = Some(path);
        store.inner.dirty.store(false, Ordering::Release);
        Ok((store, report))
    }

    /// The area for `key`. A key with an opaque origin never shares: it gets
    /// a fresh private area. Past [`MAX_STORAGE_ORIGINS`] keys, a new key
    /// gets an area of its own that is neither shared nor persisted.
    pub fn area_for(&self, key: &StorageKey) -> StorageArea {
        if !key.is_shareable() {
            return StorageArea::private();
        }
        if let Some(remote) = &self.inner.remote {
            return StorageArea { backing: Backing::Remote { areas: Arc::clone(remote), key: key.clone() } };
        }
        let mut areas = lock(&self.inner.areas);
        if let Some(area) = areas.get(key) {
            return area.clone();
        }
        if areas.len() >= MAX_STORAGE_ORIGINS {
            return StorageArea::private();
        }
        let area = StorageArea::local(Some(Arc::clone(&self.inner.dirty)));
        areas.insert(key.clone(), area.clone());
        area
    }

    /// How many storage keys have an area (diagnostics, tests).
    pub fn origin_count(&self) -> usize {
        lock(&self.inner.areas).len()
    }

    /// Whether something changed since the last flush (or load).
    pub fn is_dirty(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire)
    }

    /// Writes the store to its file if anything changed since the last
    /// write. `Ok(false)`: nothing to do (memory only, or unchanged).
    pub fn flush(&self) -> std::io::Result<bool> {
        let Some(path) = lock(&self.inner.path).clone() else { return Ok(false) };
        if !self.inner.dirty.swap(false, Ordering::AcqRel) {
            return Ok(false);
        }
        let text = self.serialize();
        if let Err(e) = write_private_file(&path, text.as_bytes()) {
            // Still unwritten: try again at the next flush.
            self.inner.dirty.store(true, Ordering::Release);
            return Err(e);
        }
        Ok(true)
    }

    /// The store in the file format (deterministic: sorted).
    pub fn serialize(&self) -> String {
        let areas: Vec<(String, StorageArea)> = lock(&self.inner.areas)
            .iter()
            .filter(|(key, _)| key.is_shareable() && key.top_level_site == key.origin.site())
            .map(|(key, area)| (key.origin.serialize(), area.clone()))
            .collect();
        let mut lines: Vec<String> = Vec::new();
        for (origin, area) in areas {
            for (k, v) in area.entries_sorted() {
                lines.push(format!("{origin}\t{}\t{}", escape(&k), escape(&v)));
            }
        }
        lines.sort();
        let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum::<usize>() + FILE_HEADER.len() + 1);
        out.push_str(FILE_HEADER);
        out.push('\n');
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Reads entries from `text` (the file format) into this store, as if
    /// script had written them: quotas and the origin cap apply, and a line
    /// that is not exactly well formed is dropped. A wrong or missing header
    /// rejects the whole text. Never panics, whatever `text` is.
    pub fn load_from_str(&self, text: &str) -> LoadReport {
        let mut report = LoadReport::default();
        let mut lines = text.split('\n');
        if lines.next().map(|l| l.trim_end_matches('\r')) != Some(FILE_HEADER) {
            report.reject("not a local-storage file of a version this build reads");
            diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "local storage file has an unknown format; starting empty");
            return report;
        }
        for (number, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(origin), Some(key), Some(value), None) = (fields.next(), fields.next(), fields.next(), fields.next()) else {
                report.reject(format!("line {}: not three fields", number + 2));
                continue;
            };
            let parsed = Origin::parse(origin);
            if parsed.is_opaque() || parsed.serialize() != origin {
                report.reject(format!("line {}: not a serialised origin", number + 2));
                continue;
            }
            let (Some(key), Some(value)) = (unescape(key), unescape(value)) else {
                report.reject(format!("line {}: bad escape", number + 2));
                continue;
            };
            let storage_key = StorageKey::first_party(&parsed);
            let area = {
                let areas = lock(&self.inner.areas);
                areas.get(&storage_key).cloned()
            };
            let area = match area {
                Some(area) => area,
                None => {
                    if self.origin_count() >= MAX_STORAGE_ORIGINS {
                        report.reject(format!("line {}: more origins than the store keeps", number + 2));
                        continue;
                    }
                    self.area_for(&storage_key)
                }
            };
            match area.set(key, value) {
                Ok(()) => report.entries += 1,
                Err(QuotaExceeded) => report.reject(format!("line {}: over the area's quota", number + 2)),
            }
        }
        if report.rejected > 0 {
            diagnostics::guard(diagnostics::Guard::ProfileFileRejected, "local storage file had lines that could not be read; they were dropped");
        }
        report
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            if c == '\r' {
                return None;
            }
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// Reads `path` as UTF-8 text of at most `cap` bytes. `Ok(None)`: no such
/// file. `Ok(Some(Err(_)))`: too big, or not UTF-8 (a content problem, not
/// an I/O one). Reads at most `cap + 1` bytes whatever the file's size.
pub fn read_capped(path: &Path, cap: usize) -> std::io::Result<Option<Result<String, String>>> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Ok(Some(Err(format!("{} is larger than {cap} bytes", path.display()))));
    }
    Ok(Some(String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))))
}

/// Writes `bytes` to `path` atomically (a sibling temporary file, then a
/// rename), so a crash mid-write never leaves a truncated file, and
/// owner-only (`0600` on Unix): profile files hold what sites stored.
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(url: &str) -> StorageKey {
        StorageKey::first_party(&Origin::parse(url))
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("be-storage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("local-storage")
    }

    #[test]
    fn one_area_per_origin_shared_by_handles_and_by_no_other_origin() {
        let store = StorageStore::new();
        let a1 = store.area_for(&key("https://a.example/x"));
        let a2 = store.area_for(&key("https://a.example/y"));
        let b = store.area_for(&key("https://b.example/"));
        assert!(a1.same_area(&a2));
        assert!(!a1.same_area(&b));
        a1.set("k".into(), "v".into()).unwrap();
        assert_eq!(a2.get("k").as_deref(), Some("v"));
        assert_eq!(b.get("k"), None);
    }

    /// Milestone 37 (ADR 019 §2): storage is keyed by (top-level site,
    /// origin), so one third party embedded under two sites has two areas.
    #[test]
    fn a_third_party_under_two_sites_gets_two_areas() {
        let store = StorageStore::new();
        let tracker = Origin::parse("https://tracker.example/");
        let under_a = store.area_for(&StorageKey::embedded(&tracker, &Origin::parse("https://a.example/")));
        let under_b = store.area_for(&StorageKey::embedded(&tracker, &Origin::parse("https://b.example/")));
        under_a.set("id".into(), "visitor-1".into()).unwrap();
        assert!(!under_a.same_area(&under_b));
        assert_eq!(under_b.get("id"), None, "site B cannot read what the tracker stored under site A");
        assert_eq!(store.area_for(&key("https://tracker.example/")).get("id"), None, "nor can the tracker's own top-level pages");
    }

    #[test]
    fn opaque_origins_never_share_and_are_never_stored() {
        let store = StorageStore::new();
        let opaque = StorageKey::first_party(&Origin::new_opaque());
        let one = store.area_for(&opaque);
        let two = store.area_for(&opaque);
        assert!(!one.same_area(&two));
        assert_eq!(store.origin_count(), 0);
    }

    #[test]
    fn the_quota_refuses_a_write_and_leaves_the_area_unchanged() {
        let area = StorageArea::private();
        area.set("a".into(), "x".repeat(MAX_STORAGE_BYTES - 1)).unwrap();
        assert_eq!(area.set("b".into(), "yy".into()), Err(QuotaExceeded));
        assert_eq!(area.get("b"), None);
        // Replacing a value counts the old one out.
        area.set("a".into(), "small".into()).unwrap();
        area.set("b".into(), "yy".into()).unwrap();
    }

    #[test]
    fn past_the_origin_cap_a_key_gets_a_private_area() {
        let store = StorageStore::new();
        for i in 0..MAX_STORAGE_ORIGINS {
            store.area_for(&key(&format!("https://o{i}.example/")));
        }
        let extra = key("https://one-too-many.example/");
        let a = store.area_for(&extra);
        let b = store.area_for(&extra);
        assert!(!a.same_area(&b));
        assert_eq!(store.origin_count(), MAX_STORAGE_ORIGINS);
    }

    #[test]
    fn a_store_round_trips_through_its_file() {
        let path = temp_path("roundtrip");
        {
            let (store, report) = StorageStore::open(&path).unwrap();
            assert_eq!(report, LoadReport::default(), "no file yet: a first run");
            store.area_for(&key("https://a.example/")).set("tab\tand\nnewline\\".into(), "v\r1".into()).unwrap();
            store.area_for(&key("http://b.example:8080/")).set("k".into(), "ü — unicode".into()).unwrap();
            store.area_for(&StorageKey::first_party(&Origin::new_opaque())).set("never".into(), "written".into()).unwrap();
            assert!(store.is_dirty());
            assert!(store.flush().unwrap());
            assert!(!store.flush().unwrap(), "nothing changed since: nothing written");
        }
        let (store, report) = StorageStore::open(&path).unwrap();
        assert_eq!(report.entries, 2);
        assert_eq!(report.rejected, 0);
        assert_eq!(store.area_for(&key("https://a.example/")).get("tab\tand\nnewline\\").as_deref(), Some("v\r1"));
        assert_eq!(store.area_for(&key("http://b.example:8080/")).get("k").as_deref(), Some("ü — unicode"));
        assert!(!store.is_dirty(), "loading is not a change");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn a_hostile_file_loads_what_is_well_formed_and_nothing_else() {
        let store = StorageStore::new();
        let text = format!(
            "{FILE_HEADER}\nhttps://ok.example\tk\tv\nnot an origin\tk\tv\nhttps://a.example/path\tk\tv\nnull\tk\tv\n\
             https://ok.example\tbad\\q\tv\nhttps://ok.example\ttoo\tmany\tfields\nhttps://ok.example\tonly-two\n\
             https://ok.example\tbig\t{}\n",
            "x".repeat(MAX_STORAGE_BYTES)
        );
        let report = store.load_from_str(&text);
        assert_eq!(report.entries, 1);
        assert_eq!(report.rejected, 7);
        assert_eq!(store.area_for(&key("https://ok.example/")).get("k").as_deref(), Some("v"));
        assert_eq!(store.origin_count(), 1);
    }

    #[test]
    fn an_unknown_version_or_garbage_loads_nothing() {
        for text in ["browser-engine local-storage 2\nhttps://a.example\tk\tv\n", "", "\u{0}\u{1}garbage", "https://a.example\tk\tv\n"] {
            let store = StorageStore::new();
            let report = store.load_from_str(text);
            assert_eq!(report.entries, 0, "{text:?}");
            assert_eq!(report.rejected, 1, "{text:?}");
            assert_eq!(store.origin_count(), 0);
        }
    }

    #[test]
    fn a_file_over_the_cap_is_not_read() {
        let path = temp_path("oversized");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_STORAGE_FILE_BYTES as u64 + 10).unwrap();
        let (store, report) = StorageStore::open(&path).unwrap();
        assert_eq!(report.rejected, 1);
        assert_eq!(store.origin_count(), 0);
    }

    #[test]
    fn loading_a_file_with_many_origins_stops_at_the_cap() {
        let mut text = format!("{FILE_HEADER}\n");
        for i in 0..MAX_STORAGE_ORIGINS + 50 {
            text.push_str(&format!("https://o{i}.example\tk\tv\n"));
        }
        let store = StorageStore::new();
        let report = store.load_from_str(&text);
        assert_eq!(report.entries, MAX_STORAGE_ORIGINS);
        assert_eq!(report.rejected, 50);
    }

    #[test]
    fn escaping_round_trips_every_special_character() {
        for s in ["", "\\", "\\t", "\t\n\r\\", "a\\\\b", "plain"] {
            assert_eq!(unescape(&escape(s)).as_deref(), Some(s));
        }
        assert_eq!(unescape("trailing\\"), None);
        assert_eq!(unescape("\\x"), None);
    }

    /// Milestone 41: a remote store's shareable areas are the other
    /// process's — every operation reaches it — while an opaque origin's
    /// area stays private and local.
    #[test]
    fn a_remote_stores_areas_are_kept_by_the_remote() {
        struct Remote(StorageStore, std::sync::atomic::AtomicUsize);
        impl RemoteAreas for Remote {
            fn get(&self, key: &StorageKey, name: &str) -> Option<String> {
                self.1.fetch_add(1, Ordering::Relaxed);
                self.0.area_for(key).get(name)
            }
            fn set(&self, key: &StorageKey, name: String, value: String) -> Result<(), QuotaExceeded> {
                self.1.fetch_add(1, Ordering::Relaxed);
                self.0.area_for(key).set(name, value)
            }
            fn remove(&self, key: &StorageKey, name: &str) {
                self.0.area_for(key).remove(name)
            }
            fn clear(&self, key: &StorageKey) {
                self.0.area_for(key).clear()
            }
            fn len(&self, key: &StorageKey) -> usize {
                self.0.area_for(key).len()
            }
        }
        let profile = StorageStore::new();
        let remote = Arc::new(Remote(profile.clone(), Default::default()));
        let renderer = StorageStore::remote(remote.clone());
        let key = StorageKey::first_party(&Origin::parse("https://a.example/"));
        renderer.area_for(&key).set("k".into(), "v".into()).unwrap();
        assert_eq!(profile.area_for(&key).get("k").as_deref(), Some("v"), "written where the remote keeps it");
        assert_eq!(renderer.area_for(&key).get("k").as_deref(), Some("v"));
        assert_eq!(renderer.area_for(&key).len(), 1);
        assert_eq!(renderer.origin_count(), 0, "nothing kept locally");
        assert!(renderer.area_for(&key).same_area(&renderer.area_for(&key)));
        let opaque = StorageKey::first_party(&Origin::new_opaque());
        renderer.area_for(&opaque).set("x".into(), "y".into()).unwrap();
        assert_eq!(remote.1.load(Ordering::Relaxed), 2, "one set and one get; an opaque origin's area never goes to the remote");
        renderer.area_for(&key).clear();
        assert!(profile.area_for(&key).is_empty());
    }
}
