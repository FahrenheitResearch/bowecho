// SPDX-License-Identifier: Apache-2.0

//! The plan-hash-keyed run registry: Studio's durable memory of every
//! launch. One directory per submitted plan under the registry root:
//!
//! ```text
//! <root>/<plan_sha256[..16]>/
//!     plan.json      exact bytes submitted to gpuwm (the hash preimage)
//!     launch.json    arwen-studio.launch.v1 record (below)
//!     events.jsonl   the child's stdout (the ONE event source)
//!     gpuwm-run/     fixture mode only: simulated gpuwm run directory
//! ```
//!
//! The real gpuwm run directory (heartbeat, wrfout files) lives wherever
//! the plan's `output_root` says; `launch.json.run_dir` records it once
//! `plan_accepted` announces it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const LAUNCH_SCHEMA: &str = "arwen-studio.launch.v1";

#[derive(Clone, Debug)]
pub struct RunRegistry {
    root: PathBuf,
}

/// The durable per-launch record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub schema: String,
    pub plan_sha256: String,
    /// Display name copied from the plan at launch.
    pub name: String,
    pub launched_at_utc: String,
    /// `"survive_studio"` or `"die_with_studio"`.
    pub ownership: String,
    pub child_pid: Option<u32>,
    /// True when this launch replayed a fixture instead of running gpuwm.
    pub fixture: bool,
    /// gpuwm's run directory, once `plan_accepted` announced it.
    #[serde(default)]
    pub run_dir: Option<String>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One registry entry, as listed.
#[derive(Clone, Debug)]
pub struct RunEntry {
    pub dir: PathBuf,
    pub record: LaunchRecord,
}

impl RunRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Platform default: `%LOCALAPPDATA%/ArWenStudio/runs` (or the temp dir
    /// as a last resort — never a panic at startup).
    pub fn default_root() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("ArWenStudio").join("runs")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Allocate the run directory for a plan about to be launched and
    /// persist the exact plan bytes. Returns (dir, full plan hash).
    /// Relaunching the same plan reuses the same directory — the registry
    /// is keyed on WHAT was asked, and a new launch overwrites the record
    /// of the old one (the old events file is rotated aside).
    pub fn allocate(&self, plan_bytes: &[u8]) -> Result<(PathBuf, String), String> {
        let hash = arwen_plan::plan::plan_sha256(plan_bytes);
        let dir = self.root.join(&hash[..16]);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("create run registry dir {}: {error}", dir.display()))?;
        std::fs::write(dir.join("plan.json"), plan_bytes)
            .map_err(|error| format!("persist plan.json: {error}"))?;
        let events = dir.join("events.jsonl");
        if events.exists() {
            let rotated = dir.join(format!(
                "events-{}.jsonl",
                chrono::Utc::now().format("%Y%m%dT%H%M%S")
            ));
            std::fs::rename(&events, &rotated)
                .map_err(|error| format!("rotate previous events.jsonl: {error}"))?;
        }
        Ok((dir, hash))
    }

    pub fn save_record(dir: &Path, record: &LaunchRecord) -> Result<(), String> {
        let text = serde_json::to_string_pretty(record)
            .map_err(|error| format!("serialize launch.json: {error}"))?;
        std::fs::write(dir.join("launch.json"), text)
            .map_err(|error| format!("write launch.json: {error}"))
    }

    pub fn load_record(dir: &Path) -> Result<LaunchRecord, String> {
        let text = std::fs::read_to_string(dir.join("launch.json"))
            .map_err(|error| format!("read launch.json: {error}"))?;
        serde_json::from_str(&text).map_err(|error| format!("parse launch.json: {error}"))
    }

    /// Every registered run, newest first.
    pub fn list(&self) -> Vec<RunEntry> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut runs: Vec<RunEntry> = entries
            .flatten()
            .filter_map(|entry| {
                let dir = entry.path();
                Self::load_record(&dir)
                    .ok()
                    .map(|record| RunEntry { dir, record })
            })
            .collect();
        runs.sort_by(|a, b| b.record.launched_at_utc.cmp(&a.record.launched_at_utc));
        runs
    }

    pub fn events_path(dir: &Path) -> PathBuf {
        dir.join("events.jsonl")
    }

    pub fn plan_path(dir: &Path) -> PathBuf {
        dir.join("plan.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry(label: &str) -> RunRegistry {
        RunRegistry::new(std::env::temp_dir().join(format!(
            "arwen-proc-registry-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )))
    }

    fn record(hash: &str, launched: &str) -> LaunchRecord {
        LaunchRecord {
            schema: LAUNCH_SCHEMA.into(),
            plan_sha256: hash.into(),
            name: "test-run".into(),
            launched_at_utc: launched.into(),
            ownership: "survive_studio".into(),
            child_pid: Some(1234),
            fixture: true,
            run_dir: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn allocate_is_keyed_on_plan_bytes_and_rotates_old_events() {
        let registry = temp_registry("alloc");
        let (dir_a, hash_a) = registry.allocate(b"{\"plan\":1}").unwrap();
        let (dir_b, hash_b) = registry.allocate(b"{\"plan\":2}").unwrap();
        assert_ne!(hash_a, hash_b);
        assert_ne!(dir_a, dir_b);
        assert_eq!(
            std::fs::read(RunRegistry::plan_path(&dir_a)).unwrap(),
            b"{\"plan\":1}"
        );

        // Same plan → same dir; existing events rotate aside.
        std::fs::write(RunRegistry::events_path(&dir_a), "old\n").unwrap();
        let (dir_again, hash_again) = registry.allocate(b"{\"plan\":1}").unwrap();
        assert_eq!(dir_again, dir_a);
        assert_eq!(hash_again, hash_a);
        assert!(!RunRegistry::events_path(&dir_a).exists());
        let rotated = std::fs::read_dir(&dir_a)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("events-"))
            .count();
        assert_eq!(rotated, 1);

        std::fs::remove_dir_all(registry.root()).unwrap();
    }

    #[test]
    fn records_round_trip_and_list_is_newest_first() {
        let registry = temp_registry("list");
        let (dir_a, hash_a) = registry.allocate(b"a").unwrap();
        let (dir_b, hash_b) = registry.allocate(b"b").unwrap();
        RunRegistry::save_record(&dir_a, &record(&hash_a, "2026-08-07T12:00:00Z")).unwrap();
        RunRegistry::save_record(&dir_b, &record(&hash_b, "2026-08-07T13:00:00Z")).unwrap();

        let listed = registry.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].record.plan_sha256, hash_b, "newest first");
        assert_eq!(
            RunRegistry::load_record(&dir_a).unwrap().plan_sha256,
            hash_a
        );

        std::fs::remove_dir_all(registry.root()).unwrap();
    }
}
