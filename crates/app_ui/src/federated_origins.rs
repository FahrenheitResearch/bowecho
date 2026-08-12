//! Authenticated discovery and deterministic failover for deliberately public
//! Rusty Weather origins.
//!
//! This is not a Community Cache transport. It accepts only the authority's
//! signed federation catalog, whose origin descriptors must also chain to
//! separately pinned institutional keys. No relay participant, ICE candidate,
//! STUN result, socket address, or ordinary-client identity is represented by
//! this module.
//!
//! BowEcho invokes this client only as an approved public-origin fallback after
//! the normal operational path (local cache, R2/CDN, authoritative Hetzner
//! HTTPS origin) has missed. Historical Community Cache relay lookup remains a
//! separate path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use rw_community_protocol::{
    FEDERATION_CATALOG_PATH, FederationLimits, FederationPublicKey, FederationQueryCapability,
    FederationTrustStore, ProtocolError, PublicOriginDescriptor, SignedFederationCatalog,
    parse_signed_federation_catalog_bounded, trusted_signing_keys_from_base64,
    verify_signed_federation_catalog,
};
use serde::Deserialize;
use thiserror::Error;

const FEDERATION_HEALTH_PATH: &str = "/v1/federation/health";
const FEDERATION_HEALTH_SCHEMA: &str = "rw.federation.health-status.v1";
const MAX_HEALTH_BYTES: u64 = 256 * 1024;
const MAX_ENDPOINT_BYTES: usize = 4 * 1024;
const MAX_BEARER_BYTES: usize = 8 * 1024;
const MAX_DNS_ANSWERS: usize = 16;
const DNS_WORKERS: usize = 2;
const DNS_QUEUE_PER_WORKER: usize = 1;
const DNS_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_READ_CHUNK: usize = 64 * 1024;
const MAX_LOCAL_QUARANTINE: Duration = Duration::from_secs(24 * 60 * 60);

/// Non-secret, persistable trust material. Embedded descriptor keys do not
/// bootstrap trust: every origin and the catalog authority need an independent
/// pin here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedFederationKey {
    pub key_id: String,
    pub public_key_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedPublicOrigin {
    pub origin_id: String,
    pub descriptor_signing_keys: Vec<PinnedFederationKey>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FederationTrustConfig {
    pub catalog_signing_keys: Vec<PinnedFederationKey>,
    pub approved_origins: Vec<ApprovedPublicOrigin>,
    pub revoked_origin_ids: Vec<String>,
    pub revoked_key_ids: Vec<String>,
}

impl FederationTrustConfig {
    pub fn from_settings(settings: &settings::FederationSettings) -> Self {
        Self {
            catalog_signing_keys: settings
                .catalog_signing_keys
                .iter()
                .map(|key| PinnedFederationKey {
                    key_id: key.key_id.clone(),
                    public_key_base64: key.public_key_base64.clone(),
                })
                .collect(),
            approved_origins: settings
                .approved_origins
                .iter()
                .map(|origin| ApprovedPublicOrigin {
                    origin_id: origin.origin_id.clone(),
                    descriptor_signing_keys: origin
                        .descriptor_signing_keys
                        .iter()
                        .map(|key| PinnedFederationKey {
                            key_id: key.key_id.clone(),
                            public_key_base64: key.public_key_base64.clone(),
                        })
                        .collect(),
                })
                .collect(),
            revoked_origin_ids: settings.revoked_origin_ids.clone(),
            revoked_key_ids: settings.revoked_key_ids.clone(),
        }
    }

    pub fn build(&self) -> Result<FederationTrustStore, FederatedOriginError> {
        if self.catalog_signing_keys.is_empty()
            || self.catalog_signing_keys.len() > FederationLimits::default().max_keys_per_usage
            || self.approved_origins.len() > FederationLimits::default().max_origins
        {
            return Err(FederatedOriginError::InvalidTrust);
        }
        let catalog_keys = trusted_signing_keys_from_base64(
            self.catalog_signing_keys
                .iter()
                .map(|key| (key.key_id.clone(), key.public_key_base64.as_str())),
        )?;
        let mut approved_origins = BTreeMap::new();
        for origin in &self.approved_origins {
            validate_identifier(&origin.origin_id)?;
            if origin.descriptor_signing_keys.is_empty()
                || origin.descriptor_signing_keys.len()
                    > FederationLimits::default().max_keys_per_usage
            {
                return Err(FederatedOriginError::InvalidTrust);
            }
            let keys = trusted_signing_keys_from_base64(
                origin
                    .descriptor_signing_keys
                    .iter()
                    .map(|key| (key.key_id.clone(), key.public_key_base64.as_str())),
            )?;
            if approved_origins
                .insert(origin.origin_id.clone(), keys)
                .is_some()
            {
                return Err(FederatedOriginError::InvalidTrust);
            }
        }
        let revoked_origin_ids = canonical_id_set(&self.revoked_origin_ids)?;
        let revoked_key_ids = canonical_id_set(&self.revoked_key_ids)?;
        if self
            .catalog_signing_keys
            .iter()
            .any(|key| revoked_key_ids.contains(&key.key_id))
        {
            return Err(FederatedOriginError::InvalidTrust);
        }
        Ok(FederationTrustStore {
            catalog_keys,
            approved_origins,
            revoked_origin_ids,
            revoked_key_ids,
        })
    }
}

fn canonical_id_set(values: &[String]) -> Result<BTreeSet<String>, FederatedOriginError> {
    let mut result = BTreeSet::new();
    for value in values {
        validate_identifier(value)?;
        if !result.insert(value.clone()) {
            return Err(FederatedOriginError::InvalidTrust);
        }
    }
    Ok(result)
}

/// Constructor settings. The bearer token is intentionally absent from
/// `Debug`; callers should source it from BowEcho's operating-system vault.
pub struct FederatedOriginClientConfig {
    authority_origin: String,
    authority_bearer_token: String,
    trust: FederationTrustStore,
    maximum_candidates: usize,
    local_failure_threshold: u32,
    local_quarantine: Duration,
}

impl FederatedOriginClientConfig {
    pub fn new(
        authority_origin: impl Into<String>,
        authority_bearer_token: impl Into<String>,
        trust: FederationTrustStore,
    ) -> Self {
        Self {
            authority_origin: authority_origin.into(),
            authority_bearer_token: authority_bearer_token.into(),
            trust,
            maximum_candidates: 8,
            local_failure_threshold: 2,
            local_quarantine: Duration::from_secs(60),
        }
    }

    pub fn maximum_candidates(mut self, maximum: usize) -> Self {
        self.maximum_candidates = maximum;
        self
    }

    pub fn local_quarantine(mut self, failure_threshold: u32, duration: Duration) -> Self {
        self.local_failure_threshold = failure_threshold;
        self.local_quarantine = duration;
        self
    }
}

impl fmt::Debug for FederatedOriginClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FederatedOriginClientConfig")
            .field("authority_origin", &self.authority_origin)
            .field("authority_bearer_token", &Redacted)
            .field("maximum_candidates", &self.maximum_candidates)
            .field("local_failure_threshold", &self.local_failure_threshold)
            .field("local_quarantine", &self.local_quarantine)
            .finish_non_exhaustive()
    }
}

