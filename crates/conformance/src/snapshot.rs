//! Recorded network snapshots: a page's URL, viewport and every response the
//! engine received while loading it — the "snapshot" of architecture doc §9
//! (URL + HTML + CSS + responses + viewport), so a page renders identically,
//! offline, forever after.
//!
//! On disk, `conformance/snapshots/<name>/`:
//!
//! - `manifest` — text, one record per line, tab-separated:
//!   `url`, `viewport`, `category`, `license`, `recorded`, then
//!   `response <index> <status> <requested url> <final url>` and
//!   `header <index> <name> <value>` lines;
//! - `bodies/<index>` — each response body, deflate-compressed.
//!
//! The manifest is our own data, but it is still parsed defensively (a
//! malformed line is an error for that snapshot, never a panic).

use std::path::{Path, PathBuf};

/// One recorded response.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub requested_url: String,
    pub status: u16,
    pub final_url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub name: String,
    pub url: String,
    pub viewport: (f32, f32),
    pub category: String,
    /// Where the content comes from and under what licence (third-party
    /// content, recorded for interoperability testing).
    pub license: String,
    pub recorded: String,
    pub responses: Vec<Recorded>,
}

/// Largest body kept when recording; larger ones (usually big images) are
/// left out and replay as 404 — the corpus has to fit in git.
pub const MAX_RECORDED_BODY_BYTES: usize = 512 * 1024;

/// Compressed bytes of IMAGES one snapshot may keep; images past it are left
/// out (they replay as 404: a missing image, which the engine handles).
/// Documents, stylesheets and scripts — what layout conformance is about —
/// are always kept. Image-heavy pages get a bigger allowance.
pub const MAX_SNAPSHOT_IMAGE_BYTES: usize = 384 * 1024;
pub const MAX_IMAGE_HEAVY_SNAPSHOT_IMAGE_BYTES: usize = 1536 * 1024;

fn is_image(r: &Recorded) -> bool {
    let content_type = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.to_ascii_lowercase());
    match content_type {
        Some(ct) => ct.starts_with("image/"),
        None => {
            let path = r.requested_url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
            [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico", ".avif"].iter().any(|ext| path.ends_with(ext))
        }
    }
}

fn clean(field: &str) -> String {
    field.replace(['\t', '\n', '\r'], " ")
}

impl Snapshot {
    pub fn dir(root: &Path, name: &str) -> PathBuf {
        root.join(name)
    }

