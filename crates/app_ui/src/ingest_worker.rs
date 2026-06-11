//! Background ingest worker: the bridge between the pure-widget
//! [`DownloadPanel`](rw_ui::DownloadPanel) and rw-ingest. The only crate
//! that wires the two together is this shell — rw-ui stays free of ingest
//! dependencies.
//!
//! One control thread owns a request channel (estimate / probe / latest /
//! start); responses stream back as plain data and every response fires
//! the `notify` hook (`ctx.request_repaint`). Cancellation bypasses the
//! queue: [`IngestWorker::cancel`] flips a shared `AtomicBool` the ingest
//! flow checks at stage boundaries.
//!
//! Scheduling: all CPU work (extraction, derived/heavy kernels, encode —
//! and the parallel availability probes) runs inside a DEDICATED rayon
//! pool whose threads sit at below-normal priority
//! (`rw_ingest::throttle::build_background_pool`), so the egui render
//! thread keeps normal priority and Windows preempts the compute under
//! load. The process priority is never lowered.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel, sync_channel};
use std::thread::JoinHandle;

use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::supported_forecast_hours;
use rw_ingest::ingest_profile::IngestProfile;
use rw_ingest::size_estimate::{Calibration, default_calibration_paths, estimate};
use rw_ingest::{IngestConfig, IngestError, IngestEvent, IngestStage, parse_hours, throttle};
use rw_ui::{AvailabilityView, DownloadSpec, DownloadStage, EstimateView, HourDoneView};

/// Requests from the UI thread.
#[derive(Debug, Clone)]
pub enum IngestRequest {
    /// Recompute the (local, cheap) size estimate for a spec.
    Estimate(DownloadSpec),
    /// Probe which forecast hours of the spec's run exist upstream.
    Probe(DownloadSpec),
    /// Find the newest available run for the spec's model.
    Latest(DownloadSpec),
    /// Run the download/ingest.
    Start(DownloadSpec),
}

/// Responses to the UI thread — all plain data, panel-ready.
#[derive(Debug, Clone)]
pub enum IngestResponse {
    /// `Err` carries a spec/validation problem for the panel's error slot.
    Estimate(Box<Result<EstimateView, String>>),
    Availability(AvailabilityView),
    Latest {
        date: String,
        cycle: u8,
    },
    LatestFailed(String),
    /// A run began over these hours.
    Started {
        hours: Vec<u16>,
    },
    StageStarted {
        hour: u16,
        stage: DownloadStage,
    },
    StageDone {
        hour: u16,
        stage: DownloadStage,
        ms: u128,
    },
    /// A historical ingest stdout/stderr line.
    Note(String),
    HourDone(HourDoneView),
    Finished,
    Cancelled,
    Failed(String),
}

