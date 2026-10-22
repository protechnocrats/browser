# ADR 002 — The Public Suffix List: vendored, offline, and where it is used

- **Status:** accepted (hardening pass after Milestone 27)
- **Decides:** where public-suffix data comes from, how it is kept current, and what may depend on it.
- **Constrains:** cookie scoping, `SameSite`, HTTP-cache partitioning, and (M28) the `Site`/`Origin` types.

## Context

The principles audit measured a real supercookie: `Set-Cookie: t=1; Domain=com`
was accepted and sent to every unrelated `.com` site (and `Domain=co.uk` to
every `.co.uk`). Fixing that — and `SameSite`, and partitioning the HTTP cache
by top-level site — all need to answer "which registrable domain (site) does
this host belong to?", which is defined by the Public Suffix List (PSL).

## Decision

1. **Vendor a snapshot of the list in the repository**
   (`crates/origin/data/public_suffix_list.dat`) and compile it into the binary
   with `include_str!`. No runtime fetch, no network dependency, reproducible
   builds, and a browser that works offline and never phones anything home to
   learn where cookie boundaries are (architecture doc §1, vendor-independent).
2. **Refresh it deliberately.** `scripts/update-public-suffix-list.sh` downloads,
   *validates* (VERSION line, both sections, ≥ 5000 rules — so an error page or a
   truncated download cannot become the list) and replaces it; the result is a
   normal reviewed diff.
3. **Staleness is a security property, so CI enforces it.**
   `scripts/check-psl-freshness.sh` fails once the snapshot is older than 180
   days: a missing new suffix means a cookie scoped to it would not be seen as a
   supercookie. (Self-tested in `scripts/test-guardrails.sh`.)
4. **Parse at first use, share immutably.** A `OnceLock<PublicSuffixList>`; after
   initialisation it is read-only, so there is no lock and no mutable global
   state. Parsing 10,325 rules takes ~ms.
5. **Hostile hosts are bounded before they are processed.** Hosts over DNS limits
   (253 bytes, 127 labels) are "not a hostname" and get no suffix work. This is
   not theoretical: the fuzz suite's stress inputs found the first implementation
   quadratic in label count (a 100,000-label host in an `<img src>` took 66 s;
   1 MiB took 433 s). The lookup is now allocation-free and linear, *and*
   bounded.
6. **Scope.** This is the minimum of the M28 origin/site model the cookie and
   cache policies need now: `site_of_host`, `site_of_url`, `is_public_suffix`,
   `registrable_domain`. It is *not* the full `Origin` type; M28 will extract it
   into its own crate.

## The data's license

The PSL is **MPL-2.0**. It is data, not a Rust dependency, so `cargo deny`'s crate
license policy does not apply; the file keeps its license header intact, and
MPL-2.0's file-level terms are satisfied by shipping the file unmodified and
keeping its source available (it is in this repository).

## Alternatives rejected

- **A crate (`publicsuffix`, `psl`).** Another dependency whose update cadence we
  do not control, and (for `psl`) generated code compiled into a crate. The list is
  ~330 KB of data and the algorithm is ~60 lines; owning both is cheaper than
  trusting either.
- **Fetch at runtime.** A network dependency and a phone-home in the
  cookie-security path; fails offline; a hostile network could serve a list that
  makes supercookies acceptable.
- **A hard-coded short list (`com`, `co.uk`, …).** Wrong for most of the ~10,000
  suffixes, including private-section ones (`github.io`, `blogspot.com`) that
  matter most for cookie isolation between unrelated users.

## Consequences

- The binary is ~330 KB larger.
- Someone must run the update script a couple of times a year; CI will say when.
- `github.io`-style hosting is handled correctly: `alice.github.io` and
  `bob.github.io` are different sites.
