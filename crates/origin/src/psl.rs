//! The Public Suffix List: which domains are *public suffixes* (`com`,
//! `co.uk`, `github.io` — names under which unrelated parties register
//! domains) and so what a host's *registrable domain* is (`www.example.
//! co.uk` → `example.co.uk`). [`crate::Site`] is built on this; cookie
//! `Domain=` validation uses [`is_public_suffix`] directly (a cookie with
//! `Domain=com` would otherwise be a "supercookie" sent to every `.com`).
//!
//! ## The data
//!
//! `data/public_suffix_list.dat` is a **vendored snapshot** of the list
//! (<https://publicsuffix.org/list/>, MPL-2.0 — the file keeps its licence
//! header), compiled in with `include_str!`: offline, reproducible, no
//! runtime fetch (architecture doc §1, vendor-independent;
//! `docs/adr/002-public-suffix-list.md`). Update with
//! `scripts/update-public-suffix-list.sh`; `scripts/check-psl-freshness.sh`
//! fails CI when the snapshot gets too old. (Moved here from
//! `network::site` in Milestone 28 — ADR 004.)

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::OnceLock;

/// DNS limits: a hostname is at most 253 bytes and 127 labels. Anything
/// longer is not a hostname, and — this was found by the fuzz suite's
/// stress inputs — must not be *processed*: the suffix lookup used to be
/// quadratic in the label count, so a 100,000-label host in an `<img src>`
/// took over a minute (a freeze reachable from web content). Rejecting
/// oversized input up front bounds every later step, and the lookup below
/// is linear as well.
const MAX_HOST_BYTES: usize = 253;
const MAX_LABELS: usize = 127;

/// A parsed Public Suffix List.
#[derive(Debug, Default)]
pub struct PublicSuffixList {
    /// Plain rules: `co.uk`.
    rules: HashSet<String>,
    /// Wildcard rules stored WITHOUT the `*.`: `*.ck` → `ck`.
    wildcards: HashSet<String>,
    /// Exception rules stored WITHOUT the `!`: `!www.ck` → `www.ck`.
    exceptions: HashSet<String>,
}

/// Normalizes a rule or host to lowercase ASCII (punycode). The list
/// carries Unicode rules (`公司.cn`); hosts reach us already as punycode
/// from the `url` crate, so rules must be converted to compare.
fn normalize(name: &str) -> Option<String> {
    let name = name.trim().trim_end_matches('.');
    // Bounded FIRST: a hostile input must not be able to make any later
    // step (IDNA conversion included) do work proportional to its length.
    if name.is_empty() || name.len() > MAX_HOST_BYTES * 4 {
        return None;
    }
    if name.is_ascii() {
        return Some(name.to_ascii_lowercase());
    }
    let host = url::Url::parse(&format!("http://{name}/")).ok()?.host_str()?.to_string();
    Some(host)
}

impl PublicSuffixList {
    /// Parses the list format: one rule per line, `//` comments, `!` for
    /// exceptions, `*.` for wildcards; a rule ends at the first whitespace.
    /// Unparseable lines are skipped — never a panic (this parser is fuzzed).
    pub fn parse(text: &str) -> Self {
        let mut list = PublicSuffixList::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let rule = line.split_whitespace().next().unwrap_or("");
            if let Some(exception) = rule.strip_prefix('!') {
                if let Some(n) = normalize(exception) {
                    list.exceptions.insert(n);
                }
            } else if let Some(wild) = rule.strip_prefix("*.") {
                if let Some(n) = normalize(wild) {
                    list.wildcards.insert(n);
                }
            } else if let Some(n) = normalize(rule) {
                list.rules.insert(n);
            }
        }
        list
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len() + self.wildcards.len() + self.exceptions.len()
    }

    /// Splits a normalized host into label start offsets, or `None` if it
    /// is not a valid, in-limits hostname (empty label, too long, too many
    /// labels).
    fn label_starts(host: &str) -> Option<Vec<usize>> {
        if host.len() > MAX_HOST_BYTES {
            return None;
        }
        let mut starts = vec![0];
        for (i, b) in host.bytes().enumerate() {
            if b == b'.' {
                starts.push(i + 1);
            }
        }
        if starts.len() > MAX_LABELS {
            return None;
        }
        // No empty labels: "a..b", or a leading dot.
        let mut previous_end = host.len() + 1;
        for &s in starts.iter().rev() {
            if s + 1 >= previous_end {
                return None;
            }
            previous_end = s;
        }
        Some(starts)
    }

    /// Number of trailing labels of `host` that form the public suffix, per
    /// the standard algorithm: the matching rule with the most labels wins,
    /// an exception rule beats every other and removes its leftmost label,
    /// and with no match the default rule `*` makes the last label the
    /// suffix. Slices only — no allocation, linear in the label count.
    fn suffix_len(&self, host: &str, starts: &[usize]) -> usize {
        let n = starts.len();
        let mut best = 1; // default rule "*"
        for i in 0..n {
            let candidate = &host[starts[i]..];
            if self.exceptions.contains(candidate) {
                return n - i - 1;
            }
            let len = n - i;
            if len > best {
                let is_rule = self.rules.contains(candidate);
                let is_wildcard = i + 1 < n && self.wildcards.contains(&host[starts[i + 1]..]);
                if is_rule || is_wildcard {
                    best = len;
                }
            }
        }
        best
    }

    /// True if `host` is itself a public suffix (`com`, `co.uk`).
    pub fn is_public_suffix(&self, host: &str) -> bool {
        let Some(host) = normalize(host) else { return false };
        let Some(starts) = Self::label_starts(&host) else { return false };
        self.suffix_len(&host, &starts) == starts.len()
    }

    /// The registrable domain (public suffix + one label), or `None` if
    /// `host` is itself a public suffix or is not a valid hostname.
    pub fn registrable_domain(&self, host: &str) -> Option<String> {
        let host = normalize(host)?;
        let starts = Self::label_starts(&host)?;
        let suffix = self.suffix_len(&host, &starts);
        if starts.len() <= suffix {
            return None;
        }
        Some(host[starts[starts.len() - suffix - 1]..].to_string())
    }
}