/// Handle to the ingest worker thread.
pub struct IngestWorker {
    tx: Sender<IngestRequest>,
    rx: Receiver<IngestResponse>,
    cancel: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl IngestWorker {
    /// Spawn the worker. `store_root` is where ingested hours land (the
    /// same root the run browser shows); `notify` wakes the UI after every
    /// response.
    pub fn spawn(store_root: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = channel::<IngestRequest>();
        let (resp_tx, resp_rx) = channel::<IngestResponse>();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let thread = std::thread::Builder::new()
            .name("rw-ingest-worker".to_string())
            .spawn(move || {
                throttle::set_current_thread_background_priority();
                worker_loop(store_root, &req_rx, &resp_tx, &notify, &worker_cancel);
            })
            .expect("spawn ingest worker thread");
        Self {
            tx: req_tx,
            rx: resp_rx,
            cancel,
            _thread: thread,
        }
    }

    /// Queue a request (dropped silently if the worker died).
    pub fn send(&self, request: IngestRequest) {
        let _ = self.tx.send(request);
    }

    /// Non-blocking poll for the next response (drain once per frame).
    pub fn try_recv(&self) -> Option<IngestResponse> {
        self.rx.try_recv().ok()
    }

    /// Request cancellation of the running ingest. Takes effect at the
    /// next stage boundary (the in-flight stage completes first); bypasses
    /// the request queue so it lands while a run is in progress.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Worker-side state: the dedicated below-normal compute pool (built once,
/// lazily).
struct WorkerState {
    store_root: PathBuf,
    pool: Option<rayon::ThreadPool>,
}

impl WorkerState {
    fn pool(&mut self) -> &rayon::ThreadPool {
        self.pool
            .get_or_insert_with(|| throttle::build_background_pool(None))
    }
}

fn worker_loop(
    store_root: PathBuf,
    requests: &Receiver<IngestRequest>,
    responses: &Sender<IngestResponse>,
    notify: &(impl Fn() + Send + Sync + 'static),
    cancel: &AtomicBool,
) {
    let mut state = WorkerState {
        store_root,
        pool: None,
    };
    let send = |response: IngestResponse| {
        let ok = responses.send(response).is_ok();
        notify();
        ok
    };
    while let Ok(request) = requests.recv() {
        match request {
            IngestRequest::Estimate(spec) => {
                let result = compute_estimate(&state.store_root, &spec);
                if !send(IngestResponse::Estimate(Box::new(result))) {
                    return;
                }
            }
            IngestRequest::Probe(spec) => {
                // Probe unconditionally: the button is the only producer of
                // Probe requests, and a click means "look again" — a fresh
                // run gains hours over a session, so a per-run cache would
                // freeze the chips while the spinner claims a fresh result.
                let view = probe_availability(&mut state, &spec);
                if !send(IngestResponse::Availability(view)) {
                    return;
                }
            }
            IngestRequest::Latest(spec) => {
                let response = match find_latest(&spec) {
                    Ok((date, cycle)) => IngestResponse::Latest { date, cycle },
                    Err(message) => IngestResponse::LatestFailed(message),
                };
                if !send(response) {
                    return;
                }
            }
            IngestRequest::Start(spec) => {
                run_download(&mut state, &spec, responses, notify, cancel);
            }
        }
    }
}

/// Spec -> validated `(model, profile, hours, cycle)` or a panel-ready
/// error message. The validation path the panel relies on: an invalid
/// combination must never reach `process_fetched_hour`.
fn resolve_spec(
    spec: &DownloadSpec,
) -> Result<(ModelId, IngestProfile, Vec<u16>, CycleSpec), String> {
    let model: ModelId = spec
        .model
        .parse()
        .map_err(|_| format!("unknown model '{}'", spec.model))?;
    if !rw_ingest::ingest_supported(model) {
        return Err(format!(
            "model '{}' is not ingest-supported yet (HRRR only today)",
            spec.model
        ));
    }
    let mut profile = IngestProfile::preset(&spec.profile)?;
    profile.level_step_hpa = spec.level_step_hpa;
    profile.derived = spec.derived;
    profile.heavy = spec.heavy;
    profile.validate()?;
    let hours = parse_hours(&spec.hours).map_err(|err| err.to_string())?;
    let cycle = CycleSpec::new(spec.date.clone(), spec.cycle).map_err(|err| err.to_string())?;
    let supported = supported_forecast_hours(model, spec.cycle);
    if let Some(&bad) = hours.iter().find(|hour| !supported.contains(hour)) {
        return Err(format!(
            "hour {bad} is outside the supported range for the {:02}z cycle (max {})",
            spec.cycle,
            supported.last().copied().unwrap_or(0)
        ));
    }
    Ok((model, profile, hours, cycle))
}

/// Source spec -> override: "auto" tries every catalog source in order.
fn source_override(spec: &DownloadSpec) -> Result<Option<SourceId>, String> {
    if spec.source == "auto" {
        return Ok(None);
    }
    spec.source
        .parse::<SourceId>()
        .map(Some)
        .map_err(|_| format!("unknown source '{}'", spec.source))
}

/// Local, no-network estimate: calibrate from the newest stored hours of
/// the same model (else the built-in HRRR measurements) and price the
/// profile.
fn compute_estimate(
    store_root: &std::path::Path,
    spec: &DownloadSpec,
) -> Result<EstimateView, String> {
    let (model, profile, hours, _) = resolve_spec(spec)?;
    source_override(spec)?;
    let model_slug = model.as_str().replace('-', "_");
    let paths = default_calibration_paths(store_root, &model_slug);
    let calibration = if paths.is_empty() {
        Calibration::builtin_default()
    } else {
        Calibration::from_hour_files(&paths).unwrap_or_else(|_| Calibration::builtin_default())
    };
    let hour_count = hours.len() as u16;
    let estimate = estimate(&profile, model, hour_count, &calibration);
    Ok(EstimateView {
        profile_summary: profile.describe(),
        hour_count,
        store_bytes: estimate.store_bytes,
        download_bytes: estimate.download_bytes,
        per_hour_store_bytes: estimate.per_hour_store_bytes,
        per_hour_download_bytes: estimate.per_hour_download_bytes,
        calibration: calibration.source.clone(),
        time_hint: format_time_hint(estimate.download_bytes),
        breakdown: estimate.breakdown,
    })
}

/// Rough cache-cold download wall-clock at an assumed 40 MB/s — labeled as
/// such; a warm raw-byte cache makes the fetch a disk read.
pub fn format_time_hint(download_bytes: u64) -> String {
    const ASSUMED_BYTES_PER_SEC: f64 = 40.0 * 1024.0 * 1024.0;
    let secs = download_bytes as f64 / ASSUMED_BYTES_PER_SEC;
    if secs < 90.0 {
        format!("≈{secs:.0} s download @ 40 MB/s (cache-cold)")
    } else {
        format!(
            "≈{:.0} m {:02.0} s download @ 40 MB/s (cache-cold)",
            (secs / 60.0).floor(),
            secs % 60.0
        )
    }
}

/// Probe the run's hours via AWS idx HEADs (parallel on the background
/// pool; HRRR's catalog order would otherwise probe NOMADS serially —
/// 49 round trips). The actual fetch still tries every source in catalog
/// order, so a freshest-hour lag on AWS only affects the chips.
fn probe_availability(state: &mut WorkerState, spec: &DownloadSpec) -> AvailabilityView {
    let mut view = AvailabilityView {
        model: spec.model.clone(),
        date: spec.date.clone(),
        cycle: spec.cycle,
        candidates: Vec::new(),
        available: Vec::new(),
        note: None,
    };
    let (model, profile) = match resolve_spec(spec) {
        Ok((model, profile, _, _)) => (model, profile),
        Err(message) => {
            view.note = Some(message);
            return view;
        }
    };
    view.candidates = supported_forecast_hours(model, spec.cycle);
    let date = spec.date.clone();
    let cycle = spec.cycle;
    let probe = |product: &str| {
        rustwx_io::available_forecast_hours(model, &date, cycle, product, Some(SourceId::Aws))
    };
    let result = state.pool().install(|| {
        let sfc = probe("sfc")?;
        if !profile.needs_prs() {
            return Ok(sfc);
        }
        let prs = probe("prs")?;
        Ok::<Vec<u16>, rustwx_io::IoError>(
            sfc.into_iter().filter(|hour| prs.contains(hour)).collect(),
        )
    });
    match result {
        Ok(available) => {
            view.available = available;
            view.note = Some("probed via AWS idx".to_string());
        }
        Err(err) => view.note = Some(format!("availability probe failed: {err}")),
    }
    view
}

/// Newest available run for the spec's model, walking back from the spec's
/// date (AWS-pinned probes for speed).
fn find_latest(spec: &DownloadSpec) -> Result<(String, u8), String> {
    let model: ModelId = spec
        .model
        .parse()
        .map_err(|_| format!("unknown model '{}'", spec.model))?;
    let latest = rustwx_models::latest_available_run(model, Some(SourceId::Aws), &spec.date)
        .map_err(|err| format!("latest-run probe failed: {err}"))?;
    Ok((latest.cycle.date_yyyymmdd, latest.cycle.hour_utc))
}

/// rw-ingest stage -> panel stage.
fn map_stage(stage: IngestStage) -> DownloadStage {
    match stage {
        IngestStage::FetchPrs => DownloadStage::FetchPrs,
        IngestStage::FetchSfc => DownloadStage::FetchSfc,
        IngestStage::ExtractPrs => DownloadStage::ExtractPrs,
        IngestStage::ExtractSfc => DownloadStage::ExtractSfc,
        IngestStage::ThermoDecode => DownloadStage::ThermoDecode,
        IngestStage::Derived => DownloadStage::Derived,
        IngestStage::Heavy => DownloadStage::Heavy,
        IngestStage::Write => DownloadStage::Write,
        IngestStage::Verify => DownloadStage::Verify,
    }
}

/// IngestEvent -> panel response.
fn map_event(event: IngestEvent) -> IngestResponse {
    match event {
        IngestEvent::StageStarted { hour, stage } => IngestResponse::StageStarted {
            hour,
            stage: map_stage(stage),
        },
        IngestEvent::StageDone { hour, stage, ms } => IngestResponse::StageDone {
            hour,
            stage: map_stage(stage),
            ms,
        },
        IngestEvent::Info { message, .. } | IngestEvent::Warning { message, .. } => {
            IngestResponse::Note(message)
        }
    }
}

/// The download itself: rw_batch's proven pipeline shape — a fetch thread
/// feeding a `sync_channel(1)` of [`rw_ingest::FetchedHour`] (bounding
/// resident raw bytes), with the CPU half running inside the dedicated
/// below-normal pool via `install()` so every nested `par_iter` (GRIB
/// extraction, derived/heavy kernels, zstd encode) rides the capped pool.
fn run_download(
    state: &mut WorkerState,
    spec: &DownloadSpec,
    responses: &Sender<IngestResponse>,
    notify: &(impl Fn() + Send + Sync + 'static),
    cancel: &AtomicBool,
) {
    let send = |response: IngestResponse| {
        let ok = responses.send(response).is_ok();
        notify();
        ok
    };
    let (model, profile, hours, cycle) = match resolve_spec(spec) {
        Ok(resolved) => resolved,
        Err(message) => {
            send(IngestResponse::Failed(message));
            return;
        }
    };
    let source = match source_override(spec) {
        Ok(source) => source,
        Err(message) => {
            send(IngestResponse::Failed(message));
            return;
        }
    };
    let cache_root = PathBuf::from(&spec.cache_dir);
    if let Err(err) = std::fs::create_dir_all(&cache_root) {
        send(IngestResponse::Failed(format!(
            "cache dir {}: {err}",
            cache_root.display()
        )));
        return;
    }
    let model_slug = model.as_str().replace('-', "_");
    let run_slug = format!("{}_{:02}z", spec.date, spec.cycle);

    cancel.store(false, Ordering::Relaxed);
    if !send(IngestResponse::Started {
        hours: hours.clone(),
    }) {
        return;
    }

    // Progress sink shared by the fetch and process halves: forward every
    // event and wake the UI. Sender is !Sync, hence the Mutex.
    let event_tx = std::sync::Mutex::new(responses.clone());
    let progress = move |event: IngestEvent| {
        if let Ok(tx) = event_tx.lock() {
            let _ = tx.send(map_event(event));
        }
        notify();
    };
    let config = IngestConfig {
        model,
        cycle: &cycle,
        source_override: source,
        cache_root: &cache_root,
        use_cache: true,
        store_root: &state.store_root,
        model_slug: &model_slug,
        run_slug: &run_slug,
        profile: &profile,
        verify: spec.verify,
        progress: &progress,
        cancel,
    };

    let pool = state
        .pool
        .get_or_insert_with(|| throttle::build_background_pool(None));

    let outcome: Result<(), IngestError> = std::thread::scope(|scope| {
        // Raw bytes are ~575 MB/hour warm; capacity 1 bounds resident
        // raw-byte sets to <= 3 (fetching + queued + processing).
        let (fetched_tx, fetched_rx) =
            sync_channel::<Result<rw_ingest::FetchedHour, IngestError>>(1);
        let fetch_hours = hours.clone();
        let fetch_config = &config;
        scope.spawn(move || {
            throttle::set_current_thread_background_priority();
            for &hour in &fetch_hours {
                match rw_ingest::fetch_hour(fetch_config, hour) {
                    Ok(fetched) => {
                        if fetched_tx.send(Ok(fetched)).is_err() {
                            return; // process half bailed
                        }
                    }
                    Err(err) => {
                        let _ = fetched_tx.send(Err(err));
                        return;
                    }
                }
            }
        });

        // CPU half on the dedicated pool: this install() is the
        // load-bearing line — nested rayon work stays on the capped
        // below-normal pool.
        let process_config = &config;
        let hour_done_tx = responses.clone();
        pool.install(move || {
            while let Ok(message) = fetched_rx.recv() {
                let fetched = message?;
                let hour = fetched.hour;
                let ingested = rw_ingest::process_fetched_hour(process_config, fetched)?;
                let _ = hour_done_tx.send(IngestResponse::HourDone(HourDoneView {
                    hour,
                    store_mb: ingested.store_mb,
                    total_ms: ingested.total_ms(),
                }));
                notify();
            }
            Ok(())
        })
    });

    match outcome {
        Ok(()) => {
            send(IngestResponse::Finished);
        }
        Err(IngestError::Cancelled) => {
            send(IngestResponse::Cancelled);
        }
        Err(err) => {
            send(IngestResponse::Failed(err.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DownloadSpec {
        DownloadSpec {
            model: "hrrr".to_string(),
            date: "20260608".to_string(),
            cycle: 0,
            hours: "4-6".to_string(),
            profile: "sounding".to_string(),
            level_step_hpa: 25,
            derived: false,
            heavy: false,
            source: "auto".to_string(),
            cache_dir: "out/cache".to_string(),
            verify: false,
        }
    }

    #[test]
    fn resolve_spec_accepts_a_valid_sounding_spec() {
        let (model, profile, hours, cycle) = resolve_spec(&spec()).expect("valid spec resolves");
        assert_eq!(model, ModelId::Hrrr);
        assert!(!profile.derived && !profile.heavy);
        assert_eq!(hours, vec![4, 5, 6]);
        assert_eq!(cycle.hour_utc, 0);
    }

    #[test]
    fn resolve_spec_surfaces_validation_errors_instead_of_panicking() {
        // Heavy on a sounding profile: the exact combination that would
        // trip process_fetched_hour's debug_assert if it got through.
        let mut bad = spec();
        bad.heavy = true;
        bad.derived = true;
        let message = resolve_spec(&bad).expect_err("invalid profile must surface");
        assert!(message.contains("named surface subset"), "got: {message}");

        let mut bad = spec();
        bad.hours = "5x".to_string();
        assert!(
            resolve_spec(&bad)
                .expect_err("bad hours")
                .contains("--hours")
        );

        let mut bad = spec();
        bad.model = "gfs".to_string();
        let message = resolve_spec(&bad).expect_err("unsupported model");
        assert!(message.contains("not ingest-supported"), "got: {message}");

        let mut bad = spec();
        bad.date = "not-a-date".to_string();
        assert!(resolve_spec(&bad).is_err());

        let mut bad = spec();
        bad.cycle = 1;
        bad.hours = "0-48".to_string(); // 01z HRRR tops out at 18
        let message = resolve_spec(&bad).expect_err("out-of-range hour");
        assert!(
            message.contains("outside the supported range"),
            "got: {message}"
        );
    }

    #[test]
    fn source_override_maps_auto_and_slugs() {
        assert_eq!(source_override(&spec()).unwrap(), None);
        let mut aws = spec();
        aws.source = "aws".to_string();
        assert_eq!(source_override(&aws).unwrap(), Some(SourceId::Aws));
        let mut bad = spec();
        bad.source = "carrier-pigeon".to_string();
        assert!(source_override(&bad).is_err());
    }

    /// The estimate path is fully local (no network): a valid spec prices
    /// against the builtin calibration when no store exists.
    #[test]
    fn compute_estimate_works_offline_with_builtin_calibration() {
        let estimate = compute_estimate(std::path::Path::new("definitely-missing-store"), &spec())
            .expect("estimate resolves");
        assert_eq!(estimate.hour_count, 3);
        assert!(estimate.store_bytes > 0);
        assert!(estimate.download_bytes > 0);
        assert!(
            estimate.calibration.contains("built-in defaults"),
            "no store -> builtin calibration with honest provenance, got: {}",
            estimate.calibration
        );
        assert!(!estimate.breakdown.is_empty());
        assert!(estimate.time_hint.contains("cache-cold"));
    }

    #[test]
    fn time_hint_formats_seconds_and_minutes() {
        assert_eq!(format_time_hint(0), "≈0 s download @ 40 MB/s (cache-cold)");
        // 1.6 GB at 40 MB/s ≈ 41 s.
        let hint = format_time_hint(1_677_721_600);
        assert_eq!(hint, "≈40 s download @ 40 MB/s (cache-cold)");
        let hint = format_time_hint(40 * 1024 * 1024 * 150);
        assert!(hint.starts_with("≈2 m 30 s"), "got: {hint}");
    }

    /// A Start over an invalid spec responds Failed without spawning any
    /// pipeline (and without panicking in a release build).
    #[test]
    fn start_with_invalid_spec_fails_cleanly() {
        let worker = IngestWorker::spawn(PathBuf::from("missing-store"), || {});
        let mut bad = spec();
        bad.heavy = true; // invalid on sounding
        worker.send(IngestRequest::Start(bad));
        let response = worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker responds");
        match response {
            IngestResponse::Failed(message) => {
                assert!(message.contains("named surface subset"), "got: {message}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Every Probe request re-probes — a second button click on the same
    /// run must never return a stale per-session cache entry (a fresh run
    /// gains hours over the session; the chips must track that).
    ///
    /// Offline proof: two specs share the old cache key (model, date,
    /// cycle) but carry different validation failures, and validation runs
    /// inside the probe before any network I/O. A cache keyed on the run
    /// would answer the second request with the first request's note.
    #[test]
    fn probe_reprobes_on_every_request() {
        let worker = IngestWorker::spawn(PathBuf::from("missing-store"), || {});
        let mut first = spec();
        first.hours = "5x".to_string(); // parse failure -> "--hours" note
        let mut second = spec();
        second.heavy = true; // invalid on sounding -> "named surface subset"
        worker.send(IngestRequest::Probe(first));
        worker.send(IngestRequest::Probe(second));
        let mut notes = Vec::new();
        for _ in 0..2 {
            let response = worker
                .rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("worker responds");
            match response {
                IngestResponse::Availability(view) => {
                    notes.push(view.note.expect("failed probe carries a note"));
                }
                other => panic!("expected Availability, got {other:?}"),
            }
        }
        assert!(notes[0].contains("--hours"), "got: {}", notes[0]);
        assert!(
            notes[1].contains("named surface subset"),
            "second probe must be fresh, not the first probe's cached view; got: {}",
            notes[1]
        );
    }

    /// Estimate requests round-trip through the worker thread.
    #[test]
    fn estimate_round_trips_through_the_worker() {
        let worker = IngestWorker::spawn(PathBuf::from("missing-store"), || {});
        worker.send(IngestRequest::Estimate(spec()));
        let response = worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker responds");
        match response {
            IngestResponse::Estimate(result) => {
                let view = result.expect("valid spec estimates");
                assert_eq!(view.hour_count, 3);
            }
            other => panic!("expected Estimate, got {other:?}"),
        }
    }
}
