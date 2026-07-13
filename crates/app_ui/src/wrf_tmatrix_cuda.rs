//! Job-scoped, batched CUDA execution service for admitted dry T-matrix nodes.
//!
//! CUDA contexts, uploaded LUTs, streams, and kernel handles stay on exactly
//! one worker thread. Rayon callers submit CPU-admitted nodes through this
//! synchronous facade; they never receive a raw CUDA object. A failed GPU
//! batch is never partially published: the service records one immutable
//! fallback reason, fails every pending request, and makes all later calls
//! return that same reason immediately so the caller can replay on the CPU.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use radar_scattering::AdditiveScattering;
use simradar_cuda::{CudaAvailability, CudaPreparedTMatrixNode, probe_cuda_cached};
#[cfg(any(windows, target_os = "linux"))]
use simradar_cuda::{CudaPreloadedTMatrixTable, CudaTMatrixExecutor, CudaTMatrixSegment};
use thiserror::Error;

#[cfg(any(windows, target_os = "linux"))]
use crate::wrf_tmatrix_assets::{
    PropertyTMatrixTableSourceKind, load_property_tmatrix_tables_exact,
};
#[cfg(any(windows, target_os = "linux"))]
use crate::wrf_tmatrix_band_assets::S_BAND_RESEARCH_FREQUENCY_HZ;

/// Default upper bound for a single kernel submission. At the current fixed
/// descriptor width this keeps staging allocations modest while amortizing
/// driver overhead across many Rayon requests.
pub const DEFAULT_CUDA_TMATRIX_BATCH_NODES: usize = 16_384;

/// Requests of different dry-table roles cannot share one kernel submission.
/// This additional bound prevents a burst of tiny Rayon requests from making
/// one batch's reply fan-out unbounded.
pub const DEFAULT_CUDA_TMATRIX_BATCH_REQUESTS: usize = 256;

/// Give sibling Rayon tasks a bounded opportunity to reach the dedicated GPU
/// worker before its first launch. The worker stops waiting as soon as this
/// many same-table nodes are queued, so a full millisecond is paid only by a
/// sparse burst instead of every launch.
const CUDA_TMATRIX_COALESCE_TARGET_NODES: usize = 12_288;

/// Hard latency ceiling for coalescing work that wakes an empty worker queue.
const CUDA_TMATRIX_COALESCE_WAIT: Duration = Duration::from_millis(1);

/// Exact preloaded dry LUT selected for one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfTMatrixCudaTableRole {
    DryOblate,
    DryProlate,
}

impl std::fmt::Display for WrfTMatrixCudaTableRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryOblate => "dry oblate",
            Self::DryProlate => "dry prolate",
        })
    }
}

/// Hard batching limits for one job-scoped service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixCudaBatchConfig {
    max_nodes: usize,
    max_requests: usize,
}

impl WrfTMatrixCudaBatchConfig {
    pub fn new(
        max_nodes: usize,
        max_requests: usize,
    ) -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        if max_nodes == 0 {
            return Err(WrfTMatrixCudaServiceOpenError::InvalidBatchLimit { field: "max nodes" });
        }
        if max_requests == 0 {
            return Err(WrfTMatrixCudaServiceOpenError::InvalidBatchLimit {
                field: "max requests",
            });
        }
        Ok(Self {
            max_nodes,
            max_requests,
        })
    }

    #[must_use]
    pub const fn max_nodes(self) -> usize {
        self.max_nodes
    }

    #[must_use]
    pub const fn max_requests(self) -> usize {
        self.max_requests
    }
}

impl Default for WrfTMatrixCudaBatchConfig {
    fn default() -> Self {
        Self {
            max_nodes: DEFAULT_CUDA_TMATRIX_BATCH_NODES,
            max_requests: DEFAULT_CUDA_TMATRIX_BATCH_REQUESTS,
        }
    }
}

/// Immutable device/artifact identity captured before the worker starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfTMatrixCudaDeviceReport {
    pub ordinal: usize,
    pub name: String,
    pub compute_capability_major: i32,
    pub compute_capability_minor: i32,
    pub total_memory_bytes: usize,
    pub driver_api_version: Option<i32>,
    pub kernel_artifact: String,
}

/// Why this job stopped submitting work to CUDA.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfTMatrixCudaFallbackReason {
    /// `Some` only when a concrete GPU batch was discarded.
    pub failed_batch_sequence: Option<u64>,
    pub discarded_node_count: usize,
    pub detail: String,
}

impl std::fmt::Display for WrfTMatrixCudaFallbackReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.failed_batch_sequence {
            Some(sequence) => write!(
                formatter,
                "batch {sequence} ({} nodes): {}",
                self.discarded_node_count, self.detail
            ),
            None => formatter.write_str(&self.detail),
        }
    }
}

