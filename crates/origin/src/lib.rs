//! Origins and sites — Milestone 28 (`docs/adr/004-origin-and-site-types.md`).
//!
//! Every security or privacy rule that asks "who is this?" asks it through
//! this crate:
//!
//! - [`Origin`] — scheme, host and port (a *tuple* origin), or an *opaque*
//!   origin that equals nothing but itself. The unit of the same-origin
//!   policy and of storage.
//! - [`Site`] — the *schemeful* registrable domain of an origin
//!   (`https://example.co.uk` for `https://www.example.co.uk:8443`). The
//!   unit of `SameSite` cookies and HTTP-cache partitioning.
//! - [`StorageKey`] — which storage area a document uses: its origin, under
//!   the site of its top-level document (frames-ready; today the two sites
//!   are always the same).
//! - [`cors_response_readable`] — the one rule for whether script on one
//!   origin may read a response from another.
//!
//! Parsing is done by the `url` crate (WHATWG URL parsing, IDNA/punycode
//! host normalisation), but no `url` type appears in this crate's public
//! API, so the parser stays replaceable. The Public Suffix List lives in
//! [`psl`] (ADR 002).
//!
//! Zero-trust: [`Origin::parse`] takes arbitrary, hostile strings (every
//! `href`, `src`, redirect `Location`, and `fetch()` argument). It never
//! panics, does bounded work (input over [`MAX_URL_BYTES`] is opaque
//! without being parsed), and is fuzzed (`fuzz_targets/origin_parse.rs`).

#![forbid(unsafe_code)]

pub mod psl;

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};

/// Longest URL string [`Origin::parse`] will parse. Longer input gets an
/// opaque origin without any parsing work. Browsers cap URLs at about this
/// size (2 MiB) too; nothing legitimate is longer.
pub const MAX_URL_BYTES: usize = 2 * 1024 * 1024;

/// Hands out opaque-origin identities. Starts at 1; only ever increments.
/// A plain counter, not a random value: an opaque origin's id never leaves
/// the process (it serialises as `null`), so it needs uniqueness, not
/// unpredictability.
static NEXT_OPAQUE_ID: AtomicU64 = AtomicU64::new(1);

/// The identity of one opaque origin. Unique per process: an opaque origin
/// is only ever equal to itself (and its clones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpaqueId(u64);

impl OpaqueId {
    fn fresh() -> Self {
        OpaqueId(NEXT_OPAQUE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// The schemes that have tuple origins. (`ftp` also does in the URL
/// standard; the engine cannot fetch it, so it is opaque here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scheme {
    Http,
    Https,
    Ws,
    Wss,
}

impl Scheme {
    fn parse(scheme: &str) -> Option<Scheme> {
        match scheme {
            "http" => Some(Scheme::Http),
            "https" => Some(Scheme::Https),
            "ws" => Some(Scheme::Ws),
            "wss" => Some(Scheme::Wss),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::Ws => "ws",
            Scheme::Wss => "wss",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http | Scheme::Ws => 80,
            Scheme::Https | Scheme::Wss => 443,
        }
    }

    /// `https` or `wss`: the connection is encrypted and authenticated.
    pub fn is_secure(self) -> bool {
        matches!(self, Scheme::Https | Scheme::Wss)
    }
}

/// A normalised host: a lowercase ASCII (punycode) domain, or an address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Host {
    Domain(String),
    Ipv4(Ipv4Addr),
    Ipv6(Ipv6Addr),
}

impl Host {
    fn from_url(host: url::Host<&str>) -> Host {
        match host {
            // The URL parser has already lowercased and IDNA-encoded it.
            url::Host::Domain(d) => Host::Domain(d.to_string()),
            url::Host::Ipv4(a) => Host::Ipv4(a),
            url::Host::Ipv6(a) => Host::Ipv6(a),
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Host::Domain(d) => f.write_str(d),
            Host::Ipv4(a) => write!(f, "{a}"),
            Host::Ipv6(a) => write!(f, "[{a}]"),
        }
    }
}

/// An origin. Two origins are the same origin exactly when they are `==`:
/// tuple origins compare scheme, host and *effective* port (default ports
/// filled in); an opaque origin equals only itself and its clones.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Origin {
    Tuple { scheme: Scheme, host: Host, port: u16 },
    Opaque(OpaqueId),
}

