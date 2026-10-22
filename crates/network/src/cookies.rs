//! Milestone 9: `Set-Cookie` parsing and a session (in-memory only —
//! no disk persistence, see milestones/milestones-01-17.md)
//! cookie jar. Pure logic, no networking — `DirectRoute` in `lib.rs`
//! is what actually wires this into requests/responses.

use origin::{Scheme, Site};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Always stored without a leading dot, lowercase — the leading
    /// dot some servers send in `Domain=.example.com` is a legacy
    /// convention with no semantic difference in modern cookie
    /// matching (RFC 6265), so it's normalized away at parse time
    /// rather than carried through to every comparison.
    pub domain: String,
    pub path: String,
    /// Unix seconds. `None` = session cookie (no expiry while this
    /// jar is alive — matches "in-memory only" scope; there's no
    /// separate "persist across restarts" concept to distinguish
    /// since nothing here survives a restart anyway).
    pub expires: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    /// True when the server sent no `Domain=`: the cookie belongs to
    /// exactly the host that set it and is NOT sent to its subdomains
    /// (RFC 6265 §5.3). `false` (a `Domain=` was honored) also matches
    /// subdomains of `domain`.
    pub host_only: bool,
    pub same_site: SameSite,
}

/// A cookie's `SameSite` attribute: whether it may be sent on requests
/// initiated from a *different site*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SameSite {
    /// No attribute. Enforced as `Lax` — the modern browser default.
    #[default]
    Unspecified,
    /// Sent on cross-site requests too. Only accepted together with `Secure`.
    None,
    /// Sent cross-site only on top-level navigations.
    Lax,
    /// Never sent cross-site.
    Strict,
}

impl SameSite {
    fn name(self) -> &'static str {
        match self {
            SameSite::Unspecified => "unspecified",
            SameSite::None => "none",
            SameSite::Lax => "lax",
            SameSite::Strict => "strict",
        }
    }
    fn from_name(s: &str) -> SameSite {
        match s.to_ascii_lowercase().as_str() {
            "none" => SameSite::None,
            "lax" => SameSite::Lax,
            "strict" => SameSite::Strict,
            _ => SameSite::Unspecified,
        }
    }
}

/// Everything the jar needs to decide which cookies ride on ONE request.
pub struct SendContext<'a> {
    /// The request's target host, path, and whether it is `https`.
    pub host: &'a str,
    pub path: &'a str,
    pub is_secure: bool,
    /// The target's (schemeful) site — `origin::Site`.
    pub request_site: &'a Site,
    /// The site of the document that initiated the request; `None` means
    /// browser-initiated (a typed URL, the startup page), which is never
    /// cross-site. An opaque initiator's site equals no request's site, so
    /// its requests are always cross-site.
    pub initiator_site: Option<&'a Site>,
    /// A top-level navigation (typing a URL, clicking a link) rather than
    /// a subresource or script request. `SameSite=Lax` cookies ride on
    /// these even cross-site.
    pub is_navigation: bool,
}

/// Performance fix: cookies are bucketed by their own exact `domain`
/// (`HashMap<domain, Vec<Cookie>>`), not a single flat `Vec` scanned in
/// full on every store/request. `header_for` — called on EVERY outgoing
/// request — used to filter every cookie in the jar regardless of
/// domain; with this, it instead does one O(1) map lookup per ancestor
/// -domain suffix of the request domain (see `domain_suffixes`), so
/// cost scales with how many labels are in the domain being requested
/// (typically 2-4), not with how many unrelated-domain cookies a long
/// session has accumulated.
#[derive(Debug, Default)]
pub struct CookieJar {
    cookies: HashMap<String, Vec<Cookie>>,
}

impl CookieJar {
    pub fn new() -> Self {
        CookieJar::default()
    }

    /// Parses one `Set-Cookie` header value and stores it, replacing
    /// any existing cookie with the same name+domain+path (matches
    /// RFC 6265 §5.3's overwrite rule). `request_domain` is the
    /// domain of the response that sent this header — used as the
    /// cookie's own domain when the header doesn't specify `Domain=`.
    pub fn store_from_header(&mut self, set_cookie_value: &str, request_domain: &str) {
        self.store(set_cookie_value, request_domain, true);
    }

