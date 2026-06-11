//! Background satellite worker: the bridge between the pure-widget
//! [`SatellitePanel`](rw_ui::SatellitePanel) / [`SatPlayerPanel`](rw_ui::SatPlayerPanel)
//! and rw-sat — mirroring rusty-weather-ui's IngestWorker. The only
//! crate that wires the panels to the satellite engine is this shell;
//! rw-ui stays free of rw-sat dependencies.
//!
//! One control thread serves cheap requests (spec validation, store scans,
//! frame reads + palette coloring); a follow session runs on its own
//! thread (`rw-sat-follow`) so playback frame loads stay responsive while
//! the engine polls the live bucket. Responses stream back as plain data
//! and every response fires the `notify` hook (`ctx.request_repaint`).
//! Cancellation bypasses the queue: [`SatWorker::stop_follow`] flips a
//! shared `AtomicBool` the follow engine observes at poll/frame
//! boundaries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Instant;

use eframe::egui::{Color32, ColorImage};
use rw_sat::composite::GoesAbiRgbCompositeStyle;
use rw_sat::events::{SatError, SatEvent};
use rw_sat::follow::FollowConfig;
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::palette::{anchor_color, band_anchors};
use rw_sat::s3::{Sector, bucket_for_satellite, object_filename};
use rw_sat::store::{frame_file_name, run_day};
use rw_sat::window::WindowConfig;
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_ui::{
    SatDiskUsage, SatFollowSpec, SatFrameImage, SatLayerOption, SatRunKey, SatRunListing,
    SatSatelliteOption, SatSectorOption, StoreView, format_bytes,
};

/// Requests from the UI thread.
#[derive(Debug, Clone)]
pub enum SatRequest {
    /// Validate a spec and build its one-line summary.
    Validate(SatFollowSpec),
    /// Enumerate the sat store's runs and frames.
    Scan,
    /// Start a live follow session (one at a time).
    Follow(SatFollowSpec),
    /// Read one stored frame and color it with its band palette.
    LoadFrame { key: SatRunKey, hhmm: u16 },
    /// Read a frame PLUS its run grid for the radar-map layer.
    LoadFrameForMap { key: SatRunKey, hhmm: u16 },
}

/// A frame prepared for the map layer: the palette-colored image, the
/// run's per-pixel lat/lon grid, and whether image rows were flipped
/// relative to grid storage order (sample image_row = ny-1-grid_row when
/// set).
#[allow(dead_code)] // key/hhmm identify the frame for future multi-layer use
pub struct SatMapFrame {
    pub key: SatRunKey,
    pub hhmm: u16,
    pub image: ColorImage,
    pub grid: std::sync::Arc<GridFile>,
    pub flip_rows: bool,
}

impl std::fmt::Debug for SatMapFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SatMapFrame({} t{:04}, {}x{})",
            self.key, self.hhmm, self.image.size[0], self.image.size[1]
        )
    }
}

/// Responses to the UI thread — all plain data, panel-ready.
#[derive(Debug)]
pub enum SatResponse {
    /// A map-layer frame (image + geolocation grid).
    MapFrame(Box<Result<SatMapFrame, String>>),
    SpecStatus(Result<String, String>),
    Runs(Vec<SatRunListing>),
    FollowStarted,
    /// The session ended: `Ok` = clean stop, `Err` = failure.
    FollowFinished(Result<String, String>),
    PollDone {
        band: u8,
        new_keys: usize,
        ms: u128,
    },
    DownloadStarted {
        id: String,
        label: String,
        bytes: u64,
    },
    DownloadDone {
        id: String,
        ms: u128,
        cache_hit: bool,
    },
    FrameWritten {
        id: String,
        run: String,
        hhmm: u16,
        bytes: u64,
        encode_ms: u64,
    },
    Evicted {
        frames: usize,
        bytes: u64,
    },
    Sleeping {
        ms: u64,
    },
    Note(String),
    DiskUsage(SatDiskUsage),
    Frame {
        key: SatRunKey,
        hhmm: u16,
        result: Box<Result<SatFrameImage, String>>,
    },
}