/// Point-in-time operational report. Counters include failed submissions, so
/// `completed_*` can be smaller than `submitted_*` after fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrfTMatrixCudaServiceReport {
    pub device: WrfTMatrixCudaDeviceReport,
    pub max_nodes_per_batch: usize,
    pub max_requests_per_batch: usize,
    pub requests_submitted: u64,
    pub batches_submitted: u64,
    pub batches_completed: u64,
    pub nodes_submitted: u64,
    pub nodes_completed: u64,
    pub fallback_reason: Option<WrfTMatrixCudaFallbackReason>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WrfTMatrixCudaRequestError {
    #[error("CUDA T-matrix service disabled; replay this request on CPU: {reason}")]
    JobDisabled {
        reason: WrfTMatrixCudaFallbackReason,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WrfTMatrixCudaServiceOpenError {
    #[error("invalid CUDA T-matrix batch configuration: {field} must be positive")]
    InvalidBatchLimit { field: &'static str },
    #[error("NVIDIA CUDA T-matrix execution is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("CUDA acceleration unavailable: {0}")]
    Unavailable(String),
    #[error("load or validate embedded T-matrix tables before CUDA startup: {0}")]
    Tables(String),
    #[error("initialize CUDA T-matrix executor and preload dry tables: {0}")]
    Executor(String),
    #[error("CUDA T-matrix initialization panicked; CPU fallback remains available")]
    InitializationPanicked,
    #[error("spawn CUDA T-matrix worker thread: {0}")]
    WorkerSpawn(String),
}

/// Synchronous, thread-safe facade over one dedicated CUDA worker.
///
/// Share `&WrfTMatrixCudaBatchService` between Rayon tasks (or put the service
/// in an `Arc`). Dropping the last owner sends an explicit shutdown command and
/// joins the worker; the worker owns the executor for its entire lifetime.
pub struct WrfTMatrixCudaBatchService {
    commands: Sender<WorkerCommand<CudaPreparedTMatrixNode, AdditiveScattering>>,
    worker: Option<JoinHandle<()>>,
    state: Arc<ServiceState>,
    device: WrfTMatrixCudaDeviceReport,
    config: WrfTMatrixCudaBatchConfig,
}

impl std::fmt::Debug for WrfTMatrixCudaBatchService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WrfTMatrixCudaBatchService")
            .field("device", &self.device)
            .field("config", &self.config)
            .field("fallback", &self.state.fallback())
            .finish_non_exhaustive()
    }
}

impl WrfTMatrixCudaBatchService {
    /// Open the best supported device reported by the cached runtime probe.
    pub fn open_preferred() -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        let ordinal = preferred_device_ordinal(probe_cuda_cached())?;
        Self::open(ordinal)
    }

    pub fn open(ordinal: usize) -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        Self::open_with_config(ordinal, WrfTMatrixCudaBatchConfig::default())
    }

    pub fn open_with_config(
        ordinal: usize,
        config: WrfTMatrixCudaBatchConfig,
    ) -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        // Keep driver/library panics at the optional-backend boundary. CUDA is
        // an optimization; failure to construct it is a normal CPU fallback.
        let initialized = catch_unwind(AssertUnwindSafe(|| ProductionBackend::open(ordinal)))
            .map_err(|_| WrfTMatrixCudaServiceOpenError::InitializationPanicked)??;
        let device = initialized.report.clone();
        let state = Arc::new(ServiceState::default());
        let (commands, receiver) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("bowecho-cuda-tmatrix".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_worker(receiver, initialized, worker_state.clone(), config)
                }));
                if result.is_err() {
                    worker_state.disable(WrfTMatrixCudaFallbackReason {
                        failed_batch_sequence: None,
                        discarded_node_count: 0,
                        detail: "dedicated CUDA worker panicked".to_owned(),
                    });
                }
            })
            .map_err(|error| WrfTMatrixCudaServiceOpenError::WorkerSpawn(error.to_string()))?;
        Ok(Self {
            commands,
            worker: Some(worker),
            state,
            device,
            config,
        })
    }

    /// Evaluate every admitted node independently on the GPU. One-node
    /// segments deliberately preserve particle order and leave all category
    /// scaling/reduction to the existing exact CPU path.
    pub fn evaluate_particles(
        &self,
        role: WrfTMatrixCudaTableRole,
        nodes: Vec<CudaPreparedTMatrixNode>,
    ) -> Result<Vec<AdditiveScattering>, WrfTMatrixCudaRequestError> {
        if nodes.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(reason) = self.state.fallback() {
            return Err(WrfTMatrixCudaRequestError::JobDisabled { reason });
        }

        let (reply, response) = mpsc::sync_channel(1);
        saturating_increment(&self.state.requests_submitted, 1);
        if self
            .commands
            .send(WorkerCommand::Request(WorkerRequest { role, nodes, reply }))
            .is_err()
        {
            let reason = self.state.disable(WrfTMatrixCudaFallbackReason {
                failed_batch_sequence: None,
                discarded_node_count: 0,
                detail: "dedicated CUDA worker stopped before accepting the request".to_owned(),
            });
            return Err(WrfTMatrixCudaRequestError::JobDisabled { reason });
        }
        response.recv().unwrap_or_else(|_| {
            let reason = self.state.disable(WrfTMatrixCudaFallbackReason {
                failed_batch_sequence: None,
                discarded_node_count: 0,
                detail: "dedicated CUDA worker stopped without replying".to_owned(),
            });
            Err(WrfTMatrixCudaRequestError::JobDisabled { reason })
        })
    }

    /// Permanently stop CUDA submissions for this job before a GPU request is
    /// made. This is used when CPU-side admission/preparation cannot produce a
    /// trustworthy CUDA descriptor. The first reason wins, matching execution
    /// failure behavior; an in-flight GPU result is discarded before publish.
    pub fn disable_for_cpu_fallback(
        &self,
        detail: impl Into<String>,
        discarded_node_count: usize,
    ) -> WrfTMatrixCudaFallbackReason {
        self.state.disable(WrfTMatrixCudaFallbackReason {
            failed_batch_sequence: None,
            discarded_node_count,
            detail: detail.into(),
        })
    }

    /// Whether this job-scoped service has permanently selected CPU replay.
    /// Callers can use this before descriptor staging to avoid rebuilding GPU
    /// work after the immutable first fallback reason has already been set.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.state.fallback.get().is_some()
    }

    #[must_use]
    pub fn report(&self) -> WrfTMatrixCudaServiceReport {
        WrfTMatrixCudaServiceReport {
            device: self.device.clone(),
            max_nodes_per_batch: self.config.max_nodes,
            max_requests_per_batch: self.config.max_requests,
            requests_submitted: self.state.requests_submitted.load(Ordering::Relaxed),
            batches_submitted: self.state.batches_submitted.load(Ordering::Relaxed),
            batches_completed: self.state.batches_completed.load(Ordering::Relaxed),
            nodes_submitted: self.state.nodes_submitted.load(Ordering::Relaxed),
            nodes_completed: self.state.nodes_completed.load(Ordering::Relaxed),
            fallback_reason: self.state.fallback(),
        }
    }

    /// Deterministic worker failure used to prove whole-category CPU replay.
    /// This constructor is absent from production builds and never opens a
    /// CUDA driver, context, module, or table upload.
    #[cfg(test)]
    pub(crate) fn failing_for_test(detail: impl Into<String>) -> Self {
        Self::with_test_backend(
            "bowecho-cuda-tmatrix-failure-test",
            FailingPreparedNodeBackend {
                detail: detail.into(),
            },
        )
    }

    /// Return placeholder outputs for `successful_calls`, then fail the next
    /// worker batch. This proves that a caller discards an earlier successful
    /// role sweep before whole-chunk CPU replay.
    #[cfg(test)]
    pub(crate) fn fail_after_successful_calls_for_test(
        successful_calls: usize,
        detail: impl Into<String>,
    ) -> Self {
        Self::with_test_backend(
            "bowecho-cuda-tmatrix-partial-success-test",
            FailAfterPreparedNodeBackend {
                successful_calls,
                calls: 0,
                detail: detail.into(),
            },
        )
    }

    #[cfg(test)]
    fn with_test_backend(
        worker_name: &'static str,
        backend: impl BatchBackend<CudaPreparedTMatrixNode, AdditiveScattering>,
    ) -> Self {
        let config = WrfTMatrixCudaBatchConfig::default();
        let device = WrfTMatrixCudaDeviceReport {
            ordinal: usize::MAX,
            name: "test-only CUDA backend".to_owned(),
            compute_capability_major: 0,
            compute_capability_minor: 0,
            total_memory_bytes: 0,
            driver_api_version: None,
            kernel_artifact: "test-only/no-kernel".to_owned(),
        };
        let state = Arc::new(ServiceState::default());
        let (commands, receiver) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name(worker_name.to_owned())
            .spawn(move || run_worker(receiver, backend, worker_state, config))
            .expect("spawn deterministic CUDA test worker");
        Self {
            commands,
            worker: Some(worker),
            state,
            device,
            config,
        }
    }
}

fn preferred_device_ordinal(
    availability: &CudaAvailability,
) -> Result<usize, WrfTMatrixCudaServiceOpenError> {
    if matches!(availability, CudaAvailability::UnsupportedPlatform) {
        return Err(WrfTMatrixCudaServiceOpenError::UnsupportedPlatform);
    }
    availability
        .preferred_device()
        .map(|device| device.ordinal)
        .ok_or_else(|| {
            WrfTMatrixCudaServiceOpenError::Unavailable(
                availability
                    .fallback_reason()
                    .unwrap_or_else(|| "no supported NVIDIA CUDA device".to_owned()),
            )
        })
}

impl Drop for WrfTMatrixCudaBatchService {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
struct ProductionBackend {
    executor: CudaTMatrixExecutor,
    dry_oblate: CudaPreloadedTMatrixTable,
    dry_prolate: CudaPreloadedTMatrixTable,
    independent_segments: Vec<CudaTMatrixSegment>,
    report: WrfTMatrixCudaDeviceReport,
}

#[cfg(any(windows, target_os = "linux"))]
impl ProductionBackend {
    /// Table parsing, the complete five-role validation gate, and both large
    /// host-to-device LUT uploads all finish synchronously here. Only then may
    /// the executor move onto its permanent worker thread.
    fn open(ordinal: usize) -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .map_err(WrfTMatrixCudaServiceOpenError::Tables)?;
        let tables = owner.borrowed_bundle();
        tables
            .validate()
            .map_err(|error| WrfTMatrixCudaServiceOpenError::Tables(error.to_string()))?;

