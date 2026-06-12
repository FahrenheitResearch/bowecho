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
            "model '{}' is not ingest-supported yet",
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
        Calibration::from_hour_files(&paths, model)
            .unwrap_or_else(|_| Calibration::builtin_default())
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
        time_hint: format_time_hint(estimate.download_bytes, &profile, hour_count),
        breakdown: estimate.breakdown,
    })
}

/// Rough cache-cold download wall-clock at an assumed 40 MB/s — labeled as
/// such; a warm raw-byte cache makes the fetch a disk read.
pub fn format_time_hint(
    download_bytes: u64,
    profile: &rw_ingest::ingest_profile::IngestProfile,
    hour_count: u16,
) -> String {
    const ASSUMED_BYTES_PER_SEC: f64 = 40.0 * 1024.0 * 1024.0;
    let secs = download_bytes as f64 / ASSUMED_BYTES_PER_SEC;
    let download = if secs < 90.0 {
        format!("≈{secs:.0} s download @ 40 MB/s (cache-cold)")
    } else {
        format!(
            "≈{:.0} m {:02.0} s download @ 40 MB/s (cache-cold)",
            (secs / 60.0).floor(),
            secs % 60.0
        )
    };
    // Compute is the part the download number hides — heavy's full ECAPE
    // stage is ~5x and dominates (field report: "downloading is slow" was
    // heavy compute, not the network).
    if profile.heavy {
        let low = hour_count as f64 * 1.5;
        let high = hour_count as f64 * 4.0;
        format!(
            "{download} · ⚠ HEAVY: full ECAPE pins EVERY core at ~100% for ≈{low:.0}–{high:.0} min (fast desktop → laptop). Other profiles barely load the machine."
        )
    } else {
        let est = (hour_count as f64 * 0.5).max(0.5);
        format!("{download} · +≈{est:.0} min compute")
    }
}

/// Probe the run's hours via AWS idx HEADs (parallel on the background
/// pool; HRRR's catalog order would otherwise probe NOMADS serially —
/// 49 round trips). The actual fetch still tries every source in catalog
/// order, so a freshest-hour lag on AWS only affects the chips.
///
/// Products come from the model's ingest fetch plan (HRRR: `prs`+`sfc`
/// pair, GFS: single `pgrb2.0p25`); an hour is available when every
/// product the profile needs exists. A pressure-only file (HRRR `prs`)
/// is probed only when the profile reads isobaric data.
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
    let products = match probe_products(model, &profile) {
        Ok(products) => products,
        Err(message) => {
            view.note = Some(message);
            return view;
        }
    };
    view.candidates = supported_forecast_hours(model, spec.cycle);
    let date = spec.date.clone();
    let cycle = spec.cycle;
    let probe = |product: &str, source: Option<SourceId>| {
        rustwx_io::available_forecast_hours(model, &date, cycle, product, source)
    };
    let result = state.pool().install(|| {
        // An hour is available when every fetch-plan product has it.
        let run = |source: Option<SourceId>| {
            let mut available: Option<Vec<u16>> = None;
            for product in &products {
                let hours = probe(product, source)?;
                available = Some(match available {
                    None => hours,
                    Some(have) => have
                        .into_iter()
                        .filter(|hour| hours.contains(hour))
                        .collect(),
                });
            }
            Ok::<Vec<u16>, rustwx_io::IoError>(available.unwrap_or_default())
        };
        // Fast path stays the parallel AWS idx HEADs; an EMPTY result
        // walks the full source catalog once — NOMADS publishes minutes
        // ahead of the mirrors, and a cycle in that window (or an AWS
        // outage) must not read as absent (field request: upload time
        // matters).
        let aws = run(Some(SourceId::Aws))?;
        if aws.is_empty() {
            run(None).map(|hours| (hours, true))
        } else {
            Ok((aws, false))
        }
    });
    match result {
        Ok((available, walked_catalog)) => {
            view.available = available;
            view.note = Some(if walked_catalog {
                "probed across the source catalog (cycle not on AWS yet)".to_owned()
            } else {
                "probed via AWS idx".to_owned()
            });
        }
        Err(err) => view.note = Some(format!("availability probe failed: {err}")),
    }
    view
}