/// Handle to the satellite worker.
pub struct SatWorker {
    tx: Sender<SatRequest>,
    rx: Receiver<SatResponse>,
    cancel: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl SatWorker {
    /// Spawn the worker. `store_root` is the sat store root (frames land
    /// and are read from here); `notify` wakes the UI after every response.
    pub fn spawn(store_root: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = channel::<SatRequest>();
        let (resp_tx, resp_rx) = channel::<SatResponse>();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(notify);
        let thread = std::thread::Builder::new()
            .name("rw-sat-worker".to_string())
            .spawn(move || {
                rw_ingest::throttle::set_current_thread_background_priority();
                worker_loop(store_root, &req_rx, &resp_tx, &notify, &worker_cancel);
            })
            .expect("spawn sat worker thread");
        Self {
            tx: req_tx,
            rx: resp_rx,
            cancel,
            _thread: thread,
        }
    }

    /// Queue a request (dropped silently if the worker died).
    pub fn send(&self, request: SatRequest) {
        let _ = self.tx.send(request);
    }

    /// Non-blocking poll for the next response (drain once per frame).
    pub fn try_recv(&self) -> Option<SatResponse> {
        self.rx.try_recv().ok()
    }

    /// Request the running follow session to stop. Takes effect at the
    /// next poll/frame boundary (the in-flight download completes first);
    /// bypasses the request queue.
    pub fn stop_follow(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Every pickable satellite (open-data buckets exist for all of these).
pub fn satellite_options() -> Vec<SatSatelliteOption> {
    [
        ("goes19", "GOES-19 (East)"),
        ("goes18", "GOES-18 (West)"),
        ("goes16", "GOES-16"),
    ]
    .into_iter()
    .map(|(slug, label)| SatSatelliteOption {
        slug: slug.to_string(),
        label: label.to_string(),
    })
    .collect()
}

/// Every pickable sector, with the live timing the panel displays.
pub fn sector_options() -> Vec<SatSectorOption> {
    [
        Sector::Conus,
        Sector::FullDisk,
        Sector::Meso1,
        Sector::Meso2,
    ]
    .into_iter()
    .map(|sector| SatSectorOption {
        slug: sector.slug().to_string(),
        label: match sector {
            Sector::Conus => "CONUS".to_string(),
            Sector::FullDisk => "Full disk".to_string(),
            Sector::Meso1 => "Meso 1".to_string(),
            Sector::Meso2 => "Meso 2".to_string(),
        },
        default_poll_secs: sector.default_poll_secs(),
        cadence_secs: sector.cadence_secs(),
    })
    .collect()
}

/// ABI band display names (UI copy; the science lives in rw-sat).
const BAND_NAMES: [&str; 16] = [
    "Blue 0.47 µm",
    "Red 0.64 µm",
    "Veggie 0.86 µm",
    "Cirrus 1.37 µm",
    "Snow/Ice 1.6 µm",
    "Cloud Particle Size 2.2 µm",
    "Shortwave Window 3.9 µm",
    "Upper-Level Water Vapor 6.2 µm",
    "Mid-Level Water Vapor 6.9 µm",
    "Lower-Level Water Vapor 7.3 µm",
    "Cloud-Top Phase 8.4 µm",
    "Ozone 9.6 µm",
    "Clean IR Window 10.3 µm",
    "IR Longwave 11.2 µm",
    "Dirty IR Window 12.3 µm",
    "CO2 Longwave 13.3 µm",
];

/// Layer picker entries: every ABI band, then every RGB composite (a
/// composite follow ingests its required bands; each band run plays in
/// the frame player).
pub fn layer_options() -> Vec<SatLayerOption> {
    let mut options: Vec<SatLayerOption> = (1u8..=16)
        .map(|band| SatLayerOption {
            slug: format!("c{band:02}"),
            label: format!("C{band:02} · {}", BAND_NAMES[usize::from(band - 1)]),
            note: String::new(),
        })
        .collect();
    for style in GoesAbiRgbCompositeStyle::ALL {
        let bands = style
            .required_channels()
            .iter()
            .map(|band| format!("C{band:02}"))
            .collect::<Vec<_>>()
            .join("+");
        options.push(SatLayerOption {
            slug: style.slug().to_string(),
            label: format!("RGB · {}", style.title()),
            note: format!("follows {bands}; each band run plays in the player"),
        });
    }
    options
}

/// Layer slug -> the ABI bands it follows, plus a description for the
/// summary line. Bands: "c13"; composites by slug ("geocolor").
fn resolve_layer(layer: &str) -> Result<(Vec<u8>, String), String> {
    let normalized = layer.trim().to_ascii_lowercase();
    if let Some(band) = normalized
        .strip_prefix('c')
        .and_then(|raw| raw.parse::<u8>().ok())
    {
        if (1..=16).contains(&band) {
            return Ok((vec![band], format!("C{band:02}")));
        }
        return Err(format!("ABI band out of range: C{band:02} (1-16)"));
    }
    if let Some(style) = GoesAbiRgbCompositeStyle::parse(&normalized) {
        let bands = style.required_channels().to_vec();
        let list = bands
            .iter()
            .map(|band| format!("C{band:02}"))
            .collect::<Vec<_>>()
            .join("+");
        return Ok((bands, format!("{} [{list}]", style.title())));
    }
    Err(format!("unknown layer '{layer}'"))
}

/// Validated pieces of a follow spec.
struct ResolvedSpec {
    /// rw-store model dir ("g19").
    model: String,
    sector: Sector,
    bands: Vec<u8>,
    layer_desc: String,
}

fn resolve_spec(spec: &SatFollowSpec) -> Result<ResolvedSpec, String> {
    bucket_for_satellite(&spec.satellite).map_err(|err| err.to_string())?;
    let sector =
        Sector::parse(&spec.sector).ok_or_else(|| format!("unknown sector '{}'", spec.sector))?;
    let (bands, layer_desc) = resolve_layer(&spec.layer)?;
    if ![1usize, 2, 4].contains(&spec.downsample) {
        return Err(format!("unsupported detail stride {}", spec.downsample));
    }
    Ok(ResolvedSpec {
        model: GoesSatellite::parse(&spec.satellite)
            .as_str()
            .to_ascii_lowercase(),
        sector,
        bands,
        layer_desc,
    })
}

/// One-line spec summary for the panel ("the interval display").
fn spec_summary(spec: &SatFollowSpec) -> Result<String, String> {
    let resolved = resolve_spec(spec)?;
    let interval = spec
        .interval()
        .unwrap_or_else(|| resolved.sector.default_poll_secs());
    let window = match (spec.max_age_minutes(), spec.max_bytes()) {
        (Some(minutes), Some(bytes)) => format!(
            "keep {:.1} h / {} per band",
            f64::from(minutes) / 60.0,
            format_bytes(bytes)
        ),
        (Some(minutes), None) => format!("keep {:.1} h", f64::from(minutes) / 60.0),
        (None, Some(bytes)) => format!("keep {} per band", format_bytes(bytes)),
        (None, None) => "unbounded window".to_string(),
    };
    let detail = match spec.downsample {
        1 => String::new(),
        step => format!(" · 1/{step} res"),
    };
    Ok(format!(
        "{} {} · {} · poll ~{interval} s (frames ~{} s apart) · {window}{detail}",
        resolved.model,
        resolved.sector.slug(),
        resolved.layer_desc,
        resolved.sector.cadence_secs(),
    ))
}

/// Spec -> rw-sat follow config (no limits: the session runs until
/// stopped).
fn follow_config(spec: &SatFollowSpec, store_root: &Path) -> Result<FollowConfig, String> {
    let resolved = resolve_spec(spec)?;
    let mut config = FollowConfig::new(&spec.satellite, resolved.sector, resolved.bands);
    config.store_root = store_root.to_path_buf();
    // Relative cache dirs resolve inside the sealed read-only bundle on
    // macOS (field report: os error 30 on sat downloads) — force them
    // under the store root.
    config.cache_dir = {
        let p = PathBuf::from(&spec.cache_dir);
        if p.is_absolute() {
            p
        } else {
            store_root.join("cache")
        }
    };
    config.poll_interval = spec.interval().map(std::time::Duration::from_secs);
    config.downsample = spec.downsample;
    config.window = WindowConfig {
        max_age_minutes: spec.max_age_minutes(),
        max_bytes: spec.max_bytes(),
    };
    Ok(config)
}

/// The run-dir prefixes a spec's eviction/usage scans cover
/// (`conus_c13`, one per followed band).
fn run_prefixes(spec: &SatFollowSpec) -> Result<(String, Vec<String>), String> {
    let resolved = resolve_spec(spec)?;
    let prefixes = resolved
        .bands
        .iter()
        .map(|band| format!("{}_c{band:02}", resolved.sector.slug()))
        .collect();
    Ok((resolved.model, prefixes))
}

/// Live on-disk footprint of the followed band(s): frame files only (the
/// same accounting the rolling window budgets).
fn disk_usage(store_root: &Path, model: &str, prefixes: &[String]) -> SatDiskUsage {
    let mut usage = SatDiskUsage {
        bytes: 0,
        frames: 0,
    };
    let model_dir = store_root.join(model);
    let Ok(runs) = std::fs::read_dir(&model_dir) else {
        return usage;
    };
    for run in runs.flatten() {
        let name = run.file_name().to_string_lossy().to_string();
        if !prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix.as_str()))
        {
            continue;
        }
        let Ok(files) = std::fs::read_dir(run.path()) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().to_string();
            if file_name.starts_with('t')
                && file_name.ends_with(".rws")
                && let Ok(meta) = file.metadata()
            {
                {
                    usage.bytes += meta.len();
                    usage.frames += 1;
                }
            }
        }
    }
    usage
}

