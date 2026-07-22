use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CliError;
use crate::artifact::{SourceReceipt, valid_sha256};
use crate::fs::write_json_atomic;
use crate::run_manifest::RunManifest;

pub const WATCH_JOURNAL_SCHEMA_VERSION: &str = "bowecho.wrf.watch.v1";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchPolicy {
    pub stable_window_ms: u64,
    pub completion_marker_suffix: String,
}

impl Default for WatchPolicy {
    fn default() -> Self {
        Self {
            stable_window_ms: 30_000,
            completion_marker_suffix: ".complete".into(),
        }
    }
}

impl WatchPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if !(1_000..=3_600_000).contains(&self.stable_window_ms) {
            return Err("stable window must be between 1 and 3600 seconds".into());
        }
        if self.completion_marker_suffix.is_empty()
            || self.completion_marker_suffix.len() > 64
            || self
                .completion_marker_suffix
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return Err(
                "completion marker suffix must be 1-64 characters with no separators/control characters"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub bytes: u64,
    pub modified_unix_seconds: u64,
    pub modified_subsec_nanos: u32,
}

impl FileFingerprint {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("cannot stat candidate {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "candidate is not a regular file: {}",
                path.display()
            ));
        }
        let modified = metadata
            .modified()
            .map_err(|error| format!("cannot read mtime for {}: {error}", path.display()))?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| format!("candidate mtime predates Unix epoch: {}", path.display()))?;
        Ok(Self {
            bytes: metadata.len(),
            modified_unix_seconds: modified.as_secs(),
            modified_subsec_nanos: modified.subsec_nanos(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyReason {
    CompletionMarker,
    StableAndReadable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    NewlyObserved,
    Changed,
    StabilityWindow {
        stable_for_ms: u64,
        required_ms: u64,
    },
    NotReadable {
        error: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadinessDecision {
    Waiting(WaitingReason),
    Ready(ReadyReason),
    InFlight,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub path: PathBuf,
    pub fingerprint: FileFingerprint,
    pub stable_since_unix_ms: u64,
    pub last_seen_unix_ms: u64,
    pub in_flight: bool,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_readability_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessedSourceRecord {
    pub source: SourceReceipt,
    pub artifact_manifest_path: PathBuf,
    pub artifact_manifest_bytes: u64,
    pub artifact_manifest_sha256: String,
    pub processed_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchJournal {
    pub schema_version: String,
    pub case_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    #[serde(default)]
    pub candidates: BTreeMap<String, CandidateRecord>,
    #[serde(default)]
    pub processed_sources: BTreeMap<String, ProcessedSourceRecord>,
}

impl WatchJournal {
    pub fn new(run: &RunManifest) -> Self {
        Self {
            schema_version: WATCH_JOURNAL_SCHEMA_VERSION.into(),
            case_id: run.case_id.clone(),
            run_id: run.run_id.clone(),
            member_id: run.member_id.clone(),
            candidates: BTreeMap::new(),
            processed_sources: BTreeMap::new(),
        }
    }

    pub fn validate_for_run(&self, run: &RunManifest) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != WATCH_JOURNAL_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported watch journal schema '{}'; expected '{WATCH_JOURNAL_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        if self.case_id != run.case_id
            || self.run_id != run.run_id
            || self.member_id != run.member_id
        {
            failures.push(format!(
                "watch journal identity {}/{}/{:?} does not match run {}/{}/{:?}",
                self.case_id, self.run_id, self.member_id, run.case_id, run.run_id, run.member_id
            ));
        }
        for (key, record) in &self.processed_sources {
            if !valid_sha256(&record.source.sha256) {
                failures.push(format!("processed source {key} has an invalid SHA-256"));
            }
            if !valid_sha256(&record.artifact_manifest_sha256) {
                failures.push(format!(
                    "processed source {key} has an invalid artifact-manifest SHA-256"
                ));
            }
        }
        failures
    }

    /// Pure transition used by both filesystem polling and deterministic
    /// tests. A Ready result is also an atomic-work claim once the journal is
    /// persisted by [`WatchJournalStore`].
    pub fn observe_with_proof(
        &mut self,
        path: &Path,
        fingerprint: FileFingerprint,
        completion_marker_present: bool,
        now_unix_ms: u64,
        policy: &WatchPolicy,
        prove_readable: impl FnOnce(&Path) -> Result<(), String>,
    ) -> ReadinessDecision {
        let key = path_key(path);
        let mut newly_observed = false;
        let mut changed = false;
        let record = self.candidates.entry(key).or_insert_with(|| {
            newly_observed = true;
            CandidateRecord {
                path: path.to_path_buf(),
                fingerprint,
                stable_since_unix_ms: now_unix_ms,
                last_seen_unix_ms: now_unix_ms,
                in_flight: false,
                attempts: 0,
                last_readability_error: None,
            }
        });
        if record.fingerprint != fingerprint {
            changed = true;
            record.fingerprint = fingerprint;
            record.stable_since_unix_ms = now_unix_ms;
            record.in_flight = false;
            record.last_readability_error = None;
        }
        record.path = path.to_path_buf();
        record.last_seen_unix_ms = now_unix_ms;

        if record.in_flight {
            return ReadinessDecision::InFlight;
        }
        if newly_observed && !completion_marker_present {
            return ReadinessDecision::Waiting(WaitingReason::NewlyObserved);
        }
        if changed && !completion_marker_present {
            return ReadinessDecision::Waiting(WaitingReason::Changed);
        }

        let stable_for_ms = now_unix_ms.saturating_sub(record.stable_since_unix_ms);
        let reason = if completion_marker_present {
            ReadyReason::CompletionMarker
        } else if stable_for_ms >= policy.stable_window_ms {
            ReadyReason::StableAndReadable
        } else {
            return ReadinessDecision::Waiting(WaitingReason::StabilityWindow {
                stable_for_ms,
                required_ms: policy.stable_window_ms,
            });
        };

        record.attempts = record.attempts.saturating_add(1);
        match prove_readable(path) {
            Ok(()) => {
                record.in_flight = true;
                record.last_readability_error = None;
                ReadinessDecision::Ready(reason)
            }
            Err(error) => {
                record.last_readability_error = Some(error.clone());
                ReadinessDecision::Waiting(WaitingReason::NotReadable { error })
            }
        }
    }

    pub fn is_processed(&self, source: &SourceReceipt) -> bool {
        self.processed_sources.contains_key(&processed_key(source))
    }

    pub fn mark_processed(&mut self, record: ProcessedSourceRecord) -> MarkProcessedResult {
        let key = processed_key(&record.source);
        self.candidates.remove(&path_key(&record.source.path));
        match self.processed_sources.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(record);
                MarkProcessedResult::Inserted
            }
            std::collections::btree_map::Entry::Occupied(_) => MarkProcessedResult::AlreadyPresent,
        }
    }

    pub fn release_claim(&mut self, path: &Path, error: impl Into<String>) -> bool {
        let Some(candidate) = self.candidates.get_mut(&path_key(path)) else {
            return false;
        };
        candidate.in_flight = false;
        candidate.last_readability_error = Some(error.into());
        true
    }

    fn recover_interrupted_claims(&mut self) -> usize {
        let mut recovered = 0;
        for candidate in self.candidates.values_mut() {
            if candidate.in_flight {
                candidate.in_flight = false;
                candidate.last_readability_error = Some(
                    "previous process stopped before writing a processed receipt; retrying".into(),
                );
                recovered += 1;
            }
        }
        recovered
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkProcessedResult {
    Inserted,
    AlreadyPresent,
}

pub struct WatchJournalStore {
    pub path: PathBuf,
    pub journal: WatchJournal,
    pub recovered_claims: usize,
}

impl WatchJournalStore {
    pub fn open(path: &Path, run: &RunManifest) -> Result<Self, CliError> {
        let mut journal = if path.exists() {
            let metadata = fs::metadata(path).map_err(|error| {
                CliError::input(format!(
                    "cannot stat watch journal {}: {error}",
                    path.display()
                ))
            })?;
            if metadata.len() > MAX_JOURNAL_BYTES {
                return Err(CliError::data(format!(
                    "watch journal is {} bytes; maximum is {MAX_JOURNAL_BYTES}",
                    metadata.len()
                )));
            }
            let bytes = fs::read(path).map_err(|error| {
                CliError::input(format!(
                    "cannot read watch journal {}: {error}",
                    path.display()
                ))
            })?;
            serde_json::from_slice::<WatchJournal>(&bytes).map_err(|error| {
                CliError::data(format!("invalid watch journal {}: {error}", path.display()))
            })?
        } else {
            WatchJournal::new(run)
        };
        let failures = journal.validate_for_run(run);
        if !failures.is_empty() {
            return Err(CliError::data(format!(
                "watch journal {} failed validation: {}",
                path.display(),
                failures.join("; ")
            )));
        }
        let recovered_claims = journal.recover_interrupted_claims();
        let store = Self {
            path: path.to_path_buf(),
            journal,
            recovered_claims,
        };
        store.save()?;
        Ok(store)
    }

    pub fn poll_candidate(
        &mut self,
        source: &Path,
        now_unix_ms: u64,
        policy: &WatchPolicy,
        prove_readable: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<ReadinessDecision, CliError> {
        policy.validate().map_err(CliError::data)?;
        let fingerprint = FileFingerprint::from_path(source).map_err(CliError::input)?;
        let marker = completion_marker_path(source, &policy.completion_marker_suffix).is_file();
        let decision = self.journal.observe_with_proof(
            source,
            fingerprint,
            marker,
            now_unix_ms,
            policy,
            prove_readable,
        );
        self.save()?;
        Ok(decision)
    }

    pub fn mark_processed(
        &mut self,
        record: ProcessedSourceRecord,
    ) -> Result<MarkProcessedResult, CliError> {
        if !valid_sha256(&record.source.sha256) || !valid_sha256(&record.artifact_manifest_sha256) {
            return Err(CliError::data(
                "processed receipt contains an invalid SHA-256",
            ));
        }
        let result = self.journal.mark_processed(record);
        self.save()?;
        Ok(result)
    }

    pub fn release_claim(
        &mut self,
        path: &Path,
        error: impl Into<String>,
    ) -> Result<bool, CliError> {
        let released = self.journal.release_claim(path, error);
        self.save()?;
        Ok(released)
    }

    pub fn save(&self) -> Result<(), CliError> {
        write_json_atomic(&self.path, &self.journal).map_err(|error| {
            CliError::input(format!(
                "cannot atomically write watch journal {}: {error}",
                self.path.display()
            ))
        })
    }
}

pub fn completion_marker_path(source: &Path, suffix: &str) -> PathBuf {
    let mut name = source
        .file_name()
        .map_or_else(OsString::new, OsString::from);
    name.push(suffix);
    source.with_file_name(name)
}

/// Production proof hook. The readiness timer/marker only decides when to
/// try; this function proves the candidate is a structurally readable WRF
/// NetCDF file with at least one WRF time before it can be claimed.
pub fn prove_wrf_netcdf_readable(path: &Path) -> Result<(), String> {
    let nc = netcrust::open(path).map_err(|error| format!("netcrust open: {error}"))?;
    let dimensions = nc
        .dimensions()
        .map_err(|error| format!("enumerate NetCDF dimensions: {error}"))?;
    let variables = nc
        .variables()
        .map_err(|error| format!("enumerate NetCDF variables: {error}"))?;
    if dimensions.is_empty() || variables.is_empty() {
        return Err("NetCDF metadata contains no dimensions or variables".into());
    }
    let wrf = wrf_core::WrfFile::open(path)
        .map_err(|error| format!("wrf-core structural read: {error}"))?;
    let times = wrf
        .times()
        .map_err(|error| format!("wrf-core Times read: {error}"))?;
    if times.is_empty() {
        return Err("WRF Times contains no records".into());
    }
    Ok(())
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn processed_key(source: &SourceReceipt) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path_key(&source.path).as_bytes());
    hasher.update([0]);
    hasher.update(source.bytes.to_le_bytes());
    hasher.update([0]);
    hasher.update(source.sha256.to_ascii_lowercase().as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crate::artifact::GridReceipt;
    use crate::run_manifest::RUN_SCHEMA_VERSION;

    use super::*;

    fn run() -> RunManifest {
        serde_json::from_value(serde_json::json!({
            "schema_version": RUN_SCHEMA_VERSION,
            "case_id": "case",
            "run_id": "run",
            "experiment_track": "wrf-parity",
            "model": "WRF"
        }))
        .unwrap()
    }

    fn fingerprint(bytes: u64, modified: u64) -> FileFingerprint {
        FileFingerprint {
            bytes,
            modified_unix_seconds: modified,
            modified_subsec_nanos: 0,
        }
    }

    fn source(path: &Path) -> SourceReceipt {
        SourceReceipt {
            path: path.to_path_buf(),
            bytes: 10,
            sha256: "a".repeat(64),
            domain: Some("d01".into()),
            initialization_time: None,
            valid_times: vec![],
            grid: GridReceipt::default(),
        }
    }

    #[test]
    fn stable_size_and_mtime_gate_readability_proof() {
        let mut journal = WatchJournal::new(&run());
        let policy = WatchPolicy {
            stable_window_ms: 1_000,
            ..WatchPolicy::default()
        };
        let path = Path::new("wrfout_d01");
        let calls = Cell::new(0);
        let first =
            journal.observe_with_proof(path, fingerprint(10, 1), false, 1_000, &policy, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            });
        assert_eq!(
            first,
            ReadinessDecision::Waiting(WaitingReason::NewlyObserved)
        );
        assert_eq!(calls.get(), 0);

        let waiting =
            journal.observe_with_proof(path, fingerprint(10, 1), false, 1_500, &policy, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            });
        assert!(matches!(
            waiting,
            ReadinessDecision::Waiting(WaitingReason::StabilityWindow { .. })
        ));
        assert_eq!(calls.get(), 0);

        let ready =
            journal.observe_with_proof(path, fingerprint(10, 1), false, 2_000, &policy, |_| {
                calls.set(calls.get() + 1);
                Ok(())
            });
        assert_eq!(
            ready,
            ReadinessDecision::Ready(ReadyReason::StableAndReadable)
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(
            journal.observe_with_proof(path, fingerprint(10, 1), false, 2_100, &policy, |_| Ok(())),
            ReadinessDecision::InFlight
        );
    }

    #[test]
    fn changed_fingerprint_restarts_stability_window() {
        let mut journal = WatchJournal::new(&run());
        let policy = WatchPolicy {
            stable_window_ms: 1_000,
            ..WatchPolicy::default()
        };
        let path = Path::new("wrfout_d01");
        journal.observe_with_proof(path, fingerprint(10, 1), false, 1_000, &policy, |_| Ok(()));
        let changed =
            journal.observe_with_proof(path, fingerprint(11, 2), false, 2_500, &policy, |_| {
                panic!("changed file must not be proven yet")
            });
        assert_eq!(changed, ReadinessDecision::Waiting(WaitingReason::Changed));
    }

    #[test]
    fn completion_marker_can_trigger_immediate_but_still_requires_readability() {
        let mut journal = WatchJournal::new(&run());
        let policy = WatchPolicy::default();
        let path = Path::new("wrfout_d01");
        let decision =
            journal.observe_with_proof(path, fingerprint(10, 1), true, 1_000, &policy, |_| Ok(()));
        assert_eq!(
            decision,
            ReadinessDecision::Ready(ReadyReason::CompletionMarker)
        );
        assert_eq!(
            completion_marker_path(path, ".complete"),
            PathBuf::from("wrfout_d01.complete")
        );

        let other = Path::new("wrfout_d02");
        let decision =
            journal.observe_with_proof(other, fingerprint(10, 1), true, 1_000, &policy, |_| {
                Err("truncated".into())
            });
        assert!(matches!(
            decision,
            ReadinessDecision::Waiting(WaitingReason::NotReadable { .. })
        ));
    }

    #[test]
    fn processed_receipts_are_idempotent_by_source_path_size_and_hash() {
        let mut journal = WatchJournal::new(&run());
        let source = source(Path::new("wrfout_d01"));
        let record = ProcessedSourceRecord {
            source: source.clone(),
            artifact_manifest_path: "artifact-manifest.json".into(),
            artifact_manifest_bytes: 100,
            artifact_manifest_sha256: "b".repeat(64),
            processed_unix_ms: 5,
        };
        assert_eq!(
            journal.mark_processed(record.clone()),
            MarkProcessedResult::Inserted
        );
        assert_eq!(
            journal.mark_processed(record),
            MarkProcessedResult::AlreadyPresent
        );
        assert_eq!(journal.processed_sources.len(), 1);
        assert!(journal.is_processed(&source));
    }

    #[test]
    fn reopening_atomic_journal_recovers_interrupted_claim() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("watch-journal.json");
        let run = run();
        let mut store = WatchJournalStore::open(&path, &run).unwrap();
        let candidate = root.path().join("wrfout_d01");
        fs::write(&candidate, b"placeholder").unwrap();
        let fingerprint = FileFingerprint::from_path(&candidate).unwrap();
        let decision = store.journal.observe_with_proof(
            &candidate,
            fingerprint,
            true,
            1_000,
            &WatchPolicy::default(),
            |_| Ok(()),
        );
        assert!(matches!(decision, ReadinessDecision::Ready(_)));
        store.save().unwrap();
        drop(store);

        let resumed = WatchJournalStore::open(&path, &run).unwrap();
        assert_eq!(resumed.recovered_claims, 1);
        assert!(
            !resumed
                .journal
                .candidates
                .values()
                .next()
                .unwrap()
                .in_flight
        );
        let reparsed: WatchJournal = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!reparsed.candidates.values().next().unwrap().in_flight);
    }
}
