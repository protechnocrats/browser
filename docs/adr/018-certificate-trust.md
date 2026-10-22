# ADR 018 — Which certificate authorities the browser trusts

- **Status:** accepted (Milestone 37, 2026-10-11)
- **Decides:** the TLS trust anchors (P8 in `ROADMAP.md` §3).
- **Constrains:** `network` (the TLS configuration of every route),
  `browser_core` (settings), hosts (reading CA files).

## Context

Until M37 the only trust anchors were the Mozilla root list compiled into
the binary (`webpki-roots`, through `ureq`). The OS store and user or
enterprise CAs were ignored, and trust changed only with a rebuild. That is
reproducible and identical on every platform — and wrong for a company
network that inspects TLS, or a user with a private CA.

## Decision

1. **The bundled Mozilla roots stay the default**, on every platform: the
   same trust decisions everywhere, reviewed by a public process, and
   independent of any OS vendor's store (architecture doc §1).
2. **Extra anchors are added, never substituted:** the profile can name PEM
   files of additional CAs (setting `tls.extra_roots`, a list of paths;
   flag `--extra-roots`). An enterprise or user CA is trusted *in addition*
   to the bundled roots. A file that cannot be read or holds no certificate
   is reported and skipped; it never empties the trust store.
3. **The OS store, as a file the user names.** On Linux the system bundle
   is a PEM file (`/etc/ssl/certs/ca-certificates.crt` and the like), so
   "trust the system store" is `--trust-system-store`, which makes `app`
   add the first such bundle it finds as extra roots. No platform
   certificate API is linked (macOS and Windows stores are later work,
   behind the same "extra roots" input).
4. **Only the host reads the files** (capability-based): `network` takes
   certificate bytes, never paths.
5. Certificate pinning and HSTS stay in the backlog.

## Consequences

- `network::TlsTrust { extra_roots_pem }`; `rustls` and `webpki-roots` (the
  versions `ureq` already uses) become direct dependencies of `network`, to
  build the same TLS configuration `ureq` builds with more anchors.
- A malformed PEM file cannot crash the browser: parsing is bounded and
  tested with hostile input.