    /// Like `store_from_header`, but told whether the response came over
    /// `https`: a `Secure` cookie (or a `__Secure-`/`__Host-` one) from an
    /// insecure origin is rejected, as in browsers — otherwise an
    /// attacker on the network could plant "secure" cookies over HTTP.
    pub fn store(&mut self, set_cookie_value: &str, request_domain: &str, request_is_secure: bool) {
        let Some(cookie) = parse_set_cookie(set_cookie_value, request_domain, request_is_secure) else { return };
        // Only the bucket for this cookie's OWN domain needs scanning
        // for the overwrite check — every cookie in it already shares
        // `cookie.domain` by construction, so `name`+`path` is enough.
        let bucket = self.cookies.entry(cookie.domain.clone()).or_default();
        bucket.retain(|c| !(c.name == cookie.name && c.path == cookie.path));
        bucket.push(cookie);
    }

    /// Removes any cookie whose `expires` is at or before `now_unix`.
    /// A maintenance operation — `header_for` already excludes expired
    /// cookies on its own regardless of whether this has been called,
    /// so this exists for memory hygiene (dropping dead entries), not
    /// correctness. Also drops any domain bucket left empty, so a
    /// long-lived jar doesn't accumulate empty `Vec`s for domains that
    /// no longer have any live cookie.
    pub fn purge_expired(&mut self, now_unix: u64) {
        for bucket in self.cookies.values_mut() {
            bucket.retain(|c| c.expires.map_or(true, |exp| exp > now_unix));
        }
        self.cookies.retain(|_, bucket| !bucket.is_empty());
    }

    /// Builds the `Cookie:` request-header value for a same-site,
    /// browser-initiated request to `request_domain`/`request_path`.
    /// `None` if no cookies match (caller shouldn't send an empty
    /// `Cookie:` header). Cross-site decisions need
    /// [`Self::header_for_request`].
    pub fn header_for(&self, request_domain: &str, request_path: &str, is_secure: bool) -> Option<String> {
        let site = Site::Tuple {
            scheme: if is_secure { Scheme::Https } else { Scheme::Http },
            domain: origin::psl::registrable_domain_or_host(request_domain),
        };
        self.header_for_request(&SendContext {
            host: request_domain,
            path: request_path,
            is_secure,
            request_site: &site,
            initiator_site: None,
            is_navigation: false,
        })
    }

    /// The full policy: domain/path/`Secure`/expiry matching, host-only
    /// scoping, and `SameSite` against the request's initiator.
    pub fn header_for_request(&self, ctx: &SendContext<'_>) -> Option<String> {
        let request_domain = ctx.host.to_ascii_lowercase();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let cross_site = ctx.initiator_site.is_some_and(|initiator| initiator != ctx.request_site);

        let mut matching: Vec<&Cookie> = Vec::new();
        for suffix in domain_suffixes(&request_domain) {
            let Some(bucket) = self.cookies.get(suffix) else { continue };
            let is_exact_host = suffix == request_domain;
            for c in bucket {
                // Host-only cookies belong to exactly the host that set them.
                if c.host_only && !is_exact_host {
                    continue;
                }
                if c.expires.is_some_and(|exp| exp <= now) || !path_matches(&c.path, ctx.path) || (c.secure && !ctx.is_secure) {
                    continue;
                }
                if cross_site && !same_site_allows_cross_site(c.same_site, ctx.is_navigation) {
                    diagnostics::guard(diagnostics::Guard::CookieWithheld, "cookie withheld from a cross-site request by SameSite");
                    continue;
                }
                matching.push(c);
            }
        }

        if matching.is_empty() {
            return None;
        }
        Some(matching.iter().map(|c| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; "))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.cookies.values().map(Vec::len).sum()
    }

    /// Phase F — disk-persistent cookies. Serializes every PERSISTENT
    /// (non-session, i.e. `expires.is_some()`) cookie to a small
    /// tab-separated text format. Session cookies are deliberately not
    /// written — real browsers don't persist them either. No `serde`
    /// dependency: the format is
    /// `name\tvalue\tdomain\tpath\texpires\tsecure\thttp_only` per line,
    /// values are percent-free by construction (a `\t` or newline in a
    /// cookie value would already have been rejected at parse time).
    pub fn serialize(&self) -> String {
        let mut out = String::from("# browser-engine cookie jar v2\n");
        for c in self.cookies.values().flatten() {
            let Some(expires) = c.expires else { continue };
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.name,
                c.value,
                c.domain,
                c.path,
                expires,
                c.secure as u8,
                c.http_only as u8,
                c.host_only as u8,
                c.same_site.name()
            ));
        }
        out
    }

