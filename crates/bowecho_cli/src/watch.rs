use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::CliError;
use crate::artifact::{
    ARTIFACT_SCHEMA_VERSION, ArtifactManifest, ArtifactStatus, SourceReceipt, valid_sha256,
};
use crate::fs::{resolve_receipt_path, sha256_file, sha256_reader, write_json_atomic};
use crate::run_manifest::{RUN_SCHEMA_VERSION, RunManifest};

pub const WATCH_JOURNAL_SCHEMA_VERSION: &str = "bowecho.wrf.watch.v1";
pub const WATCH_PROCESSING_IDENTITY_SCHEMA_VERSION: &str =
    "bowecho.wrf.watch.processing-identity.v1";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Output-affecting identity for a persistent WRF watcher.
///
/// This deliberately excludes worker count: concurrency may change execution
/// order, but it must not change the deterministic artifact contract. Variable
/// order and duplicates are normalized because they describe the same request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchProcessingIdentity {
    pub schema_version: String,
    pub run_manifest_sha256: String,
    pub bowecho_version: String,
    pub bowecho_commit: String,
    /// Exact identity produced by the WRF renderer host and also embedded in
    /// artifact provenance. This is authoritative; the descriptive fields
    /// below make journal migrations and diagnostics auditable.
    pub renderer_processing_identity_sha256: String,
    pub artifact_schema_version: String,
    pub run_schema_version: String,
    pub preset: String,
    pub variables: Vec<String>,
}

