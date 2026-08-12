//! Explicit owner publication of complete processed rw-store generations.
//!
//! This path is intentionally separate from Community Cache. A publication
//! uses ordinary authenticated HTTPS to one configured trusted Rusty Weather
//! origin; it never invokes rendezvous, TURN, ICE, STUN, or another client.
//! Merely loading this module or persisted jobs performs no network work.
//!
//! Preparation is also deliberately narrower than a generic upload picker:
//! the caller supplies a store root plus validated model/run components. The
//! source is held under [`RunLock`], deep-validated, copied through an exact
//! `run.json` inventory, and frozen as bounded SHA-256 objects. Raw wrfout,
//! arbitrary paths, symlinks/reparse points, and unregistered files have no
//! representation in the resulting protocol manifest.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use eframe::egui;
use rw_community_protocol::{
    AttributionNotice, BEGIN_RUN_GENERATION_SCHEMA, BeginRunGenerationRequest,
    CancelledRunGeneration, DataOrigin, FINALIZE_RUN_GENERATION_SCHEMA,
    FinalizeRunGenerationRequest, MAX_RUN_GENERATION_MISSING_PAGE, MAX_RUN_GENERATION_OWNER_PAGE,
    PublicationGrant, PublishedRunGeneration, REVOKE_RUN_GENERATION_SCHEMA,
    RUN_GENERATION_CAPABILITIES_PATH, RUN_GENERATION_CHUNK_SCHEMA_V1, RUN_GENERATION_FILE_SCHEMA,
    RUN_GENERATION_REPLICATION_SCHEMA, RevokeRunGenerationRequest, RunGenerationFile,
    RunGenerationFileChunk, RunGenerationFileKind, RunGenerationLimits, RunGenerationMissingPage,
    RunGenerationOwnerCapabilities, RunGenerationOwnerListPage, RunGenerationOwnerRecord,
    RunGenerationOwnerRecordState, RunGenerationReplicationManifest, RunGenerationTombstone,
    RunGenerationUploadStatus, SourceProvenance, generation_content_sha256,
};
use rw_query::RunSnapshot;
use rw_store::atomic::atomic_write_bytes;
use rw_store::run::{RwsRunManifest, validate_store_component};
use rw_store::{RunLock, ValidateDepth, validate_run_dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SETTINGS_SCHEMA: &str = "bowecho.generation-publication.settings.v1";
const JOB_SCHEMA: &str = "bowecho.generation-publication.job.v1";
const VAULT_RECORD_SCHEMA: &str = "bowecho.generation-publication.credential.v1";
const VAULT_SERVICE: &str = "research.fahrenheit.bowecho";
const OBJECT_HASH_DOMAIN: &[u8] = b"bowecho-generation-origin-binding-v1\0";
const DEFAULT_ORIGIN_ID: &str = "hetzner-primary";
const STATE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const SOURCE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_JOB_STATE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ATTRIBUTIONS: usize = 64;
const MAX_MODIFICATION_NOTICES: usize = 32;
const ECMWF_MODIFICATION_NOTICE: &str = "The ECMWF source data has been subset, processed by WRF/ArWen, normalized, and re-encoded as an rw-store generation by the publishing owner.";

fn state_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

static VAULT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultRecord {
    schema: String,
    origin_binding_sha256: String,
    bearer_token: String,
}

/// Per-origin HTTPS credential. Debug output and all public errors are
/// intentionally redacted; the token is never stored in a publication job.
pub(crate) struct GenerationOriginCredentials {
    origin_binding_sha256: String,
    bearer_token: String,
}

impl fmt::Debug for GenerationOriginCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenerationOriginCredentials")
            .field("origin_binding_sha256", &self.origin_binding_sha256)
            .field("bearer_token", &"[REDACTED]")
            .finish()
    }
}

impl GenerationOriginCredentials {
    pub(crate) fn new(
        settings: &ValidatedPublicationSettings,
        bearer_token: &str,
    ) -> Result<Self, PublicationError> {
        let token = bearer_token.trim();
        if token.is_empty() || token.len() > 16 * 1024 || token.chars().any(|ch| ch.is_control()) {
            return Err(PublicationError::Credentials);
        }
        Ok(Self {
            origin_binding_sha256: settings.origin_binding_sha256.clone(),
            bearer_token: token.to_owned(),
        })
    }

    fn bearer_token(&self) -> &str {
        &self.bearer_token
    }
}

trait PublicationCredentialBackend {
    fn load(&self, account: &str) -> Result<Option<String>, ()>;
    fn save(&self, account: &str, secret: &str) -> Result<(), ()>;
    fn delete(&self, account: &str) -> Result<bool, ()>;
}

struct NativePublicationCredentialBackend;

impl PublicationCredentialBackend for NativePublicationCredentialBackend {
    fn load(&self, account: &str) -> Result<Option<String>, ()> {
        #[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
        {
            let entry = keyring::Entry::new(VAULT_SERVICE, account).map_err(|_| ())?;
            match entry.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(()),
            }
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
        {
            let _ = account;
            Err(())
        }
    }

    fn save(&self, account: &str, secret: &str) -> Result<(), ()> {
        #[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
        {
            keyring::Entry::new(VAULT_SERVICE, account)
                .map_err(|_| ())?
                .set_password(secret)
                .map_err(|_| ())
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
        {
            let _ = (account, secret);
            Err(())
        }
    }

    fn delete(&self, account: &str) -> Result<bool, ()> {
        #[cfg(any(windows, target_os = "macos", target_os = "ios", target_os = "linux"))]
        {
            let entry = keyring::Entry::new(VAULT_SERVICE, account).map_err(|_| ())?;
            match entry.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring::Error::NoEntry) => Ok(false),
                Err(_) => Err(()),
            }
        }
        #[cfg(not(any(windows, target_os = "macos", target_os = "ios", target_os = "linux")))]
        {
            let _ = account;
            Err(())
        }
    }
}

pub(crate) fn save_origin_credentials(
    settings: &GenerationPublicationSettings,
    bearer_token: &str,
) -> Result<(), PublicationError> {
    let settings = settings.validate()?;
    let credentials = GenerationOriginCredentials::new(&settings, bearer_token)?;
    let _guard = VAULT_LOCK
        .lock()
        .map_err(|_| PublicationError::Credentials)?;
    save_credentials_with(&NativePublicationCredentialBackend, &settings, &credentials)
}

pub(crate) fn delete_origin_credentials(
    settings: &GenerationPublicationSettings,
) -> Result<bool, PublicationError> {
    let settings = settings.validate()?;
    let _guard = VAULT_LOCK
        .lock()
        .map_err(|_| PublicationError::Credentials)?;
    NativePublicationCredentialBackend
        .delete(&vault_account(&settings))
        .map_err(|_| PublicationError::Credentials)
}

pub(crate) fn save_origin_credentials_from_app(
    settings: &settings::GenerationPublicationSettings,
    bearer_token: &str,
) -> Result<(), PublicationError> {
    save_origin_credentials(
        &GenerationPublicationSettings::from_app_settings(settings)?,
        bearer_token,
    )
}

pub(crate) fn delete_origin_credentials_from_app(
    settings: &settings::GenerationPublicationSettings,
) -> Result<bool, PublicationError> {
    delete_origin_credentials(&GenerationPublicationSettings::from_app_settings(settings)?)
}

/// Explicit credential/account check used by the settings button. Calling
/// this is network activity; merely constructing a panel or loading settings
/// never calls it.
pub(crate) fn fetch_owner_capabilities(
    settings: &settings::GenerationPublicationSettings,
) -> Result<RunGenerationOwnerCapabilities, PublicationError> {
    let settings = GenerationPublicationSettings::from_app_settings(settings)?;
    let validated = settings.validate()?;
    let credentials = load_origin_credentials(&validated)?;
    let transport = HttpsGenerationPublicationTransport::new(&settings)?;
    transport.capabilities(&credentials)
}

fn load_origin_credentials(
    settings: &ValidatedPublicationSettings,
) -> Result<GenerationOriginCredentials, PublicationError> {
    let _guard = VAULT_LOCK
        .lock()
        .map_err(|_| PublicationError::Credentials)?;
    load_credentials_with(&NativePublicationCredentialBackend, settings)?
        .ok_or(PublicationError::Credentials)
}

fn save_credentials_with(
    backend: &impl PublicationCredentialBackend,
    settings: &ValidatedPublicationSettings,
    credentials: &GenerationOriginCredentials,
) -> Result<(), PublicationError> {
    if credentials.origin_binding_sha256 != settings.origin_binding_sha256 {
        return Err(PublicationError::WrongOrigin);
    }
    let record = VaultRecord {
        schema: VAULT_RECORD_SCHEMA.to_owned(),
        origin_binding_sha256: credentials.origin_binding_sha256.clone(),
        bearer_token: credentials.bearer_token.clone(),
    };
    let secret = serde_json::to_string(&record).map_err(|_| PublicationError::Credentials)?;
    backend
        .save(&vault_account(settings), &secret)
        .map_err(|_| PublicationError::Credentials)
}

fn load_credentials_with(
    backend: &impl PublicationCredentialBackend,
    settings: &ValidatedPublicationSettings,
) -> Result<Option<GenerationOriginCredentials>, PublicationError> {
    let Some(secret) = backend
        .load(&vault_account(settings))
        .map_err(|_| PublicationError::Credentials)?
    else {
        return Ok(None);
    };
    let record: VaultRecord =
        serde_json::from_str(&secret).map_err(|_| PublicationError::Credentials)?;
    if record.schema != VAULT_RECORD_SCHEMA
        || record.origin_binding_sha256 != settings.origin_binding_sha256
    {
        return Err(PublicationError::Credentials);
    }
    GenerationOriginCredentials::new(settings, &record.bearer_token).map(Some)
}

fn vault_account(settings: &ValidatedPublicationSettings) -> String {
    format!(
        "generation-publication-{}-{}",
        settings.origin_id,
        &settings.origin_binding_sha256[..16]
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationPublicationSettings {
    pub schema: String,
    /// Default-off advanced owner feature. This does not follow the Community
    /// Cache opt-in and cannot be enabled by it.
    pub enabled: bool,
    /// Exactly one trusted authority is supported initially.
    pub trusted_origin_id: String,
    pub trusted_origin_url: String,
    pub policy: GenerationPublicationPolicy,
}

impl Default for GenerationPublicationSettings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA.to_owned(),
            enabled: false,
            trusted_origin_id: DEFAULT_ORIGIN_ID.to_owned(),
            trusted_origin_url: String::new(),
            policy: GenerationPublicationPolicy::default(),
        }
    }
}

impl GenerationPublicationSettings {
    pub(crate) fn from_app_settings(
        value: &settings::GenerationPublicationSettings,
    ) -> Result<Self, PublicationError> {
        const GIB: u64 = 1024 * 1024 * 1024;
        const MIB: u64 = 1024 * 1024;
        let settings = Self {
            schema: SETTINGS_SCHEMA.to_owned(),
            enabled: value.enabled,
            trusted_origin_id: value.trusted_origin_id.clone(),
            trusted_origin_url: value.trusted_origin_url.clone(),
            policy: GenerationPublicationPolicy {
                max_generation_bytes: u64::from(value.max_generation_gib)
                    .checked_mul(GIB)
                    .ok_or(PublicationError::InvalidSettings)?,
                max_spool_bytes: u64::from(value.max_spool_gib)
                    .checked_mul(GIB)
                    .ok_or(PublicationError::InvalidSettings)?,
                max_files: usize::try_from(value.max_files)
                    .map_err(|_| PublicationError::InvalidSettings)?,
                max_chunks: usize::try_from(value.max_chunks)
                    .map_err(|_| PublicationError::InvalidSettings)?,
                chunk_bytes: u64::from(value.chunk_mib)
                    .checked_mul(MIB)
                    .ok_or(PublicationError::InvalidSettings)?,
                max_manifest_bytes: usize::try_from(
                    u64::from(value.max_manifest_mib)
                        .checked_mul(MIB)
                        .ok_or(PublicationError::InvalidSettings)?,
                )
                .map_err(|_| PublicationError::InvalidSettings)?,
                max_retention_seconds: i64::from(value.max_retention_days)
                    .checked_mul(24 * 60 * 60)
                    .ok_or(PublicationError::InvalidSettings)?,
            },
        };
        settings.validate()?;
        Ok(settings)
    }