struct BearerSecret(String);

impl BearerSecret {
    fn new(value: String) -> Result<Self, FederatedOriginError> {
        if value.is_empty()
            || value.len() > MAX_BEARER_BYTES
            || value.trim() != value
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\\'))
        {
            return Err(FederatedOriginError::InvalidBearerToken);
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BearerSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Debug, Error)]
pub enum FederatedOriginError {
    #[error("the federation authority must be a canonical public HTTPS origin root")]
    UnsafeAuthority,
    #[error("the federation bearer token is invalid")]
    InvalidBearerToken,
    #[error("the federation trust configuration is invalid")]
    InvalidTrust,
    #[error("the federation selection is invalid")]
    InvalidSelection,
    #[error("the federation response is malformed or exceeds its bound")]
    InvalidResponse,
    #[error("public-origin DNS is unavailable or returned a non-public address")]
    UnsafeDns,
    #[error("the authenticated federation endpoint returned HTTP {0}")]
    HttpStatus(u16),
    #[error("the federation request failed")]
    Network,
    #[error("no verified federation snapshot is available")]
    NoVerifiedSnapshot,
    #[error("no approved public origin can satisfy this request")]
    NoCandidate,
    #[error(
        "all approved public origins failed verification or delivery: {attempted_origin_ids:?}"
    )]
    AllCandidatesFailed { attempted_origin_ids: Vec<String> },
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Fixed-point request bounds matching the signed federation wire contract.
/// Antimeridian-crossing requests must be split into two ordinary rectangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FederationGeoBounds {
    pub west_longitude_e7: i32,
    pub south_latitude_e7: i32,
    pub east_longitude_e7: i32,
    pub north_latitude_e7: i32,
}

