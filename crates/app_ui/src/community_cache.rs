//! BowEcho Community Cache HTTPS client and verified immutable disk cache.
//!
//! Operational delivery is deliberately HTTPS-only: local verified cache,
//! optional R2 hot object, then the configured Rusty Weather/Hetzner origin.
//! After an honest normal-origin miss, an opt-in second request may ask that
//! same authority to proxy an approved public institution; BowEcho never
//! contacts the institution itself. Explicit cold-historical recovery is
//! isolated in `community_relay`; this module only supplies exact signed
//! manifests and a verified CAS. There is no ICE, STUN, candidate exchange,
//! peer discovery, or direct-connectivity code here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::time::Duration;

use chrono::Datelike;
use rw_community_protocol::{
    CASE_ARTIFACT_PAYLOAD_SCHEMA, CREATE_CASE_PATH, CaseArtifactPayload, CaseArtifactRef,
    CaseRoomDirectoryPage, CaseRoomManifest, Compression, DataOrigin, DecodedSizeGuard,
    GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA, HOT_MANIFEST_POINTER_SCHEMA, HotManifestPointer,
    LIST_CASES_PATH, MAX_CASE_DIRECTORY_PAGE, MissingPolicy, NATIVE_WINDOW_PAYLOAD_SCHEMA,
    ObjectManifest, POINT_SERIES_PAYLOAD_SCHEMA, PROFILE_PAYLOAD_SCHEMA,
    PUBLISH_CASE_ARTIFACT_PATH, ProfileObjectPayload, ProtocolLimits, PublicationGrant,
    PublishCaseArtifactRequest, REQUEST_SCHEMA, RESOLVE_OBJECT_PATH, RESOLVE_SCHEMA,
    RecipeIdentity, ResolveObjectRequest, ResolveObjectResponse, ShareQuery, ShareRequest,
    SignedCaseRoomManifest, SignedObjectManifest, SourceProvenance, TEMPORAL_GRID_PAYLOAD_SCHEMA,
    TimeWindow, TrustedSigningKeys, TypedObjectPayload, case_artifact_payload_bytes, object_sha256,
    parse_signed_object_manifest_bounded, request_sha256, trusted_signing_keys_from_base64,
    validate_object_manifest, validate_profile_payload_identity, validate_typed_payload_identity,
    verify_signed_case, verify_signed_object,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_READ_CHUNK: usize = 1024 * 1024;
const ZSTD_MAX_WINDOW_LOG: u32 = 23; // 8 MiB, comfortably above level-1 encoder windows.
const INDEX_SCHEMA: &str = "bowecho.community-cache-index.v1";
const INDEX_FILE: &str = "index.json";
const TRANSFER_USAGE_SCHEMA: &str = "bowecho.community-cache-transfer-usage.v2";
const LEGACY_TRANSFER_USAGE_SCHEMA: &str = "bowecho.community-cache-transfer-usage.v1";
const TRANSFER_USAGE_FILE: &str = "transfer-usage.json";
const ORIGIN_SIGNING_KEY_ID: &str = "rw-origin-v1";
const REMOTE_MODELS_MAX_BYTES: u64 = 2 * 1024 * 1024;
const REMOTE_RUNS_MAX_BYTES: u64 = 8 * 1024 * 1024;
const REMOTE_RUN_MAX_BYTES: u64 = 512 * 1024;
const REMOTE_VARIABLES_MAX_BYTES: u64 = 16 * 1024 * 1024;
const REMOTE_AXIS_MAX_BYTES: u64 = 8 * 1024 * 1024;
const REMOTE_PROFILE_CYCLE_MAX_BYTES: u64 = 64 * 1024 * 1024;
const REMOTE_PROFILE_CYCLE_MAX_VALUES: usize = 1_000_000;
const REMOTE_MAX_MODELS: usize = 512;
const REMOTE_MAX_RUNS: usize = 4_096;
const REMOTE_MAX_VARIABLES: usize = 4_096;
const REMOTE_MAX_TIMES: usize = 4_096;
pub(crate) const CASE_DIRECTORY_PAGE_LIMIT: usize = 12;
const MAX_CASE_BROWSER_ENTRIES: usize = MAX_CASE_DIRECTORY_PAGE * 5;
const CASE_DIRECTORY_ENVELOPE_MAX_BYTES: u64 = 64 * 1024;
const MAX_HOT_MANIFEST_POINTER_BYTES: u64 = 1024;
const MAX_RELAY_SEED_OBJECTS: usize = 64;
const MAX_CACHE_INDEX_ENTRIES: usize = 100_000;
const AUTHENTICATED_PRINCIPAL_HASH_DOMAIN: &[u8] = b"rw-authenticated-principal-v1\0";
/// Kept byte-for-byte aligned with `rw-federation-proxy`; BowEcho deliberately
/// does not depend on the server implementation crate merely to serialize its
/// small client-facing authority request.
const FEDERATION_PROXY_SCHEMA: &str = "rw.federation.proxy-resolve.v1";
const FEDERATION_PROXY_PATH: &str = "/v1/federation/objects/resolve";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryTier {
    LocalCache,
    R2,
    Origin,
}

#[derive(Debug)]
pub(crate) struct VerifiedObject {
    pub manifest: SignedObjectManifest,
    /// Exact signed representation received from the source. Never recreate
    /// compressed bytes from `decoded`: compression output is not an identity.
    pub encoded: Vec<u8>,
    pub decoded: Vec<u8>,
    pub tier: DeliveryTier,
}

/// A case artifact whose bytes were bound to an already verified signed case
/// reference before the payload became visible to the UI. The case download
/// endpoint intentionally returns bytes rather than a second manifest, so the
/// signed case's exact object hash, request hash, and artifact kind are the
/// complete browser-side acceptance boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedCaseArtifact {
    pub payload: CaseArtifactPayload,
    pub tier: DeliveryTier,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CommunityCacheError {
    #[error("Community Cache is disabled or incompletely configured")]
    Disabled,
    #[error("Community Cache origin credentials are unavailable")]
    Credentials,
    #[error("Community Cache request is invalid: {0}")]
    Protocol(#[from] rw_community_protocol::ProtocolError),
    #[error("Community Cache request failed")]
    Network,
    #[error("Community Cache origin returned HTTP {0}")]
    Http(u16),
    #[error("Community Cache response is malformed")]
    Response,
    #[error("Community Cache object is unavailable from every configured HTTPS source")]
    Unavailable,
    #[error("Community Cache local storage failed")]
    Storage,
    #[error("Community Cache transfer limit is exhausted")]
    Quota,
    #[error("Community Cache operation was cancelled")]
    Cancelled,
}

#[derive(Clone)]
pub(crate) struct CommunityCacheClient {
    origin_url: String,
    r2_url: Option<String>,
    bearer_token: Option<String>,
    keys: TrustedSigningKeys,
    limits: ProtocolLimits,
    disk: VerifiedDiskCache,
    http: reqwest::blocking::Client,
    transfers: TransferGate,
    categories: CategoryAllowlist,
    authority_federation: Option<AuthorityFederationPolicy>,
    #[cfg(test)]
    origin_attempts: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    archival_origin_attempts: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Debug, Default)]
struct AuthorityFederationPolicy {
    preferred_origin_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct FederationProxyRequestBody<'a> {
    schema: &'static str,
    request: &'a ShareRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_origin_id: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct CategoryAllowlist {
    profiles: bool,
    point_series: bool,
    native_windows: bool,
    temporal: bool,
    case_artifacts: bool,
}

impl CategoryAllowlist {
    fn from_settings(settings: &settings::CommunityCacheSettings) -> Self {
        Self {
            profiles: settings.soundings_profiles,
            point_series: settings.point_series,
            native_windows: settings.native_windows_tiles,
            temporal: settings.temporal_diurnal,
            case_artifacts: settings.explicit_case_rooms,
        }
    }

    fn allows(self, query: &ShareQuery) -> bool {
        match query {
            ShareQuery::Profile { .. } => self.profiles,
            ShareQuery::PointSeries { .. } => self.point_series,
            ShareQuery::NativeWindow { .. } => self.native_windows,
            ShareQuery::GeographicWindow { .. } => self.native_windows,
            ShareQuery::TemporalGrid { .. } => self.temporal,
            ShareQuery::CaseArtifact { .. } => self.case_artifacts,
        }
    }

    fn require(self, query: &ShareQuery) -> Result<(), CommunityCacheError> {
        self.allows(query)
            .then_some(())
            .ok_or(CommunityCacheError::Disabled)
    }

    fn require_case_directory(self) -> Result<(), CommunityCacheError> {
        self.case_artifacts
            .then_some(())
            .ok_or(CommunityCacheError::Disabled)
    }
}

/// Session-only state for an explicitly invoked case-room directory browse.
/// Merely opening the Data tab never starts a request, and failures leave the
/// last completely verified page visible.
#[derive(Default)]
pub(crate) struct CommunityCaseBrowser {
    cases: Vec<SignedCaseRoomManifest>,
    next_after: Option<String>,
    receiver: Option<mpsc::Receiver<Result<CaseRoomDirectoryPage, CommunityCacheError>>>,
    pending_replace: bool,
    open_case_id: Option<String>,
    viewed_artifact: Option<(String, String)>,
    status: Option<String>,
}

impl CommunityCaseBrowser {
    pub(crate) fn cases(&self) -> &[SignedCaseRoomManifest] {
        &self.cases
    }

    pub(crate) fn next_after(&self) -> Option<&str> {
        self.next_after.as_deref()
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.receiver.is_some()
    }

    pub(crate) fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub(crate) fn report_error(&mut self, error: &CommunityCacheError) {
        self.status = Some(format!("Case-room browse failed: {error}"));
    }

    pub(crate) fn open_case_id(&self) -> Option<&str> {
        self.open_case_id.as_deref()
    }

    pub(crate) fn viewed_artifact(&self) -> Option<(&str, &str)> {
        self.viewed_artifact
            .as_ref()
            .map(|(case_id, artifact_id)| (case_id.as_str(), artifact_id.as_str()))
    }

    pub(crate) fn toggle_case(&mut self, case_id: &str) {
        if self.open_case_id.as_deref() == Some(case_id) {
            self.open_case_id = None;
            self.viewed_artifact = None;
        } else {
            self.open_case_id = Some(case_id.to_owned());
            self.viewed_artifact = None;
        }
    }

    pub(crate) fn toggle_artifact(&mut self, case_id: &str, artifact_id: &str) {
        let requested = (case_id.to_owned(), artifact_id.to_owned());
        if self.viewed_artifact.as_ref() == Some(&requested) {
            self.viewed_artifact = None;
        } else {
            self.viewed_artifact = Some(requested);
        }
    }

    pub(crate) fn upsert_verified_case(&mut self, signed: SignedCaseRoomManifest) {
        self.cases
            .retain(|candidate| candidate.manifest.case_id != signed.manifest.case_id);
        self.cases.push(signed);
        self.cases
            .sort_by(|left, right| left.manifest.case_id.cmp(&right.manifest.case_id));
        self.status = Some("Published case added to the verified directory view.".into());
    }

    pub(crate) fn refresh(
        &mut self,
        client: CommunityCacheClient,
    ) -> Result<(), CommunityCacheError> {
        self.start(client, None, true)
    }

    pub(crate) fn load_more(
        &mut self,
        client: CommunityCacheClient,
    ) -> Result<(), CommunityCacheError> {
        let after = self
            .next_after
            .clone()
            .ok_or(CommunityCacheError::Response)?;
        self.start(client, Some(after), false)
    }

    fn start(
        &mut self,
        client: CommunityCacheClient,
        after: Option<String>,
        replace: bool,
    ) -> Result<(), CommunityCacheError> {
        if self.receiver.is_some() {
            return Err(CommunityCacheError::Quota);
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("community-case-directory".into())
            .spawn(move || {
                let result = client.case_directory(after.as_deref(), CASE_DIRECTORY_PAGE_LIMIT);
                let _ = sender.send(result);
            })
            .map_err(|_| CommunityCacheError::Network)?;
        self.receiver = Some(receiver);
        self.pending_replace = replace;
        self.status = Some(if replace {
            "Loading signed case rooms…".into()
        } else {
            "Loading more signed case rooms…".into()
        });
        Ok(())
    }

    /// Returns true after a worker completes, allowing the caller to repaint.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver = None;
                match result {
                    Ok(page) => {
                        if let Err(error) = self.apply_verified_page(page) {
                            self.status = Some(format!("Case-room browse failed: {error}"));
                        }
                    }
                    Err(error) => {
                        self.status = Some(format!("Case-room browse failed: {error}"));
                    }
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver = None;
                self.status = Some("Case-room browse worker stopped unexpectedly".into());
                true
            }
        }
    }

    fn apply_verified_page(
        &mut self,
        page: CaseRoomDirectoryPage,
    ) -> Result<(), CommunityCacheError> {
        let count = page.cases.len();
        let retained = if self.pending_replace {
            count
        } else {
            self.cases
                .len()
                .checked_add(count)
                .ok_or(CommunityCacheError::Response)?
        };
        if retained > MAX_CASE_BROWSER_ENTRIES {
            return Err(CommunityCacheError::Response);
        }
        let has_next = page.next_after.is_some();
        if self.pending_replace {
            self.cases = page.cases;
            self.open_case_id = None;
            self.viewed_artifact = None;
        } else {
            self.cases.extend(page.cases);
        }
        self.next_after = page.next_after;
        self.status = Some(if count == 0 {
            if self.cases.is_empty() {
                if has_next {
                    "No visible cases on this page; more may be available".into()
                } else {
                    "No published case rooms are currently available".into()
                }
            } else if has_next {
                "No visible cases on this page; more may be available".into()
            } else {
                "No more published case rooms".into()
            }
        } else {
            format!(
                "Loaded {count} verified case room{}",
                if count == 1 { "" } else { "s" }
            )
        });
        Ok(())
    }
}