/// Title for one sat run: `g19 · conus C13 · 2026-06-10` (with the
/// `_2` grid-move suffix kept visible).
fn run_title(model: &str, run: &str) -> String {
    let mut tokens = run.split('_');
    let sector = tokens.next().unwrap_or(run);
    let band = tokens
        .next()
        .and_then(|token| token.strip_prefix('c'))
        .and_then(|raw| raw.parse::<u8>().ok());
    let day = run_day(run)
        .map(|day| day.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    let suffix = run
        .rsplit('_')
        .next()
        .filter(|token| token.len() < 8 && token.chars().all(|ch| ch.is_ascii_digit()))
        .map(|token| format!(" (grid {token})"))
        .unwrap_or_default();
    match band {
        Some(band) => format!("{model} · {sector} C{band:02} · {day}{suffix}"),
        None => format!("{model} · {run}"),
    }
}

/// Enumerate the sat store into player-ready run listings, newest run
/// first.
fn scan_runs(store_root: &Path) -> Vec<SatRunListing> {
    let tree = StoreView::new(store_root).enumerate();
    let mut listings = Vec::new();
    for model in &tree.models {
        for run in &model.runs {
            listings.push(SatRunListing {
                key: SatRunKey {
                    model: model.model.clone(),
                    run: run.run.clone(),
                },
                title: run_title(&model.model, &run.run),
                nx: run.nx,
                ny: run.ny,
                frames: run.hours.iter().map(|hour| hour.hour).collect(),
            });
        }
    }
    listings.sort_by(|a, b| {
        b.key
            .run
            .cmp(&a.key.run)
            .then_with(|| a.key.model.cmp(&b.key.model))
    });
    listings
}

/// Per-run grid facts the frame loader caches (one `grid.rwg` read per
/// run instead of per frame).
struct GridInfo {
    hash: String,
    /// Stored row 0 is the southernmost row -> flip for display. GOES
    /// grids store north first, so this is normally `false` — but it is
    /// DERIVED from the grid, never assumed.
    flip_rows: bool,
}

#[derive(Default)]
struct WorkerState {
    grids: HashMap<(String, String), GridInfo>,
}

/// Frame + run grid for the radar-map layer (one GridFile open per call;
/// the layer rebuild is a user action, not a per-frame hot path).
fn load_frame_for_map(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<SatMapFrame, String> {
    let frame = load_frame(state, store_root, key, hhmm)?;
    let run_dir = store_root.join(&key.model).join(&key.run);
    let grid = GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| err.to_string())?;
    let flip_rows = state
        .grids
        .get(&(key.model.clone(), key.run.clone()))
        .map(|info| info.flip_rows)
        .unwrap_or(false);
    Ok(SatMapFrame {
        key: key.clone(),
        hhmm,
        image: frame.image,
        grid: std::sync::Arc::new(grid),
        flip_rows,
    })
}

/// Read one stored frame and color it with its band's production palette
/// (NaN off-earth pixels stay transparent).
fn load_frame(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<SatFrameImage, String> {
    let started = Instant::now();
    let run_dir = store_root.join(&key.model).join(&key.run);
    let reader =
        HourReader::open(&run_dir.join(frame_file_name(hhmm))).map_err(|err| err.to_string())?;
    let meta = reader.meta();
    let variable = meta
        .variables
        .iter()
        .find(|var| var.kind == "surface2d")
        .ok_or_else(|| format!("{key}/t{hhmm:04} holds no 2D variable"))?;
    let band = variable.selector["goes"]["band"]
        .as_u64()
        .and_then(|value| u8::try_from(value).ok())
        .or_else(|| {
            variable
                .name
                .strip_prefix("cmi_c")
                .and_then(|raw| raw.parse::<u8>().ok())
        })
        .ok_or_else(|| format!("{key}/t{hhmm:04} selector carries no band"))?;

    let grid_key = (key.model.clone(), key.run.clone());
    if !state.grids.contains_key(&grid_key) {
        let grid = GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| err.to_string())?;
        state.grids.insert(
            grid_key.clone(),
            GridInfo {
                hash: grid.hash.clone(),
                flip_rows: grid.lat_descending() == Some(false),
            },
        );
    }
    let grid = &state.grids[&grid_key];
    if grid.hash != meta.grid_hash {
        return Err(format!(
            "{key}/t{hhmm:04}: frame grid hash {} does not match the run grid {}",
            meta.grid_hash, grid.hash
        ));
    }

    let (nx, ny) = (meta.nx, meta.ny);
    let name = variable.name.clone();
    let values = reader.read_full_2d(&name).map_err(|err| err.to_string())?;
    let anchors = band_anchors(band);
    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if grid.flip_rows {
            ny - 1 - image_row
        } else {
            image_row
        };
        for &value in &values[grid_row * nx..(grid_row + 1) * nx] {
            let [r, g, b, a] = anchor_color(value, anchors);
            pixels.push(Color32::from_rgba_unmultiplied(r, g, b, a));
        }
    }
    Ok(SatFrameImage {
        key: key.clone(),
        hhmm,
        image: ColorImage::new([nx, ny], pixels),
        read_ms: started.elapsed().as_secs_f32() * 1000.0,
    })
}