    pub(crate) fn validate(&self) -> Result<ValidatedPublicationSettings, PublicationError> {
        if self.schema != SETTINGS_SCHEMA {
            return Err(PublicationError::InvalidSettings);
        }
        if self.trusted_origin_id != DEFAULT_ORIGIN_ID {
            return Err(PublicationError::InvalidSettings);
        }
        validate_token(&self.trusted_origin_id, 96)?;
        let origin_url = normalize_https_origin(&self.trusted_origin_url)?;
        self.policy.validate()?;
        let origin_binding_sha256 = origin_binding_sha256(&self.trusted_origin_id, &origin_url);
        Ok(ValidatedPublicationSettings {
            enabled: self.enabled,
            origin_id: self.trusted_origin_id.clone(),
            origin_url,
            origin_binding_sha256,
            policy: self.policy.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationPublicationPolicy {
    pub max_generation_bytes: u64,
    /// Includes the temporary validation copy and final immutable objects.
    pub max_spool_bytes: u64,
    pub max_files: usize,
    pub max_chunks: usize,
    pub chunk_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_retention_seconds: i64,
}

impl Default for GenerationPublicationPolicy {
    fn default() -> Self {
        Self {
            max_generation_bytes: 64 * 1024 * 1024 * 1024,
            max_spool_bytes: 144 * 1024 * 1024 * 1024,
            max_files: 8_192,
            max_chunks: 1_000_000,
            chunk_bytes: 8 * 1024 * 1024,
            max_manifest_bytes: 16 * 1024 * 1024,
            max_retention_seconds: 90 * 24 * 60 * 60,
        }
    }
}

impl GenerationPublicationPolicy {
    fn validate(&self) -> Result<(), PublicationError> {
        if self.max_generation_bytes == 0
            || self.max_spool_bytes < self.max_generation_bytes.saturating_mul(2)
            || self.max_files < 3
            || self.max_chunks == 0
            || self.chunk_bytes == 0
            || self.max_manifest_bytes == 0
            || self.max_retention_seconds <= 0
        {
            return Err(PublicationError::InvalidSettings);
        }
        self.protocol_limits()
            .validate()
            .map_err(PublicationError::Protocol)
    }

    fn protocol_limits(&self) -> RunGenerationLimits {
        RunGenerationLimits {
            max_generation_bytes: self.max_generation_bytes,
            max_files: self.max_files,
            max_chunks: self.max_chunks,
            max_chunk_bytes: self.chunk_bytes,
            max_manifest_bytes: self.max_manifest_bytes,
            max_retention_seconds: self.max_retention_seconds,
            max_provenance_entries: 64,
            max_attributions: MAX_ATTRIBUTIONS,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ValidatedPublicationSettings {
    enabled: bool,
    origin_id: String,
    origin_url: String,
    origin_binding_sha256: String,
    policy: GenerationPublicationPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OwnerGenerationKind {
    PrivateWrf,
    PrivateArwen,
    UserProvided,
}

impl OwnerGenerationKind {
    fn data_origin(self) -> DataOrigin {
        match self {
            Self::PrivateWrf => DataOrigin::PrivateWrf,
            Self::PrivateArwen => DataOrigin::PrivateArwen,
            Self::UserProvided => DataOrigin::UserProvided,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PrepareGenerationRequest {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub owner_principal_sha256: String,
    pub kind: OwnerGenerationKind,
    pub retention_seconds: i64,
    pub attributions: Vec<AttributionNotice>,
    pub modification_notices: Vec<String>,
    pub now_unix: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PublicationConfirmations {
    pub owner_publication: bool,
    pub redistribution_rights: bool,
    /// The trusted HTTPS operator necessarily observes connection metadata,
    /// including the publisher's address. Other BowEcho users are uninvolved.
    pub operator_connection_metadata: bool,
}

impl PublicationConfirmations {
    fn all_confirmed(self) -> bool {
        self.owner_publication && self.redistribution_rights && self.operator_connection_metadata
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GenerationJobStatus {
    Prepared,
    Confirmed {
        confirmed_unix: i64,
    },
    /// Persisted before the first begin request is sent. A crash or lost
    /// response can therefore never make a possible origin reservation look
    /// like an offline-only Confirmed job.
    OriginBeginUncertain {
        confirmed_unix: i64,
    },
    Uploading {
        confirmed_unix: i64,
        uploaded_chunks: u32,
        total_chunks: u32,
    },
    FinalizeUncertain {
        confirmed_unix: i64,
    },
    Published {
        result: PublishedRunGeneration,
    },
    Cancelled {
        cancelled_unix: i64,
    },
    Revoked {
        tombstone: RunGenerationTombstone,
    },
    Failed {
        code: PublicationFailureCode,
        retryable: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationFailureCode {
    Authentication,
    Conflict,
    Quota,
    RemoteRejected,
    Transport,
    LocalSpoolTampered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationPublicationJob {
    pub schema: String,
    pub job_id: String,
    pub origin_id: String,
    pub origin_binding_sha256: String,
    pub model: String,
    pub run: String,
    pub generation_sha256: String,
    pub source_snapshot_id: String,
    pub grid_hash: String,
    pub owner_principal_sha256: String,
    pub kind: OwnerGenerationKind,
    pub source_provenance: Vec<SourceProvenance>,
    pub files: Vec<RunGenerationFile>,
    pub total_bytes: u64,
    pub retention_seconds: i64,
    pub attributions: Vec<AttributionNotice>,
    pub modification_notices: Vec<String>,
    pub status: GenerationJobStatus,
    pub created_unix: i64,
    pub updated_unix: i64,
}

impl GenerationPublicationJob {
    fn confirmed_unix(&self) -> Option<i64> {
        match self.status {
            GenerationJobStatus::Confirmed { confirmed_unix }
            | GenerationJobStatus::OriginBeginUncertain { confirmed_unix }
            | GenerationJobStatus::Uploading { confirmed_unix, .. }
            | GenerationJobStatus::FinalizeUncertain { confirmed_unix } => Some(confirmed_unix),
            _ => None,
        }
    }

    fn replication_manifest(
        &self,
        limits: &RunGenerationLimits,
    ) -> Result<RunGenerationReplicationManifest, PublicationError> {
        let published_unix = self
            .confirmed_unix()
            .ok_or(PublicationError::ConfirmationRequired)?;
        let retain_until_unix = published_unix
            .checked_add(self.retention_seconds)
            .ok_or(PublicationError::Retention)?;
        let manifest = RunGenerationReplicationManifest {
            schema: RUN_GENERATION_REPLICATION_SCHEMA.to_owned(),
            generation_id: self.job_id.clone(),
            model: self.model.clone(),
            run: self.run.clone(),
            source_snapshot_id: self.source_snapshot_id.clone(),
            grid_hash: self.grid_hash.clone(),
            owner_principal_sha256: self.owner_principal_sha256.clone(),
            publication: PublicationGrant {
                data_origin: self.kind.data_origin(),
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            source_provenance: self.source_provenance.clone(),
            files: self.files.clone(),
            total_bytes: self.total_bytes,
            generation_sha256: self.generation_sha256.clone(),
            published_unix,
            retain_until_unix,
            attributions: self.attributions.clone(),
            modification_notices: self.modification_notices.clone(),
        };
        manifest.validate(limits)?;
        Ok(manifest)
    }

    fn total_chunks(&self) -> u32 {
        self.files.iter().map(|file| file.chunks.len() as u32).sum()
    }

    fn spool_must_remain(&self) -> bool {
        matches!(
            self.status,
            GenerationJobStatus::Prepared
                | GenerationJobStatus::Confirmed { .. }
                | GenerationJobStatus::OriginBeginUncertain { .. }
                | GenerationJobStatus::Uploading { .. }
                | GenerationJobStatus::FinalizeUncertain { .. }
                | GenerationJobStatus::Failed {
                    retryable: true,
                    ..
                }
        )
    }

    fn reusable_for_prepare(&self) -> bool {
        !matches!(
            self.status,
            GenerationJobStatus::Cancelled { .. } | GenerationJobStatus::Revoked { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SpoolCollectionReport {
    pub protected_objects: usize,
    pub protected_bytes: u64,
    pub removed_objects: usize,
    pub removed_object_bytes: u64,
    pub removed_staging_directories: usize,
    pub removed_staging_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PublicationError {
    #[error("generation publication is disabled")]
    Disabled,
    #[error("generation publication settings are invalid")]
    InvalidSettings,
    #[error("the configured publication origin does not match this operation")]
    WrongOrigin,
    #[error("generation publication credentials are unavailable")]
    Credentials,
    #[error("the selected rw-store generation is not a closed, valid owner run")]
    InvalidGeneration,
    #[error("the selected generation exceeds the configured publication limits")]
    Limit,
    #[error("the selected generation contains a symlink or reparse point")]
    LinkedPath,
    #[error("every stored hour must carry reviewed, nonempty source provenance")]
    MissingProvenance,
    #[error("WRF/ArWen producer identity is missing, mixed, or inconsistent")]
    ProducerIdentity,
    #[error("publication attribution is required")]
    Attribution,
    #[error("the requested publication retention is invalid")]
    Retention,
    #[error("all owner, redistribution-rights, and operator-metadata confirmations are required")]
    ConfirmationRequired,
    #[error("the publication job is in the wrong state for this action")]
    InvalidState,
    #[error(
        "the origin reports this model/run identity is already published or otherwise conflicts; re-import it with a unique run identity before publishing"
    )]
    IdentityConflict,
    #[error("the publication spool failed integrity verification")]
    SpoolTampered,
    #[error("generation publication state is malformed or exceeds its bound")]
    State,
    #[error("generation publication transport failed")]
    Transport,
    #[error("the origin may have finalized the generation; reconcile before retrying")]
    FinalizeUncertain,
    #[error("the publication origin returned HTTP {0}")]
    Http(u16),
    #[error("generation publication protocol validation failed: {0}")]
    Protocol(#[from] rw_community_protocol::ProtocolError),
    #[error("rw-store validation failed")]
    Store,
    #[error("rw-query validation failed")]
    Query,
    #[error("generation publication local storage failed")]
    Io,
}

impl From<std::io::Error> for PublicationError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

impl From<rw_store::RwStoreError> for PublicationError {
    fn from(_: rw_store::RwStoreError) -> Self {
        Self::Store
    }
}

impl From<rw_query::QueryError> for PublicationError {
    fn from(_: rw_query::QueryError) -> Self {
        Self::Query
    }
}

impl From<serde_json::Error> for PublicationError {
    fn from(_: serde_json::Error) -> Self {
        Self::State
    }
}

pub(crate) struct GenerationPublicationStore {
    root: PathBuf,
    settings: ValidatedPublicationSettings,
}

pub(crate) trait GenerationPublicationTransport {
    fn capabilities(
        &self,
        credentials: &GenerationOriginCredentials,
    ) -> Result<RunGenerationOwnerCapabilities, PublicationError>;

    fn begin(
        &self,
        credentials: &GenerationOriginCredentials,
        request: &BeginRunGenerationRequest,
    ) -> Result<RunGenerationUploadStatus, PublicationError>;

    fn missing(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RunGenerationMissingPage, PublicationError>;

    fn put_chunk(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        object_sha256: &str,
        bytes: &[u8],
    ) -> Result<(), PublicationError>;

    fn finalize(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        request: &FinalizeRunGenerationRequest,
    ) -> Result<PublishedRunGeneration, PublicationError>;

    fn publication(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
    ) -> Result<Option<RunGenerationOwnerRecord>, PublicationError>;

    fn list(
        &self,
        credentials: &GenerationOriginCredentials,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RunGenerationOwnerListPage, PublicationError>;

    fn cancel(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
    ) -> Result<Option<CancelledRunGeneration>, PublicationError>;

    fn revoke(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        request: &RevokeRunGenerationRequest,
    ) -> Result<RunGenerationTombstone, PublicationError>;
}

pub(crate) struct HttpsGenerationPublicationTransport {
    origin_url: String,
    origin_binding_sha256: String,
    http: reqwest::blocking::Client,
}

impl HttpsGenerationPublicationTransport {
    pub(crate) fn new(settings: &GenerationPublicationSettings) -> Result<Self, PublicationError> {
        let settings = settings.validate()?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| PublicationError::Transport)?;
        Ok(Self {
            origin_url: settings.origin_url,
            origin_binding_sha256: settings.origin_binding_sha256,
            http,
        })
    }

    fn require_credentials(
        &self,
        credentials: &GenerationOriginCredentials,
    ) -> Result<(), PublicationError> {
        if credentials.origin_binding_sha256 == self.origin_binding_sha256 {
            Ok(())
        } else {
            Err(PublicationError::WrongOrigin)
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path_and_query: &str,
        credentials: &GenerationOriginCredentials,
    ) -> Result<reqwest::blocking::RequestBuilder, PublicationError> {
        self.require_credentials(credentials)?;
        if !path_and_query.starts_with('/')
            || path_and_query.contains("//")
            || path_and_query.contains(['\\', '\0', '#'])
        {
            return Err(PublicationError::Transport);
        }
        let url = format!("{}{}", self.origin_url, path_and_query);
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(credentials.bearer_token())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CACHE_CONTROL, "no-store"))
    }

    fn json<T: serde::de::DeserializeOwned>(
        mut response: reqwest::blocking::Response,
        accepted: &[u16],
    ) -> Result<T, PublicationError> {
        let status = response.status().as_u16();
        if !accepted.contains(&status) {
            return Err(map_http(status));
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_JOB_STATE_BYTES)
        {
            return Err(PublicationError::Transport);
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut response)
            .take(MAX_JOB_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| PublicationError::Transport)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_JOB_STATE_BYTES {
            return Err(PublicationError::Transport);
        }
        serde_json::from_slice(&bytes).map_err(|_| PublicationError::Transport)
    }
}

impl GenerationPublicationTransport for HttpsGenerationPublicationTransport {
    fn capabilities(
        &self,
        credentials: &GenerationOriginCredentials,
    ) -> Result<RunGenerationOwnerCapabilities, PublicationError> {
        let value: RunGenerationOwnerCapabilities = Self::json(
            self.request(
                reqwest::Method::GET,
                RUN_GENERATION_CAPABILITIES_PATH,
                credentials,
            )?
            .send()
            .map_err(|_| PublicationError::Transport)?,
            &[200],
        )?;
        value.validate()?;
        Ok(value)
    }

    fn begin(
        &self,
        credentials: &GenerationOriginCredentials,
        request: &BeginRunGenerationRequest,
    ) -> Result<RunGenerationUploadStatus, PublicationError> {
        let value: RunGenerationUploadStatus = Self::json(
            self.request(
                reqwest::Method::POST,
                "/v1/community/generations",
                credentials,
            )?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(request)?)
            .send()
            .map_err(|_| PublicationError::Transport)?,
            &[200, 201],
        )?;
        value.validate()?;
        Ok(value)
    }

    fn missing(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RunGenerationMissingPage, PublicationError> {
        validate_job_id(generation_id)?;
        if limit == 0 || limit > MAX_RUN_GENERATION_MISSING_PAGE {
            return Err(PublicationError::Limit);
        }
        if after.is_some_and(|value| !is_sha256(value)) {
            return Err(PublicationError::InvalidGeneration);
        }
        let mut path = format!("/v1/community/generations/{generation_id}/missing?limit={limit}");
        if let Some(after) = after {
            path.push_str("&after=");
            path.push_str(after);
        }
        let page: RunGenerationMissingPage = Self::json(
            self.request(reqwest::Method::GET, &path, credentials)?
                .send()
                .map_err(|_| PublicationError::Transport)?,
            &[200],
        )?;
        page.validate(&RunGenerationLimits {
            max_generation_bytes: 4 * 1024 * 1024 * 1024 * 1024,
            max_files: 65_538,
            max_chunks: 1_000_000,
            max_chunk_bytes: 64 * 1024 * 1024,
            max_manifest_bytes: 32 * 1024 * 1024,
            max_retention_seconds: 366 * 24 * 60 * 60,
            max_provenance_entries: 64,
            max_attributions: 64,
        })?;
        if page.generation_id != generation_id {
            return Err(PublicationError::Transport);
        }
        Ok(page)
    }

    fn put_chunk(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        object_sha256: &str,
        bytes: &[u8],
    ) -> Result<(), PublicationError> {
        validate_job_id(generation_id)?;
        if !is_sha256(object_sha256) || bytes.is_empty() || hex_sha256(bytes) != object_sha256 {
            return Err(PublicationError::SpoolTampered);
        }
        let path = format!("/v1/community/generations/{generation_id}/chunks/{object_sha256}");
        let response = self
            .request(reqwest::Method::PUT, &path, credentials)?
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .map_err(|_| PublicationError::Transport)?;
        if response.status().as_u16() == 204 {
            Ok(())
        } else {
            Err(map_http(response.status().as_u16()))
        }
    }

    fn finalize(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        request: &FinalizeRunGenerationRequest,
    ) -> Result<PublishedRunGeneration, PublicationError> {
        validate_job_id(generation_id)?;
        let path = format!("/v1/community/generations/{generation_id}/finalize");
        let response = self
            .request(reqwest::Method::POST, &path, credentials)?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(request)?)
            .send()
            .map_err(|_| PublicationError::FinalizeUncertain)?;
        // Once the request body has left BowEcho, a gateway/origin timeout or
        // server error cannot prove that durable finalize did not complete.
        // Persist the ambiguity and reconcile by the exact owner record before
        // any future begin/finalize attempt.
        if response.status().is_server_error()
            || matches!(response.status().as_u16(), 408 | 425 | 429)
        {
            return Err(PublicationError::FinalizeUncertain);
        }
        let value: PublishedRunGeneration = Self::json(response, &[200, 201])?;
        value.validate()?;
        Ok(value)
    }

    fn publication(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
    ) -> Result<Option<RunGenerationOwnerRecord>, PublicationError> {
        validate_job_id(generation_id)?;
        let path = format!("/v1/community/generations/{generation_id}/publication");
        let response = self
            .request(reqwest::Method::GET, &path, credentials)?
            .send()
            .map_err(|_| PublicationError::Transport)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let value: RunGenerationOwnerRecord = Self::json(response, &[200])?;
        value.validate()?;
        Ok(Some(value))
    }

    fn list(
        &self,
        credentials: &GenerationOriginCredentials,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RunGenerationOwnerListPage, PublicationError> {
        if limit == 0 || limit > MAX_RUN_GENERATION_OWNER_PAGE {
            return Err(PublicationError::Limit);
        }
        if let Some(after) = after {
            validate_job_id(after)?;
        }
        let mut path = format!("/v1/community/generations?limit={limit}");
        if let Some(after) = after {
            path.push_str("&after=");
            path.push_str(after);
        }
        let value: RunGenerationOwnerListPage = Self::json(
            self.request(reqwest::Method::GET, &path, credentials)?
                .send()
                .map_err(|_| PublicationError::Transport)?,
            &[200],
        )?;
        value.validate()?;
        Ok(value)
    }

    fn cancel(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
    ) -> Result<Option<CancelledRunGeneration>, PublicationError> {
        validate_job_id(generation_id)?;
        let path = format!("/v1/community/generations/{generation_id}");
        let response = self
            .request(reqwest::Method::DELETE, &path, credentials)?
            .send()
            .map_err(|_| PublicationError::Transport)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let value: CancelledRunGeneration = Self::json(response, &[200])?;
        value.validate()?;
        Ok(Some(value))
    }

    fn revoke(
        &self,
        credentials: &GenerationOriginCredentials,
        generation_id: &str,
        request: &RevokeRunGenerationRequest,
    ) -> Result<RunGenerationTombstone, PublicationError> {
        validate_job_id(generation_id)?;
        let path = format!("/v1/community/generations/{generation_id}/revoke");
        let value: RunGenerationTombstone = Self::json(
            self.request(reqwest::Method::POST, &path, credentials)?
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(request)?)
                .send()
                .map_err(|_| PublicationError::Transport)?,
            &[200],
        )?;
        value.validate()?;
        Ok(value)
    }
}

impl GenerationPublicationStore {
    pub(crate) fn open(
        root: PathBuf,
        settings: &GenerationPublicationSettings,
    ) -> Result<Self, PublicationError> {
        let settings = settings.validate()?;
        ensure_real_directory(&root)?;
        ensure_real_directory(&root.join("objects"))?;
        ensure_real_directory(&root.join("state"))?;
        ensure_real_directory(&root.join("staging"))?;
        let root = fs::canonicalize(root)?;
        Ok(Self { root, settings })
    }

    /// Freeze one complete generation. This is entirely local and never
    /// starts, schedules, or resumes a network request.
    pub(crate) fn prepare(
        &self,
        request: PrepareGenerationRequest,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        if !self.settings.enabled {
            return Err(PublicationError::Disabled);
        }
        validate_store_component("publication model", &request.model)?;
        validate_store_component("publication run", &request.run)?;
        if !is_sha256(&request.owner_principal_sha256) {
            return Err(PublicationError::InvalidGeneration);
        }
        if request.now_unix < 0
            || request.retention_seconds <= 0
            || request.retention_seconds > self.settings.policy.max_retention_seconds
        {
            return Err(PublicationError::Retention);
        }
        if request.attributions.is_empty() || request.attributions.len() > MAX_ATTRIBUTIONS {
            return Err(PublicationError::Attribution);
        }
        if request.modification_notices.len() > MAX_MODIFICATION_NOTICES {
            return Err(PublicationError::Attribution);
        }

        // Serialize prepare and explicit GC across BowEcho processes. The
        // lock file is fixed beneath the validated spool root; no caller path
        // participates in its identity.
        let _spool_lock = RunLock::acquire(&self.root, SOURCE_LOCK_TIMEOUT)?;

        let (store_root, run_dir) =
            resolve_run_directory(&request.store_root, &request.model, &request.run)?;
        let _source_lock = RunLock::acquire(&run_dir, SOURCE_LOCK_TIMEOUT)?;
        require_deep_valid(&run_dir)?;
        let source_snapshot = RunSnapshot::open(&store_root, &request.model, &request.run)?;
        let source_snapshot_id = source_snapshot.descriptor().snapshot_id.clone();
        let grid_hash = source_snapshot.descriptor().grid_hash.clone();
        let source_provenance = source_snapshot
            .descriptor()
            .source_provenance
            .iter()
            .map(|source| SourceProvenance {
                provider: source.provider.clone(),
                roles: source.roles.clone(),
                products: source.products.clone(),
            })
            .collect::<Vec<_>>();
        if source_provenance.is_empty()
            || source_snapshot
                .manifest()
                .hours
                .values()
                .any(|entry| entry.source_provenance.is_empty())
        {
            return Err(PublicationError::MissingProvenance);
        }
        require_owner_kind(
            &store_root,
            &request.model,
            &request.run,
            request.kind,
            &source_provenance,
        )?;

        let (attributions, modification_notices) = lock_required_notices(
            &source_provenance,
            request.attributions,
            request.modification_notices,
        )?;
        let manifest =
            RwsRunManifest::load_for_run(&run_dir.join("run.json"), &request.model, &request.run)?;
        let file_specs = closed_file_specs(&manifest, &source_snapshot)?;
        preflight_files(&run_dir, &file_specs, &self.settings.policy)?;
        self.require_spool_capacity(file_specs.iter().try_fold(0_u64, |total, file| {
            let path = checked_regular_child(&run_dir, &file.file_name)?;
            total
                .checked_add(fs::metadata(path)?.len())
                .ok_or(PublicationError::Limit)
        })?)?;

        let temporary =
            TemporaryRun::create(&self.root.join("staging"), &request.model, &request.run)?;
        let (files, total_bytes) = self.freeze_files(&run_dir, &temporary.run_dir, &file_specs)?;
        require_deep_valid(&temporary.run_dir)?;
        let staged_snapshot =
            RunSnapshot::open(&temporary.store_root, &request.model, &request.run)?;
        if staged_snapshot.descriptor().grid_hash != grid_hash
            || staged_snapshot.descriptor().source_provenance
                != source_snapshot.descriptor().source_provenance
        {
            return Err(PublicationError::InvalidGeneration);
        }

        // Revalidate and rehash the source after the full copy. A process
        // ignoring the advisory lock cannot make partially copied bytes look
        // like the generation that passed the first validation.
        require_deep_valid(&run_dir)?;
        for file in &files {
            let source = checked_regular_child(&run_dir, &file.file_name)?;
            if hash_file_exact(&source, file.byte_size)? != file.file_sha256 {
                return Err(PublicationError::InvalidGeneration);
            }
        }
        let final_snapshot = RunSnapshot::open(&store_root, &request.model, &request.run)?;
        if final_snapshot.descriptor().snapshot_id != source_snapshot_id {
            return Err(PublicationError::InvalidGeneration);
        }

        let provisional = RunGenerationReplicationManifest {
            schema: RUN_GENERATION_REPLICATION_SCHEMA.to_owned(),
            generation_id: "pending".to_owned(),
            model: request.model.clone(),
            run: request.run.clone(),
            source_snapshot_id: source_snapshot_id.clone(),
            grid_hash: grid_hash.clone(),
            owner_principal_sha256: request.owner_principal_sha256.clone(),
            publication: PublicationGrant::default(),
            source_provenance: source_provenance.clone(),
            files: files.clone(),
            total_bytes,
            generation_sha256: String::new(),
            published_unix: request.now_unix,
            retain_until_unix: request.now_unix + request.retention_seconds,
            attributions: attributions.clone(),
            modification_notices: modification_notices.clone(),
        };
        let generation_sha256 = generation_content_sha256(&provisional)?;
        // Preparing the same immutable bytes again for the same owner and
        // origin reuses the durable local network identity. Cancel/revoke are
        // explicit terminal boundaries and intentionally permit a fresh ID.
        if let Some(existing) = self.list_local_jobs()?.into_iter().find(|existing| {
            existing.origin_binding_sha256 == self.settings.origin_binding_sha256
                && existing.owner_principal_sha256 == request.owner_principal_sha256
                && existing.generation_sha256 == generation_sha256
                && existing.reusable_for_prepare()
        }) {
            return Ok(existing);
        }
        let job_id = random_job_id(&generation_sha256)?;
        let job = GenerationPublicationJob {
            schema: JOB_SCHEMA.to_owned(),
            job_id,
            origin_id: self.settings.origin_id.clone(),
            origin_binding_sha256: self.settings.origin_binding_sha256.clone(),
            model: request.model,
            run: request.run,
            generation_sha256,
            source_snapshot_id,
            grid_hash,
            owner_principal_sha256: request.owner_principal_sha256,
            kind: request.kind,
            source_provenance,
            files,
            total_bytes,
            retention_seconds: request.retention_seconds,
            attributions,
            modification_notices,
            status: GenerationJobStatus::Prepared,
            created_unix: request.now_unix,
            updated_unix: request.now_unix,
        };
        self.persist_new_or_identical(&job)
    }

    pub(crate) fn confirm(
        &self,
        job_id: &str,
        confirmations: PublicationConfirmations,
        now_unix: i64,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        if !confirmations.all_confirmed() || now_unix < 0 {
            return Err(PublicationError::ConfirmationRequired);
        }
        self.update_job(job_id, |job| {
            if job.status != GenerationJobStatus::Prepared {
                return Err(PublicationError::InvalidState);
            }
            job.status = GenerationJobStatus::Confirmed {
                confirmed_unix: now_unix,
            };
            job.updated_unix = now_unix;
            job.replication_manifest(&self.settings.policy.protocol_limits())?;
            Ok(())
        })
    }

    /// Explicit network action. This is the only entry point that begins or
    /// resumes an upload; opening BowEcho and loading persisted jobs never
    /// calls it. Capabilities are fetched first and enforced as an admission
    /// contract before the origin receives a begin request.
    pub(crate) fn publish(
        &self,
        transport: &impl GenerationPublicationTransport,
        credentials: &GenerationOriginCredentials,
        job_id: &str,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        if !self.settings.enabled {
            return Err(PublicationError::Disabled);
        }
        if credentials.origin_binding_sha256 != self.settings.origin_binding_sha256 {
            return Err(PublicationError::WrongOrigin);
        }
        let mut job = self.load_job(job_id)?;
        if !matches!(
            job.status,
            GenerationJobStatus::Confirmed { .. }
                | GenerationJobStatus::OriginBeginUncertain { .. }
                | GenerationJobStatus::Uploading { .. }
                | GenerationJobStatus::FinalizeUncertain { .. }
                | GenerationJobStatus::Failed {
                    retryable: true,
                    ..
                }
        ) {
            return Err(PublicationError::InvalidState);
        }
        self.verify_job_spool(&job)?;

        if matches!(job.status, GenerationJobStatus::FinalizeUncertain { .. })
            && let Some(record) = transport.publication(credentials, job_id)?
        {
            return self.apply_remote_record(job_id, record);
        }

        let capabilities = transport.capabilities(credentials)?;
        enforce_remote_capabilities(&job, &capabilities)?;
        let limits = limits_from_capabilities(&capabilities)?;
        let manifest = job.replication_manifest(&limits)?;
        let confirmed_unix = job.confirmed_unix().ok_or(PublicationError::InvalidState)?;
        let begin = BeginRunGenerationRequest {
            schema: BEGIN_RUN_GENERATION_SCHEMA.to_owned(),
            manifest,
        };
        begin.validate(&limits)?;
        if !matches!(job.status, GenerationJobStatus::OriginBeginUncertain { .. }) {
            job.status = GenerationJobStatus::OriginBeginUncertain { confirmed_unix };
            self.write_job_locked(&job)?;
        }
        let status = transport.begin(credentials, &begin)?;
        require_upload_status(&job, &status)?;
        job.status = GenerationJobStatus::Uploading {
            confirmed_unix,
            uploaded_chunks: job.total_chunks().saturating_sub(status.missing_chunks),
            total_chunks: job.total_chunks(),
        };
        self.write_job_locked(&job)?;

        let declared = declared_chunks(&job)?;
        let mut uploaded = job.total_chunks().saturating_sub(status.missing_chunks);
        let mut after = None::<String>;
        let mut seen_missing = BTreeSet::<String>::new();
        let mut seen_cursors = BTreeSet::<String>::new();
        loop {
            let page = transport.missing(
                credentials,
                job_id,
                after.as_deref(),
                MAX_RUN_GENERATION_MISSING_PAGE,
            )?;
            if page.next_after.is_some() && page.chunks.is_empty() {
                return Err(PublicationError::Transport);
            }
            for missing in &page.chunks {
                if !seen_missing.insert(missing.object_sha256.clone())
                    || seen_missing.len() > job.total_chunks() as usize
                {
                    return Err(PublicationError::Transport);
                }
                let expected = declared
                    .get(&missing.object_sha256)
                    .ok_or(PublicationError::InvalidGeneration)?;
                if *expected != missing.byte_size {
                    return Err(PublicationError::InvalidGeneration);
                }
                let bytes = self.read_object(&missing.object_sha256, missing.byte_size)?;
                transport.put_chunk(credentials, job_id, &missing.object_sha256, &bytes)?;
                uploaded = uploaded.saturating_add(1).min(job.total_chunks());
                job.status = GenerationJobStatus::Uploading {
                    confirmed_unix,
                    uploaded_chunks: uploaded,
                    total_chunks: job.total_chunks(),
                };
                self.write_job_locked(&job)?;
            }
            match page.next_after {
                Some(next) => {
                    if after.as_ref() == Some(&next) || !seen_cursors.insert(next.clone()) {
                        return Err(PublicationError::Transport);
                    }
                    after = Some(next);
                }
                None => break,
            }
        }

        let finalize = FinalizeRunGenerationRequest {
            schema: FINALIZE_RUN_GENERATION_SCHEMA.to_owned(),
            generation_sha256: job.generation_sha256.clone(),
        };
        finalize.validate()?;
        let published = match transport.finalize(credentials, job_id, &finalize) {
            Ok(published) => published,
            Err(PublicationError::FinalizeUncertain) | Err(PublicationError::Transport) => {
                job.status = GenerationJobStatus::FinalizeUncertain { confirmed_unix };
                self.write_job_locked(&job)?;
                return Err(PublicationError::FinalizeUncertain);
            }
            Err(error) => return Err(error),
        };
        require_published_identity(&job, &published)?;
        job.status = GenerationJobStatus::Published { result: published };
        self.write_job_locked(&job)?;
        Ok(job)
    }

    pub(crate) fn reconcile(
        &self,
        transport: &impl GenerationPublicationTransport,
        credentials: &GenerationOriginCredentials,
        job_id: &str,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        let job = self.load_job(job_id)?;
        let Some(record) = transport.publication(credentials, job_id)? else {
            return Ok(job);
        };
        self.apply_remote_record(job_id, record)
    }

    pub(crate) fn list_remote(
        &self,
        transport: &impl GenerationPublicationTransport,
        credentials: &GenerationOriginCredentials,
    ) -> Result<Vec<RunGenerationOwnerRecord>, PublicationError> {
        let capabilities = transport.capabilities(credentials)?;
        capabilities.validate()?;
        let maximum_records = capabilities
            .quota
            .maximum_generations
            .checked_add(capabilities.usage.tombstones)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(PublicationError::Limit)?
            .min(self.settings.policy.max_files);
        let mut records = Vec::new();
        let mut after = None::<String>;
        let mut seen_ids = BTreeSet::<String>::new();
        let mut seen_cursors = BTreeSet::<String>::new();
        loop {
            let page =
                transport.list(credentials, after.as_deref(), MAX_RUN_GENERATION_OWNER_PAGE)?;
            if page.next_after.is_some() && page.records.is_empty() {
                return Err(PublicationError::Transport);
            }
            for record in page.records {
                if !seen_ids.insert(record.generation_id.clone()) {
                    return Err(PublicationError::Transport);
                }
                records.push(record);
                if records.len() > maximum_records {
                    return Err(PublicationError::Transport);
                }
            }
            match page.next_after {
                Some(next) => {
                    if after.as_ref() == Some(&next) || !seen_cursors.insert(next.clone()) {
                        return Err(PublicationError::Transport);
                    }
                    after = Some(next);
                }
                None => break,
            }
        }
        Ok(records)
    }

    pub(crate) fn cancel(
        &self,
        transport: &impl GenerationPublicationTransport,
        credentials: &GenerationOriginCredentials,
        job_id: &str,
        now_unix: i64,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        let job = self.load_job(job_id)?;
        match job.status {
            GenerationJobStatus::OriginBeginUncertain { .. }
            | GenerationJobStatus::Uploading { .. }
            | GenerationJobStatus::Failed {
                retryable: true, ..
            } => {
                if let Some(cancelled) = transport.cancel(credentials, job_id)?
                    && (cancelled.generation_id != job.job_id
                        || cancelled.generation_sha256 != job.generation_sha256)
                {
                    return Err(PublicationError::Transport);
                }
            }
            GenerationJobStatus::FinalizeUncertain { .. } => {
                // Do not release bytes or mutate a possibly committed
                // publication until its exact owner record is reconciled.
                return Err(PublicationError::FinalizeUncertain);
            }
            _ => return Err(PublicationError::InvalidState),
        }
        self.update_job(job_id, |job| {
            job.status = GenerationJobStatus::Cancelled {
                cancelled_unix: now_unix,
            };
            job.updated_unix = now_unix;
            Ok(())
        })
    }

    /// Discard a job that has never created an origin reservation. Prepared
    /// and Confirmed are both local-only states: `publish` persists an
    /// OriginBeginUncertain barrier before sending the first begin request.
    /// This path intentionally takes neither transport nor credentials and
    /// therefore works offline.
    pub(crate) fn discard_local(
        &self,
        job_id: &str,
        now_unix: i64,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        if now_unix < 0 {
            return Err(PublicationError::InvalidState);
        }
        self.update_job(job_id, |job| {
            if !matches!(
                job.status,
                GenerationJobStatus::Prepared | GenerationJobStatus::Confirmed { .. }
            ) {
                return Err(PublicationError::InvalidState);
            }
            job.status = GenerationJobStatus::Cancelled {
                cancelled_unix: now_unix,
            };
            job.updated_unix = now_unix;
            Ok(())
        })
    }

    pub(crate) fn revoke(
        &self,
        transport: &impl GenerationPublicationTransport,
        credentials: &GenerationOriginCredentials,
        job_id: &str,
        reason: &str,
        now_unix: i64,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        let job = self.load_job(job_id)?;
        if !matches!(job.status, GenerationJobStatus::Published { .. }) {
            return Err(PublicationError::InvalidState);
        }
        let request = RevokeRunGenerationRequest {
            schema: REVOKE_RUN_GENERATION_SCHEMA.to_owned(),
            generation_sha256: job.generation_sha256.clone(),
            rights_withdrawn: true,
            reason: reason.trim().to_owned(),
        };
        request.validate()?;
        let tombstone = transport.revoke(credentials, job_id, &request)?;
        if tombstone.generation_id != job.job_id
            || tombstone.generation_sha256 != job.generation_sha256
            || tombstone.owner_principal_sha256 != job.owner_principal_sha256
        {
            return Err(PublicationError::Transport);
        }
        self.update_job(job_id, |job| {
            job.status = GenerationJobStatus::Revoked { tombstone };
            job.updated_unix = now_unix;
            Ok(())
        })
    }

    pub(crate) fn load_job(
        &self,
        job_id: &str,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        validate_job_id(job_id)?;
        let path = self.state_path(job_id);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() || is_link_like(&metadata) {
            return Err(PublicationError::State);
        }
        if metadata.len() == 0 || metadata.len() > MAX_JOB_STATE_BYTES {
            return Err(PublicationError::State);
        }
        let mut file = File::open(path)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_JOB_STATE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JOB_STATE_BYTES {
            return Err(PublicationError::State);
        }
        let job: GenerationPublicationJob = serde_json::from_slice(&bytes)?;
        self.validate_job(&job)?;
        if job.job_id != job_id {
            return Err(PublicationError::State);
        }
        Ok(job)
    }

    pub(crate) fn list_local_jobs(
        &self,
    ) -> Result<Vec<GenerationPublicationJob>, PublicationError> {
        let mut jobs = Vec::new();
        let state_root = self.root.join("state");
        for entry in fs::read_dir(state_root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(job_id) = name.strip_suffix(".json") {
                jobs.push(self.load_job(job_id)?);
            }
        }
        jobs.sort_by(|left, right| {
            right
                .updated_unix
                .cmp(&left.updated_unix)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        Ok(jobs)
    }

    /// Explicit bounded garbage collection. No background task calls this.
    /// Chunks referenced by a resumable/ambiguous local job are never
    /// removed, even when another terminal job references the same object.
    pub(crate) fn collect_spool(&self) -> Result<SpoolCollectionReport, PublicationError> {
        let _spool_lock = RunLock::acquire(&self.root, SOURCE_LOCK_TIMEOUT)?;
        let jobs = self.list_local_jobs()?;
        let mut protected = BTreeMap::<String, u64>::new();
        for job in jobs.iter().filter(|job| job.spool_must_remain()) {
            for chunk in job.files.iter().flat_map(|file| &file.chunks) {
                match protected.insert(chunk.object_sha256.clone(), chunk.byte_size) {
                    Some(existing) if existing != chunk.byte_size => {
                        return Err(PublicationError::State);
                    }
                    _ => {}
                }
            }
        }

        let mut report = SpoolCollectionReport::default();
        let objects_root = self.root.join("objects");
        let mut inspected = 0usize;
        let maximum_entries = self.maximum_spool_entries()?;
        for prefix_entry in fs::read_dir(&objects_root)? {
            inspected = inspected.checked_add(1).ok_or(PublicationError::Limit)?;
            if inspected > maximum_entries {
                return Err(PublicationError::Limit);
            }
            let prefix_entry = prefix_entry?;
            let prefix = prefix_entry.file_name().to_string_lossy().into_owned();
            let prefix_meta = fs::symlink_metadata(prefix_entry.path())?;
            if prefix.len() != 2
                || !prefix.bytes().all(is_lower_hex)
                || !prefix_meta.file_type().is_dir()
                || is_link_like(&prefix_meta)
            {
                return Err(PublicationError::SpoolTampered);
            }
            for object_entry in fs::read_dir(prefix_entry.path())? {
                inspected = inspected.checked_add(1).ok_or(PublicationError::Limit)?;
                if inspected > maximum_entries {
                    return Err(PublicationError::Limit);
                }
                let object_entry = object_entry?;
                let sha256 = object_entry.file_name().to_string_lossy().into_owned();
                let metadata = fs::symlink_metadata(object_entry.path())?;
                if !is_sha256(&sha256)
                    || !sha256.starts_with(&prefix)
                    || !metadata.file_type().is_file()
                    || is_link_like(&metadata)
                {
                    return Err(PublicationError::SpoolTampered);
                }
                if let Some(expected_size) = protected.get(&sha256) {
                    if metadata.len() != *expected_size
                        || hash_file_exact(&object_entry.path(), metadata.len())? != sha256
                    {
                        return Err(PublicationError::SpoolTampered);
                    }
                    report.protected_objects += 1;
                    report.protected_bytes = report
                        .protected_bytes
                        .checked_add(metadata.len())
                        .ok_or(PublicationError::Limit)?;
                    continue;
                }
                fs::remove_file(object_entry.path())?;
                report.removed_objects += 1;
                report.removed_object_bytes = report
                    .removed_object_bytes
                    .checked_add(metadata.len())
                    .ok_or(PublicationError::Limit)?;
            }
            if fs::read_dir(prefix_entry.path())?.next().is_none() {
                fs::remove_dir(prefix_entry.path())?;
            }
        }

        // A live prepare always holds `_spool_lock`, so any exact generated
        // staging child visible here survived an interrupted process.
        let staging_root = self.root.join("staging");
        for entry in fs::read_dir(&staging_root)? {
            inspected = inspected.checked_add(1).ok_or(PublicationError::Limit)?;
            if inspected > maximum_entries {
                return Err(PublicationError::Limit);
            }
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(entry.path())?;
            if !generated_staging_name(&name)
                || !metadata.file_type().is_dir()
                || is_link_like(&metadata)
                || entry.path().parent() != Some(staging_root.as_path())
            {
                return Err(PublicationError::SpoolTampered);
            }
            let bytes = scan_tree_bytes(&entry.path(), &mut inspected, maximum_entries)?;
            fs::remove_dir_all(entry.path())?;
            report.removed_staging_directories += 1;
            report.removed_staging_bytes = report
                .removed_staging_bytes
                .checked_add(bytes)
                .ok_or(PublicationError::Limit)?;
        }
        Ok(report)
    }

    pub(crate) fn spool_usage_bytes(&self) -> Result<u64, PublicationError> {
        let maximum_entries = self.maximum_spool_entries()?;
        let mut inspected = 0usize;
        ["objects", "state", "staging"]
            .into_iter()
            .try_fold(0_u64, |total, child| {
                let bytes =
                    scan_tree_bytes(&self.root.join(child), &mut inspected, maximum_entries)?;
                total.checked_add(bytes).ok_or(PublicationError::Limit)
            })
    }

    fn require_spool_capacity(&self, generation_bytes: u64) -> Result<(), PublicationError> {
        let current = self.spool_usage_bytes()?;
        // Worst-case preparation temporarily holds a complete validation copy
        // and entirely novel CAS objects. Reserve one maximum-size state file
        // as well; admission may be conservative but can never overcommit.
        let required = generation_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(MAX_JOB_STATE_BYTES))
            .ok_or(PublicationError::Limit)?;
        if current
            .checked_add(required)
            .is_none_or(|peak| peak > self.settings.policy.max_spool_bytes)
        {
            return Err(PublicationError::Limit);
        }
        Ok(())
    }

    fn maximum_spool_entries(&self) -> Result<usize, PublicationError> {
        self.settings
            .policy
            .max_chunks
            .checked_mul(2)
            .and_then(|value| value.checked_add(self.settings.policy.max_files.saturating_mul(4)))
            .and_then(|value| value.checked_add(16_384))
            .ok_or(PublicationError::Limit)
    }

    fn freeze_files(
        &self,
        source_run: &Path,
        staged_run: &Path,
        specs: &[ClosedFileSpec],
    ) -> Result<(Vec<RunGenerationFile>, u64), PublicationError> {
        let mut files = Vec::with_capacity(specs.len());
        let mut total_bytes = 0_u64;
        let mut total_chunks = 0_usize;
        for spec in specs {
            let source = checked_regular_child(source_run, &spec.file_name)?;
            let destination = staged_run.join(&spec.file_name);
            let metadata = fs::metadata(&source)?;
            let size = metadata.len();
            if size == 0 {
                return Err(PublicationError::InvalidGeneration);
            }
            total_bytes = total_bytes
                .checked_add(size)
                .ok_or(PublicationError::Limit)?;
            if total_bytes > self.settings.policy.max_generation_bytes {
                return Err(PublicationError::Limit);
            }
            let (file_sha256, chunks) = self.copy_and_chunk(&source, &destination, size)?;
            total_chunks = total_chunks
                .checked_add(chunks.len())
                .ok_or(PublicationError::Limit)?;
            if total_chunks > self.settings.policy.max_chunks {
                return Err(PublicationError::Limit);
            }
            files.push(RunGenerationFile {
                schema: RUN_GENERATION_FILE_SCHEMA.to_owned(),
                kind: spec.kind,
                file_name: spec.file_name.clone(),
                byte_size: size,
                file_sha256,
                chunks,
            });
        }
        Ok((files, total_bytes))
    }

    fn copy_and_chunk(
        &self,
        source: &Path,
        destination: &Path,
        expected_size: u64,
    ) -> Result<(String, Vec<RunGenerationFileChunk>), PublicationError> {
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        let chunk_capacity = usize::try_from(self.settings.policy.chunk_bytes)
            .map_err(|_| PublicationError::Limit)?;
        let mut buffer = vec![0_u8; chunk_capacity];
        let mut file_digest = Sha256::new();
        let mut chunks = Vec::new();
        let mut offset = 0_u64;
        while offset < expected_size {
            let wanted =
                usize::try_from((expected_size - offset).min(self.settings.policy.chunk_bytes))
                    .map_err(|_| PublicationError::Limit)?;
            input.read_exact(&mut buffer[..wanted])?;
            let bytes = &buffer[..wanted];
            output.write_all(bytes)?;
            file_digest.update(bytes);
            let object_sha256 = hex_sha256(bytes);
            self.persist_object(&object_sha256, bytes)?;
            chunks.push(RunGenerationFileChunk {
                schema: RUN_GENERATION_CHUNK_SCHEMA_V1.to_owned(),
                ordinal: u32::try_from(chunks.len()).map_err(|_| PublicationError::Limit)?,
                file_offset: offset,
                object_sha256,
                byte_size: wanted as u64,
            });
            offset += wanted as u64;
        }
        let mut trailing = [0_u8; 1];
        if input.read(&mut trailing)? != 0 {
            return Err(PublicationError::InvalidGeneration);
        }
        output.sync_all()?;
        if fs::metadata(destination)?.len() != expected_size {
            return Err(PublicationError::InvalidGeneration);
        }
        Ok((hex_digest(file_digest.finalize()), chunks))
    }

    fn persist_object(&self, sha256: &str, bytes: &[u8]) -> Result<(), PublicationError> {
        if !is_sha256(sha256)
            || bytes.is_empty()
            || bytes.len() as u64 > self.settings.policy.chunk_bytes
            || hex_sha256(bytes) != sha256
        {
            return Err(PublicationError::SpoolTampered);
        }
        let directory = self.root.join("objects").join(&sha256[..2]);
        ensure_real_directory(&directory)?;
        let path = directory.join(sha256);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file()
                || is_link_like(&metadata)
                || metadata.len() != bytes.len() as u64
                || hash_file_exact(&path, metadata.len())? != sha256
            {
                return Err(PublicationError::SpoolTampered);
            }
            return Ok(());
        }
        atomic_write_bytes(&path, bytes)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file()
            || is_link_like(&metadata)
            || metadata.len() != bytes.len() as u64
            || hash_file_exact(&path, metadata.len())? != sha256
        {
            return Err(PublicationError::SpoolTampered);
        }
        Ok(())
    }

    fn read_object(&self, sha256: &str, expected_size: u64) -> Result<Vec<u8>, PublicationError> {
        if !is_sha256(sha256)
            || expected_size == 0
            || expected_size > self.settings.policy.chunk_bytes
        {
            return Err(PublicationError::SpoolTampered);
        }
        let path = self.root.join("objects").join(&sha256[..2]).join(sha256);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file()
            || is_link_like(&metadata)
            || metadata.len() != expected_size
        {
            return Err(PublicationError::SpoolTampered);
        }
        let mut file = File::open(path)?;
        let mut bytes = Vec::with_capacity(expected_size as usize);
        Read::by_ref(&mut file)
            .take(expected_size + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size || hex_sha256(&bytes) != sha256 {
            return Err(PublicationError::SpoolTampered);
        }
        Ok(bytes)
    }

    fn persist_new_or_identical(
        &self,
        job: &GenerationPublicationJob,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        let _guard = state_lock().lock().map_err(|_| PublicationError::State)?;
        let _file_lock = RunLock::acquire(&self.root.join("state"), STATE_LOCK_TIMEOUT)?;
        let path = self.state_path(&job.job_id);
        if path.exists() {
            let existing = self.load_job(&job.job_id)?;
            if existing.generation_sha256 != job.generation_sha256
                || existing.origin_binding_sha256 != job.origin_binding_sha256
                || existing.owner_principal_sha256 != job.owner_principal_sha256
            {
                return Err(PublicationError::State);
            }
            return Ok(existing);
        }
        self.write_job(job)?;
        Ok(job.clone())
    }

    fn update_job(
        &self,
        job_id: &str,
        update: impl FnOnce(&mut GenerationPublicationJob) -> Result<(), PublicationError>,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        let _guard = state_lock().lock().map_err(|_| PublicationError::State)?;
        let _file_lock = RunLock::acquire(&self.root.join("state"), STATE_LOCK_TIMEOUT)?;
        let mut job = self.load_job(job_id)?;
        update(&mut job)?;
        self.validate_job(&job)?;
        self.write_job(&job)?;
        Ok(job)
    }

    fn write_job(&self, job: &GenerationPublicationJob) -> Result<(), PublicationError> {
        self.validate_job(job)?;
        let mut bytes = serde_json::to_vec_pretty(job)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_JOB_STATE_BYTES {
            return Err(PublicationError::State);
        }
        atomic_write_bytes(&self.state_path(&job.job_id), &bytes)?;
        Ok(())
    }

    fn write_job_locked(&self, job: &GenerationPublicationJob) -> Result<(), PublicationError> {
        let _guard = state_lock().lock().map_err(|_| PublicationError::State)?;
        let _file_lock = RunLock::acquire(&self.root.join("state"), STATE_LOCK_TIMEOUT)?;
        self.write_job(job)
    }

    fn verify_job_spool(&self, job: &GenerationPublicationJob) -> Result<(), PublicationError> {
        let mut seen = BTreeMap::<String, u64>::new();
        for chunk in job.files.iter().flat_map(|file| &file.chunks) {
            match seen.insert(chunk.object_sha256.clone(), chunk.byte_size) {
                Some(existing) if existing != chunk.byte_size => {
                    return Err(PublicationError::State);
                }
                Some(_) => continue,
                None => {}
            }
            let _ = self.read_object(&chunk.object_sha256, chunk.byte_size)?;
        }
        Ok(())
    }

    fn apply_remote_record(
        &self,
        job_id: &str,
        record: RunGenerationOwnerRecord,
    ) -> Result<GenerationPublicationJob, PublicationError> {
        record.validate()?;
        self.update_job(job_id, |job| {
            if record.generation_id != job.job_id
                || record.generation_sha256 != job.generation_sha256
            {
                return Err(PublicationError::Transport);
            }
            match record.state {
                RunGenerationOwnerRecordState::Published => {
                    let published = record.publication.ok_or(PublicationError::Transport)?;
                    require_published_identity(job, &published)?;
                    job.updated_unix = published.published_unix;
                    job.status = GenerationJobStatus::Published { result: published };
                }
                RunGenerationOwnerRecordState::Tombstone => {
                    let tombstone = record.tombstone.ok_or(PublicationError::Transport)?;
                    if tombstone.owner_principal_sha256 != job.owner_principal_sha256 {
                        return Err(PublicationError::Transport);
                    }
                    job.updated_unix = tombstone.revoked_unix;
                    job.status = GenerationJobStatus::Revoked { tombstone };
                }
            }
            Ok(())
        })
    }

    fn validate_job(&self, job: &GenerationPublicationJob) -> Result<(), PublicationError> {
        if job.schema != JOB_SCHEMA
            || job.origin_id != self.settings.origin_id
            || job.origin_binding_sha256 != self.settings.origin_binding_sha256
            || !is_sha256(&job.generation_sha256)
            || !is_sha256(&job.source_snapshot_id)
            || !is_sha256(&job.grid_hash)
            || !is_sha256(&job.owner_principal_sha256)
            || validate_job_id(&job.job_id).is_err()
            || !job
                .job_id
                .strip_prefix("be-")
                .and_then(|suffix| suffix.split_once('-'))
                .is_some_and(|(prefix, _)| job.generation_sha256.starts_with(prefix))
            || job.files.len() < 3
            || job.files.len() > self.settings.policy.max_files
            || job.total_bytes == 0
            || job.total_bytes > self.settings.policy.max_generation_bytes
            || job.retention_seconds <= 0
            || job.retention_seconds > self.settings.policy.max_retention_seconds
            || job.attributions.is_empty()
            || job.source_provenance.is_empty()
        {
            return Err(PublicationError::State);
        }
        validate_store_component("publication model", &job.model)?;
        validate_store_component("publication run", &job.run)?;
        if job.confirmed_unix().is_some() {
            job.replication_manifest(&self.settings.policy.protocol_limits())?;
        }
        Ok(())
    }

    fn state_path(&self, job_id: &str) -> PathBuf {
        self.root.join("state").join(format!("{job_id}.json"))
    }
}

#[derive(Debug, Clone)]
struct ClosedFileSpec {
    kind: RunGenerationFileKind,
    file_name: String,
}

fn closed_file_specs(
    manifest: &RwsRunManifest,
    snapshot: &RunSnapshot,
) -> Result<Vec<ClosedFileSpec>, PublicationError> {
    let times = snapshot
        .time_axis()
        .iter()
        .map(|time| (time.storage_slot, time.valid_unix))
        .collect::<BTreeMap<_, _>>();
    if times.len() != manifest.hours.len() || manifest.hours.is_empty() {
        return Err(PublicationError::InvalidGeneration);
    }
    let mut files = Vec::with_capacity(manifest.hours.len() + 2);
    files.push(ClosedFileSpec {
        kind: RunGenerationFileKind::RunManifest,
        file_name: "run.json".to_owned(),
    });
    files.push(ClosedFileSpec {
        kind: RunGenerationFileKind::Grid,
        file_name: "grid.rwg".to_owned(),
    });
    for (&storage_slot, entry) in &manifest.hours {
        if !entry.file.ends_with(".rws") {
            return Err(PublicationError::InvalidGeneration);
        }
        let valid_unix = *times
            .get(&storage_slot)
            .ok_or(PublicationError::InvalidGeneration)?;
        files.push(ClosedFileSpec {
            kind: RunGenerationFileKind::Hour {
                storage_slot,
                valid_unix,
            },
            file_name: entry.file.clone(),
        });
    }
    Ok(files)
}

fn preflight_files(
    run_dir: &Path,
    files: &[ClosedFileSpec],
    policy: &GenerationPublicationPolicy,
) -> Result<(), PublicationError> {
    if files.len() < 3 || files.len() > policy.max_files {
        return Err(PublicationError::Limit);
    }
    let mut total = 0_u64;
    let mut chunks = 0_u64;
    for file in files {
        let path = checked_regular_child(run_dir, &file.file_name)?;
        let size = fs::metadata(path)?.len();
        if size == 0 {
            return Err(PublicationError::InvalidGeneration);
        }
        total = total.checked_add(size).ok_or(PublicationError::Limit)?;
        chunks = chunks
            .checked_add(size.div_ceil(policy.chunk_bytes))
            .ok_or(PublicationError::Limit)?;
    }
    let peak = total.checked_mul(2).ok_or(PublicationError::Limit)?;
    if total > policy.max_generation_bytes
        || peak > policy.max_spool_bytes
        || chunks > policy.max_chunks as u64
    {
        return Err(PublicationError::Limit);
    }
    Ok(())
}

fn limits_from_capabilities(
    capabilities: &RunGenerationOwnerCapabilities,
) -> Result<RunGenerationLimits, PublicationError> {
    capabilities.validate()?;
    let limits = RunGenerationLimits {
        max_generation_bytes: capabilities.limits.maximum_generation_bytes,
        max_files: usize::try_from(capabilities.limits.maximum_files)
            .map_err(|_| PublicationError::Limit)?,
        max_chunks: usize::try_from(capabilities.limits.maximum_chunks)
            .map_err(|_| PublicationError::Limit)?,
        max_chunk_bytes: capabilities.limits.maximum_chunk_bytes,
        max_manifest_bytes: usize::try_from(capabilities.limits.maximum_manifest_bytes)
            .map_err(|_| PublicationError::Limit)?,
        max_retention_seconds: capabilities.limits.maximum_retention_seconds,
        max_provenance_entries: usize::try_from(capabilities.limits.maximum_provenance_entries)
            .map_err(|_| PublicationError::Limit)?,
        max_attributions: usize::try_from(capabilities.limits.maximum_attributions)
            .map_err(|_| PublicationError::Limit)?,
    };
    limits.validate()?;
    Ok(limits)
}

fn enforce_remote_capabilities(
    job: &GenerationPublicationJob,
    capabilities: &RunGenerationOwnerCapabilities,
) -> Result<(), PublicationError> {
    capabilities.validate()?;
    if !capabilities.accepting_uploads {
        return Err(PublicationError::Limit);
    }
    if capabilities.owner_principal_sha256 != job.owner_principal_sha256 {
        return Err(PublicationError::Credentials);
    }
    let limits = limits_from_capabilities(capabilities)?;
    if job.total_bytes > limits.max_generation_bytes
        || job.files.len() > limits.max_files
        || job.files.iter().any(|file| {
            file.chunks
                .iter()
                .any(|chunk| chunk.byte_size > limits.max_chunk_bytes)
        })
        || job.total_chunks() as usize > limits.max_chunks
        || job.retention_seconds < capabilities.limits.minimum_retention_seconds
        || job.retention_seconds > capabilities.limits.maximum_retention_seconds
        || job.source_provenance.len() > limits.max_provenance_entries
        || job.attributions.len() > limits.max_attributions
    {
        return Err(PublicationError::Limit);
    }
    let generations = capabilities
        .usage
        .active_uploads
        .checked_add(capabilities.usage.live_publications)
        .and_then(|value| value.checked_add(capabilities.usage.pending_retirements))
        .ok_or(PublicationError::Limit)?;
    let storage = capabilities
        .usage
        .reserved_bytes
        .checked_add(capabilities.usage.published_bytes)
        .and_then(|value| value.checked_add(capabilities.usage.pending_retirement_bytes))
        .ok_or(PublicationError::Limit)?;
    if capabilities.usage.active_uploads >= capabilities.quota.maximum_concurrent_uploads
        || generations >= capabilities.quota.maximum_generations
        || storage
            .checked_add(job.total_bytes)
            .is_none_or(|value| value > capabilities.quota.maximum_storage_bytes)
        || capabilities
            .usage
            .monthly_accepted_upload_bytes
            .checked_add(job.total_bytes)
            .is_none_or(|value| value > capabilities.quota.maximum_monthly_upload_bytes)
    {
        return Err(PublicationError::Limit);
    }
    job.replication_manifest(&limits)?;
    Ok(())
}

fn declared_chunks(
    job: &GenerationPublicationJob,
) -> Result<BTreeMap<String, u64>, PublicationError> {
    let mut declared = BTreeMap::new();
    for chunk in job.files.iter().flat_map(|file| &file.chunks) {
        match declared.insert(chunk.object_sha256.clone(), chunk.byte_size) {
            Some(existing) if existing != chunk.byte_size => {
                return Err(PublicationError::InvalidGeneration);
            }
            _ => {}
        }
    }
    Ok(declared)
}

fn require_upload_status(
    job: &GenerationPublicationJob,
    status: &RunGenerationUploadStatus,
) -> Result<(), PublicationError> {
    status.validate()?;
    if status.generation_id != job.job_id
        || status.generation_sha256 != job.generation_sha256
        || status.total_chunks != job.total_chunks()
    {
        return Err(PublicationError::Transport);
    }
    Ok(())
}

fn require_published_identity(
    job: &GenerationPublicationJob,
    published: &PublishedRunGeneration,
) -> Result<(), PublicationError> {
    published.validate()?;
    if published.generation_id != job.job_id
        || published.generation_sha256 != job.generation_sha256
        || published.source_snapshot_id != job.source_snapshot_id
        || published.grid_hash != job.grid_hash
        || published.model != job.model
        || published.run != job.run
    {
        return Err(PublicationError::Transport);
    }
    Ok(())
}

fn require_owner_kind(
    store_root: &Path,
    model: &str,
    run: &str,
    requested: OwnerGenerationKind,
    provenance: &[SourceProvenance],
) -> Result<(), PublicationError> {
    let providers = provenance
        .iter()
        .map(|source| source.provider.as_str())
        .collect::<BTreeSet<_>>();
    let has_wrf = providers.contains(crate::wrf_source::PRIVATE_WRF_PROVIDER);
    let has_arwen = providers.contains(crate::wrf_source::PRIVATE_ARWEN_PROVIDER);
    if has_wrf && has_arwen {
        return Err(PublicationError::ProducerIdentity);
    }
    match requested {
        OwnerGenerationKind::PrivateWrf if has_wrf && !has_arwen => {
            let metadata = crate::wrf_source::read_run_metadata(store_root, model, run)
                .map_err(|_| PublicationError::ProducerIdentity)?
                .ok_or(PublicationError::ProducerIdentity)?;
            if metadata.producer != "wrf" {
                return Err(PublicationError::ProducerIdentity);
            }
        }
        OwnerGenerationKind::PrivateArwen if has_arwen && !has_wrf => {
            let metadata = crate::wrf_source::read_run_metadata(store_root, model, run)
                .map_err(|_| PublicationError::ProducerIdentity)?
                .ok_or(PublicationError::ProducerIdentity)?;
            if metadata.producer != "arwen" {
                return Err(PublicationError::ProducerIdentity);
            }
        }
        OwnerGenerationKind::UserProvided if !has_wrf && !has_arwen => {}
        _ => return Err(PublicationError::ProducerIdentity),
    }
    Ok(())
}

fn lock_required_notices(
    provenance: &[SourceProvenance],
    mut attributions: Vec<AttributionNotice>,
    mut modifications: Vec<String>,
) -> Result<(Vec<AttributionNotice>, Vec<String>), PublicationError> {
    if attributions.is_empty() || attributions.len() > MAX_ATTRIBUTIONS {
        return Err(PublicationError::Attribution);
    }
    let has_ecmwf = provenance
        .iter()
        .any(|source| source.provider == "ecmwf-open-data");
    if has_ecmwf {
        attributions.retain(|notice| notice.provider != "ecmwf-open-data");
        attributions.push(AttributionNotice::ecmwf_open_data());
        modifications.retain(|notice| notice != ECMWF_MODIFICATION_NOTICE);
        modifications.push(ECMWF_MODIFICATION_NOTICE.to_owned());
    }
    if modifications.len() > MAX_MODIFICATION_NOTICES {
        return Err(PublicationError::Attribution);
    }
    Ok((attributions, modifications))
}

fn resolve_run_directory(
    store_root: &Path,
    model: &str,
    run: &str,
) -> Result<(PathBuf, PathBuf), PublicationError> {
    require_real_directory(store_root)?;
    let store_root = fs::canonicalize(store_root)?;
    let model_dir = store_root.join(model);
    require_real_directory(&model_dir)?;
    let run_dir = model_dir.join(run);
    require_real_directory(&run_dir)?;
    let canonical_run = fs::canonicalize(&run_dir)?;
    if canonical_run != run_dir || !canonical_run.starts_with(&store_root) {
        return Err(PublicationError::LinkedPath);
    }
    Ok((store_root, canonical_run))
}

fn checked_regular_child(root: &Path, file_name: &str) -> Result<PathBuf, PublicationError> {
    validate_store_component("publication filename", file_name)?;
    let path = root.join(file_name);
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.file_type().is_file() || is_link_like(&metadata) {
        return Err(PublicationError::LinkedPath);
    }
    let canonical = fs::canonicalize(&path)?;
    if canonical.parent() != Some(root) {
        return Err(PublicationError::LinkedPath);
    }
    Ok(canonical)
}

fn ensure_real_directory(path: &Path) -> Result<(), PublicationError> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    require_real_directory(path)
}

fn require_real_directory(path: &Path) -> Result<(), PublicationError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || is_link_like(&metadata) {
        return Err(PublicationError::LinkedPath);
    }
    Ok(())
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn require_deep_valid(run_dir: &Path) -> Result<(), PublicationError> {
    let report = validate_run_dir(run_dir, ValidateDepth::Deep)?;
    if report.is_ok() {
        Ok(())
    } else {
        Err(PublicationError::InvalidGeneration)
    }
}

fn scan_tree_bytes(
    root: &Path,
    inspected: &mut usize,
    maximum_entries: usize,
) -> Result<u64, PublicationError> {
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.file_type().is_dir() || is_link_like(&root_metadata) {
        return Err(PublicationError::SpoolTampered);
    }
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            *inspected = inspected.checked_add(1).ok_or(PublicationError::Limit)?;
            if *inspected > maximum_entries {
                return Err(PublicationError::Limit);
            }
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if is_link_like(&metadata) {
                return Err(PublicationError::SpoolTampered);
            }
            if metadata.file_type().is_dir() {
                stack.push(entry.path());
            } else if metadata.file_type().is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or(PublicationError::Limit)?;
            } else {
                return Err(PublicationError::SpoolTampered);
            }
        }
    }
    Ok(total)
}

fn generated_staging_name(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("prepare-") else {
        return false;
    };
    let mut parts = suffix.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(counter), None)
            if !pid.is_empty()
                && !counter.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && counter.bytes().all(|byte| byte.is_ascii_digit())
    )
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

struct TemporaryRun {
    base: PathBuf,
    store_root: PathBuf,
    run_dir: PathBuf,
}

impl TemporaryRun {
    fn create(staging_root: &Path, model: &str, run: &str) -> Result<Self, PublicationError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "prepare-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let base = staging_root.join(name);
        fs::create_dir(&base)?;
        let store_root = base.join("store");
        let model_dir = store_root.join(model);
        let run_dir = model_dir.join(run);
        fs::create_dir_all(&run_dir)?;
        Ok(Self {
            base,
            store_root,
            run_dir,
        })
    }
}

impl Drop for TemporaryRun {
    fn drop(&mut self) {
        // `base` is an exact, process-generated child created above; never a
        // caller path, glob, environment expansion, or workspace root.
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn hash_file_exact(path: &Path, expected_size: u64) -> Result<String, PublicationError> {
    let mut file = File::open(path)?;
    let mut remaining = expected_size;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| PublicationError::Limit)?;
        file.read_exact(&mut buffer[..wanted])?;
        digest.update(&buffer[..wanted]);
        remaining -= wanted as u64;
    }
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(PublicationError::InvalidGeneration);
    }
    Ok(hex_digest(digest.finalize()))
}

fn normalize_https_origin(value: &str) -> Result<String, PublicationError> {
    let mut url =
        reqwest::Url::parse(value.trim()).map_err(|_| PublicationError::InvalidSettings)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PublicationError::InvalidSettings);
    }
    let normalized_path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&normalized_path);
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn map_http(status: u16) -> PublicationError {
    match status {
        401 | 403 => PublicationError::Credentials,
        409 => PublicationError::IdentityConflict,
        413 | 429 => PublicationError::Limit,
        400 | 404 | 410 | 415 | 422 => PublicationError::InvalidGeneration,
        other => PublicationError::Http(other),
    }
}

fn origin_binding_sha256(origin_id: &str, origin_url: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(OBJECT_HASH_DOMAIN);
    digest.update((origin_id.len() as u64).to_be_bytes());
    digest.update(origin_id.as_bytes());
    digest.update((origin_url.len() as u64).to_be_bytes());
    digest.update(origin_url.as_bytes());
    hex_digest(digest.finalize())
}

fn validate_token(value: &str, maximum: usize) -> Result<(), PublicationError> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(PublicationError::InvalidSettings);
    }
    Ok(())
}

fn validate_job_id(value: &str) -> Result<(), PublicationError> {
    let Some(suffix) = value.strip_prefix("be-") else {
        return Err(PublicationError::State);
    };
    let Some((hash_prefix, nonce)) = suffix.split_once('-') else {
        return Err(PublicationError::State);
    };
    if hash_prefix.len() == 16
        && hash_prefix.bytes().all(is_lower_hex)
        && nonce.len() == 32
        && nonce.bytes().all(is_lower_hex)
    {
        Ok(())
    } else {
        Err(PublicationError::State)
    }
}

fn random_job_id(generation_sha256: &str) -> Result<String, PublicationError> {
    if !is_sha256(generation_sha256) {
        return Err(PublicationError::InvalidGeneration);
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| PublicationError::Io)?;
    Ok(format!(
        "be-{}-{}",
        &generation_sha256[..16],
        hex_digest(nonce)
    ))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

enum PublicationTaskOutput {
    Job(Box<GenerationPublicationJob>),
    LocalJobs(Vec<GenerationPublicationJob>),
    RemoteRecords(Vec<RunGenerationOwnerRecord>),
    Spool(SpoolCollectionReport),
    Capabilities(RunGenerationOwnerCapabilities),
}

struct PublicationTask {
    label: &'static str,
    rx: mpsc::Receiver<Result<PublicationTaskOutput, String>>,
}

/// Session UI for the explicit owner workflow. Persisted jobs live in the
/// redacted spool state, while confirmations are deliberately session-only so
/// reopening BowEcho can never silently continue an upload.
pub(crate) struct GenerationPublicationPanel {
    settings: settings::GenerationPublicationSettings,
    jobs: Vec<GenerationPublicationJob>,
    selected_job_id: Option<String>,
    selected_source: Option<(String, String)>,
    kind: OwnerGenerationKind,
    retention_days: u16,
    confirmations: PublicationConfirmations,
    revoke_reason: String,
    task: Option<PublicationTask>,
    next_progress_poll: Option<Instant>,
    initialized: bool,
    status: Option<String>,
    remote_records: Vec<RunGenerationOwnerRecord>,
    capabilities: Option<RunGenerationOwnerCapabilities>,
}

impl Default for GenerationPublicationPanel {
    fn default() -> Self {
        let settings = settings::GenerationPublicationSettings::default();
        Self {
            retention_days: settings.default_retention_days,
            settings,
            jobs: Vec::new(),
            selected_job_id: None,
            selected_source: None,
            kind: OwnerGenerationKind::PrivateWrf,
            confirmations: PublicationConfirmations::default(),
            revoke_reason: String::new(),
            task: None,
            next_progress_poll: None,
            initialized: false,
            status: None,
            remote_records: Vec::new(),
            capabilities: None,
        }
    }
}

impl GenerationPublicationPanel {
    pub(crate) fn set_settings(&mut self, settings: &settings::GenerationPublicationSettings) {
        let changed = self.settings != *settings;
        self.settings = settings.clone();
        if changed {
            self.initialized = false;
            self.retention_days = self
                .retention_days
                .clamp(1, self.settings.max_retention_days.max(1));
            self.capabilities = None;
            if self.task.is_some() {
                self.status = Some(
                    "Publication settings changed; the already-running explicit action keeps its original immutable configuration."
                        .to_owned(),
                );
            }
        }
    }

    fn store_for(
        settings: &settings::GenerationPublicationSettings,
    ) -> Result<GenerationPublicationStore, PublicationError> {
        let configured = GenerationPublicationSettings::from_app_settings(settings)?;
        GenerationPublicationStore::open(settings::generation_publication_dir(), &configured)
    }

    fn network_for(
        settings: &settings::GenerationPublicationSettings,
    ) -> Result<
        (
            GenerationPublicationStore,
            GenerationOriginCredentials,
            HttpsGenerationPublicationTransport,
        ),
        PublicationError,
    > {
        let configured = GenerationPublicationSettings::from_app_settings(settings)?;
        let validated = configured.validate()?;
        let credentials = load_origin_credentials(&validated)?;
        let transport = HttpsGenerationPublicationTransport::new(&configured)?;
        let store =
            GenerationPublicationStore::open(settings::generation_publication_dir(), &configured)?;
        Ok((store, credentials, transport))
    }

    fn start(
        &mut self,
        ctx: &egui::Context,
        label: &'static str,
        work: impl FnOnce() -> Result<PublicationTaskOutput, PublicationError> + Send + 'static,
    ) {
        if self.task.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        match std::thread::Builder::new()
            .name(format!("bowecho-generation-{}", label.replace(' ', "-")))
            .spawn(move || {
                let result = work().map_err(|error| error.to_string());
                let _ = tx.send(result);
                repaint.request_repaint();
            }) {
            Ok(_) => {
                self.task = Some(PublicationTask { label, rx });
                self.status = Some(format!("{label} in progress..."));
                self.next_progress_poll = Some(Instant::now());
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(_) => {
                self.status = Some(format!("Could not start {label}."));
            }
        }
    }

    fn merge_job(&mut self, job: GenerationPublicationJob) {
        let id = job.job_id.clone();
        if let Some(existing) = self.jobs.iter_mut().find(|existing| existing.job_id == id) {
            *existing = job;
        } else {
            self.jobs.push(job);
        }
        self.jobs.sort_by(|left, right| {
            right
                .updated_unix
                .cmp(&left.updated_unix)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        self.selected_job_id = Some(id);
    }

    fn reload_local_progress(&mut self) {
        let Some(job_id) = self.selected_job_id.as_deref() else {
            return;
        };
        let Ok(store) = Self::store_for(&self.settings) else {
            return;
        };
        if let Ok(job) = store.load_job(job_id) {
            self.merge_job(job);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if self.task.is_some()
            && self
                .next_progress_poll
                .is_none_or(|deadline| Instant::now() >= deadline)
        {
            self.reload_local_progress();
            self.next_progress_poll = Some(Instant::now() + Duration::from_millis(500));
        }
        let result = self.task.as_ref().and_then(|task| task.rx.try_recv().ok());
        if let Some(result) = result {
            let label = self.task.take().map(|task| task.label).unwrap_or("Action");
            self.next_progress_poll = None;
            match result {
                Ok(PublicationTaskOutput::Job(job)) => {
                    self.merge_job(*job);
                    self.status = Some(format!("{label} completed."));
                }
                Ok(PublicationTaskOutput::LocalJobs(jobs)) => {
                    self.jobs = jobs;
                    if self
                        .selected_job_id
                        .as_ref()
                        .is_none_or(|id| !self.jobs.iter().any(|job| &job.job_id == id))
                    {
                        self.selected_job_id = self.jobs.first().map(|job| job.job_id.clone());
                    }
                    self.status = Some(format!(
                        "Loaded {} local publication job(s).",
                        self.jobs.len()
                    ));
                }
                Ok(PublicationTaskOutput::RemoteRecords(records)) => {
                    self.remote_records = records;
                    self.status = Some(format!(
                        "Reconciled {} owner publication/tombstone record(s).",
                        self.remote_records.len()
                    ));
                }
                Ok(PublicationTaskOutput::Spool(report)) => {
                    self.status = Some(format!(
                        "Spool cleanup removed {} object(s), {} byte(s), and {} interrupted staging director{}; {} active object(s) remain protected.",
                        report.removed_objects,
                        report.removed_object_bytes,
                        report.removed_staging_directories,
                        if report.removed_staging_directories == 1 {
                            "y"
                        } else {
                            "ies"
                        },
                        report.protected_objects,
                    ));
                }
                Ok(PublicationTaskOutput::Capabilities(capabilities)) => {
                    self.status = Some(format!(
                        "Origin account verified: {} live publication(s), {} active upload(s).",
                        capabilities.usage.live_publications, capabilities.usage.active_uploads
                    ));
                    self.capabilities = Some(capabilities);
                }
                Err(message) => self.status = Some(format!("{label} failed: {message}")),
            }
        }
        if self.task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn refresh_local(&mut self, ctx: &egui::Context) {
        let settings = self.settings.clone();
        self.start(ctx, "Refresh local jobs", move || {
            let store = Self::store_for(&settings)?;
            Ok(PublicationTaskOutput::LocalJobs(store.list_local_jobs()?))
        });
    }

    fn sync_source_kind(&mut self, store_root: &Path, selected: Option<&rw_ui::HourKey>) {
        let identity = selected.map(|hour| (hour.model.clone(), hour.run.clone()));
        if self.selected_source == identity {
            return;
        }
        self.selected_source = identity.clone();
        if let Some((model, run)) = identity
            && let Ok(Some(metadata)) =
                crate::wrf_source::read_run_metadata(store_root, &model, &run)
        {
            self.kind = if metadata.producer == "arwen" {
                OwnerGenerationKind::PrivateArwen
            } else {
                OwnerGenerationKind::PrivateWrf
            };
        }
    }

    pub(crate) fn ui(
        &mut self,
        ui: &mut egui::Ui,
        store_root: &Path,
        selected: Option<&rw_ui::HourKey>,
    ) {
        self.poll(ui.ctx());
        self.sync_source_kind(store_root, selected);
        if !self.initialized && self.task.is_none() && self.settings.locally_ready() {
            self.initialized = true;
            self.refresh_local(ui.ctx());
        }

        ui.label(
            egui::RichText::new("OWNER GENERATION PUBLICATION")
                .size(11.5)
                .strong()
                .color(crate::ui_theme::subhead_color()),
        );
        ui.weak(
            "Publish one closed, processed rw-store generation to the trusted Rusty Weather origin over authenticated HTTPS. This never uses Community Cache, TURN, ICE, STUN, or another user.",
        );
        ui.weak(
            "Nothing starts or resumes when BowEcho opens. Prepare is local-only; Publish, Resume, Reconcile, Cancel, Revoke, and List are separate explicit actions.",
        );
        ui.weak(
            "The origin operator observes your connection address and necessary request metadata. Other BowEcho users are not part of this transfer.",
        );

        if !self.settings.enabled {
            ui.colored_label(
                egui::Color32::from_rgb(244, 194, 92),
                "Owner publication is off. Configure it under Settings > Owner generation publication.",
            );
        } else if !self.settings.locally_ready() {
            ui.colored_label(
                egui::Color32::from_rgb(244, 194, 92),
                "The trusted origin, opaque owner principal, or local bounds are incomplete.",
            );
        }

        ui.add_space(4.0);
        if let Some(hour) = selected {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Selected source");
                ui.monospace(format!("{}/{}", hour.model, hour.run));
            });
            ui.horizontal_wrapped(|ui| {
                ui.strong("Published identity");
                ui.monospace(format!("{}/{}", hour.model, hour.run));
                ui.weak("exact; publication never rewrites run.json or scientific bytes");
            });
        } else {
            ui.weak("Select a local model run/time in the Model library first.");
        }

        ui.horizontal_wrapped(|ui| {
            ui.strong("Owner data type");
            egui::ComboBox::from_id_salt("generation_publication_kind")
                .selected_text(owner_kind_label(self.kind))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.kind,
                        OwnerGenerationKind::PrivateWrf,
                        "Private WRF",
                    );
                    ui.selectable_value(
                        &mut self.kind,
                        OwnerGenerationKind::PrivateArwen,
                        "Private ArWen",
                    );
                    ui.selectable_value(
                        &mut self.kind,
                        OwnerGenerationKind::UserProvided,
                        "Other owner-provided rw-store",
                    );
                });
            ui.strong("Retention");
            ui.add(
                egui::DragValue::new(&mut self.retention_days)
                    .range(1..=self.settings.max_retention_days.max(1))
                    .suffix(" days"),
            );
        });

        let busy = self.task.is_some();
        let can_prepare =
            !busy && self.settings.locally_ready() && selected.is_some() && self.retention_days > 0;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(can_prepare, egui::Button::new("1. Prepare immutable copy"))
                .on_hover_text(
                    "Local only: lock, deep-validate, inventory, hash, and freeze exactly run.json, grid.rwg, and manifest-listed .rws files into the bounded publication spool.",
                )
                .clicked()
                && let Some(hour) = selected.cloned()
            {
                let settings = self.settings.clone();
                let store_root = store_root.to_path_buf();
                let kind = self.kind;
                let retention_seconds = i64::from(self.retention_days) * 24 * 60 * 60;
                self.start(ui.ctx(), "Prepare generation", move || {
                    let store = Self::store_for(&settings)?;
                    let attributions = vec![attribution_from_settings(&settings)];
                    let modification_notices = (!settings.modification_notice.trim().is_empty())
                        .then(|| settings.modification_notice.trim().to_owned())
                        .into_iter()
                        .collect();
                    let job = store.prepare(PrepareGenerationRequest {
                        store_root,
                        model: hour.model,
                        run: hour.run,
                        owner_principal_sha256: settings.owner_principal_sha256.clone(),
                        kind,
                        retention_seconds,
                        attributions,
                        modification_notices,
                        now_unix: chrono::Utc::now().timestamp(),
                    })?;
                    Ok(PublicationTaskOutput::Job(Box::new(job)))
                });
            }
            if ui
                .add_enabled(
                    !busy && self.settings.locally_ready(),
                    egui::Button::new("Refresh local"),
                )
                .clicked()
            {
                self.refresh_local(ui.ctx());
            }
            if ui
                .add_enabled(
                    !busy && self.settings.locally_ready(),
                    egui::Button::new("Check origin capacity"),
                )
                .on_hover_text("Explicit authenticated HTTPS request; no upload begins.")
                .clicked()
            {
                let settings = self.settings.clone();
                self.start(ui.ctx(), "Check origin capacity", move || {
                    Ok(PublicationTaskOutput::Capabilities(
                        fetch_owner_capabilities(&settings)?,
                    ))
                });
            }
        });

        if let Some(task) = &self.task {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(task.label);
            });
        }
        if let Some(status) = self.status.as_deref() {
            ui.weak(status);
        }
        if let Some(capabilities) = &self.capabilities {
            ui.weak(format!(
                "Origin: {} / {} GiB published, {} / {} generations, upload admission {}.",
                capabilities.usage.published_bytes / (1024 * 1024 * 1024),
                capabilities.quota.maximum_storage_bytes / (1024 * 1024 * 1024),
                capabilities.usage.live_publications,
                capabilities.quota.maximum_generations,
                if capabilities.accepting_uploads {
                    "open"
                } else {
                    "paused"
                },
            ));
        }

        if self.jobs.is_empty() {
            return;
        }
        ui.separator();
        let selected_label = self
            .selected_job()
            .map(job_label)
            .unwrap_or_else(|| "Choose prepared generation".to_owned());
        egui::ComboBox::from_id_salt("generation_publication_job")
            .selected_text(selected_label)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for job in &self.jobs {
                    ui.selectable_value(
                        &mut self.selected_job_id,
                        Some(job.job_id.clone()),
                        job_label(job),
                    );
                }
            });
        let Some(job) = self.selected_job().cloned() else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.monospace(format!("{}/{}", job.model, job.run));
            ui.weak(format!(
                "{} files | {} chunks | {:.2} GiB | SHA {}",
                job.files.len(),
                job.total_chunks(),
                job.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                &job.generation_sha256[..12],
            ));
        });
        ui.weak(format!(
            "Origin {} | job {} | {}",
            job.origin_id,
            job.job_id,
            job_status_label(&job.status)
        ));
        for source in &job.source_provenance {
            ui.weak(format!(
                "Provenance: {} | roles {} | products {}",
                source.provider,
                source.roles.join(", "),
                source.products.join(", ")
            ));
        }

        match &job.status {
            GenerationJobStatus::Prepared => {
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.confirmations.owner_publication,
                    "I own or am authorized to publish this processed generation.",
                );
                ui.checkbox(
                    &mut self.confirmations.redistribution_rights,
                    "I reviewed every input and confirm redistribution rights and attribution.",
                );
                ui.checkbox(
                    &mut self.confirmations.operator_connection_metadata,
                    "I understand the trusted HTTPS operator sees my connection IP and necessary metadata.",
                );
                if ui
                    .add_enabled(
                        !busy && self.confirmations.all_confirmed(),
                        egui::Button::new("2. Confirm rights and publication"),
                    )
                    .clicked()
                {
                    let settings = self.settings.clone();
                    let job_id = job.job_id.clone();
                    let confirmations = self.confirmations;
                    self.start(ui.ctx(), "Confirm publication", move || {
                        let store = Self::store_for(&settings)?;
                        Ok(PublicationTaskOutput::Job(Box::new(store.confirm(
                            &job_id,
                            confirmations,
                            chrono::Utc::now().timestamp(),
                        )?)))
                    });
                }
            }
            GenerationJobStatus::Confirmed { .. }
            | GenerationJobStatus::Failed { retryable: true, .. } => {
                if ui
                    .add_enabled(!busy, egui::Button::new("3. Publish / resume over HTTPS"))
                    .on_hover_text(
                        "Explicit network action. Checks owner-scoped capability and quota before begin, uploads only missing declared chunks, then idempotently finalizes.",
                    )
                    .clicked()
                {
                    self.start_publish(ui.ctx(), job.job_id.clone());
                }
            }
            GenerationJobStatus::OriginBeginUncertain { .. }
            | GenerationJobStatus::Uploading { .. } => {
                ui.colored_label(
                    egui::Color32::from_rgb(244, 194, 92),
                    "An origin reservation may exist. Resume idempotently or cancel it over authenticated HTTPS.",
                );
                if ui
                    .add_enabled(!busy, egui::Button::new("Resume upload over HTTPS"))
                    .clicked()
                {
                    self.start_publish(ui.ctx(), job.job_id.clone());
                }
            }
            GenerationJobStatus::FinalizeUncertain { .. } => {
                ui.colored_label(
                    egui::Color32::from_rgb(244, 194, 92),
                    "The origin may have committed finalize. Reconcile the exact owner record before another upload attempt.",
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Reconcile outcome")).clicked() {
                        self.start_reconcile(ui.ctx(), job.job_id.clone());
                    }
                    if ui
                        .add_enabled(!busy, egui::Button::new("Reconcile, then resume if absent"))
                        .clicked()
                    {
                        self.start_publish(ui.ctx(), job.job_id.clone());
                    }
                });
            }
            GenerationJobStatus::Published { .. } => {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Revocation reason");
                    ui.text_edit_singleline(&mut self.revoke_reason);
                    if ui
                        .add_enabled(
                            !busy && !self.revoke_reason.trim().is_empty(),
                            egui::Button::new("Revoke publication"),
                        )
                        .clicked()
                    {
                        let settings = self.settings.clone();
                        let job_id = job.job_id.clone();
                        let reason = self.revoke_reason.trim().to_owned();
                        self.start(ui.ctx(), "Revoke publication", move || {
                            let (store, credentials, transport) = Self::network_for(&settings)?;
                            Ok(PublicationTaskOutput::Job(Box::new(store.revoke(
                                &transport,
                                &credentials,
                                &job_id,
                                &reason,
                                chrono::Utc::now().timestamp(),
                            )?)))
                        });
                    }
                });
            }
            GenerationJobStatus::Cancelled { .. }
            | GenerationJobStatus::Revoked { .. }
            | GenerationJobStatus::Failed { retryable: false, .. } => {}
        }

