//! Milestone 32 acceptance, in `cargo test`: every recorded page replays
//! offline without a crash or a timeout, and every page that renders
//! deterministically matches its golden scene (`conformance run --bless`
//! regenerates them after an intended rendering change).
//!
//! The recordings are not in the repository (`.gitignore`; they are ~23 MB
//! of third-party pages). Without them this test says so and passes; with
//! `CONFORMANCE_REQUIRED=1` (for a machine that has them) it fails instead.
//! Rebuild them with `conformance record` + `conformance run --bless`.

use conformance::runner::{self, Status};
use conformance::snapshot::Snapshot;

#[test]
fn every_snapshot_replays_without_a_crash_or_timeout_and_matches_its_golden() {
    let root = conformance::corpus_root();
    let names = Snapshot::list(&root.join("snapshots"));
    if names.is_empty() && std::env::var_os("CONFORMANCE_REQUIRED").is_none() {
        eprintln!("conformance corpus not present (conformance/snapshots/ is not in git); skipped");
        return;
    }
    assert!(names.len() >= 50, "Milestone 32 requires at least 50 snapshots; found {}", names.len());
    let mut problems = Vec::new();
    for name in &names {
        let snapshot = match Snapshot::load(&root.join("snapshots"), name) {
            Ok(s) => s,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        let outcome = runner::run(&snapshot);
        match &outcome.status {
            Status::Crashed(reason) => problems.push(format!("{name}: CRASHED: {reason}")),
            Status::TimedOut => problems.push(format!("{name}: timed out")),
            Status::LoadFailed(reason) => problems.push(format!("{name}: load failed: {reason}")),
            Status::Rendered => {}
        }
        if let (Status::Rendered, Some(true), Some(scene)) = (&outcome.status, outcome.deterministic, &outcome.scene) {
            let golden = root.join("golden").join(format!("{name}.scene.json"));
            match std::fs::read_to_string(&golden) {
                Ok(expected) if expected == engine::scene_to_json(scene) => {}
                Ok(_) => problems.push(format!("{name}: scene differs from {} (run `conformance run {name}` and diff)", golden.display())),
                Err(_) => problems.push(format!("{name}: no golden (run `conformance run --bless`)")),
            }
        }
    }
    assert!(problems.is_empty(), "{} problems:\n  {}", problems.len(), problems.join("\n  "));
}
