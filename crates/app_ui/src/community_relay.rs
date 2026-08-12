//! Background adapter for explicit cold-historical Community Cache recovery.
//!
//! This module has no generic fetch entry point. Operational callers continue
//! to use `community_cache::CommunityCacheClient::fetch`, whose closed order is
//! local -> R2 -> authoritative HTTPS. The only command accepted here is an
//! exact origin-signed historical manifest, and its closed order is local ->
//! R2 -> encrypted TURN-only community recovery -> archival HTTPS.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rw_community_protocol::{
    ProtocolLimits, ShareRequest, SignedObjectManifest, TrustedSigningKeys,
    trusted_signing_keys_from_base64,
};
use rw_community_relay::{
    ADVERTISEMENT_SCHEMA, AddressFamily, AdvertisementReceipt, HistoricalRelayClient,
    HistoricalRelayOutcome, HistoricalRelayPolicy, HistoricalRelaySecurity,
    ProviderTurnAllocationFactory, RELAY_ADVERTISE_PATH, RELAY_HISTORICAL_LOOKUP_PATH,
    RELAY_NEXT_GRANT_PATH, RELAY_ROUTE_REGISTRATION_PATH, RELAY_SESSION_COMPLETE_PATH,
    RELAY_SESSION_FAIL_PATH, RELAY_SESSION_REVOKE_PATH, RELAY_TRANSPORT_GRANT_PATH,
    RelayAdvertiseRequest, RelayBrokerHttp, RelayCancellation, RelayError, RelayGrantPollRequest,
    RelayHistoricalLookupRequest, RelayReliabilityPolicy, RelayRoutePolicy,
    RelayRouteRegistrationReceipt, RelayRouteRegistrationRequest, RelaySessionCompletionRequest,
    RelaySessionFailureRequest, RelayTerminalResponse, RelayTransportGrantRequest,
    TRANSPORT_ROUTE_GRANT_SCHEMA,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, mpsc};

use crate::community_cache::{CommunityCacheClient, CommunityCacheError};

