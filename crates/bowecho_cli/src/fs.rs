use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::CliError;

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_DISCOVERED_INPUTS: usize = 100_000;
const MAX_DISCOVERY_DEPTH: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashReceipt {
    pub bytes: u64,
    pub sha256: String,
}

/// Stream a file through SHA-256 without loading a wrfout into memory.
pub fn sha256_file(path: &Path) -> io::Result<HashReceipt> {
    let file = File::open(path)?;
    sha256_reader(file)
}

pub fn sha256_reader(reader: impl Read) -> io::Result<HashReceipt> {
    let mut reader = BufReader::with_capacity(HASH_BUFFER_BYTES, reader);
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file byte count overflow"))?;
    }
    Ok(HashReceipt {
        bytes,
        sha256: format!("{:x}", hasher.finalize()),
    })
}

/// Write JSON to a same-directory temporary file, flush it, and atomically
/// replace the destination. Source wrfout files are never passed here.
pub fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value).map_err(io::Error::other)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    // Persisting the directory entry before returning matters for manifests
    // used as deletion authorization after a crash.
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Publish an already-rendered staging file through a temporary file in the
/// destination directory. Copying into that directory before the final rename
/// keeps publication atomic even when the renderer's staging path is on a
/// different filesystem. The caller retains ownership of `staged`.
pub fn publish_file_atomic(staged: &Path, destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut source = File::open(staged)?;
    if !source.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "staged artifact is not a regular file: {}",
                staged.display()
            ),
        ));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    io::copy(&mut source, &mut temporary)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    drop(source);
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;

    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Deterministically expand one explicit file or recursively discover
/// NetCDF/HDF5 candidates in a directory. Symlinks are not followed.
pub fn expand_input_files(input: &Path) -> Result<Vec<PathBuf>, CliError> {
    let metadata = fs::symlink_metadata(input)
        .map_err(|error| CliError::input(format!("cannot inspect {}: {error}", input.display())))?;
    if metadata.file_type().is_symlink() {
        return Err(CliError::input(format!(
            "refusing symlink input {}",
            input.display()
        )));
    }
    if metadata.is_file() {
        return Ok(vec![absolute_without_canonicalizing(input)?]);
    }
    if !metadata.is_dir() {
        return Err(CliError::input(format!(
            "input is neither a regular file nor directory: {}",
            input.display()
        )));
    }
    let files = discover_input_files(input)?;
    if files.is_empty() {
        return Err(CliError::input(format!(
            "no NetCDF/HDF5 candidates found under {}",
            input.display()
        )));
    }
    Ok(files)
}

