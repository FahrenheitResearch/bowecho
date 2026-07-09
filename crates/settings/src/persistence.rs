//! Durable JSON-document I/O shared by `config.json` and `styles.json`.
//!
//! The public helpers deliberately separate reading/parsing from fallback
//! policy. Callers decide what a valid document is; this module guarantees
//! capped reads, same-directory unique temporary files, flushed/synced bytes,
//! atomic publication where the platform exposes it, and a last-known-good
//! `.bak` that is never replaced with malformed JSON.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Settings/style documents are configuration, not data stores. This cap
/// bounds both startup allocation and manually imported files while leaving
/// ample room for serialized workspaces and Brand Kits.
pub const MAX_JSON_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceAction {
    ResolveConfigDirectory,
    CreateParentDirectory,
    OpenDocument,
    ReadDocument,
    DecodeDocument,
    ParseDocument,
    SerializeDocument,
    CreateTemporary,
    WriteTemporary,
    FlushTemporary,
    SyncTemporary,
    PublishDocument,
    PublishBackup,
}

impl PersistenceAction {
    fn description(self) -> &'static str {
        match self {
            Self::ResolveConfigDirectory => "resolve the config directory for",
            Self::CreateParentDirectory => "create the parent directory for",
            Self::OpenDocument => "open",
            Self::ReadDocument => "read",
            Self::DecodeDocument => "decode UTF-8 from",
            Self::ParseDocument => "parse JSON from",
            Self::SerializeDocument => "serialize JSON for",
            Self::CreateTemporary => "create a temporary file beside",
            Self::WriteTemporary => "write a temporary file for",
            Self::FlushTemporary => "flush a temporary file for",
            Self::SyncTemporary => "sync a temporary file for",
            Self::PublishDocument => "atomically replace",
            Self::PublishBackup => "publish the last-known-good backup for",
        }
    }
}

#[derive(Debug)]
pub enum PersistenceErrorKind {
    Io(io::Error),
    TooLarge { limit: u64, actual: u64 },
    InvalidUtf8(String),
    InvalidJson(String),
    Serialization(String),
    NoConfigDirectory,
}

/// Typed, path-bearing persistence failure suitable for both logs and the UI.
#[derive(Debug)]
pub struct PersistenceError {
    pub action: PersistenceAction,
    pub path: PathBuf,
    pub kind: PersistenceErrorKind,
}

impl PersistenceError {
    pub fn io(action: PersistenceAction, path: &Path, error: io::Error) -> Self {
        Self {
            action,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::Io(error),
        }
    }

    pub fn parse(path: &Path, error: impl fmt::Display) -> Self {
        Self {
            action: PersistenceAction::ParseDocument,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::InvalidJson(error.to_string()),
        }
    }

    pub fn serialize(path: &Path, error: impl fmt::Display) -> Self {
        Self {
            action: PersistenceAction::SerializeDocument,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::Serialization(error.to_string()),
        }
    }

    pub fn no_config_directory(document_name: &str) -> Self {
        Self {
            action: PersistenceAction::ResolveConfigDirectory,
            path: PathBuf::from(document_name),
            kind: PersistenceErrorKind::NoConfigDirectory,
        }
    }

    pub fn is_not_found(&self) -> bool {
        matches!(
            &self.kind,
            PersistenceErrorKind::Io(error) if error.kind() == io::ErrorKind::NotFound
        )
    }
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "could not {} {}: ",
            self.action.description(),
            self.path.display()
        )?;
        match &self.kind {
            PersistenceErrorKind::Io(error) => write!(formatter, "{error}"),
            PersistenceErrorKind::TooLarge { limit, actual } => write!(
                formatter,
                "document is {actual} bytes; the safety limit is {limit} bytes"
            ),
            PersistenceErrorKind::InvalidUtf8(error)
            | PersistenceErrorKind::InvalidJson(error)
            | PersistenceErrorKind::Serialization(error) => formatter.write_str(error),
            PersistenceErrorKind::NoConfigDirectory => {
                formatter.write_str("the operating system did not provide a config directory")
            }
        }
    }
}

impl std::error::Error for PersistenceError {}

/// What happened while loading a persisted document. `DefaultsAfterError` is
/// intentionally distinct from `Missing`: only the former warrants an
/// operator-visible warning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentLoadStatus {
    Loaded,
    Missing,
    LoadedWithWarning {
        warning: String,
    },
    RecoveredFromBackup {
        primary_error: String,
        restore_error: Option<String>,
    },
    DefaultsAfterError {
        primary_error: String,
        backup_error: Option<String>,
    },
}

