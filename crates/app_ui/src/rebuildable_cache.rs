//! Safe disk maintenance for BowEcho-owned, rebuildable radar/map caches.
//!
//! This deliberately does not know about model stores, satellite stores,
//! user imports, settings, palettes, or any other durable data. Callers pass
//! the exact allowed roots and protected paths; every destructive operation
//! revalidates those boundaries and never follows links/reparse points.

use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheMaintenanceSummary {
    pub(crate) bytes_before: u64,
    pub(crate) bytes_after: u64,
    pub(crate) files_removed: usize,
    pub(crate) files_skipped: usize,
    pub(crate) dirs_removed: usize,
    pub(crate) links_skipped: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CacheResetSummary {
    pub(crate) bytes_removed: u64,
    pub(crate) files_removed: usize,
    pub(crate) dirs_removed: usize,
    pub(crate) links_removed: usize,
}

#[derive(Debug)]
struct CacheFile {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// Enforce one aggregate byte limit across all supplied rebuildable roots.
/// `limit_bytes == 0` means unlimited and performs no scan.
pub(crate) fn prune_to_limit(
    roots: &[PathBuf],
    allowed_roots: &[PathBuf],
    protected_paths: &[PathBuf],
    limit_bytes: u64,
) -> Result<CacheMaintenanceSummary, String> {
    if limit_bytes == 0 {
        return Ok(CacheMaintenanceSummary::default());
    }
    let roots = validated_unique_roots(roots, allowed_roots, protected_paths)?;
    let mut files = Vec::new();
    let mut summary = CacheMaintenanceSummary::default();
    for root in &roots {
        if !root.exists() {
            continue;
        }
        scan_cache_tree(root, &mut files, &mut summary.links_skipped)?;
    }
    summary.bytes_before = files.iter().map(|file| file.bytes).sum();
    summary.bytes_after = summary.bytes_before;
    if summary.bytes_after <= limit_bytes {
        return Ok(summary);
    }

    files.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    prune_sorted_files_to_limit(files, limit_bytes, &mut summary, |path| {
        fs::remove_file(path)
    });
    for root in &roots {
        if root.exists() {
            // Directory cleanup is cosmetic. A transient Windows directory
            // handle must not turn an otherwise successful byte-cap prune
            // into a failure after the file work is already complete.
            if let Ok(removed) = remove_empty_cache_dirs(root, false) {
                summary.dirs_removed += removed;
            }
        }
    }
    Ok(summary)
}

fn prune_sorted_files_to_limit(
    files: Vec<CacheFile>,
    limit_bytes: u64,
    summary: &mut CacheMaintenanceSummary,
    mut remove_file: impl FnMut(&Path) -> std::io::Result<()>,
) {
    for file in files {
        if summary.bytes_after <= limit_bytes {
            break;
        }
        match remove_file(&file.path) {
            Ok(()) => {
                summary.bytes_after = summary.bytes_after.saturating_sub(file.bytes);
                summary.files_removed += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                summary.bytes_after = summary.bytes_after.saturating_sub(file.bytes);
            }
            Err(_) => {
                // Windows may hold the active radar file open. One lock must
                // not disable recycling: leave it in the accounting and keep
                // walking newer candidates until the cap is met or exhausted.
                summary.files_skipped += 1;
            }
        }
    }
}

/// Delete and recreate only the validated rebuildable roots. Links/reparse
/// points encountered below a root are removed as links, never traversed.
pub(crate) fn reset_roots(
    roots: &[PathBuf],
    allowed_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<CacheResetSummary, String> {
    let roots = validated_unique_roots(roots, allowed_roots, protected_paths)?;
    let mut summary = CacheResetSummary::default();
    for root in roots {
        if root.exists() {
            reset_cache_dir_contents(&root, &mut summary)?;
        }
        fs::create_dir_all(&root)
            .map_err(|error| format!("recreate cache root {}: {error}", root.display()))?;
    }
    Ok(summary)
}

fn validated_unique_roots(
    roots: &[PathBuf],
    allowed_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    if roots.is_empty() {
        return Err("no rebuildable cache root is available".to_owned());
    }
    let allowed = allowed_roots
        .iter()
        .map(|path| absolute_without_following(path))
        .collect::<Result<Vec<_>, _>>()?;
    let protected = protected_paths
        .iter()
        .map(|path| absolute_without_following(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut validated = Vec::new();
    for root in roots {
        let root = absolute_without_following(root)?;
        if root.parent().is_none() {
            return Err(format!(
                "refusing filesystem-root cache path {}",
                root.display()
            ));
        }
        reject_linked_path_components(&root)?;
        if !allowed.iter().any(|allowed| allowed == &root) {
            return Err(format!(
                "refusing unrecognized cache path {}",
                root.display()
            ));
        }
        if protected
            .iter()
            .any(|protected| protected == &root || protected.starts_with(&root))
        {
            return Err(format!(
                "refusing cache path that contains protected data: {}",
                root.display()
            ));
        }
        if root.exists() {
            let metadata = fs::symlink_metadata(&root)
                .map_err(|error| format!("inspect cache root {}: {error}", root.display()))?;
            if is_link_or_reparse(&metadata) {
                return Err(format!(
                    "refusing linked/reparse cache root {}",
                    root.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!("cache root is not a directory: {}", root.display()));
            }
        }
        if !validated.contains(&root) {
            validated.push(root);
        }
    }
    // If a custom layout nests one rebuildable root below another, process
    // the outer root once. Resetting both independently would delete the
    // child while walking the parent and then recreate a surprising subtree.
    validated.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    let mut unique = Vec::<PathBuf>::new();
    for root in validated {
        if !unique.iter().any(|parent| root.starts_with(parent)) {
            unique.push(root);
        }
    }
    Ok(unique)
}

fn absolute_without_following(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("refusing an empty cache path".to_owned());
    }
    std::path::absolute(path)
        .map_err(|error| format!("resolve cache path {}: {error}", path.display()))
}

fn reject_linked_path_components(path: &Path) -> Result<(), String> {
    // `symlink_metadata(path)` does not follow the final component, but it
    // still traverses linked parents. Inspect every existing prefix so cache
    // maintenance can never escape through a junction/symlink in the path.
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "inspect cache path {}: {error}",
                    ancestor.display()
                ));
            }
        };
        if is_link_or_reparse(&metadata) {
            return Err(format!(
                "refusing cache path through a link/reparse point: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn scan_cache_tree(
    root: &Path,
    files: &mut Vec<CacheFile>,
    links_skipped: &mut usize,
) -> Result<(), String> {
    for entry in fs::read_dir(root)
        .map_err(|error| format!("read cache directory {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect cache entry {}: {error}", path.display()))?;
        if is_link_or_reparse(&metadata) {
            *links_skipped += 1;
            continue;
        }
        if metadata.is_dir() {
            scan_cache_tree(&path, files, links_skipped)?;
        } else if metadata.is_file() {
            files.push(CacheFile {
                path,
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    Ok(())
}

fn remove_empty_cache_dirs(root: &Path, remove_root: bool) -> Result<usize, String> {
    let mut removed = 0;
    for entry in fs::read_dir(root)
        .map_err(|error| format!("read cache directory {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect cache entry {}: {error}", path.display()))?;
        if metadata.is_dir() && !is_link_or_reparse(&metadata) {
            removed += remove_empty_cache_dirs(&path, true)?;
        }
    }
    if remove_root
        && fs::read_dir(root)
            .map_err(|error| format!("read cache directory {}: {error}", root.display()))?
            .next()
            .is_none()
    {
        fs::remove_dir(root)
            .map_err(|error| format!("remove empty cache directory {}: {error}", root.display()))?;
        removed += 1;
    }
    Ok(removed)
}

fn reset_cache_dir_contents(root: &Path, summary: &mut CacheResetSummary) -> Result<(), String> {
    for entry in fs::read_dir(root)
        .map_err(|error| format!("read cache directory {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
        remove_cache_entry_without_following(&entry.path(), summary)?;
    }
    Ok(())
}

fn remove_cache_entry_without_following(
    path: &Path,
    summary: &mut CacheResetSummary,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("inspect cache entry {}: {error}", path.display()))?;
    if is_link_or_reparse(&metadata) {
        remove_link_itself(path)?;
        summary.links_removed += 1;
    } else if metadata.is_dir() {
        reset_cache_dir_contents(path, summary)?;
        fs::remove_dir(path)
            .map_err(|error| format!("remove cache directory {}: {error}", path.display()))?;
        summary.dirs_removed += 1;
    } else {
        summary.bytes_removed = summary.bytes_removed.saturating_add(metadata.len());
        fs::remove_file(path)
            .map_err(|error| format!("remove cache file {}: {error}", path.display()))?;
        summary.files_removed += 1;
    }
    Ok(())
}

fn remove_link_itself(path: &Path) -> Result<(), String> {
    fs::remove_file(path)
        .or_else(|file_error| {
            fs::remove_dir(path).map_err(|dir_error| {
                std::io::Error::new(
                    dir_error.kind(),
                    format!("remove_file: {file_error}; remove_dir: {dir_error}"),
                )
            })
        })
        .map_err(|error| format!("remove cache link {}: {error}", path.display()))
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::io::Write;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bowecho-rebuildable-cache-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_at(path: &Path, bytes: usize, age_seconds: u64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&vec![0x5a; bytes]).unwrap();
        file.set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(age_seconds)),
        )
        .unwrap();
    }

    #[test]
    fn path_guard_rejects_unknown_and_protected_roots() {
        let base = scratch("guard");
        let allowed = base.join("cache");
        let unknown = base.join("not-cache");
        let protected = allowed.join("model-store");
        assert!(
            prune_to_limit(
                std::slice::from_ref(&unknown),
                std::slice::from_ref(&allowed),
                &[],
                1
            )
            .is_err()
        );
        assert!(
            reset_roots(
                std::slice::from_ref(&allowed),
                std::slice::from_ref(&allowed),
                std::slice::from_ref(&protected)
            )
            .is_err()
        );
    }

    #[test]
    fn prune_removes_oldest_files_until_aggregate_limit() {
        let base = scratch("prune");
        let radar = base.join("cache");
        let tiles = base.join("tiles");
        write_at(&radar.join("old.bin"), 6, 300);
        write_at(&tiles.join("middle.bin"), 6, 200);
        write_at(&radar.join("new.bin"), 6, 100);
        let roots = vec![radar.clone(), tiles.clone()];

        let summary = prune_to_limit(&roots, &roots, &[], 12).unwrap();

        assert_eq!(summary.bytes_before, 18);
        assert_eq!(summary.bytes_after, 12);
        assert_eq!(summary.files_removed, 1);
        assert!(!radar.join("old.bin").exists());
        assert!(tiles.join("middle.bin").exists());
        assert!(radar.join("new.bin").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn prune_skips_locked_old_file_and_keeps_recycling_newer_files() {
        let old = PathBuf::from("locked-old.bin");
        let middle = PathBuf::from("middle.bin");
        let new = PathBuf::from("new.bin");
        let files = vec![
            CacheFile {
                path: old.clone(),
                bytes: 6,
                modified: UNIX_EPOCH,
            },
            CacheFile {
                path: middle.clone(),
                bytes: 6,
                modified: UNIX_EPOCH + Duration::from_secs(1),
            },
            CacheFile {
                path: new.clone(),
                bytes: 6,
                modified: UNIX_EPOCH + Duration::from_secs(2),
            },
        ];
        let mut summary = CacheMaintenanceSummary {
            bytes_before: 18,
            bytes_after: 18,
            ..CacheMaintenanceSummary::default()
        };
        let mut removed = Vec::new();

        prune_sorted_files_to_limit(files, 12, &mut summary, |path| {
            if path == old {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "simulated Windows lock",
                ))
            } else {
                removed.push(path.to_path_buf());
                Ok(())
            }
        });

        assert_eq!(summary.files_skipped, 1);
        assert_eq!(summary.files_removed, 1);
        assert_eq!(summary.bytes_after, 12);
        assert_eq!(removed, vec![middle]);
        assert!(!removed.contains(&new));
    }

    #[test]
    fn reset_keeps_allowed_roots_and_removes_only_their_contents() {
        let base = scratch("reset");
        let cache = base.join("cache");
        let outside = base.join("user-wrfout.nc");
        write_at(&cache.join("level2/KTLX/frame"), 9, 5);
        write_at(&outside, 7, 5);

        let summary = reset_roots(
            std::slice::from_ref(&cache),
            std::slice::from_ref(&cache),
            std::slice::from_ref(&outside),
        )
        .unwrap();

        assert!(cache.is_dir());
        assert!(fs::read_dir(&cache).unwrap().next().is_none());
        assert!(outside.is_file());
        assert_eq!(summary.files_removed, 1);
        assert_eq!(summary.bytes_removed, 9);
        let _ = fs::remove_dir_all(base);
    }
}
