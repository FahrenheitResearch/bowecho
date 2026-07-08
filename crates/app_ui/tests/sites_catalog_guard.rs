//! Altitude guard for the one-site-model migration (v0.29 spec Phase 3).
//!
//! After Phase 3, feature code queries the union catalog through
//! `data_source::sites` (`resolve` / `all_sites` / `sites_near`) instead
//! of iterating `self.sites` or `intl_static_sites()` raw — raw iteration
//! is index-keyed identity, the exact bug class the `SiteRef` migration
//! kills. Both catalogs cannot yet be made private (the legacy loaders in
//! main.rs still hold `Vec<RadarSite>`), so this test pins the EXACT
//! occurrence count per allowed file: any NEW raw catalog iteration fails
//! CI with a message pointing at the catalog API, and any removal must
//! lower the pinned count in the same commit (ratchet downward only).
//!
//! Counting is whitespace-normalized because rustfmt splits method chains
//! (`self\n.sites`) — a naive single-line grep undercounts by a dozen at
//! HEAD. Occurrences inside doc comments and `#[cfg(test)]` fixtures count
//! too: the pin is a ratchet, not a semantics claim.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// (file, whitespace-normalized `self.sites` occurrences,
/// `intl_static_sites(` occurrences) — measured at the Phase-3 migration
/// commit, re-pinned at the `sites_ui.rs` extraction (site-domain
/// helpers moved out of main.rs; `sites_ui.rs` is the ONE app_ui module
/// deliberately allowed to iterate the raw catalogs), re-pinned again at
/// the Phase-4d `overlays.rs` extraction (the overlay family's four
/// `self.sites` lookups and one `intl_static_sites()` dispatch moved
/// VERBATIM out of main.rs — totals unchanged), re-pinned again at the
/// `map_paint.rs` sub-move C (markers) extraction (the marker painters'
/// five `self.sites` lookups and three `intl_static_sites()` dispatches
/// moved VERBATIM out of main.rs — totals unchanged). Files not listed
/// must contain ZERO of either.
const PINNED: [(&str, usize, usize); 6] = [
    ("event_explorer.rs", 2, 0),
    ("layers_rail.rs", 2, 2),
    ("main.rs", 42, 5),
    ("map_paint.rs", 5, 3),
    ("overlays.rs", 4, 1),
    ("sites_ui.rs", 2, 3),
];

fn rust_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read_dir {} failed: {err}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("dir entry under {} failed: {err}", dir.display()))
            .path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// Occurrences of `needle` in `haystack` NOT followed by an identifier
/// character (so `self.sites` never matches a hypothetical
/// `self.sites_foo`).
fn count_symbol(haystack: &str, needle: &str) -> usize {
    haystack
        .match_indices(needle)
        .filter(|(index, _)| {
            haystack[index + needle.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
        })
        .count()
}

#[test]
fn raw_site_catalog_iteration_stays_pinned_to_the_phase_3_set() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.iter().any(|path| path.ends_with("main.rs")),
        "guard must be scanning the real app_ui src tree"
    );

    let mut found: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read {} failed: {err}", path.display()));
        // rustfmt splits `self\n.sites` across lines: normalize ALL
        // whitespace away before counting.
        let normalized: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        let self_sites = count_symbol(&normalized, "self.sites");
        let intl_static = count_symbol(&normalized, "intl_static_sites(");
        if self_sites > 0 || intl_static > 0 {
            let relative = path
                .strip_prefix(&src)
                .expect("scanned file lives under src/")
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(relative, (self_sites, intl_static));
        }
    }

    let pinned: BTreeMap<String, (usize, usize)> = PINNED
        .iter()
        .map(|&(file, self_sites, intl_static)| (file.to_owned(), (self_sites, intl_static)))
        .collect();
    assert_eq!(
        found, pinned,
        "raw site-catalog iteration changed. New feature code must query \
         the ONE union catalog — data_source::sites::{{resolve, all_sites, \
         sites_near}} (kind-tagged SiteRecords, SiteRef identity) — never \
         iterate self.sites or intl_static_sites() directly (index-keyed \
         identity is the bug class Phase 3 killed). If you removed a raw \
         iteration, ratchet the pinned count DOWN in the same commit; new \
         raw iterations are not accepted."
    );
}
