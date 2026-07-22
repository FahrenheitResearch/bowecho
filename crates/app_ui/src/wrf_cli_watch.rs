//! Long-running WRF directory watcher for the headless BowEcho CLI.
//!
//! Readiness, claims, and resume state live in `bowecho_cli`; this module is
//! only the application-host bridge to BowEcho's production WRF renderer.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bowecho_cli::artifact::{ArtifactStatus, SourceReceipt};
use bowecho_cli::input::InputSet;
use bowecho_cli::watch::{
    FileFingerprint, MarkProcessedResult, ProcessedSourceRecord, ReadinessDecision, ReadyReason,
    WaitingReason, WatchJournalStore, WatchProcessingIdentity, prove_wrf_netcdf_readable,
};
use bowecho_cli::{CliError, ExitCode, RenderOptions, RuntimeContext, WatchOptions};
use serde::Serialize;

const WATCH_EVENT_SCHEMA_VERSION: &str = "bowecho.wrf.watch.event.v1";

#[derive(Debug, Serialize)]
struct WatchEventEnvelope {
    schema_version: &'static str,
    sequence: u64,
    emitted_unix_ms: u64,
    #[serde(flatten)]
    event: WatchEvent,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WatchEvent {
    Started {
        directory: PathBuf,
        journal: PathBuf,
        processing_identity_sha256: String,
        poll_interval_ms: u64,
        stable_window_ms: u64,
        completion_marker_suffix: String,
    },
    InterruptedClaimsRecovered {
        count: usize,
    },
    ProcessingIdentityReset {
        reset: bowecho_cli::watch::ProcessingIdentityReset,
    },
    Limitation {
        code: &'static str,
        message: &'static str,
    },
    ProcessedReceiptInvalidated {
        path: PathBuf,
        artifact_manifest_path: PathBuf,
        reason: String,
    },
    Waiting {
        path: PathBuf,
        reason: WaitingReason,
    },
    Ready {
        path: PathBuf,
        reason: ReadyReason,
    },
    Processed {
        path: PathBuf,
        source_sha256: String,
        artifact_manifest_path: PathBuf,
        artifact_manifest_sha256: String,
        receipt: &'static str,
    },
    CandidateFailed {
        path: PathBuf,
        stage: &'static str,
        error: String,
        retryable: bool,
    },
}

struct EventWriter<'a> {
    enabled: bool,
    sequence: u64,
    stdout: &'a mut dyn Write,
}

#[derive(Clone, Copy, Debug)]
struct RetryState {
    fingerprint: FileFingerprint,
    not_before_unix_ms: u64,
}

impl EventWriter<'_> {
    fn emit(&mut self, event: WatchEvent) -> Result<(), CliError> {
        if !self.enabled {
            return Ok(());
        }
        let envelope = WatchEventEnvelope {
            schema_version: WATCH_EVENT_SCHEMA_VERSION,
            sequence: self.sequence,
            emitted_unix_ms: now_unix_ms()?,
            event,
        };
        self.sequence = self.sequence.saturating_add(1);
        serde_json::to_writer(&mut *self.stdout, &envelope)
            .map_err(|error| CliError::internal(format!("serialize watch JSONL event: {error}")))?;
        self.stdout
            .write_all(b"\n")
            .and_then(|()| self.stdout.flush())
            .map_err(|error| CliError::internal(format!("write watch JSONL event: {error}")))
    }
}