impl Origin {
    /// A fresh opaque origin, distinct from every other origin — what a
    /// document with no URL (the built-in page, a synthetic document) gets.
    pub fn new_opaque() -> Origin {
        Origin::Opaque(OpaqueId::fresh())
    }

    /// The origin of `url`. Never fails: unparseable input, input over
    /// [`MAX_URL_BYTES`], and every scheme without a tuple origin (`data:`,
    /// `file:`, `about:`, `javascript:` …) give a fresh opaque origin. A
    /// `blob:` URL has the origin of the URL it wraps.
    pub fn parse(url: &str) -> Origin {
        if url.len() > MAX_URL_BYTES {
            return Origin::new_opaque();
        }
        match url::Url::parse(url) {
            Ok(parsed) => Self::of_parsed(&parsed, true),
            Err(_) => Origin::new_opaque(),
        }
    }

    fn of_parsed(url: &url::Url, allow_blob: bool) -> Origin {
        if allow_blob && url.scheme() == "blob" {
            // `blob:https://example.com/uuid` — the inner URL's origin; a
            // blob inside a blob is not a thing, so one level only.
            return match url::Url::parse(url.path()) {
                Ok(inner) => Self::of_parsed(&inner, false),
                Err(_) => Origin::new_opaque(),
            };
        }
        let (Some(scheme), Some(host), Some(port)) = (Scheme::parse(url.scheme()), url.host(), url.port_or_known_default())
        else {
            return Origin::new_opaque();
        };
        Origin::Tuple { scheme, host: Host::from_url(host), port }
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self, Origin::Opaque(_))
    }

    /// The same-origin rule. (Equivalent to `==`; named for readability at
    /// policy call sites.)
    pub fn same_origin(&self, other: &Origin) -> bool {
        self == other
    }

    /// Whether `url` is same-origin with this origin. An unparseable `url`
    /// is never same-origin with anything.
    pub fn same_origin_as_url(&self, url: &str) -> bool {
        self.same_origin(&Origin::parse(url))
    }

    pub fn scheme(&self) -> Option<Scheme> {
        match self {
            Origin::Tuple { scheme, .. } => Some(*scheme),
            Origin::Opaque(_) => None,
        }
    }

    pub fn host(&self) -> Option<&Host> {
        match self {
            Origin::Tuple { host, .. } => Some(host),
            Origin::Opaque(_) => None,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            Origin::Tuple { port, .. } => Some(*port),
            Origin::Opaque(_) => None,
        }
    }

    /// The ASCII serialisation: `https://example.com`,
    /// `http://127.0.0.1:8080`, `http://[::1]`, or `null` for an opaque
    /// origin. The port is omitted when it is the scheme's default. This is
    /// what `Access-Control-Allow-Origin` is compared against.
    pub fn serialize(&self) -> String {
        match self {
            Origin::Tuple { scheme, host, port } if *port == scheme.default_port() => format!("{}://{host}", scheme.as_str()),
            Origin::Tuple { scheme, host, port } => format!("{}://{host}:{port}", scheme.as_str()),
            Origin::Opaque(_) => "null".to_string(),
        }
    }

    /// This origin's site: `(scheme, registrable domain)`, or opaque.
    pub fn site(&self) -> Site {
        match self {
            Origin::Tuple { scheme, host, .. } => {
                let domain = match host {
                    Host::Domain(d) => psl::registrable_domain_or_host(d),
                    Host::Ipv4(_) | Host::Ipv6(_) => host.to_string(),
                };
                Site::Tuple { scheme: *scheme, domain }
            }
            Origin::Opaque(id) => Site::Opaque(*id),
        }
    }

    /// The same-site rule: equal [`Site`]s. An opaque origin is same-site
    /// only with itself.
    pub fn same_site(&self, other: &Origin) -> bool {
        self.site() == other.site()
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.serialize())
    }
}

/// A schemeful site: the scheme and registrable domain of a tuple origin
/// (an IP address, or a host that is itself a public suffix such as
/// `localhost`, is its own "domain"), or the opaque origin itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Site {
    Tuple { scheme: Scheme, domain: String },
    Opaque(OpaqueId),
}

impl Site {
    /// The site of `url` — shorthand for `Origin::parse(url).site()`.
    pub fn of_url(url: &str) -> Site {
        Origin::parse(url).site()
    }

    pub fn is_opaque(&self) -> bool {
        matches!(self, Site::Opaque(_))
    }