const RELAY_SIGNING_KEY_ID: &str = "rw-relay-v1";
const CONTROL_RESPONSE_LIMIT: usize = 256 * 1024;
const COMMAND_CAPACITY: usize = 8;
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SEED_REFRESH_INTERVAL: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const SHUTDOWN_WAIT_STEP: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommunityRelayPhase {
    Off,
    NeedsConfiguration,
    Idle,
    MeteredPause,
    Advertising,
    Serving,
    Recovering,
    FallingBack,
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommunityRelayStatus {
    pub phase: CommunityRelayPhase,
    pub retrieval_enabled: bool,
    pub seeding_enabled: bool,
    pub verified_seed_objects: usize,
    pub advertised_objects: usize,
    pub recovered_objects: u64,
    pub archival_fallbacks: u64,
    /// Stable closed code only. Provider bodies, URLs, credentials, session
    /// identifiers, allocation addresses, and participant state never enter
    /// app-visible status.
    pub last_failure_code: Option<&'static str>,
}

impl Default for CommunityRelayStatus {
    fn default() -> Self {
        Self {
            phase: CommunityRelayPhase::Off,
            retrieval_enabled: false,
            seeding_enabled: false,
            verified_seed_objects: 0,
            advertised_objects: 0,
            recovered_objects: 0,
            archival_fallbacks: 0,
            last_failure_code: None,
        }
    }
}

impl CommunityRelayStatus {
    pub(crate) fn summary(&self) -> &'static str {
        match self.phase {
            CommunityRelayPhase::Off => "Off (no relay traffic)",
            CommunityRelayPhase::NeedsConfiguration => {
                "Paused: configuration or vault token missing"
            }
            CommunityRelayPhase::Idle => "Ready: TURN-only encrypted historical recovery",
            CommunityRelayPhase::MeteredPause => {
                "Retrieval ready; seeding paused until this network is confirmed unmetered"
            }
            CommunityRelayPhase::Advertising => {
                "Publishing one verified cold-object availability record"
            }
            CommunityRelayPhase::Serving => {
                "Serving one verified object through the encrypted privacy relay"
            }
            CommunityRelayPhase::Recovering => {
                "Recovering an exact cold object through the encrypted privacy relay"
            }
            CommunityRelayPhase::FallingBack => {
                "Relay unavailable; trying the archival HTTPS origin"
            }
            CommunityRelayPhase::Stopping => "Stopping Community Cache background work",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoricalRecoveryResult {
    AlreadyLocal,
    RecoveredFromR2,
    RecoveredFromCommunity,
    RecoveredFromArchivalHttps,
    Unavailable,
    Cancelled,
}

fn fallback_result(archival_recovered: bool) -> HistoricalRecoveryResult {
    if archival_recovered {
        HistoricalRecoveryResult::RecoveredFromArchivalHttps
    } else {
        HistoricalRecoveryResult::Unavailable
    }
}

pub(crate) struct HistoricalRecoveryHandle {
    receiver: std_mpsc::Receiver<HistoricalRecoveryResult>,
    cancellation: CancellationFlag,
}

impl HistoricalRecoveryHandle {
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn wait_timeout(
        &self,
        timeout: Duration,
    ) -> Result<HistoricalRecoveryResult, std_mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Drop for HistoricalRecoveryHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

enum RuntimeCommand {
    Reconfigure {
        settings: settings::CommunityCacheSettings,
        cache_root: PathBuf,
        network_unmetered_confirmed: bool,
    },
    RecoverExactHistorical {
        request: Box<ShareRequest>,
        manifest: Box<SignedObjectManifest>,
        cancellation: CancellationFlag,
        configuration_cancellation: CancellationFlag,
        result: std_mpsc::Sender<HistoricalRecoveryResult>,
    },
    #[cfg(test)]
    InstallBlockingSeedProbe {
        started: std_mpsc::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    },
    #[cfg(test)]
    InstallBlockingRecoveryProbe {
        configuration_cancellation: CancellationFlag,
        started: std_mpsc::Sender<()>,
        cancelled: std_mpsc::Sender<()>,
    },
    #[cfg(test)]
    ResponsivenessProbe(std_mpsc::Sender<()>),
    Shutdown,
}

/// One bounded command queue and one current-thread async runtime. No network
/// work occurs on egui's thread, and shutdown cancellation wins over DNS,
/// broker HTTP, TURN allocation, and transfer futures.
pub(crate) struct CommunityRelayRuntime {
    commands: mpsc::Sender<RuntimeCommand>,
    status: Arc<Mutex<CommunityRelayStatus>>,
    cancellation: CancellationFlag,
    configuration_cancellation: Arc<Mutex<CancellationFlag>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct CommunityRelayDispatcher {
    commands: mpsc::Sender<RuntimeCommand>,
    configuration_cancellation: Arc<Mutex<CancellationFlag>>,
    #[cfg(test)]
    recovery_enqueue_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl fmt::Debug for CommunityRelayDispatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CommunityRelayDispatcher([bounded historical-only queue])")
    }
}

impl fmt::Debug for CommunityRelayRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommunityRelayRuntime")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl CommunityRelayRuntime {
    pub(crate) fn start(
        settings: &settings::CommunityCacheSettings,
        cache_root: PathBuf,
        network_unmetered_confirmed: bool,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let status = Arc::new(Mutex::new(CommunityRelayStatus::default()));
        let cancellation = CancellationFlag::default();
        let configuration_cancellation = Arc::new(Mutex::new(CancellationFlag::default()));
        let worker_status = status.clone();
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::Builder::new()
            .name("community-relay".into())
            .spawn(move || run_worker(receiver, worker_status, worker_cancellation))
            .ok();
        let runtime = Self {
            commands,
            status,
            cancellation,
            configuration_cancellation,
            worker,
        };
        runtime.reconfigure(settings, cache_root, network_unmetered_confirmed);
        runtime
    }

    pub(crate) fn reconfigure(
        &self,
        settings: &settings::CommunityCacheSettings,
        cache_root: PathBuf,
        network_unmetered_confirmed: bool,
    ) {
        let next_configuration = CancellationFlag::default();
        let previous_configuration = lock_unpoisoned(&self.configuration_cancellation).clone();
        // A settings change or the local Stop command immediately cancels an
        // in-flight recovery. The queued command then rebuilds trust/policy
        // state before the replacement token can admit another request.
        previous_configuration.cancel();
        if self
            .commands
            .try_send(RuntimeCommand::Reconfigure {
                settings: settings.clone(),
                cache_root,
                network_unmetered_confirmed,
            })
            .is_err()
        {
            mutate_status(&self.status, |status| {
                status.last_failure_code = Some("relay_command_busy");
            });
        } else {
            *lock_unpoisoned(&self.configuration_cancellation) = next_configuration;
        }
    }

    /// Explicit historical-only entry point. There is intentionally no
    /// operational/generic request method on this type.
    pub(crate) fn dispatcher(&self) -> CommunityRelayDispatcher {
        CommunityRelayDispatcher {
            commands: self.commands.clone(),
            configuration_cancellation: self.configuration_cancellation.clone(),
            #[cfg(test)]
            recovery_enqueue_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub(crate) fn status(&self) -> CommunityRelayStatus {
        lock_unpoisoned(&self.status).clone()
    }

    pub(crate) fn shutdown(&self) {
        mutate_status(&self.status, |status| {
            status.phase = CommunityRelayPhase::Stopping;
        });
        self.cancellation.cancel();
        lock_unpoisoned(&self.configuration_cancellation).cancel();
        let _ = self.commands.try_send(RuntimeCommand::Shutdown);
    }

    #[cfg(test)]
    fn shutdown_and_wait(&mut self, timeout: Duration) -> bool {
        self.shutdown();
        let deadline = Instant::now() + timeout;
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(SHUTDOWN_WAIT_STEP);
        }
        let Some(worker) = self.worker.take() else {
            return true;
        };
        if !worker.is_finished() {
            self.worker = Some(worker);
            return false;
        }
        worker.join().is_ok()
    }
}

impl CommunityRelayDispatcher {
    pub(crate) fn recover_exact_historical(
        &self,
        request: ShareRequest,
        manifest: SignedObjectManifest,
    ) -> Result<HistoricalRecoveryHandle, CommunityCacheError> {
        if manifest.manifest.request != request {
            return Err(CommunityCacheError::Response);
        }
        let cancellation = CancellationFlag::default();
        let configuration_cancellation = lock_unpoisoned(&self.configuration_cancellation).clone();
        let (result, receiver) = std_mpsc::channel();
        self.commands
            .try_send(RuntimeCommand::RecoverExactHistorical {
                request: Box::new(request),
                manifest: Box::new(manifest),
                cancellation: cancellation.clone(),
                configuration_cancellation,
                result,
            })
            .map_err(|_| CommunityCacheError::Quota)?;
        #[cfg(test)]
        self.recovery_enqueue_count.fetch_add(1, Ordering::Relaxed);
        Ok(HistoricalRecoveryHandle {
            receiver,
            cancellation,
        })
    }

    #[cfg(test)]
    pub(crate) fn recovery_enqueue_count_for_test(&self) -> usize {
        self.recovery_enqueue_count.load(Ordering::Relaxed)
    }
}

impl Drop for CommunityRelayRuntime {
    fn drop(&mut self) {
        self.shutdown();
        // Never make application exit wait on a network stack. Cancellation
        // drops the in-flight async future; a finished worker is joined, and
        // an unfinished OS handle is deliberately detached.
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Default)]
struct CancellationFlag {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationFlag {
    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    async fn cancelled(&self) {
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        self.notify.notified().await;
    }
}

impl RelayCancellation for CancellationFlag {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

struct CombinedCancellation<'a> {
    global: &'a CancellationFlag,
    request: &'a CancellationFlag,
    configuration: &'a CancellationFlag,
}

impl RelayCancellation for CombinedCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.global.is_cancelled()
            || self.request.is_cancelled()
            || self.configuration.is_cancelled()
    }
}

type RelayClient = HistoricalRelayClient<RelayHttpBroker, ProviderTurnAllocationFactory>;

struct WorkerState {
    cache: CommunityCacheClient,
    relay: Arc<RelayClient>,
    seed_lane: SeedLane,
}

#[derive(Clone)]
struct SeedLane {
    cache: CommunityCacheClient,
    relay: Arc<RelayClient>,
    seeding_enabled: bool,
    metered_pause: bool,
    schedule: Arc<Mutex<SeedSchedule>>,
}

struct SeedSchedule {
    seed_candidates: Vec<rw_community_relay::VerifiedSeedObject>,
    next_seed: usize,
    next_refresh: Instant,
}

fn run_worker(
    receiver: mpsc::Receiver<RuntimeCommand>,
    status: Arc<Mutex<CommunityRelayStatus>>,
    cancellation: CancellationFlag,
) {
    // Two bounded scheduler workers keep the command/cancellation loop
    // responsive even if a seed-store lookup performs synchronous verified
    // CAS work while serving a grant.
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("community-relay-async")
        .enable_all()
        .build()
    else {
        mutate_status(&status, |value| {
            value.phase = CommunityRelayPhase::NeedsConfiguration;
            value.last_failure_code = Some("relay_runtime_unavailable");
        });
        return;
    };
    let retained_clients = runtime.block_on(worker_loop(receiver, status, cancellation));
    drop(runtime);
    // reqwest's blocking client owns an internal runtime and must be dropped
    // outside Tokio's async context. This is especially important when relay
    // configuration is incomplete and only the conventional fallback client
    // was retained.
    drop(retained_clients);
}

async fn worker_loop(
    mut receiver: mpsc::Receiver<RuntimeCommand>,
    status: Arc<Mutex<CommunityRelayStatus>>,
    cancellation: CancellationFlag,
) -> (Option<WorkerState>, Option<CommunityCacheClient>) {
    let mut state: Option<WorkerState> = None;
    // The conventional cache client is retained independently of TURN
    // readiness. An exact signed historical request must still be able to do
    // local -> R2 -> archival HTTPS when relay opt-in, credentials, routing,
    // or quota make the peer-assisted lane unavailable.
    let mut fallback_cache: Option<CommunityCacheClient> = None;
    let mut seed_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut ticker = tokio::time::interval(BACKGROUND_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => break,
            command = receiver.recv() => match command {
                Some(RuntimeCommand::Reconfigure { settings, cache_root, network_unmetered_confirmed }) => {
                    if let Some(task) = seed_task.take() {
                        task.abort();
                        let _ = task.await;
                    }
                    // A blocking reqwest client owns an internal runtime. Both
                    // construction and the final drop therefore live on the
                    // blocking pool, never inside this async dispatcher.
                    let retired = (state.take(), fallback_cache.take());
                    let _ = tokio::task::spawn_blocking(move || drop(retired)).await;
                    let build_status = status.clone();
                    match tokio::task::spawn_blocking(move || {
                        let fallback_cache =
                            CommunityCacheClient::from_settings(&settings, cache_root).ok();
                        let state = build_worker_state(
                            &settings,
                            fallback_cache.clone(),
                            network_unmetered_confirmed,
                            &build_status,
                        );
                        (state, fallback_cache)
                    })
                    .await
                    {
                        Ok((new_state, new_fallback_cache)) => {
                            state = new_state;
                            fallback_cache = new_fallback_cache;
                        }
                        Err(_) => {
                            state = None;
                            fallback_cache = None;
                            let _ = relay_configuration_failed(
                                &status,
                                "relay_cache_configuration_invalid",
                            );
                        }
                    }
                }
                Some(RuntimeCommand::RecoverExactHistorical {
                    request,
                    manifest,
                    cancellation: request_cancel,
                    configuration_cancellation,
                    result,
                }) => {
                    // User-initiated recovery always preempts passive seeding.
                    // Aborting the seed task drops its shared concurrency
                    // reservation before the download path is admitted.
                    if let Some(task) = seed_task.take() {
                        task.abort();
                        let _ = task.await;
                    }
                    let outcome = match state.as_mut() {
                        Some(state) => recover_exact_historical(
                            state,
                            *request,
                            *manifest,
                            &request_cancel,
                            &configuration_cancellation,
                            &cancellation,
                            &status,
                        ).await,
                        None => match fallback_cache.clone() {
                            Some(cache) => recover_without_relay(
                                cache,
                                *request,
                                *manifest,
                                &request_cancel,
                                &configuration_cancellation,
                                &cancellation,
                                &status,
                            ).await,
                            None => HistoricalRecoveryResult::Unavailable,
                        },
                    };
                    let _ = result.send(outcome);
                }
                #[cfg(test)]
                Some(RuntimeCommand::InstallBlockingSeedProbe { started, release }) => {
                    if let Some(task) = seed_task.take() {
                        task.abort();
                    }
                    seed_task = Some(tokio::spawn(async move {
                        let _ = started.send(());
                        let _ = release.await;
                    }));
                }
                #[cfg(test)]
                Some(RuntimeCommand::InstallBlockingRecoveryProbe {
                    configuration_cancellation,
                    started,
                    cancelled,
                }) => {
                    let _ = started.send(());
                    tokio::select! {
                        _ = cancellation.cancelled() => {},
                        _ = configuration_cancellation.cancelled() => {},
                    }
                    let _ = cancelled.send(());
                }
                #[cfg(test)]
                Some(RuntimeCommand::ResponsivenessProbe(response)) => {
                    let _ = response.send(());
                }
                Some(RuntimeCommand::Shutdown) | None => break,
            },
            _ = ticker.tick() => {
                if seed_task.as_ref().is_some_and(tokio::task::JoinHandle::is_finished)
                    && let Some(task) = seed_task.take()
                {
                    let _ = task.await;
                }
                if seed_task.is_none()
                    && let Some(state) = state.as_ref()
                {
                    let lane = state.seed_lane.clone();
                    let task_cancellation = cancellation.clone();
                    let task_status = status.clone();
                    // Seeding is deliberately detached from the command
                    // dispatcher: a 90-second TURN session cannot hold up a
                    // recovery request, reconfiguration, or shutdown.
                    seed_task = Some(tokio::spawn(async move {
                        service_seed_lane(lane, &task_cancellation, &task_status).await;
                    }));
                }
            }
        }
    }
    if let Some(task) = seed_task.take() {
        task.abort();
        let _ = task.await;
    }
    mutate_status(&status, |value| {
        value.phase = CommunityRelayPhase::Off;
        value.retrieval_enabled = false;
        value.seeding_enabled = false;
    });
    (state, fallback_cache)
}

fn build_worker_state(
    settings: &settings::CommunityCacheSettings,
    cache: Option<CommunityCacheClient>,
    network_unmetered_confirmed: bool,
    status: &Arc<Mutex<CommunityRelayStatus>>,
) -> Option<WorkerState> {
    if !settings.historical_relay_ready() {
        mutate_status(status, |value| {
            *value = CommunityRelayStatus::default();
            value.phase = if settings.historical_relay_enabled {
                CommunityRelayPhase::NeedsConfiguration
            } else {
                CommunityRelayPhase::Off
            };
        });
        return None;
    }
    let cache = match cache {
        Some(cache) => cache,
        None => return relay_configuration_failed(status, "relay_cache_configuration_invalid"),
    };
    let Some(bearer_token) = crate::community_credentials::load_credentials()
        .ok()
        .flatten()
        .map(|credentials| credentials.bearer_token().to_owned())
    else {
        mutate_status(status, |value| {
            *value = CommunityRelayStatus::default();
            value.phase = CommunityRelayPhase::NeedsConfiguration;
            value.last_failure_code = Some("relay_credentials_missing");
        });
        return None;
    };
    let relay_keys = match trusted_relay_keys(settings) {
        Ok(keys) => keys,
        Err(_) => return relay_configuration_failed(status, "relay_keyring_invalid"),
    };
    let route_policy =
        match RelayRoutePolicy::from_audited_cidrs(settings.relay_provider_allocation_cidrs.iter())
        {
            Ok(policy) => policy,
            Err(_) => return relay_configuration_failed(status, "relay_allocation_range_invalid"),
        };
    let broker = match RelayHttpBroker::new(&settings.origin_url, bearer_token) {
        Ok(broker) => broker,
        Err(_) => return relay_configuration_failed(status, "relay_broker_configuration_invalid"),
    };
    let upload_allowance = cache.remaining_relay_upload_bytes();
    let download_allowance = cache.remaining_relay_download_bytes();
    if download_allowance == 0 {
        mutate_status(status, |value| {
            *value = CommunityRelayStatus::default();
            value.phase = CommunityRelayPhase::Idle;
            value.last_failure_code = Some(RelayError::QuotaReached.public_code());
        });
        return None;
    }
    let effective_seeding = settings.historical_relay_seeding_enabled && upload_allowance > 0;
    let metered_pause = effective_seeding
        && settings.pause_sharing_on_metered_networks
        && !network_unmetered_confirmed;
    let policy = HistoricalRelayPolicy {
        opted_in: true,
        seeding_opted_in: effective_seeding,
        metered_network: metered_pause,
        allow_metered_seeding: false,
        disk_allowance_bytes: gib_to_bytes(u32::from(settings.disk_allowance_gib)),
        upload_allowance_bytes: upload_allowance,
        download_allowance_bytes: download_allowance,
        route_poll_attempts: 60,
        route_poll_interval: Duration::from_millis(250),
        session_timeout: Duration::from_secs(90),
        reliability: RelayReliabilityPolicy::default(),
    };
    let security = HistoricalRelaySecurity {
        trusted_origin_keys: cache.relay_origin_keys(),
        trusted_relay_keys: relay_keys,
        route_policy,
        limits: ProtocolLimits::default(),
    };
    let relay = match HistoricalRelayClient::new(
        broker,
        ProviderTurnAllocationFactory {
            family: AddressFamily::Ipv4,
        },
        security,
        policy,
    ) {
        Ok(relay) => Arc::new(relay),
        Err(_) => return relay_configuration_failed(status, "relay_security_policy_invalid"),
    };
    cache.prune_untrusted_relay_entries();
    let seed_candidates = if effective_seeding {
        cache.relay_seed_candidates()
    } else {
        Vec::new()
    };
    mutate_status(status, |value| {
        let recovered_objects = value.recovered_objects;
        let archival_fallbacks = value.archival_fallbacks;
        *value = CommunityRelayStatus {
            phase: if metered_pause {
                CommunityRelayPhase::MeteredPause
            } else {
                CommunityRelayPhase::Idle
            },
            retrieval_enabled: true,
            seeding_enabled: effective_seeding && !metered_pause,
            verified_seed_objects: seed_candidates.len(),
            advertised_objects: 0,
            recovered_objects,
            archival_fallbacks,
            last_failure_code: None,
        };
    });
    let seed_lane = SeedLane {
        cache: cache.clone(),
        relay: relay.clone(),
        seeding_enabled: effective_seeding,
        metered_pause,
        schedule: Arc::new(Mutex::new(SeedSchedule {
            seed_candidates,
            next_seed: 0,
            next_refresh: Instant::now() + SEED_REFRESH_INTERVAL,
        })),
    };
    Some(WorkerState {
        cache,
        relay,
        seed_lane,
    })
}

fn relay_configuration_failed(
    status: &Arc<Mutex<CommunityRelayStatus>>,
    code: &'static str,
) -> Option<WorkerState> {
    mutate_status(status, |value| {
        *value = CommunityRelayStatus::default();
        value.phase = CommunityRelayPhase::NeedsConfiguration;
        value.last_failure_code = Some(code);
    });
    None
}

async fn service_seed_lane(
    lane: SeedLane,
    cancellation: &CancellationFlag,
    status: &Arc<Mutex<CommunityRelayStatus>>,
) {
    if cancellation.is_cancelled() || !lane.seeding_enabled || lane.metered_pause {
        return;
    }
    let refresh_due = Instant::now() >= lock_unpoisoned(&lane.schedule).next_refresh;
    if refresh_due {
        let cache = lane.cache.clone();
        let refreshed = tokio::task::spawn_blocking(move || {
            cache.prune_untrusted_relay_entries();
            cache.relay_seed_candidates()
        });
        let refreshed = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = refreshed => result.ok(),
        };
        let Some(refreshed) = refreshed else {
            mutate_status(status, |value| {
                value.last_failure_code = Some("relay_seed_refresh_failed");
            });
            return;
        };
        let refreshed_len = refreshed.len();
        {
            let mut schedule = lock_unpoisoned(&lane.schedule);
            schedule.seed_candidates = refreshed;
            schedule.next_seed = 0;
            schedule.next_refresh = Instant::now() + SEED_REFRESH_INTERVAL;
        }
        mutate_status(status, |value| {
            value.verified_seed_objects = refreshed_len;
            value.advertised_objects = 0;
        });
    }
    let object = {
        let mut schedule = lock_unpoisoned(&lane.schedule);
        let object = schedule.seed_candidates.get(schedule.next_seed).cloned();
        if object.is_some() {
            schedule.next_seed = schedule.next_seed.saturating_add(1);
        }
        object
    };
    if let Some(object) = object {
        mutate_status(status, |value| {
            value.phase = CommunityRelayPhase::Advertising
        });
        let advertised = tokio::select! {
            _ = cancellation.cancelled() => return,
            result = lane.relay.advertise_verified(&object, unix_now()) => result,
        };
        match advertised {
            Ok(_) => mutate_status(status, |value| {
                value.advertised_objects = value.advertised_objects.saturating_add(1);
                value.last_failure_code = None;
            }),
            Err(error) => mutate_status(status, |value| {
                value.last_failure_code = Some(error.public_code());
            }),
        }
    }
    let Ok(_transfer) = lane.cache.begin_relay_transfer() else {
        mutate_status(status, |value| {
            value.phase = CommunityRelayPhase::Idle;
            value.last_failure_code = Some(RelayError::QuotaReached.public_code());
        });
        return;
    };
    mutate_status(status, |value| value.phase = CommunityRelayPhase::Serving);
    let served = tokio::select! {
        _ = cancellation.cancelled() => return,
        result = lane.relay.serve_one(&lane.cache, unix_now(), cancellation) => result,
    };
    match served {
        Ok(_) => mutate_status(status, |value| value.last_failure_code = None),
        Err(error) if error != RelayError::NotAvailable => mutate_status(status, |value| {
            value.last_failure_code = Some(error.public_code());
        }),
        Err(_) => {}
    }
    mutate_status(status, |value| value.phase = CommunityRelayPhase::Idle);
}

async fn recover_exact_historical(
    state: &mut WorkerState,
    request: ShareRequest,
    manifest: SignedObjectManifest,
    request_cancel: &CancellationFlag,
    configuration_cancel: &CancellationFlag,
    global_cancel: &CancellationFlag,
    status: &Arc<Mutex<CommunityRelayStatus>>,
) -> HistoricalRecoveryResult {
    if request_cancel.is_cancelled()
        || configuration_cancel.is_cancelled()
        || global_cancel.is_cancelled()
    {
        return HistoricalRecoveryResult::Cancelled;
    }
    let cache = state.cache.clone();
    let request_for_preflight = request.clone();
    let preflight = tokio::task::spawn_blocking(move || {
        if cache.has_verified_local_object(&request_for_preflight)? {
            return Ok::<_, CommunityCacheError>(HistoricalRecoveryResult::AlreadyLocal);
        }
        if cache.recover_r2_hot_object(&request_for_preflight)? {
            return Ok(HistoricalRecoveryResult::RecoveredFromR2);
        }
        Ok(HistoricalRecoveryResult::Unavailable)
    });
    let preflight = tokio::select! {
        _ = global_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = request_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = configuration_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        result = preflight => result,
    };
    match preflight {
        Ok(Ok(HistoricalRecoveryResult::AlreadyLocal)) => {
            return HistoricalRecoveryResult::AlreadyLocal;
        }
        Ok(Ok(HistoricalRecoveryResult::RecoveredFromR2)) => {
            return HistoricalRecoveryResult::RecoveredFromR2;
        }
        Ok(Ok(_)) => {}
        _ => return HistoricalRecoveryResult::Unavailable,
    }

    mutate_status(status, |value| {
        value.phase = CommunityRelayPhase::Recovering
    });
    let combined = CombinedCancellation {
        global: global_cancel,
        request: request_cancel,
        configuration: configuration_cancel,
    };
    let relay_reservation = state
        .cache
        .reserve_relay_download(manifest.manifest.encoded_size);
    let recovered = if relay_reservation.is_ok() {
        let relay = state
            .relay
            .recover_historical(&request, &manifest, unix_now(), &combined);
        tokio::select! {
            _ = global_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
            _ = request_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
            _ = configuration_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
            result = relay => result,
        }
    } else {
        Err(RelayError::QuotaReached)
    };
    if let Ok(HistoricalRelayOutcome::Recovered(encoded)) = recovered {
        if state
            .cache
            .accept_relay_recovery(&request, &manifest, encoded)
            .is_ok()
        {
            mutate_status(status, |value| {
                value.phase = CommunityRelayPhase::Idle;
                value.recovered_objects = value.recovered_objects.saturating_add(1);
                value.last_failure_code = None;
            });
            return HistoricalRecoveryResult::RecoveredFromCommunity;
        }
        mutate_status(status, |value| {
            value.last_failure_code = Some(RelayError::UntrustedObject.public_code());
        });
    } else if let Err(error) = recovered {
        mutate_status(status, |value| {
            value.last_failure_code = Some(error.public_code());
        });
    }
    drop(relay_reservation);

    // Any relay miss/failure is non-terminal. The ordinary authenticated
    // archival HTTPS origin gets one exact signed-object attempt.
    mutate_status(status, |value| {
        value.phase = CommunityRelayPhase::FallingBack
    });
    let cache = state.cache.clone();
    let archival_request = request.clone();
    let archival_manifest = manifest.clone();
    let archival = tokio::task::spawn_blocking(move || {
        cache.recover_archival_https(&archival_request, &archival_manifest)
    });
    let archival = tokio::select! {
        _ = global_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = request_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = configuration_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        result = archival => result,
    };
    mutate_status(status, |value| {
        value.phase = CommunityRelayPhase::Idle;
        value.archival_fallbacks = value.archival_fallbacks.saturating_add(1);
    });
    fallback_result(matches!(archival, Ok(Ok(()))))
}

/// Conventional fallback for an exact signed historical identity when the
/// TURN lane is not currently admissible. This function deliberately has no
/// broker or allocation object in scope, so disabled/misconfigured relay can
/// never suppress the final authoritative HTTPS attempt.
async fn recover_without_relay(
    cache: CommunityCacheClient,
    request: ShareRequest,
    manifest: SignedObjectManifest,
    request_cancel: &CancellationFlag,
    configuration_cancel: &CancellationFlag,
    global_cancel: &CancellationFlag,
    status: &Arc<Mutex<CommunityRelayStatus>>,
) -> HistoricalRecoveryResult {
    if request_cancel.is_cancelled()
        || configuration_cancel.is_cancelled()
        || global_cancel.is_cancelled()
    {
        return HistoricalRecoveryResult::Cancelled;
    }
    let preflight_cache = cache.clone();
    let preflight_request = request.clone();
    let preflight = tokio::task::spawn_blocking(move || {
        if preflight_cache.has_verified_local_object(&preflight_request)? {
            return Ok::<_, CommunityCacheError>(HistoricalRecoveryResult::AlreadyLocal);
        }
        if preflight_cache.recover_r2_hot_object(&preflight_request)? {
            return Ok(HistoricalRecoveryResult::RecoveredFromR2);
        }
        Ok(HistoricalRecoveryResult::Unavailable)
    });
    let preflight = tokio::select! {
        _ = global_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = request_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = configuration_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        result = preflight => result,
    };
    match preflight {
        Ok(Ok(HistoricalRecoveryResult::AlreadyLocal)) => {
            return HistoricalRecoveryResult::AlreadyLocal;
        }
        Ok(Ok(HistoricalRecoveryResult::RecoveredFromR2)) => {
            return HistoricalRecoveryResult::RecoveredFromR2;
        }
        Ok(Ok(_)) => {}
        _ => return HistoricalRecoveryResult::Unavailable,
    }

    let previous_phase = lock_unpoisoned(status).phase;
    mutate_status(status, |value| {
        value.phase = CommunityRelayPhase::FallingBack
    });
    let archival =
        tokio::task::spawn_blocking(move || cache.recover_archival_https(&request, &manifest));
    let archival = tokio::select! {
        _ = global_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = request_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        _ = configuration_cancel.cancelled() => return HistoricalRecoveryResult::Cancelled,
        result = archival => result,
    };
    mutate_status(status, |value| {
        value.phase = previous_phase;
        value.archival_fallbacks = value.archival_fallbacks.saturating_add(1);
    });
    fallback_result(matches!(archival, Ok(Ok(()))))
}

#[derive(Clone)]
struct RelayHttpBroker {
    base_url: String,
    bearer_token: Arc<str>,
    http: reqwest::Client,
}

impl fmt::Debug for RelayHttpBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayHttpBroker([redacted authenticated HTTPS authority])")
    }
}