impl DocumentLoadStatus {
    pub fn user_message(&self, label: &str) -> Option<String> {
        match self {
            Self::Loaded | Self::Missing => None,
            Self::LoadedWithWarning { warning } => Some(format!("Loaded {label}, but {warning}")),
            Self::RecoveredFromBackup {
                primary_error,
                restore_error,
            } => Some(match restore_error {
                Some(restore_error) => format!(
                    "Recovered {label} from its last-known-good backup after {primary_error}; the primary file could not be restored: {restore_error}"
                ),
                None => format!(
                    "Recovered {label} from its last-known-good backup after {primary_error}"
                ),
            }),
            Self::DefaultsAfterError {
                primary_error,
                backup_error,
            } => Some(match backup_error {
                Some(backup_error) => format!(
                    "Could not load {label}; using defaults for this session. Primary: {primary_error}. Backup: {backup_error}"
                ),
                None => format!(
                    "Could not load {label}; using defaults for this session: {primary_error}"
                ),
            }),
        }
    }
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("settings.json"));
    name.push(".bak");
    path.with_file_name(name)
}

/// Read one UTF-8 document without trusting metadata or allowing an unbounded
/// `read_to_string` allocation. The extra byte distinguishes exactly-at-limit
/// from oversized files even if the file grows after metadata is read.
pub fn read_text_capped(path: &Path, limit: u64) -> Result<String, PersistenceError> {
    let file = File::open(path)
        .map_err(|error| PersistenceError::io(PersistenceAction::OpenDocument, path, error))?;
    let metadata_len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    if metadata_len > limit {
        return Err(PersistenceError {
            action: PersistenceAction::ReadDocument,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::TooLarge {
                limit,
                actual: metadata_len,
            },
        });
    }

    let mut bytes = Vec::with_capacity(metadata_len.min(limit) as usize);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| PersistenceError::io(PersistenceAction::ReadDocument, path, error))?;
    if bytes.len() as u64 > limit {
        return Err(PersistenceError {
            action: PersistenceAction::ReadDocument,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::TooLarge {
                limit,
                actual: bytes.len() as u64,
            },
        });
    }
    String::from_utf8(bytes).map_err(|error| PersistenceError {
        action: PersistenceAction::DecodeDocument,
        path: path.to_path_buf(),
        kind: PersistenceErrorKind::InvalidUtf8(error.to_string()),
    })
}

pub fn atomic_write_json<T: Serialize>(
    path: &Path,
    value: &T,
    limit: u64,
) -> Result<(), PersistenceError> {
    atomic_write_json_with_backup_validator(path, value, limit, |text| {
        serde_json::from_str::<serde_json::Value>(text).is_ok()
    })
}

pub fn atomic_write_json_with_backup_validator<T, F>(
    path: &Path,
    value: &T,
    limit: u64,
    valid_existing: F,
) -> Result<(), PersistenceError>
where
    T: Serialize,
    F: Fn(&str) -> bool,
{
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| PersistenceError::serialize(path, error))?;
    atomic_write_json_bytes_with_backup_validator(path, &bytes, limit, valid_existing)
}

/// Publish already-validated JSON bytes. Existing primary bytes are copied to
/// `.bak` only when they still parse as JSON, so a corrupt primary can never
/// destroy the last-known-good recovery copy.
pub fn atomic_write_json_bytes(
    path: &Path,
    bytes: &[u8],
    limit: u64,
) -> Result<(), PersistenceError> {
    atomic_write_json_bytes_with_backup_validator(path, bytes, limit, |text| {
        serde_json::from_str::<serde_json::Value>(text).is_ok()
    })
}