        let mut executor = CudaTMatrixExecutor::open(ordinal)
            .map_err(|error| WrfTMatrixCudaServiceOpenError::Executor(error.to_string()))?;
        let dry_oblate = executor
            .preload_table(tables.dry_oblate)
            .map_err(|error| WrfTMatrixCudaServiceOpenError::Executor(error.to_string()))?;
        let dry_prolate = executor
            .preload_table(tables.dry_prolate)
            .map_err(|error| WrfTMatrixCudaServiceOpenError::Executor(error.to_string()))?;
        let device = executor.device();
        let report = WrfTMatrixCudaDeviceReport {
            ordinal: device.ordinal,
            name: device.name.clone(),
            compute_capability_major: device.compute_capability_major,
            compute_capability_minor: device.compute_capability_minor,
            total_memory_bytes: device.total_memory_bytes,
            driver_api_version: driver_api_version_for_ordinal(ordinal),
            kernel_artifact: executor.kernel_artifact().to_owned(),
        };
        Ok(Self {
            executor,
            dry_oblate,
            dry_prolate,
            independent_segments: Vec::new(),
            report,
        })
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl BatchBackend<CudaPreparedTMatrixNode, AdditiveScattering> for ProductionBackend {
    fn evaluate(
        &mut self,
        role: WrfTMatrixCudaTableRole,
        nodes: &[CudaPreparedTMatrixNode],
    ) -> Result<Vec<AdditiveScattering>, String> {
        let table = match role {
            WrfTMatrixCudaTableRole::DryOblate => self.dry_oblate,
            WrfTMatrixCudaTableRole::DryProlate => self.dry_prolate,
        };
        self.independent_segments.clear();
        self.independent_segments.reserve(nodes.len());
        for index in 0..nodes.len() {
            self.independent_segments
                .push(CudaTMatrixSegment::new(index, 1).map_err(|error| error.to_string())?);
        }
        self.executor
            .evaluate_preloaded_segments(table, nodes, &self.independent_segments)
            .map_err(|error| error.to_string())
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
struct ProductionBackend {
    // Kept in the cross-platform shape so the service constructor remains one
    // type-checked implementation. `open` always returns UnsupportedPlatform,
    // so no synthetic device identity is ever constructed or reported.
    report: WrfTMatrixCudaDeviceReport,
}

#[cfg(not(any(windows, target_os = "linux")))]
impl ProductionBackend {
    fn open(_ordinal: usize) -> Result<Self, WrfTMatrixCudaServiceOpenError> {
        Err(WrfTMatrixCudaServiceOpenError::UnsupportedPlatform)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
impl BatchBackend<CudaPreparedTMatrixNode, AdditiveScattering> for ProductionBackend {
    fn evaluate(
        &mut self,
        _role: WrfTMatrixCudaTableRole,
        _nodes: &[CudaPreparedTMatrixNode],
    ) -> Result<Vec<AdditiveScattering>, String> {
        Err("NVIDIA CUDA T-matrix execution is unsupported on this platform".to_owned())
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn driver_api_version_for_ordinal(ordinal: usize) -> Option<i32> {
    match probe_cuda_cached() {
        CudaAvailability::Available {
            driver_api_version,
            devices,
        } if devices.iter().any(|device| device.ordinal == ordinal) => Some(*driver_api_version),
        _ => None,
    }
}

trait BatchBackend<N, O>: Send + 'static {
    fn evaluate(&mut self, role: WrfTMatrixCudaTableRole, nodes: &[N]) -> Result<Vec<O>, String>;
}

#[cfg(test)]
struct FailingPreparedNodeBackend {
    detail: String,
}

#[cfg(test)]
impl BatchBackend<CudaPreparedTMatrixNode, AdditiveScattering> for FailingPreparedNodeBackend {
    fn evaluate(
        &mut self,
        _role: WrfTMatrixCudaTableRole,
        _nodes: &[CudaPreparedTMatrixNode],
    ) -> Result<Vec<AdditiveScattering>, String> {
        Err(self.detail.clone())
    }
}

#[cfg(test)]
struct FailAfterPreparedNodeBackend {
    successful_calls: usize,
    calls: usize,
    detail: String,
}

#[cfg(test)]
impl BatchBackend<CudaPreparedTMatrixNode, AdditiveScattering> for FailAfterPreparedNodeBackend {
    fn evaluate(
        &mut self,
        _role: WrfTMatrixCudaTableRole,
        nodes: &[CudaPreparedTMatrixNode],
    ) -> Result<Vec<AdditiveScattering>, String> {
        let call = self.calls;
        self.calls = self.calls.saturating_add(1);
        if call >= self.successful_calls {
            Err(self.detail.clone())
        } else {
            Ok((0..nodes.len())
                .map(|_| AdditiveScattering::default())
                .collect())
        }
    }
}

enum WorkerCommand<N, O> {
    Request(WorkerRequest<N, O>),
    Shutdown,
}

struct WorkerRequest<N, O> {
    role: WrfTMatrixCudaTableRole,
    nodes: Vec<N>,
    reply: SyncSender<Result<Vec<O>, WrfTMatrixCudaRequestError>>,
}

struct PendingRequest<N, O> {
    role: WrfTMatrixCudaTableRole,
    nodes: Vec<N>,
    next_node: usize,
    outputs: Vec<O>,
    reply: SyncSender<Result<Vec<O>, WrfTMatrixCudaRequestError>>,
}

impl<N, O> From<WorkerRequest<N, O>> for PendingRequest<N, O> {
    fn from(request: WorkerRequest<N, O>) -> Self {
        let capacity = request.nodes.len();
        Self {
            role: request.role,
            nodes: request.nodes,
            next_node: 0,
            outputs: Vec::with_capacity(capacity),
            reply: request.reply,
        }
    }
}

#[derive(Default)]
struct ServiceState {
    fallback: OnceLock<WrfTMatrixCudaFallbackReason>,
    requests_submitted: AtomicU64,
    batches_submitted: AtomicU64,
    batches_completed: AtomicU64,
    nodes_submitted: AtomicU64,
    nodes_completed: AtomicU64,
}

impl ServiceState {
    fn fallback(&self) -> Option<WrfTMatrixCudaFallbackReason> {
        self.fallback.get().cloned()
    }

    fn disable(&self, reason: WrfTMatrixCudaFallbackReason) -> WrfTMatrixCudaFallbackReason {
        self.fallback.get_or_init(|| reason).clone()
    }
}

fn run_worker<N, O, B>(
    receiver: Receiver<WorkerCommand<N, O>>,
    mut backend: B,
    state: Arc<ServiceState>,
    config: WrfTMatrixCudaBatchConfig,
) where
    N: Clone + Send + 'static,
    O: Send + 'static,
    B: BatchBackend<N, O>,
{
    let mut pending = VecDeque::new();
    let mut shutdown = false;
    let mut batch_sequence = 0_u64;

    loop {
        if let Some(reason) = state.fallback() {
            fail_pending(&mut pending, &reason);
            match receiver.recv() {
                Ok(WorkerCommand::Request(request)) => {
                    let _ = request
                        .reply
                        .send(Err(WrfTMatrixCudaRequestError::JobDisabled {
                            reason: reason.clone(),
                        }));
                }
                Ok(WorkerCommand::Shutdown) | Err(_) => break,
            }
            continue;
        }

        let mut woke_from_empty = false;
        if pending.is_empty() && !shutdown {
            match receiver.recv() {
                Ok(WorkerCommand::Request(request)) => {
                    pending.push_back(request.into());
                    woke_from_empty = true;
                }
                Ok(WorkerCommand::Shutdown) => shutdown = true,
                Err(_) => break,
            }
        }

        if !shutdown {
            collect_peer_requests(
                &receiver,
                &mut pending,
                config,
                &mut shutdown,
                &state,
                CUDA_TMATRIX_COALESCE_TARGET_NODES,
                if woke_from_empty {
                    CUDA_TMATRIX_COALESCE_WAIT
                } else {
                    Duration::ZERO
                },
            );
        }

        if pending.is_empty() {
            if shutdown {
                break;
            }
            continue;
        }

        // An admission failure can disable the service from a Rayon caller
        // while this worker is receiving/coalescing requests. Do not launch a
        // new batch after that first immutable fallback reason is installed.
        if let Some(reason) = state.fallback() {
            fail_pending(&mut pending, &reason);
            continue;
        }

        batch_sequence = batch_sequence.saturating_add(1);
        if let Err(failure) =
            process_one_batch(&mut pending, &mut backend, config, batch_sequence, &state)
        {
            let reason = state.disable(failure);
            fail_pending(&mut pending, &reason);
        }
    }
}

fn collect_peer_requests<N, O>(
    receiver: &Receiver<WorkerCommand<N, O>>,
    pending: &mut VecDeque<PendingRequest<N, O>>,
    config: WrfTMatrixCudaBatchConfig,
    shutdown: &mut bool,
    state: &ServiceState,
    target_nodes: usize,
    coalesce_wait: Duration,
) {
    let Some(role) = pending.front().map(|request| request.role) else {
        return;
    };
    let deadline = Instant::now() + coalesce_wait;
    loop {
        let queued_nodes = pending
            .iter()
            .take_while(|request| request.role == role)
            .map(|request| request.nodes.len().saturating_sub(request.next_node))
            .fold(0_usize, usize::saturating_add);
        let role_boundary_queued = pending.iter().any(|request| request.role != role);
        if pending.len() >= config.max_requests
            || queued_nodes >= config.max_nodes
            || queued_nodes >= target_nodes
            || role_boundary_queued
            || state.fallback().is_some()
        {
            break;
        }

        let command = match receiver.try_recv() {
            Ok(command) => Ok(command),
            Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
            Err(TryRecvError::Empty) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                receiver.recv_timeout(remaining)
            }
        };
        match command {
            Ok(WorkerCommand::Request(request)) => {
                let role_changed = request.role != role;
                pending.push_back(request.into());
                if role_changed {
                    break;
                }
            }
            Ok(WorkerCommand::Shutdown) => {
                *shutdown = true;
                break;
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                *shutdown = true;
                break;
            }
        }
    }
}

fn process_one_batch<N, O, B>(
    pending: &mut VecDeque<PendingRequest<N, O>>,
    backend: &mut B,
    config: WrfTMatrixCudaBatchConfig,
    batch_sequence: u64,
    state: &ServiceState,
) -> Result<(), WrfTMatrixCudaFallbackReason>
where
    N: Clone,
    B: BatchBackend<N, O>,
{
    let role = pending
        .front()
        .expect("process_one_batch requires pending work")
        .role;
    let mut batch_nodes = Vec::with_capacity(config.max_nodes);
    let mut spans = Vec::with_capacity(config.max_requests);

    for (request_index, request) in pending.iter_mut().enumerate() {
        if request.role != role || spans.len() == config.max_requests {
            continue;
        }
        let remaining = request.nodes.len().saturating_sub(request.next_node);
        let available = config.max_nodes.saturating_sub(batch_nodes.len());
        let take = remaining.min(available);
        if take == 0 {
            continue;
        }
        let start = request.next_node;
        let end = start + take;
        batch_nodes.extend_from_slice(&request.nodes[start..end]);
        request.next_node = end;
        spans.push((request_index, take));
        if batch_nodes.len() == config.max_nodes {
            break;
        }
    }

    debug_assert!(!batch_nodes.is_empty());
    saturating_increment(&state.batches_submitted, 1);
    saturating_increment(&state.nodes_submitted, usize_to_u64(batch_nodes.len()));
    let evaluated = catch_unwind(AssertUnwindSafe(|| backend.evaluate(role, &batch_nodes)))
        .map_err(|_| WrfTMatrixCudaFallbackReason {
            failed_batch_sequence: Some(batch_sequence),
            discarded_node_count: batch_nodes.len(),
            detail: "CUDA backend panicked while executing the batch".to_owned(),
        })?
        .map_err(|detail| WrfTMatrixCudaFallbackReason {
            failed_batch_sequence: Some(batch_sequence),
            discarded_node_count: batch_nodes.len(),
            detail,
        })?;
    if evaluated.len() != batch_nodes.len() {
        return Err(WrfTMatrixCudaFallbackReason {
            failed_batch_sequence: Some(batch_sequence),
            discarded_node_count: batch_nodes.len(),
            detail: format!(
                "CUDA backend returned {} particle outputs for {} one-node segments",
                evaluated.len(),
                batch_nodes.len()
            ),
        });
    }
    // A different Rayon task may have hit a CPU-side preparation failure while
    // this kernel was running. Publishing this successful result would split
    // one job across pre/post-disable eras, so discard it with the stored
    // reason before touching any request's output buffer.
    if let Some(reason) = state.fallback() {
        return Err(reason);
    }

    let mut outputs = evaluated.into_iter();
    for (request_index, count) in spans {
        let request = pending
            .get_mut(request_index)
            .expect("batch span points to a stable pending request");
        request.outputs.extend(outputs.by_ref().take(count));
    }
    debug_assert!(outputs.next().is_none());
    saturating_increment(&state.batches_completed, 1);
    saturating_increment(&state.nodes_completed, usize_to_u64(batch_nodes.len()));
    reply_to_completed(pending);
    Ok(())
}

fn reply_to_completed<N, O>(pending: &mut VecDeque<PendingRequest<N, O>>) {
    let mut index = 0;
    while index < pending.len() {
        let complete = {
            let request = &pending[index];
            request.next_node == request.nodes.len() && request.outputs.len() == request.nodes.len()
        };
        if complete {
            let request = pending
                .remove(index)
                .expect("completed pending request index remains valid");
            let _ = request.reply.send(Ok(request.outputs));
        } else {
            index += 1;
        }
    }
}

fn fail_pending<N, O>(
    pending: &mut VecDeque<PendingRequest<N, O>>,
    reason: &WrfTMatrixCudaFallbackReason,
) {
    while let Some(request) = pending.pop_front() {
        let _ = request
            .reply
            .send(Err(WrfTMatrixCudaRequestError::JobDisabled {
                reason: reason.clone(),
            }));
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn saturating_increment(counter: &AtomicU64, increment: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[cfg(any(windows, target_os = "linux"))]
    use radar_scattering::{
        OrientationModel, PsdSpheroidHabit, RadarViewGeometry, SpheroidConvention,
        TMatrixEvaluationRequest,
    };
    use rayon::prelude::*;

    use super::*;

    type MockCall = (WrfTMatrixCudaTableRole, Vec<u32>);
    type MockCallLog = Arc<Mutex<Vec<MockCall>>>;
    type MockPendingReply = Receiver<Result<Vec<u32>, WrfTMatrixCudaRequestError>>;
    type MockPendingRequest = (PendingRequest<u32, u32>, MockPendingReply);

    #[derive(Clone)]
    struct MockControl {
        calls: MockCallLog,
        fail_call: Option<usize>,
    }

    struct MockBackend {
        control: MockControl,
    }

    impl BatchBackend<u32, u32> for MockBackend {
        fn evaluate(
            &mut self,
            role: WrfTMatrixCudaTableRole,
            nodes: &[u32],
        ) -> Result<Vec<u32>, String> {
            let mut calls = self.control.calls.lock().unwrap();
            calls.push((role, nodes.to_vec()));
            let call = calls.len();
            if self.control.fail_call == Some(call) {
                return Err("mock launch failed".to_owned());
            }
            Ok(nodes.iter().map(|node| node * 2).collect())
        }
    }

    struct DisableDuringEvaluationBackend {
        state: Arc<ServiceState>,
    }

    struct DropAwareBackend {
        dropped: Arc<AtomicBool>,
    }

    impl Drop for DropAwareBackend {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl BatchBackend<u32, u32> for DropAwareBackend {
        fn evaluate(
            &mut self,
            _role: WrfTMatrixCudaTableRole,
            nodes: &[u32],
        ) -> Result<Vec<u32>, String> {
            Ok(nodes.to_vec())
        }
    }

    impl BatchBackend<u32, u32> for DisableDuringEvaluationBackend {
        fn evaluate(
            &mut self,
            _role: WrfTMatrixCudaTableRole,
            nodes: &[u32],
        ) -> Result<Vec<u32>, String> {
            self.state.disable(WrfTMatrixCudaFallbackReason {
                failed_batch_sequence: None,
                discarded_node_count: 1,
                detail: "parallel CPU node preparation failed".to_owned(),
            });
            Ok(nodes.to_vec())
        }
    }

    fn mock() -> (MockBackend, MockControl) {
        let control = MockControl {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_call: None,
        };
        (
            MockBackend {
                control: control.clone(),
            },
            control,
        )
    }

    fn pending(role: WrfTMatrixCudaTableRole, nodes: Vec<u32>) -> MockPendingRequest {
        let (reply, response) = mpsc::sync_channel(1);
        (
            PendingRequest::from(WorkerRequest { role, nodes, reply }),
            response,
        )
    }

    #[test]
    fn coalesces_same_table_requests_and_preserves_each_input_order() {
        let (mut backend, control) = mock();
        let config = WrfTMatrixCudaBatchConfig::new(8, 8).unwrap();
        let state = ServiceState::default();
        let (first, first_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![1, 2]);
        let (second, second_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![9, 4, 7]);
        let (other_role, other_reply) = pending(WrfTMatrixCudaTableRole::DryProlate, vec![100]);
        let mut queue = VecDeque::from([first, second, other_role]);

        process_one_batch(&mut queue, &mut backend, config, 1, &state).unwrap();

        assert_eq!(first_reply.recv().unwrap().unwrap(), vec![2, 4]);
        assert_eq!(second_reply.recv().unwrap().unwrap(), vec![18, 8, 14]);
        assert!(other_reply.try_recv().is_err());
        assert_eq!(queue.len(), 1);
        assert_eq!(
            *control.calls.lock().unwrap(),
            vec![(WrfTMatrixCudaTableRole::DryOblate, vec![1, 2, 9, 4, 7])]
        );
    }

    #[test]
    fn empty_queue_coalescing_stops_at_target_nodes() {
        let config = WrfTMatrixCudaBatchConfig::new(8, 8).unwrap();
        let state = ServiceState::default();
        let (first, _first_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![1]);
        let mut queue = VecDeque::from([first]);
        let (commands, receiver) = mpsc::channel();
        for nodes in [vec![2, 3, 4], vec![5]] {
            let (reply, _response) = mpsc::sync_channel(1);
            commands
                .send(WorkerCommand::Request(WorkerRequest {
                    role: WrfTMatrixCudaTableRole::DryOblate,
                    nodes,
                    reply,
                }))
                .unwrap();
        }
        let mut shutdown = false;
        collect_peer_requests(
            &receiver,
            &mut queue,
            config,
            &mut shutdown,
            &state,
            4,
            Duration::from_millis(100),
        );

        assert!(!shutdown);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].nodes, vec![1]);
        assert_eq!(queue[1].nodes, vec![2, 3, 4]);
        let WorkerCommand::Request(waiting) = receiver.try_recv().unwrap() else {
            panic!("target must stop before receiving shutdown");
        };
        assert_eq!(waiting.nodes, vec![5]);
    }

    #[test]
    fn empty_queue_coalescing_stops_at_full_batch() {
        let config = WrfTMatrixCudaBatchConfig::new(4, 8).unwrap();
        let state = ServiceState::default();
        let (first, _first_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![1, 2]);
        let mut queue = VecDeque::from([first]);
        let (commands, receiver) = mpsc::channel();
        for nodes in [vec![3, 4], vec![5]] {
            let (reply, _response) = mpsc::sync_channel(1);
            commands
                .send(WorkerCommand::Request(WorkerRequest {
                    role: WrfTMatrixCudaTableRole::DryOblate,
                    nodes,
                    reply,
                }))
                .unwrap();
        }
        let mut shutdown = false;
        collect_peer_requests(
            &receiver,
            &mut queue,
            config,
            &mut shutdown,
            &state,
            8,
            Duration::from_millis(100),
        );

        assert!(!shutdown);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].nodes, vec![1, 2]);
        assert_eq!(queue[1].nodes, vec![3, 4]);
        let WorkerCommand::Request(waiting) = receiver.try_recv().unwrap() else {
            panic!("full batch must stop before receiving shutdown");
        };
        assert_eq!(waiting.nodes, vec![5]);
    }

    #[test]
    fn empty_queue_coalescing_stops_at_role_boundary() {
        let config = WrfTMatrixCudaBatchConfig::new(8, 8).unwrap();
        let state = ServiceState::default();
        let (first, _first_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![1]);
        let mut queue = VecDeque::from([first]);
        let (commands, receiver) = mpsc::channel();
        for (role, nodes) in [
            (WrfTMatrixCudaTableRole::DryOblate, vec![2, 3]),
            (WrfTMatrixCudaTableRole::DryProlate, vec![4]),
            (WrfTMatrixCudaTableRole::DryOblate, vec![5]),
        ] {
            let (reply, _response) = mpsc::sync_channel(1);
            commands
                .send(WorkerCommand::Request(WorkerRequest { role, nodes, reply }))
                .unwrap();
        }
        let mut shutdown = false;
        collect_peer_requests(
            &receiver,
            &mut queue,
            config,
            &mut shutdown,
            &state,
            8,
            Duration::from_millis(100),
        );

        assert!(!shutdown);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].nodes, vec![1]);
        assert_eq!(queue[1].nodes, vec![2, 3]);
        assert_eq!(queue[2].nodes, vec![4]);
        assert_eq!(queue[2].role, WrfTMatrixCudaTableRole::DryProlate);
        let WorkerCommand::Request(waiting) = receiver.try_recv().unwrap() else {
            panic!("role boundary must stop before receiving shutdown");
        };
        assert_eq!(waiting.nodes, vec![5]);
    }

    #[test]
    fn concurrent_burst_reaches_target_in_two_batches_and_preserves_replies() {
        const REQUESTS: usize = 24;
        const NODES_PER_REQUEST: usize = 1_024;

        let (backend, control) = mock();
        let state = Arc::new(ServiceState::default());
        let (commands, receiver) = mpsc::channel();
        let replies = (0..REQUESTS)
            .into_par_iter()
            .map(|request_index| {
                let commands = commands.clone();
                let start = request_index * NODES_PER_REQUEST;
                let nodes = (start..start + NODES_PER_REQUEST)
                    .map(|node| u32::try_from(node).unwrap())
                    .collect::<Vec<_>>();
                let (reply, response) = mpsc::sync_channel(1);
                commands
                    .send(WorkerCommand::Request(WorkerRequest {
                        role: WrfTMatrixCudaTableRole::DryOblate,
                        nodes,
                        reply,
                    }))
                    .unwrap();
                (request_index, response)
            })
            .collect::<Vec<_>>();
        commands.send(WorkerCommand::Shutdown).unwrap();

        run_worker(
            receiver,
            backend,
            Arc::clone(&state),
            WrfTMatrixCudaBatchConfig::default(),
        );

        for (request_index, response) in replies {
            let output = response.recv().unwrap().unwrap();
            let start = request_index * NODES_PER_REQUEST;
            assert_eq!(output.len(), NODES_PER_REQUEST);
            assert_eq!(output[0], u32::try_from(start * 2).unwrap());
            assert_eq!(
                output[NODES_PER_REQUEST - 1],
                u32::try_from((start + NODES_PER_REQUEST - 1) * 2).unwrap()
            );
        }
        let batch_sizes = control
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, nodes)| nodes.len())
            .collect::<Vec<_>>();
        assert_eq!(batch_sizes, vec![12_288, 12_288]);
        assert_eq!(state.batches_completed.load(Ordering::Relaxed), 2);
        assert_eq!(state.nodes_completed.load(Ordering::Relaxed), 24_576);
    }

