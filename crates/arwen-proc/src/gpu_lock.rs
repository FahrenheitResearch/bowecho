// SPDX-License-Identifier: Apache-2.0
//
// ADOPTED VERBATIM from rusty-weather rw-sim/src/gpu_lock.rs (Apache-2.0,
// branch codex/rwwps-simulation-studio @ 31b681e3) per the 2026-08-07
// ArWen Studio recon. The schema string and lock-file layout are a
// CROSS-PROCESS PROTOCOL shared with gpuwm's own byte-0 lock — never
// change them here. Studio only PROBES this lock (acquire + immediate
// release); holding it would starve the gpuwm child of its own lock.

//! One nonblocking, OS-released lock protocol for a physical GPU UUID.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLockEvidence {
    pub schema: String,
    pub gpu_uuid: String,
    pub path: PathBuf,
    pub root_resolution: String,
    pub key_digest: String,
    pub mechanism: String,
    pub locked_range: String,
    pub owner_pid: u32,
    pub run_id: String,
    pub acquired_utc: String,
    pub released_utc: Option<String>,
    pub persistent_file: bool,
    pub os_auto_release: bool,
}

#[derive(Debug)]
pub struct GpuUuidLock {
    file: fs::File,
    evidence: GpuLockEvidence,
    locked: bool,
}

impl GpuUuidLock {
    pub fn acquire(gpu_uuid: &str, run_id: &str) -> Result<Self, String> {
        if gpu_uuid.trim().is_empty() || gpu_uuid.contains(['\0', ',', ';']) {
            return Err("GPU lock requires one nonempty unambiguous device identifier".to_string());
        }
        #[cfg(windows)]
        let root = crate::process::trusted_program_data_directory()?;
        #[cfg(not(windows))]
        let root = std::env::temp_dir();
        Self::acquire_at_root_with_policy(
            &root,
            gpu_uuid,
            run_id,
            if cfg!(windows) {
                "Windows FOLDERID_ProgramData known-folder API"
            } else {
                "platform temporary directory"
            },
        )
    }

    /// Explicit root for deterministic tests and controlled multi-process
    /// deployments. Production GUI and CLI callers use [`Self::acquire`].
    pub fn acquire_at_root(root: &Path, gpu_uuid: &str, run_id: &str) -> Result<Self, String> {
        Self::acquire_at_root_with_policy(root, gpu_uuid, run_id, "explicit caller root")
    }

    fn acquire_at_root_with_policy(
        root: &Path,
        gpu_uuid: &str,
        run_id: &str,
        root_resolution: &str,
    ) -> Result<Self, String> {
        let key_digest = format!("{:x}", Sha256::digest(gpu_uuid.as_bytes()))[..24].to_string();
        let directory = root.join("gpuwm").join("locks");
        fs::create_dir_all(&directory).map_err(|error| {
            format!(
                "could not create GPU lock directory {}: {error}",
                directory.display()
            )
        })?;
        let path = directory.join(format!("gpu-{key_digest}.lock"));
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("could not open GPU lock {}: {error}", path.display()))?;
        if file.metadata().map_err(|error| error.to_string())?.len() == 0 {
            file.write_all(&[0])
                .map_err(|error| format!("could not initialize GPU lock: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("could not sync GPU lock: {error}"))?;
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not seek GPU lock: {error}"))?;
        match FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => {
                return Err(format!(
                    "GPU {gpu_uuid} is already locked by another GPUWM run: {}",
                    path.display()
                ));
            }
            Err(fs4::TryLockError::Error(error)) => {
                return Err(format!(
                    "could not acquire GPU lock {}: {error}",
                    path.display()
                ));
            }
        }
        let evidence = GpuLockEvidence {
            schema: "rusty-weather-gpu-uuid-lock-v1".to_string(),
            gpu_uuid: gpu_uuid.to_string(),
            path: path.clone(),
            root_resolution: root_resolution.to_string(),
            key_digest,
            mechanism: if cfg!(windows) {
                "fs4 LockFileEx range overlapping gpuwm byte-0 msvcrt lock".to_string()
            } else {
                "fs4 flock exclusive nonblocking".to_string()
            },
            locked_range: "whole file in Rust; overlaps GPUWM byte offset 0 length 1".to_string(),
            owner_pid: std::process::id(),
            run_id: run_id.to_string(),
            acquired_utc: Utc::now().to_rfc3339(),
            released_utc: None,
            persistent_file: true,
            os_auto_release: true,
        };
        let owner = serde_json::to_vec(&evidence)
            .map_err(|error| format!("could not serialize GPU lock owner: {error}"))?;
        file.seek(SeekFrom::Start(1))
            .and_then(|_| file.set_len(1))
            .and_then(|_| file.write_all(&owner))
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("could not publish GPU lock owner: {error}"))?;
        Ok(Self {
            file,
            evidence,
            locked: true,
        })
    }

    pub fn evidence(&self) -> GpuLockEvidence {
        self.evidence.clone()
    }

    pub fn release(mut self) -> Result<GpuLockEvidence, String> {
        FileExt::unlock(&self.file)
            .map_err(|error| format!("could not release GPU lock: {error}"))?;
        self.locked = false;
        self.evidence.released_utc = Some(Utc::now().to_rfc3339());
        Ok(self.evidence.clone())
    }
}

impl Drop for GpuUuidLock {
    fn drop(&mut self) {
        if self.locked {
            let _ = FileExt::unlock(&self.file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rw-sim-gpu-lock-{label}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn contention_and_drop_recovery_share_one_uuid_key() {
        let root = root("drop");
        let uuid = "GPU-00000000-0000-0000-0000-000000000001";
        let first = GpuUuidLock::acquire_at_root(&root, uuid, "first").unwrap();
        let error = GpuUuidLock::acquire_at_root(&root, uuid, "second").unwrap_err();
        assert!(error.contains("already locked"), "{error}");
        drop(first);
        drop(GpuUuidLock::acquire_at_root(&root, uuid, "retry").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_release_records_time_and_allows_reacquisition() {
        let root = root("release");
        let uuid = "GPU-00000000-0000-0000-0000-000000000002";
        let released = GpuUuidLock::acquire_at_root(&root, uuid, "first")
            .unwrap()
            .release()
            .unwrap();
        assert!(released.released_utc.is_some());
        drop(GpuUuidLock::acquire_at_root(&root, uuid, "retry").unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