/// Map one follow-engine event into panel-ready responses. `current_key`
/// stitches the strictly sequential download → frame-written pair so the
/// frame row keeps one id end to end.
fn map_event(event: SatEvent, current_key: &mut Option<String>) -> Vec<SatResponse> {
    match event {
        SatEvent::PollStarted { .. } => Vec::new(),
        SatEvent::PollDone { band, new_keys, ms } => {
            vec![SatResponse::PollDone { band, new_keys, ms }]
        }
        SatEvent::DownloadStarted { key, bytes } => {
            *current_key = Some(key.clone());
            let label = download_label(&key);
            vec![SatResponse::DownloadStarted {
                id: key,
                label,
                bytes,
            }]
        }
        SatEvent::DownloadDone {
            key, ms, cache_hit, ..
        } => vec![SatResponse::DownloadDone {
            id: key,
            ms,
            cache_hit,
        }],
        SatEvent::FrameWritten {
            run,
            hhmm,
            bytes,
            encode_ms,
            ..
        } => vec![SatResponse::FrameWritten {
            id: current_key.take().unwrap_or_default(),
            run,
            hhmm,
            bytes,
            encode_ms,
        }],
        SatEvent::Evicted { frames, bytes, .. } => vec![SatResponse::Evicted { frames, bytes }],
        SatEvent::Sleeping { ms } => vec![SatResponse::Sleeping { ms }],
        SatEvent::Info { message } => vec![SatResponse::Note(message)],
        SatEvent::Warning { message } => vec![SatResponse::Note(format!("warning: {message}"))],
    }
}