        match job.status {
            GenerationJobStatus::Prepared | GenerationJobStatus::Confirmed { .. } => {
                if ui
                    .add_enabled(!busy, egui::Button::new("Discard local prepared copy"))
                    .on_hover_text(
                        "Offline local action. No origin reservation exists until Publish begins successfully. Immutable objects shared with another active job remain protected.",
                    )
                    .clicked()
                {
                    let settings = self.settings.clone();
                    let job_id = job.job_id.clone();
                    self.start(ui.ctx(), "Discard local publication", move || {
                        let store = Self::store_for(&settings)?;
                        Ok(PublicationTaskOutput::Job(Box::new(store.discard_local(
                            &job_id,
                            chrono::Utc::now().timestamp(),
                        )?)))
                    });
                }
            }
            GenerationJobStatus::OriginBeginUncertain { .. }
            | GenerationJobStatus::Uploading { .. }
            | GenerationJobStatus::Failed {
                retryable: true, ..
            } => {
                if ui
                    .add_enabled(!busy, egui::Button::new("Cancel active origin upload"))
                    .on_hover_text(
                        "Authenticated HTTPS action: cancel the durable origin upload and release its reservation.",
                    )
                    .clicked()
                {
                    let settings = self.settings.clone();
                    let job_id = job.job_id.clone();
                    self.start(ui.ctx(), "Cancel origin upload", move || {
                        let (store, credentials, transport) = Self::network_for(&settings)?;
                        Ok(PublicationTaskOutput::Job(Box::new(store.cancel(
                            &transport,
                            &credentials,
                            &job_id,
                            chrono::Utc::now().timestamp(),
                        )?)))
                    });
                }
            }
            GenerationJobStatus::FinalizeUncertain { .. } => {
                ui.weak(
                    "Cancel is disabled until exact reconciliation proves whether finalize committed.",
                );
            }
            _ => {}
        }

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!busy, egui::Button::new("List my origin records"))
                .on_hover_text("Explicit authenticated HTTPS reconciliation; records are owner-scoped.")
                .clicked()
            {
                let settings = self.settings.clone();
                self.start(ui.ctx(), "List origin records", move || {
                    let (store, credentials, transport) = Self::network_for(&settings)?;
                    Ok(PublicationTaskOutput::RemoteRecords(
                        store.list_remote(&transport, &credentials)?,
                    ))
                });
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Clean unused spool objects"))
                .on_hover_text(
                    "Explicit local cleanup. Prepared, confirmed, uploading, retryable, and finalize-uncertain chunks remain protected.",
                )
                .clicked()
            {
                let settings = self.settings.clone();
                self.start(ui.ctx(), "Clean publication spool", move || {
                    let store = Self::store_for(&settings)?;
                    Ok(PublicationTaskOutput::Spool(store.collect_spool()?))
                });
            }
        });
        if !self.remote_records.is_empty() {
            egui::CollapsingHeader::new(format!("Origin records ({})", self.remote_records.len()))
                .id_salt("generation_publication_remote_records")
                .show(ui, |ui| {
                    for record in &self.remote_records {
                        ui.monospace(format!(
                            "{} | {} | {}",
                            record.generation_id,
                            match record.state {
                                RunGenerationOwnerRecordState::Published => "published",
                                RunGenerationOwnerRecordState::Tombstone => "tombstone",
                            },
                            &record.generation_sha256[..12]
                        ));
                    }
                });
        }
    }

    fn selected_job(&self) -> Option<&GenerationPublicationJob> {
        let id = self.selected_job_id.as_deref()?;
        self.jobs.iter().find(|job| job.job_id == id)
    }

    fn start_publish(&mut self, ctx: &egui::Context, job_id: String) {
        let settings = self.settings.clone();
        self.start(ctx, "Publish generation", move || {
            let (store, credentials, transport) = Self::network_for(&settings)?;
            Ok(PublicationTaskOutput::Job(Box::new(store.publish(
                &transport,
                &credentials,
                &job_id,
            )?)))
        });
    }

    fn start_reconcile(&mut self, ctx: &egui::Context, job_id: String) {
        let settings = self.settings.clone();
        self.start(ctx, "Reconcile publication", move || {
            let (store, credentials, transport) = Self::network_for(&settings)?;
            Ok(PublicationTaskOutput::Job(Box::new(store.reconcile(
                &transport,
                &credentials,
                &job_id,
            )?)))
        });
    }
}