    /// Writes the snapshot to `root/<name>/`, applying the size budget.
    /// Returns how many responses were left out for size.
    pub fn save(&self, root: &Path) -> std::io::Result<usize> {
        let dir = Self::dir(root, &self.name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        std::fs::create_dir_all(dir.join("bodies"))?;
        let mut manifest = String::from("# browser-engine conformance snapshot v1\n");
        manifest.push_str(&format!("url\t{}\n", clean(&self.url)));
        manifest.push_str(&format!("viewport\t{}\t{}\n", self.viewport.0, self.viewport.1));
        manifest.push_str(&format!("category\t{}\n", clean(&self.category)));
        manifest.push_str(&format!("license\t{}\n", clean(&self.license)));
        manifest.push_str(&format!("recorded\t{}\n", clean(&self.recorded)));
        let image_budget = if self.category == "image-heavy" { MAX_IMAGE_HEAVY_SNAPSHOT_IMAGE_BYTES } else { MAX_SNAPSHOT_IMAGE_BYTES };
        let mut image_total = 0usize;
        let mut skipped = 0usize;
        let mut index = 0usize;
        for (i, r) in self.responses.iter().enumerate() {
            let is_document = i == 0;
            if !is_document && r.body.len() > MAX_RECORDED_BODY_BYTES {
                skipped += 1;
                continue;
            }
            let compressed = miniz_oxide::deflate::compress_to_vec(&r.body, 9);
            if !is_document && is_image(r) {
                if image_total + compressed.len() > image_budget {
                    skipped += 1;
                    continue;
                }
                image_total += compressed.len();
            }
            std::fs::write(dir.join("bodies").join(index.to_string()), &compressed)?;
            manifest.push_str(&format!("response\t{index}\t{}\t{}\t{}\n", r.status, clean(&r.requested_url), clean(&r.final_url)));
            for (k, v) in &r.headers {
                manifest.push_str(&format!("header\t{index}\t{}\t{}\n", clean(k), clean(v)));
            }
            index += 1;
        }
        std::fs::write(dir.join("manifest"), manifest)?;
        Ok(skipped)
    }

    /// Reads `root/<name>/`.
    pub fn load(root: &Path, name: &str) -> Result<Snapshot, String> {
        let dir = Self::dir(root, name);
        let manifest = std::fs::read_to_string(dir.join("manifest")).map_err(|e| format!("{name}: manifest: {e}"))?;
        let mut snapshot = Snapshot {
            name: name.to_string(),
            url: String::new(),
            viewport: (1024.0, 768.0),
            category: String::new(),
            license: String::new(),
            recorded: String::new(),
            responses: Vec::new(),
        };
        let bad = |line: &str| format!("{name}: malformed manifest line {line:?}");
        for line in manifest.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["url", url] => snapshot.url = url.to_string(),
                ["viewport", w, h] => {
                    snapshot.viewport = (w.parse().map_err(|_| bad(line))?, h.parse().map_err(|_| bad(line))?);
                }
                ["category", c] => snapshot.category = c.to_string(),
                ["license", l] => snapshot.license = l.to_string(),
                ["recorded", r] => snapshot.recorded = r.to_string(),
                ["response", index, status, requested, final_url] => {
                    let index: usize = index.parse().map_err(|_| bad(line))?;
                    if index != snapshot.responses.len() {
                        return Err(bad(line));
                    }
                    let compressed = std::fs::read(dir.join("bodies").join(index.to_string())).map_err(|e| format!("{name}: body {index}: {e}"))?;
                    let body = miniz_oxide::inflate::decompress_to_vec_with_limit(&compressed, MAX_RECORDED_BODY_BYTES.max(64 * 1024 * 1024))
                        .map_err(|e| format!("{name}: body {index}: {e:?}"))?;
                    snapshot.responses.push(Recorded {
                        requested_url: requested.to_string(),
                        status: status.parse().map_err(|_| bad(line))?,
                        final_url: final_url.to_string(),
                        headers: Vec::new(),
                        body,
                    });
                }
                ["header", index, key, value] => {
                    let index: usize = index.parse().map_err(|_| bad(line))?;
                    let response = snapshot.responses.get_mut(index).ok_or_else(|| bad(line))?;
                    response.headers.push((key.to_string(), value.to_string()));
                }
                _ => return Err(bad(line)),
            }
        }
        if snapshot.url.is_empty() || snapshot.responses.is_empty() {
            return Err(format!("{name}: no url or no responses"));
        }
        Ok(snapshot)
    }

    /// Every snapshot name under `root`, sorted.
    pub fn list(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().join("manifest").exists())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_round_trips_through_disk_and_applies_the_budget() {
        let dir = std::env::temp_dir().join(format!("conformance-snapshot-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let snapshot = Snapshot {
            name: "t".into(),
            url: "https://example.test/".into(),
            viewport: (800.0, 600.0),
            category: "docs".into(),
            license: "test\tdata".into(),
            recorded: "2026-09-23".into(),
            responses: vec![
                Recorded { requested_url: "https://example.test/".into(), status: 200, final_url: "https://example.test/".into(), headers: vec![("Content-Type".into(), "text/html".into())], body: b"<html><body>hi</body></html>".to_vec() },
                Recorded { requested_url: "https://example.test/big.png".into(), status: 200, final_url: "https://example.test/big.png".into(), headers: vec![], body: vec![7; MAX_RECORDED_BODY_BYTES + 1] },
                Recorded { requested_url: "https://example.test/s.css".into(), status: 404, final_url: "https://example.test/s.css".into(), headers: vec![], body: b"nope".to_vec() },
            ],
        };
        let skipped = snapshot.save(&dir).unwrap();
        assert_eq!(skipped, 1, "the oversized body is left out");
        let loaded = Snapshot::load(&dir, "t").unwrap();
        assert_eq!(loaded.responses.len(), 2);
        assert_eq!(loaded.responses[0], snapshot.responses[0]);
        assert_eq!(loaded.responses[1].status, 404);
        assert_eq!(loaded.license, "test data", "tabs cannot break the format");
        assert_eq!(Snapshot::list(&dir), vec!["t".to_string()]);
        std::fs::write(dir.join("t/manifest"), "url\thttps://x/\nresponse\tnot-a-number\n").unwrap();
        assert!(Snapshot::load(&dir, "t").is_err(), "malformed: an error, not a panic");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