/// Row label for one S3 object ("C13 19:21:18Z"), falling back to the
/// file name for unparseable keys.
fn download_label(key: &str) -> String {
    match parse_goes_abi_filename(object_filename(key)) {
        Ok(parsed) => {
            let band = parsed
                .channel
                .map(|band| format!("C{band:02}"))
                .unwrap_or_else(|| parsed.product.clone());
            format!("{band} {}", parsed.start_time_utc.format("%H:%M:%SZ"))
        }
        Err(_) => object_filename(key).to_string(),
    }
}

fn worker_loop(
    store_root: PathBuf,
    requests: &Receiver<SatRequest>,
    responses: &Sender<SatResponse>,
    notify: &Arc<dyn Fn() + Send + Sync>,
    cancel: &Arc<AtomicBool>,
) {
    let mut state = WorkerState::default();
    let follow_active = Arc::new(AtomicBool::new(false));
    let send = |response: SatResponse| {
        let ok = responses.send(response).is_ok();
        notify();
        ok
    };
    while let Ok(request) = requests.recv() {
        match request {
            SatRequest::Validate(spec) => {
                if !send(SatResponse::SpecStatus(spec_summary(&spec))) {
                    return;
                }
            }
            SatRequest::Scan => {
                if !send(SatResponse::Runs(scan_runs(&store_root))) {
                    return;
                }
            }
            SatRequest::LoadFrame { key, hhmm } => {
                let result = load_frame(&mut state, &store_root, &key, hhmm);
                if !send(SatResponse::Frame {
                    key,
                    hhmm,
                    result: Box::new(result),
                }) {
                    return;
                }
            }
            SatRequest::LoadFrameForMap { key, hhmm } => {
                let result = load_frame_for_map(&mut state, &store_root, &key, hhmm);
                if !send(SatResponse::MapFrame(Box::new(result))) {
                    return;
                }
            }
            SatRequest::Follow(spec) => {
                if follow_active.swap(true, Ordering::SeqCst) {
                    send(SatResponse::Note(
                        "a follow session is already running".to_string(),
                    ));
                    continue;
                }
                let config = match follow_config(&spec, &store_root) {
                    Ok(config) => config,
                    Err(message) => {
                        follow_active.store(false, Ordering::SeqCst);
                        send(SatResponse::FollowFinished(Err(message)));
                        continue;
                    }
                };
                let (model, prefixes) =
                    run_prefixes(&spec).expect("spec validated by follow_config");
                cancel.store(false, Ordering::Relaxed);
                if !send(SatResponse::FollowStarted) {
                    return;
                }
                send(SatResponse::DiskUsage(disk_usage(
                    &store_root,
                    &model,
                    &prefixes,
                )));

                let tx = responses.clone();
                let thread_notify = Arc::clone(notify);
                let thread_cancel = Arc::clone(cancel);
                let active = Arc::clone(&follow_active);
                let root = store_root.clone();
                let spawned = std::thread::Builder::new()
                    .name("rw-sat-follow".to_string())
                    .spawn(move || {
                        rw_ingest::throttle::set_current_thread_background_priority();
                        let mut current_key: Option<String> = None;
                        let mut sink = |event: SatEvent| {
                            let usage_due = matches!(
                                event,
                                SatEvent::FrameWritten { .. } | SatEvent::Evicted { .. }
                            );
                            for response in map_event(event, &mut current_key) {
                                let _ = tx.send(response);
                            }
                            if usage_due {
                                let _ = tx.send(SatResponse::DiskUsage(disk_usage(
                                    &root, &model, &prefixes,
                                )));
                            }
                            thread_notify();
                        };
                        let result = rw_sat::follow(&config, &mut sink, &thread_cancel);
                        active.store(false, Ordering::SeqCst);
                        let response = match result {
                            Ok(summary) => SatResponse::FollowFinished(Ok(format!(
                                "done — {} frame(s) in {} poll(s)",
                                summary.frames.len(),
                                summary.polls
                            ))),
                            Err(SatError::Cancelled) => SatResponse::FollowFinished(Ok(
                                "stopped — the rolling window stays on disk".to_string(),
                            )),
                            Err(err) => SatResponse::FollowFinished(Err(err.to_string())),
                        };
                        let _ = tx.send(response);
                        thread_notify();
                    });
                if let Err(err) = spawned {
                    follow_active.store(false, Ordering::SeqCst);
                    send(SatResponse::FollowFinished(Err(format!(
                        "failed to spawn the follow thread: {err}"
                    ))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rw_sat::abi::{AbiFixedGrid, AbiSector, GoesAbiField, GoesAbiScene, GoesImagerProjection};
    use rw_sat::geostationary::SweepAngleAxis;
    use rw_sat::store::write_band_frame;

    fn spec() -> SatFollowSpec {
        SatFollowSpec::default()
    }

    #[test]
    fn layer_resolution_handles_bands_and_composites() {
        let (bands, desc) = resolve_layer("c13").expect("band layer");
        assert_eq!(bands, vec![13]);
        assert_eq!(desc, "C13");

        let (bands, desc) = resolve_layer("geocolor").expect("composite layer");
        assert_eq!(bands, vec![1, 2, 3]);
        assert!(
            desc.contains("GeoColor") && desc.contains("C01+C02+C03"),
            "got: {desc}"
        );

        assert!(resolve_layer("c0").is_err());
        assert!(resolve_layer("c17").is_err());
        assert!(resolve_layer("bogus").is_err());
    }

    #[test]
    fn spec_summary_describes_the_session() {
        let summary = spec_summary(&spec()).expect("default spec is valid");
        assert!(summary.contains("g19"), "got: {summary}");
        assert!(summary.contains("conus"), "got: {summary}");
        assert!(summary.contains("C13"), "got: {summary}");
        assert!(summary.contains("poll ~30 s"), "got: {summary}");
        assert!(summary.contains("keep 6.0 h"), "got: {summary}");

        let mut bad = spec();
        bad.sector = "antarctica".to_string();
        assert!(spec_summary(&bad).is_err());
        let mut bad = spec();
        bad.satellite = "himawari".to_string();
        assert!(spec_summary(&bad).is_err());
    }

    #[test]
    fn follow_config_maps_the_window_and_interval() {
        let mut spec = spec();
        spec.auto_interval = false;
        spec.interval_secs = 45;
        spec.layer = "geocolor".to_string();
        spec.downsample = 2;
        let config = follow_config(&spec, Path::new("sat-root")).expect("valid spec");
        assert_eq!(config.bands, vec![1, 2, 3]);
        assert_eq!(config.sector, Sector::Conus);
        assert_eq!(
            config.poll_interval,
            Some(std::time::Duration::from_secs(45))
        );
        assert_eq!(config.downsample, 2);
        assert_eq!(config.window.max_age_minutes, Some(360));
        assert_eq!(config.window.max_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(config.store_root, PathBuf::from("sat-root"));
        assert_eq!(config.max_polls, None, "UI sessions run until stopped");

        let (model, prefixes) = run_prefixes(&spec).unwrap();
        assert_eq!(model, "g19");
        assert_eq!(prefixes, vec!["conus_c01", "conus_c02", "conus_c03"]);
    }

    #[test]
    fn layer_options_cover_all_bands_and_composites() {
        let options = layer_options();
        assert_eq!(options.len(), 16 + GoesAbiRgbCompositeStyle::ALL.len());
        for option in &options {
            resolve_layer(&option.slug).expect("every picker entry resolves");
        }
        assert!(options[12].label.contains("Clean IR"), "C13 label");
    }

    /// Small synthetic CONUS-ish scene near the sub-satellite point (same
    /// shape as rw-sat's internal test support, which is not exported).
    fn synthetic_field(nx: usize, ny: usize, hour: u32, minute: u32, band: u8) -> GoesAbiField {
        let x_scan_rad: Vec<f64> = (0..nx)
            .map(|i| -0.02 + 0.04 * i as f64 / (nx.max(2) - 1) as f64)
            .collect();
        let y_scan_rad: Vec<f64> = (0..ny)
            .map(|j| 0.05 - 0.03 * j as f64 / (ny.max(2) - 1) as f64)
            .collect();
        let start = Utc.with_ymd_and_hms(2026, 6, 10, hour, minute, 18).unwrap();
        let scene = GoesAbiScene {
            path: PathBuf::from("synthetic.nc"),
            product: "ABI-L2-CMIPC".to_string(),
            sector: AbiSector::Conus,
            channel: Some(band),
            satellite: GoesSatellite::G19,
            start_time_utc: start,
            end_time_utc: start + chrono::Duration::seconds(150),
            projection: GoesImagerProjection {
                perspective_point_height_m: 35_786_023.0,
                semi_major_axis_m: 6_378_137.0,
                semi_minor_axis_m: 6_356_752.314_14,
                longitude_of_projection_origin_deg: -75.0,
                sweep_angle_axis: SweepAngleAxis::X,
            },
            fixed_grid: AbiFixedGrid {
                nx,
                ny,
                x_scan_rad,
                y_scan_rad,
            },
        };
        let mut values: Vec<f32> = (0..nx * ny).map(|i| 200.0 + (i % 97) as f32).collect();
        values[0] = f32::NAN; // an off-earth-ish pixel
        GoesAbiField {
            scene,
            variable_name: "CMI".to_string(),
            units: Some("K".to_string()),
            values,
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-sat-worker-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_lists_runs_newest_first_with_titles() {
        let dir = test_dir("scan");
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 13), 1).unwrap();
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 56, 13), 2).unwrap();
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 8), 3).unwrap();

        let runs = scan_runs(&dir);
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].key.run, "conus_c13_20260610",
            "c13 sorts after c08 -> newest-first puts it first"
        );
        assert_eq!(runs[0].frames, vec![1851, 1856]);
        assert_eq!(runs[0].title, "g19 · conus C13 · 2026-06-10");
        assert_eq!((runs[0].nx, runs[0].ny), (8, 6));
        assert_eq!(runs[1].key.run, "conus_c08_20260610");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_frame_colors_with_the_band_palette() {
        let dir = test_dir("load");
        let field = synthetic_field(8, 6, 18, 51, 13);
        let written = write_band_frame(&dir, &field, 1).unwrap();
        let key = SatRunKey {
            model: written.model.clone(),
            run: written.run.clone(),
        };
        let mut state = WorkerState::default();
        let frame = load_frame(&mut state, &dir, &key, 1851).expect("frame loads");
        assert_eq!(frame.hhmm, 1851);
        assert_eq!(frame.image.size, [8, 6]);
        // The synthetic grid stores north first (y scan angles descend), so
        // rows are NOT flipped: pixel 0 is the NaN we planted -> transparent.
        assert_eq!(frame.image.pixels[0].a(), 0, "NaN renders transparent");
        // A 200 K pixel on the clean-IR ramp is bright and opaque.
        let bright = frame.image.pixels[1];
        assert_eq!(bright.a(), 255);
        assert!(bright.r() > 200, "cold pixel is bright: {bright:?}");
        assert_eq!(state.grids.len(), 1, "grid facts cached per run");

        // Second frame of the same run reuses the cached grid info.
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 56, 13), 2).unwrap();
        load_frame(&mut state, &dir, &key, 1856).expect("second frame loads");
        assert_eq!(state.grids.len(), 1);

        let missing = load_frame(&mut state, &dir, &key, 1900);
        assert!(missing.is_err(), "absent frame surfaces an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_usage_counts_only_matching_band_frames() {
        let dir = test_dir("usage");
        let one = write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 13), 1).unwrap();
        let two = write_band_frame(&dir, &synthetic_field(8, 6, 18, 56, 13), 2).unwrap();
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 8), 3).unwrap();

        let usage = disk_usage(&dir, "g19", &["conus_c13".to_string()]);
        assert_eq!(usage.frames, 2);
        assert_eq!(
            usage.bytes,
            one.bytes + two.bytes,
            "grid.rwg/run.json not counted"
        );

        let none = disk_usage(&dir, "g19", &["meso1_c02".to_string()]);
        assert_eq!(
            none,
            SatDiskUsage {
                bytes: 0,
                frames: 0
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_labels_parse_band_and_scan_time() {
        let label = download_label(
            "ABI-L2-CMIPC/2026/161/19/OR_ABI-L2-CMIPC-M6C13_G19_s20261611921186_e20261611923571_c20261611924043.nc",
        );
        assert_eq!(label, "C13 19:21:18Z");
        assert_eq!(download_label("not/a/goes-key.nc"), "goes-key.nc");
    }

    #[test]
    fn event_mapping_stitches_download_and_frame_ids() {
        let mut current = None;
        let key = "ABI-L2-CMIPC/2026/161/19/OR_ABI-L2-CMIPC-M6C13_G19_s20261611921186_e20261611923571_c20261611924043.nc".to_string();
        let started = map_event(
            SatEvent::DownloadStarted {
                key: key.clone(),
                bytes: 42,
            },
            &mut current,
        );
        assert_eq!(started.len(), 1);
        assert!(
            matches!(&started[0], SatResponse::DownloadStarted { id, label, bytes: 42 }
            if id == &key && label == "C13 19:21:18Z")
        );

        let written = map_event(
            SatEvent::FrameWritten {
                model: "g19".to_string(),
                run: "conus_c13_20260610".to_string(),
                hhmm: 1921,
                scan_time_utc: Utc.with_ymd_and_hms(2026, 6, 10, 19, 21, 18).unwrap(),
                path: PathBuf::from("t1921.rws"),
                bytes: 8_431_077,
                encode_ms: 950,
            },
            &mut current,
        );
        assert!(
            matches!(&written[0], SatResponse::FrameWritten { id, hhmm: 1921, .. } if id == &key)
        );
        assert!(current.is_none(), "id consumed by the frame");
    }

    /// A Follow over an invalid spec responds FollowFinished(Err) without
    /// spawning a session.
    #[test]
    fn follow_with_invalid_spec_fails_cleanly() {
        let worker = SatWorker::spawn(PathBuf::from("missing-sat-store"), || {});
        let mut bad = spec();
        bad.layer = "c99".to_string();
        worker.send(SatRequest::Follow(bad));
        let response = worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("worker responds");
        match response {
            SatResponse::FollowFinished(Err(message)) => {
                assert!(message.contains("ABI band out of range"), "got: {message}");
            }
            other => panic!("expected FollowFinished(Err), got {other:?}"),
        }
    }

    /// Validate and Scan round-trip through the worker thread.
    #[test]
    fn validate_and_scan_round_trip_through_the_worker() {
        let dir = test_dir("worker-roundtrip");
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 13), 1).unwrap();
        let worker = SatWorker::spawn(dir.clone(), || {});
        worker.send(SatRequest::Validate(spec()));
        worker.send(SatRequest::Scan);
        match worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("validate responds")
        {
            SatResponse::SpecStatus(Ok(summary)) => assert!(summary.contains("C13")),
            other => panic!("expected SpecStatus, got {other:?}"),
        }
        match worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("scan responds")
        {
            SatResponse::Runs(runs) => assert_eq!(runs.len(), 1),
            other => panic!("expected Runs, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