    #[test]
    fn slices_oversized_request_but_replies_only_after_ordered_completion() {
        let (mut backend, control) = mock();
        let config = WrfTMatrixCudaBatchConfig::new(4, 8).unwrap();
        let state = ServiceState::default();
        let input = (0..10).collect::<Vec<_>>();
        let (request, reply) = pending(WrfTMatrixCudaTableRole::DryProlate, input.clone());
        let mut queue = VecDeque::from([request]);

        process_one_batch(&mut queue, &mut backend, config, 1, &state).unwrap();
        assert!(reply.try_recv().is_err());
        process_one_batch(&mut queue, &mut backend, config, 2, &state).unwrap();
        assert!(reply.try_recv().is_err());
        process_one_batch(&mut queue, &mut backend, config, 3, &state).unwrap();

        assert_eq!(
            reply.recv().unwrap().unwrap(),
            input.iter().map(|node| node * 2).collect::<Vec<_>>()
        );
        assert!(queue.is_empty());
        assert_eq!(
            control
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, nodes)| nodes.len())
                .collect::<Vec<_>>(),
            vec![4, 4, 2]
        );
        assert_eq!(state.batches_completed.load(Ordering::Relaxed), 3);
        assert_eq!(state.nodes_completed.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn failed_batch_is_discarded_once_and_all_pending_requests_share_reason() {
        let control = MockControl {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_call: Some(1),
        };
        let mut backend = MockBackend { control };
        let config = WrfTMatrixCudaBatchConfig::new(8, 8).unwrap();
        let state = ServiceState::default();
        let (first, first_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![1, 2]);
        let (second, second_reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![3]);
        let mut queue = VecDeque::from([first, second]);

        let failure = process_one_batch(&mut queue, &mut backend, config, 7, &state).unwrap_err();
        let stored = state.disable(failure);
        fail_pending(&mut queue, &stored);
        let first_error = first_reply.recv().unwrap().unwrap_err();
        let second_error = second_reply.recv().unwrap().unwrap_err();

        assert_eq!(first_error, second_error);
        assert_eq!(
            first_error,
            WrfTMatrixCudaRequestError::JobDisabled {
                reason: WrfTMatrixCudaFallbackReason {
                    failed_batch_sequence: Some(7),
                    discarded_node_count: 3,
                    detail: "mock launch failed".to_owned(),
                },
            }
        );
        assert_eq!(state.batches_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(state.batches_completed.load(Ordering::Relaxed), 0);
        assert_eq!(state.nodes_submitted.load(Ordering::Relaxed), 3);
        assert_eq!(state.nodes_completed.load(Ordering::Relaxed), 0);
        assert_eq!(state.fallback(), Some(stored));
    }

    #[test]
    fn disabled_worker_returns_stored_reason_without_calling_backend_again() {
        let (backend, control) = mock();
        let config = WrfTMatrixCudaBatchConfig::new(4, 4).unwrap();
        let state = Arc::new(ServiceState::default());
        let reason = state.disable(WrfTMatrixCudaFallbackReason {
            failed_batch_sequence: Some(2),
            discarded_node_count: 4,
            detail: "frozen failure".to_owned(),
        });
        let (commands, receiver) = mpsc::channel();
        let worker_state = Arc::clone(&state);
        let worker = thread::spawn(move || run_worker(receiver, backend, worker_state, config));
        let (reply, response) = mpsc::sync_channel(1);
        commands
            .send(WorkerCommand::Request(WorkerRequest {
                role: WrfTMatrixCudaTableRole::DryOblate,
                nodes: vec![1],
                reply,
            }))
            .unwrap();
        let error = response.recv().unwrap().unwrap_err();
        commands.send(WorkerCommand::Shutdown).unwrap();
        worker.join().unwrap();

        assert_eq!(error, WrfTMatrixCudaRequestError::JobDisabled { reason });
        assert!(control.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn external_disable_discards_an_in_flight_success_before_publish() {
        let state = Arc::new(ServiceState::default());
        let mut backend = DisableDuringEvaluationBackend {
            state: Arc::clone(&state),
        };
        let config = WrfTMatrixCudaBatchConfig::new(4, 4).unwrap();
        let (request, reply) = pending(WrfTMatrixCudaTableRole::DryOblate, vec![7, 8]);
        let mut queue = VecDeque::from([request]);

        let failure = process_one_batch(&mut queue, &mut backend, config, 1, &state).unwrap_err();

        assert_eq!(failure, state.fallback().unwrap());
        assert_eq!(queue[0].outputs, Vec::<u32>::new());
        assert!(reply.try_recv().is_err());
        fail_pending(&mut queue, &failure);
        assert_eq!(
            reply.recv().unwrap().unwrap_err(),
            WrfTMatrixCudaRequestError::JobDisabled { reason: failure }
        );
        assert_eq!(state.batches_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(state.batches_completed.load(Ordering::Relaxed), 0);
        assert_eq!(state.nodes_completed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shutdown_joins_worker_and_drops_thread_owned_backend() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WrfTMatrixCudaBatchService>();

        let dropped = Arc::new(AtomicBool::new(false));
        let backend = DropAwareBackend {
            dropped: Arc::clone(&dropped),
        };
        let config = WrfTMatrixCudaBatchConfig::new(4, 4).unwrap();
        let state = Arc::new(ServiceState::default());
        let (commands, receiver) = mpsc::channel();
        let worker = thread::spawn(move || run_worker(receiver, backend, state, config));

        commands.send(WorkerCommand::Shutdown).unwrap();
        worker.join().unwrap();

        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn preferred_device_resolution_distinguishes_unsupported_and_unavailable() {
        assert_eq!(
            preferred_device_ordinal(&CudaAvailability::UnsupportedPlatform),
            Err(WrfTMatrixCudaServiceOpenError::UnsupportedPlatform)
        );
        assert_eq!(
            preferred_device_ordinal(&CudaAvailability::DriverUnavailable),
            Err(WrfTMatrixCudaServiceOpenError::Unavailable(
                "NVIDIA CUDA driver not found".to_owned()
            ))
        );

        let old_only = CudaAvailability::Available {
            driver_api_version: 12_000,
            devices: vec![simradar_cuda::CudaDeviceInfo {
                ordinal: 4,
                name: "old test device".to_owned(),
                compute_capability_major: 7,
                compute_capability_minor: 4,
                total_memory_bytes: 64,
            }],
        };
        assert_eq!(
            preferred_device_ordinal(&old_only),
            Err(WrfTMatrixCudaServiceOpenError::Unavailable(
                "CUDA devices are older than compute capability 7.5".to_owned()
            ))
        );

        let supported = CudaAvailability::Available {
            driver_api_version: 13_000,
            devices: vec![
                simradar_cuda::CudaDeviceInfo {
                    ordinal: 2,
                    name: "small".to_owned(),
                    compute_capability_major: 8,
                    compute_capability_minor: 6,
                    total_memory_bytes: 16,
                },
                simradar_cuda::CudaDeviceInfo {
                    ordinal: 7,
                    name: "large".to_owned(),
                    compute_capability_major: 12,
                    compute_capability_minor: 0,
                    total_memory_bytes: 32,
                },
            ],
        };
        assert_eq!(preferred_device_ordinal(&supported), Ok(7));
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn actual_gpu_service_preloads_and_executes_one_admitted_node_when_available() {
        let availability = probe_cuda_cached();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping CUDA service integration test: {availability:?}");
            return;
        };
        let service = WrfTMatrixCudaBatchService::open_with_config(
            device.ordinal,
            WrfTMatrixCudaBatchConfig::new(32, 8).unwrap(),
        )
        .unwrap();
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .unwrap();
        let table = owner.borrowed_bundle().dry_oblate;
        let request = TMatrixEvaluationRequest::new(
            S_BAND_RESEARCH_FREQUENCY_HZ,
            SpheroidConvention::OblateMinorVertical,
            RadarViewGeometry::horizontal(),
        )
        .unwrap();
        let prepared = table
            .prepare_dry_particle_geometry_per_m3(
                260.0,
                1.0e-3,
                400.0,
                0.8,
                PsdSpheroidHabit::Oblate,
                None,
                None,
                table.dry_particle_node_fall_speed_provenance().unwrap(),
                OrientationModel::GaussianCanting {
                    mean_deg: 0.0,
                    standard_deviation_deg: 20.0,
                    quadrature_points: 50,
                },
                request,
            )
            .unwrap();
        let interpolation = table
            .prepare_dry_particle_lut_interpolation(&prepared)
            .unwrap();
        let cuda_node = CudaPreparedTMatrixNode::new(&interpolation, 1.0).unwrap();
        let gpu = service
            .evaluate_particles(WrfTMatrixCudaTableRole::DryOblate, vec![cuda_node])
            .unwrap();
        let cpu = table
            .evaluate_prepared_dry_particle_node_per_m3(&prepared)
            .unwrap();

        assert_eq!(gpu.len(), 1);
        assert_eq!(
            gpu[0].components().map(f64::to_bits),
            cpu.components().map(f64::to_bits)
        );
        let report = service.report();
        assert_eq!(report.device.ordinal, device.ordinal);
        assert_eq!(report.batches_submitted, 1);
        assert_eq!(report.batches_completed, 1);
        assert_eq!(report.nodes_completed, 1);
        assert_eq!(report.fallback_reason, None);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    #[ignore = "manual CUDA throughput microbenchmark; requires a supported NVIDIA GPU"]
    fn manual_cuda_throughput_matches_scalar_cpu_for_65k_nodes() {
        const NODE_COUNT: usize = 65_536;
        const CUDA_BATCHES: usize = 5;

        let availability = probe_cuda_cached();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping CUDA throughput benchmark: {availability:?}");
            return;
        };
        let service = WrfTMatrixCudaBatchService::open_with_config(
            device.ordinal,
            WrfTMatrixCudaBatchConfig::new(NODE_COUNT, DEFAULT_CUDA_TMATRIX_BATCH_REQUESTS)
                .unwrap(),
        )
        .unwrap();
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .unwrap();
        let table = owner.borrowed_bundle().dry_oblate;
        let request = TMatrixEvaluationRequest::new(
            S_BAND_RESEARCH_FREQUENCY_HZ,
            SpheroidConvention::OblateMinorVertical,
            RadarViewGeometry::horizontal(),
        )
        .unwrap();
        let prepared = table
            .prepare_dry_particle_geometry_per_m3(
                260.0,
                1.0e-3,
                400.0,
                0.8,
                PsdSpheroidHabit::Oblate,
                None,
                None,
                table.dry_particle_node_fall_speed_provenance().unwrap(),
                OrientationModel::GaussianCanting {
                    mean_deg: 0.0,
                    standard_deviation_deg: 20.0,
                    quadrature_points: 50,
                },
                request,
            )
            .unwrap();
        let interpolation = table
            .prepare_dry_particle_lut_interpolation(&prepared)
            .unwrap();
        let cuda_node = CudaPreparedTMatrixNode::new(&interpolation, 1.0).unwrap();

        let cpu_started = std::time::Instant::now();
        let cpu = (0..NODE_COUNT)
            .map(|_| {
                table
                    .evaluate_prepared_dry_particle_node_per_m3(&prepared)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let cpu_elapsed = cpu_started.elapsed();

        let mut gpu_samples = Vec::with_capacity(CUDA_BATCHES);
        let mut gpu = Vec::new();
        for _ in 0..CUDA_BATCHES {
            let gpu_started = std::time::Instant::now();
            gpu = service
                .evaluate_particles(
                    WrfTMatrixCudaTableRole::DryOblate,
                    vec![cuda_node; NODE_COUNT],
                )
                .unwrap();
            gpu_samples.push(gpu_started.elapsed());
        }
        let gpu_elapsed = gpu_samples.iter().copied().sum::<std::time::Duration>();
        let cold_gpu_elapsed = gpu_samples[0];
        let steady_gpu_elapsed = gpu_samples
            .iter()
            .skip(1)
            .copied()
            .sum::<std::time::Duration>()
            / (CUDA_BATCHES - 1) as u32;

        assert_eq!(gpu.len(), cpu.len());
        for (index, (gpu_node, cpu_node)) in gpu.iter().zip(&cpu).enumerate() {
            assert_eq!(
                gpu_node.components().map(f64::to_bits),
                cpu_node.components().map(f64::to_bits),
                "GPU node {index} differs from the scalar CPU oracle"
            );
        }
        let report = service.report();
        assert_eq!(report.requests_submitted, CUDA_BATCHES as u64);
        assert_eq!(report.batches_submitted, CUDA_BATCHES as u64);
        assert_eq!(report.batches_completed, CUDA_BATCHES as u64);
        assert_eq!(report.nodes_completed, (NODE_COUNT * CUDA_BATCHES) as u64);
        assert_eq!(report.fallback_reason, None);
        eprintln!(
            "T-matrix throughput ({}): CPU {:.0} nodes/s ({:.6} s); CUDA {:.0} nodes/s ({:.6} s mean); cold {:.6} s; steady {:.6} s; steady speedup {:.3}x; service batches {}/{}",
            report.device.name,
            NODE_COUNT as f64 / cpu_elapsed.as_secs_f64(),
            cpu_elapsed.as_secs_f64(),
            (NODE_COUNT * CUDA_BATCHES) as f64 / gpu_elapsed.as_secs_f64(),
            gpu_elapsed.as_secs_f64() / CUDA_BATCHES as f64,
            cold_gpu_elapsed.as_secs_f64(),
            steady_gpu_elapsed.as_secs_f64(),
            cpu_elapsed.as_secs_f64() / steady_gpu_elapsed.as_secs_f64(),
            report.batches_completed,
            report.batches_submitted,
        );
        eprintln!(
            "CUDA 65k batch samples: {:?}",
            gpu_samples
                .iter()
                .map(std::time::Duration::as_secs_f64)
                .collect::<Vec<_>>()
        );

        const BURST_REQUESTS: usize = 64;
        const BURST_NODES_PER_REQUEST: usize = NODE_COUNT / BURST_REQUESTS;
        let before_burst = service.report();
        let burst_started = std::time::Instant::now();
        let burst = (0..BURST_REQUESTS)
            .into_par_iter()
            .map(|_| {
                service
                    .evaluate_particles(
                        WrfTMatrixCudaTableRole::DryOblate,
                        vec![cuda_node; BURST_NODES_PER_REQUEST],
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let burst_elapsed = burst_started.elapsed();
        assert_eq!(burst.len(), BURST_REQUESTS);
        for (request, outputs) in burst.iter().enumerate() {
            assert_eq!(outputs.len(), BURST_NODES_PER_REQUEST);
            for (node, output) in outputs.iter().enumerate() {
                assert_eq!(
                    output.components().map(f64::to_bits),
                    cpu[0].components().map(f64::to_bits),
                    "burst request {request}, node {node} differs from CPU"
                );
            }
        }
        let after_burst = service.report();
        let burst_batches = after_burst.batches_completed - before_burst.batches_completed;
        assert_eq!(
            after_burst.requests_submitted - before_burst.requests_submitted,
            BURST_REQUESTS as u64
        );
        assert_eq!(
            after_burst.nodes_completed - before_burst.nodes_completed,
            NODE_COUNT as u64
        );
        eprintln!(
            "CUDA concurrent burst: {BURST_REQUESTS} requests x {BURST_NODES_PER_REQUEST} nodes -> {burst_batches} batches in {:.6} s ({:.0} nodes/s)",
            burst_elapsed.as_secs_f64(),
            NODE_COUNT as f64 / burst_elapsed.as_secs_f64(),
        );
    }

    /// Representative RawStateLinear launch-shape referee for the bounded
    /// gate pipeline. The legacy leg deliberately blocks after every
    /// category-sized request, matching v0.33.2's gate/category call boundary.
    /// The pipeline leg uses the Center-mode runtime host bound, publishes each
    /// chunk in two stable table-role sweeps, and scatters answers back to the
    /// original work positions.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    #[ignore = "manual cut-wide CUDA pipeline benchmark; requires a supported NVIDIA GPU"]
    fn manual_cut_pipeline_launch_shape_and_parity() {
        const GATES: usize = 1_024;
        const POPULATIONS_PER_GATE: usize = 3;
        const NODES_PER_POPULATION: usize = 32;
        const CENTER_BEAM_SAMPLE_POINTS: usize = 1;
        // Keep the benchmark's explicit Center-mode mirror guarded here;
        // wrf_radar owns and unit-tests the production quadrature mapping.
        const HOST_COLUMN_BUDGET: usize = 8 * 27;
        const HOST_GATE_CAP: usize = 64;
        const GATES_PER_CHUNK: usize = HOST_GATE_CAP;
        const HOST_CHUNKS: usize = (GATES + GATES_PER_CHUNK - 1) / GATES_PER_CHUNK;
        const POPULATIONS: usize = GATES * POPULATIONS_PER_GATE;
        const NODE_COUNT: usize = POPULATIONS * NODES_PER_POPULATION;

        assert_eq!(GATES_PER_CHUNK, 64);
        assert!(GATES_PER_CHUNK * CENTER_BEAM_SAMPLE_POINTS <= HOST_COLUMN_BUDGET);

        let availability = probe_cuda_cached();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping cut-wide CUDA pipeline benchmark: {availability:?}");
            return;
        };
        let service = WrfTMatrixCudaBatchService::open_with_config(
            device.ordinal,
            WrfTMatrixCudaBatchConfig::default(),
        )
        .unwrap();
        let owner = load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )
        .unwrap();
        let tables = owner.borrowed_bundle();
        let orientation = OrientationModel::GaussianCanting {
            mean_deg: 0.0,
            standard_deviation_deg: 20.0,
            quadrature_points: 50,
        };
        let prepare = |role| {
            let (table, convention, habit) = match role {
                WrfTMatrixCudaTableRole::DryOblate => (
                    tables.dry_oblate,
                    SpheroidConvention::OblateMinorVertical,
                    PsdSpheroidHabit::Oblate,
                ),
                WrfTMatrixCudaTableRole::DryProlate => (
                    tables.dry_prolate,
                    SpheroidConvention::ProlateMajorVertical,
                    PsdSpheroidHabit::Prolate,
                ),
            };
            let request = TMatrixEvaluationRequest::new(
                S_BAND_RESEARCH_FREQUENCY_HZ,
                convention,
                RadarViewGeometry::horizontal(),
            )
            .unwrap();
            let prepared = table
                .prepare_dry_particle_geometry_per_m3(
                    260.0,
                    1.0e-3,
                    400.0,
                    0.8,
                    habit,
                    None,
                    None,
                    table.dry_particle_node_fall_speed_provenance().unwrap(),
                    orientation.clone(),
                    request,
                )
                .unwrap();
            let interpolation = table
                .prepare_dry_particle_lut_interpolation(&prepared)
                .unwrap();
            let cuda = CudaPreparedTMatrixNode::new(&interpolation, 1.0).unwrap();
            let cpu = table
                .evaluate_prepared_dry_particle_node_per_m3(&prepared)
                .unwrap();
            (cuda, cpu)
        };
        let (oblate_node, oblate_cpu) = prepare(WrfTMatrixCudaTableRole::DryOblate);
        let (prolate_node, prolate_cpu) = prepare(WrfTMatrixCudaTableRole::DryProlate);

        let role_for_population = |population: usize| {
            if population % 3 == 1 {
                WrfTMatrixCudaTableRole::DryProlate
            } else {
                WrfTMatrixCudaTableRole::DryOblate
            }
        };
        let node_for_role = |role| match role {
            WrfTMatrixCudaTableRole::DryOblate => oblate_node,
            WrfTMatrixCudaTableRole::DryProlate => prolate_node,
        };

        let legacy_before = service.report();
        let legacy_started = std::time::Instant::now();
        let mut legacy = Vec::with_capacity(NODE_COUNT);
        for population in 0..POPULATIONS {
            let role = role_for_population(population);
            legacy.extend(
                service
                    .evaluate_particles(role, vec![node_for_role(role); NODES_PER_POPULATION])
                    .unwrap(),
            );
        }
        let legacy_elapsed = legacy_started.elapsed();
        let legacy_after = service.report();

        let pipeline_before = service.report();
        let pipeline_started = std::time::Instant::now();
        let mut pipeline = vec![None; NODE_COUNT];
        for first_gate in (0..GATES).step_by(GATES_PER_CHUNK) {
            let end_gate = first_gate.saturating_add(GATES_PER_CHUNK).min(GATES);
            let first_population = first_gate * POPULATIONS_PER_GATE;
            let end_population = end_gate * POPULATIONS_PER_GATE;
            let chunk_nodes = (end_population - first_population) * NODES_PER_POPULATION;
            let mut oblate_positions = Vec::with_capacity(chunk_nodes * 2 / 3);
            let mut oblate_nodes = Vec::with_capacity(chunk_nodes * 2 / 3);
            let mut prolate_positions = Vec::with_capacity(chunk_nodes / 3);
            let mut prolate_nodes = Vec::with_capacity(chunk_nodes / 3);
            for population in first_population..end_population {
                let role = role_for_population(population);
                for within_population in 0..NODES_PER_POPULATION {
                    let position = population * NODES_PER_POPULATION + within_population;
                    match role {
                        WrfTMatrixCudaTableRole::DryOblate => {
                            oblate_positions.push(position);
                            oblate_nodes.push(oblate_node);
                        }
                        WrfTMatrixCudaTableRole::DryProlate => {
                            prolate_positions.push(position);
                            prolate_nodes.push(prolate_node);
                        }
                    }
                }
            }
            let oblate = service
                .evaluate_particles(WrfTMatrixCudaTableRole::DryOblate, oblate_nodes)
                .unwrap();
            let prolate = service
                .evaluate_particles(WrfTMatrixCudaTableRole::DryProlate, prolate_nodes)
                .unwrap();
            for (position, output) in oblate_positions.into_iter().zip(oblate) {
                pipeline[position] = Some(output);
            }
            for (position, output) in prolate_positions.into_iter().zip(prolate) {
                pipeline[position] = Some(output);
            }
        }
        let pipeline_elapsed = pipeline_started.elapsed();
        let pipeline_after = service.report();

        assert_eq!(legacy.len(), NODE_COUNT);
        for (position, (legacy_output, pipeline_output)) in legacy.iter().zip(&pipeline).enumerate()
        {
            let population = position / NODES_PER_POPULATION;
            let cpu = match role_for_population(population) {
                WrfTMatrixCudaTableRole::DryOblate => oblate_cpu,
                WrfTMatrixCudaTableRole::DryProlate => prolate_cpu,
            };
            let pipeline_output = pipeline_output
                .as_ref()
                .expect("each role sweep returned every staged descriptor");
            assert_eq!(
                pipeline_output.components().map(f64::to_bits),
                legacy_output.components().map(f64::to_bits),
                "pipeline changed particle bits at ordered position {position}"
            );
            assert_eq!(
                pipeline_output.components().map(f64::to_bits),
                cpu.components().map(f64::to_bits),
                "CUDA changed scalar-oracle bits at ordered position {position}"
            );
        }

        let legacy_requests = legacy_after.requests_submitted - legacy_before.requests_submitted;
        let legacy_batches = legacy_after.batches_completed - legacy_before.batches_completed;
        let pipeline_requests =
            pipeline_after.requests_submitted - pipeline_before.requests_submitted;
        let pipeline_batches = pipeline_after.batches_completed - pipeline_before.batches_completed;
        assert_eq!(legacy_requests, POPULATIONS as u64);
        assert_eq!(pipeline_requests, (HOST_CHUNKS * 2) as u64);
        assert_eq!(pipeline_batches, pipeline_requests);
        assert_eq!(
            pipeline_after.nodes_completed - pipeline_before.nodes_completed,
            NODE_COUNT as u64
        );
        assert_eq!(pipeline_after.fallback_reason, None);
        eprintln!(
            "bounded CUDA gate pipeline ({}; artifact={}): {GATES} Center gates in {HOST_CHUNKS} x {GATES_PER_CHUNK}-gate host chunks, {POPULATIONS} populations, {NODE_COUNT} nodes; legacy {legacy_requests} requests/{legacy_batches} batches/{:.6}s; role sweeps {pipeline_requests} requests/{pipeline_batches} batches/{:.6}s; request_reduction={:.3}x; speedup {:.3}x; exact_bits=true",
            pipeline_after.device.name,
            pipeline_after.device.kernel_artifact,
            legacy_elapsed.as_secs_f64(),
            pipeline_elapsed.as_secs_f64(),
            legacy_requests as f64 / pipeline_requests as f64,
            legacy_elapsed.as_secs_f64() / pipeline_elapsed.as_secs_f64(),
        );
    }
}