impl FederationGeoBounds {
    fn validate(self) -> Result<(), FederatedOriginError> {
        if !(-1_800_000_000..=1_800_000_000).contains(&self.west_longitude_e7)
            || !(-1_800_000_000..=1_800_000_000).contains(&self.east_longitude_e7)
            || !(-900_000_000..=900_000_000).contains(&self.south_latitude_e7)
            || !(-900_000_000..=900_000_000).contains(&self.north_latitude_e7)
            || self.west_longitude_e7 >= self.east_longitude_e7
            || self.south_latitude_e7 >= self.north_latitude_e7
        {
            return Err(FederatedOriginError::InvalidSelection);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOriginSelection {
    pub model: String,
    pub product: String,
    pub query: FederationQueryCapability,
    pub pressure_level_hpa: Option<u16>,
    pub bounds: Option<FederationGeoBounds>,
    pub minimum_response_bytes: u64,
    pub require_replication: bool,
}

impl PublicOriginSelection {
    fn validate(&self) -> Result<(), FederatedOriginError> {
        validate_identifier(&self.model)?;
        validate_identifier(&self.product)?;
        if self.minimum_response_bytes == 0
            || self
                .pressure_level_hpa
                .is_some_and(|level| level == 0 || level > 1_200)
        {
            return Err(FederatedOriginError::InvalidSelection);
        }
        if let Some(bounds) = self.bounds {
            bounds.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicOriginHealth {
    Unknown,
    Healthy,
    Degraded,
    Quarantined,
}

impl PublicOriginHealth {
    fn label(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Healthy => "Healthy",
            Self::Degraded => "Degraded",
            Self::Quarantined => "Quarantined",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedOriginCandidate {
    /// Deliberately public institutional descriptor. It contains the keys a
    /// caller must use to authenticate the returned product/object.
    pub descriptor: PublicOriginDescriptor,
    pub health: PublicOriginHealth,
    pub consecutive_failures: u32,
}

impl FederatedOriginCandidate {
    pub fn origin_id(&self) -> &str {
        &self.descriptor.origin_id
    }

    pub fn object_signing_keys(&self) -> &[FederationPublicKey] {
        &self.descriptor.object_signing_keys
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationRefreshSummary {
    pub catalog_id: String,
    pub catalog_expires_unix: i64,
    pub public_origin_count: usize,
    pub health_monitor_enabled: bool,
}

/// UI-safe view of the signed catalog and coarse health endpoint. All URLs in
/// this value are deliberately public institutional metadata; it can never
/// contain a Community Cache participant address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationDirectoryOverview {
    pub catalog_id: String,
    pub generated_unix: i64,
    pub expires_unix: i64,
    pub health_monitor_enabled: bool,
    pub last_health_round_unix: Option<i64>,
    pub origins: Vec<FederatedOriginOverview>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedOriginOverview {
    pub origin_id: String,
    pub display_name: String,
    pub public_https_url: String,
    pub health: PublicOriginHealth,
    pub consecutive_failures: u32,
    pub quarantine_until_unix: Option<i64>,
    pub descriptor_expires_unix: i64,
    pub model_count: usize,
    pub product_count: usize,
    pub coverage_area_count: usize,
    pub maximum_response_bytes: u64,
    pub attribution_url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FederationOriginHealthStatus {
    origin_id: String,
    state: PublicOriginHealth,
    consecutive_failures: u32,
    quarantine_until_unix: Option<i64>,
    last_probe_unix: Option<i64>,
    last_success_unix: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FederationHealthStatus {
    schema: String,
    monitor_enabled: bool,
    total_origins: usize,
    healthy_origins: usize,
    degraded_origins: usize,
    quarantined_origins: usize,
    unknown_origins: usize,
    last_round_unix: Option<i64>,
    origins: Vec<FederationOriginHealthStatus>,
}

#[derive(Debug)]
struct VerifiedDirectory {
    catalog: SignedFederationCatalog,
    health: BTreeMap<String, FederationOriginHealthStatus>,
    monitor_enabled: bool,
    last_health_round_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct LocalHealth {
    failures: u32,
    quarantine_until_unix: i64,
}

/// The concrete authenticated discovery client. The authority credential is
/// sent only to the configured authority. This module deliberately makes no
/// data request to a federated origin because v1 descriptors do not declare an
/// end-user authentication mode; this prevents an authority-scoped token from
/// ever leaking to a university/lab host.
pub struct FederatedOriginClient {
    authority: PublicOriginRoot,
    authority_bearer: BearerSecret,
    trust: FederationTrustStore,
    limits: FederationLimits,
    maximum_candidates: usize,
    local_failure_threshold: u32,
    local_quarantine_seconds: i64,
    directory: Option<VerifiedDirectory>,
    local_health: BTreeMap<String, LocalHealth>,
    dns: BoundedDnsPool,
}

impl fmt::Debug for FederatedOriginClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FederatedOriginClient")
            .field("authority", &self.authority.raw)
            .field("authority_bearer", &self.authority_bearer)
            .field("maximum_candidates", &self.maximum_candidates)
            .field(
                "verified_origins",
                &self
                    .directory
                    .as_ref()
                    .map(|directory| directory.catalog.catalog.origins.len()),
            )
            .finish_non_exhaustive()
    }
}

impl FederatedOriginClient {
    pub fn new(config: FederatedOriginClientConfig) -> Result<Self, FederatedOriginError> {
        if config.maximum_candidates == 0
            || config.maximum_candidates > FederationLimits::default().max_origins
            || config.local_failure_threshold == 0
            || config.local_quarantine.is_zero()
            || config.local_quarantine > MAX_LOCAL_QUARANTINE
        {
            return Err(FederatedOriginError::InvalidSelection);
        }
        let local_quarantine_seconds = i64::try_from(config.local_quarantine.as_secs())
            .map_err(|_| FederatedOriginError::InvalidSelection)?;
        Ok(Self {
            authority: PublicOriginRoot::parse(&config.authority_origin)?,
            authority_bearer: BearerSecret::new(config.authority_bearer_token)?,
            trust: config.trust,
            limits: FederationLimits::default(),
            maximum_candidates: config.maximum_candidates,
            local_failure_threshold: config.local_failure_threshold,
            local_quarantine_seconds,
            directory: None,
            local_health: BTreeMap::new(),
            dns: BoundedDnsPool::new(DNS_WORKERS, DNS_QUEUE_PER_WORKER),
        })
    }

    /// Atomically refresh both authenticated endpoints. A failed refresh never
    /// replaces the last verified directory; an expired previous catalog is
    /// still rejected when candidates are selected.
    pub fn refresh(&mut self) -> Result<FederationRefreshSummary, FederatedOriginError> {
        let now = now_unix();
        let client = self.pinned_client(&self.authority)?;
        let catalog_bytes = authority_get_json(
            &client,
            &self.authority,
            FEDERATION_CATALOG_PATH,
            self.authority_bearer.expose(),
            self.limits.max_catalog_bytes,
        )?;
        let health_bytes = authority_get_json(
            &client,
            &self.authority,
            FEDERATION_HEALTH_PATH,
            self.authority_bearer.expose(),
            MAX_HEALTH_BYTES,
        )?;
        self.install_snapshot(&catalog_bytes, &health_bytes, now)
    }

    fn install_snapshot(
        &mut self,
        catalog_bytes: &[u8],
        health_bytes: &[u8],
        now: i64,
    ) -> Result<FederationRefreshSummary, FederatedOriginError> {
        let catalog = parse_signed_federation_catalog_bounded(catalog_bytes, &self.limits)?;
        verify_signed_federation_catalog(&catalog, now, &self.trust, &self.limits)?;
        let health: FederationHealthStatus = serde_json::from_slice(health_bytes)
            .map_err(|_| FederatedOriginError::InvalidResponse)?;
        let health = validate_health_status(health, &catalog, now, &self.limits)?;
        let summary = FederationRefreshSummary {
            catalog_id: catalog.catalog.catalog_id.clone(),
            catalog_expires_unix: catalog.catalog.expires_unix,
            public_origin_count: catalog.catalog.origins.len(),
            health_monitor_enabled: health.monitor_enabled,
        };
        let admitted = catalog
            .catalog
            .origins
            .iter()
            .map(|origin| origin.descriptor.origin_id.as_str())
            .collect::<BTreeSet<_>>();
        self.local_health
            .retain(|origin_id, _| admitted.contains(origin_id.as_str()));
        self.directory = Some(VerifiedDirectory {
            catalog,
            health: health.origins,
            monitor_enabled: health.monitor_enabled,
            last_health_round_unix: health.last_round_unix,
        });
        Ok(summary)
    }

    pub fn directory_overview(&self) -> Result<FederationDirectoryOverview, FederatedOriginError> {
        self.directory_overview_at(now_unix())
    }

    fn directory_overview_at(
        &self,
        now: i64,
    ) -> Result<FederationDirectoryOverview, FederatedOriginError> {
        let directory = self
            .directory
            .as_ref()
            .ok_or(FederatedOriginError::NoVerifiedSnapshot)?;
        verify_signed_federation_catalog(&directory.catalog, now, &self.trust, &self.limits)?;
        let mut origins = Vec::with_capacity(directory.catalog.catalog.origins.len());
        for signed in &directory.catalog.catalog.origins {
            let descriptor = &signed.descriptor;
            let health = directory
                .health
                .get(&descriptor.origin_id)
                .ok_or(FederatedOriginError::InvalidResponse)?;
            let local = self
                .local_health
                .get(&descriptor.origin_id)
                .copied()
                .unwrap_or_default();
            let local_quarantined = now < local.quarantine_until_unix;
            origins.push(FederatedOriginOverview {
                origin_id: descriptor.origin_id.clone(),
                display_name: descriptor.display_name.clone(),
                public_https_url: descriptor.https_base_url.clone(),
                health: if local_quarantined {
                    PublicOriginHealth::Quarantined
                } else {
                    health.state
                },
                consecutive_failures: health.consecutive_failures.saturating_add(local.failures),
                quarantine_until_unix: if local_quarantined {
                    Some(local.quarantine_until_unix)
                } else {
                    health.quarantine_until_unix
                },
                descriptor_expires_unix: descriptor.expires_unix,
                model_count: descriptor.models.len(),
                product_count: descriptor
                    .models
                    .iter()
                    .map(|model| model.products.len())
                    .sum(),
                coverage_area_count: descriptor.geographic_coverage.len(),
                maximum_response_bytes: descriptor.quotas.maximum_response_bytes,
                attribution_url: descriptor.policy_links.attribution_url.clone(),
            });
        }
        Ok(FederationDirectoryOverview {
            catalog_id: directory.catalog.catalog.catalog_id.clone(),
            generated_unix: directory.catalog.catalog.generated_unix,
            expires_unix: directory.catalog.catalog.expires_unix,
            health_monitor_enabled: directory.monitor_enabled,
            last_health_round_unix: directory.last_health_round_unix,
            origins,
        })
    }

    pub fn candidates(
        &self,
        selection: &PublicOriginSelection,
    ) -> Result<Vec<FederatedOriginCandidate>, FederatedOriginError> {
        self.candidates_at(selection, now_unix())
    }

    fn candidates_at(
        &self,
        selection: &PublicOriginSelection,
        now: i64,
    ) -> Result<Vec<FederatedOriginCandidate>, FederatedOriginError> {
        selection.validate()?;
        let directory = self
            .directory
            .as_ref()
            .ok_or(FederatedOriginError::NoVerifiedSnapshot)?;
        verify_signed_federation_catalog(&directory.catalog, now, &self.trust, &self.limits)?;
        let mut selected = Vec::new();
        for signed in &directory.catalog.catalog.origins {
            let descriptor = &signed.descriptor;
            let health = directory
                .health
                .get(&descriptor.origin_id)
                .ok_or(FederatedOriginError::InvalidResponse)?;
            let local = self
                .local_health
                .get(&descriptor.origin_id)
                .copied()
                .unwrap_or_default();
            if health.state == PublicOriginHealth::Quarantined
                || health
                    .quarantine_until_unix
                    .is_some_and(|until| now < until)
                || now < local.quarantine_until_unix
                || descriptor.quotas.maximum_response_bytes < selection.minimum_response_bytes
                || selection.require_replication && !descriptor.replication.accepts_replication
                || selection.require_replication
                    && !descriptor.replication.models.contains(&selection.model)
                || selection.bounds.is_some_and(|bounds| {
                    !descriptor.geographic_coverage.iter().any(|area| {
                        area.west_longitude_e7 <= bounds.west_longitude_e7
                            && area.south_latitude_e7 <= bounds.south_latitude_e7
                            && area.east_longitude_e7 >= bounds.east_longitude_e7
                            && area.north_latitude_e7 >= bounds.north_latitude_e7
                    })
                })
            {
                continue;
            }
            let capability = descriptor
                .models
                .iter()
                .find(|model| model.model == selection.model)
                .and_then(|model| {
                    model
                        .products
                        .iter()
                        .find(|product| product.product == selection.product)
                });
            let Some(capability) = capability else {
                continue;
            };
            if !capability.queries.contains(&selection.query)
                || selection
                    .pressure_level_hpa
                    .is_some_and(|level| !capability.pressure_levels_hpa.contains(&level))
            {
                continue;
            }
            selected.push((
                health.consecutive_failures.saturating_add(local.failures),
                FederatedOriginCandidate {
                    descriptor: descriptor.clone(),
                    health: health.state,
                    consecutive_failures: health
                        .consecutive_failures
                        .saturating_add(local.failures),
                },
            ));
        }
        selected.sort_by(|(a_failures, a), (b_failures, b)| {
            a_failures
                .cmp(b_failures)
                .then_with(|| a.origin_id().cmp(b.origin_id()))
        });
        selected.truncate(self.maximum_candidates);
        Ok(selected
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect())
    }

    pub fn record_success(&mut self, origin_id: &str) -> Result<(), FederatedOriginError> {
        self.require_admitted_origin(origin_id)?;
        self.local_health.remove(origin_id);
        Ok(())
    }

    pub fn record_failure(&mut self, origin_id: &str) -> Result<(), FederatedOriginError> {
        self.record_failure_at(origin_id, now_unix())
    }

    fn record_failure_at(&mut self, origin_id: &str, now: i64) -> Result<(), FederatedOriginError> {
        self.require_admitted_origin(origin_id)?;
        let health = self.local_health.entry(origin_id.to_owned()).or_default();
        health.failures = health.failures.saturating_add(1);
        if health.failures >= self.local_failure_threshold {
            health.quarantine_until_unix = now.saturating_add(self.local_quarantine_seconds);
        }
        Ok(())
    }

    fn require_admitted_origin(&self, origin_id: &str) -> Result<(), FederatedOriginError> {
        validate_identifier(origin_id)?;
        let directory = self
            .directory
            .as_ref()
            .ok_or(FederatedOriginError::NoVerifiedSnapshot)?;
        if !directory
            .catalog
            .catalog
            .origins
            .iter()
            .any(|origin| origin.descriptor.origin_id == origin_id)
        {
            return Err(FederatedOriginError::InvalidSelection);
        }
        Ok(())
    }

    fn pinned_client(
        &self,
        root: &PublicOriginRoot,
    ) -> Result<reqwest::blocking::Client, FederatedOriginError> {
        let addresses = self.dns.resolve(&root.host, DNS_TIMEOUT)?;
        reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .resolve_to_addrs(&root.host, &addresses)
            .user_agent("BowEcho/0.34 federated-public-origin")
            .build()
            .map_err(|_| FederatedOriginError::Network)
    }
}

/// Self-contained settings/status panel. The app shell may mount this after a
/// background refresh; it performs no I/O on the egui thread.
pub fn show_federation_discovery_ui(
    ui: &mut eframe::egui::Ui,
    client: Option<&FederatedOriginClient>,
) {
    use eframe::egui::{Color32, RichText};

    ui.heading("Public Origin Federation");
    ui.label(
        "Verified university, lab, and public Rusty Weather origins. Their HTTPS addresses are intentionally public; ordinary Community Cache users never appear here.",
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Model data stays authority-mediated: BowEcho asks Hetzner to resolve an exact signed request, and Hetzner alone may contact an approved institution. BowEcho never sends credentials or data requests to these addresses.",
        )
        .color(Color32::LIGHT_BLUE),
    );

    let Some(client) = client else {
        ui.weak("Federation is not configured.");
        return;
    };
    let Ok(overview) = client.directory_overview() else {
        ui.weak("No current verified catalog and health snapshot is available.");
        return;
    };

    ui.add_space(6.0);
    eframe::egui::Grid::new("federation_directory_summary")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            ui.weak("Catalog");
            ui.monospace(&overview.catalog_id);
            ui.end_row();
            ui.weak("Origins");
            ui.label(overview.origins.len().to_string());
            ui.end_row();
            ui.weak("Signed validity");
            ui.monospace(format!(
                "{} - {} UTC seconds",
                overview.generated_unix, overview.expires_unix
            ));
            ui.end_row();
            ui.weak("Health monitor");
            ui.label(if overview.health_monitor_enabled {
                "Enabled"
            } else {
                "Passive / unknown"
            });
            ui.end_row();
        });

    ui.add_space(6.0);
    for origin in overview.origins {
        let color = match origin.health {
            PublicOriginHealth::Healthy => Color32::LIGHT_GREEN,
            PublicOriginHealth::Unknown => Color32::GRAY,
            PublicOriginHealth::Degraded => Color32::YELLOW,
            PublicOriginHealth::Quarantined => Color32::LIGHT_RED,
        };
        eframe::egui::CollapsingHeader::new(format!(
            "{}  [{}]",
            origin.display_name,
            origin.health.label()
        ))
        .id_salt(("federated_public_origin", &origin.origin_id))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(color, origin.health.label());
                ui.weak(format!("{} failures", origin.consecutive_failures));
                if let Some(until) = origin.quarantine_until_unix {
                    ui.weak(format!("quarantined through {until} UTC seconds"));
                }
            });
            ui.monospace(&origin.origin_id);
            ui.hyperlink_to(&origin.public_https_url, &origin.public_https_url);
            eframe::egui::Grid::new(("federated_origin_details", &origin.origin_id))
                .num_columns(2)
                .show(ui, |ui| {
                    ui.weak("Capabilities");
                    ui.label(format!(
                        "{} models / {} products",
                        origin.model_count, origin.product_count
                    ));
                    ui.end_row();
                    ui.weak("Coverage");
                    ui.label(format!("{} signed areas", origin.coverage_area_count));
                    ui.end_row();
                    ui.weak("Per-response limit");
                    ui.label(format_bytes(origin.maximum_response_bytes));
                    ui.end_row();
                    ui.weak("Descriptor expires");
                    ui.monospace(origin.descriptor_expires_unix.to_string());
                    ui.end_row();
                });
            ui.hyperlink_to("Attribution and terms", &origin.attribution_url);
        });
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

fn authority_get_json(
    client: &reqwest::blocking::Client,
    root: &PublicOriginRoot,
    path: &str,
    bearer: &str,
    maximum_bytes: u64,
) -> Result<Vec<u8>, FederatedOriginError> {
    let response = client
        .get(root.endpoint(path)?)
        .bearer_auth(bearer)
        .send()
        .map_err(|_| FederatedOriginError::Network)?;
    read_bounded_json_response(response, maximum_bytes)
}

fn read_bounded_json_response(
    response: reqwest::blocking::Response,
    maximum_bytes: u64,
) -> Result<Vec<u8>, FederatedOriginError> {
    if maximum_bytes == 0 {
        return Err(FederatedOriginError::InvalidResponse);
    }
    let status = response.status();
    if !status.is_success() {
        return Err(FederatedOriginError::HttpStatus(status.as_u16()));
    }
    if let Some(encoding) = response.headers().get(CONTENT_ENCODING) {
        let encoding = encoding
            .to_str()
            .map_err(|_| FederatedOriginError::InvalidResponse)?;
        if !encoding.eq_ignore_ascii_case("identity") {
            return Err(FederatedOriginError::InvalidResponse);
        }
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    if !content_type.eq_ignore_ascii_case("application/json") {
        return Err(FederatedOriginError::InvalidResponse);
    }
    if let Some(value) = response.headers().get(CONTENT_LENGTH) {
        let length = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(FederatedOriginError::InvalidResponse)?;
        if length == 0 || length > maximum_bytes {
            return Err(FederatedOriginError::InvalidResponse);
        }
    }
    let mut reader = response.take(maximum_bytes.saturating_add(1));
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; HTTP_READ_CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| FederatedOriginError::Network)?;
        if read == 0 {
            break;
        }
        bytes
            .try_reserve(read)
            .map_err(|_| FederatedOriginError::InvalidResponse)?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.is_empty() || bytes.len() as u64 > maximum_bytes {
        return Err(FederatedOriginError::InvalidResponse);
    }
    Ok(bytes)
}

struct ValidatedHealth {
    monitor_enabled: bool,
    last_round_unix: Option<i64>,
    origins: BTreeMap<String, FederationOriginHealthStatus>,
}

fn validate_health_status(
    value: FederationHealthStatus,
    catalog: &SignedFederationCatalog,
    now: i64,
    limits: &FederationLimits,
) -> Result<ValidatedHealth, FederatedOriginError> {
    if value.schema != FEDERATION_HEALTH_SCHEMA
        || value.total_origins != value.origins.len()
        || value.total_origins != catalog.catalog.origins.len()
        || value.total_origins > limits.max_origins
        || value
            .healthy_origins
            .checked_add(value.degraded_origins)
            .and_then(|count| count.checked_add(value.quarantined_origins))
            .and_then(|count| count.checked_add(value.unknown_origins))
            != Some(value.total_origins)
        || !timestamp_is_safe(value.last_round_unix, now)
    {
        return Err(FederatedOriginError::InvalidResponse);
    }
    let catalog_ids = catalog
        .catalog
        .origins
        .iter()
        .map(|origin| origin.descriptor.origin_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut origins = BTreeMap::new();
    let mut counts = [0usize; 4];
    for origin in value.origins {
        validate_identifier(&origin.origin_id)?;
        if !catalog_ids.contains(origin.origin_id.as_str())
            || !timestamp_is_safe(origin.last_probe_unix, now)
            || !timestamp_is_safe(origin.last_success_unix, now)
            || origin
                .last_success_unix
                .zip(origin.last_probe_unix)
                .is_some_and(|(success, probe)| success > probe)
            || origins.contains_key(&origin.origin_id)
        {
            return Err(FederatedOriginError::InvalidResponse);
        }
        match origin.state {
            PublicOriginHealth::Unknown
                if origin.consecutive_failures == 0
                    && origin.quarantine_until_unix.is_none()
                    && origin.last_probe_unix.is_none() =>
            {
                counts[0] += 1;
            }
            PublicOriginHealth::Healthy
                if origin.consecutive_failures == 0
                    && origin.quarantine_until_unix.is_none()
                    && origin.last_probe_unix.is_some()
                    && origin.last_success_unix.is_some() =>
            {
                counts[1] += 1;
            }
            PublicOriginHealth::Degraded
                if origin.consecutive_failures > 0
                    && origin.quarantine_until_unix.is_none()
                    && origin.last_probe_unix.is_some() =>
            {
                counts[2] += 1;
            }
            PublicOriginHealth::Quarantined
                if origin.consecutive_failures > 0
                    && origin
                        .quarantine_until_unix
                        .is_some_and(|until| until > now)
                    && origin.last_probe_unix.is_some() =>
            {
                counts[3] += 1;
            }
            _ => return Err(FederatedOriginError::InvalidResponse),
        }
        origins.insert(origin.origin_id.clone(), origin);
    }
    if counts
        != [
            value.unknown_origins,
            value.healthy_origins,
            value.degraded_origins,
            value.quarantined_origins,
        ]
    {
        return Err(FederatedOriginError::InvalidResponse);
    }
    Ok(ValidatedHealth {
        monitor_enabled: value.monitor_enabled,
        last_round_unix: value.last_round_unix,
        origins,
    })
}

fn timestamp_is_safe(value: Option<i64>, now: i64) -> bool {
    value.is_none_or(|timestamp| timestamp >= 0 && timestamp <= now.saturating_add(300))
}

fn validate_identifier(value: &str) -> Result<(), FederatedOriginError> {
    if value.is_empty()
        || value.len() > 128
        || value.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.'))
        })
        || value.starts_with(['-', '_', '.'])
        || value.ends_with(['-', '_', '.'])
    {
        return Err(FederatedOriginError::InvalidSelection);
    }
    Ok(())
}

#[derive(Debug)]
struct PublicOriginRoot {
    raw: String,
    host: String,
}

impl PublicOriginRoot {
    fn parse(value: &str) -> Result<Self, FederatedOriginError> {
        if value.len() > 512
            || !value.is_ascii()
            || !value.starts_with("https://")
            || value
                .chars()
                .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
            || value.contains(['\\', '@', '#', '?'])
        {
            return Err(FederatedOriginError::UnsafeAuthority);
        }
        let host = &value[8..];
        if host.is_empty()
            || host.len() > 253
            || host.contains(['/', ':'])
            || host != host.to_ascii_lowercase()
            || !host.contains('.')
            || host
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
            || forbidden_public_host(host)
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err(FederatedOriginError::UnsafeAuthority);
        }
        Ok(Self {
            raw: value.to_owned(),
            host: host.to_owned(),
        })
    }

    fn endpoint(&self, path_and_query: &str) -> Result<String, FederatedOriginError> {
        safe_public_origin_url(&self.raw, path_and_query)
    }
}

fn forbidden_public_host(host: &str) -> bool {
    [
        "localhost",
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home",
        ".test",
        ".invalid",
        ".example",
        ".onion",
    ]
    .iter()
    .any(|suffix| host == suffix.trim_start_matches('.') || host.ends_with(suffix))
}

fn safe_public_origin_url(
    origin_root: &str,
    path_and_query: &str,
) -> Result<String, FederatedOriginError> {
    PublicOriginRoot::parse(origin_root)?;
    validate_api_path(path_and_query)?;
    Ok(format!("{origin_root}{path_and_query}"))
}

fn validate_api_path(value: &str) -> Result<(), FederatedOriginError> {
    if value.len() > MAX_ENDPOINT_BYTES
        || !value.is_ascii()
        || !value.starts_with("/v1/")
        || value.contains(['\\', '#'])
        || value
            .chars()
            .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
    {
        return Err(FederatedOriginError::InvalidSelection);
    }
    let path = value.split('?').next().unwrap_or_default();
    if path.contains("//")
        || path.to_ascii_lowercase().contains("%2e")
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(FederatedOriginError::InvalidSelection);
    }
    Ok(())
}

#[derive(Debug)]
struct BoundedDnsPool {
    senders: Vec<mpsc::SyncSender<DnsJob>>,
    cursor: AtomicUsize,
}

#[derive(Debug)]
struct DnsJob {
    host: String,
    response: mpsc::SyncSender<DnsResult>,
}

#[derive(Debug)]
enum DnsResult {
    Addresses(Vec<SocketAddr>),
    Rejected,
}

impl BoundedDnsPool {
    fn new(workers: usize, queue_per_worker: usize) -> Self {
        let mut senders = Vec::with_capacity(workers);
        for _ in 0..workers {
            let (sender, receiver) = mpsc::sync_channel::<DnsJob>(queue_per_worker);
            thread::spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let result = resolve_public_addresses(&job.host)
                        .map(DnsResult::Addresses)
                        .unwrap_or(DnsResult::Rejected);
                    let _ = job.response.send(result);
                }
            });
            senders.push(sender);
        }
        Self {
            senders,
            cursor: AtomicUsize::new(0),
        }
    }