    /// A string that identifies this site and nothing else, for use as a
    /// map key (the HTTP cache's partition). Tuple sites serialise as
    /// `https://example.com`; each opaque site gets its own key, so opaque
    /// documents never share a partition with anything.
    pub fn partition_key(&self) -> String {
        match self {
            Site::Tuple { scheme, domain } => format!("{}://{domain}", scheme.as_str()),
            Site::Opaque(OpaqueId(id)) => format!("opaque#{id}"),
        }
    }
}

impl fmt::Display for Site {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Site::Tuple { .. } => f.write_str(&self.partition_key()),
            Site::Opaque(_) => f.write_str("null"),
        }
    }
}

/// Which storage area (`localStorage`, later IndexedDB and the Cache API) a
/// document uses: its own origin, **under the site of the top-level
/// document** it is displayed in. The second half is what keeps a third
/// party embedded on two different sites from sharing one storage area
/// between them once frames exist; today every document is top-level, so
/// `top_level_site == origin.site()` ([`StorageKey::first_party`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorageKey {
    pub top_level_site: Site,
    pub origin: Origin,
}

impl StorageKey {
    /// The key for a top-level document of `origin`.
    pub fn first_party(origin: &Origin) -> StorageKey {
        StorageKey { top_level_site: origin.site(), origin: origin.clone() }
    }

    /// The key for a document of `origin` embedded under a top-level
    /// document of `top_level` (frames — not built yet).
    pub fn embedded(origin: &Origin, top_level: &Origin) -> StorageKey {
        StorageKey { top_level_site: top_level.site(), origin: origin.clone() }
    }

    /// Opaque origins never get storage shared with any other document.
    pub fn is_shareable(&self) -> bool {
        !self.origin.is_opaque() && !self.top_level_site.is_opaque()
    }
}