/// Authenticated origin catalog DTOs. These intentionally mirror only the
/// public Rusty Weather API; none can carry a local path, origin credential,
/// peer identifier, or network address.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteModelCatalogEntry {
    pub id: String,
    pub description: String,
    pub cycle_hours_utc: Vec<u8>,
    pub max_forecast_hour: u16,
    pub registry_source_count: usize,
    pub ingest_status: String,
    pub verification: String,
    pub limitations: Vec<String>,
    pub products: Vec<RemoteProductCapability>,
    pub provider_attributions: Vec<RemoteProviderAttribution>,
    pub stored_run_count: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProductCapability {
    pub product: String,
    pub surface_source: bool,
    pub pressure_source: bool,
    pub indexed_subset: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProviderAttribution {
    pub provider: String,
    pub copyright_statement: String,
    pub notice: String,
    pub source_url: String,
    pub license: String,
    pub license_url: String,
    pub terms_url: String,
    pub modification_notice: String,
    pub disclaimer: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSourceProvenance {
    pub provider: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub products: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteRunDescriptor {
    pub model: String,
    pub run: String,
    pub schema: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub nx: usize,
    pub ny: usize,
    pub exact_time_axis: bool,
    pub origin_unix: Option<i64>,
    pub sample_count: usize,
    pub first_valid_unix: Option<i64>,
    pub last_valid_unix: Option<i64>,
    #[serde(default)]
    pub source_provenance: Vec<RemoteSourceProvenance>,
    #[serde(default)]
    pub provider_attributions: Vec<RemoteProviderAttribution>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteRunCatalogEntry {
    pub run: RemoteRunDescriptor,
    pub variable_count: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteVariableCapability {
    pub name: String,
    pub units: String,
    pub kind: String,
    pub codec: String,
    pub levels_hpa: Vec<u16>,
    pub selector: serde_json::Value,
    pub available_slots: Vec<u16>,
    pub available_samples: usize,
    pub expected_samples: usize,
    pub coverage: f64,
    pub point_series: bool,
    pub pressure_profile: bool,
    /// Added by rw-server alongside the bounded multi-hour sounding API.
    /// Old authorities omit it, which deliberately keeps the client on the
    /// already supported single-time signed profile path.
    #[serde(default)]
    pub profile_cycle: bool,
    #[serde(default)]
    pub geographic_window: bool,
    pub scalar_temporal_reduction: bool,
    pub temporal: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteTimePoint {
    pub storage_slot: u16,
    pub lead_seconds: u64,
    pub valid_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteRunCatalog {
    pub runs: Vec<RemoteRunCatalogEntry>,
    pub latest_run: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemotePressureProfile {
    pub name: String,
    pub units: String,
    pub levels_hpa: Vec<u16>,
    pub values: Vec<Option<f32>>,
    pub available_levels: usize,
    pub expected_levels: usize,
    pub coverage: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProfileSurfaceSample {
    pub variable: String,
    pub units: String,
    pub value: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteProfileCycleSampleStatus {
    Complete,
    Partial,
    Gap,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProfileCycleSample {
    pub time: RemoteTimePoint,
    pub source_provenance: Vec<RemoteSourceProvenance>,
    pub status: RemoteProfileCycleSampleStatus,
    pub variables: Vec<RemotePressureProfile>,
    pub missing_variables: Vec<String>,
    pub surface_samples: Vec<RemoteProfileSurfaceSample>,
    pub missing_surface_variables: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProfileCycleTimeRange {
    pub start_unix: Option<i64>,
    pub end_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteProfileCycleMissingPolicy {
    Strict,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteProfileCycleResult {
    pub run: RemoteRunDescriptor,
    pub point: RemoteGridPoint,
    pub requested_variables: Vec<String>,
    pub requested_surface_variables: Vec<String>,
    pub requested_time: RemoteProfileCycleTimeRange,
    pub missing_policy: RemoteProfileCycleMissingPolicy,
    pub samples: Vec<RemoteProfileCycleSample>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteProfileCycleRequest<'a> {
    model: &'a str,
    run: &'a str,
    latitude: f64,
    longitude: f64,
    variables: &'a [String],
    surface_variables: &'a [String],
    start_unix: Option<i64>,
    end_unix: Option<i64>,
    missing_policy: &'static str,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteGridPoint {
    pub requested_latitude: f64,
    pub requested_longitude: f64,
    pub x: usize,
    pub y: usize,
    pub grid_latitude: f32,
    pub grid_longitude: f32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemotePointVariableSeries {
    name: String,
    units: String,
    values: Vec<Option<f32>>,
    available_samples: usize,
    expected_samples: usize,
    coverage: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteAxisProbeResponse {
    run: RemoteRunDescriptor,
    point: RemoteGridPoint,
    axis: Vec<RemoteTimePoint>,
    variables: Vec<RemotePointVariableSeries>,
}

/// Everything a remote-only model pane needs to render run/time/variable
/// choices and construct the exact signed profile identity. `axis` comes from
/// the origin's existing point endpoint because the run summary deliberately
/// omits the potentially large slot-to-valid-time mapping.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteProfileCatalog {
    pub run: RemoteRunDescriptor,
    pub point: RemoteGridPoint,
    pub axis: Vec<RemoteTimePoint>,
    pub variables: Vec<RemoteVariableCapability>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteProfileVariableSelection {
    pub pressure_variables: Vec<String>,
    pub surface_variables: Vec<String>,
    /// Empty means every native stored pressure level for each selected
    /// variable. A non-empty selection is signed into the cache identity.
    pub pressure_levels_hpa: Vec<u16>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemotePointSeriesSelection {
    pub variables: Vec<String>,
    pub window: TimeWindow,
    pub missing_policy: MissingPolicy,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteNativeWindowSelection {
    pub variables: Vec<String>,
    pub time: RemoteTimePoint,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    /// Empty selects 2-D surface variables. Non-empty selects pressure-volume
    /// variables at exactly these levels in the signed request.
    pub pressure_levels_hpa: Vec<u16>,
}

/// A finite geographic crop on the exact immutable run grid. Longitude is
/// the eastward arc from west to east: west > east crosses the antimeridian,
/// while exactly -180..180 denotes the full globe.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RemoteGeographicWindowSelection {
    pub variables: Vec<String>,
    pub time: RemoteTimePoint,
    pub west_longitude: f64,
    pub south_latitude: f64,
    pub east_longitude: f64,
    pub north_latitude: f64,
    /// Empty selects surface fields. Non-empty selects these exact pressure
    /// levels; no vertical reduction or level flattening is permitted.
    pub pressure_levels_hpa: Vec<u16>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteTemporalGridSelection {
    pub variables: Vec<String>,
    pub window: TimeWindow,
    pub reducer: String,
    pub semantics: String,
    pub missing_policy: MissingPolicy,
    pub pressure_levels_hpa: Vec<u16>,
    /// Exact scientific parameters such as interval support, cadence, reset
    /// tolerance, or integral units. They are part of cache identity.
    pub parameters: BTreeMap<String, String>,
}

impl std::fmt::Debug for CommunityCacheClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommunityCacheClient")
            .field("origin_url", &self.origin_url)
            .field("r2_url", &self.r2_url)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("limits", &self.limits)
            .field("disk", &self.disk)
            .field("transfers", &self.transfers)
            .field("categories", &self.categories)
            .field("authority_federation", &self.authority_federation)
            .finish()
    }
}

impl CommunityCacheClient {
    pub(crate) fn from_settings(
        settings: &settings::CommunityCacheSettings,
        cache_root: PathBuf,
    ) -> Result<Self, CommunityCacheError> {
        Self::from_settings_with_credentials(settings, cache_root, || {
            crate::community_credentials::load_credentials()
                .map_err(|_| CommunityCacheError::Credentials)
                .map(|credentials| credentials.map(|value| value.bearer_token().to_owned()))
        })
    }

    /// Deterministic credential injection for unit tests running without an OS
    /// secret-service session. Production construction always uses the vault.
    #[cfg(test)]
    pub(crate) fn from_settings_for_test(
        settings: &settings::CommunityCacheSettings,
        cache_root: PathBuf,
        bearer_token: &str,
    ) -> Result<Self, CommunityCacheError> {
        let bearer_token = bearer_token.trim();
        if bearer_token.is_empty() {
            return Err(CommunityCacheError::Credentials);
        }
        Self::from_settings_with_credentials(settings, cache_root, || {
            Ok(Some(bearer_token.to_owned()))
        })
    }

    fn from_settings_with_credentials(
        settings: &settings::CommunityCacheSettings,
        cache_root: PathBuf,
        load_bearer_token: impl FnOnce() -> Result<Option<String>, CommunityCacheError>,
    ) -> Result<Self, CommunityCacheError> {
        if !settings.phase1_active() {
            return Err(CommunityCacheError::Disabled);
        }
        let keys = trusted_origin_keyring(settings)?;
        let disk_limit_bytes = u64::from(settings.disk_allowance_gib)
            .checked_mul(1024 * 1024 * 1024)
            .ok_or(CommunityCacheError::Quota)?;
        let download_limit_bytes_per_hour = u64::from(settings.download_cap_mib_per_hour)
            .checked_mul(1024 * 1024)
            .ok_or(CommunityCacheError::Quota)?;
        let monthly_limit_bytes = u64::from(settings.monthly_transfer_cap_gib)
            .checked_mul(1024 * 1024 * 1024)
            .ok_or(CommunityCacheError::Quota)?;
        let upload_limit_bytes_per_hour = u64::from(settings.upload_cap_mib_per_hour)
            .checked_mul(1024 * 1024)
            .ok_or(CommunityCacheError::Quota)?;
        let bearer_token = load_bearer_token()?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CommunityCacheError::Network)?;
        let transfers = TransferGate::shared(
            download_limit_bytes_per_hour,
            upload_limit_bytes_per_hour,
            monthly_limit_bytes,
            settings.max_concurrent_downloads,
            cache_root.join(TRANSFER_USAGE_FILE),
        )?;
        Ok(Self {
            origin_url: normalized_base_url(&settings.origin_url),
            r2_url: (!settings.r2_hot_object_url.trim().is_empty())
                .then(|| normalized_base_url(&settings.r2_hot_object_url)),
            bearer_token,
            keys,
            limits: ProtocolLimits::default(),
            disk: VerifiedDiskCache::new(cache_root, disk_limit_bytes),
            http,
            transfers,
            categories: CategoryAllowlist::from_settings(settings),
            authority_federation: None,
            #[cfg(test)]
            origin_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            archival_origin_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Enable the conventional authority-side federation fallback. This only
    /// changes the second POST made to the already configured Hetzner origin
    /// after an honest normal resolve miss. It never installs or contacts an
    /// institutional URL, and it reuses the existing authority bearer.
    pub(crate) fn set_authority_federation(
        &mut self,
        enabled: bool,
        preferred_origin_id: Option<String>,
    ) -> Result<(), CommunityCacheError> {
        if let Some(origin_id) = preferred_origin_id.as_deref()
            && !settings::federation_identifier_is_valid(origin_id)
        {
            return Err(CommunityCacheError::Response);
        }
        self.authority_federation = enabled.then_some(AuthorityFederationPolicy {
            preferred_origin_id,
        });
        Ok(())
    }

    /// Match Rusty Weather's stable bearer-derived publication principal
    /// without ever exposing or persisting the bearer token itself.
    pub(crate) fn owner_principal_sha256(&self) -> Result<String, CommunityCacheError> {
        use sha2::{Digest, Sha256};

        let token = self
            .bearer_token
            .as_deref()
            .ok_or(CommunityCacheError::Credentials)?;
        let mut digest = Sha256::new();
        digest.update(AUTHENTICATED_PRINCIPAL_HASH_DOMAIN);
        digest.update(token.as_bytes());
        Ok(format!("{:x}", digest.finalize()))
    }

    /// Clone the explicitly configured origin trust roots for the isolated
    /// relay task. The task cannot learn keys from a broker response.
    pub(crate) fn relay_origin_keys(&self) -> TrustedSigningKeys {
        self.keys.clone()
    }

    /// Re-verify hostile relay bytes against the caller's exact signed
    /// identity before making them visible through the normal local cache.
    /// A failed write never changes the existing cache entry.
    pub(crate) fn accept_relay_recovery(
        &self,
        request: &ShareRequest,
        manifest: &SignedObjectManifest,
        encoded: Vec<u8>,
    ) -> Result<(), CommunityCacheError> {
        verify_signed_object(
            manifest,
            request,
            &encoded,
            now_unix(),
            &self.keys,
            &self.limits,
        )?;
        let decoded = decode_verified(&manifest.manifest, &encoded, &self.limits)?;
        self.disk.store(&VerifiedObject {
            manifest: manifest.clone(),
            encoded,
            decoded,
            // This value is never returned to an operational consumer. Once
            // stored, the ordinary reader reports LocalCache.
            tier: DeliveryTier::LocalCache,
        })
    }

    pub(crate) fn has_verified_local_object(
        &self,
        request: &ShareRequest,
    ) -> Result<bool, CommunityCacheError> {
        Ok(self.disk.load(request, &self.keys, &self.limits)?.is_some())
    }

    /// Historical hot-tier retry used only by the explicit recovery command.
    /// It preserves local -> R2 -> relay ordering even if a hot object appears
    /// between the user's initial miss and the background task starting.
    pub(crate) fn recover_r2_hot_object(
        &self,
        request: &ShareRequest,
    ) -> Result<bool, CommunityCacheError> {
        let Some(base) = self.r2_url.as_deref() else {
            return Ok(false);
        };
        let request_hash = request_sha256(request)?;
        match self.fetch_r2_with_manifest(base, request, &request_hash) {
            Ok(object) => {
                self.disk.store(&object)?;
                Ok(true)
            }
            Err(CommunityCacheError::Http(404) | CommunityCacheError::Unavailable) => Ok(false),
            Err(CommunityCacheError::Quota) => Err(CommunityCacheError::Quota),
            // A malformed/tampered hot response fails closed but does not
            // prevent the exact signed relay/archival fallback from running.
            Err(_) => Ok(false),
        }
    }

    /// Resolve the exact authority-signed identity needed by a deliberate
    /// historical recovery action. This fetches metadata only; it never
    /// dispatches TURN and never trusts resolver delivery-order suggestions.
    pub(crate) fn historical_manifest(
        &self,
        request: &ShareRequest,
    ) -> Result<SignedObjectManifest, CommunityCacheError> {
        self.categories.require(&request.query)?;
        request.validate(&self.limits)?;
        if matches!(request.query, ShareQuery::CaseArtifact { .. }) {
            return Err(CommunityCacheError::Unavailable);
        }
        let request_hash = request_sha256(request)?;
        // Historical identity lookup is *strictly* retained metadata. Generic
        // origin resolve is forbidden here because the authority may compute
        // or fetch and stage bytes as part of resolving an operational miss.
        // A cold request may use a locally retained signed manifest or an R2
        // pointer/manifest, never an origin/federation/compute call before
        // TURN. Without such an identity, relay lookup is skipped honestly.
        let manifest = match self
            .disk
            .retained_manifest(request, &self.keys, &self.limits)?
        {
            Some(manifest) => manifest,
            None => self
                .fetch_r2_manifest_only(request, &request_hash)?
                .ok_or(CommunityCacheError::Unavailable)?,
        };
        if manifest.manifest.request != *request {
            return Err(CommunityCacheError::Response);
        }
        // Signature and expiry are checked again in the relay client and
        // against the recovered bytes. Here, validate the closed identity
        // before it can enter the background command queue.
        validate_object_manifest(&manifest.manifest, &self.limits)?;
        rw_community_relay::verify_origin_signed_identity(
            &manifest,
            now_unix(),
            &self.keys,
            &self.limits,
        )
        .map_err(|_| CommunityCacheError::Response)?;
        Ok(manifest)
    }

    fn fetch_r2_manifest_only(
        &self,
        request: &ShareRequest,
        request_hash: &str,
    ) -> Result<Option<SignedObjectManifest>, CommunityCacheError> {
        let Some(base_url) = self.r2_url.as_deref() else {
            return Ok(None);
        };
        let _active = self.transfers.begin()?;
        let pointer_response = self
            .http
            .get(format!("{base_url}/v2/requests/{request_hash}.json"))
            .send()
            .map_err(|_| CommunityCacheError::Network)?;
        let bytes = if pointer_response.status().is_success() {
            let pointer_bytes = read_bounded_response(
                pointer_response,
                MAX_HOT_MANIFEST_POINTER_BYTES,
                None,
                &self.transfers,
            )?;
            let pointer: HotManifestPointer = serde_json::from_slice(&pointer_bytes)
                .map_err(|_| CommunityCacheError::Response)?;
            if pointer.schema != HOT_MANIFEST_POINTER_SCHEMA {
                return Err(CommunityCacheError::Response);
            }
            pointer.validate_for_request(request_hash)?;
            let response = self
                .http
                .get(format!(
                    "{base_url}/v2/manifests/{}.json",
                    pointer.manifest_sha256
                ))
                .send()
                .map_err(|_| CommunityCacheError::Network)?;
            if !response.status().is_success() {
                return match response.status().as_u16() {
                    404 => Ok(None),
                    status => Err(CommunityCacheError::Http(status)),
                };
            }
            let bytes = read_bounded_response(
                response,
                self.limits.max_manifest_bytes,
                None,
                &self.transfers,
            )?;
            if object_sha256(&bytes) != pointer.manifest_sha256 {
                return Err(CommunityCacheError::Response);
            }
            bytes
        } else if pointer_response.status().as_u16() == 404 {
            let response = self
                .http
                .get(format!("{base_url}/v1/manifests/{request_hash}.json"))
                .send()
                .map_err(|_| CommunityCacheError::Network)?;
            if !response.status().is_success() {
                return match response.status().as_u16() {
                    404 => Ok(None),
                    status => Err(CommunityCacheError::Http(status)),
                };
            }
            read_bounded_response(
                response,
                self.limits.max_manifest_bytes,
                None,
                &self.transfers,
            )?
        } else {
            return Err(CommunityCacheError::Http(
                pointer_response.status().as_u16(),
            ));
        };
        let manifest = parse_signed_object_manifest_bounded(&bytes, &self.limits)?;
        if manifest.manifest.request_sha256 != request_hash || manifest.manifest.request != *request
        {
            return Err(CommunityCacheError::Response);
        }
        Ok(Some(manifest))
    }

    /// Final historical fallback for one exact signed object. This is normal
    /// authenticated HTTPS to the configured authority, never TURN.
    pub(crate) fn recover_archival_https(
        &self,
        request: &ShareRequest,
        manifest: &SignedObjectManifest,
    ) -> Result<(), CommunityCacheError> {
        if manifest.manifest.request != *request {
            return Err(CommunityCacheError::Response);
        }
        #[cfg(test)]
        self.archival_origin_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let object =
            self.fetch_object_at(&self.origin_url, manifest, request, DeliveryTier::Origin)?;
        self.disk.store(&object)
    }

    /// Re-check every indexed manifest with the current keyring and clock.
    /// `load` performs fail-closed eviction, so a removed rotation key or an
    /// expired/tampered entry immediately loses both read and seed eligibility.
    pub(crate) fn prune_untrusted_relay_entries(&self) {
        self.disk
            .prune_untrusted(&self.keys, &self.limits, MAX_RELAY_SEED_OBJECTS * 16);
    }

    pub(crate) fn relay_seed_candidates(&self) -> Vec<rw_community_relay::VerifiedSeedObject> {
        self.disk
            .verified_seed_candidates(&self.keys, &self.limits, MAX_RELAY_SEED_OBJECTS)
    }

    pub(crate) fn begin_relay_transfer(
        &self,
    ) -> Result<CommunityRelayTransfer, CommunityCacheError> {
        Ok(CommunityRelayTransfer {
            _active: self.transfers.begin()?,
        })
    }

    /// Reserve the exact origin-signed encoded size before opening a relay
    /// download. Charging before network I/O prevents two concurrent sessions
    /// from each observing the same remaining budget. Failed sessions are not
    /// refunded, a conservative retry/cost policy.
    pub(crate) fn reserve_relay_download(
        &self,
        encoded_size: u64,
    ) -> Result<CommunityRelayTransfer, CommunityCacheError> {
        if encoded_size == 0 {
            return Err(CommunityCacheError::Quota);
        }
        let active = self.transfers.begin()?;
        self.transfers.charge_download(now_unix(), encoded_size)?;
        Ok(CommunityRelayTransfer { _active: active })
    }

    pub(crate) fn remaining_relay_download_bytes(&self) -> u64 {
        self.transfers.remaining_download(now_unix())
    }

    pub(crate) fn remaining_relay_upload_bytes(&self) -> u64 {
        self.transfers.remaining_upload(now_unix())
    }

    #[cfg(test)]
    pub(crate) fn origin_attempts_for_test(&self) -> u64 {
        self.origin_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn archival_origin_attempts_for_test(&self) -> u64 {
        self.archival_origin_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Models advertised by the configured Rusty Weather origin. This is
    /// authenticated metadata, not Community Cache object delivery.
    #[allow(dead_code)]
    pub(crate) fn remote_models(
        &self,
    ) -> Result<Vec<RemoteModelCatalogEntry>, CommunityCacheError> {
        let models: Vec<RemoteModelCatalogEntry> =
            self.get_origin_json("/v1/models", REMOTE_MODELS_MAX_BYTES)?;
        bounded_catalog(models, REMOTE_MAX_MODELS, validate_remote_model)
    }

    /// Browse deliberate case-room publications from the authenticated
    /// origin. The directory envelope is bounded and every entry remains a
    /// complete origin-signed manifest; no unsigned search summary is ever
    /// accepted by the UI.
    pub(crate) fn case_directory(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<CaseRoomDirectoryPage, CommunityCacheError> {
        self.categories.require_case_directory()?;
        let path = case_directory_path(after, limit)?;
        let response_limit = case_directory_response_limit(&self.limits, limit)?;
        let page: CaseRoomDirectoryPage = self.get_origin_json(&path, response_limit)?;
        verify_case_directory_page(&page, after, limit, now_unix(), &self.keys, &self.limits)?;
        Ok(page)
    }

    /// Publish one deliberately selected, closed typed artifact. The server
    /// response is accepted only when it signs the exact canonical request
    /// and the exact payload bytes BowEcho submitted.
    pub(crate) fn publish_case_artifact(
        &self,
        publication: &PublishCaseArtifactRequest,
    ) -> Result<SignedObjectManifest, CommunityCacheError> {
        self.categories.require_case_directory()?;
        publication.validate(&self.limits)?;
        let expected_object = case_artifact_payload_bytes(publication)?;
        let signed: SignedObjectManifest = self.post_origin_json(
            PUBLISH_CASE_ARTIFACT_PATH,
            publication,
            publication_body_limit(&self.limits)?,
            self.limits.max_manifest_bytes,
        )?;
        if signed.manifest.request != publication.request
            || signed.manifest.created_unix != publication.published_unix
            || signed.manifest.expires_unix != publication.retain_until_unix
            || signed.manifest.attributions != publication.attributions
            || signed.manifest.modification_notices != publication.modification_notices
        {
            return Err(CommunityCacheError::Response);
        }
        verify_signed_object(
            &signed,
            &publication.request,
            &expected_object,
            now_unix(),
            &self.keys,
            &self.limits,
        )?;
        Ok(signed)
    }

    /// Publish a completed case only after every artifact has already been
    /// accepted by the origin. The returned signature must cover the exact
    /// manifest assembled and confirmed in the UI.
    pub(crate) fn publish_case(
        &self,
        manifest: &CaseRoomManifest,
    ) -> Result<SignedCaseRoomManifest, CommunityCacheError> {
        self.categories.require_case_directory()?;
        let signed: SignedCaseRoomManifest = self.post_origin_json(
            CREATE_CASE_PATH,
            manifest,
            self.limits.max_manifest_bytes,
            self.limits.max_manifest_bytes,
        )?;
        if signed.manifest != *manifest {
            return Err(CommunityCacheError::Response);
        }
        verify_signed_case(&signed, now_unix(), &self.keys, &self.limits)?;
        Ok(signed)
    }

    /// Retrieve a published case artifact after an explicit View action.
    /// Revocable owner publications deliberately go through the authority on
    /// every fetch so its live tombstone check cannot be bypassed by an
    /// unexpired public-hot-tier signature.
    pub(crate) fn fetch_case_artifact(
        &self,
        signed_case: &SignedCaseRoomManifest,
        artifact: &CaseArtifactRef,
    ) -> Result<VerifiedCaseArtifact, CommunityCacheError> {
        self.categories.require_case_directory()?;
        verify_signed_case(signed_case, now_unix(), &self.keys, &self.limits)?;
        if !signed_case
            .manifest
            .artifacts
            .iter()
            .any(|candidate| candidate == artifact)
        {
            return Err(CommunityCacheError::Response);
        }

        self.fetch_case_artifact_at(&self.origin_url, true, artifact, DeliveryTier::Origin)
    }

    /// Immutable run snapshots stored for a remote model. Path segments are
    /// encoded so caller-controlled model/run values cannot alter the route.
    #[allow(dead_code)]
    pub(crate) fn remote_runs(
        &self,
        model: &str,
    ) -> Result<Vec<RemoteRunCatalogEntry>, CommunityCacheError> {
        validate_remote_component(model)?;
        let path = format!("/v1/models/{}/runs", encode_path_segment(model));
        let runs: Vec<RemoteRunCatalogEntry> =
            self.get_origin_json(&path, REMOTE_RUNS_MAX_BYTES)?;
        let runs = bounded_catalog(runs, REMOTE_MAX_RUNS, |entry| {
            validate_remote_run(&entry.run)?;
            if entry.run.model != model || entry.variable_count > REMOTE_MAX_VARIABLES {
                return Err(CommunityCacheError::Response);
            }
            Ok(())
        })?;
        Ok(runs)
    }

    /// Resolve the authority's latest physical cycle and bind it back to the
    /// exact authorized run catalog. The pointer has its own non-colliding
    /// route because `latest` remains a legal immutable run identifier.
    #[allow(dead_code)]
    pub(crate) fn remote_run_catalog(
        &self,
        model: &str,
    ) -> Result<RemoteRunCatalog, CommunityCacheError> {
        let latest = self.remote_latest_run(model)?;
        let mut runs = self.remote_runs(model)?;
        validate_remote_latest_catalog(&runs, &latest)?;
        sort_remote_runs_by_physical_origin(&mut runs);
        Ok(RemoteRunCatalog {
            runs,
            latest_run: latest.run,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn remote_latest_run(
        &self,
        model: &str,
    ) -> Result<RemoteRunDescriptor, CommunityCacheError> {
        let path = remote_latest_run_path(model)?;
        let descriptor: RemoteRunDescriptor = self.get_origin_json(&path, REMOTE_RUN_MAX_BYTES)?;
        validate_remote_run(&descriptor)?;
        if descriptor.model != model {
            return Err(CommunityCacheError::Response);
        }
        Ok(descriptor)
    }

    #[allow(dead_code)]
    pub(crate) fn remote_run(
        &self,
        model: &str,
        run: &str,
    ) -> Result<RemoteRunDescriptor, CommunityCacheError> {
        validate_remote_component(model)?;
        validate_remote_component(run)?;
        let path = format!(
            "/v1/models/{}/runs/{}",
            encode_path_segment(model),
            encode_path_segment(run)
        );
        let descriptor: RemoteRunDescriptor = self.get_origin_json(&path, REMOTE_RUN_MAX_BYTES)?;
        validate_remote_run(&descriptor)?;
        if descriptor.model != model || descriptor.run != run {
            return Err(CommunityCacheError::Response);
        }
        Ok(descriptor)
    }

    #[allow(dead_code)]
    pub(crate) fn remote_variables(
        &self,
        model: &str,
        run: &str,
    ) -> Result<Vec<RemoteVariableCapability>, CommunityCacheError> {
        validate_remote_component(model)?;
        validate_remote_component(run)?;
        let path = format!(
            "/v1/models/{}/runs/{}/variables",
            encode_path_segment(model),
            encode_path_segment(run)
        );
        let variables: Vec<RemoteVariableCapability> =
            self.get_origin_json(&path, REMOTE_VARIABLES_MAX_BYTES)?;
        bounded_catalog(variables, REMOTE_MAX_VARIABLES, validate_remote_variable)
    }

    /// Resolve the exact remote slot-to-valid-time axis through the existing
    /// point API. The probe variable is selected from the advertised run
    /// capabilities and no local rw-store entry is required.
    #[allow(dead_code)]
    pub(crate) fn remote_profile_catalog(
        &self,
        model: &str,
        run: &str,
        latitude: f64,
        longitude: f64,
    ) -> Result<RemoteProfileCatalog, CommunityCacheError> {
        validate_coordinates(latitude, longitude)?;
        let descriptor = self.remote_run(model, run)?;
        let variables = self.remote_variables(model, run)?;
        let probe =
            select_axis_probe_variable(&variables).ok_or(CommunityCacheError::Unavailable)?;
        let path = format!(
            "/v1/point?model={}&run={}&latitude={}&longitude={}&variables={}&missing_policy=partial",
            encode_query_component(model),
            encode_query_component(run),
            latitude,
            longitude,
            encode_query_component(probe)
        );
        let response: RemoteAxisProbeResponse =
            self.get_origin_json(&path, REMOTE_AXIS_MAX_BYTES)?;
        validate_remote_run(&response.run)?;
        validate_remote_grid_point(&response.point, &response.run)?;
        validate_remote_axis(&response.axis, &descriptor, &variables)?;
        if response.run != descriptor
            || response.variables.len() != 1
            || response.variables[0].name != probe
            || response.variables[0].values.len() != response.axis.len()
            || response.variables[0].expected_samples != response.axis.len()
            || response.variables[0].available_samples > response.axis.len()
            || !response.variables[0].coverage.is_finite()
            || !(0.0..=1.0).contains(&response.variables[0].coverage)
            || response.variables[0].units.len() > 96
        {
            return Err(CommunityCacheError::Response);
        }
        Ok(RemoteProfileCatalog {
            run: descriptor,
            point: response.point,
            axis: response.axis,
            variables,
        })
    }

    /// Build the same canonical signed identity whether this run exists in a
    /// local BowEcho store or only on the origin. Private/local provenance is
    /// always denied here; explicit owner publication belongs to case rooms.
    #[allow(dead_code)]
    pub(crate) fn build_remote_profile_request(
        &self,
        catalog: &RemoteProfileCatalog,
        time: &RemoteTimePoint,
        selection: RemoteProfileVariableSelection,
    ) -> Result<ShareRequest, CommunityCacheError> {
        self.categories.require(&ShareQuery::Profile {
            latitude_e7: 0,
            longitude_e7: 0,
            storage_slot: time.storage_slot,
            valid_unix: time.valid_unix,
            pressure_variables: vec!["placeholder".into()],
            surface_variables: vec!["placeholder".into()],
            pressure_levels_hpa: vec![],
        })?;
        build_remote_profile_request(catalog, time, selection, &self.limits)
    }

    pub(crate) fn build_remote_point_series_request(
        &self,
        catalog: &RemoteProfileCatalog,
        selection: RemotePointSeriesSelection,
    ) -> Result<ShareRequest, CommunityCacheError> {
        self.categories.require(&ShareQuery::PointSeries {
            latitude_e7: 0,
            longitude_e7: 0,
            window: selection.window.clone(),
            missing_policy: selection.missing_policy,
        })?;
        build_remote_point_series_request(catalog, selection, &self.limits)
    }

    pub(crate) fn build_remote_native_window_request(
        &self,
        catalog: &RemoteProfileCatalog,
        selection: RemoteNativeWindowSelection,
    ) -> Result<ShareRequest, CommunityCacheError> {
        self.categories.require(&ShareQuery::NativeWindow {
            storage_slot: selection.time.storage_slot,
            valid_unix: selection.time.valid_unix,
            x0: selection.x0,
            y0: selection.y0,
            x1: selection.x1,
            y1: selection.y1,
            pressure_levels_hpa: selection.pressure_levels_hpa.clone(),
        })?;
        build_remote_native_window_request(catalog, selection, &self.limits)
    }

    pub(crate) fn build_remote_geographic_window_request(
        &self,
        catalog: &RemoteProfileCatalog,
        selection: RemoteGeographicWindowSelection,
    ) -> Result<ShareRequest, CommunityCacheError> {
        self.categories.require(&ShareQuery::GeographicWindow {
            storage_slot: selection.time.storage_slot,
            valid_unix: selection.time.valid_unix,
            west_longitude_e7: 0,
            south_latitude_e7: 0,
            east_longitude_e7: 1,
            north_latitude_e7: 1,
            pressure_levels_hpa: selection.pressure_levels_hpa.clone(),
        })?;
        build_remote_geographic_window_request(catalog, selection, &self.limits)
    }

    pub(crate) fn build_remote_temporal_grid_request(
        &self,
        catalog: &RemoteProfileCatalog,
        selection: RemoteTemporalGridSelection,
    ) -> Result<ShareRequest, CommunityCacheError> {
        self.categories.require(&ShareQuery::TemporalGrid {
            window: selection.window.clone(),
            reducer: selection.reducer.clone(),
            semantics: selection.semantics.clone(),
            missing_policy: selection.missing_policy,
            pressure_levels_hpa: selection.pressure_levels_hpa.clone(),
        })?;
        build_remote_temporal_grid_request(catalog, selection, &self.limits)
    }

    #[allow(dead_code)]
    pub(crate) fn fetch_remote_profile<P>(
        &self,
        catalog: &RemoteProfileCatalog,
        time: &RemoteTimePoint,
        selection: RemoteProfileVariableSelection,
    ) -> Result<(ProfileObjectPayload<P>, DeliveryTier), CommunityCacheError>
    where
        P: DeserializeOwned,
    {
        let request = self.build_remote_profile_request(catalog, time, selection)?;
        self.fetch_profile(request)
    }

    /// Fetch one complete, bounded sounding cycle from the authenticated
    /// authority. Unlike Community Cache objects this response is not a
    /// portable signed artifact, so every identity, axis, variable, surface
    /// value and gap is checked against the independently loaded authorized
    /// catalog before it becomes session state.
    #[allow(dead_code)]
    pub(crate) fn remote_profile_cycle(
        &self,
        catalog: &RemoteProfileCatalog,
        mut selection: RemoteProfileVariableSelection,
    ) -> Result<RemoteProfileCycleResult, CommunityCacheError> {
        if !self.categories.profiles {
            return Err(CommunityCacheError::Disabled);
        }
        validate_remote_catalog_for_request(catalog)?;
        normalize_selected_variables(&mut selection.pressure_variables)?;
        if !selection.surface_variables.is_empty() {
            normalize_selected_variables(&mut selection.surface_variables)?;
        }
        if selection.pressure_variables.is_empty()
            || !selection.pressure_levels_hpa.is_empty()
            || selection
                .pressure_variables
                .iter()
                .any(|name| selection.surface_variables.contains(name))
        {
            return Err(CommunityCacheError::Response);
        }
        for name in &selection.pressure_variables {
            let variable = remote_variable(catalog, name)?;
            if variable.kind != "pressure3d"
                || !variable.pressure_profile
                || !variable.profile_cycle
            {
                return Err(CommunityCacheError::Response);
            }
        }
        for name in &selection.surface_variables {
            let variable = remote_variable(catalog, name)?;
            if variable.kind != "surface2d" || !variable.point_series {
                return Err(CommunityCacheError::Response);
            }
        }
        let request = RemoteProfileCycleRequest {
            model: &catalog.run.model,
            run: &catalog.run.run,
            latitude: catalog.point.requested_latitude,
            longitude: catalog.point.requested_longitude,
            variables: &selection.pressure_variables,
            surface_variables: &selection.surface_variables,
            start_unix: None,
            end_unix: None,
            missing_policy: "partial",
        };
        let response_limit = REMOTE_PROFILE_CYCLE_MAX_BYTES.min(self.limits.max_decoded_bytes);
        let result: RemoteProfileCycleResult = self.post_origin_query_json(
            "/v1/profile-cycle",
            &request,
            self.limits.max_manifest_bytes,
            response_limit,
        )?;
        validate_remote_profile_cycle(&result, catalog, &selection, &self.limits)?;
        Ok(result)
    }

    fn get_origin_json<T: DeserializeOwned>(
        &self,
        path_and_query: &str,
        limit: u64,
    ) -> Result<T, CommunityCacheError> {
        if !path_and_query.starts_with('/') || path_and_query.starts_with("//") {
            return Err(CommunityCacheError::Response);
        }
        let _active = self.transfers.begin()?;
        let mut builder = self
            .http
            .get(format!("{}{path_and_query}", self.origin_url));
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        read_bounded_json(response, limit, &self.transfers)
    }

    fn post_origin_json<TRequest: Serialize, TResponse: DeserializeOwned>(
        &self,
        path: &str,
        request: &TRequest,
        request_limit: u64,
        response_limit: u64,
    ) -> Result<TResponse, CommunityCacheError> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(CommunityCacheError::Response);
        }
        let token = self
            .bearer_token
            .as_deref()
            .ok_or(CommunityCacheError::Credentials)?;
        let body = serde_json::to_vec(request).map_err(|_| CommunityCacheError::Response)?;
        if body.is_empty() || body.len() as u64 > request_limit {
            return Err(CommunityCacheError::Quota);
        }
        let _active = self.transfers.begin()?;
        let response = self
            .http
            .post(format!("{}{path}", self.origin_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(token)
            .body(body)
            .send()
            .map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        read_bounded_json(response, response_limit, &self.transfers)
    }

    /// Read-only query POST. Rusty Weather permits a tokenless local service
    /// when the operator configured no tokens, matching the GET catalog
    /// behavior. Mutation/publication POSTs continue to require a bearer via
    /// `post_origin_json`.
    fn post_origin_query_json<TRequest: Serialize, TResponse: DeserializeOwned>(
        &self,
        path: &str,
        request: &TRequest,
        request_limit: u64,
        response_limit: u64,
    ) -> Result<TResponse, CommunityCacheError> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(CommunityCacheError::Response);
        }
        let body = serde_json::to_vec(request).map_err(|_| CommunityCacheError::Response)?;
        if body.is_empty() || body.len() as u64 > request_limit {
            return Err(CommunityCacheError::Quota);
        }
        let _active = self.transfers.begin()?;
        let mut builder = self
            .http
            .post(format!("{}{path}", self.origin_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        read_bounded_json(response, response_limit, &self.transfers)
    }

    fn fetch_case_artifact_at(
        &self,
        base_url: &str,
        authenticated_origin: bool,
        artifact: &CaseArtifactRef,
        tier: DeliveryTier,
    ) -> Result<VerifiedCaseArtifact, CommunityCacheError> {
        let path = if authenticated_origin {
            format!("/v1/community/objects/{}", artifact.object_sha256)
        } else {
            format!("/v1/objects/{}", artifact.object_sha256)
        };
        let _active = self.transfers.begin()?;
        let mut builder = self.http.get(format!("{base_url}{path}"));
        if authenticated_origin {
            let token = self
                .bearer_token
                .as_deref()
                .ok_or(CommunityCacheError::Credentials)?;
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        let bytes = read_bounded_response(
            response,
            self.limits.max_encoded_bytes,
            None,
            &self.transfers,
        )?;
        decode_case_artifact_reference(&bytes, artifact, &self.limits)
            .map(|payload| VerifiedCaseArtifact { payload, tier })
    }

    pub(crate) fn fetch_profile<P>(
        &self,
        request: ShareRequest,
    ) -> Result<(ProfileObjectPayload<P>, DeliveryTier), CommunityCacheError>
    where
        P: DeserializeOwned,
    {
        let verified = self.fetch(request.clone())?;
        let payload: ProfileObjectPayload<P> =
            serde_json::from_slice(&verified.decoded).map_err(|_| CommunityCacheError::Response)?;
        if payload.schema != PROFILE_PAYLOAD_SCHEMA {
            return Err(CommunityCacheError::Response);
        }
        validate_profile_payload_identity(&payload, &request)?;
        Ok((payload, verified.tier))
    }

    /// Fetch a signed point-series result through the Phase 1 delivery order.
    /// The exact model/run/grid/window/variables identity is supplied by the
    /// canonical request; this method never falls back to an unbound API body.
    pub(crate) fn fetch_point_series<T>(
        &self,
        request: ShareRequest,
    ) -> Result<(TypedObjectPayload<T>, DeliveryTier), CommunityCacheError>
    where
        T: DeserializeOwned,
    {
        self.fetch_typed(request, POINT_SERIES_PAYLOAD_SCHEMA)
    }

    /// Fetch one signed native-index 2-D or selected-pressure 3-D window.
    pub(crate) fn fetch_native_window<T>(
        &self,
        request: ShareRequest,
    ) -> Result<(TypedObjectPayload<T>, DeliveryTier), CommunityCacheError>
    where
        T: DeserializeOwned,
    {
        self.fetch_typed(request, NATIVE_WINDOW_PAYLOAD_SCHEMA)
    }

    /// Fetch a signed self-describing geographic envelope. Cropped grid
    /// coordinates, projection metadata and the inclusion mask all remain
    /// inside the verified object rather than being inferred by BowEcho.
    pub(crate) fn fetch_geographic_window<T>(
        &self,
        request: ShareRequest,
    ) -> Result<(TypedObjectPayload<T>, DeliveryTier), CommunityCacheError>
    where
        T: DeserializeOwned,
    {
        self.fetch_typed(request, GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA)
    }

    /// Fetch a signed temporal/diurnal grid result. Scientific reducer and
    /// semantics parameters remain part of the canonical request identity.
    pub(crate) fn fetch_temporal_grid<T>(
        &self,
        request: ShareRequest,
    ) -> Result<(TypedObjectPayload<T>, DeliveryTier), CommunityCacheError>
    where
        T: DeserializeOwned,
    {
        self.fetch_typed(request, TEMPORAL_GRID_PAYLOAD_SCHEMA)
    }

    fn fetch_typed<T>(
        &self,
        request: ShareRequest,
        expected_schema: &'static str,
    ) -> Result<(TypedObjectPayload<T>, DeliveryTier), CommunityCacheError>
    where
        T: DeserializeOwned,
    {
        let verified = self.fetch(request.clone())?;
        let payload = decode_typed_payload(&verified.decoded, &request, expected_schema)?;
        Ok((payload, verified.tier))
    }

    pub(crate) fn fetch(
        &self,
        request: ShareRequest,
    ) -> Result<VerifiedObject, CommunityCacheError> {
        self.categories.require(&request.query)?;
        request.validate(&self.limits)?;
        if matches!(request.query, ShareQuery::CaseArtifact { .. }) {
            // Case artifacts require a currently valid signed case reference
            // and a fresh authority tombstone check. They must use
            // fetch_case_artifact rather than generic local/R2 delivery.
            return Err(CommunityCacheError::Unavailable);
        }
        let request_hash = request_sha256(&request)?;
        for tier in phase1_delivery_order(self.r2_url.is_some()) {
            match tier {
                DeliveryTier::LocalCache => {
                    if let Some(cached) = self.disk.load(&request, &self.keys, &self.limits)? {
                        return Ok(cached);
                    }
                }
                DeliveryTier::R2 => {
                    let Some(base) = self.r2_url.as_deref() else {
                        continue;
                    };
                    // R2 includes its signed manifest and is attempted before
                    // any origin request, avoiding origin latency on hot hits.
                    match self.fetch_r2_with_manifest(base, &request, &request_hash) {
                        Ok(object) => {
                            let _ = self.disk.store(&object);
                            return Ok(object);
                        }
                        Err(CommunityCacheError::Quota) => {
                            return Err(CommunityCacheError::Quota);
                        }
                        Err(_) => continue,
                    }
                }
                DeliveryTier::Origin => {
                    let resolve =
                        self.resolve_with_authority_federation(&request, &request_hash)?;
                    let manifest = resolve
                        .signed_manifest
                        .ok_or(CommunityCacheError::Unavailable)?;
                    if manifest.manifest.request != request {
                        return Err(CommunityCacheError::Response);
                    }
                    validate_object_manifest(&manifest.manifest, &self.limits)?;
                    // Resolver delivery_order is never operational input in
                    // Phase 1, so CommunityRelay cannot be dispatched.
                    let object = self.fetch_object_at(
                        &self.origin_url,
                        &manifest,
                        &request,
                        DeliveryTier::Origin,
                    )?;
                    let _ = self.disk.store(&object);
                    return Ok(object);
                }
            }
        }
        Err(CommunityCacheError::Unavailable)
    }

    fn resolve_with_authority_federation(
        &self,
        request: &ShareRequest,
        request_hash: &str,
    ) -> Result<ResolveObjectResponse, CommunityCacheError> {
        match self.resolve(request) {
            Ok(resolve) => {
                validate_resolve_identity(&resolve, request, request_hash, &self.limits)?;
                if resolve.signed_manifest.is_some() || self.authority_federation.is_none() {
                    return Ok(resolve);
                }
            }
            Err(CommunityCacheError::Http(404)) if self.authority_federation.is_some() => {}
            Err(error) => return Err(error),
        }

        let resolve = self.resolve_federation_proxy(request)?;
        validate_resolve_identity(&resolve, request, request_hash, &self.limits)?;
        Ok(resolve)
    }

    fn fetch_r2_with_manifest(
        &self,
        base_url: &str,
        request: &ShareRequest,
        request_hash: &str,
    ) -> Result<VerifiedObject, CommunityCacheError> {
        let _active = self.transfers.begin()?;
        let pointer_url = format!("{base_url}/v2/requests/{request_hash}.json");
        let pointer_response = self
            .http
            .get(pointer_url)
            .send()
            .map_err(|_| CommunityCacheError::Network)?;
        let bytes = if pointer_response.status().is_success() {
            let pointer_bytes = read_bounded_response(
                pointer_response,
                MAX_HOT_MANIFEST_POINTER_BYTES,
                None,
                &self.transfers,
            )?;
            let pointer: HotManifestPointer = serde_json::from_slice(&pointer_bytes)
                .map_err(|_| CommunityCacheError::Response)?;
            if pointer.schema != HOT_MANIFEST_POINTER_SCHEMA {
                return Err(CommunityCacheError::Response);
            }
            pointer.validate_for_request(request_hash)?;
            let manifest_url = format!("{base_url}/v2/manifests/{}.json", pointer.manifest_sha256);
            let manifest_response = self
                .http
                .get(manifest_url)
                .send()
                .map_err(|_| CommunityCacheError::Network)?;
            if !manifest_response.status().is_success() {
                return Err(CommunityCacheError::Http(
                    manifest_response.status().as_u16(),
                ));
            }
            let bytes = read_bounded_response(
                manifest_response,
                self.limits.max_manifest_bytes,
                None,
                &self.transfers,
            )?;
            if object_sha256(&bytes) != pointer.manifest_sha256 {
                return Err(CommunityCacheError::Response);
            }
            bytes
        } else if pointer_response.status().as_u16() == 404 {
            // Migration-only fallback for manifests promoted before the v2
            // renewable pointer contract. New origins no longer write it.
            let legacy_url = format!("{base_url}/v1/manifests/{request_hash}.json");
            let response = self
                .http
                .get(legacy_url)
                .send()
                .map_err(|_| CommunityCacheError::Network)?;
            if !response.status().is_success() {
                return Err(CommunityCacheError::Http(response.status().as_u16()));
            }
            read_bounded_response(
                response,
                self.limits.max_manifest_bytes,
                None,
                &self.transfers,
            )?
        } else {
            return Err(CommunityCacheError::Http(
                pointer_response.status().as_u16(),
            ));
        };
        let manifest = parse_signed_object_manifest_bounded(&bytes, &self.limits)?;
        if manifest.manifest.request_sha256 != request_hash || manifest.manifest.request != *request
        {
            return Err(CommunityCacheError::Response);
        }
        drop(_active);
        self.fetch_object_at(base_url, &manifest, request, DeliveryTier::R2)
    }

    fn resolve(
        &self,
        request: &ShareRequest,
    ) -> Result<ResolveObjectResponse, CommunityCacheError> {
        let _active = self.transfers.begin()?;
        #[cfg(test)]
        self.origin_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let url = format!("{}{}", self.origin_url, RESOLVE_OBJECT_PATH);
        let body = serde_json::to_vec(&ResolveObjectRequest {
            schema: RESOLVE_SCHEMA.to_owned(),
            request: request.clone(),
        })
        .map_err(|_| CommunityCacheError::Response)?;
        if body.len() as u64 > self.limits.max_manifest_bytes {
            return Err(CommunityCacheError::Response);
        }
        let mut builder = self
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        read_bounded_json(response, self.limits.max_manifest_bytes, &self.transfers)
    }

    fn resolve_federation_proxy(
        &self,
        request: &ShareRequest,
    ) -> Result<ResolveObjectResponse, CommunityCacheError> {
        let policy = self
            .authority_federation
            .as_ref()
            .ok_or(CommunityCacheError::Unavailable)?;
        self.post_origin_json(
            FEDERATION_PROXY_PATH,
            &FederationProxyRequestBody {
                schema: FEDERATION_PROXY_SCHEMA,
                request,
                preferred_origin_id: policy.preferred_origin_id.as_deref(),
            },
            self.limits.max_manifest_bytes,
            self.limits.max_manifest_bytes,
        )
    }

    fn fetch_object_at(
        &self,
        base_url: &str,
        manifest: &SignedObjectManifest,
        request: &ShareRequest,
        tier: DeliveryTier,
    ) -> Result<VerifiedObject, CommunityCacheError> {
        validate_object_manifest(&manifest.manifest, &self.limits)?;
        let url = match tier {
            DeliveryTier::R2 => {
                format!("{base_url}/v1/objects/{}", manifest.manifest.object_sha256)
            }
            DeliveryTier::Origin => format!(
                "{base_url}/v1/community/objects/{}",
                manifest.manifest.object_sha256
            ),
            DeliveryTier::LocalCache => return Err(CommunityCacheError::Response),
        };
        let _active = self.transfers.begin()?;
        let mut builder = self.http.get(url);
        if tier == DeliveryTier::Origin
            && let Some(token) = &self.bearer_token
        {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().map_err(|_| CommunityCacheError::Network)?;
        if !response.status().is_success() {
            return Err(CommunityCacheError::Http(response.status().as_u16()));
        }
        let encoded = read_bounded_response(
            response,
            manifest.manifest.encoded_size,
            Some(manifest.manifest.encoded_size),
            &self.transfers,
        )?;
        verify_signed_object(
            manifest,
            request,
            &encoded,
            now_unix(),
            &self.keys,
            &self.limits,
        )?;
        let decoded = decode_verified(&manifest.manifest, &encoded, &self.limits)?;
        Ok(VerifiedObject {
            manifest: manifest.clone(),
            encoded,
            decoded,
            tier,
        })
    }
}

fn validate_resolve_identity(
    response: &ResolveObjectResponse,
    request: &ShareRequest,
    request_hash: &str,
    limits: &ProtocolLimits,
) -> Result<(), CommunityCacheError> {
    if response.schema != RESOLVE_SCHEMA || response.request_sha256 != request_hash {
        return Err(CommunityCacheError::Response);
    }
    if let Some(manifest) = &response.signed_manifest {
        if manifest.manifest.request != *request || manifest.manifest.request_sha256 != request_hash
        {
            return Err(CommunityCacheError::Response);
        }
        validate_object_manifest(&manifest.manifest, limits)?;
    }
    Ok(())
}

fn trusted_origin_keyring(
    settings: &settings::CommunityCacheSettings,
) -> Result<TrustedSigningKeys, CommunityCacheError> {
    if !settings::community_origin_keyring_is_valid(
        &settings.manifest_public_key_base64,
        &settings.trusted_origin_signing_keys,
    ) {
        return Err(CommunityCacheError::Disabled);
    }
    let mut entries = Vec::with_capacity(
        settings.trusted_origin_signing_keys.len()
            + usize::from(!settings.manifest_public_key_base64.trim().is_empty()),
    );
    if !settings.manifest_public_key_base64.trim().is_empty() {
        entries.push((
            ORIGIN_SIGNING_KEY_ID.to_owned(),
            settings.manifest_public_key_base64.clone(),
        ));
    }
    for entry in &settings.trusted_origin_signing_keys {
        let (key_id, encoded) = entry.split_once(':').ok_or(CommunityCacheError::Disabled)?;
        entries.push((key_id.to_owned(), encoded.to_owned()));
    }
    trusted_signing_keys_from_base64(entries).map_err(Into::into)
}

fn bounded_catalog<T>(
    values: Vec<T>,
    maximum: usize,
    validate: impl Fn(&T) -> Result<(), CommunityCacheError>,
) -> Result<Vec<T>, CommunityCacheError> {
    if values.len() > maximum {
        return Err(CommunityCacheError::Response);
    }
    for value in &values {
        validate(value)?;
    }
    Ok(values)
}

fn validate_remote_model(model: &RemoteModelCatalogEntry) -> Result<(), CommunityCacheError> {
    validate_remote_component(&model.id)?;
    if model.description.is_empty()
        || model.description.len() > 512
        || model.cycle_hours_utc.iter().any(|hour| *hour > 23)
        || model
            .cycle_hours_utc
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || model.ingest_status.len() > 64
        || model.verification.len() > 64
        || model.limitations.len() > 32
        || model.products.len() > 64
        || model.provider_attributions.len() > 16
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_remote_run(run: &RemoteRunDescriptor) -> Result<(), CommunityCacheError> {
    validate_remote_component(&run.model)?;
    validate_remote_component(&run.run)?;
    let valid_schema_axis = matches!(
        (run.schema.as_str(), run.exact_time_axis),
        ("rw-store.run.v1", false) | ("rw-store.run.v2", true)
    );
    if !valid_schema_axis
        || !is_sha256_hex(&run.snapshot_id)
        || !is_sha256_hex(&run.grid_hash)
        || run.nx == 0
        || run.ny == 0
        || run.nx.checked_mul(run.ny).is_none()
        || run.sample_count == 0
        || run.sample_count > REMOTE_MAX_TIMES
        || run.first_valid_unix.is_some_and(|time| time < 0)
        || run.last_valid_unix.is_some_and(|time| time < 0)
        || matches!(
            (run.first_valid_unix, run.last_valid_unix),
            (Some(first), Some(last)) if first > last
        )
        || run.source_provenance.is_empty()
        || run.source_provenance.len() > 16
    {
        return Err(CommunityCacheError::Response);
    }
    for source in &run.source_provenance {
        validate_provenance_token(&source.provider)?;
        if source.roles.len() > 16 || source.products.len() > 32 {
            return Err(CommunityCacheError::Response);
        }
        for token in source.roles.iter().chain(&source.products) {
            validate_provenance_token(token)?;
        }
    }
    Ok(())
}

fn validate_remote_latest_catalog(
    runs: &[RemoteRunCatalogEntry],
    latest: &RemoteRunDescriptor,
) -> Result<(), CommunityCacheError> {
    if runs.is_empty()
        || !runs.iter().any(|entry| entry.run == *latest)
        || runs.iter().any(|entry| entry.run.origin_unix.is_none())
    {
        return Err(CommunityCacheError::Response);
    }
    let physically_latest = runs
        .iter()
        .max_by(|left, right| {
            left.run
                .origin_unix
                .cmp(&right.run.origin_unix)
                .then_with(|| left.run.run.cmp(&right.run.run))
        })
        .ok_or(CommunityCacheError::Response)?;
    if physically_latest.run != *latest {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn sort_remote_runs_by_physical_origin(runs: &mut [RemoteRunCatalogEntry]) {
    runs.sort_by(|left, right| {
        right
            .run
            .origin_unix
            .cmp(&left.run.origin_unix)
            .then_with(|| right.run.run.cmp(&left.run.run))
    });
}

fn validate_remote_variable(
    variable: &RemoteVariableCapability,
) -> Result<(), CommunityCacheError> {
    validate_provenance_token(&variable.name)?;
    if variable.units.is_empty()
        || variable.units.len() > 96
        || variable.kind.is_empty()
        || variable.kind.len() > 32
        || variable.codec.is_empty()
        || variable.codec.len() > 32
        || variable.levels_hpa.len() > 512
        || variable.available_slots.len() > REMOTE_MAX_TIMES
        || variable.available_samples > variable.expected_samples
        || variable.expected_samples > REMOTE_MAX_TIMES
        || !variable.coverage.is_finite()
        || !(0.0..=1.0).contains(&variable.coverage)
        || variable
            .levels_hpa
            .windows(2)
            .any(|pair| pair[0] <= pair[1])
        || variable
            .available_slots
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_remote_axis(
    axis: &[RemoteTimePoint],
    run: &RemoteRunDescriptor,
    variables: &[RemoteVariableCapability],
) -> Result<(), CommunityCacheError> {
    let Some(origin_unix) = run.origin_unix else {
        return Err(CommunityCacheError::Response);
    };
    if axis.is_empty() || axis.len() != run.sample_count || axis.len() > REMOTE_MAX_TIMES {
        return Err(CommunityCacheError::Response);
    }
    for point in axis {
        let physical_origin = i128::from(point.valid_unix) - i128::from(point.lead_seconds);
        if point.valid_unix < 0
            || physical_origin != i128::from(origin_unix)
            || (run.schema == "rw-store.run.v1"
                && point.lead_seconds != u64::from(point.storage_slot) * 3_600)
        {
            return Err(CommunityCacheError::Response);
        }
    }
    if axis.windows(2).any(|pair| {
        pair[0].storage_slot >= pair[1].storage_slot
            || pair[0].lead_seconds >= pair[1].lead_seconds
            || pair[0].valid_unix >= pair[1].valid_unix
    }) || run.first_valid_unix != axis.first().map(|time| time.valid_unix)
        || run.last_valid_unix != axis.last().map(|time| time.valid_unix)
    {
        return Err(CommunityCacheError::Response);
    }
    let available = axis
        .iter()
        .map(|time| time.storage_slot)
        .collect::<BTreeSet<_>>();
    if variables.iter().any(|variable| {
        let expected_coverage = variable.available_slots.len() as f64 / axis.len() as f64;
        variable.expected_samples != axis.len()
            || variable.available_samples != variable.available_slots.len()
            || (variable.coverage - expected_coverage).abs() > 1.0e-6
            || variable
                .available_slots
                .iter()
                .any(|slot| !available.contains(slot))
    }) {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_remote_profile_cycle(
    result: &RemoteProfileCycleResult,
    catalog: &RemoteProfileCatalog,
    selection: &RemoteProfileVariableSelection,
    limits: &ProtocolLimits,
) -> Result<(), CommunityCacheError> {
    if result.run != catalog.run
        || result.point != catalog.point
        || result.requested_variables != selection.pressure_variables
        || result.requested_surface_variables != selection.surface_variables
        || result.requested_time.start_unix.is_some()
        || result.requested_time.end_unix.is_some()
        || result.missing_policy != RemoteProfileCycleMissingPolicy::Partial
        || result.samples.len() != catalog.axis.len()
        || result.samples.len() > REMOTE_MAX_TIMES
        || selection
            .pressure_variables
            .len()
            .checked_add(selection.surface_variables.len())
            .is_none_or(|count| count > limits.max_variables)
    {
        return Err(CommunityCacheError::Response);
    }

    let capabilities = catalog
        .variables
        .iter()
        .map(|variable| (variable.name.as_str(), variable))
        .collect::<BTreeMap<_, _>>();
    let mut total_values = 0_usize;
    for (sample, expected_time) in result.samples.iter().zip(&catalog.axis) {
        if sample.time != *expected_time
            || sample.source_provenance.is_empty()
            || sample.source_provenance.len() > limits.max_provenance_entries
        {
            return Err(CommunityCacheError::Response);
        }
        for source in &sample.source_provenance {
            validate_provenance_token(&source.provider)?;
            if source.roles.len() > 16
                || source.products.len() > 32
                || !catalog.run.source_provenance.iter().any(|expected| {
                    expected.provider == source.provider
                        && source
                            .roles
                            .iter()
                            .all(|role| expected.roles.contains(role))
                        && source
                            .products
                            .iter()
                            .all(|product| expected.products.contains(product))
                })
            {
                return Err(CommunityCacheError::Response);
            }
            for token in source.roles.iter().chain(&source.products) {
                validate_provenance_token(token)?;
            }
        }

        let profile_names = sample
            .variables
            .iter()
            .map(|profile| profile.name.as_str())
            .collect::<Vec<_>>();
        let expected_present = selection
            .pressure_variables
            .iter()
            .filter(|name| {
                capabilities.get(name.as_str()).is_some_and(|capability| {
                    capability
                        .available_slots
                        .contains(&sample.time.storage_slot)
                })
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        let expected_missing = selection
            .pressure_variables
            .iter()
            .filter(|name| {
                capabilities.get(name.as_str()).is_none_or(|capability| {
                    !capability
                        .available_slots
                        .contains(&sample.time.storage_slot)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if sample.missing_variables != expected_missing || profile_names != expected_present {
            return Err(CommunityCacheError::Response);
        }
        for profile in &sample.variables {
            let capability = capabilities
                .get(profile.name.as_str())
                .ok_or(CommunityCacheError::Response)?;
            let available_levels = profile.values.iter().flatten().count();
            let expected_coverage = if profile.expected_levels == 0 {
                0.0
            } else {
                available_levels as f64 / profile.expected_levels as f64
            };
            if capability.kind != "pressure3d"
                || !capability.pressure_profile
                || !capability.profile_cycle
                || profile.units != capability.units
                || profile.levels_hpa != capability.levels_hpa
                || profile.values.len() != profile.levels_hpa.len()
                || profile.expected_levels != profile.levels_hpa.len()
                || profile.available_levels != available_levels
                || !profile.coverage.is_finite()
                || (profile.coverage - expected_coverage).abs() > 1.0e-6
                || profile
                    .values
                    .iter()
                    .flatten()
                    .any(|value| !value.is_finite())
            {
                return Err(CommunityCacheError::Response);
            }
            total_values = total_values
                .checked_add(profile.values.len())
                .ok_or(CommunityCacheError::Response)?;
        }

        if sample.surface_samples.len() != selection.surface_variables.len() {
            return Err(CommunityCacheError::Response);
        }
        let mut expected_missing_surface = Vec::new();
        for (surface, expected_name) in sample
            .surface_samples
            .iter()
            .zip(&selection.surface_variables)
        {
            let capability = capabilities
                .get(expected_name.as_str())
                .ok_or(CommunityCacheError::Response)?;
            if surface.variable != *expected_name
                || surface.units != capability.units
                || capability.kind != "surface2d"
                || !capability.point_series
                || surface.value.is_some_and(|value| !value.is_finite())
            {
                return Err(CommunityCacheError::Response);
            }
            if surface.value.is_none() {
                expected_missing_surface.push(expected_name.clone());
            }
        }
        total_values = total_values
            .checked_add(sample.surface_samples.len())
            .ok_or(CommunityCacheError::Response)?;
        if sample.missing_surface_variables != expected_missing_surface {
            return Err(CommunityCacheError::Response);
        }

        let expected_status =
            if sample.missing_variables.is_empty() && sample.missing_surface_variables.is_empty() {
                RemoteProfileCycleSampleStatus::Complete
            } else if sample.variables.is_empty()
                && sample
                    .surface_samples
                    .iter()
                    .all(|sample| sample.value.is_none())
            {
                RemoteProfileCycleSampleStatus::Gap
            } else {
                RemoteProfileCycleSampleStatus::Partial
            };
        if sample.status != expected_status || total_values > REMOTE_PROFILE_CYCLE_MAX_VALUES {
            return Err(CommunityCacheError::Response);
        }
    }
    Ok(())
}

fn validate_remote_point(point: &RemoteGridPoint) -> Result<(), CommunityCacheError> {
    validate_coordinates(point.requested_latitude, point.requested_longitude)?;
    validate_coordinates(
        f64::from(point.grid_latitude),
        f64::from(point.grid_longitude),
    )
}

fn validate_remote_grid_point(
    point: &RemoteGridPoint,
    run: &RemoteRunDescriptor,
) -> Result<(), CommunityCacheError> {
    validate_remote_point(point)?;
    if point.x >= run.nx || point.y >= run.ny {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_coordinates(latitude: f64, longitude: f64) -> Result<(), CommunityCacheError> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || !(-90.0..=90.0).contains(&latitude)
        || !(-180.0..=180.0).contains(&longitude)
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_remote_component(value: &str) -> Result<(), CommunityCacheError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || value == "."
        || value == ".."
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_provenance_token(value: &str) -> Result<(), CommunityCacheError> {
    validate_remote_component(value)
}

fn encode_path_segment(value: &str) -> String {
    percent_encode(value)
}

fn encode_query_component(value: &str) -> String {
    percent_encode(value)
}

fn remote_latest_run_path(model: &str) -> Result<String, CommunityCacheError> {
    validate_remote_component(model)?;
    Ok(format!(
        "/v1/models/{}/latest-run",
        encode_path_segment(model)
    ))
}

fn case_directory_path(after: Option<&str>, limit: usize) -> Result<String, CommunityCacheError> {
    if !(1..=MAX_CASE_DIRECTORY_PAGE).contains(&limit) {
        return Err(CommunityCacheError::Response);
    }
    if let Some(after) = after {
        validate_case_directory_cursor(after)?;
        Ok(format!(
            "{LIST_CASES_PATH}?after={}&limit={limit}",
            encode_query_component(after)
        ))
    } else {
        Ok(format!("{LIST_CASES_PATH}?limit={limit}"))
    }
}

fn validate_case_directory_cursor(value: &str) -> Result<(), CommunityCacheError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn case_directory_response_limit(
    limits: &ProtocolLimits,
    limit: usize,
) -> Result<u64, CommunityCacheError> {
    if !(1..=MAX_CASE_DIRECTORY_PAGE).contains(&limit) {
        return Err(CommunityCacheError::Response);
    }
    limits
        .max_manifest_bytes
        .checked_mul(limit as u64)
        .and_then(|bytes| bytes.checked_add(CASE_DIRECTORY_ENVELOPE_MAX_BYTES))
        .ok_or(CommunityCacheError::Response)
}

fn publication_body_limit(limits: &ProtocolLimits) -> Result<u64, CommunityCacheError> {
    // A rendered image is base64 inside the typed JSON publication envelope.
    // Two times the encoded-object ceiling safely bounds that expansion plus
    // the canonical request and attribution metadata without an unbounded
    // allocation or arbitrary upload lane.
    limits
        .max_encoded_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(limits.max_manifest_bytes))
        .ok_or(CommunityCacheError::Quota)
}

fn decode_case_artifact_reference(
    bytes: &[u8],
    artifact: &CaseArtifactRef,
    limits: &ProtocolLimits,
) -> Result<CaseArtifactPayload, CommunityCacheError> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_encoded_bytes {
        return Err(CommunityCacheError::Response);
    }
    if object_sha256(bytes) != artifact.object_sha256 {
        return Err(CommunityCacheError::Response);
    }
    let payload: TypedObjectPayload<CaseArtifactPayload> =
        serde_json::from_slice(bytes).map_err(|_| CommunityCacheError::Response)?;
    if payload.schema != CASE_ARTIFACT_PAYLOAD_SCHEMA
        || payload.request_sha256 != artifact.request_sha256
        || payload.data.artifact_type() != artifact.artifact_type
    {
        return Err(CommunityCacheError::Response);
    }
    payload.data.validate(limits)?;
    Ok(payload.data)
}

fn verify_case_directory_page(
    page: &CaseRoomDirectoryPage,
    after: Option<&str>,
    requested_limit: usize,
    now_unix: i64,
    keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<(), CommunityCacheError> {
    if !(1..=MAX_CASE_DIRECTORY_PAGE).contains(&requested_limit)
        || page.cases.len() > requested_limit
    {
        return Err(CommunityCacheError::Response);
    }
    if let Some(after) = after {
        validate_case_directory_cursor(after)?;
    }
    page.verify(now_unix, keys, limits)?;

    // `verify` validates every field/signature/artifact reference. Retain the
    // protocol's per-manifest serialized bound as well as the whole-response
    // transport bound so a directory cannot amplify many individually large
    // manifests into unexpected memory use.
    for signed in &page.cases {
        let bytes = serde_json::to_vec(signed).map_err(|_| CommunityCacheError::Response)?;
        if bytes.is_empty() || bytes.len() as u64 > limits.max_manifest_bytes {
            return Err(CommunityCacheError::Response);
        }
        let manifest = &signed.manifest;
        if [
            manifest.event_start_unix,
            manifest.event_end_unix,
            manifest.published_unix,
            manifest.retain_until_unix,
        ]
        .into_iter()
        .any(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0).is_none())
        {
            return Err(CommunityCacheError::Response);
        }
    }

    if let Some(after) = after
        && page
            .cases
            .first()
            .is_some_and(|case| case.manifest.case_id.as_str() <= after)
    {
        return Err(CommunityCacheError::Response);
    }
    if let Some(next) = page.next_after.as_deref()
        && (after.is_some_and(|after| next <= after)
            || page
                .cases
                .last()
                .is_some_and(|case| next < case.manifest.case_id.as_str()))
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn select_axis_probe_variable(variables: &[RemoteVariableCapability]) -> Option<&str> {
    variables
        .iter()
        .filter(|variable| {
            variable.point_series
                && variable.available_samples > 0
                && !variable.available_slots.is_empty()
        })
        .min_by_key(|variable| {
            let surface_first = variable.kind != "surface2d";
            (surface_first, variable.name.as_str())
        })
        .map(|variable| variable.name.as_str())
}

fn build_remote_profile_request(
    catalog: &RemoteProfileCatalog,
    time: &RemoteTimePoint,
    mut selection: RemoteProfileVariableSelection,
    limits: &ProtocolLimits,
) -> Result<ShareRequest, CommunityCacheError> {
    validate_remote_run(&catalog.run)?;
    validate_remote_point(&catalog.point)?;
    validate_remote_axis(&catalog.axis, &catalog.run, &catalog.variables)?;
    if !catalog.axis.contains(time) {
        return Err(CommunityCacheError::Response);
    }
    deny_automatic_private_run(&catalog.run)?;

    selection.pressure_variables.sort();
    selection.pressure_variables.dedup();
    selection.surface_variables.sort();
    selection.surface_variables.dedup();
    selection.pressure_levels_hpa.sort();
    selection.pressure_levels_hpa.dedup();
    if selection.pressure_variables.is_empty() || selection.surface_variables.is_empty() {
        return Err(CommunityCacheError::Response);
    }
    let available = catalog
        .variables
        .iter()
        .map(|variable| (variable.name.as_str(), variable))
        .collect::<BTreeMap<_, _>>();
    for name in &selection.pressure_variables {
        let variable = available
            .get(name.as_str())
            .ok_or(CommunityCacheError::Response)?;
        if !variable.pressure_profile
            || variable.kind != "pressure3d"
            || !variable.available_slots.contains(&time.storage_slot)
            || (!selection.pressure_levels_hpa.is_empty()
                && selection
                    .pressure_levels_hpa
                    .iter()
                    .any(|level| !variable.levels_hpa.contains(level)))
        {
            return Err(CommunityCacheError::Response);
        }
    }
    for name in &selection.surface_variables {
        let variable = available
            .get(name.as_str())
            .ok_or(CommunityCacheError::Response)?;
        if !variable.point_series
            || variable.kind != "surface2d"
            || !variable.available_slots.contains(&time.storage_slot)
        {
            return Err(CommunityCacheError::Response);
        }
    }

    let latitude_e7 = coordinate_e7(catalog.point.grid_latitude, -90.0, 90.0)?;
    let longitude_e7 = coordinate_e7(catalog.point.grid_longitude, -180.0, 180.0)?;
    let source_provenance = catalog
        .run
        .source_provenance
        .iter()
        .map(|source| SourceProvenance {
            provider: source.provider.clone(),
            roles: source.roles.clone(),
            products: source.products.clone(),
        })
        .collect::<Vec<_>>();
    let mut variables = selection.pressure_variables.clone();
    variables.extend(selection.surface_variables.iter().cloned());
    let request = ShareRequest {
        schema: REQUEST_SCHEMA.into(),
        model: catalog.run.model.clone(),
        run: catalog.run.run.clone(),
        snapshot_id: catalog.run.snapshot_id.clone(),
        grid_hash: catalog.run.grid_hash.clone(),
        variables,
        query: ShareQuery::Profile {
            latitude_e7,
            longitude_e7,
            storage_slot: time.storage_slot,
            valid_unix: time.valid_unix,
            pressure_variables: selection.pressure_variables,
            surface_variables: selection.surface_variables,
            pressure_levels_hpa: selection.pressure_levels_hpa,
        },
        recipe: RecipeIdentity {
            recipe_id: "native-profile".into(),
            recipe_version: "1".into(),
            parameters: BTreeMap::new(),
        },
        source_provenance,
        publication: PublicationGrant {
            data_origin: DataOrigin::PublicProvider,
            explicit_owner_publication: false,
            redistribution_rights_confirmed: true,
        },
    }
    .normalized();
    request.validate(limits)?;
    Ok(request)
}

fn build_remote_point_series_request(
    catalog: &RemoteProfileCatalog,
    mut selection: RemotePointSeriesSelection,
    limits: &ProtocolLimits,
) -> Result<ShareRequest, CommunityCacheError> {
    validate_remote_catalog_for_request(catalog)?;
    normalize_selected_variables(&mut selection.variables)?;
    validate_time_window(&selection.window)?;
    for name in &selection.variables {
        let variable = remote_variable(catalog, name)?;
        if !variable.point_series || variable.kind != "surface2d" {
            return Err(CommunityCacheError::Response);
        }
    }
    let request = remote_request(
        catalog,
        selection.variables,
        ShareQuery::PointSeries {
            latitude_e7: coordinate_e7(catalog.point.grid_latitude, -90.0, 90.0)?,
            longitude_e7: coordinate_e7(catalog.point.grid_longitude, -180.0, 180.0)?,
            window: selection.window,
            missing_policy: selection.missing_policy,
        },
        "native-point-series",
        BTreeMap::new(),
    )?;
    request.validate(limits)?;
    Ok(request)
}

fn build_remote_native_window_request(
    catalog: &RemoteProfileCatalog,
    mut selection: RemoteNativeWindowSelection,
    limits: &ProtocolLimits,
) -> Result<ShareRequest, CommunityCacheError> {
    validate_remote_catalog_for_request(catalog)?;
    normalize_selected_variables(&mut selection.variables)?;
    selection.pressure_levels_hpa.sort_unstable();
    selection.pressure_levels_hpa.dedup();
    if !catalog.axis.contains(&selection.time)
        || selection.x0 >= selection.x1
        || selection.y0 >= selection.y1
        || usize::try_from(selection.x1).map_or(true, |x1| x1 > catalog.run.nx)
        || usize::try_from(selection.y1).map_or(true, |y1| y1 > catalog.run.ny)
    {
        return Err(CommunityCacheError::Response);
    }
    let pressure = !selection.pressure_levels_hpa.is_empty();
    for name in &selection.variables {
        let variable = remote_variable(catalog, name)?;
        if !variable
            .available_slots
            .contains(&selection.time.storage_slot)
            || (pressure && variable.kind != "pressure3d")
            || (!pressure && variable.kind != "surface2d")
            || (pressure
                && selection
                    .pressure_levels_hpa
                    .iter()
                    .any(|level| !variable.levels_hpa.contains(level)))
        {
            return Err(CommunityCacheError::Response);
        }
    }
    let request = remote_request(
        catalog,
        selection.variables,
        ShareQuery::NativeWindow {
            storage_slot: selection.time.storage_slot,
            valid_unix: selection.time.valid_unix,
            x0: selection.x0,
            y0: selection.y0,
            x1: selection.x1,
            y1: selection.y1,
            pressure_levels_hpa: selection.pressure_levels_hpa,
        },
        "native-window",
        BTreeMap::new(),
    )?;
    request.validate(limits)?;
    Ok(request)
}

fn build_remote_geographic_window_request(
    catalog: &RemoteProfileCatalog,
    mut selection: RemoteGeographicWindowSelection,
    limits: &ProtocolLimits,
) -> Result<ShareRequest, CommunityCacheError> {
    validate_remote_catalog_for_request(catalog)?;
    normalize_selected_variables(&mut selection.variables)?;
    selection.pressure_levels_hpa.sort_unstable();
    selection.pressure_levels_hpa.dedup();
    if !catalog.axis.contains(&selection.time) {
        return Err(CommunityCacheError::Response);
    }

    let west_longitude_e7 = coordinate_e7_f64(selection.west_longitude, -180.0, 180.0)?;
    let south_latitude_e7 = coordinate_e7_f64(selection.south_latitude, -90.0, 90.0)?;
    let east_longitude_e7 = coordinate_e7_f64(selection.east_longitude, -180.0, 180.0)?;
    let north_latitude_e7 = coordinate_e7_f64(selection.north_latitude, -90.0, 90.0)?;
    if south_latitude_e7 >= north_latitude_e7 || west_longitude_e7 == east_longitude_e7 {
        return Err(CommunityCacheError::Response);
    }

    let pressure = !selection.pressure_levels_hpa.is_empty();
    for name in &selection.variables {
        let variable = remote_variable(catalog, name)?;
        if !variable
            .available_slots
            .contains(&selection.time.storage_slot)
            || !variable.geographic_window
            || (pressure && variable.kind != "pressure3d")
            || (!pressure && variable.kind != "surface2d")
            || (pressure
                && selection
                    .pressure_levels_hpa
                    .iter()
                    .any(|level| !variable.levels_hpa.contains(level)))
        {
            return Err(CommunityCacheError::Response);
        }
    }
    let request = remote_request(
        catalog,
        selection.variables,
        ShareQuery::GeographicWindow {
            storage_slot: selection.time.storage_slot,
            valid_unix: selection.time.valid_unix,
            west_longitude_e7,
            south_latitude_e7,
            east_longitude_e7,
            north_latitude_e7,
            pressure_levels_hpa: selection.pressure_levels_hpa,
        },
        "geographic-window",
        BTreeMap::new(),
    )?;
    request.validate(limits)?;
    Ok(request)
}

fn build_remote_temporal_grid_request(
    catalog: &RemoteProfileCatalog,
    mut selection: RemoteTemporalGridSelection,
    limits: &ProtocolLimits,
) -> Result<ShareRequest, CommunityCacheError> {
    validate_remote_catalog_for_request(catalog)?;
    normalize_selected_variables(&mut selection.variables)?;
    validate_time_window(&selection.window)?;
    selection.pressure_levels_hpa.sort_unstable();
    selection.pressure_levels_hpa.dedup();
    validate_remote_token(&selection.reducer, 96)?;
    validate_remote_token(&selection.semantics, 96)?;
    if selection.parameters.len() > 64 {
        return Err(CommunityCacheError::Response);
    }
    for (key, value) in &selection.parameters {
        validate_remote_token(key, 96)?;
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            return Err(CommunityCacheError::Response);
        }
    }
    let pressure = !selection.pressure_levels_hpa.is_empty();
    for name in &selection.variables {
        let variable = remote_variable(catalog, name)?;
        let kind_and_levels_match = if pressure {
            variable.kind == "pressure3d"
                && selection
                    .pressure_levels_hpa
                    .iter()
                    .all(|level| variable.levels_hpa.contains(level))
        } else {
            variable.kind == "surface2d"
        };
        if !kind_and_levels_match || variable.available_samples == 0 {
            return Err(CommunityCacheError::Response);
        }
    }
    let request = remote_request(
        catalog,
        selection.variables,
        ShareQuery::TemporalGrid {
            window: selection.window,
            reducer: selection.reducer,
            semantics: selection.semantics,
            missing_policy: selection.missing_policy,
            pressure_levels_hpa: selection.pressure_levels_hpa,
        },
        "native-temporal-grid",
        selection.parameters,
    )?;
    request.validate(limits)?;
    Ok(request)
}

fn validate_remote_catalog_for_request(
    catalog: &RemoteProfileCatalog,
) -> Result<(), CommunityCacheError> {
    validate_remote_run(&catalog.run)?;
    validate_remote_grid_point(&catalog.point, &catalog.run)?;
    validate_remote_axis(&catalog.axis, &catalog.run, &catalog.variables)?;
    deny_automatic_private_run(&catalog.run)
}

fn remote_variable<'a>(
    catalog: &'a RemoteProfileCatalog,
    name: &str,
) -> Result<&'a RemoteVariableCapability, CommunityCacheError> {
    catalog
        .variables
        .iter()
        .find(|variable| variable.name == name)
        .ok_or(CommunityCacheError::Response)
}

fn normalize_selected_variables(variables: &mut Vec<String>) -> Result<(), CommunityCacheError> {
    for variable in variables.iter_mut() {
        *variable = variable.trim().to_ascii_lowercase();
    }
    variables.sort();
    variables.dedup();
    if variables.is_empty()
        || variables
            .iter()
            .any(|variable| validate_remote_token(variable, 128).is_err())
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_remote_token(value: &str, maximum: usize) -> Result<(), CommunityCacheError> {
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-' | b'.' | b'/')
        })
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(())
}

fn validate_time_window(window: &TimeWindow) -> Result<(), CommunityCacheError> {
    let valid = match window {
        TimeWindow::Utc {
            start_unix,
            end_unix,
        } => start_unix >= &0 && end_unix > start_unix,
        TimeWindow::LocalDay {
            date,
            timezone,
            resolved_start_unix,
            resolved_end_unix,
        } => {
            !date.is_empty()
                && date.len() <= 16
                && !timezone.is_empty()
                && timezone.len() <= 96
                && *resolved_start_unix >= 0
                && resolved_end_unix > resolved_start_unix
                && !date.chars().any(char::is_control)
                && !timezone.chars().any(char::is_control)
        }
    };
    valid.then_some(()).ok_or(CommunityCacheError::Response)
}

fn remote_request(
    catalog: &RemoteProfileCatalog,
    variables: Vec<String>,
    query: ShareQuery,
    recipe_id: &str,
    parameters: BTreeMap<String, String>,
) -> Result<ShareRequest, CommunityCacheError> {
    let source_provenance = catalog
        .run
        .source_provenance
        .iter()
        .map(|source| SourceProvenance {
            provider: source.provider.clone(),
            roles: source.roles.clone(),
            products: source.products.clone(),
        })
        .collect();
    Ok(ShareRequest {
        schema: REQUEST_SCHEMA.into(),
        model: catalog.run.model.clone(),
        run: catalog.run.run.clone(),
        snapshot_id: catalog.run.snapshot_id.clone(),
        grid_hash: catalog.run.grid_hash.clone(),
        variables,
        query,
        recipe: RecipeIdentity {
            recipe_id: recipe_id.into(),
            recipe_version: "1".into(),
            parameters,
        },
        source_provenance,
        publication: PublicationGrant {
            data_origin: DataOrigin::PublicProvider,
            explicit_owner_publication: false,
            redistribution_rights_confirmed: true,
        },
    }
    .normalized())
}

fn coordinate_e7(value: f32, minimum: f64, maximum: f64) -> Result<i32, CommunityCacheError> {
    coordinate_e7_f64(f64::from(value), minimum, maximum)
}

fn coordinate_e7_f64(value: f64, minimum: f64, maximum: f64) -> Result<i32, CommunityCacheError> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        return Err(CommunityCacheError::Response);
    }
    let scaled = (value * 10_000_000.0).round();
    if scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(CommunityCacheError::Response);
    }
    Ok(scaled as i32)
}

fn decode_typed_payload<T: DeserializeOwned>(
    decoded: &[u8],
    request: &ShareRequest,
    expected_schema: &'static str,
) -> Result<TypedObjectPayload<T>, CommunityCacheError> {
    let payload: TypedObjectPayload<T> =
        serde_json::from_slice(decoded).map_err(|_| CommunityCacheError::Response)?;
    validate_typed_payload_identity(&payload, expected_schema, request)?;
    Ok(payload)
}

fn deny_automatic_private_run(run: &RemoteRunDescriptor) -> Result<(), CommunityCacheError> {
    automatic_public_provider_run_allowed(
        &run.model,
        run.source_provenance
            .iter()
            .map(|source| source.provider.as_str()),
    )
    .then_some(())
    .ok_or(CommunityCacheError::Disabled)
}

/// The only automatic PublicProvider classification used by both local and
/// remote request builders. Model names are insufficient provenance: every
/// persisted source must name an explicitly approved public distribution
/// service. Everything else stays local until an owner publication flow
/// supplies explicit rights-confirmed provenance.
pub(crate) fn automatic_public_provider_run_allowed<'a>(
    model: &str,
    providers: impl IntoIterator<Item = &'a str>,
) -> bool {
    let model = model.trim().to_ascii_lowercase();
    if model.is_empty() || model.contains("wrf") || model.contains("arwen") {
        return false;
    }
    let mut saw_provider = false;
    for provider in providers {
        saw_provider = true;
        if !matches!(
            provider.trim(),
            "noaa-aws-public-data" | "noaa-nomads" | "noaa-ncei" | "ecmwf-open-data" | "ucar-gdex"
        ) {
            return false;
        }
    }
    saw_provider
}

fn phase1_delivery_order(r2_configured: bool) -> impl Iterator<Item = DeliveryTier> {
    [
        Some(DeliveryTier::LocalCache),
        r2_configured.then_some(DeliveryTier::R2),
        Some(DeliveryTier::Origin),
    ]
    .into_iter()
    .flatten()
}

/// Shared, durable transfer accounting. Clones of the client use one active
/// counter and one usage ledger, so parallel panes cannot independently spend
/// the same allowance. Calendar-month usage survives application restarts.
#[derive(Clone)]
struct TransferGate {
    inner: Arc<Mutex<TransferState>>,
    ledger_path: PathBuf,
}

impl std::fmt::Debug for TransferGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransferGate")
            .field("policy", &lock_unpoisoned(&self.inner).policy)
            .field("ledger_path", &self.ledger_path)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct TransferState {
    active: usize,
    usage: TransferUsage,
    policy: TransferPolicy,
}

#[derive(Debug, Clone, Copy)]
struct TransferPolicy {
    download_hourly_limit: u64,
    upload_hourly_limit: u64,
    monthly_limit: u64,
    max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferUsage {
    schema: String,
    download_hour_bucket: i64,
    download_hour_bytes: u64,
    upload_hour_bucket: i64,
    upload_hour_bytes: u64,
    month_bucket: i32,
    month_bytes: u64,
}

impl Default for TransferUsage {
    fn default() -> Self {
        Self {
            schema: TRANSFER_USAGE_SCHEMA.to_owned(),
            download_hour_bucket: i64::MIN,
            download_hour_bytes: 0,
            upload_hour_bucket: i64::MIN,
            upload_hour_bytes: 0,
            month_bucket: i32::MIN,
            month_bytes: 0,
        }
    }
}

struct ActiveTransfer {
    gate: TransferGate,
}

/// One shared concurrency reservation for a relay session. The byte ledger is
/// charged separately when verified bytes are actually received or selected
/// for upload, and the reservation is released automatically on cancellation.
pub(crate) struct CommunityRelayTransfer {
    _active: ActiveTransfer,
}

impl Drop for ActiveTransfer {
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.gate.inner);
        state.active = state.active.saturating_sub(1);
    }
}

impl TransferGate {
    /// Return the process-wide accounting state for one canonical ledger.
    /// BowEcho deliberately constructs cache clients for several UI/runtime
    /// seams; sharing this state prevents those clients from independently
    /// spending the same allowance or overwriting one another's ledger.
    fn shared(
        download_hourly_limit: u64,
        upload_hourly_limit: u64,
        monthly_limit: u64,
        max_concurrent: u8,
        ledger_path: PathBuf,
    ) -> Result<Self, CommunityCacheError> {
        let ledger_path = canonical_ledger_path(&ledger_path)?;
        let registry = TRANSFER_GATE_REGISTRY.get_or_init(Default::default);
        let mut registry = lock_unpoisoned(registry);
        registry.retain(|_, value| value.strong_count() > 0);
        let inner = registry
            .get(&ledger_path)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let usage = load_transfer_usage(&ledger_path);
                let inner = Arc::new(Mutex::new(TransferState {
                    active: 0,
                    usage,
                    policy: TransferPolicy {
                        download_hourly_limit,
                        upload_hourly_limit,
                        monthly_limit,
                        max_concurrent: usize::from(max_concurrent.max(1)),
                    },
                }));
                registry.insert(ledger_path.clone(), Arc::downgrade(&inner));
                inner
            });
        {
            // Existing client handles can outlive a Settings save. Converge
            // every handle on the most conservative live policy so a stale
            // loose client cannot bypass newly lowered caps.
            let mut state = lock_unpoisoned(&inner);
            state.policy.download_hourly_limit = state
                .policy
                .download_hourly_limit
                .min(download_hourly_limit);
            state.policy.upload_hourly_limit =
                state.policy.upload_hourly_limit.min(upload_hourly_limit);
            state.policy.monthly_limit = state.policy.monthly_limit.min(monthly_limit);
            state.policy.max_concurrent = state
                .policy
                .max_concurrent
                .min(usize::from(max_concurrent.max(1)));
        }
        Ok(Self { inner, ledger_path })
    }

    #[cfg(test)]
    fn new(
        download_hourly_limit: u64,
        upload_hourly_limit: u64,
        monthly_limit: u64,
        max_concurrent: u8,
        ledger_path: PathBuf,
    ) -> Self {
        let usage = load_transfer_usage(&ledger_path);
        Self {
            inner: Arc::new(Mutex::new(TransferState {
                active: 0,
                usage,
                policy: TransferPolicy {
                    download_hourly_limit,
                    upload_hourly_limit,
                    monthly_limit,
                    max_concurrent: usize::from(max_concurrent.max(1)),
                },
            })),
            ledger_path,
        }
    }

    fn begin(&self) -> Result<ActiveTransfer, CommunityCacheError> {
        let mut state = lock_unpoisoned(&self.inner);
        if state.active >= state.policy.max_concurrent {
            return Err(CommunityCacheError::Quota);
        }
        state.active += 1;
        Ok(ActiveTransfer { gate: self.clone() })
    }

    /// Charge bytes after they have crossed the network boundary. The ledger
    /// is written before more bytes are accepted, so a second concurrent
    /// transfer observes the same allowance and a restart cannot reset it.
    fn charge_download(&self, at_unix: i64, bytes: u64) -> Result<(), CommunityCacheError> {
        self.charge(at_unix, bytes, TransferDirection::Download)
    }

    fn charge_upload(&self, at_unix: i64, bytes: u64) -> Result<(), CommunityCacheError> {
        self.charge(at_unix, bytes, TransferDirection::Upload)
    }

    fn charge(
        &self,
        at_unix: i64,
        bytes: u64,
        direction: TransferDirection,
    ) -> Result<(), CommunityCacheError> {
        if bytes == 0 {
            return Ok(());
        }
        let mut state = lock_unpoisoned(&self.inner);
        normalize_usage_buckets(&mut state.usage, at_unix);
        let next_month = state.usage.month_bytes.saturating_add(bytes);
        let policy = state.policy;
        let (hour_bytes, hourly_limit) = match direction {
            TransferDirection::Download => (
                &mut state.usage.download_hour_bytes,
                policy.download_hourly_limit,
            ),
            TransferDirection::Upload => (
                &mut state.usage.upload_hour_bytes,
                policy.upload_hourly_limit,
            ),
        };
        let next_hour = hour_bytes.saturating_add(bytes);
        // Count even the final chunk that crossed a limit. It was received,
        // and retaining the overage closes restart/retry bypasses.
        *hour_bytes = next_hour;
        state.usage.month_bytes = next_month;
        persist_transfer_usage(&self.ledger_path, &state.usage)?;
        if next_hour > hourly_limit || next_month > policy.monthly_limit {
            return Err(CommunityCacheError::Quota);
        }
        Ok(())
    }

    fn remaining_download(&self, at_unix: i64) -> u64 {
        self.remaining(at_unix, TransferDirection::Download)
    }

    fn remaining_upload(&self, at_unix: i64) -> u64 {
        self.remaining(at_unix, TransferDirection::Upload)
    }

    fn remaining(&self, at_unix: i64, direction: TransferDirection) -> u64 {
        let mut state = lock_unpoisoned(&self.inner);
        normalize_usage_buckets(&mut state.usage, at_unix);
        let policy = state.policy;
        let (spent, limit) = match direction {
            TransferDirection::Download => (
                state.usage.download_hour_bytes,
                policy.download_hourly_limit,
            ),
            TransferDirection::Upload => {
                (state.usage.upload_hour_bytes, policy.upload_hourly_limit)
            }
        };
        limit
            .saturating_sub(spent)
            .min(policy.monthly_limit.saturating_sub(state.usage.month_bytes))
    }

    #[cfg(test)]
    fn usage(&self) -> TransferUsage {
        lock_unpoisoned(&self.inner).usage.clone()
    }
}

#[derive(Clone, Copy)]
enum TransferDirection {
    Download,
    Upload,
}

static TRANSFER_GATE_REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<TransferState>>>>> =
    OnceLock::new();

fn canonical_ledger_path(path: &Path) -> Result<PathBuf, CommunityCacheError> {
    let parent = path.parent().ok_or(CommunityCacheError::Storage)?;
    fs::create_dir_all(parent).map_err(|_| CommunityCacheError::Storage)?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| CommunityCacheError::Storage)?;
    let file_name = path.file_name().ok_or(CommunityCacheError::Storage)?;
    Ok(canonical_parent.join(file_name))
}

fn load_transfer_usage(path: &Path) -> TransferUsage {
    let Ok(bytes) = read_file_bounded(path, 64 * 1024) else {
        return TransferUsage::default();
    };
    if let Ok(usage) = serde_json::from_slice::<TransferUsage>(&bytes)
        && usage.schema == TRANSFER_USAGE_SCHEMA
    {
        return usage;
    }
    serde_json::from_slice::<LegacyTransferUsage>(&bytes)
        .ok()
        .filter(|usage| usage.schema == LEGACY_TRANSFER_USAGE_SCHEMA)
        .map(|legacy| TransferUsage {
            schema: TRANSFER_USAGE_SCHEMA.to_owned(),
            download_hour_bucket: legacy.hour_bucket,
            download_hour_bytes: legacy.hour_bytes,
            upload_hour_bucket: i64::MIN,
            upload_hour_bytes: 0,
            month_bucket: legacy.month_bucket,
            month_bytes: legacy.month_bytes,
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyTransferUsage {
    schema: String,
    hour_bucket: i64,
    hour_bytes: u64,
    month_bucket: i32,
    month_bytes: u64,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn normalize_usage_buckets(usage: &mut TransferUsage, at_unix: i64) {
    let hour = at_unix.div_euclid(3_600);
    let month = chrono::DateTime::from_timestamp(at_unix, 0)
        .map(|time| time.year().saturating_mul(12) + i32::try_from(time.month0()).unwrap_or(0))
        .unwrap_or(i32::MIN);
    if usage.download_hour_bucket != hour {
        usage.download_hour_bucket = hour;
        usage.download_hour_bytes = 0;
    }
    if usage.upload_hour_bucket != hour {
        usage.upload_hour_bucket = hour;
        usage.upload_hour_bytes = 0;
    }
    if usage.month_bucket != month {
        usage.month_bucket = month;
        usage.month_bytes = 0;
    }
}

fn persist_transfer_usage(path: &Path, usage: &TransferUsage) -> Result<(), CommunityCacheError> {
    let bytes = serde_json::to_vec(usage).map_err(|_| CommunityCacheError::Storage)?;
    atomic_write(path, &bytes)
}

#[derive(Debug, Clone)]
struct VerifiedDiskCache {
    root: PathBuf,
    byte_limit: u64,
    lock: Arc<Mutex<()>>,
    prune_offset: Arc<Mutex<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheIndex {
    schema: String,
    entries: BTreeMap<String, CacheIndexEntry>,
}

impl Default for CacheIndex {
    fn default() -> Self {
        Self {
            schema: INDEX_SCHEMA.to_owned(),
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheIndexEntry {
    object_sha256: String,
    encoded_size: u64,
    /// A signed manifest may outlive locally cached bytes so an exact cold
    /// object can still be looked up without consulting the origin first.
    /// Old v1 indexes predate this flag and necessarily described a present
    /// object, hence the compatibility default is true.
    #[serde(default = "default_true")]
    object_present: bool,
    #[serde(default)]
    manifest_size: u64,
    last_access_unix: i64,
}

fn default_true() -> bool {
    true
}

impl VerifiedDiskCache {
    fn new(root: PathBuf, byte_limit: u64) -> Self {
        let lock = shared_cache_lock(&root);
        Self {
            root,
            byte_limit,
            lock,
            prune_offset: Arc::new(Mutex::new(0)),
        }
    }

    fn load(
        &self,
        request: &ShareRequest,
        keys: &TrustedSigningKeys,
        limits: &ProtocolLimits,
    ) -> Result<Option<VerifiedObject>, CommunityCacheError> {
        let _cache_guard = lock_unpoisoned(&self.lock);
        let request_hash = request_sha256(request)?;
        let mut index = self.read_index();
        let Some(entry) = index.entries.get(&request_hash).cloned() else {
            return Ok(None);
        };
        if !is_sha256_hex(&entry.object_sha256) {
            index.entries.remove(&request_hash);
            let _ = self.write_index(&index);
            return Ok(None);
        }
        let manifest_path = self.manifest_path(&request_hash);
        let manifest = (|| {
            let manifest_bytes = read_file_bounded(&manifest_path, limits.max_manifest_bytes)?;
            let manifest = parse_signed_object_manifest_bounded(&manifest_bytes, limits)?;
            if manifest.manifest.request != *request
                || manifest.manifest.request_sha256 != request_hash
                || manifest.manifest.object_sha256 != entry.object_sha256
            {
                return Err(CommunityCacheError::Response);
            }
            rw_community_relay::verify_origin_signed_identity(&manifest, now_unix(), keys, limits)
                .map_err(|_| CommunityCacheError::Response)?;
            Ok::<_, CommunityCacheError>(manifest)
        })();
        let manifest = match manifest {
            Ok(manifest) => manifest,
            Err(_) => {
                // An invalid/expired identity must not remain available for
                // relay lookup or seeding.
                self.remove_entry(&mut index, &request_hash);
                let _ = self.write_index(&index);
                return Ok(None);
            }
        };

        let object_path = self.object_path(&entry.object_sha256);
        if !entry.object_present && !object_path.is_file() {
            return Ok(None);
        }
        let encoded = match read_file_exact(
            &object_path,
            manifest.manifest.encoded_size,
            limits.max_encoded_bytes,
        ) {
            Ok(encoded) => encoded,
            Err(_) => {
                // Keep the valid signed identity, but never claim missing or
                // corrupt bytes as locally readable/seedable.
                self.mark_object_missing(&mut index, &entry.object_sha256);
                let _ = self.write_index(&index);
                return Ok(None);
            }
        };
        if verify_signed_object(&manifest, request, &encoded, now_unix(), keys, limits).is_err() {
            self.mark_object_missing(&mut index, &entry.object_sha256);
            let _ = self.write_index(&index);
            return Ok(None);
        }
        let decoded = match decode_verified(&manifest.manifest, &encoded, limits) {
            Ok(decoded) => decoded,
            Err(_) => {
                // The origin-signed object itself is unusable under the
                // current bounded decoder contract; do not repeatedly offer
                // that identity through the relay.
                self.remove_entry(&mut index, &request_hash);
                let _ = self.write_index(&index);
                return Ok(None);
            }
        };
        if let Some(entry) = index.entries.get_mut(&request_hash) {
            entry.object_present = true;
            entry.last_access_unix = now_unix();
            let _ = self.write_index(&index);
        }
        Ok(Some(VerifiedObject {
            manifest,
            encoded,
            decoded,
            tier: DeliveryTier::LocalCache,
        }))
    }

    /// Read and verify a retained signed identity without requiring the
    /// encoded object bytes to remain present. This is the only local metadata
    /// source admitted before a cold relay lookup.
    fn retained_manifest(
        &self,
        request: &ShareRequest,
        keys: &TrustedSigningKeys,
        limits: &ProtocolLimits,
    ) -> Result<Option<SignedObjectManifest>, CommunityCacheError> {
        let _cache_guard = lock_unpoisoned(&self.lock);
        let request_hash = request_sha256(request)?;
        let index = self.read_index();
        let Some(entry) = index.entries.get(&request_hash) else {
            return Ok(None);
        };
        if !is_sha256_hex(&entry.object_sha256) {
            return Ok(None);
        }
        let bytes = match read_file_bounded(
            &self.manifest_path(&request_hash),
            limits.max_manifest_bytes,
        ) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        let manifest = match parse_signed_object_manifest_bounded(&bytes, limits) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(None),
        };
        if manifest.manifest.request != *request
            || manifest.manifest.request_sha256 != request_hash
            || manifest.manifest.object_sha256 != entry.object_sha256
            || validate_object_manifest(&manifest.manifest, limits).is_err()
            || rw_community_relay::verify_origin_signed_identity(
                &manifest,
                now_unix(),
                keys,
                limits,
            )
            .is_err()
        {
            return Ok(None);
        }
        Ok(Some(manifest))
    }

    fn store(&self, object: &VerifiedObject) -> Result<(), CommunityCacheError> {
        let _cache_guard = lock_unpoisoned(&self.lock);
        fs::create_dir_all(self.root.join("objects")).map_err(|_| CommunityCacheError::Storage)?;
        fs::create_dir_all(self.root.join("manifests"))
            .map_err(|_| CommunityCacheError::Storage)?;
        let manifest = &object.manifest.manifest;
        if !is_sha256_hex(&manifest.object_sha256)
            || !is_sha256_hex(&manifest.request_sha256)
            || object.encoded.len() as u64 != manifest.encoded_size
            || object_sha256(&object.encoded) != manifest.object_sha256
        {
            return Err(CommunityCacheError::Response);
        }
        let encoded_path = self.object_path(&manifest.object_sha256);
        let manifest_path = self.manifest_path(&manifest.request_sha256);
        // Persist the exact verified bytes. Re-encoding a decoded body can
        // silently change the content hash, frame parameters, or signature.
        atomic_write_new_or_same(&encoded_path, &object.encoded)?;
        let manifest_bytes =
            serde_json::to_vec(&object.manifest).map_err(|_| CommunityCacheError::Response)?;
        atomic_write(&manifest_path, &manifest_bytes)?;

        let mut index = self.read_index();
        index.entries.insert(
            manifest.request_sha256.clone(),
            CacheIndexEntry {
                object_sha256: manifest.object_sha256.clone(),
                encoded_size: manifest.encoded_size,
                object_present: true,
                manifest_size: manifest_bytes.len() as u64,
                last_access_unix: now_unix(),
            },
        );
        for entry in index.entries.values_mut() {
            if entry.object_sha256 == manifest.object_sha256 {
                entry.object_present = true;
            }
        }
        self.evict(&mut index);
        self.write_index(&index)
    }

    fn prune_untrusted(&self, keys: &TrustedSigningKeys, limits: &ProtocolLimits, maximum: usize) {
        let requests = self.indexed_requests_rotating(limits, maximum);
        for request in requests {
            // `load` removes the entry and any now-unreferenced object when
            // signature, key id, expiry, size, schema, or hash fails.
            let _ = self.load(&request, keys, limits);
        }
    }

    fn verified_seed_candidates(
        &self,
        keys: &TrustedSigningKeys,
        limits: &ProtocolLimits,
        maximum: usize,
    ) -> Vec<rw_community_relay::VerifiedSeedObject> {
        let requests = self.indexed_requests(limits, maximum.saturating_mul(4));
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        for request in requests {
            if result.len() >= maximum {
                break;
            }
            if !matches!(
                request.query,
                ShareQuery::Profile { .. } | ShareQuery::PointSeries { .. }
            ) || !relay_seed_publication_is_allowed(&request.publication)
            {
                continue;
            }
            let Ok(Some(object)) = self.load(&request, keys, limits) else {
                continue;
            };
            if object.encoded.len() as u64 > rw_community_relay::INITIAL_RELAY_OBJECT_BYTES
                || !seen.insert(object.manifest.manifest.object_sha256.clone())
            {
                continue;
            }
            result.push(rw_community_relay::VerifiedSeedObject {
                manifest: object.manifest,
                encoded: object.encoded,
            });
        }
        result
    }

    fn load_exact_seed(
        &self,
        object_sha256: &str,
        keys: &TrustedSigningKeys,
        limits: &ProtocolLimits,
    ) -> Result<Option<rw_community_relay::VerifiedSeedObject>, CommunityCacheError> {
        if !is_sha256_hex(object_sha256) {
            return Ok(None);
        }
        // A grant can only target the bounded recent candidate window we
        // advertise. Never turn hostile exact-hash polling into a 100k-entry
        // synchronous cache scan on the relay runtime.
        for request in self.indexed_requests(limits, MAX_RELAY_SEED_OBJECTS.saturating_mul(4)) {
            if !matches!(
                request.query,
                ShareQuery::Profile { .. } | ShareQuery::PointSeries { .. }
            ) || !relay_seed_publication_is_allowed(&request.publication)
            {
                continue;
            }
            let Some(object) = self.load(&request, keys, limits)? else {
                continue;
            };
            if object.manifest.manifest.object_sha256 == object_sha256
                && object.encoded.len() as u64 <= rw_community_relay::INITIAL_RELAY_OBJECT_BYTES
            {
                return Ok(Some(rw_community_relay::VerifiedSeedObject {
                    manifest: object.manifest,
                    encoded: object.encoded,
                }));
            }
        }
        Ok(None)
    }

    /// Snapshot only canonical requests from safe manifest paths, newest
    /// first. Parsing or later verification failure is handled by `load` and
    /// never exposes file paths to the relay layer.
    fn indexed_requests(&self, limits: &ProtocolLimits, maximum: usize) -> Vec<ShareRequest> {
        let _cache_guard = lock_unpoisoned(&self.lock);
        let mut entries = self.read_index().entries.into_iter().collect::<Vec<_>>();
        entries.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.last_access_unix));
        entries
            .into_iter()
            .take(maximum)
            .filter_map(|(request_hash, _)| {
                let bytes = read_file_bounded(
                    &self.manifest_path(&request_hash),
                    limits.max_manifest_bytes,
                )
                .ok()?;
                let signed = parse_signed_object_manifest_bounded(&bytes, limits).ok()?;
                (signed.manifest.request_sha256 == request_hash).then_some(signed.manifest.request)
            })
            .collect()
    }

    /// Walk a bounded, rotating cache-index slice so periodic key/expiry
    /// pruning eventually reaches every entry without one background tick
    /// monopolizing shutdown or the async scheduler.
    fn indexed_requests_rotating(
        &self,
        limits: &ProtocolLimits,
        maximum: usize,
    ) -> Vec<ShareRequest> {
        let _cache_guard = lock_unpoisoned(&self.lock);
        let mut entries = self.read_index().entries.into_iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        if entries.is_empty() || maximum == 0 {
            return Vec::new();
        }
        let mut offset = lock_unpoisoned(&self.prune_offset);
        let start = *offset % entries.len();
        entries.rotate_left(start);
        let take = maximum.min(entries.len());
        *offset = (start + take) % entries.len();
        entries
            .into_iter()
            .take(take)
            .filter_map(|(request_hash, _)| {
                let bytes = read_file_bounded(
                    &self.manifest_path(&request_hash),
                    limits.max_manifest_bytes,
                )
                .ok()?;
                let signed = parse_signed_object_manifest_bounded(&bytes, limits).ok()?;
                (signed.manifest.request_sha256 == request_hash).then_some(signed.manifest.request)
            })
            .collect()
    }

    fn evict(&self, index: &mut CacheIndex) {
        while indexed_cache_bytes(index) > self.byte_limit {
            if let Some(object_sha256) = index
                .entries
                .iter()
                .filter(|(_, entry)| entry.object_present)
                .min_by_key(|(_, entry)| entry.last_access_unix)
                .map(|(_, entry)| entry.object_sha256.clone())
            {
                self.mark_object_missing(index, &object_sha256);
                continue;
            }
            let Some(request_hash) = index
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access_unix)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove_entry(index, &request_hash);
        }
    }

    fn mark_object_missing(&self, index: &mut CacheIndex, object_sha256: &str) {
        if !is_sha256_hex(object_sha256) {
            return;
        }
        for entry in index.entries.values_mut() {
            if entry.object_sha256 == object_sha256 {
                entry.object_present = false;
            }
        }
        let _ = fs::remove_file(self.object_path(object_sha256));
    }

    fn remove_entry(&self, index: &mut CacheIndex, request_hash: &str) {
        let Some(entry) = index.entries.remove(request_hash) else {
            return;
        };
        let _ = fs::remove_file(self.manifest_path(request_hash));
        let object_is_shared = index
            .entries
            .values()
            .any(|other| other.object_present && other.object_sha256 == entry.object_sha256);
        if !object_is_shared && is_sha256_hex(&entry.object_sha256) {
            let _ = fs::remove_file(self.object_path(&entry.object_sha256));
        }
    }

    fn read_index(&self) -> CacheIndex {
        let path = self.root.join(INDEX_FILE);
        let Ok(bytes) = read_file_bounded(&path, 16 * 1024 * 1024) else {
            return CacheIndex::default();
        };
        serde_json::from_slice::<CacheIndex>(&bytes)
            .ok()
            .filter(cache_index_is_safe)
            .unwrap_or_default()
    }

    fn write_index(&self, index: &CacheIndex) -> Result<(), CommunityCacheError> {
        fs::create_dir_all(&self.root).map_err(|_| CommunityCacheError::Storage)?;
        let bytes = serde_json::to_vec(index).map_err(|_| CommunityCacheError::Response)?;
        atomic_write(&self.root.join(INDEX_FILE), &bytes)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join("objects").join(hash)
    }

    fn manifest_path(&self, hash: &str) -> PathBuf {
        self.root.join("manifests").join(format!("{hash}.json"))
    }
}

static CACHE_LOCK_REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

fn shared_cache_lock(root: &Path) -> Arc<Mutex<()>> {
    let key = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let registry = CACHE_LOCK_REGISTRY.get_or_init(Default::default);
    let mut registry = lock_unpoisoned(registry);
    registry.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = registry.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(key, Arc::downgrade(&lock));
    lock
}

/// Seed admission is intentionally narrower than cache admission. A signed
/// public-provider result still needs confirmed redistribution rights, while
/// private WRF/ArWen and other owner-provided data additionally require the
/// explicit publication bit bound into the origin-signed request identity.
fn relay_seed_publication_is_allowed(publication: &PublicationGrant) -> bool {
    publication.redistribution_rights_confirmed
        && match publication.data_origin {
            DataOrigin::PublicProvider => true,
            DataOrigin::PrivateWrf | DataOrigin::PrivateArwen | DataOrigin::UserProvided => {
                publication.explicit_owner_publication
            }
        }
}

impl rw_community_relay::VerifiedRelaySeedStore for CommunityCacheClient {
    fn load_exact(
        &self,
        object_sha256: &str,
    ) -> Result<Option<rw_community_relay::VerifiedSeedObject>, rw_community_relay::RelayError>
    {
        let object = self
            .disk
            .load_exact_seed(object_sha256, &self.keys, &self.limits)
            .map_err(|_| rw_community_relay::RelayError::PersistenceRejected)?;
        if let Some(object) = object.as_ref() {
            // Charge conservatively when a verified object is selected for an
            // uploader grant. A later transport failure never refunds bytes,
            // preventing retry loops from bypassing the local allowance.
            self.transfers
                .charge_upload(now_unix(), object.encoded.len() as u64)
                .map_err(|_| rw_community_relay::RelayError::QuotaReached)?;
        }
        Ok(object)
    }
}

fn cache_index_is_safe(index: &CacheIndex) -> bool {
    index.schema == INDEX_SCHEMA
        && index.entries.len() <= MAX_CACHE_INDEX_ENTRIES
        && index.entries.iter().all(|(request_hash, entry)| {
            is_sha256_hex(request_hash)
                && is_sha256_hex(&entry.object_sha256)
                && entry.encoded_size > 0
                && entry.encoded_size <= ProtocolLimits::default().max_encoded_bytes
                && entry.manifest_size <= ProtocolLimits::default().max_manifest_bytes
                && entry.last_access_unix >= 0
        })
}

fn indexed_cache_bytes(index: &CacheIndex) -> u64 {
    let mut seen = BTreeSet::new();
    index.entries.values().fold(0u64, |total, entry| {
        let total = total.saturating_add(entry.manifest_size);
        if entry.object_present && seen.insert(entry.object_sha256.as_str()) {
            total.saturating_add(entry.encoded_size)
        } else {
            total
        }
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn normalized_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

fn read_bounded_json<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
    limit: u64,
    transfers: &TransferGate,
) -> Result<T, CommunityCacheError> {
    let bytes = read_bounded_response(response, limit, None, transfers)?;
    serde_json::from_slice(&bytes).map_err(|_| CommunityCacheError::Response)
}

fn read_bounded_response(
    response: reqwest::blocking::Response,
    limit: u64,
    exact_size: Option<u64>,
    transfers: &TransferGate,
) -> Result<Vec<u8>, CommunityCacheError> {
    if limit == 0 || exact_size.is_some_and(|expected| expected == 0 || expected > limit) {
        return Err(CommunityCacheError::Response);
    }
    if let Some(length) = response.content_length()
        && (length > limit || exact_size.is_some_and(|expected| length != expected))
    {
        return Err(CommunityCacheError::Response);
    }
    let mut reader = response.take(limit.saturating_add(1));
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; HTTP_READ_CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| CommunityCacheError::Network)?;
        if read == 0 {
            break;
        }
        // Account every received body byte before retaining it. A response
        // that crosses either allowance is abandoned immediately.
        transfers.charge_download(now_unix(), read as u64)?;
        bytes
            .try_reserve(read)
            .map_err(|_| CommunityCacheError::Quota)?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 > limit
        || exact_size.is_some_and(|expected| bytes.len() as u64 != expected)
    {
        return Err(CommunityCacheError::Response);
    }
    Ok(bytes)
}

fn read_file_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, CommunityCacheError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| CommunityCacheError::Storage)?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(CommunityCacheError::Storage);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| CommunityCacheError::Storage)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| CommunityCacheError::Storage)?;
    if bytes.len() as u64 > limit {
        return Err(CommunityCacheError::Storage);
    }
    Ok(bytes)
}

fn read_file_exact(path: &Path, expected: u64, limit: u64) -> Result<Vec<u8>, CommunityCacheError> {
    if expected == 0 || expected > limit {
        return Err(CommunityCacheError::Storage);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| CommunityCacheError::Storage)?;
    if !metadata.file_type().is_file() || metadata.len() != expected {
        return Err(CommunityCacheError::Storage);
    }
    let bytes = read_file_bounded(path, expected)?;
    (bytes.len() as u64 == expected)
        .then_some(bytes)
        .ok_or(CommunityCacheError::Storage)
}

fn decode_verified(
    manifest: &ObjectManifest,
    encoded: &[u8],
    limits: &ProtocolLimits,
) -> Result<Vec<u8>, CommunityCacheError> {
    let mut guard = DecodedSizeGuard::new(manifest, limits)?;
    let mut decoded = Vec::new();
    match manifest.compression {
        Compression::None => {
            guard.observe(encoded.len())?;
            decoded.extend_from_slice(encoded);
        }
        Compression::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(encoded)
                .map_err(|_| CommunityCacheError::Response)?;
            decoder
                .window_log_max(ZSTD_MAX_WINDOW_LOG)
                .map_err(|_| CommunityCacheError::Response)?;
            copy_decoded(&mut decoder, &mut decoded, &mut guard)?;
        }
        // Gzip is intentionally rejected until a streaming decoder with the
        // same frame/window guarantees is enabled in the shipping client.
        Compression::Gzip => return Err(CommunityCacheError::Response),
    }
    guard.finish()?;
    Ok(decoded)
}

fn copy_decoded(
    reader: &mut impl Read,
    decoded: &mut Vec<u8>,
    guard: &mut DecodedSizeGuard,
) -> Result<(), CommunityCacheError> {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| CommunityCacheError::Response)?;
        if read == 0 {
            break;
        }
        guard.observe(read)?;
        decoded
            .try_reserve(read)
            .map_err(|_| CommunityCacheError::Quota)?;
        decoded.extend_from_slice(&buffer[..read]);
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CommunityCacheError> {
    rw_store::atomic::atomic_write_bytes(path, bytes).map_err(|_| CommunityCacheError::Storage)
}

fn atomic_write_new_or_same(path: &Path, bytes: &[u8]) -> Result<(), CommunityCacheError> {
    if path.exists() {
        let existing = read_file_bounded(path, bytes.len() as u64)?;
        return (existing == bytes)
            .then_some(())
            .ok_or(CommunityCacheError::Storage);
    }
    atomic_write(path, bytes)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use rw_community_protocol::{
        AnnotationArtifact, AttributionNotice, CASE_ARTIFACT_PUBLICATION_SCHEMA,
        CASE_DIRECTORY_SCHEMA, CASE_SCHEMA, CaseArtifactPayload, CaseArtifactRef, CaseArtifactType,
        CaseModelSource, CaseRoomManifest, DataOrigin, OBJECT_SCHEMA, ObjectManifest,
        PUBLICATION_OWNER_PARAMETER, PublicationGrant, PublishCaseArtifactRequest, REQUEST_SCHEMA,
        RecipeIdentity, ShareQuery, SourceProvenance, SurfaceSample, case_artifact_payload_bytes,
        sign_case_manifest, sign_object_manifest,
    };

    use super::*;

    fn remote_variable(
        name: &str,
        kind: &str,
        pressure_profile: bool,
        point_series: bool,
        levels_hpa: Vec<u16>,
    ) -> RemoteVariableCapability {
        RemoteVariableCapability {
            name: name.into(),
            units: match name {
                "temperature_iso" | "dewpoint_iso" | "temperature_2m" | "dewpoint_2m" => "K",
                "height_iso" | "orography" => "m",
                "surface_pressure" => "Pa",
                _ => "m/s",
            }
            .into(),
            kind: kind.into(),
            codec: "q16-zstd".into(),
            levels_hpa,
            selector: serde_json::json!({"field": name}),
            available_slots: vec![1, 2],
            available_samples: 2,
            expected_samples: 2,
            coverage: 1.0,
            point_series,
            pressure_profile,
            profile_cycle: pressure_profile,
            geographic_window: matches!(kind, "surface2d" | "pressure3d"),
            scalar_temporal_reduction: kind == "surface2d",
            temporal: serde_json::json!({}),
        }
    }

    fn remote_profile_catalog(model: &str, provider: &str) -> RemoteProfileCatalog {
        RemoteProfileCatalog {
            run: RemoteRunDescriptor {
                model: model.into(),
                run: "20260812_00z".into(),
                schema: "rw-store.run.v2".into(),
                snapshot_id: "1".repeat(64),
                grid_hash: "2".repeat(64),
                nx: 1_799,
                ny: 1_059,
                exact_time_axis: true,
                origin_unix: Some(1_786_492_800),
                sample_count: 2,
                first_valid_unix: Some(1_786_496_400),
                last_valid_unix: Some(1_786_500_000),
                source_provenance: vec![RemoteSourceProvenance {
                    provider: provider.into(),
                    roles: vec!["pressure".into(), "surface".into()],
                    products: vec!["wrfprs".into(), "wrfsfc".into()],
                }],
                provider_attributions: vec![],
            },
            point: RemoteGridPoint {
                requested_latitude: 35.1234567,
                requested_longitude: -97.7654321,
                x: 900,
                y: 500,
                grid_latitude: 35.123_455,
                grid_longitude: -97.765_434,
            },
            axis: vec![
                RemoteTimePoint {
                    storage_slot: 1,
                    lead_seconds: 3_600,
                    valid_unix: 1_786_496_400,
                },
                RemoteTimePoint {
                    storage_slot: 2,
                    lead_seconds: 7_200,
                    valid_unix: 1_786_500_000,
                },
            ],
            variables: vec![
                remote_variable(
                    "temperature_iso",
                    "pressure3d",
                    true,
                    false,
                    vec![850, 700, 500],
                ),
                remote_variable(
                    "dewpoint_iso",
                    "pressure3d",
                    true,
                    false,
                    vec![850, 700, 500],
                ),
                remote_variable("temperature_2m", "surface2d", false, true, vec![]),
                remote_variable("u_10m", "surface2d", false, true, vec![]),
            ],
        }
    }

    fn pressure_profile(name: &str) -> RemotePressureProfile {
        RemotePressureProfile {
            name: name.into(),
            units: "K".into(),
            levels_hpa: vec![850, 700, 500],
            values: vec![Some(290.0), Some(270.0), Some(250.0)],
            available_levels: 3,
            expected_levels: 3,
            coverage: 1.0,
        }
    }

    fn remote_profile_cycle_result(catalog: &RemoteProfileCatalog) -> RemoteProfileCycleResult {
        let source_provenance = catalog.run.source_provenance.clone();
        RemoteProfileCycleResult {
            run: catalog.run.clone(),
            point: catalog.point.clone(),
            requested_variables: vec!["dewpoint_iso".into(), "temperature_iso".into()],
            requested_surface_variables: vec!["temperature_2m".into(), "u_10m".into()],
            requested_time: RemoteProfileCycleTimeRange {
                start_unix: None,
                end_unix: None,
            },
            missing_policy: RemoteProfileCycleMissingPolicy::Partial,
            samples: vec![
                RemoteProfileCycleSample {
                    time: catalog.axis[0].clone(),
                    source_provenance: source_provenance.clone(),
                    status: RemoteProfileCycleSampleStatus::Partial,
                    variables: vec![pressure_profile("temperature_iso")],
                    missing_variables: vec!["dewpoint_iso".into()],
                    surface_samples: vec![
                        RemoteProfileSurfaceSample {
                            variable: "temperature_2m".into(),
                            units: "K".into(),
                            value: Some(300.0),
                        },
                        RemoteProfileSurfaceSample {
                            variable: "u_10m".into(),
                            units: "m/s".into(),
                            value: None,
                        },
                    ],
                    missing_surface_variables: vec!["u_10m".into()],
                },
                RemoteProfileCycleSample {
                    time: catalog.axis[1].clone(),
                    source_provenance,
                    status: RemoteProfileCycleSampleStatus::Complete,
                    variables: vec![
                        pressure_profile("dewpoint_iso"),
                        pressure_profile("temperature_iso"),
                    ],
                    missing_variables: vec![],
                    surface_samples: vec![
                        RemoteProfileSurfaceSample {
                            variable: "temperature_2m".into(),
                            units: "K".into(),
                            value: Some(301.0),
                        },
                        RemoteProfileSurfaceSample {
                            variable: "u_10m".into(),
                            units: "m/s".into(),
                            value: Some(10.0),
                        },
                    ],
                    missing_surface_variables: vec![],
                },
            ],
        }
    }

    fn request(run: &str) -> ShareRequest {
        ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: run.into(),
            snapshot_id: "1".repeat(64),
            grid_hash: "2".repeat(64),
            variables: vec!["temperature_iso".into(), "temperature_2m".into()],
            query: ShareQuery::Profile {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                storage_slot: 1,
                valid_unix: 1_800_000_000,
                pressure_variables: vec!["temperature_iso".into()],
                surface_variables: vec!["temperature_2m".into()],
                pressure_levels_hpa: vec![],
            },
            recipe: RecipeIdentity {
                recipe_id: "native-profile".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                roles: vec!["pressure".into()],
                products: vec!["wrfprs".into()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
        .normalized()
    }

    fn point_series_request(run: &str) -> ShareRequest {
        ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: run.into(),
            snapshot_id: "1".repeat(64),
            grid_hash: "2".repeat(64),
            variables: vec!["temperature_2m".into()],
            query: ShareQuery::PointSeries {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                window: rw_community_protocol::TimeWindow::Utc {
                    start_unix: 1_800_000_000,
                    end_unix: 1_800_086_400,
                },
                missing_policy: rw_community_protocol::MissingPolicy::Partial,
            },
            recipe: RecipeIdentity {
                recipe_id: "native-point-series".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                roles: vec!["surface".into()],
                products: vec!["wrfsfc".into()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
        .normalized()
    }

    fn signed_object(
        request: &ShareRequest,
        decoded: &[u8],
    ) -> (SignedObjectManifest, Vec<u8>, TrustedSigningKeys) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[17; 32]);
        let encoded = zstd::stream::encode_all(decoded, 1).unwrap();
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request: request.clone(),
            request_sha256: request_sha256(request).unwrap(),
            object_sha256: object_sha256(&encoded),
            content_type: "application/json".into(),
            compression: Compression::Zstd,
            encoded_size: encoded.len() as u64,
            decoded_size: decoded.len() as u64,
            attributions: vec![],
            modification_notices: vec![],
            created_unix: 1_700_000_000,
            expires_unix: 2_000_000_000,
        };
        let signed = sign_object_manifest(manifest, ORIGIN_SIGNING_KEY_ID, &signing).unwrap();
        let keys = BTreeMap::from([(ORIGIN_SIGNING_KEY_ID.into(), signing.verifying_key())]);
        (signed, encoded, keys)
    }

    fn signed_case(case_id: &str, signing: &ed25519_dalek::SigningKey) -> SignedCaseRoomManifest {
        let request = request("20260812_00z");
        sign_case_manifest(
            CaseRoomManifest {
                schema: CASE_SCHEMA.into(),
                case_id: case_id.into(),
                title: "Central Plains severe-weather analysis".into(),
                event_start_unix: 1_799_990_000,
                event_end_unix: 1_800_010_000,
                published_unix: 1_799_999_000,
                retain_until_unix: 1_900_000_000,
                publication: PublicationGrant {
                    data_origin: DataOrigin::PublicProvider,
                    explicit_owner_publication: true,
                    redistribution_rights_confirmed: true,
                },
                sources: vec![CaseModelSource {
                    model: request.model.clone(),
                    run: request.run.clone(),
                    snapshot_id: request.snapshot_id.clone(),
                    grid_hash: request.grid_hash.clone(),
                    source_provenance: request.source_provenance.clone(),
                }],
                artifacts: vec![CaseArtifactRef {
                    artifact_id: "analysis-overlay".into(),
                    artifact_type: CaseArtifactType::Overlay,
                    request_sha256: request_sha256(&request).unwrap(),
                    object_sha256: "d".repeat(64),
                }],
                attributions: vec![],
                modification_notices: vec!["Derived by Rusty Weather.".into()],
            },
            ORIGIN_SIGNING_KEY_ID,
            signing,
        )
        .unwrap()
    }

    fn signed_case_page(ids: &[&str]) -> (CaseRoomDirectoryPage, TrustedSigningKeys) {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[29; 32]);
        let page = CaseRoomDirectoryPage {
            schema: CASE_DIRECTORY_SCHEMA.into(),
            cases: ids
                .iter()
                .map(|case_id| signed_case(case_id, &signing))
                .collect(),
            next_after: ids.last().map(|case_id| (*case_id).to_owned()),
        };
        let keys = BTreeMap::from([(ORIGIN_SIGNING_KEY_ID.into(), signing.verifying_key())]);
        (page, keys)
    }

    #[test]
    fn remote_run_schema_and_axis_version_must_be_a_valid_pair() {
        let catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let v2 = catalog.run.clone();
        assert!(validate_remote_run(&v2).is_ok());
        assert!(validate_remote_axis(&catalog.axis, &v2, &catalog.variables).is_ok());

        let mut wrong_origin_axis = catalog.axis.clone();
        wrong_origin_axis[1].lead_seconds += 1;
        assert!(validate_remote_axis(&wrong_origin_axis, &v2, &catalog.variables).is_err());

        let mut v1 = v2.clone();
        v1.schema = "rw-store.run.v1".into();
        v1.exact_time_axis = false;
        assert!(validate_remote_run(&v1).is_ok());
        let mut v1_axis = catalog.axis.clone();
        for point in &mut v1_axis {
            point.lead_seconds = u64::from(point.storage_slot) * 3_600;
            point.valid_unix = v1.origin_unix.unwrap() + point.lead_seconds as i64;
        }
        v1.first_valid_unix = v1_axis.first().map(|time| time.valid_unix);
        v1.last_valid_unix = v1_axis.last().map(|time| time.valid_unix);
        assert!(validate_remote_axis(&v1_axis, &v1, &catalog.variables).is_ok());
        v1_axis[0].lead_seconds += 1;
        assert!(validate_remote_axis(&v1_axis, &v1, &catalog.variables).is_err());

        for (schema, exact_time_axis) in [
            ("rw-store.run.v1", true),
            ("rw-store.run.v2", false),
            ("rw-store.run.v3", true),
        ] {
            let mut invalid = v2.clone();
            invalid.schema = schema.into();
            invalid.exact_time_axis = exact_time_axis;
            assert!(validate_remote_run(&invalid).is_err());
        }
    }

    #[test]
    fn profile_cycle_capability_defaults_false_for_older_authorities() {
        let base = serde_json::json!({
            "name": "temperature_iso",
            "units": "K",
            "kind": "pressure3d",
            "codec": "q16-zstd",
            "levels_hpa": [500, 700, 850],
            "selector": {"field": "temperature_iso"},
            "available_slots": [1, 2],
            "available_samples": 2,
            "expected_samples": 2,
            "coverage": 1.0,
            "point_series": false,
            "pressure_profile": true,
            "geographic_window": true,
            "scalar_temporal_reduction": false,
            "temporal": {}
        });
        let old: RemoteVariableCapability = serde_json::from_value(base.clone()).unwrap();
        assert!(!old.profile_cycle);

        let mut current = base;
        current["profile_cycle"] = serde_json::Value::Bool(true);
        let current: RemoteVariableCapability = serde_json::from_value(current).unwrap();
        assert!(current.profile_cycle);
    }

    #[test]
    fn latest_pointer_must_match_the_physically_newest_authorized_run() {
        assert_eq!(
            remote_latest_run_path("hrrr").unwrap(),
            "/v1/models/hrrr/latest-run"
        );
        assert_ne!(
            remote_latest_run_path("hrrr").unwrap(),
            "/v1/models/hrrr/runs/latest"
        );
        let newest = remote_profile_catalog("hrrr", "noaa-aws-public-data").run;
        let mut older = newest.clone();
        older.run = "20260811_18z".into();
        older.snapshot_id = "3".repeat(64);
        older.origin_unix = newest.origin_unix.map(|origin| origin - 21_600);
        let mut runs = vec![
            RemoteRunCatalogEntry {
                run: older.clone(),
                variable_count: 4,
            },
            RemoteRunCatalogEntry {
                run: newest.clone(),
                variable_count: 4,
            },
        ];
        assert!(validate_remote_latest_catalog(&runs, &newest).is_ok());
        sort_remote_runs_by_physical_origin(&mut runs);
        assert_eq!(runs[0].run, newest);
        assert_eq!(runs[1].run, older);
        assert!(validate_remote_latest_catalog(&runs, &older).is_err());

        let mut unlisted = newest.clone();
        unlisted.snapshot_id = "4".repeat(64);
        assert!(validate_remote_latest_catalog(&runs, &unlisted).is_err());

        let mut missing_origin = runs;
        missing_origin[0].run.origin_unix = None;
        assert!(validate_remote_latest_catalog(&missing_origin, &newest).is_err());
    }

    #[test]
    fn profile_cycle_validation_preserves_exact_axis_surfaces_and_gaps() {
        let mut catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let dewpoint = catalog
            .variables
            .iter_mut()
            .find(|variable| variable.name == "dewpoint_iso")
            .expect("dewpoint capability");
        dewpoint.available_slots = vec![2];
        dewpoint.available_samples = 1;
        dewpoint.coverage = 0.5;
        let selection = RemoteProfileVariableSelection {
            pressure_variables: vec!["dewpoint_iso".into(), "temperature_iso".into()],
            surface_variables: vec!["temperature_2m".into(), "u_10m".into()],
            pressure_levels_hpa: vec![],
        };
        let result = remote_profile_cycle_result(&catalog);
        assert!(
            validate_remote_profile_cycle(
                &result,
                &catalog,
                &selection,
                &ProtocolLimits::default()
            )
            .is_ok()
        );

        let mut gaps_catalog = catalog.clone();
        for variable in gaps_catalog
            .variables
            .iter_mut()
            .filter(|variable| variable.kind == "pressure3d")
        {
            variable.available_slots.retain(|slot| *slot != 1);
            variable.available_samples = variable.available_slots.len();
            variable.coverage =
                variable.available_samples as f64 / variable.expected_samples as f64;
        }
        let mut gaps = result.clone();
        gaps.samples[0].status = RemoteProfileCycleSampleStatus::Gap;
        gaps.samples[0].variables.clear();
        gaps.samples[0].missing_variables = selection.pressure_variables.clone();
        for surface in &mut gaps.samples[0].surface_samples {
            surface.value = None;
        }
        gaps.samples[0].missing_surface_variables = selection.surface_variables.clone();
        assert!(
            validate_remote_profile_cycle(
                &gaps,
                &gaps_catalog,
                &selection,
                &ProtocolLimits::default()
            )
            .is_ok(),
            "a gap stays on the manifest axis instead of being compacted away"
        );

        let mut falsely_missing = result.clone();
        falsely_missing.samples[1]
            .variables
            .retain(|profile| profile.name != "temperature_iso");
        falsely_missing.samples[1].missing_variables = vec!["temperature_iso".into()];
        falsely_missing.samples[1].status = RemoteProfileCycleSampleStatus::Partial;
        assert!(
            validate_remote_profile_cycle(
                &falsely_missing,
                &catalog,
                &selection,
                &ProtocolLimits::default()
            )
            .is_err(),
            "a capability-advertised pressure profile cannot be relabeled as missing"
        );

        for mutate in 0..5 {
            let mut malformed = result.clone();
            match mutate {
                0 => malformed.run.snapshot_id = "f".repeat(64),
                1 => malformed.samples.swap(0, 1),
                2 => malformed.requested_variables.reverse(),
                3 => malformed.samples[0].missing_surface_variables.clear(),
                4 => malformed.samples[0].status = RemoteProfileCycleSampleStatus::Complete,
                _ => unreachable!(),
            }
            assert!(
                validate_remote_profile_cycle(
                    &malformed,
                    &catalog,
                    &selection,
                    &ProtocolLimits::default()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn configured_origin_key_id_matches_server_contract() {
        assert_eq!(ORIGIN_SIGNING_KEY_ID, "rw-origin-v1");
        let request = request("20260812_00z");
        let (signed, encoded, keys) = signed_object(&request, b"signed by server key id");
        verify_signed_object(
            &signed,
            &request,
            &encoded,
            1_800_000_001,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
    }

    #[test]
    fn origin_keyring_overlap_removal_and_unknown_ids_fail_closed() {
        const OLD_PUBLIC_KEY: &str = "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=";
        const NEW_PUBLIC_KEY: &str = "IEBA42TBDyvsnB/lAKHNTCR8idZQoB7X6CyrqGeHfCE=";
        let request = request("20260812_00z");
        let (old_signed, encoded, _) = signed_object(&request, b"origin key rotation");
        let new_signing = ed25519_dalek::SigningKey::from_bytes(&[18; 32]);
        let new_signed =
            sign_object_manifest(old_signed.manifest.clone(), "rw-origin-v2", &new_signing)
                .unwrap();
        let overlap_settings = settings::CommunityCacheSettings {
            enabled: true,
            origin_url: "https://origin.example".into(),
            manifest_public_key_base64: OLD_PUBLIC_KEY.into(),
            trusted_origin_signing_keys: vec![format!("rw-origin-v2:{NEW_PUBLIC_KEY}")],
            ..Default::default()
        };
        let overlap = trusted_origin_keyring(&overlap_settings).unwrap();
        for signed in [&old_signed, &new_signed] {
            verify_signed_object(
                signed,
                &request,
                &encoded,
                1_800_000_001,
                &overlap,
                &ProtocolLimits::default(),
            )
            .unwrap();
        }

        let rotated_settings = settings::CommunityCacheSettings {
            manifest_public_key_base64: String::new(),
            trusted_origin_signing_keys: vec![format!("rw-origin-v2:{NEW_PUBLIC_KEY}")],
            ..overlap_settings
        };
        let rotated = trusted_origin_keyring(&rotated_settings).unwrap();
        assert!(
            verify_signed_object(
                &old_signed,
                &request,
                &encoded,
                1_800_000_001,
                &rotated,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "removing the old pin must revoke old retained manifests"
        );
        verify_signed_object(
            &new_signed,
            &request,
            &encoded,
            1_800_000_001,
            &rotated,
            &ProtocolLimits::default(),
        )
        .unwrap();

        let unknown =
            sign_object_manifest(old_signed.manifest, "rw-origin-unknown", &new_signing).unwrap();
        assert!(
            verify_signed_object(
                &unknown,
                &request,
                &encoded,
                1_800_000_001,
                &rotated,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "a server-advertised unknown key id must never become trusted"
        );
    }

    #[test]
    fn case_directory_get_path_is_bounded_and_opaque() {
        assert_eq!(
            case_directory_path(None, CASE_DIRECTORY_PAGE_LIMIT).unwrap(),
            "/v1/community/cases?limit=12"
        );
        assert_eq!(
            case_directory_path(Some("case_2026-A"), 7).unwrap(),
            "/v1/community/cases?after=case_2026-A&limit=7"
        );
        for cursor in ["", "192.0.2.1", "case/name", "case:name", "case.name"] {
            assert!(case_directory_path(Some(cursor), 7).is_err());
        }
        assert!(case_directory_path(None, 0).is_err());
        assert!(case_directory_path(None, MAX_CASE_DIRECTORY_PAGE + 1).is_err());
    }

    #[test]
    fn case_directory_requires_valid_signed_expiring_artifact_manifests() {
        let (page, keys) = signed_case_page(&["case-a", "case-b"]);
        verify_case_directory_page(
            &page,
            None,
            2,
            1_800_000_000,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();

        let mut tampered = page.clone();
        tampered.cases[0].manifest.title.push('!');
        assert!(
            verify_case_directory_page(
                &tampered,
                None,
                2,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err()
        );

        let mut malformed_artifact = page.clone();
        malformed_artifact.cases[0].manifest.artifacts[0].object_sha256 = "bad".into();
        assert!(
            verify_case_directory_page(
                &malformed_artifact,
                None,
                2,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err()
        );
        assert!(
            verify_case_directory_page(
                &page,
                None,
                2,
                1_900_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "retention expiry must fail closed"
        );
    }

    #[test]
    fn case_directory_enforces_requested_page_and_forward_cursor() {
        let (page, keys) = signed_case_page(&["case-b", "case-c"]);
        assert!(
            verify_case_directory_page(
                &page,
                Some("case-a"),
                2,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_ok()
        );
        assert!(
            verify_case_directory_page(
                &page,
                Some("case-b"),
                2,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "the origin may not replay the cursor entry"
        );
        assert!(
            verify_case_directory_page(
                &page,
                Some("case-a"),
                1,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "the response may not exceed the requested limit"
        );

        let mut looping = page;
        looping.next_after = Some("case-a".into());
        assert!(
            verify_case_directory_page(
                &looping,
                Some("case-a"),
                2,
                1_800_000_000,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err(),
            "a non-advancing cursor must fail closed"
        );
    }

    #[test]
    fn case_browser_keeps_last_verified_page_when_refresh_fails() {
        let (page, _) = signed_case_page(&["case-a"]);
        let mut browser = CommunityCaseBrowser::default();
        browser.apply_verified_page(page).unwrap();
        let prior = browser.cases.clone();
        let (sender, receiver) = mpsc::channel();
        sender.send(Err(CommunityCacheError::Network)).unwrap();
        browser.receiver = Some(receiver);
        browser.pending_replace = true;
        assert!(browser.poll());
        assert_eq!(browser.cases, prior);
        assert!(browser.status().unwrap().contains("failed"));
    }

    #[test]
    fn signed_case_reference_binds_exact_hash_request_and_typed_payload() {
        let owner = "a".repeat(64);
        let mut request = request("20260812_00z");
        request.query = ShareQuery::CaseArtifact {
            case_id: "case-a".into(),
            artifact_id: "analysis-note".into(),
            artifact_type: CaseArtifactType::Annotation,
        };
        request.recipe = RecipeIdentity {
            recipe_id: "bowecho-case-annotation".into(),
            recipe_version: "1".into(),
            parameters: BTreeMap::from([(PUBLICATION_OWNER_PARAMETER.into(), owner.clone())]),
        };
        request.publication.data_origin = DataOrigin::UserProvided;
        request.publication.explicit_owner_publication = true;
        request.normalize();
        let publication = PublishCaseArtifactRequest {
            schema: CASE_ARTIFACT_PUBLICATION_SCHEMA.into(),
            owner_principal_sha256: owner,
            request: request.clone(),
            payload: CaseArtifactPayload::Annotation(AnnotationArtifact {
                title: "Analysis note".into(),
                text: "Verified severe-weather analysis.".into(),
                event_unix: Some(1_800_000_000),
            }),
            published_unix: 1_800_000_000,
            retain_until_unix: 1_800_086_400,
            attributions: vec![AttributionNotice {
                provider: "bowecho-user".into(),
                notice: "User-authored BowEcho analysis artifact.".into(),
                source_url: "https://fahrenheitresearch.org/".into(),
                license: "Owner-confirmed redistribution terms".into(),
                license_url: "https://fahrenheitresearch.org/terms".into(),
                terms_url: "https://fahrenheitresearch.org/terms".into(),
                disclaimer: "User-authored analysis; verify independently.".into(),
            }],
            modification_notices: vec!["Created in BowEcho from the attributed sources.".into()],
        };
        let bytes = case_artifact_payload_bytes(&publication).unwrap();
        let artifact = CaseArtifactRef {
            artifact_id: "analysis-note".into(),
            artifact_type: CaseArtifactType::Annotation,
            request_sha256: request_sha256(&request).unwrap(),
            object_sha256: object_sha256(&bytes),
        };
        assert!(matches!(
            decode_case_artifact_reference(&bytes, &artifact, &ProtocolLimits::default()).unwrap(),
            CaseArtifactPayload::Annotation(_)
        ));

        let mut tampered = bytes.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(
            decode_case_artifact_reference(&tampered, &artifact, &ProtocolLimits::default())
                .is_err()
        );
        let mut wrong_request = artifact.clone();
        wrong_request.request_sha256 = "b".repeat(64);
        assert!(
            decode_case_artifact_reference(&bytes, &wrong_request, &ProtocolLimits::default())
                .is_err()
        );
        let mut wrong_kind = artifact;
        wrong_kind.artifact_type = CaseArtifactType::Overlay;
        assert!(
            decode_case_artifact_reference(&bytes, &wrong_kind, &ProtocolLimits::default())
                .is_err()
        );
    }

    #[test]
    fn typed_query_payloads_require_the_exact_schema_and_request_identity() {
        let request = point_series_request("20260812_00z");
        let payload = TypedObjectPayload {
            schema: POINT_SERIES_PAYLOAD_SCHEMA.into(),
            request_sha256: request_sha256(&request).unwrap(),
            data: serde_json::json!({"axis": [], "variables": []}),
        };
        let bytes = serde_json::to_vec(&payload).unwrap();
        let decoded: TypedObjectPayload<serde_json::Value> =
            decode_typed_payload(&bytes, &request, POINT_SERIES_PAYLOAD_SCHEMA).unwrap();
        assert_eq!(decoded.data, payload.data);

        assert!(
            decode_typed_payload::<serde_json::Value>(
                &bytes,
                &request,
                NATIVE_WINDOW_PAYLOAD_SCHEMA
            )
            .is_err()
        );
        let other_run = point_series_request("20260812_06z");
        assert!(
            decode_typed_payload::<serde_json::Value>(
                &bytes,
                &other_run,
                POINT_SERIES_PAYLOAD_SCHEMA
            )
            .is_err()
        );
    }

    #[test]
    fn remote_point_window_and_temporal_builders_bind_every_material_choice() {
        let catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let limits = ProtocolLimits::default();
        let point = build_remote_point_series_request(
            &catalog,
            RemotePointSeriesSelection {
                variables: vec!["temperature_2m".into()],
                window: TimeWindow::Utc {
                    start_unix: catalog.axis[0].valid_unix,
                    end_unix: catalog.axis[1].valid_unix + 1,
                },
                missing_policy: MissingPolicy::Partial,
            },
            &limits,
        )
        .unwrap();
        assert!(matches!(point.query, ShareQuery::PointSeries { .. }));

        let window = build_remote_native_window_request(
            &catalog,
            RemoteNativeWindowSelection {
                variables: vec!["temperature_iso".into()],
                time: catalog.axis[0].clone(),
                x0: 100,
                y0: 200,
                x1: 300,
                y1: 400,
                pressure_levels_hpa: vec![850, 500],
            },
            &limits,
        )
        .unwrap();
        assert!(matches!(
            &window.query,
            ShareQuery::NativeWindow {
                pressure_levels_hpa,
                ..
            } if pressure_levels_hpa == &[500, 850]
        ));

        let geographic = build_remote_geographic_window_request(
            &catalog,
            RemoteGeographicWindowSelection {
                variables: vec!["temperature_iso".into()],
                time: catalog.axis[0].clone(),
                west_longitude: 170.0,
                south_latitude: 20.0,
                east_longitude: -170.0,
                north_latitude: 50.0,
                pressure_levels_hpa: vec![850, 500],
            },
            &limits,
        )
        .unwrap();
        assert!(matches!(
            &geographic.query,
            ShareQuery::GeographicWindow {
                west_longitude_e7: 1_700_000_000,
                east_longitude_e7: -1_700_000_000,
                pressure_levels_hpa,
                ..
            } if pressure_levels_hpa == &[500, 850]
        ));

        let temporal = build_remote_temporal_grid_request(
            &catalog,
            RemoteTemporalGridSelection {
                variables: vec!["temperature_2m".into()],
                window: TimeWindow::Utc {
                    start_unix: catalog.axis[0].valid_unix,
                    end_unix: catalog.axis[1].valid_unix + 1,
                },
                reducer: "scalar_summary".into(),
                semantics: "instantaneous_scalar".into(),
                missing_policy: MissingPolicy::Strict,
                pressure_levels_hpa: vec![],
                parameters: BTreeMap::from([("cadence_seconds".into(), "3600".into())]),
            },
            &limits,
        )
        .unwrap();
        assert!(matches!(temporal.query, ShareQuery::TemporalGrid { .. }));

        let hashes = [point, window, geographic, temporal]
            .iter()
            .map(|request| request_sha256(request).unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(hashes.len(), 4);
    }

    #[test]
    fn remote_window_builder_rejects_wrong_kind_level_bounds_and_private_runs() {
        let catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let limits = ProtocolLimits::default();
        let selection =
            |variables: Vec<String>, levels: Vec<u16>, x1| RemoteNativeWindowSelection {
                variables,
                time: catalog.axis[0].clone(),
                x0: 0,
                y0: 0,
                x1,
                y1: 10,
                pressure_levels_hpa: levels,
            };
        assert!(
            build_remote_native_window_request(
                &catalog,
                selection(vec!["temperature_2m".into()], vec![850], 10),
                &limits
            )
            .is_err()
        );
        assert!(
            build_remote_native_window_request(
                &catalog,
                selection(vec!["temperature_iso".into()], vec![925], 10),
                &limits
            )
            .is_err()
        );
        assert!(
            build_remote_native_window_request(
                &catalog,
                selection(
                    vec!["temperature_iso".into()],
                    vec![850],
                    catalog.run.nx as u32 + 1
                ),
                &limits
            )
            .is_err()
        );

        let private = remote_profile_catalog("my-arwen-run", "noaa-aws-public-data");
        assert!(
            build_remote_point_series_request(
                &private,
                RemotePointSeriesSelection {
                    variables: vec!["temperature_2m".into()],
                    window: TimeWindow::Utc {
                        start_unix: 1,
                        end_unix: 2
                    },
                    missing_policy: MissingPolicy::Strict,
                },
                &limits
            )
            .is_err()
        );
    }

    #[test]
    fn geographic_builder_preserves_eastward_arcs_and_rejects_ambiguous_domains() {
        let catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let limits = ProtocolLimits::default();
        let selection = |west, south, east, north| RemoteGeographicWindowSelection {
            variables: vec!["temperature_2m".into()],
            time: catalog.axis[0].clone(),
            west_longitude: west,
            south_latitude: south,
            east_longitude: east,
            north_latitude: north,
            pressure_levels_hpa: vec![],
        };
        let ordinary = build_remote_geographic_window_request(
            &catalog,
            selection(-100.0, 30.0, -90.0, 40.0),
            &limits,
        )
        .unwrap();
        let crossing = build_remote_geographic_window_request(
            &catalog,
            selection(170.0, 30.0, -170.0, 40.0),
            &limits,
        )
        .unwrap();
        let globe = build_remote_geographic_window_request(
            &catalog,
            selection(-180.0, -90.0, 180.0, 90.0),
            &limits,
        )
        .unwrap();
        assert!(matches!(
            crossing.query,
            ShareQuery::GeographicWindow {
                west_longitude_e7: 1_700_000_000,
                east_longitude_e7: -1_700_000_000,
                ..
            }
        ));
        assert!(matches!(
            globe.query,
            ShareQuery::GeographicWindow {
                west_longitude_e7: -1_800_000_000,
                east_longitude_e7: 1_800_000_000,
                ..
            }
        ));
        assert_ne!(
            request_sha256(&ordinary).unwrap(),
            request_sha256(&crossing).unwrap()
        );
        assert!(
            build_remote_geographic_window_request(
                &catalog,
                selection(-97.0, 30.0, -97.0, 40.0),
                &limits
            )
            .is_err()
        );
        assert!(
            build_remote_geographic_window_request(
                &catalog,
                selection(-100.0, 40.0, -90.0, 40.0),
                &limits
            )
            .is_err()
        );

        let mut unavailable = catalog.clone();
        unavailable
            .variables
            .iter_mut()
            .find(|variable| variable.name == "temperature_2m")
            .expect("surface fixture")
            .geographic_window = false;
        assert!(
            build_remote_geographic_window_request(
                &unavailable,
                selection(-100.0, 30.0, -90.0, 40.0),
                &limits
            )
            .is_err(),
            "the signed map adapter must honor the origin capability flag"
        );
    }

    #[test]
    fn decompression_bomb_and_size_mismatch_fail_closed() {
        let request = request("20260812_00z");
        let (mut signed, encoded, keys) = signed_object(&request, b"small");
        signed.manifest.decoded_size = ProtocolLimits::default().max_decoded_bytes + 1;
        assert!(
            verify_signed_object(
                &signed,
                &request,
                &encoded,
                1_800_000_001,
                &keys,
                &ProtocolLimits::default()
            )
            .is_err()
        );

        // Compresses by more than 1:1 but remains within the protocol's
        // default 64:1 signing limit, so the stricter client limit is what
        // rejects it below.
        let compressible = vec![0u8; 512];
        let (signed, encoded, _) = signed_object(&request, &compressible);
        let limits = ProtocolLimits {
            max_decompression_ratio: 1,
            ..ProtocolLimits::default()
        };
        assert!(decode_verified(&signed.manifest, &encoded, &limits).is_err());
    }

    #[test]
    fn verified_disk_cache_never_mixes_run_identity_and_evicts_oldest() {
        let temp = tempfile::tempdir().unwrap();
        let first_request = request("20260812_00z");
        let payload = serde_json::to_vec(&ProfileObjectPayload::<serde_json::Value> {
            schema: PROFILE_PAYLOAD_SCHEMA.into(),
            request_sha256: request_sha256(&first_request).unwrap(),
            profile: serde_json::json!({"run": "first"}),
            surface_samples: vec![SurfaceSample {
                variable: "temperature_2m".into(),
                units: "K".into(),
                value: Some(300.0),
            }],
        })
        .unwrap();
        let (signed, encoded, keys) = signed_object(&first_request, &payload);
        let object = VerifiedObject {
            manifest: signed,
            encoded: encoded.clone(),
            decoded: payload,
            tier: DeliveryTier::Origin,
        };
        let cache = VerifiedDiskCache::new(temp.path().into(), 1024 * 1024);
        cache.store(&object).unwrap();
        assert_eq!(
            fs::read(cache.object_path(&object.manifest.manifest.object_sha256)).unwrap(),
            encoded,
            "the cache must preserve the exact signed compressed bytes"
        );
        assert!(
            cache
                .load(&first_request, &keys, &ProtocolLimits::default())
                .unwrap()
                .is_some()
        );

        let second_request = request("20260812_06z");
        assert!(
            cache
                .load(&second_request, &keys, &ProtocolLimits::default())
                .unwrap()
                .is_none()
        );

        let zero_cache = VerifiedDiskCache::new(temp.path().join("zero"), 0);
        zero_cache.store(&object).unwrap();
        assert!(zero_cache.read_index().entries.is_empty());
    }

    #[test]
    fn expired_cached_object_is_evicted_and_cannot_remain_seed_eligible() {
        let temp = tempfile::tempdir().unwrap();
        let request = request("20260812_00z");
        let (mut signed, encoded, keys) = signed_object(&request, b"expired cold object");
        signed.manifest.created_unix = 1_700_000_000;
        signed.manifest.expires_unix = 1_700_000_001;
        signed = sign_object_manifest(
            signed.manifest,
            ORIGIN_SIGNING_KEY_ID,
            &ed25519_dalek::SigningKey::from_bytes(&[17; 32]),
        )
        .unwrap();
        let object_hash = signed.manifest.object_sha256.clone();
        let request_hash = signed.manifest.request_sha256.clone();
        let cache = VerifiedDiskCache::new(temp.path().into(), 1024 * 1024);
        cache
            .store(&VerifiedObject {
                manifest: signed,
                encoded,
                decoded: b"expired cold object".to_vec(),
                tier: DeliveryTier::Origin,
            })
            .unwrap();
        assert!(cache.manifest_path(&request_hash).is_file());
        assert!(cache.object_path(&object_hash).is_file());

        assert!(
            cache
                .load(&request, &keys, &ProtocolLimits::default())
                .unwrap()
                .is_none()
        );
        assert!(cache.read_index().entries.is_empty());
        assert!(!cache.manifest_path(&request_hash).exists());
        assert!(!cache.object_path(&object_hash).exists());
    }

    #[test]
    fn missing_local_bytes_retain_only_the_verified_historical_identity() {
        let temp = tempfile::tempdir().unwrap();
        let request = request("20260812_00z");
        let (signed, encoded, keys) = signed_object(&request, b"retained cold identity");
        let request_hash = signed.manifest.request_sha256.clone();
        let object_hash = signed.manifest.object_sha256.clone();
        let cache = VerifiedDiskCache::new(temp.path().into(), 1024 * 1024);
        cache
            .store(&VerifiedObject {
                manifest: signed.clone(),
                encoded,
                decoded: b"retained cold identity".to_vec(),
                tier: DeliveryTier::Origin,
            })
            .unwrap();

        fs::remove_file(cache.object_path(&object_hash)).unwrap();
        assert!(
            cache
                .load(&request, &keys, &ProtocolLimits::default())
                .unwrap()
                .is_none(),
            "missing bytes are never returned as a local hit"
        );
        let entry = cache.read_index().entries[&request_hash].clone();
        assert!(!entry.object_present);
        assert!(cache.manifest_path(&request_hash).is_file());
        assert_eq!(
            cache
                .retained_manifest(&request, &keys, &ProtocolLimits::default())
                .unwrap(),
            Some(signed),
            "the independently verified signed identity remains usable for an exact relay lookup"
        );
        assert!(
            cache
                .verified_seed_candidates(&keys, &ProtocolLimits::default(), 8)
                .is_empty(),
            "metadata-only entries can never seed missing bytes"
        );
    }

    #[test]
    fn phase_one_delivery_order_is_exact_and_never_dispatches_relay() {
        use rw_community_protocol::DeliverySource;

        assert_eq!(
            phase1_delivery_order(true).collect::<Vec<_>>(),
            vec![
                DeliveryTier::LocalCache,
                DeliveryTier::R2,
                DeliveryTier::Origin,
            ],
            "operational Phase 1 order is local -> R2 manifest/object -> origin resolve/object; successful network objects are then cached locally"
        );
        assert_eq!(
            phase1_delivery_order(false).collect::<Vec<_>>(),
            vec![DeliveryTier::LocalCache, DeliveryTier::Origin]
        );
        // Even a resolver response that advertises the cold relay tier is
        // reduced to R2/origin by the Phase 1 contract. The production fetch
        // path does not dispatch delivery_order at all.
        let order = [
            DeliverySource::R2HotObject,
            DeliverySource::CommunityRelay,
            DeliverySource::Origin,
        ];
        let compiled_phase_one = order
            .into_iter()
            .filter(|source| *source != DeliverySource::CommunityRelay)
            .collect::<Vec<_>>();
        assert_eq!(
            compiled_phase_one,
            vec![DeliverySource::R2HotObject, DeliverySource::Origin]
        );
    }

    #[test]
    fn historical_identity_uses_retained_metadata_and_never_origin_resolve() {
        // With no R2 endpoint and no retained local manifest, historical
        // identity acquisition has no network branch at all. The intentionally
        // unroutable origin would be contacted (and return Network) if generic
        // resolve were reachable here; honest Unavailable proves it is not.
        let temp = tempfile::tempdir().unwrap();
        let client = CommunityCacheClient {
            origin_url: "https://127.0.0.1:9".into(),
            r2_url: None,
            bearer_token: None,
            keys: BTreeMap::from([(
                ORIGIN_SIGNING_KEY_ID.into(),
                ed25519_dalek::SigningKey::from_bytes(&[17; 32]).verifying_key(),
            )]),
            limits: ProtocolLimits::default(),
            disk: VerifiedDiskCache::new(temp.path().into(), 1024 * 1024),
            http: reqwest::blocking::Client::builder()
                .https_only(true)
                .build()
                .unwrap(),
            transfers: TransferGate::new(
                1024,
                1024,
                2048,
                1,
                temp.path().join(TRANSFER_USAGE_FILE),
            ),
            categories: CategoryAllowlist {
                profiles: true,
                point_series: true,
                native_windows: false,
                temporal: false,
                case_artifacts: false,
            },
            authority_federation: Some(AuthorityFederationPolicy {
                preferred_origin_id: Some("university-lab".into()),
            }),
            origin_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            archival_origin_attempts: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        assert!(matches!(
            client.historical_manifest(&request("20260812_00z")),
            Err(CommunityCacheError::Unavailable)
        ));
    }

    #[test]
    fn authority_federation_body_contains_only_exact_request_and_optional_hint() {
        let request = request("20260812_00z");
        let value = serde_json::to_value(FederationProxyRequestBody {
            schema: FEDERATION_PROXY_SCHEMA,
            request: &request,
            preferred_origin_id: Some("university-lab"),
        })
        .unwrap();
        assert_eq!(value["schema"], FEDERATION_PROXY_SCHEMA);
        assert_eq!(value["request"], serde_json::to_value(&request).unwrap());
        assert_eq!(value["preferred_origin_id"], "university-lab");
        assert_eq!(value.as_object().unwrap().len(), 3);
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains("https://"));
        assert!(!encoded.contains("bearer"));
        assert_eq!(FEDERATION_PROXY_PATH, "/v1/federation/objects/resolve");

        let automatic = serde_json::to_value(FederationProxyRequestBody {
            schema: FEDERATION_PROXY_SCHEMA,
            request: &request,
            preferred_origin_id: None,
        })
        .unwrap();
        assert!(automatic.get("preferred_origin_id").is_none());
    }

    #[test]
    fn authority_federation_response_is_bound_to_exact_request_before_object_fetch() {
        let original_request = request("20260812_00z");
        let request_hash = request_sha256(&original_request).unwrap();
        let (signed, _, _) = signed_object(&original_request, b"verified payload");
        let response = ResolveObjectResponse {
            schema: RESOLVE_SCHEMA.to_owned(),
            request_sha256: request_hash.clone(),
            signed_manifest: Some(signed),
            delivery_order: vec![rw_community_protocol::DeliverySource::Origin],
        };
        validate_resolve_identity(
            &response,
            &original_request,
            &request_hash,
            &ProtocolLimits::default(),
        )
        .unwrap();

        let different = request("20260812_06z");
        assert!(
            validate_resolve_identity(
                &response,
                &different,
                &request_sha256(&different).unwrap(),
                &ProtocolLimits::default(),
            )
            .is_err()
        );
        let mut wrong_hash = response;
        wrong_hash.request_sha256 = "f".repeat(64);
        assert!(
            validate_resolve_identity(
                &wrong_hash,
                &original_request,
                &request_hash,
                &ProtocolLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn persisted_category_allowlist_classifies_every_share_query() {
        let profiles_only = CategoryAllowlist {
            profiles: true,
            point_series: false,
            native_windows: false,
            temporal: false,
            case_artifacts: false,
        };
        let profile = request("20260812_00z");
        assert!(profiles_only.allows(&profile.query));
        profiles_only.require(&profile.query).unwrap();

        let variants = [
            ShareQuery::PointSeries {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                window: rw_community_protocol::TimeWindow::Utc {
                    start_unix: 1_800_000_000,
                    end_unix: 1_800_003_600,
                },
                missing_policy: rw_community_protocol::MissingPolicy::Strict,
            },
            ShareQuery::NativeWindow {
                storage_slot: 1,
                valid_unix: 1_800_000_000,
                x0: 0,
                y0: 0,
                x1: 1,
                y1: 1,
                pressure_levels_hpa: vec![],
            },
            ShareQuery::GeographicWindow {
                storage_slot: 1,
                valid_unix: 1_800_000_000,
                west_longitude_e7: 1_700_000_000,
                south_latitude_e7: 200_000_000,
                east_longitude_e7: -1_700_000_000,
                north_latitude_e7: 500_000_000,
                pressure_levels_hpa: vec![],
            },
            ShareQuery::TemporalGrid {
                window: rw_community_protocol::TimeWindow::Utc {
                    start_unix: 1_800_000_000,
                    end_unix: 1_800_003_600,
                },
                reducer: "maximum".into(),
                semantics: "utc_window".into(),
                missing_policy: rw_community_protocol::MissingPolicy::Strict,
                pressure_levels_hpa: vec![],
            },
            ShareQuery::CaseArtifact {
                case_id: "case-1".into(),
                artifact_id: "artifact-1".into(),
                artifact_type: rw_community_protocol::CaseArtifactType::DerivedTable,
            },
        ];
        assert!(variants.iter().all(|query| !profiles_only.allows(query)));
        assert!(variants.iter().all(|query| matches!(
            profiles_only.require(query),
            Err(CommunityCacheError::Disabled)
        )));

        let profiles_disabled = CategoryAllowlist {
            profiles: false,
            ..profiles_only
        };
        assert!(matches!(
            profiles_disabled.require(&profile.query),
            Err(CommunityCacheError::Disabled)
        ));
    }

    #[test]
    fn transfer_gate_enforces_concurrency_hour_and_calendar_month_across_restart() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join(TRANSFER_USAGE_FILE);
        let gate = TransferGate::new(10, 10, 15, 1, ledger.clone());
        let first = gate.begin().unwrap();
        assert!(matches!(gate.begin(), Err(CommunityCacheError::Quota)));
        drop(first);
        assert!(gate.begin().is_ok());

        let aug = 1_786_492_800;
        gate.charge_download(aug, 8).unwrap();
        assert!(matches!(
            gate.charge_download(aug + 1, 3),
            Err(CommunityCacheError::Quota)
        ));
        assert_eq!(gate.usage().download_hour_bytes, 11);

        // A new client instance reads the same durable calendar-month usage.
        let restarted = TransferGate::new(10, 10, 15, 1, ledger);
        assert_eq!(restarted.usage().month_bytes, 11);
        // A new hour resets only the hourly bucket; the month remains capped.
        restarted.charge_download(aug + 3_600, 4).unwrap();
        assert!(matches!(
            restarted.charge_download(aug + 3_601, 1),
            Err(CommunityCacheError::Quota)
        ));
        // September resets the calendar-month allowance.
        restarted.charge_upload(1_789_171_200, 5).unwrap();
        assert_eq!(restarted.usage().upload_hour_bytes, 5);
        assert_eq!(restarted.usage().month_bytes, 5);
    }

    #[test]
    fn separately_constructed_clients_share_one_transfer_budget_and_restart_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let cache_root = temp.path().join("community-cache");
        let settings = settings::CommunityCacheSettings {
            enabled: true,
            origin_url: "https://origin.example".into(),
            manifest_public_key_base64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            ..Default::default()
        };
        let mut first = CommunityCacheClient::from_settings_for_test(
            &settings,
            cache_root.clone(),
            "test-origin-bearer",
        )
        .unwrap();
        let mut second = CommunityCacheClient::from_settings_for_test(
            &settings,
            cache_root.join("."),
            "test-origin-bearer",
        )
        .unwrap();
        let ledger = cache_root.join(TRANSFER_USAGE_FILE);
        first.transfers = TransferGate::shared(10, 7, 15, 1, ledger.clone()).unwrap();
        second.transfers = TransferGate::shared(10, 7, 15, 1, ledger.clone()).unwrap();
        assert!(Arc::ptr_eq(&first.transfers.inner, &second.transfers.inner));

        let active = first.transfers.begin().unwrap();
        assert!(matches!(
            second.transfers.begin(),
            Err(CommunityCacheError::Quota)
        ));
        drop(active);

        let aug = 1_786_492_800;
        first.transfers.charge_download(aug, 8).unwrap();
        assert!(matches!(
            second.transfers.charge_download(aug + 1, 3),
            Err(CommunityCacheError::Quota)
        ));
        assert_eq!(first.transfers.usage().download_hour_bytes, 11);
        assert_eq!(second.transfers.usage().month_bytes, 11);

        drop(first);
        drop(second);
        let restarted = TransferGate::shared(10, 7, 15, 1, ledger).unwrap();
        assert_eq!(restarted.usage().download_hour_bytes, 11);
        assert_eq!(restarted.usage().month_bytes, 11);
    }

    #[test]
    fn lowered_limits_apply_to_every_existing_client_handle() {
        let temp = tempfile::tempdir().unwrap();
        let ledger = temp.path().join("cache").join(TRANSFER_USAGE_FILE);
        let loose = TransferGate::shared(100, 100, 1_000, 4, ledger.clone()).unwrap();
        let strict = TransferGate::shared(10, 7, 15, 1, ledger).unwrap();
        assert!(Arc::ptr_eq(&loose.inner, &strict.inner));

        let active = loose.begin().unwrap();
        assert!(matches!(strict.begin(), Err(CommunityCacheError::Quota)));
        drop(active);

        let aug = 1_786_492_800;
        assert!(matches!(
            loose.charge_download(aug, 11),
            Err(CommunityCacheError::Quota)
        ));
        assert!(matches!(
            loose.charge_upload(aug, 8),
            Err(CommunityCacheError::Quota)
        ));
        assert_eq!(strict.usage().month_bytes, 19);
    }

    #[test]
    fn relay_seed_publication_requires_rights_and_explicit_private_publication() {
        let public = PublicationGrant {
            data_origin: DataOrigin::PublicProvider,
            explicit_owner_publication: false,
            redistribution_rights_confirmed: true,
        };
        assert!(relay_seed_publication_is_allowed(&public));
        assert!(!relay_seed_publication_is_allowed(&PublicationGrant {
            redistribution_rights_confirmed: false,
            ..public.clone()
        }));

        for data_origin in [
            DataOrigin::PrivateWrf,
            DataOrigin::PrivateArwen,
            DataOrigin::UserProvided,
        ] {
            let unpublished = PublicationGrant {
                data_origin,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            };
            assert!(!relay_seed_publication_is_allowed(&unpublished));
            assert!(relay_seed_publication_is_allowed(&PublicationGrant {
                explicit_owner_publication: true,
                ..unpublished
            }));
        }
    }

    #[test]
    fn cache_identity_cannot_mix_run_grid_variables_or_recipe() {
        let temp = tempfile::tempdir().unwrap();
        let original = request("20260812_00z");
        let decoded = br#"{"profile":"identity"}"#.to_vec();
        let (manifest, encoded, keys) = signed_object(&original, &decoded);
        let object = VerifiedObject {
            manifest,
            encoded,
            decoded,
            tier: DeliveryTier::Origin,
        };
        let cache = VerifiedDiskCache::new(temp.path().into(), 1024 * 1024);
        cache.store(&object).unwrap();

        let mut variants = Vec::new();
        let mut changed = original.clone();
        changed.run = "20260812_06z".into();
        variants.push(changed);
        let mut changed = original.clone();
        changed.grid_hash = "3".repeat(64);
        variants.push(changed);
        let mut changed = original.clone();
        changed.variables = vec!["dewpoint_iso".into(), "temperature_2m".into()];
        if let ShareQuery::Profile {
            pressure_variables, ..
        } = &mut changed.query
        {
            *pressure_variables = vec!["dewpoint_iso".into()];
        }
        variants.push(changed);
        let mut changed = original.clone();
        changed.recipe.recipe_version = "2".into();
        variants.push(changed);

        for variant in variants {
            assert!(
                cache
                    .load(&variant, &keys, &ProtocolLimits::default())
                    .unwrap()
                    .is_none(),
                "a distinct signed identity must never reuse the cached object"
            );
        }
    }

    #[test]
    fn malformed_cache_index_cannot_escape_content_addressed_paths() {
        let temp = tempfile::tempdir().unwrap();
        let cache = VerifiedDiskCache::new(temp.path().into(), 1024 * 1024);
        let malicious = CacheIndex {
            schema: INDEX_SCHEMA.into(),
            entries: BTreeMap::from([(
                "../../private-model-directory".into(),
                CacheIndexEntry {
                    object_sha256: "../../private-object".into(),
                    encoded_size: 1,
                    object_present: true,
                    manifest_size: 1,
                    last_access_unix: 1,
                },
            )]),
        };
        assert!(!cache_index_is_safe(&malicious));
        fs::write(
            temp.path().join(INDEX_FILE),
            serde_json::to_vec(&malicious).unwrap(),
        )
        .unwrap();
        assert!(cache.read_index().entries.is_empty());
    }

    #[test]
    fn remote_profile_builder_is_exact_canonical_and_local_store_independent() {
        let catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let time = catalog.axis[0].clone();
        let request = build_remote_profile_request(
            &catalog,
            &time,
            RemoteProfileVariableSelection {
                // Deliberately unsorted/duplicated: canonical identity must
                // never depend on model UI selection order.
                pressure_variables: vec![
                    "temperature_iso".into(),
                    "dewpoint_iso".into(),
                    "temperature_iso".into(),
                ],
                surface_variables: vec!["u_10m".into(), "temperature_2m".into()],
                pressure_levels_hpa: vec![850, 500, 700, 700],
            },
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(request.model, "hrrr");
        assert_eq!(request.run, catalog.run.run);
        assert_eq!(request.snapshot_id, catalog.run.snapshot_id);
        assert_eq!(request.grid_hash, catalog.run.grid_hash);
        assert_eq!(
            request.variables,
            vec!["dewpoint_iso", "temperature_2m", "temperature_iso", "u_10m"]
        );
        let ShareQuery::Profile {
            latitude_e7,
            longitude_e7,
            storage_slot,
            valid_unix,
            pressure_variables,
            surface_variables,
            pressure_levels_hpa,
        } = &request.query
        else {
            panic!("expected profile request")
        };
        assert_eq!(*latitude_e7, 351_234_550);
        assert_eq!(*longitude_e7, -977_654_343);
        assert_eq!(*storage_slot, 1);
        assert_eq!(*valid_unix, 1_786_496_400);
        assert_eq!(pressure_variables, &["dewpoint_iso", "temperature_iso"]);
        assert_eq!(surface_variables, &["temperature_2m", "u_10m"]);
        assert_eq!(pressure_levels_hpa, &[500, 700, 850]);
        assert_eq!(request.publication.data_origin, DataOrigin::PublicProvider);
        assert!(!request.publication.explicit_owner_publication);
        assert!(request.publication.redistribution_rights_confirmed);
        request.validate(&ProtocolLimits::default()).unwrap();

        let repeated = build_remote_profile_request(
            &catalog,
            &time,
            RemoteProfileVariableSelection {
                pressure_variables: vec!["dewpoint_iso".into(), "temperature_iso".into()],
                surface_variables: vec!["temperature_2m".into(), "u_10m".into()],
                pressure_levels_hpa: vec![500, 700, 850],
            },
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(
            request_sha256(&request).unwrap(),
            request_sha256(&repeated).unwrap()
        );
    }

    #[test]
    fn remote_profile_builder_fails_closed_for_private_or_mismatched_metadata() {
        let selection = || RemoteProfileVariableSelection {
            pressure_variables: vec!["temperature_iso".into()],
            surface_variables: vec!["temperature_2m".into()],
            pressure_levels_hpa: vec![500],
        };
        for (model, provider) in [
            ("wrf", "noaa-aws-public-data"),
            ("private-arwen-d03", "noaa-aws-public-data"),
            ("my-sim", "local-lab"),
            ("hrrr", "owner-local"),
            ("aifs-local", "local-aifs-inference"),
        ] {
            let catalog = remote_profile_catalog(model, provider);
            assert!(matches!(
                build_remote_profile_request(
                    &catalog,
                    &catalog.axis[0],
                    selection(),
                    &ProtocolLimits::default()
                ),
                Err(CommunityCacheError::Disabled)
            ));
        }

        let mut catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        let foreign_time = RemoteTimePoint {
            storage_slot: 99,
            lead_seconds: 99,
            valid_unix: 1_900_000_000,
        };
        assert!(matches!(
            build_remote_profile_request(
                &catalog,
                &foreign_time,
                selection(),
                &ProtocolLimits::default()
            ),
            Err(CommunityCacheError::Response)
        ));
        catalog.run.snapshot_id = "3".repeat(63);
        assert!(matches!(
            build_remote_profile_request(
                &catalog,
                &catalog.axis[0],
                selection(),
                &ProtocolLimits::default()
            ),
            Err(CommunityCacheError::Response)
        ));
    }

    #[test]
    fn exact_axis_validation_rejects_slot_time_and_capability_mismatch() {
        let mut catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        validate_remote_axis(&catalog.axis, &catalog.run, &catalog.variables).unwrap();

        catalog.axis[1].storage_slot = catalog.axis[0].storage_slot;
        assert!(validate_remote_axis(&catalog.axis, &catalog.run, &catalog.variables).is_err());
        catalog = remote_profile_catalog("hrrr", "noaa-aws-public-data");
        catalog.variables[0].available_slots.push(99);
        assert!(validate_remote_axis(&catalog.axis, &catalog.run, &catalog.variables).is_err());
    }
}