fn owner_kind_label(kind: OwnerGenerationKind) -> &'static str {
    match kind {
        OwnerGenerationKind::PrivateWrf => "Private WRF",
        OwnerGenerationKind::PrivateArwen => "Private ArWen",
        OwnerGenerationKind::UserProvided => "Other owner-provided rw-store",
    }
}

fn job_status_label(status: &GenerationJobStatus) -> String {
    match status {
        GenerationJobStatus::Prepared => "prepared locally".to_owned(),
        GenerationJobStatus::Confirmed { .. } => "rights confirmed; not uploaded".to_owned(),
        GenerationJobStatus::OriginBeginUncertain { .. } => {
            "origin begin/reservation uncertain".to_owned()
        }
        GenerationJobStatus::Uploading {
            uploaded_chunks,
            total_chunks,
            ..
        } => format!("uploading {uploaded_chunks}/{total_chunks} chunks"),
        GenerationJobStatus::FinalizeUncertain { .. } => "finalize uncertain".to_owned(),
        GenerationJobStatus::Published { .. } => "published".to_owned(),
        GenerationJobStatus::Cancelled { .. } => "cancelled".to_owned(),
        GenerationJobStatus::Revoked { .. } => "revoked".to_owned(),
        GenerationJobStatus::Failed { retryable, .. } => if *retryable {
            "failed; retryable"
        } else {
            "failed closed"
        }
        .to_owned(),
    }
}