impl RelayHttpBroker {
    fn new(base_url: &str, bearer_token: String) -> Result<Self, RelayError> {
        if !settings::public_https_base_url_is_valid(base_url) || bearer_token.trim().is_empty() {
            return Err(RelayError::SecurityGate);
        }
        let http = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| RelayError::ProviderUnavailable)?;
        Ok(Self {
            base_url: base_url.trim().trim_end_matches('/').to_owned(),
            bearer_token: Arc::from(bearer_token),
            http,
        })
    }

    async fn post<T: Serialize>(
        &self,
        path: &'static str,
        value: &T,
    ) -> Result<Vec<u8>, RelayError> {
        if !known_broker_path(path) {
            return Err(RelayError::SecurityGate);
        }
        let body = serde_json::to_vec(value).map_err(|_| RelayError::CredentialInvalid)?;
        if body.is_empty() || body.len() > CONTROL_RESPONSE_LIMIT {
            return Err(RelayError::CredentialInvalid);
        }
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::CACHE_CONTROL, "no-store")
            .bearer_auth(self.bearer_token.as_ref())
            .body(body)
            .send()
            .await
            .map_err(|_| RelayError::ProviderUnavailable)?;
        match response.status().as_u16() {
            200..=299 => read_control_response(response).await,
            404 => Err(RelayError::NotAvailable),
            401 | 403 => Err(RelayError::CredentialInvalid),
            429 => Err(RelayError::QuotaReached),
            _ => Err(RelayError::ProviderUnavailable),
        }
    }

    async fn post_typed<TRequest: Serialize, TResponse: DeserializeOwned>(
        &self,
        path: &'static str,
        request: &TRequest,
    ) -> Result<TResponse, RelayError> {
        let bytes = self.post(path, request).await?;
        serde_json::from_slice(&bytes).map_err(|_| RelayError::CredentialInvalid)
    }
}