    /// Loads cookies from `serialize`'s format, dropping any that are
    /// already expired or malformed. Merges into the current jar
    /// (replacing same name+domain+path, matching `store_from_header`).
    pub fn load_from_str(&mut self, text: &str) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            // v2 has 9 fields; a v1 file (7 fields) still loads, with the
            // pre-v2 behaviour (matches subdomains, no SameSite).
            if f.len() != 7 && f.len() != 9 {
                continue;
            }
            let Ok(expires) = f[4].parse::<u64>() else { continue };
            if expires <= now {
                continue;
            }
            let cookie = Cookie {
                name: f[0].to_string(),
                value: f[1].to_string(),
                domain: f[2].to_ascii_lowercase(),
                path: f[3].to_string(),
                expires: Some(expires),
                secure: f[5] == "1",
                http_only: f[6] == "1",
                host_only: f.get(7).is_some_and(|v| *v == "1"),
                same_site: f.get(8).map(|v| SameSite::from_name(v)).unwrap_or_default(),
            };
            let bucket = self.cookies.entry(cookie.domain.clone()).or_default();
            bucket.retain(|c| !(c.name == cookie.name && c.path == cookie.path));
            bucket.push(cookie);
        }
    }

    /// Convenience: write `serialize()` to a file.
    pub fn save_to_path(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.serialize())
    }

    /// Convenience: `load_from_str` the contents of a file if it exists
    /// (a missing file is not an error — first run).
    pub fn load_from_path(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                self.load_from_str(&text);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_raw_for_test(&mut self, cookie: Cookie) {
        self.cookies.entry(cookie.domain.clone()).or_default().push(cookie);
    }
}

/// A cookie's own domain matches a request domain if they're
/// identical, or the request domain is a subdomain of the cookie's
/// domain (`example.com` cookie also sent to `www.example.com`). Only
/// used for the `Domain=` attribute's own domain-match validation at
/// parse time now — `header_for`'s cookie lookup uses
/// `domain_suffixes` instead (same semantics, expressed as `HashMap`
/// keys rather than a per-cookie predicate).
fn domain_matches(cookie_domain: &str, request_domain: &str) -> bool {
    request_domain == cookie_domain || request_domain.ends_with(&format!(".{cookie_domain}"))
}

/// Successive ancestor-domain suffixes of `domain`, most-specific
/// first and borrowed (no allocation): `"shop.example.com"` yields
/// `"shop.example.com"`, `"example.com"`, `"com"` in that order. This
/// is `domain_matches`'s same "cookie domain equals, or is a parent
/// of, the request domain" rule, reshaped so `header_for` can do an
/// O(1) `HashMap` lookup per suffix instead of an O(n) scan over every
/// stored cookie checking `domain_matches` against each one.
fn domain_suffixes(domain: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(domain);
    std::iter::from_fn(move || {
        let current = rest?;
        rest = current.find('.').map(|i| &current[i + 1..]);
        Some(current)
    })
}

/// Whether a cookie with `same_site` may ride on a CROSS-site request.
/// `Unspecified` is enforced as `Lax`.
fn same_site_allows_cross_site(same_site: SameSite, is_navigation: bool) -> bool {
    match same_site {
        SameSite::None => true,
        SameSite::Strict => false,
        SameSite::Lax | SameSite::Unspecified => is_navigation,
    }
}

/// `/` matches every path (the common case — most cookies don't set
/// `Path=`). Otherwise: exact match, or the request path is "inside"
/// the cookie's path directory.
fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    if cookie_path == "/" {
        return true;
    }
    request_path == cookie_path || request_path.starts_with(&format!("{}/", cookie_path.trim_end_matches('/')))
}