    fn resolve(
        &self,
        host: &str,
        timeout: Duration,
    ) -> Result<Vec<SocketAddr>, FederatedOriginError> {
        if self.senders.is_empty() {
            return Err(FederatedOriginError::UnsafeDns);
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        let mut job = Some(DnsJob {
            host: host.to_owned(),
            response: response_sender,
        });
        for offset in 0..self.senders.len() {
            let index = (start + offset) % self.senders.len();
            let Some(next) = job.take() else {
                break;
            };
            match self.senders[index].try_send(next) {
                Ok(()) => {
                    return match response_receiver.recv_timeout(timeout) {
                        Ok(DnsResult::Addresses(addresses)) => Ok(addresses),
                        Ok(DnsResult::Rejected)
                        | Err(mpsc::RecvTimeoutError::Timeout)
                        | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            Err(FederatedOriginError::UnsafeDns)
                        }
                    };
                }
                Err(mpsc::TrySendError::Full(returned)) => job = Some(returned),
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        Err(FederatedOriginError::UnsafeDns)
    }
}

fn resolve_public_addresses(host: &str) -> Result<Vec<SocketAddr>, ()> {
    let addresses = (host, 443).to_socket_addrs().map_err(|_| ())?;
    let mut unique = BTreeSet::new();
    for address in addresses {
        if unique.len() >= MAX_DNS_ANSWERS || !ip_is_public(address.ip()) {
            return Err(());
        }
        unique.insert(address);
    }
    if unique.is_empty() {
        return Err(());
    }
    Ok(unique.into_iter().collect())
}

fn ip_is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_public(address),
        IpAddr::V6(address) => ipv6_is_public(address),
    }
}