#[async_trait]
impl RelayBrokerHttp for RelayHttpBroker {
    async fn historical_lookup(
        &self,
        request: RelayHistoricalLookupRequest,
    ) -> Result<Vec<u8>, RelayError> {
        self.post(RELAY_HISTORICAL_LOOKUP_PATH, &request).await
    }

    async fn advertise(
        &self,
        request: RelayAdvertiseRequest,
    ) -> Result<AdvertisementReceipt, RelayError> {
        let expected_hash = request.signed_manifest.manifest.object_sha256.clone();
        let receipt: AdvertisementReceipt = self.post_typed(RELAY_ADVERTISE_PATH, &request).await?;
        if receipt.schema != ADVERTISEMENT_SCHEMA
            || receipt.object_sha256 != expected_hash
            || receipt.expires_unix <= unix_now()
        {
            return Err(RelayError::ProviderRejected);
        }
        Ok(receipt)
    }

    async fn next_grant(&self, request: RelayGrantPollRequest) -> Result<Vec<u8>, RelayError> {
        self.post(RELAY_NEXT_GRANT_PATH, &request).await
    }

    async fn register_route(
        &self,
        request: RelayRouteRegistrationRequest,
    ) -> Result<RelayRouteRegistrationReceipt, RelayError> {
        let expected_session = request.credential.claims.session_id.clone();
        let expected_role = match request.credential.claims.direction {
            rw_community_protocol::RelayDirection::Upload => {
                rw_community_relay::RelayRole::Uploader
            }
            rw_community_protocol::RelayDirection::Download => {
                rw_community_relay::RelayRole::Downloader
            }
        };
        let receipt: RelayRouteRegistrationReceipt = self
            .post_typed(RELAY_ROUTE_REGISTRATION_PATH, &request)
            .await?;
        if receipt.schema != TRANSPORT_ROUTE_GRANT_SCHEMA
            || receipt.session_id != expected_session
            || receipt.role != expected_role
        {
            return Err(RelayError::ProviderRejected);
        }
        Ok(receipt)
    }

