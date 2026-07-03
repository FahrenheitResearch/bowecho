//! Grep-guard for the archive-world unification (v0.29 spec §11
//! Milestone C.4, gated in §11 Gate C).
//!
//! `data_source::level2_objects_for_date` / `level2_objects_for_window`
//! are the verified-good US Level-II archive listers. Phase 2 collapses
//! every archive browse/load surface onto ONE `archive_browser.rs` widget
//! over `ArchiveLister::Us` (spec §5) — so until that lands, NO new
//! direct caller may appear in `app_ui`: a new feature that lists the US
//! archive belongs on the unified path, not on a fifth hand-rolled
//! worker body. This test pins the exact source-file set that mentions
//! either symbol; when Phase 2 moves the callers into
//! `archive_browser.rs`, update the pinned set in the same commit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const GUARDED_SYMBOLS: [&str; 2] = ["level2_objects_for_date", "level2_objects_for_window"];

/// Source files allowed to mention the guarded symbols, relative to the
/// crate's `src/` directory. Phase 2 landed `archive_browser.rs` — the
/// unified widget's `ArchiveLister::Us` wraps both listers, and the
/// browser's day-listing caller moved there from main.rs. The remaining
/// main.rs callers are the US loaders the spec keeps unwrapped
/// (loop-ending-at, event windows, track jumps); event_explorer.rs keeps
/// its window lookup. The Phase-4d extraction moved the overlay
/// archive-window worker body (day-listing caller unchanged) VERBATIM
/// from main.rs into `overlays.rs` — a file move, not a new caller.
const ALLOWED_FILES: [&str; 4] = [
    "archive_browser.rs",
    "event_explorer.rs",
    "main.rs",
    "overlays.rs",
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

#[test]
fn level2_archive_lister_callers_stay_pinned_until_phase_2_unifies_them() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.iter().any(|path| path.ends_with("main.rs")),
        "guard must be scanning the real app_ui src tree"
    );

    let mut found = BTreeSet::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("read {} failed: {err}", path.display()));
        if GUARDED_SYMBOLS.iter().any(|symbol| text.contains(symbol)) {
            let relative = path
                .strip_prefix(&src)
                .expect("scanned file lives under src/")
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(relative);
        }
    }

    let allowed: BTreeSet<String> = ALLOWED_FILES
        .iter()
        .map(|file| (*file).to_owned())
        .collect();
    assert_eq!(
        found, allowed,
        "the US Level-II archive lister caller set changed. New archive \
         listing belongs on the unified Phase-2 path (archive_browser.rs, \
         spec §5) — do not add direct level2_objects_for_* callers. If a \
         caller legitimately moved, update ALLOWED_FILES in the same commit."
    );
}