/// The compiled-in list, parsed once on first use. Immutable after that,
/// so sharing it across threads needs no lock and is not mutable global
/// state.
pub fn public_suffix_list() -> &'static PublicSuffixList {
    static LIST: OnceLock<PublicSuffixList> = OnceLock::new();
    LIST.get_or_init(|| PublicSuffixList::parse(include_str!("../data/public_suffix_list.dat")))
}

fn is_ip_literal(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>().is_ok()
}

/// True if `host` is a public suffix. IP literals never are.
pub fn is_public_suffix(host: &str) -> bool {
    !is_ip_literal(host) && public_suffix_list().is_public_suffix(host)
}

/// The registrable domain of `host`, or — for an IP literal, a host that is
/// itself a public suffix (`localhost`), or a string too long to be a
/// hostname — the lowercased host itself. A trailing dot is ignored. This is
/// the host half of a [`crate::Site`].
pub fn registrable_domain_or_host(host: &str) -> String {
    let lowered = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if lowered.len() > MAX_HOST_BYTES || is_ip_literal(&lowered) {
        // An IP literal, or too long to be a hostname (see MAX_HOST_BYTES):
        // its own site, with no suffix work.
        return lowered;
    }
    public_suffix_list().registrable_domain(&lowered).unwrap_or(lowered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psl() -> &'static PublicSuffixList {
        public_suffix_list()
    }

    #[test]
    fn the_vendored_list_is_real_and_sizeable() {
        assert!(psl().rule_count() > 8000, "{} rules", psl().rule_count());
    }

    #[test]
    fn registrable_domains_follow_the_list() {
        assert_eq!(psl().registrable_domain("example.com").as_deref(), Some("example.com"));
        assert_eq!(psl().registrable_domain("www.example.com").as_deref(), Some("example.com"));
        assert_eq!(psl().registrable_domain("a.b.c.example.com").as_deref(), Some("example.com"));
        assert_eq!(psl().registrable_domain("www.example.co.uk").as_deref(), Some("example.co.uk"));
        assert_eq!(psl().registrable_domain("shop.example.com.au").as_deref(), Some("example.com.au"));
    }

    #[test]
    fn public_suffixes_have_no_registrable_domain() {
        for suffix in ["com", "co.uk", "com.au", "org", "github.io", "blogspot.com"] {
            assert!(psl().is_public_suffix(suffix), "{suffix} is a public suffix");
            assert_eq!(psl().registrable_domain(suffix), None, "{suffix}");
        }
        assert!(!psl().is_public_suffix("example.com"));
        assert!(!psl().is_public_suffix("example.co.uk"));
    }

    #[test]
    fn private_section_suffixes_partition_sites() {
        // Two GitHub Pages sites are different sites, not one.
        assert_eq!(psl().registrable_domain("alice.github.io").as_deref(), Some("alice.github.io"));
        assert_eq!(psl().registrable_domain("bob.github.io").as_deref(), Some("bob.github.io"));
        assert_ne!(registrable_domain_or_host("alice.github.io"), registrable_domain_or_host("bob.github.io"));
    }

    #[test]
    fn wildcard_and_exception_rules() {
        // `*.ck` makes every second-level label a suffix; `!www.ck` carves
        // out an exception, making `www.ck` registrable under suffix `ck`.
        assert!(psl().is_public_suffix("anything.ck"));
        assert_eq!(psl().registrable_domain("a.anything.ck").as_deref(), Some("a.anything.ck"));
        assert_eq!(psl().registrable_domain("www.ck").as_deref(), Some("www.ck"));
        assert!(!psl().is_public_suffix("www.ck"));
        assert!(psl().is_public_suffix("ck"));
    }

    #[test]
    fn unknown_tlds_use_the_default_rule() {
        assert_eq!(psl().registrable_domain("example.notarealtld").as_deref(), Some("example.notarealtld"));
        assert_eq!(psl().registrable_domain("www.example.notarealtld").as_deref(), Some("example.notarealtld"));
        assert!(psl().is_public_suffix("notarealtld"));
    }

    #[test]
    fn internationalized_rules_match_punycode_hosts() {
        // `公司.cn` is a rule in the list; hosts arrive as punycode.
        let unicode = url::Url::parse("http://食狮.公司.cn/").unwrap();
        let host = unicode.host_str().unwrap().to_string();
        assert!(host.starts_with("xn--"), "{host}");
        let suffix_only = url::Url::parse("http://公司.cn/").unwrap().host_str().unwrap().to_string();
        assert!(psl().is_public_suffix(&suffix_only), "{suffix_only}");
        assert_eq!(psl().registrable_domain(&host).as_deref(), Some(host.as_str()));
    }

    #[test]
    fn case_trailing_dots_and_garbage_are_handled() {
        assert_eq!(registrable_domain_or_host("WWW.Example.COM."), "example.com");
        for garbage in ["", ".", "..", "a..b", "-", "\u{0}", "a b", "https://x.com"] {
            let _ = registrable_domain_or_host(garbage);
            let _ = psl().registrable_domain(garbage);
            let _ = psl().is_public_suffix(garbage);
        }
        assert_eq!(psl().registrable_domain(""), None);
        assert_eq!(psl().registrable_domain("a..b"), None);
    }

    #[test]
    fn ip_literals_and_single_label_hosts_are_their_own_site() {
        assert_eq!(registrable_domain_or_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(registrable_domain_or_host("[::1]"), "[::1]");
        assert_eq!(registrable_domain_or_host("::1"), "::1");
        assert_eq!(registrable_domain_or_host("localhost"), "localhost");
        assert!(!is_public_suffix("127.0.0.1"), "an IP is never a suffix");
    }

    #[test]
    fn parsing_hostile_list_text_never_panics() {
        for text in ["", "\n\n\n", "!", "*.", "!*.", "*.*.*", "// only comment", "a b c", "\u{0}\u{1}", &"x.".repeat(100_000), "公司.cn\n食狮\n\u{202e}"] {
            let list = PublicSuffixList::parse(text);
            let _ = list.registrable_domain("a.b.c");
        }
    }

    #[test]
    fn parsing_the_real_list_is_fast() {
        let started = std::time::Instant::now();
        let list = PublicSuffixList::parse(include_str!("../data/public_suffix_list.dat"));
        assert!(list.rule_count() > 8000);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn hostile_hosts_are_bounded_not_quadratic() {
        // Found by the fuzz stress inputs: a 100,000-label host took 66 s,
        // 1 MiB took 433 s — reachable from an `<img src>`.
        let started = std::time::Instant::now();
        let huge_labels = "a.".repeat(100_000) + "com";
        let one_mib = "a".repeat(1 << 20);
        for host in [huge_labels.as_str(), one_mib.as_str(), &"a.".repeat(1 << 19)] {
            assert_eq!(psl().registrable_domain(host), None, "not a valid hostname");
            assert!(!psl().is_public_suffix(host));
            let _ = registrable_domain_or_host(host);
            let _ = crate::Origin::parse(&format!("http://{host}/")).site();
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn hosts_at_the_dns_limits_still_work() {
        let label63 = "a".repeat(63);
        let host = format!("{label63}.{label63}.{label63}.example.com"); // 63*3 + 3 + 11 = 203 bytes
        assert_eq!(psl().registrable_domain(&host).as_deref(), Some("example.com"));
        let many = format!("{}example.com", "a.".repeat(100)); // 102 labels
        assert_eq!(psl().registrable_domain(&many).as_deref(), Some("example.com"));
    }
}