/// The product files an availability probe must check for `model` under
/// `profile`: every fetch-plan entry the profile actually reads. Surface
/// sources are always read; pressure sources only when the profile needs
/// isobaric data (`needs_prs`). HRRR sounding-grade -> `["prs", "sfc"]`
/// (plan order); GFS -> `["pgrb2.0p25"]`.
fn probe_products(model: ModelId, profile: &IngestProfile) -> Result<Vec<&'static str>, String> {
    let plan = rw_ingest::fetch_plan(model).map_err(|err| err.to_string())?;
    Ok(plan
        .iter()
        .filter(|fetch| fetch.surface_source || (fetch.pressure_source && profile.needs_prs()))
        .map(|fetch| fetch.product)
        .collect())
}

/// Newest available run for the spec's model, walking back from the spec's
/// date. Probes the whole source catalog (or the spec's pinned source):
/// the engine prefers the newest cycle over source priority, so a run
/// that NOMADS has published but the mirrors haven't yet still wins —
/// the old AWS pin both lagged fresh cycles and turned one source's
/// outage into "no working source found".
fn find_latest(spec: &DownloadSpec) -> Result<(String, u8), String> {
    let model: ModelId = spec
        .model
        .parse()
        .map_err(|_| format!("unknown model '{}'", spec.model))?;
    let source = source_override(spec)?;
    let latest = rustwx_models::latest_available_run(model, source, &spec.date)
        .map_err(|err| format!("latest-run probe failed: {err}"))?;
    Ok((latest.cycle.date_yyyymmdd, latest.cycle.hour_utc))
}

// --- One-click ("Fetch latest") ingest helpers — pure, no network. ---

/// Rough HRRR CONUS coverage test: lat 21..52.5 N, lon 134..60 W. The
/// HRRR Lambert domain's top edge sags toward its corners (≈52.6 N at
/// top-center, lower east/west), so the box top stays inside the grid and
/// the test errs toward the GFS fallback when a radar is outside CONUS
/// (Guam, Alaska, international feeds). The northernmost CONUS 88Ds sit
/// ≈48.7 N, far from the edge either way.
pub fn hrrr_conus_covers(lat_deg: f32, lon_deg: f32) -> bool {
    (21.0..=52.5).contains(&lat_deg) && (-134.0..=-60.0).contains(&lon_deg)
}

/// Publication lag floor for a model: minutes after init when the first
/// forecast files are plausibly complete upstream. HRRR hours appear
/// ~50-55 min after init; GFS pgrb2.0p25 early hours land ~3.5-4 h after
/// init (NCEP production suite timing), so 220 min is the optimistic
/// edge — the one-click path falls back one cycle when the guess loses.
pub fn publication_lag_minutes(model: ModelId) -> i64 {
    match model {
        ModelId::Gfs => 220,
        _ => 55,
    }
}

/// The freshest `count` plausible runs for a model with `cycle_hours`
/// cadence: every cycle init at or before `now - lag_minutes`, newest
/// first, as (YYYYMMDD, cycle-hour) pairs. Generalizes the HRRR one-click
/// guess (hourly cadence, candidates at now-55 m and now-115 m) to sparse
/// cadences like GFS's 00/06/12/18z.
pub fn recent_cycle_candidates(
    now: chrono::DateTime<chrono::Utc>,
    cycle_hours: &[u8],
    lag_minutes: i64,
    count: usize,
) -> Vec<(String, u8)> {
    use chrono::Timelike;
    let mut cycles: Vec<u8> = cycle_hours.iter().copied().filter(|&h| h < 24).collect();
    cycles.sort_unstable();
    cycles.dedup();
    if cycles.is_empty() || count == 0 {
        return Vec::new();
    }
    let cutoff = now - chrono::Duration::minutes(lag_minutes);
    let cutoff_hour = cutoff.hour() as u8;
    let mut out = Vec::with_capacity(count);
    let mut day = cutoff.date_naive();
    let mut first_day = true;
    while out.len() < count {
        for &cycle in cycles.iter().rev() {
            if first_day && cycle > cutoff_hour {
                continue;
            }
            out.push((day.format("%Y%m%d").to_string(), cycle));
            if out.len() == count {
                break;
            }
        }
        first_day = false;
        let Some(previous) = day.pred_opt() else {
            break;
        };
        day = previous;
    }
    out
}