fn ipv4_is_public(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && matches!(b, 18 | 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn ipv6_is_public(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || segments[0] & 0xe000 != 0x2000
    {
        return false;
    }
    // Documentation, Teredo, and 6to4 ranges are rejected conservatively;
    // they cannot be pinned to an ordinary globally routed origin address.
    !(segments[0] == 0x2001 && matches!(segments[1], 0 | 0x0db8)) && segments[0] != 0x2002
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        FEDERATION_CATALOG_SCHEMA, FEDERATION_ORIGIN_SCHEMA, FederationCatalog,
        FederationCoverageArea, FederationModelCapability, FederationPolicyLinks,
        FederationProductCapability, FederationQuotaSummary, FederationReplicationPolicy,
        FederationRetentionSummary, SignatureAlgorithm, sign_federation_catalog,
        sign_public_origin_descriptor,
    };

    const NOW: i64 = 2_000_000_000;

    fn encode_base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let first = chunk[0];
            let second = chunk.get(1).copied().unwrap_or(0);
            let third = chunk.get(2).copied().unwrap_or(0);
            encoded.push(ALPHABET[(first >> 2) as usize] as char);
            encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
            encoded.push(if chunk.len() >= 2 {
                ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
            } else {
                '='
            });
            encoded.push(if chunk.len() == 3 {
                ALPHABET[(third & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        encoded
    }

    fn public_key(key_id: &str, key: &SigningKey) -> FederationPublicKey {
        FederationPublicKey {
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: key_id.to_owned(),
            public_key_base64: encode_base64(key.verifying_key().as_bytes()),
            not_before_unix: NOW - 60,
            expires_unix: NOW + 3_600,
        }
    }

    fn descriptor(
        origin_id: &str,
        root: &str,
        key_id: &str,
        key: &SigningKey,
        west: i32,
        east: i32,
        levels: Vec<u16>,
    ) -> PublicOriginDescriptor {
        PublicOriginDescriptor {
            schema: FEDERATION_ORIGIN_SCHEMA.to_owned(),
            origin_id: origin_id.to_owned(),
            display_name: format!("{origin_id} weather lab"),
            https_base_url: root.to_owned(),
            health_path: "/v1/health".to_owned(),
            descriptor_signing_keys: vec![public_key(key_id, key)],
            object_signing_keys: vec![public_key(&format!("{origin_id}-objects"), key)],
            models: vec![FederationModelCapability {
                model: "hrrr".to_owned(),
                products: vec![FederationProductCapability {
                    product: "temperature".to_owned(),
                    queries: vec![FederationQueryCapability::ArbitraryDomainMap],
                    pressure_levels_hpa: levels,
                }],
            }],
            geographic_coverage: vec![FederationCoverageArea {
                coverage_id: "primary".to_owned(),
                west_longitude_e7: west,
                south_latitude_e7: 200_000_000,
                east_longitude_e7: east,
                north_latitude_e7: 600_000_000,
            }],
            retention: FederationRetentionSummary {
                queryable_run_hours: 48,
                immutable_object_hours: 168,
                published_case_hours: 720,
                previous_generations: 1,
            },
            api_schema_version: "v1".to_owned(),
            build_version: "test".to_owned(),
            issued_unix: NOW - 30,
            expires_unix: NOW + 1_800,
            policy_links: FederationPolicyLinks {
                attribution_url: format!("{root}/attribution"),
                acceptable_use_url: format!("{root}/policy"),
                privacy_url: format!("{root}/privacy"),
            },
            replication: FederationReplicationPolicy {
                accepts_replication: false,
                maximum_object_bytes: 0,
                monthly_ingress_bytes: 0,
                models: vec![],
            },
            quotas: FederationQuotaSummary {
                maximum_request_bytes: 64 * 1024,
                maximum_response_bytes: 16 * 1024 * 1024,
                requests_per_minute: 120,
                concurrent_requests: 4,
                monthly_egress_bytes: 1024 * 1024 * 1024,
            },
        }
    }

    struct Fixture {
        catalog_bytes: Vec<u8>,
        health_bytes: Vec<u8>,
        trust: FederationTrustStore,
    }

    fn fixture() -> Fixture {
        let catalog_key = SigningKey::from_bytes(&[7u8; 32]);
        let alpha_key = SigningKey::from_bytes(&[8u8; 32]);
        let beta_key = SigningKey::from_bytes(&[9u8; 32]);
        let limits = FederationLimits::default();
        let alpha = sign_public_origin_descriptor(
            descriptor(
                "alpha-lab",
                "https://alpha.weather.edu",
                "alpha-descriptor",
                &alpha_key,
                -1_300_000_000,
                -700_000_000,
                vec![500, 700, 850],
            ),
            "alpha-descriptor",
            &alpha_key,
            &limits,
        )
        .unwrap();
        let beta = sign_public_origin_descriptor(
            descriptor(
                "beta-lab",
                "https://beta.weather.edu",
                "beta-descriptor",
                &beta_key,
                -1_400_000_000,
                -600_000_000,
                vec![500, 700, 850],
            ),
            "beta-descriptor",
            &beta_key,
            &limits,
        )
        .unwrap();
        let catalog = sign_federation_catalog(
            FederationCatalog {
                schema: FEDERATION_CATALOG_SCHEMA.to_owned(),
                catalog_id: "bowecho-public-origins".to_owned(),
                generated_unix: NOW - 10,
                expires_unix: NOW + 900,
                origins: vec![alpha, beta],
            },
            "catalog-v1",
            &catalog_key,
            &limits,
        )
        .unwrap();
        let trust = FederationTrustConfig {
            catalog_signing_keys: vec![PinnedFederationKey {
                key_id: "catalog-v1".to_owned(),
                public_key_base64: encode_base64(catalog_key.verifying_key().as_bytes()),
            }],
            approved_origins: vec![
                ApprovedPublicOrigin {
                    origin_id: "alpha-lab".to_owned(),
                    descriptor_signing_keys: vec![PinnedFederationKey {
                        key_id: "alpha-descriptor".to_owned(),
                        public_key_base64: encode_base64(alpha_key.verifying_key().as_bytes()),
                    }],
                },
                ApprovedPublicOrigin {
                    origin_id: "beta-lab".to_owned(),
                    descriptor_signing_keys: vec![PinnedFederationKey {
                        key_id: "beta-descriptor".to_owned(),
                        public_key_base64: encode_base64(beta_key.verifying_key().as_bytes()),
                    }],
                },
            ],
            ..FederationTrustConfig::default()
        }
        .build()
        .unwrap();
        let health_bytes = serde_json::to_vec(&serde_json::json!({
            "schema": FEDERATION_HEALTH_SCHEMA,
            "monitor_enabled": true,
            "total_origins": 2,
            "healthy_origins": 2,
            "degraded_origins": 0,
            "quarantined_origins": 0,
            "unknown_origins": 0,
            "last_round_unix": NOW - 2,
            "origins": [
                {
                    "origin_id": "alpha-lab",
                    "state": "healthy",
                    "consecutive_failures": 0,
                    "quarantine_until_unix": null,
                    "last_probe_unix": NOW - 2,
                    "last_success_unix": NOW - 2
                },
                {
                    "origin_id": "beta-lab",
                    "state": "healthy",
                    "consecutive_failures": 0,
                    "quarantine_until_unix": null,
                    "last_probe_unix": NOW - 2,
                    "last_success_unix": NOW - 2
                }
            ]
        }))
        .unwrap();
        Fixture {
            catalog_bytes: serde_json::to_vec(&catalog).unwrap(),
            health_bytes,
            trust,
        }
    }

    fn client(trust: FederationTrustStore) -> FederatedOriginClient {
        FederatedOriginClient::new(FederatedOriginClientConfig::new(
            "https://authority.weather.net",
            "secret-token",
            trust,
        ))
        .unwrap()
    }

    fn selection(level: Option<u16>, bounds: FederationGeoBounds) -> PublicOriginSelection {
        PublicOriginSelection {
            model: "hrrr".to_owned(),
            product: "temperature".to_owned(),
            query: FederationQueryCapability::ArbitraryDomainMap,
            pressure_level_hpa: level,
            bounds: Some(bounds),
            minimum_response_bytes: 1024,
            require_replication: false,
        }
    }

    #[test]
    fn signed_catalog_health_capability_level_and_geography_select_exactly() {
        let fixture = fixture();
        let mut client = client(fixture.trust);
        client
            .install_snapshot(&fixture.catalog_bytes, &fixture.health_bytes, NOW)
            .unwrap();
        let candidates = client
            .candidates_at(
                &selection(
                    Some(700),
                    FederationGeoBounds {
                        west_longitude_e7: -1_200_000_000,
                        south_latitude_e7: 300_000_000,
                        east_longitude_e7: -800_000_000,
                        north_latitude_e7: 500_000_000,
                    },
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(FederatedOriginCandidate::origin_id)
                .collect::<Vec<_>>(),
            ["alpha-lab", "beta-lab"]
        );
        assert!(
            client
                .candidates_at(
                    &selection(
                        Some(925),
                        FederationGeoBounds {
                            west_longitude_e7: -1_200_000_000,
                            south_latitude_e7: 300_000_000,
                            east_longitude_e7: -800_000_000,
                            north_latitude_e7: 500_000_000,
                        },
                    ),
                    NOW,
                )
                .unwrap()
                .is_empty()
        );
        let beta_only = client
            .candidates_at(
                &selection(
                    Some(700),
                    FederationGeoBounds {
                        west_longitude_e7: -1_350_000_000,
                        south_latitude_e7: 300_000_000,
                        east_longitude_e7: -1_250_000_000,
                        north_latitude_e7: 500_000_000,
                    },
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(beta_only[0].origin_id(), "beta-lab");
    }

    #[test]
    fn catalog_tamper_unknown_health_origin_and_expiry_fail_closed_atomically() {
        let fixture = fixture();
        let mut client = client(fixture.trust);
        client
            .install_snapshot(&fixture.catalog_bytes, &fixture.health_bytes, NOW)
            .unwrap();
        let original_id = client
            .directory
            .as_ref()
            .unwrap()
            .catalog
            .catalog
            .catalog_id
            .clone();

        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fixture.catalog_bytes).unwrap();
        catalog["catalog"]["origins"][0]["descriptor"]["build_version"] =
            serde_json::json!("tampered");
        assert!(
            client
                .install_snapshot(
                    &serde_json::to_vec(&catalog).unwrap(),
                    &fixture.health_bytes,
                    NOW
                )
                .is_err()
        );
        assert_eq!(
            client
                .directory
                .as_ref()
                .unwrap()
                .catalog
                .catalog
                .catalog_id,
            original_id
        );

        let mut health: serde_json::Value = serde_json::from_slice(&fixture.health_bytes).unwrap();
        health["origins"][0]["origin_id"] = serde_json::json!("ordinary-relay-client");
        assert!(
            client
                .install_snapshot(
                    &fixture.catalog_bytes,
                    &serde_json::to_vec(&health).unwrap(),
                    NOW
                )
                .is_err()
        );
        assert!(
            client
                .candidates_at(
                    &selection(
                        Some(700),
                        FederationGeoBounds {
                            west_longitude_e7: -1_200_000_000,
                            south_latitude_e7: 300_000_000,
                            east_longitude_e7: -800_000_000,
                            north_latitude_e7: 500_000_000,
                        }
                    ),
                    NOW + 901,
                )
                .is_err()
        );
    }

    #[test]
    fn deterministic_local_failure_order_and_quarantine_advance_failover() {
        let fixture = fixture();
        let mut client = client(fixture.trust);
        client
            .install_snapshot(&fixture.catalog_bytes, &fixture.health_bytes, NOW)
            .unwrap();
        let request = selection(
            Some(700),
            FederationGeoBounds {
                west_longitude_e7: -1_200_000_000,
                south_latitude_e7: 300_000_000,
                east_longitude_e7: -800_000_000,
                north_latitude_e7: 500_000_000,
            },
        );
        assert_eq!(
            client.candidates_at(&request, NOW).unwrap()[0].origin_id(),
            "alpha-lab"
        );
        client.record_failure_at("alpha-lab", NOW).unwrap();
        assert_eq!(
            client.candidates_at(&request, NOW).unwrap()[0].origin_id(),
            "beta-lab"
        );
        client.record_failure_at("alpha-lab", NOW).unwrap();
        assert!(
            client
                .candidates_at(&request, NOW)
                .unwrap()
                .iter()
                .all(|candidate| candidate.origin_id() != "alpha-lab")
        );
        client.record_success("alpha-lab").unwrap();
        assert_eq!(
            client.candidates_at(&request, NOW).unwrap()[0].origin_id(),
            "alpha-lab"
        );
    }

    #[test]
    fn endpoints_are_https_same_origin_and_authority_secret_is_redacted() {
        let fixture = fixture();
        let config = FederatedOriginClientConfig::new(
            "https://authority.weather.net",
            "super-secret-token",
            fixture.trust,
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret-token"));
        assert!(debug.contains("[REDACTED]"));
        assert!(PublicOriginRoot::parse("http://weather.edu").is_err());
        assert!(PublicOriginRoot::parse("https://127.0.0.1").is_err());
        assert!(PublicOriginRoot::parse("https://weather.edu/path").is_err());
        assert!(safe_public_origin_url("https://weather.edu", "/v1/models?hrrr=1").is_ok());
        assert!(safe_public_origin_url("https://weather.edu", "//evil.test/v1").is_err());
        assert!(safe_public_origin_url("https://weather.edu", "/v1/%2e%2e/secret").is_err());
        assert!(!ipv4_is_public(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!ipv4_is_public(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(ipv4_is_public(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!ipv6_is_public(Ipv6Addr::LOCALHOST));
        assert!(ipv6_is_public("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn persisted_settings_convert_to_sendable_non_secret_runtime_trust() {
        fn assert_send<T: Send>() {}
        assert_send::<FederatedOriginClient>();

        let catalog_key = SigningKey::from_bytes(&[21; 32]);
        let origin_key = SigningKey::from_bytes(&[22; 32]);
        let settings = settings::FederationSettings {
            enabled: true,
            catalog_signing_keys: vec![settings::FederationPinnedKeySettings {
                key_id: "catalog-v1".to_owned(),
                public_key_base64: encode_base64(catalog_key.verifying_key().as_bytes()),
            }],
            approved_origins: vec![settings::FederationApprovedOriginSettings {
                origin_id: "university-lab".to_owned(),
                descriptor_signing_keys: vec![settings::FederationPinnedKeySettings {
                    key_id: "university-descriptor-v1".to_owned(),
                    public_key_base64: encode_base64(origin_key.verifying_key().as_bytes()),
                }],
            }],
            revoked_origin_ids: vec!["retired-lab".to_owned()],
            revoked_key_ids: vec!["retired-key".to_owned()],
        };
        assert!(settings.trust_ready());
        let trust = FederationTrustConfig::from_settings(&settings)
            .build()
            .unwrap();
        assert!(trust.catalog_keys.contains_key("catalog-v1"));
        assert!(trust.approved_origins.contains_key("university-lab"));
        assert!(trust.revoked_origin_ids.contains("retired-lab"));
        assert!(trust.revoked_key_ids.contains("retired-key"));
    }

    #[test]
    fn embedded_origin_key_cannot_self_bootstrap() {
        let fixture = fixture();
        let mut untrusted = fixture.trust;
        untrusted.approved_origins.remove("alpha-lab");
        let mut client = client(untrusted);
        assert!(
            client
                .install_snapshot(&fixture.catalog_bytes, &fixture.health_bytes, NOW)
                .is_err()
        );
    }
}