impl WatchProcessingIdentity {
    pub fn new(
        run_manifest_sha256: impl Into<String>,
        bowecho_version: impl Into<String>,
        bowecho_commit: impl Into<String>,
        renderer_processing_identity_sha256: impl Into<String>,
        preset: impl Into<String>,
        variables: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut variables = variables.into_iter().collect::<Vec<_>>();
        variables.sort();
        variables.dedup();
        Self {
            schema_version: WATCH_PROCESSING_IDENTITY_SCHEMA_VERSION.into(),
            run_manifest_sha256: run_manifest_sha256.into(),
            bowecho_version: bowecho_version.into(),
            bowecho_commit: bowecho_commit.into(),
            renderer_processing_identity_sha256: renderer_processing_identity_sha256.into(),
            artifact_schema_version: ARTIFACT_SCHEMA_VERSION.into(),
            run_schema_version: RUN_SCHEMA_VERSION.into(),
            preset: preset.into(),
            variables,
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema_version != WATCH_PROCESSING_IDENTITY_SCHEMA_VERSION {
            failures.push(format!(
                "unsupported processing identity schema '{}'; expected '{WATCH_PROCESSING_IDENTITY_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        if !valid_sha256(&self.run_manifest_sha256) {
            failures.push("processing identity run-manifest SHA-256 is invalid".into());
        }
        if !valid_sha256(&self.renderer_processing_identity_sha256) {
            failures.push("renderer processing identity SHA-256 is invalid".into());
        }
        if self.artifact_schema_version != ARTIFACT_SCHEMA_VERSION {
            failures.push(format!(
                "processing identity artifact schema '{}' does not match '{ARTIFACT_SCHEMA_VERSION}'",
                self.artifact_schema_version
            ));
        }
        if self.run_schema_version != RUN_SCHEMA_VERSION {
            failures.push(format!(
                "processing identity run schema '{}' does not match '{RUN_SCHEMA_VERSION}'",
                self.run_schema_version
            ));
        }
        for (name, value) in [
            ("bowecho version", self.bowecho_version.as_str()),
            ("bowecho commit", self.bowecho_commit.as_str()),
            ("preset", self.preset.as_str()),
        ] {
            if value.trim().is_empty() {
                failures.push(format!("processing identity {name} is empty"));
            }
        }
        if self.variables.windows(2).any(|pair| pair[0] >= pair[1]) {
            failures.push(
                "processing identity variables must be strictly sorted and deduplicated".into(),
            );
        }
        failures
    }

    /// Exact digest shared with the artifact manifest's provenance.
    pub fn sha256(&self) -> &str {
        &self.renderer_processing_identity_sha256
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingIdentityResetReason {
    MissingFromLegacyJournal,
    Changed,
}

impl ProcessingIdentityResetReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingFromLegacyJournal => "missing_from_legacy_journal",
            Self::Changed => "changed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessingIdentityReset {
    pub reason: ProcessingIdentityResetReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_identity_sha256: Option<String>,
    pub current_identity_sha256: String,
    pub discarded_candidates: usize,
    pub discarded_processed_sources: usize,
}

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
    /// Exact size+mtime observed when the source hash was accepted. This lets
    /// a long-running watcher ignore an unchanged completed file without
    /// hashing it again on every poll. Older journals omit it and safely fall
    /// back to a new proof/hash pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<FileFingerprint>,
    pub artifact_manifest_path: PathBuf,
    pub artifact_manifest_bytes: u64,
    pub artifact_manifest_sha256: String,
    pub processed_unix_ms: u64,
}

/// Verify the immutable manifest that authorizes a stat-only processed-source
/// skip. Callers cache a successful result for the lifetime of one watcher so
/// this hashes once per receipt, never once per poll.
pub fn verify_processed_artifact_manifest(record: &ProcessedSourceRecord) -> Result<(), String> {
    let metadata = fs::symlink_metadata(&record.artifact_manifest_path).map_err(|error| {
        format!(
            "cannot inspect immutable artifact manifest {}: {error}",
            record.artifact_manifest_path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "immutable artifact manifest is not a regular non-symlink file: {}",
            record.artifact_manifest_path.display()
        ));
    }
    if metadata.len() != record.artifact_manifest_bytes {
        return Err(format!(
            "immutable artifact manifest byte count mismatch: expected {}, got {}",
            record.artifact_manifest_bytes,
            metadata.len()
        ));
    }
    let bytes = fs::read(&record.artifact_manifest_path).map_err(|error| {
        format!(
            "cannot read immutable artifact manifest {}: {error}",
            record.artifact_manifest_path.display()
        )
    })?;
    let hash = sha256_reader(&bytes[..]).map_err(|error| {
        format!(
            "cannot hash immutable artifact manifest {}: {error}",
            record.artifact_manifest_path.display()
        )
    })?;
    if hash.bytes != record.artifact_manifest_bytes {
        return Err(format!(
            "immutable artifact manifest byte count changed while hashing: expected {}, got {}",
            record.artifact_manifest_bytes, hash.bytes
        ));
    }
    if !hash
        .sha256
        .eq_ignore_ascii_case(&record.artifact_manifest_sha256)
    {
        return Err(format!(
            "immutable artifact manifest SHA-256 mismatch: expected {}, got {}",
            record.artifact_manifest_sha256, hash.sha256
        ));
    }
    let manifest: ArtifactManifest = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "invalid immutable artifact manifest {}: {error}",
            record.artifact_manifest_path.display()
        )
    })?;
    let contract_failures = manifest.validate_contract();
    if !contract_failures.is_empty() {
        return Err(format!(
            "immutable artifact manifest failed contract validation: {}",
            contract_failures.join("; ")
        ));
    }
    if manifest.status != ArtifactStatus::Complete || manifest.products.is_empty() {
        return Err(format!(
            "immutable artifact manifest is not a complete non-empty processing receipt ({:?}, {} product(s))",
            manifest.status,
            manifest.products.len()
        ));
    }
    let root = record
        .artifact_manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        format!(
            "cannot canonicalize immutable artifact root {}: {error}",
            root.display()
        )
    })?;
    for product in &manifest.products {
        let path = resolve_receipt_path(root, &product.artifact.relative_path, false)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect artifact {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "artifact is not a regular non-symlink file: {}",
                path.display()
            ));
        }
        let canonical = fs::canonicalize(&path)
            .map_err(|error| format!("cannot canonicalize artifact {}: {error}", path.display()))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(format!(
                "artifact resolves outside immutable manifest root: {}",
                canonical.display()
            ));
        }
        let artifact_hash = sha256_file(&path)
            .map_err(|error| format!("cannot hash artifact {}: {error}", path.display()))?;
        if artifact_hash.bytes != product.artifact.bytes
            || !artifact_hash
                .sha256
                .eq_ignore_ascii_case(&product.artifact.sha256)
        {
            return Err(format!(
                "artifact receipt mismatch for {}: expected {} bytes {}, got {} bytes {}",
                product.name,
                product.artifact.bytes,
                product.artifact.sha256,
                artifact_hash.bytes,
                artifact_hash.sha256
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WatchJournal {
    pub schema_version: String,
    pub case_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    /// Missing only in journals written before processing identity was added.
    /// Such a journal is migrated by discarding all candidates and receipts;
    /// its old stat-only skips are never trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing_identity: Option<WatchProcessingIdentity>,
    #[serde(default)]
    pub candidates: BTreeMap<String, CandidateRecord>,
    #[serde(default)]
    pub processed_sources: BTreeMap<String, ProcessedSourceRecord>,
}

impl WatchJournal {
    pub fn new(run: &RunManifest, processing_identity: WatchProcessingIdentity) -> Self {
        Self {
            schema_version: WATCH_JOURNAL_SCHEMA_VERSION.into(),
            case_id: run.case_id.clone(),
            run_id: run.run_id.clone(),
            member_id: run.member_id.clone(),
            processing_identity: Some(processing_identity),
            candidates: BTreeMap::new(),
            processed_sources: BTreeMap::new(),
        }
    }

    pub fn validate_for_run(&self, run: &RunManifest) -> Vec<String> {
        let mut failures = self.validate_journal_identity_for_run(run);
        if let Some(identity) = &self.processing_identity {
            failures.extend(identity.validate());
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
            if record
                .fingerprint
                .is_some_and(|fingerprint| fingerprint.bytes != record.source.bytes)
            {
                failures.push(format!(
                    "processed source {key} fingerprint byte count does not match its SHA-256 receipt"
                ));
            }
        }
        failures
    }

    fn validate_journal_identity_for_run(&self, run: &RunManifest) -> Vec<String> {
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

    pub fn is_unchanged_processed_source(&self, path: &Path, fingerprint: FileFingerprint) -> bool {
        self.unchanged_processed_source_record(path, fingerprint)
            .is_some()
    }

    pub fn unchanged_processed_source_record(
        &self,
        path: &Path,
        fingerprint: FileFingerprint,
    ) -> Option<(String, ProcessedSourceRecord)> {
        let path = path_key(path);
        self.processed_sources.iter().find_map(|(key, record)| {
            (path_key(&record.source.path) == path && record.fingerprint == Some(fingerprint))
                .then(|| (key.clone(), record.clone()))
        })
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
    pub created: bool,
    pub processing_identity_reset: Option<ProcessingIdentityReset>,
}

impl WatchJournalStore {
    pub fn open(
        path: &Path,
        run: &RunManifest,
        processing_identity: WatchProcessingIdentity,
    ) -> Result<Self, CliError> {
        let identity_failures = processing_identity.validate();
        if !identity_failures.is_empty() {
            return Err(CliError::data(format!(
                "invalid watch processing identity: {}",
                identity_failures.join("; ")
            )));
        }
        let created = !path.exists();
        let mut journal = if !created {
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
            WatchJournal::new(run, processing_identity.clone())
        };
        let run_failures = journal.validate_journal_identity_for_run(run);
        if !run_failures.is_empty() {
            return Err(CliError::data(format!(
                "watch journal {} failed validation: {}",
                path.display(),
                run_failures.join("; ")
            )));
        }
        let same_processing_identity = journal
            .processing_identity
            .as_ref()
            .is_some_and(|value| value.sha256() == processing_identity.sha256());
        let processing_identity_reset = if created || same_processing_identity {
            // Keep descriptive fields current even when the authoritative
            // renderer hash is unchanged.
            journal.processing_identity = Some(processing_identity);
            None
        } else {
            let previous = journal
                .processing_identity
                .as_ref()
                .map(|value| value.sha256().to_owned());
            let reset = ProcessingIdentityReset {
                reason: if previous.is_some() {
                    ProcessingIdentityResetReason::Changed
                } else {
                    ProcessingIdentityResetReason::MissingFromLegacyJournal
                },
                previous_identity_sha256: previous,
                current_identity_sha256: processing_identity.sha256().to_owned(),
                discarded_candidates: journal.candidates.len(),
                discarded_processed_sources: journal.processed_sources.len(),
            };
            journal.candidates.clear();
            journal.processed_sources.clear();
            journal.processing_identity = Some(processing_identity);
            Some(reset)
        };
        let failures = journal.validate_for_run(run);
        if !failures.is_empty() {
            return Err(CliError::data(format!(
                "watch journal {} failed validation: {}",
                path.display(),
                failures.join("; ")
            )));
        }
        let recovered_claims = if processing_identity_reset.is_none() {
            journal.recover_interrupted_claims()
        } else {
            0
        };
        let store = Self {
            path: path.to_path_buf(),
            journal,
            recovered_claims,
            created,
            processing_identity_reset,
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

    pub fn remove_processed_source(
        &mut self,
        processed_key: &str,
    ) -> Result<Option<ProcessedSourceRecord>, CliError> {
        let removed = self.journal.processed_sources.remove(processed_key);
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
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

    use crate::artifact::{
        ArtifactFileReceipt, ArtifactManifest, BuildIdentity, DerivationMethod, ExperimentTrack,
        FiniteStatistics, GridReceipt, ProductReceipt, ProvenanceHashes,
    };
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

    fn processing_identity(value: char) -> WatchProcessingIdentity {
        processing_identity_with(value, Vec::new())
    }

    fn processing_identity_with(
        renderer_value: char,
        variables: Vec<String>,
    ) -> WatchProcessingIdentity {
        WatchProcessingIdentity::new(
            "a".repeat(64),
            "0.34.5",
            "commit",
            renderer_value.to_string().repeat(64),
            "severe",
            variables,
        )
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
        let mut journal = WatchJournal::new(&run(), processing_identity('a'));
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
        let mut journal = WatchJournal::new(&run(), processing_identity('a'));
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
        let mut journal = WatchJournal::new(&run(), processing_identity('a'));
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
        let mut journal = WatchJournal::new(&run(), processing_identity('a'));
        let source = source(Path::new("wrfout_d01"));
        let record = ProcessedSourceRecord {
            source: source.clone(),
            fingerprint: Some(fingerprint(10, 1)),
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
        assert!(journal.is_unchanged_processed_source(Path::new("wrfout_d01"), fingerprint(10, 1)));
        assert!(
            !journal.is_unchanged_processed_source(Path::new("wrfout_d01"), fingerprint(10, 2))
        );
    }

    #[test]
    fn reopening_atomic_journal_recovers_interrupted_claim() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("watch-journal.json");
        let run = run();
        let mut store = WatchJournalStore::open(&path, &run, processing_identity('a')).unwrap();
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

        let resumed = WatchJournalStore::open(&path, &run, processing_identity('a')).unwrap();
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

    #[test]
    fn legacy_journal_without_processing_identity_discards_unsafe_state() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("watch-journal.json");
        let run = run();
        let mut journal = WatchJournal::new(&run, processing_identity('a'));
        journal.processing_identity = None;
        journal.observe_with_proof(
            Path::new("wrfout_d01"),
            fingerprint(10, 1),
            false,
            1_000,
            &WatchPolicy::default(),
            |_| Ok(()),
        );
        journal.mark_processed(ProcessedSourceRecord {
            source: source(Path::new("wrfout_d02")),
            fingerprint: Some(fingerprint(10, 1)),
            artifact_manifest_path: "old.json".into(),
            artifact_manifest_bytes: 10,
            artifact_manifest_sha256: "b".repeat(64),
            processed_unix_ms: 1,
        });
        write_json_atomic(&path, &journal).unwrap();

        let reopened = WatchJournalStore::open(&path, &run, processing_identity('a')).unwrap();
        let reset = reopened.processing_identity_reset.unwrap();
        assert_eq!(
            reset.reason,
            ProcessingIdentityResetReason::MissingFromLegacyJournal
        );
        assert_eq!(reset.discarded_candidates, 1);
        assert_eq!(reset.discarded_processed_sources, 1);
        assert!(reopened.journal.candidates.is_empty());
        assert!(reopened.journal.processed_sources.is_empty());
        assert_eq!(
            reopened.journal.processing_identity,
            Some(processing_identity('a'))
        );
    }

    #[test]
    fn changed_processing_config_discards_receipts_but_variable_order_does_not() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("watch-journal.json");
        let run = run();
        let first = processing_identity_with('a', vec!["mucape".into(), "sbcape".into()]);
        let same =
            processing_identity_with('a', vec!["sbcape".into(), "mucape".into(), "sbcape".into()]);
        assert_eq!(first, same);
        let mut store = WatchJournalStore::open(&path, &run, first).unwrap();
        store
            .mark_processed(ProcessedSourceRecord {
                source: source(Path::new("wrfout_d01")),
                fingerprint: Some(fingerprint(10, 1)),
                artifact_manifest_path: "old.json".into(),
                artifact_manifest_bytes: 10,
                artifact_manifest_sha256: "b".repeat(64),
                processed_unix_ms: 1,
            })
            .unwrap();
        drop(store);

        let unchanged = WatchJournalStore::open(&path, &run, same).unwrap();
        assert!(unchanged.processing_identity_reset.is_none());
        assert_eq!(unchanged.journal.processed_sources.len(), 1);
        drop(unchanged);

        let changed = WatchJournalStore::open(
            &path,
            &run,
            processing_identity_with('b', vec!["mucape".into(), "sbcape".into()]),
        )
        .unwrap();
        let reset = changed.processing_identity_reset.unwrap();
        assert_eq!(reset.reason, ProcessingIdentityResetReason::Changed);
        assert_eq!(reset.discarded_processed_sources, 1);
        assert!(changed.journal.processed_sources.is_empty());
    }

    #[test]
    fn immutable_manifest_receipt_detects_same_length_corruption() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join("artifact-manifest.source-test.json");
        let artifact = root.path().join("d01/ref.png");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([4, 5, 6, 255]))
            .save(&artifact)
            .unwrap();
        let artifact_hash = sha256_file(&artifact).unwrap();
        let source = source(Path::new("wrfout_d01"));
        write_json_atomic(
            &manifest,
            &ArtifactManifest {
                schema_version: ARTIFACT_SCHEMA_VERSION.into(),
                status: ArtifactStatus::Complete,
                case_id: "case".into(),
                run_id: "run".into(),
                member_id: None,
                experiment_track: ExperimentTrack::WrfParity,
                model: "WRF".into(),
                microphysics_scheme: None,
                provenance: ProvenanceHashes::default(),
                bowecho: BuildIdentity {
                    version: "test".into(),
                    commit: "test".into(),
                },
                sources: vec![source.clone()],
                products: vec![ProductReceipt {
                    name: "reflectivity".into(),
                    domain: Some("d01".into()),
                    initialization_time: Some("2026-07-22T00:00:00Z".into()),
                    valid_time: Some("2026-07-22T00:00:00Z".into()),
                    storage_slot: Some(0),
                    lead_seconds: Some(0),
                    units: "dBZ".into(),
                    source_variables: vec!["REFL_10CM".into()],
                    derivation: DerivationMethod::Direct,
                    artifact: ArtifactFileReceipt {
                        relative_path: PathBuf::from("d01/ref.png"),
                        mime_type: "image/png".into(),
                        width: Some(2),
                        height: Some(2),
                        bytes: artifact_hash.bytes,
                        sha256: artifact_hash.sha256,
                    },
                    statistics: FiniteStatistics::default(),
                }],
                warnings: vec![],
                unavailable_products: vec![],
                failures: vec![],
            },
        )
        .unwrap();
        let hash = sha256_file(&manifest).unwrap();
        let record = ProcessedSourceRecord {
            source,
            fingerprint: Some(fingerprint(10, 1)),
            artifact_manifest_path: manifest.clone(),
            artifact_manifest_bytes: hash.bytes,
            artifact_manifest_sha256: hash.sha256,
            processed_unix_ms: 1,
        };
        verify_processed_artifact_manifest(&record).unwrap();

        let mut artifact_bytes = fs::read(&artifact).unwrap();
        let last = artifact_bytes.len() - 1;
        artifact_bytes[last] ^= 1;
        fs::write(&artifact, &artifact_bytes).unwrap();
        let error = verify_processed_artifact_manifest(&record).unwrap_err();
        assert!(error.contains("artifact receipt mismatch"));

        image::RgbaImage::from_pixel(2, 2, image::Rgba([4, 5, 6, 255]))
            .save(&artifact)
            .unwrap();
        let mut manifest_bytes = fs::read(&manifest).unwrap();
        let last = manifest_bytes.len() - 1;
        manifest_bytes[last] = b' ';
        fs::write(&manifest, manifest_bytes).unwrap();
        let error = verify_processed_artifact_manifest(&record).unwrap_err();
        assert!(error.contains("SHA-256 mismatch"));
        fs::remove_file(&manifest).unwrap();
        let error = verify_processed_artifact_manifest(&record).unwrap_err();
        assert!(error.contains("cannot inspect immutable artifact manifest"));
    }
}