pub fn atomic_write_json_bytes_with_backup_validator<F>(
    path: &Path,
    bytes: &[u8],
    limit: u64,
    valid_existing: F,
) -> Result<(), PersistenceError>
where
    F: Fn(&str) -> bool,
{
    if bytes.len() as u64 > limit {
        return Err(PersistenceError {
            action: PersistenceAction::WriteTemporary,
            path: path.to_path_buf(),
            kind: PersistenceErrorKind::TooLarge {
                limit,
                actual: bytes.len() as u64,
            },
        });
    }
    serde_json::from_slice::<serde_json::Value>(bytes)
        .map_err(|error| PersistenceError::parse(path, error))?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        PersistenceError::io(PersistenceAction::CreateParentDirectory, path, error)
    })?;

    let (temp_path, mut temp_file) = create_unique_temp(path, "tmp")?;
    let write_result = (|| {
        temp_file.write_all(bytes).map_err(|error| {
            PersistenceError::io(PersistenceAction::WriteTemporary, path, error)
        })?;
        temp_file.flush().map_err(|error| {
            PersistenceError::io(PersistenceAction::FlushTemporary, path, error)
        })?;
        temp_file.sync_all().map_err(|error| {
            PersistenceError::io(PersistenceAction::SyncTemporary, path, error)
        })?;
        Ok(())
    })();
    drop(temp_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    let valid_primary = read_text_capped(path, limit)
        .ok()
        .filter(|text| valid_existing(text));
    let publish_result = publish_document(path, &temp_path, valid_primary.as_deref());
    if publish_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    publish_result
}

fn create_unique_temp(
    destination: &Path,
    role: &str,
) -> Result<(PathBuf, File), PersistenceError> {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    for _ in 0..64 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = destination.with_file_name(format!(
            ".{name}.{role}-{}-{id}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(PersistenceError::io(
                    PersistenceAction::CreateTemporary,
                    destination,
                    error,
                ));
            }
        }
    }
    Err(PersistenceError::io(
        PersistenceAction::CreateTemporary,
        destination,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary file name after 64 attempts",
        ),
    ))
}

#[cfg(not(windows))]
fn publish_document(
    destination: &Path,
    temp_path: &Path,
    valid_primary: Option<&str>,
) -> Result<(), PersistenceError> {
    if let Some(primary) = valid_primary {
        publish_backup_bytes(destination, primary.as_bytes())?;
    }
    fs::rename(temp_path, destination).map_err(|error| {
        PersistenceError::io(PersistenceAction::PublishDocument, destination, error)
    })?;
    sync_parent_directory(destination);
    Ok(())
}