    async fn transport_grant(
        &self,
        request: RelayTransportGrantRequest,
    ) -> Result<Vec<u8>, RelayError> {
        self.post(RELAY_TRANSPORT_GRANT_PATH, &request).await
    }

    async fn complete(
        &self,
        request: RelaySessionCompletionRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        self.post_typed(RELAY_SESSION_COMPLETE_PATH, &request).await
    }

    async fn fail(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        self.post_typed(RELAY_SESSION_FAIL_PATH, &request).await
    }

    async fn revoke(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError> {
        self.post_typed(RELAY_SESSION_REVOKE_PATH, &request).await
    }
}

async fn read_control_response(mut response: reqwest::Response) -> Result<Vec<u8>, RelayError> {
    if response
        .content_length()
        .is_some_and(|length| length == 0 || length > CONTROL_RESPONSE_LIMIT as u64)
    {
        return Err(RelayError::ProviderRejected);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| RelayError::ProviderUnavailable)?
    {
        let next = body
            .len()
            .checked_add(chunk.len())
            .ok_or(RelayError::ProviderRejected)?;
        if next > CONTROL_RESPONSE_LIMIT {
            return Err(RelayError::ProviderRejected);
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Err(RelayError::ProviderRejected);
    }
    Ok(body)
}

fn known_broker_path(path: &str) -> bool {
    matches!(
        path,
        RELAY_ADVERTISE_PATH
            | RELAY_HISTORICAL_LOOKUP_PATH
            | RELAY_NEXT_GRANT_PATH
            | RELAY_ROUTE_REGISTRATION_PATH
            | RELAY_TRANSPORT_GRANT_PATH
            | RELAY_SESSION_COMPLETE_PATH
            | RELAY_SESSION_FAIL_PATH
            | RELAY_SESSION_REVOKE_PATH
    )
}

fn trusted_relay_keys(
    settings: &settings::CommunityCacheSettings,
) -> Result<TrustedSigningKeys, RelayError> {
    if !settings::community_relay_keyring_is_valid(
        &settings.relay_public_key_base64,
        &settings.trusted_relay_signing_keys,
    ) {
        return Err(RelayError::SecurityGate);
    }
    let mut entries = Vec::new();
    if !settings.relay_public_key_base64.trim().is_empty() {
        entries.push((
            RELAY_SIGNING_KEY_ID.to_owned(),
            settings.relay_public_key_base64.clone(),
        ));
    }
    for entry in &settings.trusted_relay_signing_keys {
        let (key_id, encoded) = entry.split_once(':').ok_or(RelayError::SecurityGate)?;
        entries.push((key_id.to_owned(), encoded.to_owned()));
    }
    trusted_signing_keys_from_base64(entries).map_err(|_| RelayError::SecurityGate)
}

fn gib_to_bytes(value: u32) -> u64 {
    u64::from(value).saturating_mul(1024 * 1024 * 1024)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mutate_status(
    status: &Arc<Mutex<CommunityRelayStatus>>,
    mutate: impl FnOnce(&mut CommunityRelayStatus),
) {
    mutate(&mut lock_unpoisoned(status));
}

// Compile-time separation: operational dispatch has no relay branch and this
// module exposes only `recover_exact_historical`.
const _: rw_community_relay::OperationalFallback = rw_community_relay::after_operational_r2_miss();

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const KEY_B: &str = "//////////////////////////////////////////8=";
    const ORIGIN_PUBLIC_KEY: &str = "0EqyMnQrtKs6E2i9RhXk5tAiSrcaAWuvhSCjMsl3hzc=";

    fn test_request() -> ShareRequest {
        ShareRequest {
            schema: rw_community_protocol::REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812_00z".into(),
            snapshot_id: "1".repeat(64),
            grid_hash: "2".repeat(64),
            variables: vec!["temperature_iso".into(), "temperature_2m".into()],
            query: rw_community_protocol::ShareQuery::Profile {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                storage_slot: 1,
                valid_unix: 1_800_000_000,
                pressure_variables: vec!["temperature_iso".into()],
                surface_variables: vec!["temperature_2m".into()],
                pressure_levels_hpa: vec![],
            },
            recipe: rw_community_protocol::RecipeIdentity {
                recipe_id: "native-profile".into(),
                recipe_version: "1".into(),
                parameters: std::collections::BTreeMap::new(),
            },
            source_provenance: vec![rw_community_protocol::SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                roles: vec!["pressure".into()],
                products: vec!["wrfprs".into()],
            }],
            publication: rw_community_protocol::PublicationGrant {
                data_origin: rw_community_protocol::DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
        .normalized()
    }

    fn test_manifest(request: &ShareRequest) -> SignedObjectManifest {
        let encoded = b"cold historical profile";
        let now = unix_now();
        rw_community_protocol::sign_object_manifest(
            rw_community_protocol::ObjectManifest {
                schema: rw_community_protocol::OBJECT_SCHEMA.into(),
                request: request.clone(),
                request_sha256: rw_community_protocol::request_sha256(request).unwrap(),
                object_sha256: rw_community_protocol::object_sha256(encoded),
                content_type: "application/json".into(),
                compression: rw_community_protocol::Compression::None,
                encoded_size: encoded.len() as u64,
                decoded_size: encoded.len() as u64,
                attributions: vec![],
                modification_notices: vec![],
                created_unix: now.saturating_sub(60),
                expires_unix: now.saturating_add(3_600),
            },
            "rw-origin-v1",
            &ed25519_dalek::SigningKey::from_bytes(&[17; 32]),
        )
        .unwrap()
    }

    #[test]
    fn relay_key_rotation_is_explicit_and_unknown_ids_stay_untrusted() {
        let settings = settings::CommunityCacheSettings {
            relay_public_key_base64: KEY_A.into(),
            trusted_relay_signing_keys: vec![format!("rw-relay-v2:{KEY_B}")],
            ..Default::default()
        };
        let keys = trusted_relay_keys(&settings).unwrap();
        assert!(keys.contains_key("rw-relay-v1"));
        assert!(keys.contains_key("rw-relay-v2"));
        assert!(!keys.contains_key("rw-relay-v3"));

        let removed = settings::CommunityCacheSettings {
            relay_public_key_base64: String::new(),
            ..settings
        };
        let keys = trusted_relay_keys(&removed).unwrap();
        assert!(!keys.contains_key("rw-relay-v1"));
        assert!(keys.contains_key("rw-relay-v2"));
    }

    #[test]
    fn broker_surface_is_fixed_and_never_accepts_peer_or_arbitrary_urls() {
        for path in [
            RELAY_ADVERTISE_PATH,
            RELAY_HISTORICAL_LOOKUP_PATH,
            RELAY_NEXT_GRANT_PATH,
            RELAY_ROUTE_REGISTRATION_PATH,
            RELAY_TRANSPORT_GRANT_PATH,
            RELAY_SESSION_COMPLETE_PATH,
            RELAY_SESSION_FAIL_PATH,
            RELAY_SESSION_REVOKE_PATH,
        ] {
            assert!(known_broker_path(path));
        }
        assert!(!known_broker_path("https://192.0.2.1/peer"));
        assert!(!known_broker_path("/v1/community/relay/direct"));
        let debug = format!(
            "{:?}",
            RelayHttpBroker::new("https://origin.example", "secret".into()).unwrap()
        );
        assert!(!debug.contains("origin.example"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("192.0.2.1"));
    }

    #[test]
    fn app_visible_status_cannot_carry_a_peer_address() {
        let status = CommunityRelayStatus {
            phase: CommunityRelayPhase::Recovering,
            last_failure_code: Some(RelayError::TransportUnavailable.public_code()),
            ..Default::default()
        };
        let visible = format!("{status:?} {}", status.summary());
        assert!(!visible.contains("192.0.2.1"));
        assert!(!visible.contains("peer"));
        assert!(!visible.contains("turn:"));
    }

    #[test]
    fn runtime_shutdown_is_bounded_without_configuration() {
        let mut runtime = CommunityRelayRuntime::start(
            &settings::CommunityCacheSettings::default(),
            std::env::temp_dir().join("bowecho-relay-shutdown-test"),
            false,
        );
        assert!(runtime.shutdown_and_wait(Duration::from_secs(2)));
        assert_eq!(runtime.status().phase, CommunityRelayPhase::Off);
    }

    #[test]
    fn unavailable_relay_state_still_attempts_exact_archival_https() {
        let temp = tempfile::tempdir().unwrap();
        let settings = settings::CommunityCacheSettings {
            enabled: true,
            origin_url: "https://127.0.0.1:9".into(),
            manifest_public_key_base64: ORIGIN_PUBLIC_KEY.into(),
            // Relay deliberately remains disabled. The exact signed request
            // must still retain its conventional HTTPS fallback.
            historical_relay_enabled: false,
            ..Default::default()
        };
        let mut runtime = CommunityRelayRuntime::start(&settings, temp.path().to_path_buf(), false);

        let request = test_request();
        let result = runtime
            .dispatcher()
            .recover_exact_historical(request.clone(), test_manifest(&request))
            .unwrap()
            .wait_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(result, HistoricalRecoveryResult::Unavailable);
        let status = runtime.status();
        assert_eq!(status.archival_fallbacks, 1);
        assert_eq!(status.phase, CommunityRelayPhase::Off);
        assert!(!status.retrieval_enabled);
        assert_eq!(status.advertised_objects, 0);
        assert!(runtime.shutdown_and_wait(Duration::from_secs(2)));
    }

    #[test]
    fn blocked_seed_session_never_head_of_line_blocks_commands_or_shutdown() {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let status = Arc::new(Mutex::new(CommunityRelayStatus::default()));
        let cancellation = CancellationFlag::default();
        let worker_status = status.clone();
        let worker_cancellation = cancellation.clone();
        let worker =
            std::thread::spawn(move || run_worker(receiver, worker_status, worker_cancellation));

        let (started_tx, started_rx) = std_mpsc::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
        commands
            .blocking_send(RuntimeCommand::InstallBlockingSeedProbe {
                started: started_tx,
                release: release_rx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (probe_tx, probe_rx) = std_mpsc::channel();
        commands
            .blocking_send(RuntimeCommand::ResponsivenessProbe(probe_tx))
            .unwrap();
        probe_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("a pending seed session must not block the command dispatcher");

        cancellation.cancel();
        let _ = commands.blocking_send(RuntimeCommand::Shutdown);
        worker.join().unwrap();
        assert_eq!(lock_unpoisoned(&status).phase, CommunityRelayPhase::Off);
    }

    #[test]
    fn reconfigure_immediately_cancels_an_inflight_recovery_generation() {
        let settings = settings::CommunityCacheSettings::default();
        let mut runtime = CommunityRelayRuntime::start(
            &settings,
            std::env::temp_dir().join("bowecho-relay-reconfigure-cancel-test"),
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while runtime.status().phase != CommunityRelayPhase::Off && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let configuration_cancellation =
            lock_unpoisoned(&runtime.configuration_cancellation).clone();
        let (started_tx, started_rx) = std_mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std_mpsc::channel();
        runtime
            .commands
            .blocking_send(RuntimeCommand::InstallBlockingRecoveryProbe {
                configuration_cancellation,
                started: started_tx,
                cancelled: cancelled_tx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        runtime.reconfigure(
            &settings,
            std::env::temp_dir().join("bowecho-relay-reconfigure-cancel-test"),
            false,
        );
        cancelled_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("Stop/reconfigure must cancel recovery before its network timeout");
        assert!(runtime.shutdown_and_wait(Duration::from_secs(2)));
    }

    #[test]
    fn command_queue_and_result_types_have_no_operational_dispatch_variant() {
        let variants = [
            HistoricalRecoveryResult::AlreadyLocal,
            HistoricalRecoveryResult::RecoveredFromR2,
            HistoricalRecoveryResult::RecoveredFromCommunity,
            HistoricalRecoveryResult::RecoveredFromArchivalHttps,
            HistoricalRecoveryResult::Unavailable,
            HistoricalRecoveryResult::Cancelled,
        ];
        assert_eq!(variants.len(), 6);
        assert_eq!(
            rw_community_relay::after_operational_r2_miss(),
            rw_community_relay::OperationalFallback::HetznerHttpsOrigin
        );
    }

    #[test]
    fn every_relay_failure_branch_ends_in_archival_or_honest_unavailable() {
        assert_eq!(
            fallback_result(true),
            HistoricalRecoveryResult::RecoveredFromArchivalHttps
        );
        assert_eq!(
            fallback_result(false),
            HistoricalRecoveryResult::Unavailable
        );
    }
}
