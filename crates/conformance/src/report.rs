//! The corpus report: one line per page, then every unsupported feature
//! ranked by how many pages use it — the input to the layout (M33) and text
//! (M35) backlogs.

use crate::runner::{Outcome, Status};
use std::collections::BTreeMap;
use std::fmt::Write;

/// An unsupported feature, `(kind, name)`, and how widely it is used:
/// `(pages using it, total occurrences)`.
pub type RankedFeature = ((&'static str, String), (usize, u64));

pub struct Report<'a> {
    pub outcomes: &'a [Outcome],
}

impl Report<'_> {
    /// Pages that crashed or timed out (each needs a regression test).
    pub fn failures(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| matches!(o.status, Status::Crashed(_) | Status::TimedOut)).collect()
    }

    /// (kind, name) → (pages using it, total occurrences), most-used first.
    pub fn ranked_unsupported(&self) -> Vec<RankedFeature> {
        let mut by_feature: BTreeMap<(&'static str, String), (usize, u64)> = BTreeMap::new();
        for outcome in self.outcomes {
            for (kind, name, count) in &outcome.unsupported {
                let entry = by_feature.entry((*kind, name.clone())).or_default();
                entry.0 += 1;
                entry.1 += count;
            }
        }
        let mut ranked: Vec<_> = by_feature.into_iter().collect();
        ranked.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(b.1 .1.cmp(&a.1 .1)).then(a.0.cmp(&b.0)));
        ranked
    }

    pub fn render(&self, top: usize) -> String {
        let mut out = String::new();
        let rendered = self.outcomes.iter().filter(|o| o.status == Status::Rendered).count();
        let _ = writeln!(out, "conformance: {} pages, {} rendered, {} failed to load, {} crashed/timed out", self.outcomes.len(), rendered,
            self.outcomes.iter().filter(|o| matches!(o.status, Status::LoadFailed(_))).count(), self.failures().len());
        let _ = writeln!(out, "\n{:<28} {:<10} {:<12} {:>8} {:>7} {:>6} {:>6} {:>5}", "page", "category", "status", "ms", "items", "unsup", "miss", "det");
        for o in self.outcomes {
            let status = match &o.status {
                Status::Rendered => "ok".to_string(),
                Status::LoadFailed(_) => "load-failed".to_string(),
                Status::Crashed(_) => "CRASHED".to_string(),
                Status::TimedOut => "TIMEOUT".to_string(),
            };
            let items = o.scene.as_ref().map_or(0, |s| s.items.len());
            let det = match o.deterministic {
                Some(true) => "yes",
                Some(false) => "no",
                None => "-",
            };
            let _ = writeln!(out, "{:<28} {:<10} {:<12} {:>8} {:>7} {:>6} {:>6} {:>5}", o.name, o.category, status, o.elapsed.as_millis(), items, o.unsupported.len(), o.misses.len(), det);
        }
        for o in self.outcomes {
            match &o.status {
                Status::Crashed(reason) => {
                    let _ = writeln!(out, "\nCRASH {}: {reason}", o.name);
                }
                Status::LoadFailed(reason) => {
                    let _ = writeln!(out, "\nload failed {}: {reason}", o.name);
                }
                _ => {}
            }
        }
        let _ = writeln!(out, "\nunsupported features, by pages using them (top {top}):");
        let _ = writeln!(out, "{:>5} {:>9}  {:<12} feature", "pages", "count", "kind");
        for ((kind, name), (pages, count)) in self.ranked_unsupported().into_iter().take(top) {
            let _ = writeln!(out, "{pages:>5} {count:>9}  {kind:<12} {name}");
        }
        out
    }
}
