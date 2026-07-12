//! Lazy, hash-qualified acquisition of the official WRF P3 v5.4 tables.
//!
//! The official two- and three-moment files are too large to embed in every
//! BowEcho binary. They are downloaded only when a P3 50--53 simulation needs
//! the matching table, stored in the application model cache, and accepted
//! only after the strict `radar_scattering` byte-length, SHA-256, header, and
//! record-layout checks pass.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use radar_scattering::{P3OfficialTableKind, P3OfficialTableV54};

const P3_CACHE_FAMILY: &str = "bowecho-simradar/p3-v5.4";

// Successes are process-lifetime values so evaluators can borrow them without
// leaking memory or using unsafe code. Failures are deliberately not cached:
// a later build may retry after connectivity or filesystem permissions change.
static TWO_MOMENT_TABLE: OnceLock<Arc<P3OfficialTableV54>> = OnceLock::new();
static THREE_MOMENT_TABLE: OnceLock<Arc<P3OfficialTableV54>> = OnceLock::new();
static TABLE_LOAD_LOCK: Mutex<()> = Mutex::new(());

fn table_slot(kind: P3OfficialTableKind) -> &'static OnceLock<Arc<P3OfficialTableV54>> {
    match kind {
        P3OfficialTableKind::TwoMoment => &TWO_MOMENT_TABLE,
        P3OfficialTableKind::ThreeMoment => &THREE_MOMENT_TABLE,
    }
}

/// Cache directory containing only exact, validator-qualified official P3
/// files. This follows BowEcho's data-directory override through the shared
/// model-cache path.
#[must_use]
pub fn official_p3_cache_dir() -> PathBuf {
    settings::model_cache_dir().join(P3_CACHE_FAMILY)
}

/// Expected local path for one official table.
#[must_use]
pub fn official_p3_table_path(kind: P3OfficialTableKind) -> PathBuf {
    official_p3_cache_dir().join(kind.asset_spec().file_name)
}

/// Load the exact table required by a P3 scheme, downloading the pinned WRF
/// source asset on first use. A corrupt or partial app-owned cache entry is
/// removed before retrying; the replacement is still rejected unless the full
/// strict table parser accepts it.
pub fn load_or_download_official_p3_table(
    kind: P3OfficialTableKind,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<Arc<P3OfficialTableV54>, String> {
    let slot = table_slot(kind);
    if let Some(table) = slot.get() {
        return Ok(Arc::clone(table));
    }
    let _load_guard = TABLE_LOAD_LOCK
        .lock()
        .map_err(|_| "official P3 table cache lock was poisoned".to_owned())?;
    if let Some(table) = slot.get() {
        return Ok(Arc::clone(table));
    }

    let spec = kind.asset_spec();
    let path = official_p3_table_path(kind);
    let table = match P3OfficialTableV54::load_path(kind, &path) {
        Ok(table) => {
            progress(&format!(
                "using cached official P3 {} table ({})",
                spec.table_version, spec.file_name
            ));
            table
        }
        Err(existing_error) => {
            if path.exists() {
                progress(&format!(
                    "cached P3 table failed validation ({existing_error}); replacing it from the pinned WRF source"
                ));
                remove_app_cache_file(&path)?;
            } else {
                progress(&format!(
                    "downloading official P3 {} table ({:.1} MiB)…",
                    spec.table_version,
                    spec.expected_bytes as f64 / 1_048_576.0
                ));
            }
            data_source::gdex::download_to_path(spec.source_url, &path).map_err(|error| {
                format!(
                    "download official P3 {} table from {}: {error}",
                    spec.table_version, spec.source_url
                )
            })?;
            match P3OfficialTableV54::load_path(kind, &path) {
                Ok(table) => table,
                Err(error) => {
                    let _ = remove_app_cache_file(&path);
                    return Err(format!(
                        "downloaded P3 {} table failed its pinned validation and was removed: {error}",
                        spec.table_version
                    ));
                }
            }
        }
    };
    progress(&format!(
        "qualified official P3 {} table: SHA-256 {}",
        spec.table_version, spec.expected_sha256
    ));
    let table = Arc::new(table);
    // A single initialization lock makes this set deterministic, but retain
    // the race-safe fallback if the implementation changes later.
    let _ = slot.set(Arc::clone(&table));
    Ok(Arc::clone(
        slot.get()
            .expect("a qualified official P3 table was installed"),
    ))
}

/// Static reference for a reusable evaluator. The table is acquired and
/// qualified exactly as in [`load_or_download_official_p3_table`], then kept
/// by the success-only process cache.
pub fn load_or_download_official_p3_table_ref(
    kind: P3OfficialTableKind,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<&'static P3OfficialTableV54, String> {
    load_or_download_official_p3_table(kind, progress)?;
    Ok(table_slot(kind)
        .get()
        .expect("successful P3 acquisition populated its process cache")
        .as_ref())
}

fn remove_app_cache_file(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path)
        .map_err(|error| format!("remove invalid P3 cache file {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_preserve_exact_official_file_names() {
        for kind in [
            P3OfficialTableKind::TwoMoment,
            P3OfficialTableKind::ThreeMoment,
        ] {
            assert_eq!(
                official_p3_table_path(kind).file_name().unwrap(),
                kind.asset_spec().file_name
            );
        }
    }

    #[test]
    fn cache_family_is_scoped_away_from_replaceable_render_bricks() {
        assert!(P3_CACHE_FAMILY.contains("simradar"));
        assert!(!P3_CACHE_FAMILY.contains("simsat-cache"));
    }
}