/// Parses one `Set-Cookie` header value into a `Cookie`. Returns
/// `None` only for genuinely malformed input (no `name=value` at
/// all) — never panics on truncated/weird attribute lists, same
/// graceful-degradation discipline as `html_parser`/`css_parser`.
///
/// `Expires=` (an HTTP-date) is recognized but not parsed — real
/// HTTP-date parsing (RFC 1123 plus the legacy formats real servers
/// still send) is genuinely involved, and `Max-Age` is both simpler
/// and takes precedence over `Expires` when both are present anyway
/// (matching RFC 6265 §5.3's precedence rule) — so `Expires`-only
/// cookies become session cookies here rather than a half-correct
/// date parse. A documented simplification, not an oversight.
fn parse_set_cookie(value: &str, request_domain: &str, request_is_secure: bool) -> Option<Cookie> {
    let mut parts = value.split(';');
    let first = parts.next()?.trim();
    let (name, val) = first.split_once('=')?;
    let name = name.trim();
    let val = val.trim();
    if name.is_empty() {
        return None;
    }

    let request_domain = request_domain.trim_start_matches('.').to_ascii_lowercase();
    let mut cookie = Cookie {
        name: name.to_string(),
        value: val.to_string(),
        domain: request_domain.clone(),
        path: "/".to_string(),
        expires: None,
        secure: false,
        http_only: false,
        host_only: true,
        same_site: SameSite::Unspecified,
    };

    let mut max_age: Option<i64> = None;

    for attr in parts {
        let attr = attr.trim();
        if attr.is_empty() {
            continue;
        }
        let (key, attr_val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (attr, None),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" => {
                if let Some(v) = attr_val {
                    if !v.is_empty() {
                        let requested = v.trim_start_matches('.').to_ascii_lowercase();
                        // RFC 6265 §5.3: a `Domain=` attribute is only
                        // honored if the RESPONDING host domain-matches
                        // it — otherwise the whole cookie is rejected.
                        // Without this check, any server (or a redirect
                        // target it controls) could set `Domain=` to an
                        // ENTIRELY UNRELATED host — e.g. attacker.com
                        // responding with `Set-Cookie: s=x;
                        // Domain=example.com` — and have that cookie
                        // attached to every later request this jar makes
                        // to example.com (session fixation / cross-site
                        // cookie injection).
                        if !domain_matches(&requested, &request_domain) {
                            diagnostics::guard(
                                diagnostics::Guard::CookieRejected,
                                "Set-Cookie Domain= does not domain-match the responding host",
                            );
                            return None;
                        }
                        // A `Domain=` naming a PUBLIC SUFFIX (`com`,
                        // `co.uk`) would scope the cookie to every site
                        // under it — a supercookie. Refused, unless the
                        // responding host IS that name (then it is just a
                        // host-only cookie for that host).
                        if origin::psl::is_public_suffix(&requested) {
                            if requested == request_domain {
                                // keep host-only scoping
                                continue;
                            }
                            diagnostics::guard(
                                diagnostics::Guard::CookieRejected,
                                "Set-Cookie Domain= is a public suffix (supercookie); rejected",
                            );
                            return None;
                        }
                        cookie.domain = requested;
                        cookie.host_only = false;
                    }
                }
            }
            "path" => {
                if let Some(v) = attr_val {
                    if !v.is_empty() {
                        cookie.path = v.to_string();
                    }
                }
            }
            "max-age" => {
                if let Some(v) = attr_val {
                    max_age = v.parse::<i64>().ok();
                }
            }
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "samesite" => cookie.same_site = attr_val.map(SameSite::from_name).unwrap_or_default(),
            _ => {}
        }
    }

    // Browser rules that keep an attacker from planting cookies a site
    // would trust: `Secure` needs a secure origin; `SameSite=None` needs
    // `Secure`; and the `__Secure-`/`__Host-` name prefixes are promises
    // the cookie must actually keep.
    let reject = |reason: &'static str| {
        diagnostics::guard(diagnostics::Guard::CookieRejected, reason);
    };
    if cookie.secure && !request_is_secure {
        reject("Secure cookie set over an insecure connection; rejected");
        return None;
    }
    if cookie.same_site == SameSite::None && !cookie.secure {
        reject("SameSite=None without Secure; rejected");
        return None;
    }
    if cookie.name.starts_with("__Secure-") && !cookie.secure {
        reject("__Secure- cookie without Secure; rejected");
        return None;
    }
    if cookie.name.starts_with("__Host-") && !(cookie.secure && cookie.host_only && cookie.path == "/") {
        reject("__Host- cookie must be Secure, host-only, and Path=/; rejected");
        return None;
    }

    if let Some(seconds) = max_age {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        cookie.expires = Some(if seconds <= 0 {
            // Max-Age <= 0 means "delete this cookie" (RFC 6265
            // §5.3). We don't special-case deletion as its own
            // operation — storing a cookie with an already-past
            // expiry and letting the normal same-name/domain/path
            // replacement in `store_from_header` overwrite whatever
            // was there achieves the same effect for free.
            0
        } else {
            now.saturating_add(seconds as u64)
        });
    }

    Some(cookie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_retrieves_a_simple_cookie() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123", "example.com");
        assert_eq!(jar.header_for("example.com", "/", false), Some("session=abc123".to_string()));
    }

    #[test]
    fn no_cookies_for_unrelated_domain() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123", "example.com");
        assert_eq!(jar.header_for("other.com", "/", false), None);
    }

    /// Performance regression: with cookies bucketed by exact domain,
    /// a request for one domain must only ever surface cookies from
    /// its OWN ancestor-domain chain — never a sibling or unrelated
    /// domain's cookies, even though they now live in entirely
    /// separate `HashMap` buckets rather than one flat scanned list.
    /// Also exercises `domain_suffixes` walking multiple label levels
    /// (`a.b.example.com` -> `b.example.com` -> `example.com`).
    #[test]
    fn cookies_across_many_domains_only_match_their_own_ancestor_chain() {
        let mut jar = CookieJar::new();
        jar.store_from_header("a=1", "example.com");
        jar.store_from_header("b=2", "other.com");
        jar.store_from_header("c=3; Domain=example.com", "sub.example.com");
        jar.store_from_header("d=4", "unrelated.net");

        assert_eq!(jar.len(), 4);

        let for_root = jar.header_for("example.com", "/", false).unwrap();
        assert!(for_root.contains("a=1") && for_root.contains("c=3"));
        assert!(!for_root.contains("b=2") && !for_root.contains("d=4"));

        // A deeper subdomain still reaches the `example.com`-scoped cookie
        // via `domain_suffixes`'s walk up the label chain — `c=3` (explicit
        // `Domain=example.com`) — but NOT `a=1`: that one had no `Domain=`,
        // so it is host-only and belongs to `example.com` alone. (This
        // test used to pin the opposite — a documented simplification in
        // which host-only cookies leaked to every subdomain.)
        let for_deep_sub = jar.header_for("a.b.example.com", "/", false).unwrap();
        assert!(for_deep_sub.contains("c=3"));
        assert!(!for_deep_sub.contains("a=1"), "a host-only cookie must not reach a subdomain");
        assert!(!for_deep_sub.contains("b=2") && !for_deep_sub.contains("d=4"));

        assert_eq!(jar.header_for("other.com", "/", false).unwrap(), "b=2");
        assert_eq!(jar.header_for("unrelated.net", "/", false).unwrap(), "d=4");
    }

    #[test]
    fn explicit_domain_attribute_allows_subdomain_match() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Domain=example.com", "www.example.com");
        assert_eq!(jar.header_for("example.com", "/", false), Some("session=abc123".to_string()));
        assert_eq!(jar.header_for("shop.example.com", "/", false), Some("session=abc123".to_string()));
        assert_eq!(jar.header_for("notexample.com", "/", false), None);
    }

    #[test]
    fn leading_dot_in_domain_attribute_is_normalized() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Domain=.example.com", "example.com");
        assert_eq!(jar.header_for("example.com", "/", false), Some("session=abc123".to_string()));
    }

    /// Security regression: a response from `attacker.com` (or a
    /// redirect target it controls) must NOT be able to plant/overwrite
    /// a cookie for an unrelated domain via `Domain=`. Without a
    /// domain-match check on the attribute, this would let any site the
    /// browser visits inject session-fixation cookies into any other
    /// domain's cookie jar.
    #[test]
    fn domain_attribute_for_an_unrelated_host_is_rejected_entirely() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=evil; Domain=example.com", "attacker.com");
        assert_eq!(jar.header_for("example.com", "/", false), None, "attacker.com must not be able to set a cookie for example.com");
        assert_eq!(jar.header_for("attacker.com", "/", false), None, "the whole cookie is rejected, not just the Domain attribute");
    }

    /// A broader/parent domain-attribute is still fine as long as the
    /// responding host is actually within it (the real, legitimate use
    /// case `Domain=` exists for).
    #[test]
    fn domain_attribute_for_a_parent_of_the_responding_host_is_accepted() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Domain=example.com", "www.example.com");
        assert_eq!(jar.header_for("example.com", "/", false), Some("session=abc123".to_string()));
    }

    #[test]
    fn path_scoping_restricts_to_matching_paths() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Path=/account", "example.com");
        assert_eq!(jar.header_for("example.com", "/account", false), Some("session=abc123".to_string()));
        assert_eq!(jar.header_for("example.com", "/account/settings", false), Some("session=abc123".to_string()));
        assert_eq!(jar.header_for("example.com", "/other", false), None);
    }

    #[test]
    fn secure_cookie_only_sent_over_https() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Secure", "example.com");
        assert_eq!(jar.header_for("example.com", "/", false), None); // http
        assert_eq!(jar.header_for("example.com", "/", true), Some("session=abc123".to_string())); // https
    }

    #[test]
    fn max_age_sets_a_future_expiry_and_cookie_is_still_valid() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Max-Age=3600", "example.com");
        assert!(jar.header_for("example.com", "/", false).is_some());
    }

    #[test]
    fn max_age_zero_deletes_the_cookie() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123", "example.com");
        assert!(jar.header_for("example.com", "/", false).is_some());

        jar.store_from_header("session=abc123; Max-Age=0", "example.com");
        assert_eq!(jar.header_for("example.com", "/", false), None);
    }

    #[test]
    fn multiple_cookies_are_joined_with_semicolons() {
        let mut jar = CookieJar::new();
        jar.store_from_header("a=1", "example.com");
        jar.store_from_header("b=2", "example.com");
        let header = jar.header_for("example.com", "/", false).unwrap();
        assert!(header.contains("a=1"));
        assert!(header.contains("b=2"));
        assert!(header.contains("; "));
    }

    #[test]
    fn same_name_domain_path_overwrites_not_duplicates() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=old", "example.com");
        jar.store_from_header("session=new", "example.com");
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.header_for("example.com", "/", false), Some("session=new".to_string()));
    }

    #[test]
    fn purge_expired_removes_dead_entries() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; Max-Age=0", "example.com");
        assert_eq!(jar.len(), 1);
        jar.purge_expired(u64::MAX); // "now" far in the future — everything with an expiry is dead
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn purge_expired_keeps_session_cookies() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123", "example.com"); // no Max-Age => session cookie
        jar.purge_expired(u64::MAX);
        assert_eq!(jar.len(), 1);
    }

    #[test]
    fn malformed_header_does_not_panic() {
        let mut jar = CookieJar::new();
        for input in ["", ";;;", "=noname", "novalue", "a=1; ; Domain=; Path=", "a=1;;;Secure;;;"] {
            jar.store_from_header(input, "example.com");
        }
        // The only real assertion is that none of the above panicked.
    }

    #[test]
    fn already_expired_cookie_is_never_sent() {
        let mut jar = CookieJar::new();
        // Directly inserted with an expiry in the deep past — proves
        // `header_for`'s own real-time expiry check works, independent
        // of Max-Age parsing (already covered by
        // `max_age_zero_deletes_the_cookie`) and without needing to
        // sleep real wall-clock time in a test.
        jar.insert_raw_for_test(Cookie {
            name: "session".to_string(),
            value: "abc123".to_string(),
            domain: "example.com".to_string(),
            path: "/".to_string(),
            expires: Some(1), // 1 second after the Unix epoch — always in the past
            secure: false,
            http_only: false,
            host_only: true,
            same_site: SameSite::Unspecified,
        });
        assert_eq!(jar.header_for("example.com", "/", false), None);
    }

    #[test]
    fn http_only_and_samesite_are_recognized_without_affecting_header_for() {
        let mut jar = CookieJar::new();
        jar.store_from_header("session=abc123; HttpOnly; SameSite=Strict", "example.com");
        let cookie = &jar.cookies["example.com"][0];
        assert!(cookie.http_only);
        // header_for doesn't distinguish HttpOnly — that attribute
        // only matters for JS access (Milestone 12's `document.cookie`,
        // not built yet), not for whether the browser itself sends it.
        assert!(jar.header_for("example.com", "/", false).is_some());
    }

    // ---- Phase F: disk-persistent cookies ----

    #[test]
    fn persistent_cookies_round_trip_through_serialize_and_load() {
        let mut jar = CookieJar::new();
        jar.store_from_header("pref=dark; Max-Age=100000; Path=/; Secure", "example.com");
        jar.store_from_header("session=tmp", "example.com"); // session cookie — not persisted

        let text = jar.serialize();
        let mut restored = CookieJar::new();
        restored.load_from_str(&text);

        assert_eq!(restored.len(), 1, "only the persistent cookie survives a save/load");
        assert_eq!(restored.header_for("example.com", "/", true), Some("pref=dark".to_string()));
        assert_eq!(restored.cookies["example.com"][0].secure, true);
    }

    #[test]
    fn loading_skips_already_expired_and_malformed_lines() {
        let mut jar = CookieJar::new();
        jar.load_from_str("# header\nold\tv\texample.com\t/\t1\t0\t0\ngarbage line\nk\tv\texample.com\t/\t99999999999\t0\t0\n");
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.cookies["example.com"][0].name, "k");
    }

    #[test]
    fn save_to_path_then_load_from_path_survives_a_new_jar() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("be-cookie-test-{}.txt", std::process::id()));
        let mut jar = CookieJar::new();
        jar.store_from_header("token=xyz; Max-Age=100000", "site.test");
        jar.save_to_path(&path).unwrap();

        let mut fresh = CookieJar::new();
        fresh.load_from_path(&path).unwrap();
        assert_eq!(fresh.header_for("site.test", "/", false), Some("token=xyz".to_string()));

        // A missing file is a no-op, not an error.
        let _ = std::fs::remove_file(&path);
        let mut empty = CookieJar::new();
        assert!(empty.load_from_path(&path).is_ok());
    }

    // ---- Milestone 27: a rejected cookie is observable ----

    #[test]
    fn a_domain_mismatched_cookie_is_rejected_visibly_and_an_honest_one_is_silent() {
        use diagnostics::{collector::capture, Guard};
        let mut jar = CookieJar::new();
        let (_, c) = capture(|| jar.store_from_header("s=x; Domain=example.com", "attacker.test"));
        assert!(c.saw_guard(Guard::CookieRejected));
        assert_eq!(jar.header_for("example.com", "/", false), None, "and it really was rejected");

        let (_, c) = capture(|| jar.store_from_header("s=x; Domain=example.com", "www.example.com"));
        assert!(c.events.is_empty(), "a legitimate parent-domain cookie trips nothing: {:?}", c.events);
    }

    // ---- Hardening pass: real cookie scoping and SameSite ----

    use diagnostics::{collector::capture, Guard};

    fn https_site(domain: &str) -> Site {
        Site::Tuple { scheme: Scheme::Https, domain: domain.to_string() }
    }

    /// The `Cookie` header `jar` sends to `host` (an https request whose
    /// site is `site`) when the request was initiated from `initiator`.
    fn send_cross_site(jar: &CookieJar, host: &str, site: &str, initiator: &str, is_navigation: bool) -> Option<String> {
        let (site, initiator) = (https_site(site), https_site(initiator));
        jar.header_for_request(&SendContext { host, path: "/", is_secure: true, request_site: &site, initiator_site: Some(&initiator), is_navigation })
    }

    #[test]
    fn a_host_only_cookie_is_not_sent_to_subdomains_but_a_domain_cookie_is() {
        let mut jar = CookieJar::new();
        jar.store_from_header("hostonly=1", "example.com");
        jar.store_from_header("shared=1; Domain=example.com", "example.com");
        let root = jar.header_for("example.com", "/", true).unwrap();
        assert!(root.contains("hostonly=1") && root.contains("shared=1"));
        let sub = jar.header_for("www.example.com", "/", true).unwrap();
        assert_eq!(sub, "shared=1", "only the Domain= cookie reaches a subdomain");
    }

    #[test]
    fn supercookies_on_public_suffixes_are_rejected() {
        // Measured in the principles audit: `Domain=com` was accepted and
        // sent to every unrelated .com site.
        let mut jar = CookieJar::new();
        for (header, host) in [
            ("track=1; Domain=com", "example.com"),
            ("track=1; Domain=.com", "example.com"),
            ("track=1; Domain=co.uk", "shop.example.co.uk"),
            ("track=1; Domain=github.io", "alice.github.io"),
            ("track=1; Domain=blogspot.com", "mine.blogspot.com"),
        ] {
            let (_, c) = capture(|| jar.store_from_header(header, host));
            assert!(c.saw_guard(Guard::CookieRejected), "{header} from {host} must be rejected");
        }
        assert_eq!(jar.len(), 0);
        assert_eq!(jar.header_for("evil.com", "/", true), None);
        assert_eq!(jar.header_for("other.co.uk", "/", true), None);
    }

    #[test]
    fn a_domain_naming_a_suffix_that_is_the_responding_host_becomes_host_only() {
        // e.g. an intranet host literally called `localhost`, or `com`.
        let mut jar = CookieJar::new();
        jar.store_from_header("a=1; Domain=localhost", "localhost");
        assert_eq!(jar.header_for("localhost", "/", true).as_deref(), Some("a=1"));
        assert_eq!(jar.header_for("sub.localhost", "/", true), None, "and it stays host-only");
    }

    #[test]
    fn a_registrable_domain_is_still_allowed_as_a_domain_attribute() {
        let mut jar = CookieJar::new();
        let (_, c) = capture(|| jar.store_from_header("ok=1; Domain=example.co.uk", "www.example.co.uk"));
        assert!(c.events.is_empty(), "{:?}", c.events);
        assert_eq!(jar.header_for("api.example.co.uk", "/", true).as_deref(), Some("ok=1"));
    }

    #[test]
    fn secure_cookies_require_a_secure_origin_and_prefixes_keep_their_promises() {
        let mut jar = CookieJar::new();
        for (header, secure_origin, accepted) in [
            ("a=1; Secure", false, false),
            ("a=1; Secure", true, true),
            ("b=1; SameSite=None", true, false),
            ("b=1; SameSite=None; Secure", true, true),
            ("__Secure-c=1", true, false),
            ("__Secure-c=1; Secure", true, true),
            ("__Host-d=1; Secure; Path=/", true, true),
            ("__Host-e=1; Secure; Path=/x", true, false),
            ("__Host-f=1; Secure; Path=/; Domain=example.com", true, false),
            ("__Host-g=1; Path=/", true, false),
        ] {
            let before = jar.len();
            let (_, c) = capture(|| jar.store(header, "example.com", secure_origin));
            assert_eq!(jar.len() > before, accepted, "{header} (secure origin: {secure_origin})");
            assert_eq!(c.saw_guard(Guard::CookieRejected), !accepted, "{header}");
        }
    }

    #[test]
    fn samesite_is_enforced_against_the_requests_initiator() {
        let mut jar = CookieJar::new();
        jar.store_from_header("strict=1; SameSite=Strict", "example.com");
        jar.store_from_header("lax=1; SameSite=Lax", "example.com");
        jar.store_from_header("unspec=1", "example.com");
        jar.store_from_header("none=1; SameSite=None; Secure", "example.com");

        // Same site (or browser-initiated): everything is sent.
        let example = https_site("example.com");
        let same = SendContext { host: "example.com", path: "/", is_secure: true, request_site: &example, initiator_site: Some(&example), is_navigation: false };
        let all = jar.header_for_request(&same).unwrap();
        for name in ["strict=1", "lax=1", "unspec=1", "none=1"] {
            assert!(all.contains(name), "{name} rides same-site requests: {all}");
        }
        let typed = SendContext { initiator_site: None, ..same };
        assert!(jar.header_for_request(&typed).unwrap().contains("strict=1"), "browser-initiated is never cross-site");

        // Cross-site SUBRESOURCE / script request: only SameSite=None.
        let (header, c) = capture(|| send_cross_site(&jar, "example.com", "example.com", "evil.test", false));
        assert_eq!(header.as_deref(), Some("none=1"));
        assert!(c.saw_guard(Guard::CookieWithheld), "withholding is observable");

        // Cross-site top-level NAVIGATION (clicking a link): Lax and unspecified ride along too.
        let nav = send_cross_site(&jar, "example.com", "example.com", "evil.test", true).unwrap();
        assert!(nav.contains("lax=1") && nav.contains("unspec=1") && nav.contains("none=1"));
        assert!(!nav.contains("strict=1"), "Strict never crosses sites, even on navigation: {nav}");
    }

    #[test]
    fn subdomains_of_one_site_are_same_site() {
        let mut jar = CookieJar::new();
        jar.store_from_header("s=1; SameSite=Strict; Domain=example.com", "example.com");
        // app.example.com -> api.example.com: same site, so Strict rides.
        assert_eq!(send_cross_site(&jar, "api.example.com", "example.com", "example.com", false).as_deref(), Some("s=1"));
        // Two github.io users are DIFFERENT sites (private-section suffix).
        jar.store_from_header("g=1; SameSite=Strict", "alice.github.io");
        assert_eq!(send_cross_site(&jar, "alice.github.io", "alice.github.io", "bob.github.io", false), None);
    }

    #[test]
    fn persistence_round_trips_scoping_and_still_reads_the_old_format() {
        let mut jar = CookieJar::new();
        jar.store_from_header("h=1; Max-Age=3600; SameSite=Strict", "example.com");
        jar.store_from_header("d=1; Max-Age=3600; Domain=example.com; SameSite=Lax", "example.com");
        let text = jar.serialize();
        assert!(text.starts_with("# browser-engine cookie jar v2"));

        let mut reloaded = CookieJar::new();
        reloaded.load_from_str(&text);
        assert_eq!(reloaded.header_for("www.example.com", "/", true).as_deref(), Some("d=1"), "host-only-ness survived");
        let strict = send_cross_site(&reloaded, "example.com", "example.com", "evil.test", true).unwrap();
        assert!(strict.contains("d=1") && !strict.contains("h=1"), "SameSite survived: {strict}");

        // A v1 file (7 fields) still loads, with the old behaviour.
        let future = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
        let mut legacy = CookieJar::new();
        legacy.load_from_str(&format!("old\t1\texample.com\t/\t{future}\t0\t0\n"));
        assert_eq!(legacy.header_for("www.example.com", "/", true).as_deref(), Some("old=1"));
    }
}