#[cfg(not(windows))]
fn publish_backup_bytes(destination: &Path, bytes: &[u8]) -> Result<(), PersistenceError> {
    let backup = backup_path(destination);
    let (temp_backup, mut file) = create_unique_temp(&backup, "backup")?;
    let result = (|| {
        file.write_all(bytes).map_err(|error| {
            PersistenceError::io(PersistenceAction::WriteTemporary, &backup, error)
        })?;
        file.flush().map_err(|error| {
            PersistenceError::io(PersistenceAction::FlushTemporary, &backup, error)
        })?;
        file.sync_all().map_err(|error| {
            PersistenceError::io(PersistenceAction::SyncTemporary, &backup, error)
        })?;
        drop(file);
        fs::rename(&temp_backup, &backup).map_err(|error| {
            PersistenceError::io(PersistenceAction::PublishBackup, &backup, error)
        })?;
        sync_parent_directory(&backup);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp_backup);
    }
    result
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

#[cfg(windows)]
fn publish_document(
    destination: &Path,
    temp_path: &Path,
    valid_primary: Option<&str>,
) -> Result<(), PersistenceError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, ReplaceFileW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let destination_wide = wide(destination);
    let temp_wide = wide(temp_path);
    if destination.exists() {
        let backup_temp = valid_primary
            .map(|_| create_unique_temp(&backup_path(destination), "replace-backup"))
            .transpose()?;
        let backup_temp_path = backup_temp.as_ref().map(|(path, _)| path.clone());
        // ReplaceFileW needs a path that does not already contain an open file.
        if let Some((path, file)) = backup_temp {
            drop(file);
            fs::remove_file(&path).map_err(|error| {
                PersistenceError::io(PersistenceAction::PublishBackup, destination, error)
            })?;
        }
        let backup_wide = backup_temp_path.as_deref().map(wide);
        let backup_ptr = backup_wide
            .as_ref()
            .map(|path| path.as_ptr())
            .unwrap_or(std::ptr::null());
        // SAFETY: all paths are NUL-terminated and live for the duration of
        // this call; the reserved parameters are required to be null.
        let replaced = unsafe {
            ReplaceFileW(
                destination_wide.as_ptr(),
                temp_wide.as_ptr(),
                backup_ptr,
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced == 0 {
            if let Some(path) = backup_temp_path {
                let _ = fs::remove_file(path);
            }
            return Err(PersistenceError::io(
                PersistenceAction::PublishDocument,
                destination,
                io::Error::last_os_error(),
            ));
        }
        if let Some(backup_temp_path) = backup_temp_path {
            let backup = backup_path(destination);
            let backup_temp_wide = wide(&backup_temp_path);
            let backup_wide = wide(&backup);
            // SAFETY: the NUL-terminated path buffers live through the call.
            let moved = unsafe {
                MoveFileExW(
                    backup_temp_wide.as_ptr(),
                    backup_wide.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if moved == 0 {
                let _ = fs::remove_file(&backup_temp_path);
                return Err(PersistenceError::io(
                    PersistenceAction::PublishBackup,
                    &backup,
                    io::Error::last_os_error(),
                ));
            }
        }
        return Ok(());
    }

    // SAFETY: the NUL-terminated path buffers live through the call.
    let moved = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(PersistenceError::io(
            PersistenceAction::PublishDocument,
            destination,
            io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("bowecho-settings-persistence-{}-{id}", std::process::id()))
            .join(format!("{tag}.json"))
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                let _ = fs::remove_dir_all(parent);
            } else {
                let _ = fs::remove_file(parent);
            }
        }
    }

    #[test]
    fn atomic_write_success_keeps_previous_valid_document_as_backup() {
        let path = temp_path("success");
        atomic_write_json_bytes(&path, br#"{"version":1}"#, 1024).unwrap();
        atomic_write_json_bytes(&path, br#"{"version":2}"#, 1024).unwrap();

        assert_eq!(read_text_capped(&path, 1024).unwrap(), r#"{"version":2}"#);
        assert_eq!(
            read_text_capped(&backup_path(&path), 1024).unwrap(),
            r#"{"version":1}"#
        );
        cleanup(&path);
    }

    #[test]
    fn semantic_validator_never_replaces_good_backup_with_wrong_typed_json() {
        #[derive(serde::Deserialize, serde::Serialize)]
        struct TestDocument {
            version: u64,
        }
        fn valid_test_document(text: &str) -> bool {
            serde_json::from_str::<TestDocument>(text).is_ok()
        }

        let path = temp_path("semantic-backup");
        atomic_write_json_with_backup_validator(
            &path,
            &TestDocument { version: 1 },
            1024,
            valid_test_document,
        )
        .unwrap();
        atomic_write_json_with_backup_validator(
            &path,
            &TestDocument { version: 2 },
            1024,
            valid_test_document,
        )
        .unwrap();
        fs::write(&path, br#"{"version":"wrong type"}"#).unwrap();

        atomic_write_json_with_backup_validator(
            &path,
            &TestDocument { version: 3 },
            1024,
            valid_test_document,
        )
        .unwrap();
        let backup: TestDocument =
            serde_json::from_str(&read_text_capped(&backup_path(&path), 1024).unwrap()).unwrap();
        assert_eq!(backup.version, 1);
        cleanup(&path);
    }

    #[test]
    fn failed_temporary_write_reports_the_destination_and_preserves_it() {
        let path = temp_path("blocked-parent");
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent.parent().unwrap()).unwrap();
        fs::write(parent, b"not a directory").unwrap();

        let error = atomic_write_json_bytes(&path, br#"{"ok":true}"#, 1024).unwrap_err();
        assert_eq!(error.path, path);
        assert!(matches!(
            error.action,
            PersistenceAction::CreateParentDirectory | PersistenceAction::CreateTemporary
        ));
        cleanup(&path);
    }

    #[test]
    fn failed_replace_does_not_remove_the_existing_destination() {
        let path = temp_path("replace-directory");
        fs::create_dir_all(&path).unwrap();

        let error = atomic_write_json_bytes(&path, br#"{"ok":true}"#, 1024).unwrap_err();
        assert_eq!(error.action, PersistenceAction::PublishDocument);
        assert!(path.is_dir());
        cleanup(&path);
    }

    #[test]
    fn capped_read_rejects_oversized_documents() {
        let path = temp_path("oversized");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"12345").unwrap();
        let error = read_text_capped(&path, 4).unwrap_err();
        assert!(matches!(
            error.kind,
            PersistenceErrorKind::TooLarge {
                limit: 4,
                actual: 5
            }
        ));
        cleanup(&path);
    }
}