/// Discover current NetCDF/HDF5 candidates beneath a live directory. Unlike
/// [`expand_input_files`], an empty directory is a valid snapshot: a watcher
/// waits for the producer instead of treating "nothing written yet" as a
/// fatal input error. Symlinks are still refused/skipped and ordering remains
/// deterministic.
pub fn discover_input_files(directory: &Path) -> Result<Vec<PathBuf>, CliError> {
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        CliError::input(format!("cannot inspect {}: {error}", directory.display()))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(CliError::input(format!(
            "refusing symlink input {}",
            directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(CliError::input(format!(
            "watch input is not a directory: {}",
            directory.display()
        )));
    }
    let mut files = Vec::new();
    visit_directory(directory, 0, &mut files)?;
    files.sort_by_key(|path| stable_path_key(path));
    files.dedup();
    Ok(files)
}

fn visit_directory(
    directory: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), CliError> {
    if depth > MAX_DISCOVERY_DEPTH {
        return Err(CliError::input(format!(
            "input discovery exceeds the safe directory depth of {MAX_DISCOVERY_DEPTH} at {}",
            directory.display()
        )));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            CliError::input(format!(
                "cannot read directory {}: {error}",
                directory.display()
            ))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            CliError::input(format!("cannot enumerate {}: {error}", directory.display()))
        })?;
    entries.sort_by_key(|entry| stable_path_key(&entry.path()));

    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| CliError::input(format!("cannot stat {}: {error}", path.display())))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            visit_directory(&path, depth + 1, files)?;
        } else if metadata.is_file() && is_netcdf_candidate(&path) {
            files.push(absolute_without_canonicalizing(&path)?);
            if files.len() > MAX_DISCOVERED_INPUTS {
                return Err(CliError::input(format!(
                    "input discovery exceeds the safe limit of {MAX_DISCOVERED_INPUTS} files"
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn is_netcdf_candidate(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension_match = matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "nc" | "nc4" | "cdf" | "h5" | "hdf5"
    );
    if extension_match
        || (name.starts_with("wrfout")
            && path
                .extension()
                .is_none_or(|extension| extension.is_empty()))
    {
        return true;
    }

    let mut signature = [0_u8; 8];
    File::open(path)
        .and_then(|mut file| file.read(&mut signature))
        .is_ok_and(|read| netcrust::looks_like_netcdf(&signature[..read]))
}

fn absolute_without_canonicalizing(path: &Path) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| CliError::input(format!("cannot resolve current directory: {error}")))
}

fn stable_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Resolve a manifest-relative path while rejecting traversal. Artifact
/// receipts must remain beneath the manifest directory; source receipts may
/// be absolute because wrfout retention is managed outside BowEcho.
pub fn resolve_receipt_path(
    manifest_directory: &Path,
    declared: &Path,
    allow_absolute: bool,
) -> Result<PathBuf, String> {
    if declared.as_os_str().is_empty() {
        return Err("receipt path is empty".into());
    }
    if declared.is_absolute() {
        return allow_absolute
            .then(|| declared.to_path_buf())
            .ok_or_else(|| "artifact receipt path must be relative".into());
    }
    if !allow_absolute
        && declared.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "receipt path escapes the manifest directory: {}",
            declared.display()
        ));
    }
    Ok(manifest_directory.join(declared))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn hashes_stream_with_known_sha256() {
        let receipt = sha256_reader(&b"abc"[..]).unwrap();
        assert_eq!(receipt.bytes, 3);
        assert_eq!(
            receipt.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn discovery_is_recursive_deterministic_and_skips_non_candidates() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("z.nc"), b"CDF\x01data").unwrap();
        fs::write(
            root.path().join("nested").join("a"),
            b"\x89HDF\r\n\x1a\ndata",
        )
        .unwrap();
        fs::write(root.path().join("ignore.txt"), b"not netcdf").unwrap();
        fs::write(root.path().join("wrfout_preview.png"), b"not a wrfout").unwrap();

        let paths = expand_input_files(root.path()).unwrap();
        assert_eq!(paths.len(), 2);
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["a", "z.nc"]);
    }

    #[test]
    fn live_discovery_allows_an_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        assert!(discover_input_files(root.path()).unwrap().is_empty());
        assert!(expand_input_files(root.path()).is_err());
    }

    #[test]
    fn atomic_json_replaces_existing_complete_document() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("manifest.json");
        fs::write(&path, b"old").unwrap();
        write_json_atomic(&path, &serde_json::json!({"complete": true})).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["complete"], true);
    }

    #[test]
    fn atomic_file_publish_replaces_destination_and_preserves_staging() {
        let root = tempfile::tempdir().unwrap();
        let staged = root.path().join("render-staging.png");
        let destination = root.path().join("published").join("plot.png");
        fs::write(&staged, b"new complete image").unwrap();
        fs::create_dir(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"old image").unwrap();

        publish_file_atomic(&staged, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"new complete image");
        assert_eq!(fs::read(&staged).unwrap(), b"new complete image");
    }

    #[test]
    fn artifacts_cannot_escape_manifest_directory() {
        assert!(resolve_receipt_path(Path::new("out"), Path::new("../raw"), false).is_err());
        assert_eq!(
            resolve_receipt_path(Path::new("out"), Path::new("d01/a.webp"), false).unwrap(),
            PathBuf::from("out/d01/a.webp")
        );
    }
}