/// First forecast hour worth ingesting for a run initialized
/// `age_minutes` ago: the hour whose valid time most recently passed, so
/// `first..=first+3` brackets the now..now+3 h window the auto hail-env
/// path samples. A fresh HRRR run (age <2 h) starts at f00/f01; an older
/// GFS cycle (age ~4-10 h) starts at f04-f10 instead of wasting the
/// fetch on hours whose valid times are already hours in the past.
pub fn first_live_hour(age_minutes: i64) -> u16 {
    (age_minutes.max(0) / 60) as u16
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
    // Relative cache paths (the crate default, or persisted old specs)
    // resolve inside the read-only bundle on macOS — force them under the
    // app's config-scoped cache instead.
    let cache_root = {
        let p = PathBuf::from(&spec.cache_dir);
        if p.is_absolute() {
            p
        } else {
            settings::model_cache_dir()
        }
    };
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

    /// Field repro: GFS Latest/Download did nothing with no error. Drives
    /// the REAL worker thread with a gfs spec exactly like the panel does
    /// and requires a response for every request — a worker panic shows up
    /// here as a recv timeout instead of a silent no-op.
    /// `cargo test -p app_ui gfs_panel_live -- --ignored --nocapture`
    #[test]
    #[ignore = "live network probe of the GFS panel path"]
    fn gfs_panel_live_latest_estimate_probe_all_answer() {
        let scratch = std::env::temp_dir().join("bowecho-gfs-panel-live");
        let worker = IngestWorker::spawn(scratch, || {});
        let mut gfs = spec();
        gfs.model = "gfs".to_string();
        gfs.date = chrono::Utc::now().format("%Y%m%d").to_string();
        gfs.cycle = 0;
        gfs.hours = "0-3".to_string();

        let recv = |what: &str| -> IngestResponse {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                if let Some(response) = worker.try_recv() {
                    return response;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what}: no response in 120 s — worker thread likely dead"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        };

        worker.send(IngestRequest::Estimate(gfs.clone()));
        let estimate = recv("estimate");
        println!("estimate -> {estimate:?}");

        worker.send(IngestRequest::Latest(gfs.clone()));
        let latest = recv("latest");
        println!("latest -> {latest:?}");
        match &latest {
            IngestResponse::Latest { date, cycle } => {
                gfs.date = date.clone();
                gfs.cycle = *cycle;
            }
            other => panic!("Latest must answer Latest{{..}}, got {other:?}"),
        }

        worker.send(IngestRequest::Probe(gfs.clone()));
        let probe = recv("probe");
        println!("probe -> {probe:?}");
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

        // GFS became ingest-supported at rusty-weather d853afa (multi-model
        // phase A) — it must now RESOLVE; a bogus slug still errors.
        let mut good = spec();
        good.model = "gfs".to_string();
        good.hours = "6".to_string(); // GFS cadence
        assert!(resolve_spec(&good).is_ok(), "gfs is ingest-supported now");
        let mut bad = spec();
        bad.model = "definitely-not-a-model".to_string();
        assert!(resolve_spec(&bad).is_err());

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
    fn conus_box_separates_hrrr_radars_from_international_sites() {
        // Inside: KTLX, KEAX, Miami, Seattle.
        assert!(hrrr_conus_covers(35.33, -97.28));
        assert!(hrrr_conus_covers(38.81, -94.26));
        assert!(hrrr_conus_covers(25.76, -80.19));
        assert!(hrrr_conus_covers(47.68, -122.25));
        // Outside: Stockholm, Guam (PGUA), Anchorage (PAHG), São Paulo.
        assert!(!hrrr_conus_covers(59.3, 18.1));
        assert!(!hrrr_conus_covers(13.46, 144.81));
        assert!(!hrrr_conus_covers(60.73, -151.35));
        assert!(!hrrr_conus_covers(-23.55, -46.63));
        // Box edges stay inside (top capped at 52.5 N: the HRRR Lambert
        // domain's upper edge sags toward its corners).
        assert!(hrrr_conus_covers(21.0, -134.0));
        assert!(hrrr_conus_covers(52.5, -60.0));
        assert!(!hrrr_conus_covers(52.9, -100.0));
    }

    fn utc(date: &str, h: u32, m: u32) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        let d = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").unwrap();
        chrono::Utc.from_utc_datetime(&d.and_hms_opt(h, m, 0).unwrap())
    }

    #[test]
    fn hourly_candidates_match_the_historical_hrrr_guess() {
        // The HRRR one-click guess was (now-55m, now-115m) formatted to the
        // hour. The generalized helper must reproduce it exactly.
        let hourly: Vec<u8> = (0..24).collect();
        let now = utc("20260611", 12, 30);
        let got = recent_cycle_candidates(now, &hourly, 55, 2);
        assert_eq!(
            got,
            vec![("20260611".to_string(), 11), ("20260611".to_string(), 10)]
        );
        // Midnight wrap: 00:30 UTC - 55 m lands on the previous day.
        let got = recent_cycle_candidates(utc("20260611", 0, 30), &hourly, 55, 2);
        assert_eq!(
            got,
            vec![("20260610".to_string(), 23), ("20260610".to_string(), 22)]
        );
    }

    #[test]
    fn six_hourly_candidates_respect_cadence_and_lag() {
        let gfs: &[u8] = &[0, 6, 12, 18];
        // 14:00 UTC with a 220-min lag: cutoff 10:20 -> 06z, then 00z.
        let got = recent_cycle_candidates(utc("20260611", 14, 0), gfs, 220, 2);
        assert_eq!(
            got,
            vec![("20260611".to_string(), 6), ("20260611".to_string(), 0)]
        );
        // 16:00 UTC: cutoff 12:20 -> 12z is in.
        let got = recent_cycle_candidates(utc("20260611", 16, 0), gfs, 220, 2);
        assert_eq!(
            got,
            vec![("20260611".to_string(), 12), ("20260611".to_string(), 6)]
        );
        // 02:00 UTC: cutoff 22:20 previous day -> 18z, 12z of 06/10.
        let got = recent_cycle_candidates(utc("20260611", 2, 0), gfs, 220, 2);
        assert_eq!(
            got,
            vec![("20260610".to_string(), 18), ("20260610".to_string(), 12)]
        );
        // Degenerate inputs.
        assert!(recent_cycle_candidates(utc("20260611", 2, 0), &[], 220, 2).is_empty());
        assert!(recent_cycle_candidates(utc("20260611", 2, 0), gfs, 220, 0).is_empty());
    }

    #[test]
    fn first_live_hour_floors_run_age_to_hours() {
        assert_eq!(first_live_hour(0), 0);
        assert_eq!(first_live_hour(55), 0);
        assert_eq!(first_live_hour(60), 1);
        assert_eq!(first_live_hour(115), 1);
        assert_eq!(first_live_hour(220), 3);
        assert_eq!(first_live_hour(9 * 60 + 40), 9);
        assert_eq!(first_live_hour(-5), 0); // clock skew never panics
    }

    #[test]
    fn probe_products_follow_the_fetch_plan() {
        // HRRR sounding-grade reads 3D volumes -> both prs and sfc.
        let sounding = IngestProfile::preset("sounding").unwrap();
        assert_eq!(
            probe_products(ModelId::Hrrr, &sounding).unwrap(),
            vec!["prs", "sfc"]
        );
        // GFS: one file serves both roles.
        assert_eq!(
            probe_products(ModelId::Gfs, &sounding).unwrap(),
            vec!["pgrb2.0p25"]
        );
        // Models without a fetch plan surface an error, not a panic.
        assert!(probe_products(ModelId::Rap, &sounding).is_err());
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
        let light = rw_ingest::ingest_profile::IngestProfile::sounding();
        let hint = format_time_hint(0, &light, 1);
        assert!(hint.starts_with("≈0 s download"), "got: {hint}");
        // 1.6 GB at 40 MB/s ≈ 41 s; light profile adds a small compute note.
        let hint = format_time_hint(1_677_721_600, &light, 3);
        assert!(hint.starts_with("≈40 s download"), "got: {hint}");
        assert!(hint.contains("compute"), "got: {hint}");
        let hint = format_time_hint(40 * 1024 * 1024 * 150, &light, 3);
        assert!(hint.starts_with("≈2 m 30 s"), "got: {hint}");
        // Heavy gets the saturation warning.
        let mut heavy = rw_ingest::ingest_profile::IngestProfile::full();
        heavy.heavy = true;
        let hint = format_time_hint(0, &heavy, 3);
        assert!(hint.contains("HEAVY"), "got: {hint}");
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

    /// LIVE end-to-end GFS lane validation — network + a real (subsetted)
    /// GFS download into a SCRATCH store (never the user's). Run manually:
    ///
    /// ```text
    /// cargo test -p app_ui -- --ignored gfs_live --nocapture
    /// ```
    ///
    /// Ingests two sounding-grade hours of the freshest plausible GFS
    /// cycle, then asserts the full app-side chain: the stored field loads
    /// with the 0.25° global dims, the map-drape InverseLut builds and
    /// resolves Stockholm (59.33 N 18.07 E — far outside HRRR coverage),
    /// and a native skew-T extracts there with physically plausible
    /// temperatures/pressures.
    #[test]
    #[ignore = "live network: downloads a GFS hour into a scratch store"]
    fn gfs_live_ingest_field_lut_and_stockholm_sounding() {
        let scratch = std::env::temp_dir().join(format!("bowecho-gfs-e2e-{}", std::process::id()));
        let store_root = scratch.join("store");
        let cache_root = scratch.join("cache");
        std::fs::create_dir_all(&store_root).expect("scratch store dir");
        std::fs::create_dir_all(&cache_root).expect("scratch cache dir");

        // --- Ingest: freshest plausible cycle via the one-click helpers ---
        let model = ModelId::Gfs;
        let now = chrono::Utc::now();
        let candidates = recent_cycle_candidates(
            now,
            rustwx_models::model_summary(model).cycle_hours_utc,
            publication_lag_minutes(model),
            2,
        );
        let profile = IngestProfile::sounding();
        let cancel = AtomicBool::new(false);
        let progress = |event: IngestEvent| {
            if let IngestEvent::StageStarted { hour, stage } = event {
                println!("gfs e2e: f{hour:02} {stage:?}…");
            }
        };
        let mut stored: Option<(String, u8, Vec<u16>)> = None;
        'candidates: for (date, cycle_hour) in candidates {
            let cycle = CycleSpec::new(&date, cycle_hour).expect("cycle spec");
            let run_slug = format!("{date}_{cycle_hour:02}z");
            let config = IngestConfig {
                model,
                cycle: &cycle,
                source_override: None,
                cache_root: &cache_root,
                use_cache: true,
                store_root: &store_root,
                model_slug: "gfs",
                run_slug: &run_slug,
                profile: &profile,
                verify: false,
                progress: &progress,
                cancel: &cancel,
            };
            let init = chrono::NaiveDate::parse_from_str(&date, "%Y%m%d")
                .expect("candidate date")
                .and_hms_opt(cycle_hour as u32, 0, 0)
                .expect("cycle time")
                .and_utc();
            let first = first_live_hour((now - init).num_minutes());
            let hours = vec![first, first + 1];
            for &hour in &hours {
                if let Err(err) = rw_ingest::ingest_hour_serial(&config, hour) {
                    println!("gfs e2e: {date} {cycle_hour:02}z f{hour:02} failed: {err}");
                    continue 'candidates;
                }
                println!("gfs e2e: {date} {cycle_hour:02}z f{hour:02} stored");
            }
            stored = Some((date, cycle_hour, hours));
            break;
        }
        let (date, cycle_hour, hours) =
            stored.expect("no plausible GFS cycle ingested — network down or NOMADS/AWS outage?");

        // --- Store read-back: the exact dock flow (rw-ui StoreWorker) ---
        let worker = rw_ui::StoreWorker::spawn(rw_ui::StoreView::new(&store_root), || {});
        let timeout = std::time::Duration::from_secs(60);
        let key = rw_ui::HourKey {
            model: "gfs".to_owned(),
            run: format!("{date}_{cycle_hour:02}z"),
            hour: hours[0],
        };
        worker.send(rw_ui::StoreRequest::LoadHour(key.clone()));
        let vars = loop {
            match worker.recv_timeout(timeout).expect("hour vars response") {
                rw_ui::StoreResponse::HourVars(got, result) if got == key => {
                    break result.expect("stored hour lists variables");
                }
                _ => {}
            }
        };
        let surface_var = vars
            .iter()
            .find(|var| var.kind == rw_ui::VarKind::Surface2D)
            .expect("sounding profile stores 2D surface vars")
            .name
            .clone();
        worker.send(rw_ui::StoreRequest::LoadField(rw_ui::FieldKey {
            hour: key.clone(),
            var: surface_var.clone(),
        }));
        let field = loop {
            if let rw_ui::StoreResponse::Field(_, boxed) =
                worker.recv_timeout(timeout).expect("field response")
            {
                break boxed.expect("field loads");
            }
        };
        println!(
            "gfs e2e: field {surface_var} nx={} ny={} ({} values, units {})",
            field.nx,
            field.ny,
            field.values.len(),
            field.units
        );
        // GFS 0.25°: 1440 x 721 global grid.
        assert_eq!((field.nx, field.ny), (1440, 721), "GFS 0.25° dims");
        assert_eq!(field.values.len(), field.nx * field.ny);
        let grid = field.grid.as_ref().expect(".rwg grid metadata present");

        // --- Map drape: the InverseLut the radar map layer uses ---
        let lut = crate::model_layer::InverseLut::build(&grid.lat, &grid.lon)
            .expect("InverseLut builds for the global grid");
        let (stockholm_lat, stockholm_lon) = (59.33_f32, 18.07_f32);
        let index = lut
            .lookup(stockholm_lat, stockholm_lon)
            .expect("Stockholm resolves in the LUT");
        println!(
            "gfs e2e: LUT Stockholm -> index {index} (grid point {:.2}N {:.2}E, field value {:.2} {})",
            grid.lat[index], grid.lon[index], field.values[index], field.units
        );
        assert!((grid.lat[index] - stockholm_lat).abs() < 0.5);
        assert!((grid.lon[index] - stockholm_lon).abs() < 0.5);

        // --- Native sounding at Stockholm (model-agnostic fx/fy path) ---
        let (fx, fy) = ((index % field.nx) as f64, (index / field.nx) as f64);
        worker.send(rw_ui::StoreRequest::LoadSounding {
            hour: key.clone(),
            fx,
            fy,
        });
        let sounding = loop {
            if let rw_ui::StoreResponse::Sounding(_, result) =
                worker.recv_timeout(timeout).expect("sounding response")
            {
                break result.expect("sounding extracts");
            }
        };
        println!(
            "gfs e2e: sounding at fx={fx} fy={fy} -> lat {:?} lon {:?}, {} profile vars, {} surface samples",
            sounding.lat,
            sounding.lon,
            sounding.vars.len(),
            sounding.surface.len()
        );
        let native = crate::build_native_sounding_adjusted(&sounding, None)
            .expect("sharprs-native sounding builds");
        let profile = &native.profile;
        assert!(
            profile.pres.len() >= 20,
            "expected a real column, got {} levels",
            profile.pres.len()
        );
        let sfc_p = profile.sfc_pressure();
        let sfc_t = profile.tmpc[0];
        println!(
            "gfs e2e: Stockholm profile — {} levels, sfc {sfc_p:.1} hPa / {sfc_t:.1} C, top {:.1} hPa / {:.1} C",
            profile.pres.len(),
            profile.pres[profile.pres.len() - 1],
            profile.tmpc[profile.tmpc.len() - 1]
        );
        for (p, t) in profile.pres.iter().zip(profile.tmpc.iter()).take(6) {
            println!("gfs e2e:   {p:.0} hPa  {t:.1} C");
        }
        // Stockholm is near sea level; June surface temps are mild.
        assert!(
            (950.0..=1050.0).contains(&sfc_p),
            "surface pressure {sfc_p} hPa implausible for Stockholm"
        );
        assert!(
            (-15.0..=40.0).contains(&sfc_t),
            "surface temperature {sfc_t} C implausible"
        );
        // Pressure strictly decreases with height; temps stay physical.
        assert!(
            profile.pres.windows(2).all(|w| w[1] < w[0]),
            "pressure column must descend"
        );
        assert!(
            profile
                .tmpc
                .iter()
                .filter(|t| t.is_finite())
                .all(|&t| (-120.0..=60.0).contains(&t)),
            "temperatures outside physical bounds"
        );
        // 0°C crossing height above the surface — the auto hail-env value.
        let sfc_h = profile.sfc_height();
        let h0 = profile
            .tmpc
            .iter()
            .zip(profile.hght.iter())
            .collect::<Vec<_>>()
            .windows(2)
            .find_map(|pair| {
                let (&t0, &h0) = pair[0];
                let (&t1, &h1) = pair[1];
                (t0 >= 0.0 && t1 <= 0.0 && t0 != t1)
                    .then(|| (h0 + (t0 / (t0 - t1)) * (h1 - h0) - sfc_h) / 1000.0)
            });
        println!("gfs e2e: 0C crossing {h0:?} km AGL");
        let h0 = h0.expect("June mid-latitude profile crosses 0C");
        assert!((0.1..=6.0).contains(&h0), "0C height {h0} km implausible");

        drop(worker);
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