/// Whether script running in `reader` may read a response that came from
/// `response_origin` — the single same-origin/CORS read rule (ADR 004).
///
/// - Same origin: always readable.
/// - Cross-origin, without credentials: readable if
///   `Access-Control-Allow-Origin` is `*` or exactly `reader`'s serialised
///   origin.
/// - Cross-origin, *with* credentials (cookies were sent): `*` is not
///   enough — the header must name `reader` exactly and
///   `Access-Control-Allow-Credentials` must be `true`.
/// - `Access-Control-Allow-Origin: null` never grants access: every opaque
///   origin serialises to `null`, so honouring it would let any sandboxed or
///   `data:` document read the response.
pub fn cors_response_readable(
    reader: &Origin,
    response_origin: &Origin,
    credentials_sent: bool,
    allow_origin: Option<&str>,
    allow_credentials: Option<&str>,
) -> bool {
    if reader.same_origin(response_origin) {
        return true;
    }
    let Some(allowed) = allow_origin.map(str::trim) else { return false };
    if allowed == "null" {
        return false;
    }
    if credentials_sent {
        return !reader.is_opaque() && allowed == reader.serialize() && allow_credentials.map(str::trim) == Some("true");
    }
    allowed == "*" || (!reader.is_opaque() && allowed == reader.serialize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(url: &str) -> Origin {
        Origin::parse(url)
    }

    #[test]
    fn tuple_origins_ignore_path_query_fragment_and_userinfo() {
        assert_eq!(o("https://example.com/a/b?x=1#f"), o("https://example.com/"));
        assert_eq!(o("https://user:pw@example.com/"), o("https://example.com"));
        assert_eq!(o("https://example.com/").serialize(), "https://example.com");
    }

    #[test]
    fn scheme_host_and_port_each_distinguish_origins() {
        assert_ne!(o("https://example.com/"), o("http://example.com/"));
        assert_ne!(o("https://example.com/"), o("https://www.example.com/"));
        assert_ne!(o("https://example.com/"), o("https://example.com:8443/"));
        assert_ne!(o("http://example.com/"), o("ws://example.com/"), "ws and http are different schemes");
    }

    #[test]
    fn default_ports_are_normalised() {
        assert_eq!(o("https://example.com:443/"), o("https://example.com/"));
        assert_eq!(o("http://example.com:80/"), o("http://example.com/"));
        assert_eq!(o("wss://example.com:443/"), o("wss://example.com/"));
        assert_eq!(o("https://example.com:443/").serialize(), "https://example.com");
        assert_eq!(o("http://example.com:8080/").serialize(), "http://example.com:8080");
        assert_eq!(o("http://example.com:8080/").port(), Some(8080));
        assert_eq!(o("https://example.com/").port(), Some(443));
    }

    #[test]
    fn hosts_are_case_normalised_but_trailing_dots_are_kept() {
        assert_eq!(o("HTTPS://EXAMPLE.com/"), o("https://example.com/"));
        // As in browsers: `example.com.` is a different origin (the URL
        // parser keeps the dot) but the same site.
        assert_ne!(o("https://example.com./"), o("https://example.com/"));
        assert!(o("https://example.com./").same_site(&o("https://example.com/")));
    }

    #[test]
    fn idn_hosts_are_punycode_and_homographs_are_different_origins() {
        let unicode = o("https://bücher.example/");
        assert_eq!(unicode.serialize(), "https://xn--bcher-kva.example");
        assert_eq!(unicode, o("https://xn--bcher-kva.example/"), "Unicode and punycode spellings are one origin");
        // Cyrillic "а" (U+0430) in place of Latin "a": looks the same, is not.
        let homograph = o("https://\u{0430}pple.com/");
        assert_ne!(homograph, o("https://apple.com/"));
        assert!(!homograph.same_site(&o("https://apple.com/")));
        assert!(homograph.serialize().contains("xn--"), "{}", homograph.serialize());
    }

    #[test]
    fn userinfo_and_backslash_tricks_resolve_to_the_real_host() {
        // The part before `@` is credentials, not the host.
        assert_eq!(o("https://good.com@evil.com/"), o("https://evil.com/"));
        // For special schemes a backslash is a path separator: the host is
        // `evil.com`, and `@good.com` is path.
        assert_eq!(o("https://evil.com\\@good.com/"), o("https://evil.com/"));
        assert_eq!(o("https://evil.com\\.good.com/").host(), Some(&Host::Domain("evil.com".into())));
    }

    #[test]
    fn ip_literals() {
        assert_eq!(o("http://127.0.0.1:8080/").host(), Some(&Host::Ipv4(Ipv4Addr::new(127, 0, 0, 1))));
        assert_eq!(o("http://[::1]/").serialize(), "http://[::1]");
        assert_eq!(o("http://[0:0:0:0:0:0:0:1]/"), o("http://[::1]/"), "IPv6 spellings normalise");
        assert_eq!(o("http://0x7f.1/"), o("http://127.0.0.1/"), "IPv4 spellings normalise");
        assert_eq!(o("http://[::1]:8080/").site(), o("http://[::1]/").site(), "an address is its own site");
        assert_ne!(o("http://127.0.0.1/").site(), o("http://127.0.0.2/").site());
    }

    #[test]
    fn opaque_origins_equal_only_themselves() {
        let a = o("data:text/html,hi");
        let b = o("data:text/html,hi");
        assert!(a.is_opaque() && b.is_opaque());
        assert_ne!(a, b, "two data: documents are never same-origin, even with identical URLs");
        assert_eq!(a, a.clone(), "an origin is the same as itself");
        assert_eq!(a.serialize(), "null");
        assert_ne!(a.site(), b.site());
        assert!(!a.same_site(&b));
        assert_ne!(Origin::new_opaque(), Origin::new_opaque());
    }

    #[test]
    fn file_about_javascript_and_garbage_are_opaque() {
        for url in [
            "file:///etc/passwd",
            "file:///home/user/a.html",
            "about:blank",
            "javascript:alert(1)",
            "mailto:a@b.c",
            "ftp://example.com/",
            "not a url",
            "",
            "https://",
            "http://[::1/",
            "http://exa mple.com/",
        ] {
            assert!(o(url).is_opaque(), "{url:?} should be opaque");
        }
        // Two local files are not same-origin with each other.
        assert_ne!(o("file:///a.html"), o("file:///a.html"));
    }

    #[test]
    fn blob_urls_take_the_inner_origin() {
        assert_eq!(o("blob:https://example.com/0b1c-uuid"), o("https://example.com/"));
        assert!(o("blob:data:text/plain,x").is_opaque());
        assert!(o("blob:blob:https://example.com/x").is_opaque(), "nested blobs are not a thing");
        assert!(o("blob:garbage").is_opaque());
    }

    #[test]
    fn sites_are_schemeful_registrable_domains() {
        let site = o("https://www.example.co.uk:8443/x").site();
        assert_eq!(site, Site::Tuple { scheme: Scheme::Https, domain: "example.co.uk".into() });
        assert_eq!(site.partition_key(), "https://example.co.uk");
        assert!(o("https://a.example.com/").same_site(&o("https://b.example.com/")));
        assert!(!o("https://a.example.com/").same_origin(&o("https://b.example.com/")));
        assert!(!o("http://example.com/").same_site(&o("https://example.com/")), "schemeful: http is a different site");
        assert!(!o("https://alice.github.io/").same_site(&o("https://bob.github.io/")), "private-section suffixes");
        assert_eq!(o("http://localhost:1234/").site(), o("http://localhost/").site());
        assert_ne!(o("http://localhost/").site(), o("http://127.0.0.1/").site());
    }

    #[test]
    fn site_partition_keys_never_collide_between_opaque_and_tuple() {
        let opaque = o("data:,x").site();
        assert!(opaque.partition_key().starts_with("opaque#"));
        assert_ne!(opaque.partition_key(), o("data:,x").site().partition_key());
        assert_eq!(opaque.to_string(), "null");
    }

    #[test]
    fn storage_keys() {
        let a = o("https://a.example.com/");
        assert_eq!(StorageKey::first_party(&a), StorageKey::first_party(&o("https://a.example.com/other")));
        assert_ne!(StorageKey::first_party(&a), StorageKey::first_party(&o("https://b.example.com/")), "same site, different origin");
        assert!(StorageKey::first_party(&a).is_shareable());
        assert!(!StorageKey::first_party(&Origin::new_opaque()).is_shareable());
        // Frames-ready: the same third party under two top-level sites is two keys.
        let third = o("https://tracker.example/");
        assert_ne!(StorageKey::embedded(&third, &o("https://news.test/")), StorageKey::embedded(&third, &o("https://shop.test/")));
    }

    #[test]
    fn cors_read_rule() {
        let page = o("https://app.example/");
        let api = o("https://api.other/");
        // Same origin: always.
        assert!(cors_response_readable(&page, &o("https://app.example/x"), true, None, None));
        // Cross-origin, no header: never.
        assert!(!cors_response_readable(&page, &api, false, None, None));
        // `*` without credentials; exact origin; wrong origin; case matters.
        assert!(cors_response_readable(&page, &api, false, Some("*"), None));
        assert!(cors_response_readable(&page, &api, false, Some(" https://app.example "), None));
        assert!(!cors_response_readable(&page, &api, false, Some("https://evil.example"), None));
        assert!(!cors_response_readable(&page, &api, false, Some("https://app.example/"), None), "an origin has no path");
        assert!(!cors_response_readable(&page, &api, false, Some("HTTPS://APP.EXAMPLE"), None));
        // With credentials: `*` is not enough; ACAC must be `true`.
        assert!(!cors_response_readable(&page, &api, true, Some("*"), Some("true")));
        assert!(!cors_response_readable(&page, &api, true, Some("https://app.example"), None));
        assert!(cors_response_readable(&page, &api, true, Some("https://app.example"), Some("true")));
    }

    #[test]
    fn cors_null_never_grants_and_opaque_readers_need_a_wildcard() {
        let opaque = Origin::new_opaque();
        let api = o("https://api.other/");
        assert!(!cors_response_readable(&opaque, &api, false, Some("null"), None));
        assert!(!cors_response_readable(&o("https://app.example/"), &api, false, Some("null"), None));
        assert!(cors_response_readable(&opaque, &api, false, Some("*"), None));
        assert!(!cors_response_readable(&opaque, &api, true, Some("null"), Some("true")));
        // An opaque reader is cross-origin even to another opaque response.
        assert!(!cors_response_readable(&opaque, &Origin::new_opaque(), false, None, None));
        assert!(cors_response_readable(&opaque, &opaque.clone(), false, None, None));
    }

    #[test]
    fn hostile_input_is_bounded_and_never_panics() {
        let started = std::time::Instant::now();
        let long = format!("https://{}.com/", "a".repeat(MAX_URL_BYTES));
        assert!(o(&long).is_opaque(), "over the cap: opaque without parsing");
        let many_labels = format!("https://{}com/", "a.".repeat(100_000));
        let _ = o(&many_labels).site(); // parseable, but no suffix work past DNS limits
        for s in ["\0", "https://\u{202e}moc.live/", "http://%00/", "http://a%2eb/", "https://[::ffff:1.2.3.4]/", "https://x:99999/", "https://x:-1/"] {
            let origin = o(s);
            let _ = (origin.serialize(), origin.site(), origin.same_site(&origin));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
