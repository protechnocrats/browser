# Contributing

Thanks for your interest. The project is young and moves fast, so please
open an issue to discuss anything large before you start on it.

## How work is organised

- **Milestones.** Larger pieces of work are planned as milestones in
  [`ROADMAP.md`](ROADMAP.md) §3, each with a scope and acceptance criteria.
  A milestone is finished when it meets the Definition of Done in
  `ROADMAP.md` §2. Then it gets its own write-up under `milestones/`, and
  `AUDIT.md` and `ROADMAP.md` §1/§3 are updated to match.
- **Gaps.** Smaller fixes close a named row in `AUDIT.md`'s open gaps or an
  item in the `ROADMAP.md` backlog (§4). Say which one in your change.
- **Design decisions.** A change to the architecture gets an architecture
  decision record in [`docs/adr/`](docs/adr/).

## Before you send a change

- `cargo test --workspace --release` passes.
- The guardrail scripts listed in the [README](README.md#building-and-testing)
  pass. In particular, no `unsafe` code and no new clippy warnings.
- A new crate is listed in `scripts/core-crates.txt` (platform-free) or
  `scripts/platform-crates.txt` (may use `wgpu`/`winit`).
- New dependencies use a license allowed in [`deny.toml`](deny.toml) that
  is compatible with GPL-3.0-only.
- Every commit is signed off (see below).

## Be honest about what was verified

Some things can't be checked on every machine: changes to input in the live
window (check them by hand with `cargo run -p app`), libFuzzer runs (they
need `g++` and nightly Rust), and some GPU paths. If you couldn't verify
something, say so in the change rather than claiming it works.

## The real-site corpus

`cargo run -p conformance --release -- run` replays a corpus of recorded
real websites and compares each one against its expected rendering. The
recordings are third-party content, so they are kept out of the repository.
Without them, the corpus test reports the corpus as absent and passes.

## Sign your work

Like the Linux kernel, the project uses the
[Developer Certificate of Origin](https://developercertificate.org/)
(DCO) instead of a contributor license agreement. You keep the copyright
on what you write; by adding a `Signed-off-by:` line to a commit you
certify that you wrote it, or otherwise have the right to submit it under
the project's license. `git commit -s` adds the line for you:

    Signed-off-by: Your Name <you@example.com>

Changes whose commits are not all signed off can't be merged.

## License

The project is licensed under GPL-3.0-only (see [`COPYING`](COPYING)), and
contributions are accepted under the same license.