/// Run `bowecho wrf watch` without initializing eframe or a GPU surface.
///
/// This normally returns only for a fatal directory/journal/output error;
/// per-file readability and render failures release their persisted claim and
/// remain retryable on later polls.
pub(crate) fn execute_watch(
    options: &WatchOptions,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    options.readiness.validate().map_err(CliError::data)?;
    let loaded_run = bowecho_cli::run_manifest::load_run_manifest(&options.run_manifest)?;
    // Discovery validates the watched path, but an empty first snapshot is a
    // normal live-run state rather than an input failure.
    let _ = bowecho_cli::fs::discover_input_files(&options.directory)?;
    let journal_path = options.journal.clone().unwrap_or_else(|| {
        bowecho_cli::paths::watch_journal_path(&options.output_directory, &loaded_run.manifest)
    });
    let identity_options = RenderOptions {
        inputs: InputSet::parse([OsString::from(options.directory.as_os_str())])?,
        preset: options.preset,
        output_directory: options.output_directory.clone(),
        run_manifest: options.run_manifest.clone(),
        workers: options.workers,
        variables: options.variables.clone(),
        json: false,
    };
    let renderer_processing_identity =
        crate::wrf_cli_host::processing_identity(&identity_options, &loaded_run.sha256, context);
    let processing_identity = WatchProcessingIdentity::new(
        loaded_run.sha256.clone(),
        context.bowecho_version.clone(),
        context.bowecho_commit.clone(),
        renderer_processing_identity,
        options.preset.as_str(),
        options.variables.clone(),
    );
    let processing_identity_sha256 = processing_identity.sha256().to_owned();
    let mut journal =
        WatchJournalStore::open(&journal_path, &loaded_run.manifest, processing_identity)?;
    let recovered_claims = journal.recovered_claims;
    let mut events = EventWriter {
        enabled: options.jsonl,
        sequence: 0,
        stdout,
    };
    events.emit(WatchEvent::Started {
        directory: options.directory.clone(),
        journal: journal_path.clone(),
        processing_identity_sha256,
        poll_interval_ms: options.poll_interval_ms,
        stable_window_ms: options.readiness.stable_window_ms,
        completion_marker_suffix: options.readiness.completion_marker_suffix.clone(),
    })?;
    if recovered_claims > 0 {
        events.emit(WatchEvent::InterruptedClaimsRecovered {
            count: recovered_claims,
        })?;
    }
    if let Some(reset) = journal.processing_identity_reset.clone() {
        events.emit(WatchEvent::ProcessingIdentityReset {
            reset: reset.clone(),
        })?;
        writeln!(
            stderr,
            "WRF watch processing identity {}: discarded {} candidate(s) and {} processed receipt(s)",
            reset.reason.as_str(),
            reset.discarded_candidates,
            reset.discarded_processed_sources
        )
        .map_err(|error| CliError::internal(format!("write watch progress: {error}")))?;
    }
    const INTERVAL_CONTEXT_NOTE: &str = "Each ready file is rendered independently. Interval precipitation requiring a previous compatible forecast time may be unavailable; use `wrf render` with an ordered multi-file input set for that product.";
    events.emit(WatchEvent::Limitation {
        code: "single_file_interval_context",
        message: INTERVAL_CONTEXT_NOTE,
    })?;
    writeln!(stderr, "WRF watch note: {INTERVAL_CONTEXT_NOTE}")
        .map_err(|error| CliError::internal(format!("write watch progress: {error}")))?;
    writeln!(
        stderr,
        "BowEcho WRF watch: {} (journal {}, {} recovered claim(s))",
        options.directory.display(),
        journal_path.display(),
        recovered_claims
    )
    .map_err(|error| CliError::internal(format!("write watch progress: {error}")))?;

    // Waiting transitions can persist for many polls. Emit JSONL only when a
    // candidate's reason changes so a quiet directory cannot grow an
    // unbounded duplicate log.
    let mut last_waiting = BTreeMap::<String, String>::new();
    // A completion marker bypasses the stability timer by design. Back off
    // unchanged deterministic failures so a multi-minute WRF job cannot be
    // relaunched every poll; any fingerprint change retries immediately.
    let mut retry_state = BTreeMap::<String, RetryState>::new();
    // A receipt manifest is hashed once in this process before its source is
    // eligible for the cheap size+mtime skip. The token includes every
    // integrity-relevant field, so replacing a journal record forces a fresh
    // verification without hashing on every poll.
    let mut verified_receipts = BTreeSet::<String>::new();
    loop {
        let candidates = bowecho_cli::fs::discover_input_files(&options.directory)?;
        for source_path in candidates {
            let fingerprint = match FileFingerprint::from_path(&source_path) {
                Ok(value) => value,
                Err(error) => {
                    emit_candidate_failure(
                        &mut events,
                        stderr,
                        &source_path,
                        "fingerprint",
                        error,
                    )?;
                    continue;
                }
            };
            let source_key = path_key(&source_path);
            let now = now_unix_ms()?;
            if let Some(retry) = retry_state.get(&source_key).copied() {
                if retry.fingerprint != fingerprint {
                    retry_state.remove(&source_key);
                } else if now < retry.not_before_unix_ms {
                    continue;
                }
            }
            // This is intentionally before readiness and its NetCDF proof:
            // an unchanged completed source costs one stat, not a fresh open
            // and hash on every poll.
            let mut unchanged_receipt_verified = false;
            if let Some((processed_key, record)) = journal
                .journal
                .unchanged_processed_source_record(&source_path, fingerprint)
            {
                let token = receipt_verification_token(&processed_key, &record);
                if verified_receipts.contains(&token) {
                    unchanged_receipt_verified = true;
                } else {
                    match bowecho_cli::watch::verify_processed_artifact_manifest(&record) {
                        Ok(()) => {
                            verified_receipts.insert(token);
                            unchanged_receipt_verified = true;
                        }
                        Err(reason) => {
                            journal.remove_processed_source(&processed_key)?;
                            events.emit(WatchEvent::ProcessedReceiptInvalidated {
                                path: source_path.clone(),
                                artifact_manifest_path: record.artifact_manifest_path.clone(),
                                reason: reason.clone(),
                            })?;
                            writeln!(
                                stderr,
                                "WRF watch invalidated processed receipt for {}: {reason}",
                                source_path.display()
                            )
                            .map_err(|error| {
                                CliError::internal(format!("write watch progress: {error}"))
                            })?;
                        }
                    }
                }
            }
            if unchanged_receipt_verified {
                last_waiting.remove(&source_key);
                retry_state.remove(&source_key);
                continue;
            }

            let decision = match journal.poll_candidate(
                &source_path,
                now,
                &options.readiness,
                prove_wrf_netcdf_readable,
            ) {
                Ok(value) => value,
                Err(error) => {
                    // `poll_candidate` persists every candidate transition.
                    // An input/internal failure here can be the journal write;
                    // continuing would run without durable claim state.
                    if matches!(error.code, ExitCode::Input | ExitCode::Internal) {
                        return Err(error);
                    }
                    emit_candidate_failure(
                        &mut events,
                        stderr,
                        &source_path,
                        "readiness",
                        error.message,
                    )?;
                    continue;
                }
            };
            match decision {
                ReadinessDecision::Waiting(reason) => {
                    if matches!(reason, WaitingReason::NotReadable { .. }) {
                        let attempts = candidate_attempts(&journal, &source_path).unwrap_or(1);
                        retry_state.insert(
                            source_key.clone(),
                            RetryState {
                                fingerprint,
                                not_before_unix_ms: now.saturating_add(retry_delay_ms(
                                    attempts,
                                    options.poll_interval_ms,
                                )),
                            },
                        );
                    }
                    emit_waiting_transition(&mut events, &mut last_waiting, source_path, reason)?;
                }
                ReadinessDecision::InFlight => {}
                ReadinessDecision::Ready(reason) => {
                    last_waiting.remove(&source_key);
                    events.emit(WatchEvent::Ready {
                        path: source_path.clone(),
                        reason,
                    })?;
                    writeln!(stderr, "WRF ready: {} ({reason:?})", source_path.display()).map_err(
                        |error| CliError::internal(format!("write watch progress: {error}")),
                    )?;
                    let claimed_fingerprint =
                        claimed_fingerprint(&journal, &source_path).unwrap_or(fingerprint);
                    let attempts = candidate_attempts(&journal, &source_path).unwrap_or(1);
                    if let Err(error) = process_ready_source(
                        options,
                        context,
                        &source_path,
                        claimed_fingerprint,
                        &mut journal,
                        &mut events,
                        stderr,
                    ) {
                        // Rendering/data errors are per-source and retryable;
                        // journal/stdout failures are safety-critical and must
                        // stop rather than run without durable receipts.
                        if matches!(error.code, ExitCode::Internal | ExitCode::Input) {
                            return Err(error);
                        }
                        retry_state.insert(
                            source_key,
                            RetryState {
                                fingerprint: claimed_fingerprint,
                                not_before_unix_ms: now_unix_ms()?.saturating_add(retry_delay_ms(
                                    attempts,
                                    options.poll_interval_ms,
                                )),
                            },
                        );
                    } else {
                        retry_state.remove(&source_key);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(options.poll_interval_ms));
    }
}

#[allow(clippy::too_many_arguments)]
fn process_ready_source(
    options: &WatchOptions,
    context: &RuntimeContext,
    source_path: &Path,
    claimed_fingerprint: FileFingerprint,
    journal: &mut WatchJournalStore,
    events: &mut EventWriter<'_>,
    stderr: &mut dyn Write,
) -> Result<(), CliError> {
    let render_options = RenderOptions {
        inputs: InputSet::parse([OsString::from(source_path.as_os_str())])?,
        preset: options.preset,
        output_directory: options.output_directory.clone(),
        run_manifest: options.run_manifest.clone(),
        workers: options.workers,
        variables: options.variables.clone(),
        json: false,
    };
    let output = match crate::wrf_cli_host::render_inputs_with_mode(
        &render_options,
        context,
        vec![source_path.to_path_buf()],
        true,
        stderr,
    ) {
        Ok(output) => output,
        Err(error) => {
            release_after_failure(
                journal,
                events,
                stderr,
                source_path,
                "render",
                error.message.clone(),
            )?;
            return Err(error);
        }
    };
    if output.manifest.status != ArtifactStatus::Complete {
        let error = format!(
            "renderer produced {:?} artifact status ({} failure record(s))",
            output.manifest.status,
            output.manifest.failures.len()
        );
        release_after_failure(
            journal,
            events,
            stderr,
            source_path,
            "manifest",
            error.clone(),
        )?;
        return Err(CliError::data(error));
    }

    // If a nominally-stable producer resumed writing during rendering, do
    // not issue a processed receipt for the earlier claim. The next poll will
    // restart the stability window and replace the same deterministic plots.
    let after = match FileFingerprint::from_path(source_path) {
        Ok(value) => value,
        Err(error) => {
            release_after_failure(
                journal,
                events,
                stderr,
                source_path,
                "post_render_fingerprint",
                error.clone(),
            )?;
            return Err(CliError::input(error));
        }
    };
    if after != claimed_fingerprint {
        let error = "source size or modification time changed during rendering".to_string();
        release_after_failure(
            journal,
            events,
            stderr,
            source_path,
            "post_render_fingerprint",
            error.clone(),
        )?;
        return Err(CliError::data(error));
    }
    let source = source_receipt_for_path(
        &output.input_sources,
        source_path,
        claimed_fingerprint.bytes,
    );
    let Some(source) = source else {
        let error = format!(
            "artifact manifest has no source receipt for {} with {} bytes",
            source_path.display(),
            claimed_fingerprint.bytes
        );
        release_after_failure(
            journal,
            events,
            stderr,
            source_path,
            "source_receipt",
            error.clone(),
        )?;
        return Err(CliError::data(error));
    };

    // The cumulative run manifest changes as later files arrive. Preserve a
    // source-scoped, hash-addressed snapshot so resume validation hashes only
    // this source's artifacts instead of every earlier forecast time.
    let receipt_manifest_path = match receipt_manifest_path(&output.manifest_path, &source.sha256) {
        Ok(path) => path,
        Err(error) => {
            release_after_failure(
                journal,
                events,
                stderr,
                source_path,
                "receipt_path",
                error.message.clone(),
            )?;
            return Err(error);
        }
    };
    let receipt_manifest = match source_scoped_receipt_manifest(&output.manifest, &source) {
        Ok(manifest) => manifest,
        Err(error) => {
            release_after_failure(
                journal,
                events,
                stderr,
                source_path,
                "receipt_scope",
                error.message.clone(),
            )?;
            return Err(error);
        }
    };
    if let Err(error) =
        bowecho_cli::fs::write_json_atomic(&receipt_manifest_path, &receipt_manifest)
    {
        let message = format!(
            "cannot publish immutable watch receipt {}: {error}",
            receipt_manifest_path.display()
        );
        release_after_failure(
            journal,
            events,
            stderr,
            source_path,
            "receipt_publish",
            message.clone(),
        )?;
        return Err(CliError::input(message));
    }
    let manifest_hash = match bowecho_cli::fs::sha256_file(&receipt_manifest_path) {
        Ok(value) => value,
        Err(error) => {
            let message = format!(
                "cannot hash artifact manifest {}: {error}",
                receipt_manifest_path.display()
            );
            release_after_failure(
                journal,
                events,
                stderr,
                source_path,
                "receipt_hash",
                message.clone(),
            )?;
            return Err(CliError::input(message));
        }
    };
    let processed = ProcessedSourceRecord {
        source: source.clone(),
        fingerprint: Some(claimed_fingerprint),
        artifact_manifest_path: receipt_manifest_path.clone(),
        artifact_manifest_bytes: manifest_hash.bytes,
        artifact_manifest_sha256: manifest_hash.sha256.clone(),
        processed_unix_ms: now_unix_ms()?,
    };
    let marked = match journal.mark_processed(processed.clone()) {
        Ok(value) => value,
        Err(error) => {
            journal.release_claim(source_path, error.message.clone())?;
            return Err(error);
        }
    };
    if marked == MarkProcessedResult::AlreadyPresent {
        // A producer may touch a complete file without changing its bytes.
        // Refresh that content-addressed receipt's fingerprint so the cheap
        // pre-proof skip does not rerender it forever on every later poll.
        if let Some(existing) = journal
            .journal
            .processed_sources
            .values_mut()
            .find(|record| {
                path_key(&record.source.path) == path_key(source_path)
                    && record.source.bytes == source.bytes
                    && record.source.sha256.eq_ignore_ascii_case(&source.sha256)
            })
        {
            *existing = processed;
            journal.save()?;
        }
    }
    let receipt = match marked {
        MarkProcessedResult::Inserted => "inserted",
        MarkProcessedResult::AlreadyPresent => "already_present",
    };
    events.emit(WatchEvent::Processed {
        path: source_path.to_path_buf(),
        source_sha256: source.sha256,
        artifact_manifest_path: receipt_manifest_path,
        artifact_manifest_sha256: manifest_hash.sha256,
        receipt,
    })?;
    writeln!(
        stderr,
        "WRF processed: {} ({receipt})",
        source_path.display()
    )
    .map_err(|error| CliError::internal(format!("write watch progress: {error}")))?;
    Ok(())
}

fn source_scoped_receipt_manifest(
    cumulative: &bowecho_cli::artifact::ArtifactManifest,
    source: &SourceReceipt,
) -> Result<bowecho_cli::artifact::ArtifactManifest, CliError> {
    let mut receipt = cumulative.clone();
    let valid_times = source.valid_times.iter().collect::<BTreeSet<_>>();
    receipt.sources = vec![source.clone()];
    receipt.products.retain(|product| {
        product.domain == source.domain
            && product
                .valid_time
                .as_ref()
                .is_some_and(|valid| valid_times.contains(valid))
    });
    receipt.unavailable_products.retain(|product| {
        product.domain == source.domain
            && product
                .valid_time
                .as_ref()
                .is_some_and(|valid| valid_times.contains(valid))
    });
    receipt.failures.clear();
    receipt.status = if receipt.products.is_empty() {
        ArtifactStatus::Failed
    } else {
        ArtifactStatus::Complete
    };
    let failures = receipt.validate_contract();
    if receipt.status != ArtifactStatus::Complete || !failures.is_empty() {
        return Err(CliError::data(format!(
            "source-scoped artifact receipt is not complete: {} product(s); {}",
            receipt.products.len(),
            if failures.is_empty() {
                "no product artifacts matched this source".to_owned()
            } else {
                failures.join("; ")
            }
        )));
    }
    Ok(receipt)
}

fn emit_waiting_transition(
    events: &mut EventWriter<'_>,
    last_waiting: &mut BTreeMap<String, String>,
    path: PathBuf,
    reason: WaitingReason,
) -> Result<(), CliError> {
    let key = path_key(&path);
    let identity = serde_json::to_string(&reason)
        .map_err(|error| CliError::internal(format!("serialize readiness reason: {error}")))?;
    if last_waiting.get(&key) == Some(&identity) {
        return Ok(());
    }
    last_waiting.insert(key, identity);
    events.emit(WatchEvent::Waiting { path, reason })
}

fn release_after_failure(
    journal: &mut WatchJournalStore,
    events: &mut EventWriter<'_>,
    stderr: &mut dyn Write,
    path: &Path,
    stage: &'static str,
    error: String,
) -> Result<(), CliError> {
    journal.release_claim(path, error.clone())?;
    emit_candidate_failure(events, stderr, path, stage, error)
}

fn emit_candidate_failure(
    events: &mut EventWriter<'_>,
    stderr: &mut dyn Write,
    path: &Path,
    stage: &'static str,
    error: String,
) -> Result<(), CliError> {
    events.emit(WatchEvent::CandidateFailed {
        path: path.to_path_buf(),
        stage,
        error: error.clone(),
        retryable: true,
    })?;
    writeln!(
        stderr,
        "WRF watch {stage} failed for {}: {error}",
        path.display()
    )
    .map_err(|write_error| {
        CliError::internal(format!("write watch failure progress: {write_error}"))
    })
}

fn source_receipt_for_path(
    receipts: &[SourceReceipt],
    path: &Path,
    bytes: u64,
) -> Option<SourceReceipt> {
    let key = path_key(path);
    receipts
        .iter()
        .rev()
        .find(|receipt| path_key(&receipt.path) == key && receipt.bytes == bytes)
        .cloned()
}

fn receipt_verification_token(processed_key: &str, record: &ProcessedSourceRecord) -> String {
    let fingerprint = record.fingerprint.map_or_else(
        || "legacy".to_owned(),
        |value| {
            format!(
                "{}:{}:{}",
                value.bytes, value.modified_unix_seconds, value.modified_subsec_nanos
            )
        },
    );
    format!(
        "{processed_key}\0{}\0{}\0{}\0{fingerprint}",
        path_key(&record.artifact_manifest_path),
        record.artifact_manifest_bytes,
        record.artifact_manifest_sha256.to_ascii_lowercase()
    )
}

fn claimed_fingerprint(journal: &WatchJournalStore, path: &Path) -> Option<FileFingerprint> {
    let key = path_key(path);
    journal
        .journal
        .candidates
        .values()
        .find(|candidate| path_key(&candidate.path) == key && candidate.in_flight)
        .map(|candidate| candidate.fingerprint)
}

fn candidate_attempts(journal: &WatchJournalStore, path: &Path) -> Option<u32> {
    let key = path_key(path);
    journal
        .journal
        .candidates
        .values()
        .find(|candidate| path_key(&candidate.path) == key)
        .map(|candidate| candidate.attempts)
}

fn retry_delay_ms(attempts: u32, poll_interval_ms: u64) -> u64 {
    const BASE_MS: u64 = 5_000;
    const CEILING_MS: u64 = 5 * 60_000;
    let exponent = attempts.saturating_sub(1).min(6);
    BASE_MS
        .saturating_mul(1_u64 << exponent)
        .max(poll_interval_ms)
        .min(CEILING_MS.max(poll_interval_ms))
}

fn receipt_manifest_path(main: &Path, source_sha256: &str) -> Result<PathBuf, CliError> {
    if source_sha256.len() < 16 || !source_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CliError::data(
            "source receipt has an invalid SHA-256 for watch manifest naming",
        ));
    }
    let parent = main.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(format!(
        "artifact-manifest.source-{}.json",
        &source_sha256[..16].to_ascii_lowercase()
    )))
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn now_unix_ms() -> Result<u64, CliError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            CliError::internal(format!("system clock predates Unix epoch: {error}"))
        })?;
    u64::try_from(elapsed.as_millis())
        .map_err(|_| CliError::internal("current Unix millisecond time does not fit u64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowecho_cli::artifact::{
        ArtifactFileReceipt, ArtifactManifest, BuildIdentity, DerivationMethod, ExperimentTrack,
        FiniteStatistics, GridReceipt, ProductReceipt, ProvenanceHashes,
    };

    #[test]
    fn jsonl_event_is_versioned_tagged_and_single_line() {
        let mut bytes = Vec::new();
        let mut writer = EventWriter {
            enabled: true,
            sequence: 7,
            stdout: &mut bytes,
        };
        writer
            .emit(WatchEvent::Waiting {
                path: PathBuf::from("wrfout_d02"),
                reason: WaitingReason::StabilityWindow {
                    stable_for_ms: 500,
                    required_ms: 1_000,
                },
            })
            .unwrap();
        assert_eq!(bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["schema_version"], WATCH_EVENT_SCHEMA_VERSION);
        assert_eq!(value["sequence"], 7);
        assert_eq!(value["event"], "waiting");
        assert_eq!(value["path"], "wrfout_d02");
        assert_eq!(value["reason"]["stability_window"]["required_ms"], 1_000);
    }

    #[test]
    fn receipt_manifest_stays_beside_main_manifest() {
        let main = Path::new("out/case/run/artifact-manifest.json");
        let receipt = receipt_manifest_path(main, &"a".repeat(64)).unwrap();
        assert_eq!(
            receipt,
            PathBuf::from("out/case/run/artifact-manifest.source-aaaaaaaaaaaaaaaa.json")
        );
        assert!(receipt_manifest_path(main, "bad").is_err());
    }

    #[test]
    fn source_receipt_match_is_path_and_fingerprint_bounded() {
        let receipt = SourceReceipt {
            path: PathBuf::from("run/wrfout_d01"),
            bytes: 42,
            sha256: "b".repeat(64),
            domain: Some("d01".into()),
            initialization_time: None,
            valid_times: Vec::new(),
            grid: Default::default(),
        };
        assert!(
            source_receipt_for_path(std::slice::from_ref(&receipt), &receipt.path, 42).is_some()
        );
        assert!(
            source_receipt_for_path(std::slice::from_ref(&receipt), &receipt.path, 41).is_none()
        );
        assert!(source_receipt_for_path(&[receipt], Path::new("other"), 42).is_none());
    }

    #[test]
    fn immutable_receipt_is_scoped_to_one_sources_domain_and_times() {
        let source = SourceReceipt {
            path: PathBuf::from("wrfout_d01_t1"),
            bytes: 42,
            sha256: "a".repeat(64),
            domain: Some("d01".into()),
            initialization_time: Some("2026-07-22T00:00:00Z".into()),
            valid_times: vec!["2026-07-22T00:15:00Z".into()],
            grid: GridReceipt::default(),
        };
        let product = |domain: &str, valid: &str, name: &str| ProductReceipt {
            name: name.into(),
            domain: Some(domain.into()),
            initialization_time: Some("2026-07-22T00:00:00Z".into()),
            valid_time: Some(valid.into()),
            storage_slot: Some(0),
            lead_seconds: Some(900),
            units: "K".into(),
            source_variables: vec!["T2".into()],
            derivation: DerivationMethod::Direct,
            artifact: ArtifactFileReceipt {
                relative_path: PathBuf::from(format!("{domain}/{valid}/{name}.png")),
                mime_type: "image/png".into(),
                width: Some(1200),
                height: Some(900),
                bytes: 1,
                sha256: "b".repeat(64),
            },
            statistics: FiniteStatistics::default(),
        };
        let cumulative = ArtifactManifest {
            schema_version: bowecho_cli::artifact::ARTIFACT_SCHEMA_VERSION.into(),
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
            products: vec![
                product("d01", "2026-07-22T00:15:00Z", "t2"),
                product("d01", "2026-07-22T00:30:00Z", "t2"),
                product("d02", "2026-07-22T00:15:00Z", "t2"),
            ],
            warnings: vec![],
            unavailable_products: vec![],
            failures: vec![],
        };
        let scoped = source_scoped_receipt_manifest(&cumulative, &source).unwrap();
        assert_eq!(scoped.sources, vec![source]);
        assert_eq!(scoped.products.len(), 1);
        assert_eq!(scoped.products[0].domain.as_deref(), Some("d01"));
        assert_eq!(
            scoped.products[0].valid_time.as_deref(),
            Some("2026-07-22T00:15:00Z")
        );
    }

    #[test]
    fn retry_backoff_is_bounded_but_never_shorter_than_polling() {
        assert_eq!(retry_delay_ms(1, 1_000), 5_000);
        assert_eq!(retry_delay_ms(4, 1_000), 40_000);
        assert_eq!(retry_delay_ms(100, 1_000), 300_000);
        assert_eq!(retry_delay_ms(1, 600_000), 600_000);
    }
}