fn job_label(job: &GenerationPublicationJob) -> String {
    format!(
        "{}/{} | {} | {}",
        job.model,
        job.run,
        job_status_label(&job.status),
        &job.generation_sha256[..12]
    )
}

fn attribution_from_settings(
    settings: &settings::GenerationPublicationSettings,
) -> AttributionNotice {
    AttributionNotice {
        provider: settings.attribution_provider.trim().to_owned(),
        notice: settings.attribution_notice.trim().to_owned(),
        source_url: settings.attribution_source_url.trim().to_owned(),
        license: settings.attribution_license.trim().to_owned(),
        license_url: settings.attribution_license_url.trim().to_owned(),
        terms_url: settings.attribution_terms_url.trim().to_owned(),
        disclaimer: settings.attribution_disclaimer.trim().to_owned(),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid};
    use rw_community_protocol::{
        RUN_GENERATION_OWNER_CAPABILITIES_SCHEMA, RunGenerationAdvertisedLimits,
        RunGenerationOwnerQuota, RunGenerationOwnerUsage,
    };
    use rw_store::{HourIngestWriter, RwsSourceProvenance};

    use super::*;

    fn enabled_settings(spool_bytes: u64) -> GenerationPublicationSettings {
        GenerationPublicationSettings {
            schema: SETTINGS_SCHEMA.to_owned(),
            enabled: true,
            trusted_origin_id: DEFAULT_ORIGIN_ID.to_owned(),
            trusted_origin_url: "https://models.example.test/rusty".to_owned(),
            policy: GenerationPublicationPolicy {
                max_generation_bytes: 16 * 1024 * 1024,
                max_spool_bytes: spool_bytes,
                max_files: 64,
                max_chunks: 1_024,
                chunk_bytes: 1_024,
                max_manifest_bytes: 1024 * 1024,
                max_retention_seconds: 30 * 24 * 60 * 60,
            },
        }
    }

    fn attribution() -> AttributionNotice {
        AttributionNotice {
            provider: "owner".into(),
            notice: "Owner-provided WRF simulation.".into(),
            source_url: "https://example.test/source".into(),
            license: "Owner-authorized redistribution".into(),
            license_url: "https://example.test/license".into(),
            terms_url: "https://example.test/terms".into(),
            disclaimer: "Research output; no operational warranty.".into(),
        }
    }

    fn create_wrf_run(root: &Path, run: &str, provider: &str) {
        let shape = GridShape::new(2, 2).unwrap();
        let grid = LatLonGrid::new(
            shape,
            vec![35.0, 35.0, 36.0, 36.0],
            vec![-98.0, -97.0, -98.0, -97.0],
        )
        .unwrap();
        let mut writer = HourIngestWriter::begin_exact(
            root,
            "wrf",
            run,
            0,
            rw_store::RwsExactTime::new(0, 1_700_000_000),
            &grid,
            None,
            "generation-publication-test",
        )
        .unwrap();
        writer
            .set_source_provenance(vec![
                RwsSourceProvenance::new(
                    provider,
                    vec!["owner-processed".into()],
                    vec!["rw-store".into()],
                )
                .unwrap(),
            ])
            .unwrap();
        writer
            .add_field_2d(
                "temperature_2m",
                "K",
                serde_json::to_value(FieldSelector::surface(CanonicalField::Temperature)).unwrap(),
                &[290.0, 291.0, 292.0, 293.0],
            )
            .unwrap();
        writer.finish(1_700_000_010).unwrap();
        let metadata = crate::wrf_source::WrfRunSourceMetadata {
            producer: if provider == crate::wrf_source::PRIVATE_ARWEN_PROVIDER {
                "arwen".into()
            } else {
                "wrf".into()
            },
            producer_version: (provider == crate::wrf_source::PRIVATE_ARWEN_PROVIDER)
                .then(|| "test".into()),
            domain: Some(1),
            nx: 2,
            ny: 2,
            dx_m: Some(1_000.0),
            dy_m: Some(1_000.0),
        };
        crate::wrf_source::write_run_metadata(root, "wrf", run, metadata).unwrap();
    }

    fn prepare_request(
        store_root: &Path,
        run: &str,
        owner: &str,
        kind: OwnerGenerationKind,
    ) -> PrepareGenerationRequest {
        PrepareGenerationRequest {
            store_root: store_root.to_path_buf(),
            model: "wrf".into(),
            run: run.into(),
            owner_principal_sha256: owner.into(),
            kind,
            retention_seconds: 7 * 24 * 60 * 60,
            attributions: vec![attribution()],
            modification_notices: vec!["WRF simulation processed and re-encoded by owner.".into()],
            now_unix: 1_700_000_100,
        }
    }

    #[test]
    fn prepare_freezes_only_closed_inventory_and_state_has_no_source_path_or_token() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        fs::write(
            store
                .path()
                .join("wrf/local_20231114221320_d01/wrfout_d01_secret"),
            b"raw private file",
        )
        .unwrap();
        let settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let owner = "1".repeat(64);
        let job = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &owner,
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        assert_eq!(job.files.len(), 3);
        assert!(job.files.iter().any(|file| file.file_name == "run.json"));
        assert!(job.files.iter().any(|file| file.file_name == "grid.rwg"));
        assert!(job.files.iter().any(|file| file.file_name == "f000.rws"));
        assert!(
            job.files
                .iter()
                .all(|file| !file.file_name.contains("wrfout"))
        );
        let state = fs::read_to_string(publication.state_path(&job.job_id)).unwrap();
        assert!(!state.contains(store.path().to_string_lossy().as_ref()));
        assert!(!state.contains("secret-token-canary"));
        assert!(!state.contains("wrfout_d01_secret"));
        publication.verify_job_spool(&job).unwrap();
    }

    #[test]
    fn identical_bytes_get_random_owner_scoped_upload_ids_and_restart_reuses_each_id() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let settings = enabled_settings(64 * 1024 * 1024);
        let root = spool.path().join("publication");
        let publication = GenerationPublicationStore::open(root.clone(), &settings).unwrap();
        let first = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        let second = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"2".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        assert_eq!(first.generation_sha256, second.generation_sha256);
        assert_ne!(first.job_id, second.job_id);
        validate_job_id(&first.job_id).unwrap();
        let reopened = GenerationPublicationStore::open(root, &settings).unwrap();
        assert_eq!(
            reopened.load_job(&first.job_id).unwrap().job_id,
            first.job_id
        );
    }

    #[test]
    fn confirmations_are_all_required_and_public_provider_has_no_mapping() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let job = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        assert!(matches!(
            publication.confirm(
                &job.job_id,
                PublicationConfirmations {
                    owner_publication: true,
                    redistribution_rights: true,
                    operator_connection_metadata: false,
                },
                1_700_000_200,
            ),
            Err(PublicationError::ConfirmationRequired)
        ));
        let confirmed = publication
            .confirm(
                &job.job_id,
                PublicationConfirmations {
                    owner_publication: true,
                    redistribution_rights: true,
                    operator_connection_metadata: true,
                },
                1_700_000_200,
            )
            .unwrap();
        assert_eq!(
            confirmed
                .replication_manifest(&settings.policy.protocol_limits())
                .unwrap()
                .publication
                .data_origin,
            DataOrigin::PrivateWrf
        );
    }

    #[test]
    fn prepared_and_confirmed_jobs_discard_offline_but_active_cancel_requires_transport() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let prepared = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        let discarded = publication
            .discard_local(&prepared.job_id, 1_700_000_150)
            .unwrap();
        assert!(matches!(
            discarded.status,
            GenerationJobStatus::Cancelled { .. }
        ));

        let confirmed = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .and_then(|job| {
                publication.confirm(
                    &job.job_id,
                    PublicationConfirmations {
                        owner_publication: true,
                        redistribution_rights: true,
                        operator_connection_metadata: true,
                    },
                    1_700_000_200,
                )
            })
            .unwrap();
        let discarded = publication
            .discard_local(&confirmed.job_id, 1_700_000_250)
            .unwrap();
        assert!(matches!(
            discarded.status,
            GenerationJobStatus::Cancelled { .. }
        ));
        assert!(matches!(
            publication.discard_local(&discarded.job_id, 1_700_000_260),
            Err(PublicationError::InvalidState)
        ));
    }

    #[test]
    fn same_owner_origin_and_content_prepare_reuses_nonterminal_job() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let request = || {
            prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            )
        };
        let first = publication.prepare(request()).unwrap();
        let second = publication.prepare(request()).unwrap();
        assert_eq!(first.job_id, second.job_id);
        assert_eq!(publication.list_local_jobs().unwrap().len(), 1);
    }

    #[test]
    fn tampered_cas_fails_closed_and_gc_protects_active_jobs() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let job = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        let protected = job.files[0].chunks[0].object_sha256.clone();
        let report = publication.collect_spool().unwrap();
        assert!(report.protected_objects > 0);
        let path = publication
            .root
            .join("objects")
            .join(&protected[..2])
            .join(&protected);
        fs::write(path, b"tampered").unwrap();
        assert!(matches!(
            publication.verify_job_spool(&job),
            Err(PublicationError::SpoolTampered)
        ));
        assert!(matches!(
            publication.collect_spool(),
            Err(PublicationError::SpoolTampered)
        ));
    }

    #[test]
    fn current_spool_usage_is_part_of_prepare_admission() {
        let store = tempfile::tempdir().unwrap();
        let spool = tempfile::tempdir().unwrap();
        create_wrf_run(
            store.path(),
            "local_20231114221320_d01",
            crate::wrf_source::PRIVATE_WRF_PROVIDER,
        );
        let mut settings = enabled_settings(64 * 1024 * 1024);
        let publication =
            GenerationPublicationStore::open(spool.path().join("publication"), &settings).unwrap();
        let first = publication
            .prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"1".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            ))
            .unwrap();
        let used = publication.spool_usage_bytes().unwrap();
        settings.policy.max_spool_bytes = used + first.total_bytes * 2 + MAX_JOB_STATE_BYTES - 1;
        let constrained =
            GenerationPublicationStore::open(publication.root.clone(), &settings).unwrap();
        assert!(matches!(
            constrained.prepare(prepare_request(
                store.path(),
                "local_20231114221320_d01",
                &"2".repeat(64),
                OwnerGenerationKind::PrivateWrf,
            )),
            Err(PublicationError::Limit)
        ));
    }

    #[derive(Default)]
    struct MemoryVault {
        records: RefCell<BTreeMap<String, String>>,
    }

    impl PublicationCredentialBackend for MemoryVault {
        fn load(&self, account: &str) -> Result<Option<String>, ()> {
            Ok(self.records.borrow().get(account).cloned())
        }
        fn save(&self, account: &str, secret: &str) -> Result<(), ()> {
            self.records
                .borrow_mut()
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<bool, ()> {
            Ok(self.records.borrow_mut().remove(account).is_some())
        }
    }

    #[test]
    fn credentials_are_per_origin_and_debug_redacted() {
        let first_settings = enabled_settings(64 * 1024 * 1024).validate().unwrap();
        let mut other = enabled_settings(64 * 1024 * 1024);
        other.trusted_origin_url = "https://other.example.test".into();
        let other_settings = other.validate().unwrap();
        let credentials =
            GenerationOriginCredentials::new(&first_settings, "secret-token-canary").unwrap();
        assert!(!format!("{credentials:?}").contains("secret-token-canary"));
        let vault = MemoryVault::default();
        save_credentials_with(&vault, &first_settings, &credentials).unwrap();
        assert_eq!(
            load_credentials_with(&vault, &first_settings)
                .unwrap()
                .unwrap()
                .bearer_token(),
            "secret-token-canary"
        );
        assert!(
            load_credentials_with(&vault, &other_settings)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            save_credentials_with(&vault, &other_settings, &credentials),
            Err(PublicationError::WrongOrigin)
        ));
    }

    #[test]
    fn remote_capabilities_are_enforced_before_begin() {
        let job = GenerationPublicationJob {
            schema: JOB_SCHEMA.into(),
            job_id: format!("be-{}-{}", "a".repeat(16), "b".repeat(32)),
            origin_id: DEFAULT_ORIGIN_ID.into(),
            origin_binding_sha256: "c".repeat(64),
            model: "wrf".into(),
            run: "local_20231114221320_d01".into(),
            generation_sha256: format!("{}{}", "a".repeat(16), "d".repeat(48)),
            source_snapshot_id: "e".repeat(64),
            grid_hash: "f".repeat(64),
            owner_principal_sha256: "1".repeat(64),
            kind: OwnerGenerationKind::PrivateWrf,
            source_provenance: vec![SourceProvenance {
                provider: crate::wrf_source::PRIVATE_WRF_PROVIDER.into(),
                roles: vec!["owner-processed".into()],
                products: vec!["rw-store".into()],
            }],
            files: Vec::new(),
            total_bytes: 10,
            retention_seconds: 100,
            attributions: vec![attribution()],
            modification_notices: vec!["modified".into()],
            status: GenerationJobStatus::Confirmed {
                confirmed_unix: 1_700_000_000,
            },
            created_unix: 1_700_000_000,
            updated_unix: 1_700_000_000,
        };
        let capabilities = RunGenerationOwnerCapabilities {
            schema: RUN_GENERATION_OWNER_CAPABILITIES_SCHEMA.into(),
            owner_principal_sha256: job.owner_principal_sha256.clone(),
            accepting_uploads: false,
            limits: RunGenerationAdvertisedLimits {
                maximum_generation_bytes: 1_000,
                maximum_files: 64,
                maximum_chunks: 1_024,
                maximum_chunk_bytes: 1_024,
                maximum_manifest_bytes: 1024 * 1024,
                minimum_retention_seconds: 1,
                maximum_retention_seconds: 1_000,
                maximum_provenance_entries: 64,
                maximum_attributions: 64,
                upload_ttl_seconds: 300,
            },
            quota: RunGenerationOwnerQuota {
                maximum_storage_bytes: 10_000,
                maximum_generations: 10,
                maximum_concurrent_uploads: 1,
                maximum_monthly_upload_bytes: 10_000,
            },
            usage: RunGenerationOwnerUsage {
                active_uploads: 0,
                live_publications: 0,
                pending_retirements: 0,
                tombstones: 0,
                reserved_bytes: 0,
                published_bytes: 0,
                pending_retirement_bytes: 0,
                billing_utc_month: "2023-11".into(),
                monthly_accepted_upload_bytes: 0,
            },
        };
        assert!(matches!(
            enforce_remote_capabilities(&job, &capabilities),
            Err(PublicationError::Limit)
        ));
    }
}
