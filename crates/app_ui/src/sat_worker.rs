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
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};
use eframe::egui::{Color32, ColorImage};
use rustwx_core::{GridShape, LatLonGrid};
use rw_sat::abi::{GoesAbiField, GoesAbiScene, read_goes_abi_field};
use rw_sat::composite::{GoesAbiRgbCompositeStyle, compose_rgb_pixels, values_on_base_grid};
use rw_sat::events::{SatError, SatEvent};
use rw_sat::follow::FollowConfig;
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::himawari::{
    HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA, HimawariDownloadManifest, HimawariLatestRequest,
    HimawariManifestSegment, HimawariProduct, HimawariSatellite, HimawariValueMode,
    assemble_hsd_segments, is_complete_segment_set, list_latest_segments, stage_download_manifest,
};
use rw_sat::palette::{anchor_color, band_anchors};
use rw_sat::s3::{
    S3Object, Sector, abi_filename_product_matches_request, band_hour_prefix, bucket_for_satellite,
    build_agent, download_object, list_s3_objects, object_filename, object_url,
};
use rw_sat::store::{
    SatelliteGridField, SatelliteGridScene, SatelliteProjection, WrittenFrame, downsample_field,
    frame_file_name, run_day, sector_slug, selector_band,
};
use rw_sat::window::WindowConfig;
use rw_store::format::RwsWriterInfo;
use rw_store::grid::{GridFile, write_grid};
use rw_store::lock::RunLock;
use rw_store::reader::HourReader;
use rw_store::run::{RwsHourEntry, RwsRunManifest};
use rw_store::writer::HourWriter;
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
    /// One-shot current-hour ingest for quickly creating a playable loop.
    LoadLoop(SatFollowSpec),
    /// Read one stored frame and color it with its band palette.
    LoadFrame { key: SatRunKey, hhmm: u16 },
    /// Read a frame PLUS its run grid for the radar-map layer.
    LoadFrameForMap { key: SatRunKey, hhmm: u16 },
    /// Download/decode the latest Himawari AHI frame into the shared sat store.
    IngestLatestHimawari(HimawariQuickSpec),
    /// Fetch the ABI bands a composite needs (co-registered by scan time),
    /// compose a true/natural-color RGB, and write it as one composite frame.
    IngestLatestGoesComposite(GoesCompositeSpec),
}

/// One-shot GOES ABI RGB-composite ingest request (Track D true-color path).
/// The composite's required bands are derived from `style`; every band of the
/// latest scan that has ALL of them is fetched, co-registered onto the base
/// channel's fixed grid, and composed per-pixel through rw-sat's
/// [`compose_rgb_pixels`].
#[derive(Debug, Clone)]
pub struct GoesCompositeSpec {
    /// Satellite slug (`goes19`, `goes18`, `goes16`).
    pub satellite: String,
    /// Sector slug (`conus`, `fulldisk`, `meso1`, `meso2`).
    pub sector: String,
    /// Composite style slug (`natural_color`, `geocolor`, ...).
    pub style: String,
    /// Per-band decimation stride applied on ingest (keeps hi-res C02 sane).
    pub downsample: usize,
    /// How far back to scan hour prefixes for the latest all-band scan.
    pub lookback_minutes: i64,
}

impl Default for GoesCompositeSpec {
    fn default() -> Self {
        Self {
            satellite: "goes19".to_string(),
            sector: "conus".to_string(),
            style: "natural_color".to_string(),
            downsample: 4,
            lookback_minutes: 180,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HimawariQuickSpec {
    pub satellite: String,
    pub band: u8,
    pub lookback_minutes: i64,
    pub segment_limit: usize,
    pub downsample: usize,
}

impl Default for HimawariQuickSpec {
    fn default() -> Self {
        Self {
            satellite: "h9".to_string(),
            band: 13,
            lookback_minutes: 180,
            segment_limit: usize::MAX,
            downsample: 2,
        }
    }
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
    SelectFrame {
        key: SatRunKey,
        hhmm: u16,
    },
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

/// The RGB composite styles offered by the one-shot true-color ingest, as
/// `(slug, label)` for a UI picker. Every `GoesAbiRgbCompositeStyle` is
/// ingestable (its required bands are fetched co-registered on demand), so
/// this mirrors [`GoesAbiRgbCompositeStyle::ALL`] with the daytime
/// natural-color recipe first.
pub fn goes_composite_style_options() -> Vec<(String, String)> {
    let mut styles = vec![GoesAbiRgbCompositeStyle::NaturalColor];
    for style in GoesAbiRgbCompositeStyle::ALL {
        if style != GoesAbiRgbCompositeStyle::NaturalColor {
            styles.push(style);
        }
    }
    styles
        .into_iter()
        .map(|style| {
            let bands = style
                .required_channels()
                .iter()
                .map(|band| format!("C{band:02}"))
                .collect::<Vec<_>>()
                .join("+");
            (
                style.slug().to_string(),
                format!("{} · {bands}", style.title()),
            )
        })
        .collect()
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

pub fn run_filters_for_spec(spec: &SatFollowSpec) -> Result<(String, Vec<String>), String> {
    run_prefixes(spec)
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
    // Composite RGB runs are `<sector>_rgb_<style>_<YYYYMMDD>[_<k>]`.
    if run.contains("_rgb_") {
        let day = run_day(run)
            .map(|day| day.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        for style in GoesAbiRgbCompositeStyle::ALL {
            let marker = format!("_rgb_{}_", style.slug());
            if let Some(pos) = run.find(&marker) {
                let sector = &run[..pos];
                return format!("{model} · {sector} {} · {day}", style.title());
            }
        }
        return format!("{model} · {run}");
    }
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

struct ColoredSatFrame {
    frame: SatFrameImage,
}

/// Frame + run grid for the radar-map layer (one GridFile open per call;
/// the layer rebuild is a user action, not a per-frame hot path).
fn load_frame_for_map(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<SatMapFrame, String> {
    let colored = load_colored_frame(state, store_root, key, hhmm, true)?;
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
        image: colored.frame.image,
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
    load_colored_frame(state, store_root, key, hhmm, false).map(|colored| colored.frame)
}

fn load_colored_frame(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
    map_overlay: bool,
) -> Result<ColoredSatFrame, String> {
    let started = Instant::now();
    let run_dir = store_root.join(&key.model).join(&key.run);
    let reader =
        HourReader::open(&run_dir.join(frame_file_name(hhmm))).map_err(|err| err.to_string())?;
    let meta = reader.meta();

    // Grid facts (cached per run) plus the frame/run grid-hash agreement
    // check — required for both the single-band and composite render paths.
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
    let flip_rows = grid.flip_rows;
    let (nx, ny) = (meta.nx, meta.ny);

    // Composite (true/natural-color) frames carry three baked RGB planes
    // instead of a single band; render them straight to Color32.
    let is_composite = meta.variables.iter().any(|var| var.name == COMPOSITE_R_VAR);
    let pixels = if is_composite {
        let r = reader
            .read_full_2d(COMPOSITE_R_VAR)
            .map_err(|err| err.to_string())?;
        let g = reader
            .read_full_2d(COMPOSITE_G_VAR)
            .map_err(|err| err.to_string())?;
        let b = reader
            .read_full_2d(COMPOSITE_B_VAR)
            .map_err(|err| err.to_string())?;
        render_composite_pixels(&r, &g, &b, nx, ny, flip_rows)
    } else {
        let variable = meta
            .variables
            .iter()
            .find(|var| var.kind == "surface2d")
            .ok_or_else(|| format!("{key}/t{hhmm:04} holds no 2D variable"))?;
        let band = selector_band(&variable.selector, &variable.name)
            .ok_or_else(|| format!("{key}/t{hhmm:04} selector carries no band"))?;
        let name = variable.name.clone();
        let values = reader.read_full_2d(&name).map_err(|err| err.to_string())?;
        render_sat_pixels(&name, band, &values, nx, ny, flip_rows, map_overlay)
    };

    Ok(ColoredSatFrame {
        frame: SatFrameImage {
            key: key.clone(),
            hhmm,
            image: ColorImage::new([nx, ny], pixels),
            read_ms: started.elapsed().as_secs_f32() * 1000.0,
        },
    })
}

/// Store variable names for a baked GOES ABI RGB composite frame: three
/// f32 planes in `[0, 255]` (NaN = transparent / off-earth / night).
const COMPOSITE_R_VAR: &str = "rgb_r";
const COMPOSITE_G_VAR: &str = "rgb_g";
const COMPOSITE_B_VAR: &str = "rgb_b";

/// Render three baked RGB planes to `Color32`. A pixel is transparent when
/// any channel is non-finite (the composite stored a `TRANSPARENT` pixel);
/// otherwise it is opaque true color. Row flipping matches
/// [`render_sat_pixels`] so composites and single-band frames share the
/// map/player geometry.
fn render_composite_pixels(
    r: &[f32],
    g: &[f32],
    b: &[f32],
    nx: usize,
    ny: usize,
    flip_rows: bool,
) -> Vec<Color32> {
    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if flip_rows { ny - 1 - image_row } else { image_row };
        for col in 0..nx {
            let idx = grid_row * nx + col;
            let (rv, gv, bv) = (r[idx], g[idx], b[idx]);
            if !(rv.is_finite() && gv.is_finite() && bv.is_finite()) {
                pixels.push(Color32::TRANSPARENT);
                continue;
            }
            pixels.push(Color32::from_rgb(
                rv.round().clamp(0.0, 255.0) as u8,
                gv.round().clamp(0.0, 255.0) as u8,
                bv.round().clamp(0.0, 255.0) as u8,
            ));
        }
    }
    pixels
}

fn render_sat_pixels(
    variable_name: &str,
    band: u8,
    values: &[f32],
    nx: usize,
    ny: usize,
    flip_rows: bool,
    map_overlay: bool,
) -> Vec<Color32> {
    // Himawari AHI brightness-temperature bands are NOT stored in the Kelvin
    // domain GOES uses: `ahi_bt_c13` lands ~326-330 (verified from a real
    // full-disk frame), not ~190-310 K, so a fixed-temperature table clamps
    // the whole disk to the warm/dark end and it renders black. Auto-stretch
    // Himawari's real range (p2..p98) through the enhanced-IR color ramp
    // instead -- the old grayscale intent, now in color. GOES longwave window
    // (13-15) IS real Kelvin, so it keeps the physically-anchored fixed
    // enhancement. Every other band uses its production palette.
    let dynamic = (variable_name.starts_with("ahi_bt_") && (7..=16).contains(&band))
        .then(|| finite_percentile_range(values, 0.02, 0.98))
        .flatten();
    let static_anchors = enhanced_anchors_for_band(band).unwrap_or_else(|| band_anchors(band));
    let ir_band = (7..=16).contains(&band);

    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if flip_rows {
            ny - 1 - image_row
        } else {
            image_row
        };
        for &value in &values[grid_row * nx..(grid_row + 1) * nx] {
            if let Some((lo, hi)) = dynamic {
                if !value.is_finite() {
                    pixels.push(Color32::TRANSPARENT);
                    continue;
                }
                // norm: 0 = coldest (low value), 1 = warmest; mapped onto the
                // colorful part of the enhancement.
                let norm = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
                let pseudo_k = DYN_COLD_K + norm * (DYN_WARM_K - DYN_COLD_K);
                let [r, g, b, _] = anchor_color(pseudo_k, ENHANCED_IR);
                let alpha = if map_overlay {
                    ((1.0 - norm) * 235.0) as u8 // cold=opaque, warm=clear
                } else {
                    255
                };
                pixels.push(Color32::from_rgba_unmultiplied(r, g, b, alpha));
            } else {
                let [r, g, b, a] = anchor_color(value, static_anchors);
                let alpha = if map_overlay && ir_band {
                    bt_overlay_alpha(value)
                } else {
                    a
                };
                pixels.push(Color32::from_rgba_unmultiplied(r, g, b, alpha));
            }
        }
    }
    pixels
}

/// Coldest/warmest pseudo-Kelvin the Himawari dynamic stretch maps its
/// auto-detected value range onto, i.e. the colorful span of [`ENHANCED_IR`].
const DYN_COLD_K: f32 = 200.0;
const DYN_WARM_K: f32 = 292.0;

/// Robust value range over the given finite percentiles (auto-contrast for a
/// band whose values are not in a fixed physical domain).
fn finite_percentile_range(values: &[f32], low: f32, high: f32) -> Option<(f32, f32)> {
    let mut finite = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if finite.is_empty() {
        return None;
    }
    finite.sort_by(|a, b| a.total_cmp(b));
    let last = finite.len() - 1;
    let low_idx = ((last as f32) * low.clamp(0.0, 1.0)).round() as usize;
    let high_idx = ((last as f32) * high.clamp(0.0, 1.0)).round() as usize;
    let lo = finite[low_idx.min(last)];
    let hi = finite[high_idx.max(low_idx).min(last)];
    if hi > lo {
        Some((lo, hi))
    } else {
        Some((lo, lo + 1.0))
    }
}

/// Enhanced-IR "rainbow" enhancement over brightness temperature (Kelvin),
/// applied to the longwave window bands (13/14/15) on BOTH GOES ABI and
/// Himawari AHI. Warm surface/low cloud reads grayscale; cold cloud tops light
/// up green → yellow → orange → red → dark, with the magenta/white overshoot
/// tips — the classic tropical/severe IR look (CIMSS-style). Anchors are
/// (Kelvin, [r,g,b]); °C is shown for reference (K = °C + 273.15).
const ENHANCED_IR: rw_sat::palette::Anchors = &[
    (173.0, [255, 255, 255]),  // -100 C: coldest overshoot
    (183.0, [232, 84, 232]),   //  -90 C: magenta
    (193.0, [188, 188, 188]),  //  -80 C: light gray
    (203.0, [70, 8, 8]),       //  -70 C: very dark red
    (213.0, [226, 26, 26]),    //  -60 C: red
    (223.0, [245, 148, 28]),   //  -50 C: orange
    (233.0, [246, 240, 42]),   //  -40 C: yellow
    (243.0, [48, 200, 66]),    //  -30 C: green (cold-cloud onset)
    (253.0, [120, 150, 120]),  //  -20 C: gray-green transition
    (263.0, [205, 205, 205]),  //  -10 C: light gray
    (273.15, [245, 245, 245]), //   0 C: white
    (293.0, [96, 96, 96]),     //  +20 C: gray
    (313.0, [46, 26, 14]),     //  +40 C: dark brown (warm surface)
    (330.0, [8, 5, 4]),        //  +57 C: near black
];

/// Longwave-window bands get the enhanced rainbow; every other band keeps its
/// specialized production palette (water vapor 8-10, shortwave 7, visible 1-6).
fn enhanced_anchors_for_band(band: u8) -> Option<rw_sat::palette::Anchors> {
    matches!(band, 13..=15).then_some(ENHANCED_IR)
}

/// Radar-map-overlay alpha from brightness temperature: warm (> +5 C) fully
/// transparent so radar/basemap shows through; cold storm tops (< -40 C)
/// nearly opaque. Replaces the old luminance proxy, which mis-fired once IR is
/// in color (saturated hues have low luminance).
fn bt_overlay_alpha(bt: f32) -> u8 {
    if !bt.is_finite() {
        return 0;
    }
    const WARM: f32 = 278.0; // +5 C
    const COLD: f32 = 233.0; // -40 C
    (((WARM - bt) / (WARM - COLD)).clamp(0.0, 1.0) * 235.0) as u8
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

/// CF `sweep_angle_axis = "y"` scan-angle navigation for JMA AHI scenes.
///
/// Himawari HSD navigation is the CGMS normalized geostationary projection
/// (LRIT/HRIT Global Specification, CGMS 03, Issue 2.6, 1999, §4.4), which
/// PROJ/satpy express as `+proj=geos +sweep=y`; GOES-R ABI is `sweep=x`
/// (GOES-R Product Definition and Users' Guide Vol. 3, §5.1.2.8). The two
/// conventions apply the E-W/N-S gimbal rotations in opposite order, which
/// moves ground points by ~5-15 km mid-disk and tens of km near the limb.
/// The pinned rw-sat stamps Himawari scenes with the GOES convention, and
/// its `SweepAngleAxis::Y` branch swaps the input angles (an image
/// transpose, thousands of km off) instead of swapping the rotation order,
/// so AHI meshes are navigated here instead — see the
/// `rw_sat_sweep_y_branch_is_still_transposed_upstream` tripwire test.
///
/// Same ellipsoid-intersection quadratic as the GOES PUG, with the view
/// vector assembled in sweep=y order (PROJ `geos` inverse, sweep=y branch):
/// `Vy = tan(x)`, `Vz = tan(y) * hypot(1, Vy)`.
fn ahi_scan_angles_to_lat_lon(
    projection: &SatelliteProjection,
    x_rad: f64,
    y_rad: f64,
) -> Option<(f32, f32)> {
    let h = projection.perspective_point_height_m + projection.semi_major_axis_m;
    let a = projection.semi_major_axis_m;
    let b = projection.semi_minor_axis_m;
    let lon0 = projection.longitude_of_projection_origin_deg;
    if !h.is_finite() || !lon0.is_finite() || !x_rad.is_finite() || !y_rad.is_finite() {
        return None;
    }
    if h <= 0.0 || a <= 0.0 || b <= 0.0 {
        return None;
    }

    // Satellite -> ground view vector (x toward the earth center, y east,
    // z north), assembled in the sweep=y decomposition cited above.
    let v_y = x_rad.tan();
    let v_z = y_rad.tan() * 1.0_f64.hypot(v_y);
    let eq_to_pol = (a * a) / (b * b);

    let a_var = 1.0 + v_y * v_y + eq_to_pol * v_z * v_z;
    let b_var = -2.0 * h;
    let c_var = h * h - a * a;
    let discriminant = b_var * b_var - 4.0 * a_var * c_var;
    if discriminant < 0.0 {
        return None; // looking past the limb
    }

    let r_s = (-b_var - discriminant.sqrt()) / (2.0 * a_var);
    if !r_s.is_finite() || r_s <= 0.0 {
        return None;
    }

    let s_x = r_s;
    let s_y = -r_s * v_y;
    let s_z = r_s * v_z;

    let latitude = (eq_to_pol * (s_z / (h - s_x).hypot(s_y))).atan();
    let longitude = lon0.to_radians() - (s_y / (h - s_x)).atan();
    let lat_deg = latitude.to_degrees();
    let mut lon_deg = (longitude.to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
    if lon_deg == -180.0 {
        lon_deg = 180.0;
    }
    if !lat_deg.is_finite() || !lon_deg.is_finite() {
        return None;
    }
    Some((lat_deg as f32, lon_deg as f32))
}

/// `SatelliteGridScene::lat_lon_mesh` with the sweep=y navigation above
/// (rows outer / columns inner, matching rw-sat's stored value order).
fn ahi_lat_lon_mesh(scene: &SatelliteGridScene) -> (Vec<f32>, Vec<f32>) {
    let len = scene.fixed_grid.nx.saturating_mul(scene.fixed_grid.ny);
    let mut lat = Vec::with_capacity(len);
    let mut lon = Vec::with_capacity(len);
    for &y in &scene.fixed_grid.y_scan_rad {
        for &x in &scene.fixed_grid.x_scan_rad {
            match ahi_scan_angles_to_lat_lon(&scene.projection, x, y) {
                Some((lat_value, lon_value)) => {
                    lat.push(lat_value);
                    lon.push(lon_value);
                }
                None => {
                    lat.push(f32::NAN);
                    lon.push(f32::NAN);
                }
            }
        }
    }
    (lat, lon)
}

/// rw-store token sanitizer, byte-for-byte rw-sat's store convention so
/// Himawari run dirs keep their established names.
fn sanitize_store_token(value: &str) -> String {
    let sanitized = value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn coords_bit_identical(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The generic `{"satellite": {...}}` frame selector rw-sat writes, except
/// the projection block says `sweep_angle_axis = "y"` — the convention the
/// stored mesh is actually navigated with (the HSD assembler stamps the
/// GOES `"x"` on the scene).
fn himawari_selector(field: &SatelliteGridField) -> serde_json::Value {
    let scene = &field.scene;
    let projection = serde_json::json!({
        "perspective_point_height_m": scene.projection.perspective_point_height_m,
        "semi_major_axis_m": scene.projection.semi_major_axis_m,
        "semi_minor_axis_m": scene.projection.semi_minor_axis_m,
        "longitude_of_projection_origin_deg":
            scene.projection.longitude_of_projection_origin_deg,
        "sweep_angle_axis": "y",
    });
    serde_json::json!({
        "satellite": {
            "provider": scene.provider,
            "instrument": scene.instrument,
            "satellite": scene.satellite,
            "model": scene.model,
            "product": scene.product,
            "sector": scene.sector,
            "band": scene.band,
            "layer": scene.layer,
            "source_variable": scene.source_variable,
            "scan_start_utc": scene.start_time_utc.to_rfc3339(),
            "scan_end_utc": scene.end_time_utc.to_rfc3339(),
            "projection": projection,
            "metadata": scene.metadata,
        }
    })
}

/// `rw_sat::store::write_satellite_grid_frame` with one change: the
/// per-pixel lat/lon mesh — the store's geometry of record (the `.rwg`
/// projection slot is `None`) — comes from the CF sweep=y navigation above.
/// Everything else follows the documented rw-sat store contract: one
/// `grid.rwg` per run dir shared by bit-identical grids, `t{HHMM}.rws`
/// hour frames, a `run.json` manifest, and a fresh suffixed run dir when
/// the fixed grid changes. Only `ingest_latest_himawari` writes h8/h9
/// model dirs, so this fork's blast radius is Himawari alone; retire it
/// for the upstream writer once rw-sat navigates sweep=y correctly (the
/// tripwire test fires when that happens).
fn write_himawari_grid_frame(
    store_root: &Path,
    field: &SatelliteGridField,
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    let scene = &field.scene;
    let model = sanitize_store_token(&scene.model);
    let sector = sanitize_store_token(&scene.sector);
    let day = scene.start_time_utc.format("%Y%m%d").to_string();
    let hhmm = (scene.start_time_utc.hour() * 100 + scene.start_time_utc.minute()) as u16;
    let run_base = format!("{sector}_c{band:02}_{day}", band = scene.band);

    let (nx, ny) = (scene.fixed_grid.nx, scene.fixed_grid.ny);
    if field.values.len() != nx.saturating_mul(ny) {
        return Err(format!(
            "field length {} does not match grid {nx}x{ny}",
            field.values.len()
        ));
    }
    let (lat, lon) = ahi_lat_lon_mesh(scene);
    let shape = GridShape::new(nx, ny).map_err(|err| err.to_string())?;
    let grid = LatLonGrid::new(shape, lat, lon).map_err(|err| err.to_string())?;

    // Reuse the run dir whose stored grid is bit-identical, else take the
    // first free suffixed name — rw-sat's rule that keeps grid changes
    // honest (a moved/rescaled grid opens a fresh run dir).
    let model_dir = store_root.join(&model);
    let mut candidates: Vec<String> = Vec::new();
    if model_dir.is_dir() {
        for entry in std::fs::read_dir(&model_dir).map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            let is_dir = entry.file_type().map_err(|err| err.to_string())?.is_dir();
            let name = entry.file_name().to_string_lossy().to_string();
            if is_dir && (name == run_base || name.starts_with(&format!("{run_base}_"))) {
                candidates.push(name);
            }
        }
    }
    candidates.sort();
    let mut resolved: Option<(String, String)> = None;
    for name in &candidates {
        let grid_path = model_dir.join(name).join("grid.rwg");
        if !grid_path.is_file() {
            continue;
        }
        let existing = GridFile::open(&grid_path).map_err(|err| err.to_string())?;
        if existing.nx == nx
            && existing.ny == ny
            && coords_bit_identical(&existing.lat, &grid.lat_deg)
            && coords_bit_identical(&existing.lon, &grid.lon_deg)
        {
            resolved = Some((name.clone(), existing.hash));
            break;
        }
    }
    let created_run = resolved.is_none();
    let (run_name, existing_grid_hash) = match resolved {
        Some((name, hash)) => (name, Some(hash)),
        None => {
            let mut suffix = 1usize;
            loop {
                let name = if suffix == 1 {
                    run_base.clone()
                } else {
                    format!("{run_base}_{suffix}")
                };
                if !candidates.contains(&name) {
                    break (name, None);
                }
                suffix += 1;
            }
        }
    };

    let run_dir = model_dir.join(&run_name);
    std::fs::create_dir_all(&run_dir).map_err(|err| err.to_string())?;
    // Same 60 s frame lock rw-sat's writers hold.
    let _lock =
        RunLock::acquire(&run_dir, Duration::from_secs(60)).map_err(|err| err.to_string())?;

    let grid_path = run_dir.join("grid.rwg");
    let grid_hash = match existing_grid_hash {
        Some(hash) => hash,
        None => write_grid(&grid_path, &grid, None).map_err(|err| err.to_string())?,
    };

    let started = Instant::now();
    let variable = field.variable_name.clone();
    let selector = himawari_selector(field);
    let writer_build = concat!("bowecho app_ui ", env!("CARGO_PKG_VERSION"));
    let mut writer = HourWriter::new(&model, &run_name, hhmm, nx, ny, &grid_hash, writer_build);
    writer
        .add_surface2d(&variable, &field.units, selector, &field.values)
        .map_err(|err| err.to_string())?;
    let file_name = frame_file_name(hhmm);
    let frame_path = run_dir.join(&file_name);
    writer.finish(&frame_path).map_err(|err| err.to_string())?;
    let encode_ms = started.elapsed().as_millis() as u64;
    let bytes = std::fs::metadata(&frame_path)
        .map_err(|err| err.to_string())?
        .len();

    let manifest_path = run_dir.join("run.json");
    let writer_info = RwsWriterInfo {
        name: "bowecho".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: writer_build.to_string(),
    };
    let mut manifest = RwsRunManifest::load_or_new(
        &manifest_path,
        &model,
        &run_name,
        &grid_hash,
        nx,
        ny,
        writer_info,
    )
    .map_err(|err| err.to_string())?;
    manifest.register_hour(
        hhmm,
        RwsHourEntry {
            file: file_name,
            written_unix,
            encode_ms,
            variables: vec![variable.clone()],
        },
    );
    manifest
        .save(&manifest_path)
        .map_err(|err| err.to_string())?;

    Ok(WrittenFrame {
        model,
        run: run_name,
        hhmm,
        scan_time_utc: scene.start_time_utc,
        path: frame_path,
        bytes,
        encode_ms,
        grid_hash,
        created_run,
        variable,
    })
}

fn ingest_latest_himawari(
    store_root: &Path,
    spec: &HimawariQuickSpec,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<String, String> {
    let satellite = HimawariSatellite::parse(&spec.satellite)
        .ok_or_else(|| format!("unknown Himawari satellite '{}'", spec.satellite))?;
    let band = spec.band.clamp(1, 16);
    let agent = build_agent();
    let request = HimawariLatestRequest {
        satellite,
        product: HimawariProduct::AhiL1bFldk,
        band: Some(band),
        lookback_minutes: spec.lookback_minutes.max(10),
        require_complete: true,
    };
    let result = list_latest_segments(&agent, &request).map_err(|err| err.to_string())?;
    let source_complete = is_complete_segment_set(&result.segments);
    let segment_count = spec.segment_limit.max(1).min(result.segments.len());
    let cache_root = store_root.join("cache");
    let source_root = store_root.join("sources").join("himawari");
    let manifest_dir = source_root.join("manifest");
    let raw_dir = source_root.join("raw");

    let mut manifest_segments = Vec::with_capacity(segment_count);
    let mut row_ids = Vec::with_capacity(segment_count);
    let mut total_bytes = 0_u64;
    for segment in result.segments.iter().take(segment_count) {
        let id = segment.object.key.clone();
        row_ids.push(id.clone());
        let label = format!(
            "{} B{:02} S{:02}/{:02}",
            result.satellite.platform(),
            segment.name.band,
            segment.name.segment_index,
            segment.name.segment_count
        );
        send(SatResponse::DownloadStarted {
            id: id.clone(),
            label,
            bytes: segment.object.size_bytes,
        });
        let started = Instant::now();
        let downloaded = download_object(
            &agent,
            result.satellite.bucket(),
            &cache_root,
            &segment.object,
            true,
        )
        .map_err(|err| err.to_string())?;
        send(SatResponse::DownloadDone {
            id,
            ms: started.elapsed().as_millis(),
            cache_hit: downloaded.cache_hit,
        });
        total_bytes = total_bytes.saturating_add(segment.object.size_bytes);
        manifest_segments.push(HimawariManifestSegment {
            band: segment.name.band,
            segment_index: segment.name.segment_index,
            segment_count: segment.name.segment_count,
            product: segment.name.product.clone(),
            resolution: segment.name.resolution.clone(),
            key: segment.object.key.clone(),
            url: object_url(result.satellite.bucket(), &segment.object.key),
            last_modified: segment.object.last_modified.clone(),
            size_bytes: segment.object.size_bytes,
            cache_path: downloaded.path.display().to_string(),
            cache_hit: downloaded.cache_hit,
        });
    }

    std::fs::create_dir_all(&manifest_dir).map_err(|err| err.to_string())?;
    let manifest_path = manifest_dir.join(format!(
        "{}_{}_b{band:02}_{}.json",
        result.satellite.slug(),
        result.product.slug(),
        result.scan_time.format("%Y%m%dT%H%M%SZ")
    ));
    let manifest = HimawariDownloadManifest {
        schema: HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA.to_string(),
        satellite: result.satellite.slug().to_string(),
        platform: result.satellite.platform().to_string(),
        bucket: result.satellite.bucket().to_string(),
        product: result.product.slug().to_string(),
        scan_time_utc: result.scan_time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        prefix: result.prefix,
        band,
        segments_downloaded: manifest_segments.len(),
        segments_available: result.segments.len(),
        source_complete,
        allow_partial: false,
        total_downloaded_bytes: total_bytes,
        cache_root: cache_root.display().to_string(),
        segments: manifest_segments,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|err| err.to_string())?;
    std::fs::write(&manifest_path, manifest_bytes).map_err(|err| err.to_string())?;

    let staged =
        stage_download_manifest(&manifest_path, &raw_dir).map_err(|err| err.to_string())?;
    let paths = staged
        .segments
        .iter()
        .map(|segment| PathBuf::from(&segment.raw_path))
        .collect::<Vec<_>>();
    let field = assemble_hsd_segments(
        &paths,
        HimawariValueMode::BrightnessTemperature,
        spec.downsample.max(1),
    )
    .map_err(|err| err.to_string())?;
    let nx = field.scene.fixed_grid.nx;
    let ny = field.scene.fixed_grid.ny;
    // AHI is CF sweep_angle_axis "y": write through the local sweep=y
    // writer so the stored mesh is real AHI navigation (rw-sat's writer
    // navigates with the GOES "x" convention; see write_himawari_grid_frame).
    let frame =
        write_himawari_grid_frame(store_root, &field, Utc::now().timestamp().max(0) as u64)?;
    for id in row_ids {
        send(SatResponse::FrameWritten {
            id,
            run: frame.run.clone(),
            hhmm: frame.hhmm,
            bytes: frame.bytes,
            encode_ms: frame.encode_ms,
        });
    }
    send(SatResponse::Runs(scan_runs(store_root)));
    send(SatResponse::SelectFrame {
        key: SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        },
        hhmm: frame.hhmm,
    });
    Ok(format!(
        "Himawari {} B{band:02}: {} segment(s), {}x{}, wrote {}/{}/t{:04}",
        result.satellite.platform(),
        paths.len(),
        nx,
        ny,
        frame.model,
        frame.run,
        frame.hhmm
    ))
}

/// ABI scan mode token in the open-data filenames (mode 6 since 2019; mode
/// 3 is the legacy contingency schedule). A mode flip degrades to editing
/// this constant, mirroring rw-sat's follow engine.
const GOES_ABI_SCAN_MODE: u8 = 6;

/// Newest scan start time for which EVERY required band has an object under
/// the recent hour prefixes, plus that scan's per-band object. All 16 ABI
/// channels of one scan share the same filename `s` timestamp, so the scan
/// keys line up exactly across bands (no fuzzy time matching needed).
fn latest_common_scan(
    bucket: &str,
    abi_product: &str,
    satellite: &GoesSatellite,
    bands: &[u8],
    hours: &[DateTime<Utc>],
) -> Result<(DateTime<Utc>, HashMap<u8, S3Object>), String> {
    let agent = build_agent();
    let mut per_band: HashMap<u8, HashMap<DateTime<Utc>, S3Object>> = HashMap::new();
    for &band in bands {
        let mut scans: HashMap<DateTime<Utc>, S3Object> = HashMap::new();
        for hour in hours {
            let prefix = band_hour_prefix(abi_product, satellite, GOES_ABI_SCAN_MODE, band, *hour);
            let objects =
                list_s3_objects(&agent, bucket, &prefix, None).map_err(|err| err.to_string())?;
            for object in objects {
                if !object.key.ends_with(".nc") {
                    continue;
                }
                let Ok(parsed) = parse_goes_abi_filename(object_filename(&object.key)) else {
                    continue;
                };
                if parsed.channel != Some(band)
                    || !abi_filename_product_matches_request(&parsed.product, abi_product)
                {
                    continue;
                }
                scans.insert(parsed.start_time_utc, object);
            }
        }
        if scans.is_empty() {
            return Err(format!(
                "no recent GOES {abi_product} C{band:02} objects in the last {} hour prefix(es)",
                hours.len()
            ));
        }
        per_band.insert(band, scans);
    }

    let base = bands[0];
    let mut candidates: Vec<DateTime<Utc>> = per_band[&base].keys().copied().collect();
    candidates.sort_unstable();
    for scan in candidates.into_iter().rev() {
        if bands.iter().all(|band| per_band[band].contains_key(&scan)) {
            let objects = bands
                .iter()
                .map(|band| (*band, per_band[band][&scan].clone()))
                .collect();
            return Ok((scan, objects));
        }
    }
    Err("no scan time yet has every band the composite needs".to_string())
}

/// Download the ABI bands a composite needs (the latest scan that has all of
/// them), co-register them onto the base channel's fixed grid, compose the
/// RGB per pixel through rw-sat, and write one composite frame into the sat
/// store. This is the Track D true/natural-color ingest — the single-band
/// enhanced-IR/visible path already lives in [`render_sat_pixels`].
///
/// Recipes and per-pixel math are rw-sat's [`compose_rgb_pixels`]; GeoColor /
/// NaturalColor here are the daytime pseudo-true-color visible composites
/// (CIRA/CIMSS "GeoColor" lineage; night renders dark/transparent).
fn ingest_latest_goes_composite(
    store_root: &Path,
    spec: &GoesCompositeSpec,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<String, String> {
    let style = GoesAbiRgbCompositeStyle::parse(&spec.style)
        .ok_or_else(|| format!("unknown composite style '{}'", spec.style))?;
    let bucket = bucket_for_satellite(&spec.satellite).map_err(|err| err.to_string())?;
    let sector =
        Sector::parse(&spec.sector).ok_or_else(|| format!("unknown sector '{}'", spec.sector))?;
    let satellite = GoesSatellite::parse(&spec.satellite);
    let abi_product = sector.abi_product();
    let bands = style.required_channels().to_vec();
    let base_channel = style.base_channel();
    let downsample = spec.downsample.max(1);
    let cache_dir = store_root.join("cache");

    // Recent hour prefixes to scan for the newest all-band scan.
    let now = Utc::now();
    let hour_span = (spec.lookback_minutes.max(20) / 60) + 2;
    let hours: Vec<DateTime<Utc>> = (0..hour_span)
        .map(|i| now - chrono::Duration::hours(i))
        .collect();

    let (scan_start, objects) =
        latest_common_scan(&bucket, abi_product, &satellite, &bands, &hours)?;

    // Download + decode + decimate every required band.
    let agent = build_agent();
    let mut fields: HashMap<u8, GoesAbiField> = HashMap::with_capacity(bands.len());
    for &band in &bands {
        let object = &objects[&band];
        send(SatResponse::DownloadStarted {
            id: object.key.clone(),
            label: download_label(&object.key),
            bytes: object.size_bytes,
        });
        let started = Instant::now();
        let downloaded = download_object(&agent, &bucket, &cache_dir, object, true)
            .map_err(|err| err.to_string())?;
        send(SatResponse::DownloadDone {
            id: object.key.clone(),
            ms: started.elapsed().as_millis(),
            cache_hit: downloaded.cache_hit,
        });
        let field = read_goes_abi_field(&downloaded.path, "CMI").map_err(|err| err.to_string())?;
        fields.insert(band, downsample_field(field, downsample));
    }

    // Base grid = the (decimated) base channel; resample every band onto it,
    // then compose per pixel.
    let base_scene = fields
        .get(&base_channel)
        .ok_or_else(|| format!("composite base channel C{base_channel:02} was not fetched"))?
        .scene
        .clone();
    let (nx, ny) = (base_scene.fixed_grid.nx, base_scene.fixed_grid.ny);
    let len = nx.saturating_mul(ny);
    let mut planes: HashMap<u8, Vec<f32>> = HashMap::with_capacity(bands.len());
    for (&band, field) in &fields {
        let values = values_on_base_grid(field, &base_scene).map_err(|err| err.to_string())?;
        planes.insert(band, values);
    }
    let rgba = compose_rgb_pixels(style, &planes, len).map_err(|err| err.to_string())?;

    // Split into three f32 planes (NaN = transparent / off-earth / night).
    let (mut r, mut g, mut b) = (
        Vec::with_capacity(len),
        Vec::with_capacity(len),
        Vec::with_capacity(len),
    );
    for pixel in &rgba {
        if pixel[3] == 0 {
            r.push(f32::NAN);
            g.push(f32::NAN);
            b.push(f32::NAN);
        } else {
            r.push(f32::from(pixel[0]));
            g.push(f32::from(pixel[1]));
            b.push(f32::from(pixel[2]));
        }
    }

    let frame = write_goes_composite_frame(
        store_root,
        &base_scene,
        style,
        &r,
        &g,
        &b,
        Utc::now().timestamp().max(0) as u64,
    )?;

    for object in objects.values() {
        send(SatResponse::FrameWritten {
            id: object.key.clone(),
            run: frame.run.clone(),
            hhmm: frame.hhmm,
            bytes: frame.bytes,
            encode_ms: frame.encode_ms,
        });
    }
    send(SatResponse::Runs(scan_runs(store_root)));
    send(SatResponse::SelectFrame {
        key: SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        },
        hhmm: frame.hhmm,
    });

    let lit = rgba.iter().filter(|pixel| pixel[3] != 0).count();
    Ok(format!(
        "GOES {} {} {}: scan {} · {} band(s) · {}x{} · {:.0}% lit · wrote {}/{}/t{:04}",
        satellite.as_str(),
        sector.slug(),
        style.title(),
        scan_start.format("%Y-%m-%d %H:%MZ"),
        bands.len(),
        nx,
        ny,
        100.0 * lit as f64 / len.max(1) as f64,
        frame.model,
        frame.run,
        frame.hhmm
    ))
}

/// The generic satellite selector for a baked RGB composite frame: base band
/// on the GOES fixed-grid projection (sweep=x), plus a `composite` block
/// naming the style and its source bands.
fn composite_selector(scene: &GoesAbiScene, style: GoesAbiRgbCompositeStyle) -> serde_json::Value {
    let projection = &scene.projection;
    let sweep = match projection.sweep_angle_axis {
        rw_sat::geostationary::SweepAngleAxis::X => "x",
        rw_sat::geostationary::SweepAngleAxis::Y => "y",
    };
    let bands = style
        .required_channels()
        .iter()
        .map(|band| serde_json::json!(band))
        .collect::<Vec<_>>();
    serde_json::json!({
        "satellite": {
            "provider": "noaa",
            "instrument": "abi",
            "satellite": scene.satellite.as_str(),
            "product": scene.product,
            "band": style.base_channel(),
            "layer": format!("rgb_{}", style.slug()),
            "source_variable": "CMI",
            "composite": {
                "style": style.slug(),
                "title": style.title(),
                "bands": bands,
            },
            "scan_start_utc": scene.start_time_utc.to_rfc3339(),
            "scan_end_utc": scene.end_time_utc.to_rfc3339(),
            "projection": {
                "perspective_point_height_m": projection.perspective_point_height_m,
                "semi_major_axis_m": projection.semi_major_axis_m,
                "semi_minor_axis_m": projection.semi_minor_axis_m,
                "longitude_of_projection_origin_deg":
                    projection.longitude_of_projection_origin_deg,
                "sweep_angle_axis": sweep,
            },
        }
    })
}

/// Write a baked RGB composite as one store frame: three `rgb_r/g/b` f32
/// planes on the GOES per-pixel lat/lon mesh, following the same store
/// contract as [`write_band_frame`] (grid.rwg shared by bit-identical grids,
/// `t{HHMM}.rws`, a `run.json` manifest, fresh suffixed run dir on a grid
/// change). Composite runs are `<sector>_rgb_<style>_<YYYYMMDD>`.
#[allow(clippy::too_many_arguments)]
fn write_goes_composite_frame(
    store_root: &Path,
    scene: &GoesAbiScene,
    style: GoesAbiRgbCompositeStyle,
    r: &[f32],
    g: &[f32],
    b: &[f32],
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    let model = scene.satellite.as_str().to_ascii_lowercase();
    let sector = sector_slug(&scene.sector);
    let day = scene.start_time_utc.format("%Y%m%d").to_string();
    let hhmm = (scene.start_time_utc.hour() * 100 + scene.start_time_utc.minute()) as u16;
    let run_base = format!("{sector}_rgb_{}_{day}", style.slug());

    let (nx, ny) = (scene.fixed_grid.nx, scene.fixed_grid.ny);
    let expected = nx.saturating_mul(ny);
    if r.len() != expected || g.len() != expected || b.len() != expected {
        return Err(format!(
            "composite plane length mismatch for grid {nx}x{ny}: r={} g={} b={}",
            r.len(),
            g.len(),
            b.len()
        ));
    }
    let (lat, lon) = scene.lat_lon_mesh();
    let shape = GridShape::new(nx, ny).map_err(|err| err.to_string())?;
    let grid = LatLonGrid::new(shape, lat, lon).map_err(|err| err.to_string())?;

    // Reuse the run dir whose stored grid is bit-identical, else the next
    // free suffix — the store rule that keeps grid changes honest.
    let model_dir = store_root.join(&model);
    let mut candidates: Vec<String> = Vec::new();
    if model_dir.is_dir() {
        for entry in std::fs::read_dir(&model_dir).map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            let is_dir = entry.file_type().map_err(|err| err.to_string())?.is_dir();
            let name = entry.file_name().to_string_lossy().to_string();
            if is_dir && (name == run_base || name.starts_with(&format!("{run_base}_"))) {
                candidates.push(name);
            }
        }
    }
    candidates.sort();
    let mut resolved: Option<(String, String)> = None;
    for name in &candidates {
        let grid_path = model_dir.join(name).join("grid.rwg");
        if !grid_path.is_file() {
            continue;
        }
        let existing = GridFile::open(&grid_path).map_err(|err| err.to_string())?;
        if existing.nx == nx
            && existing.ny == ny
            && coords_bit_identical(&existing.lat, &grid.lat_deg)
            && coords_bit_identical(&existing.lon, &grid.lon_deg)
        {
            resolved = Some((name.clone(), existing.hash));
            break;
        }
    }
    let created_run = resolved.is_none();
    let (run_name, existing_grid_hash) = match resolved {
        Some((name, hash)) => (name, Some(hash)),
        None => {
            let mut suffix = 1usize;
            loop {
                let name = if suffix == 1 {
                    run_base.clone()
                } else {
                    format!("{run_base}_{suffix}")
                };
                if !candidates.contains(&name) {
                    break (name, None);
                }
                suffix += 1;
            }
        }
    };

    let run_dir = model_dir.join(&run_name);
    std::fs::create_dir_all(&run_dir).map_err(|err| err.to_string())?;
    let _lock =
        RunLock::acquire(&run_dir, Duration::from_secs(60)).map_err(|err| err.to_string())?;

    let grid_path = run_dir.join("grid.rwg");
    let grid_hash = match existing_grid_hash {
        Some(hash) => hash,
        None => write_grid(&grid_path, &grid, None).map_err(|err| err.to_string())?,
    };

    let started = Instant::now();
    let selector = composite_selector(scene, style);
    let writer_build = concat!("bowecho app_ui ", env!("CARGO_PKG_VERSION"));
    let mut writer = HourWriter::new(&model, &run_name, hhmm, nx, ny, &grid_hash, writer_build);
    writer
        .add_surface2d(COMPOSITE_R_VAR, "rgb8", selector, r)
        .map_err(|err| err.to_string())?;
    writer
        .add_surface2d(COMPOSITE_G_VAR, "rgb8", serde_json::Value::Null, g)
        .map_err(|err| err.to_string())?;
    writer
        .add_surface2d(COMPOSITE_B_VAR, "rgb8", serde_json::Value::Null, b)
        .map_err(|err| err.to_string())?;
    let file_name = frame_file_name(hhmm);
    let frame_path = run_dir.join(&file_name);
    writer.finish(&frame_path).map_err(|err| err.to_string())?;
    let encode_ms = started.elapsed().as_millis() as u64;
    let bytes = std::fs::metadata(&frame_path)
        .map_err(|err| err.to_string())?
        .len();

    let manifest_path = run_dir.join("run.json");
    let writer_info = RwsWriterInfo {
        name: "bowecho".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: writer_build.to_string(),
    };
    let mut manifest = RwsRunManifest::load_or_new(
        &manifest_path,
        &model,
        &run_name,
        &grid_hash,
        nx,
        ny,
        writer_info,
    )
    .map_err(|err| err.to_string())?;
    manifest.register_hour(
        hhmm,
        RwsHourEntry {
            file: file_name,
            written_unix,
            encode_ms,
            variables: vec![
                COMPOSITE_R_VAR.to_string(),
                COMPOSITE_G_VAR.to_string(),
                COMPOSITE_B_VAR.to_string(),
            ],
        },
    );
    manifest.save(&manifest_path).map_err(|err| err.to_string())?;

    Ok(WrittenFrame {
        model,
        run: run_name,
        hhmm,
        scan_time_utc: scene.start_time_utc,
        path: frame_path,
        bytes,
        encode_ms,
        grid_hash,
        created_run,
        variable: COMPOSITE_R_VAR.to_string(),
    })
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
            SatRequest::IngestLatestHimawari(spec) => {
                send(SatResponse::Note(format!(
                    "Himawari: locating latest {} B{:02}",
                    spec.satellite, spec.band
                )));
                match ingest_latest_himawari(&store_root, &spec, &send) {
                    Ok(summary) => {
                        send(SatResponse::Note(summary));
                        send(SatResponse::Runs(scan_runs(&store_root)));
                    }
                    Err(message) => {
                        send(SatResponse::Note(format!("Himawari failed: {message}")));
                    }
                }
            }
            SatRequest::IngestLatestGoesComposite(spec) => {
                send(SatResponse::Note(format!(
                    "GOES composite: locating latest {} {} {}",
                    spec.satellite, spec.sector, spec.style
                )));
                match ingest_latest_goes_composite(&store_root, &spec, &send) {
                    Ok(summary) => {
                        send(SatResponse::Note(summary));
                        send(SatResponse::Runs(scan_runs(&store_root)));
                    }
                    Err(message) => {
                        send(SatResponse::Note(format!(
                            "GOES composite failed: {message}"
                        )));
                    }
                }
            }
            SatRequest::LoadLoop(spec) => {
                if follow_active.swap(true, Ordering::SeqCst) {
                    send(SatResponse::Note(
                        "a satellite ingest session is already running".to_string(),
                    ));
                    continue;
                }
                let mut config = match follow_config(&spec, &store_root) {
                    Ok(config) => config,
                    Err(message) => {
                        follow_active.store(false, Ordering::SeqCst);
                        send(SatResponse::FollowFinished(Err(message)));
                        continue;
                    }
                };
                config.max_polls = Some(1);
                config.max_frames = Some(24);
                config.poll_interval = Some(Duration::from_secs(1));
                config.jitter_frac = 0.0;
                let (model, prefixes) =
                    run_prefixes(&spec).expect("spec validated by follow_config");
                cancel.store(false, Ordering::Relaxed);
                if !send(SatResponse::FollowStarted) {
                    return;
                }
                send(SatResponse::Note(
                    "GOES loop: loading current-hour frames".to_string(),
                ));
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
                    .name("rw-sat-loop-load".to_string())
                    .spawn(move || {
                        rw_ingest::throttle::set_current_thread_background_priority();
                        let result = {
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
                            rw_sat::follow(&config, &mut sink, &thread_cancel)
                        };
                        active.store(false, Ordering::SeqCst);
                        let response = match result {
                            Ok(summary) => SatResponse::FollowFinished(Ok(format!(
                                "loop load done - {} frame(s) in {} poll(s)",
                                summary.frames.len(),
                                summary.polls
                            ))),
                            Err(SatError::Cancelled) => {
                                SatResponse::FollowFinished(Ok("loop load stopped".to_string()))
                            }
                            Err(err) => SatResponse::FollowFinished(Err(err.to_string())),
                        };
                        let _ = tx.send(response);
                        let _ = tx.send(SatResponse::Runs(scan_runs(&root)));
                        let _ =
                            tx.send(SatResponse::DiskUsage(disk_usage(&root, &model, &prefixes)));
                        thread_notify();
                    });
                if let Err(err) = spawned {
                    follow_active.store(false, Ordering::SeqCst);
                    send(SatResponse::FollowFinished(Err(format!(
                        "failed to spawn the loop-load thread: {err}"
                    ))));
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
    // The differential test against rw-sat's upstream writer uses it
    // directly; production code writes through the local
    // `write_himawari_grid_frame` fork above.
    use chrono::{TimeZone, Utc};
    use rw_sat::abi::{AbiFixedGrid, AbiSector, GoesAbiField, GoesAbiScene, GoesImagerProjection};
    use rw_sat::geostationary::SweepAngleAxis;
    use rw_sat::store::write_satellite_grid_frame;
    use rw_sat::store::{read_frame, write_band_frame};

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
        // A cold band-13 pixel is colorized (enhanced-IR rainbow) and opaque —
        // grayscale would have r == g == b.
        let cold = frame.image.pixels[1];
        assert_eq!(cold.a(), 255);
        assert!(
            !(cold.r() == cold.g() && cold.g() == cold.b()),
            "band-13 IR is colorized, not grayscale: {cold:?}"
        );
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
    fn himawari_ir_is_colorized_not_grayscale() {
        // Kelvin brightness temps: NaN, warm surface, a -60 C top, a -40 C top.
        let values = vec![f32::NAN, 300.0, 213.0, 233.0];
        let pixels = render_sat_pixels("cmi_c13", 13, &values, 4, 1, false, false);

        assert_eq!(pixels[0].a(), 0, "NaN stays transparent");
        // -60 C cloud top is RED in the enhancement — proof it is colorized,
        // not the old grayscale stretch (which was r == g == b).
        let cold = pixels[2];
        assert!(
            cold.r() > 180 && cold.g() < 90 && cold.b() < 90,
            "cold top is red: {cold:?}"
        );
        assert!(
            !(cold.r() == cold.g() && cold.g() == cold.b()),
            "IR is no longer grayscale"
        );
        // -40 C is yellow.
        let yellow = pixels[3];
        assert!(
            yellow.r() > 180 && yellow.g() > 180 && yellow.b() < 110,
            "yellow: {yellow:?}"
        );
    }

    /// The `SatelliteGridField` shape rw-sat's HSD assembler produces for a
    /// tiny Himawari-9 full-disk cutout. Projection constants follow the
    /// JMA Himawari Standard Data User's Guide: satellite distance
    /// 42164 km (height 35785.863 km above the 6378.137 km equator), GRS80
    /// polar radius 6356.7523 km, sub-lon 140.7E. The assembler stamps the
    /// GOES sweep axis (X) on the scene — the defect under test.
    fn synthetic_ahi_field(hour: u32, minute: u32, x_scan_rad: Vec<f64>) -> SatelliteGridField {
        let start = Utc.with_ymd_and_hms(2026, 6, 10, hour, minute, 0).unwrap();
        let nx = x_scan_rad.len();
        SatelliteGridField {
            scene: SatelliteGridScene {
                model: "h9".to_string(),
                satellite: "Himawari-9".to_string(),
                provider: "jma".to_string(),
                instrument: "ahi".to_string(),
                product: "AHI-L1b-FLDK".to_string(),
                sector: "fulldisk".to_string(),
                band: 13,
                layer: "bt_c13".to_string(),
                source_variable: "HSD count".to_string(),
                start_time_utc: start,
                end_time_utc: start + chrono::Duration::seconds(600),
                projection: SatelliteProjection {
                    perspective_point_height_m: 35_785_863.0,
                    semi_major_axis_m: 6_378_137.0,
                    semi_minor_axis_m: 6_356_752.3,
                    longitude_of_projection_origin_deg: 140.7,
                    sweep_angle_axis: SweepAngleAxis::X,
                },
                fixed_grid: AbiFixedGrid {
                    nx,
                    ny: 2,
                    x_scan_rad,
                    y_scan_rad: vec![0.12, 0.0],
                },
                metadata: serde_json::json!({"source_format": "himawari_standard_data"}),
            },
            variable_name: "ahi_bt_c13".to_string(),
            units: "K".to_string(),
            values: (0..nx * 2).map(|i| 210.0 + i as f32).collect(),
        }
    }

    /// FIX R6: the stored Himawari mesh must be CF sweep=y navigation.
    /// Reference points from pyproj 3.7.2 (`+proj=geos +h=35785863
    /// +a=6378137 +b=6356752.3 +lon_0=140.7`) at scan angles x=0.04 rad,
    /// y=0.12 rad:
    ///   sweep=y -> 46.296691N 160.862374E  (JMA AHI convention)
    ///   sweep=x -> 46.248876N 160.996382E  (GOES convention, ~10 km off —
    ///                                       what the rw-sat writer bakes)
    #[test]
    fn himawari_frames_bake_a_cf_sweep_y_mesh() {
        let dir = test_dir("ahi-sweep");
        let field = synthetic_ahi_field(2, 0, vec![0.0, 0.04]);
        let frame = write_himawari_grid_frame(&dir, &field, 7).expect("frame writes");
        assert_eq!((frame.model.as_str(), frame.hhmm), ("h9", 200));

        let grid = GridFile::open(&dir.join("h9").join(&frame.run).join("grid.rwg")).unwrap();
        // Row 0 is y=0.12, so index 1 is (x=0.04, y=0.12).
        let (lat, lon) = (f64::from(grid.lat[1]), f64::from(grid.lon[1]));
        assert!(
            (lat - 46.296691).abs() < 2e-3 && (lon - 160.862374).abs() < 2e-3,
            "sweep=y reference, got {lat} {lon}"
        );
        // Index 2 is the sub-satellite point, identical in both conventions.
        let (lat0, lon0) = (f64::from(grid.lat[2]), f64::from(grid.lon[2]));
        assert!(
            lat0.abs() < 1e-5 && (lon0 - 140.7).abs() < 1e-4,
            "sub-satellite point, got {lat0} {lon0}"
        );

        // The stored selector labels the mesh honestly.
        let stored = read_frame(&dir, &frame.model, &frame.run, frame.hhmm).unwrap();
        assert_eq!(
            stored.selector["satellite"]["projection"]["sweep_angle_axis"],
            "y"
        );

        // The upstream writer bakes the GOES point for the same field —
        // the pre-fix behavior this test exists to reject — and the store
        // contract forks the run dir for the differing mesh.
        let upstream = write_satellite_grid_frame(&dir, &field, 8).expect("upstream writes");
        assert_ne!(upstream.run, frame.run, "different mesh -> forked run dir");
        let old = GridFile::open(&dir.join("h9").join(&upstream.run).join("grid.rwg")).unwrap();
        let (old_lat, old_lon) = (f64::from(old.lat[1]), f64::from(old.lon[1]));
        assert!(
            (old_lat - 46.248876).abs() < 2e-3 && (old_lon - 160.996382).abs() < 2e-3,
            "rw-sat still navigates sweep=x, got {old_lat} {old_lon}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn himawari_writer_follows_the_store_contract() {
        let dir = test_dir("ahi-store");
        let first = write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 0, vec![0.0, 0.04]), 7)
            .expect("first frame");
        assert!(first.created_run);
        let second =
            write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 10, vec![0.0, 0.04]), 8)
                .expect("second frame joins the run");
        assert!(!second.created_run);
        assert_eq!(second.run, first.run);
        assert_eq!(
            second.grid_hash, first.grid_hash,
            "grid written once per run"
        );

        // A different fixed grid (e.g. another downsample) forks the run.
        let moved =
            write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 20, vec![0.0, 0.05]), 9)
                .expect("changed grid writes");
        assert!(moved.created_run);
        assert_ne!(moved.run, first.run);

        // The app's own reader accepts the frames end-to-end (grid-hash
        // validation included) and the player sees one run per grid.
        let runs = scan_runs(&dir);
        assert_eq!(runs.len(), 2);
        let mut state = WorkerState::default();
        let key = SatRunKey {
            model: first.model.clone(),
            run: first.run.clone(),
        };
        let frame = load_frame(&mut state, &dir, &key, 210).expect("t0210 loads");
        assert_eq!(frame.image.size, [2, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Documents WHY `write_himawari_grid_frame` exists: the pinned rw-sat
    /// navigates its `SweepAngleAxis::Y` branch by swapping the scan angles
    /// (an image transpose), which is not CF sweep=y. When this test starts
    /// FAILING, upstream rw-sat has fixed sweep=y: switch the Himawari
    /// ingest back to `rw_sat::store::write_satellite_grid_frame` with the
    /// scene projection set to `SweepAngleAxis::Y`, and retire the local
    /// writer plus this tripwire.
    #[test]
    fn rw_sat_sweep_y_branch_is_still_transposed_upstream() {
        use rw_sat::geostationary::scan_angles_to_lat_lon;
        // Himawari-9: h = 42164 km - 6378.137 km, GRS80, sub-lon 140.7E.
        let (h, a, b, lon0) = (35_785_863.0, 6_378_137.0, 6_356_752.3, 140.7);
        let (lat_x, lon_x) =
            scan_angles_to_lat_lon(h, a, b, lon0, SweepAngleAxis::X, 0.04, 0.12).unwrap();
        assert!(
            (f64::from(lat_x) - 46.248876).abs() < 1e-3
                && (f64::from(lon_x) - 160.996382).abs() < 1e-3,
            "rw-sat sweep=x no longer matches the GOES reference: {lat_x} {lon_x}"
        );
        let (lat_y, lon_y) =
            scan_angles_to_lat_lon(h, a, b, lon0, SweepAngleAxis::Y, 0.04, 0.12).unwrap();
        let true_sweep_y = (f64::from(lat_y) - 46.296691).abs() < 1e-3
            && (f64::from(lon_y) - 160.862374).abs() < 1e-3;
        assert!(
            !true_sweep_y,
            "rw-sat now implements CF sweep=y: retire write_himawari_grid_frame \
             in favor of write_satellite_grid_frame + SweepAngleAxis::Y"
        );
    }

    #[test]
    fn export_display_proof_png_when_env_is_set() {
        let Some(out) = std::env::var_os("BOWECHO_SAT_PROOF_PNG") else {
            return;
        };
        let store = std::env::var_os("BOWECHO_SAT_PROOF_STORE")
            .map(PathBuf::from)
            .expect("BOWECHO_SAT_PROOF_STORE is required when exporting a proof PNG");
        let model = std::env::var("BOWECHO_SAT_PROOF_MODEL")
            .expect("BOWECHO_SAT_PROOF_MODEL is required when exporting a proof PNG");
        let run = std::env::var("BOWECHO_SAT_PROOF_RUN")
            .expect("BOWECHO_SAT_PROOF_RUN is required when exporting a proof PNG");
        let hhmm = std::env::var("BOWECHO_SAT_PROOF_HHMM")
            .expect("BOWECHO_SAT_PROOF_HHMM is required when exporting a proof PNG")
            .parse::<u16>()
            .expect("BOWECHO_SAT_PROOF_HHMM must be HHMM");

        let key = SatRunKey { model, run };
        let mut state = WorkerState::default();
        let frame = load_frame(&mut state, &store, &key, hhmm).expect("proof frame loads");
        let mut rgba = Vec::with_capacity(frame.image.pixels.len() * 4);
        for pixel in frame.image.pixels {
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
        let image = image::RgbaImage::from_raw(
            frame.image.size[0] as u32,
            frame.image.size[1] as u32,
            rgba,
        )
        .expect("proof image dimensions match");
        if let Some(parent) = PathBuf::from(&out).parent() {
            std::fs::create_dir_all(parent).expect("proof png parent directory");
        }
        image.save(&out).expect("proof png writes");
    }

    /// A co-registered visible scene (reflectance factor 0..1) with a
    /// vegetation pixel, a bright cloud, dark water, and an off-earth NaN.
    fn composite_visible_field(nx: usize, ny: usize, band: u8, values: Vec<f32>) -> GoesAbiField {
        let mut field = synthetic_field(nx, ny, 18, 51, band);
        field.units = Some("1".to_string());
        field.values = values;
        field
    }

    #[test]
    fn composite_natural_color_round_trips_and_greens_vegetation() {
        let dir = test_dir("composite");
        let (nx, ny) = (2usize, 2usize);
        // Row-major: [0] vegetation, [1] bright cloud, [2] dark water,
        // [3] off-earth (NaN in the NIR band -> transparent composite).
        let c01 = composite_visible_field(nx, ny, 1, vec![0.04, 0.85, 0.03, 0.05]);
        let c02 = composite_visible_field(nx, ny, 2, vec![0.06, 0.88, 0.04, 0.05]);
        let c03 = composite_visible_field(nx, ny, 3, vec![0.50, 0.90, 0.05, f32::NAN]);
        let style = GoesAbiRgbCompositeStyle::NaturalColor;
        let base_scene = c02.scene.clone();
        let len = nx * ny;

        let mut planes: HashMap<u8, Vec<f32>> = HashMap::new();
        planes.insert(1, values_on_base_grid(&c01, &base_scene).unwrap());
        planes.insert(2, values_on_base_grid(&c02, &base_scene).unwrap());
        planes.insert(3, values_on_base_grid(&c03, &base_scene).unwrap());
        let rgba = compose_rgb_pixels(style, &planes, len).expect("compose");

        let (mut r, mut g, mut b) = (Vec::new(), Vec::new(), Vec::new());
        for pixel in &rgba {
            if pixel[3] == 0 {
                r.push(f32::NAN);
                g.push(f32::NAN);
                b.push(f32::NAN);
            } else {
                r.push(f32::from(pixel[0]));
                g.push(f32::from(pixel[1]));
                b.push(f32::from(pixel[2]));
            }
        }
        let frame = write_goes_composite_frame(&dir, &base_scene, style, &r, &g, &b, 1).unwrap();
        assert_eq!(frame.model, "g19");
        assert!(
            frame.run.contains("_rgb_natural_color_"),
            "composite run naming: {}",
            frame.run
        );
        assert!(frame.created_run);

        // Load back through the exact player path (composite branch).
        let mut state = WorkerState::default();
        let key = SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        };
        let loaded = load_frame(&mut state, &dir, &key, frame.hhmm).expect("composite loads");
        assert_eq!(loaded.image.size, [nx, ny]);

        // Vegetation pixel: opaque, green channel dominant (the natural-color
        // green synthesized from NIR/red/blue).
        let veg = loaded.image.pixels[0];
        assert_eq!(veg.a(), 255, "lit composite pixel is opaque");
        assert!(
            veg.g() > veg.r() && veg.g() > veg.b(),
            "vegetation renders green, not garbage: {veg:?}"
        );
        // Bright cloud: opaque and near-neutral bright.
        let cloud = loaded.image.pixels[1];
        assert_eq!(cloud.a(), 255);
        assert!(
            cloud.r() > 150 && cloud.g() > 150 && cloud.b() > 150,
            "cloud is bright: {cloud:?}"
        );
        // Off-earth pixel (NaN band) is transparent.
        assert_eq!(
            loaded.image.pixels[3].a(),
            0,
            "off-earth composite pixel is transparent"
        );

        // The stored frame is self-describing as a composite.
        let stored = rw_sat::store::read_frame(&dir, &frame.model, &frame.run, frame.hhmm).unwrap();
        assert_eq!(
            stored.selector["satellite"]["composite"]["style"],
            "natural_color"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn composite_run_titles_and_filters_recognize_rgb_runs() {
        let title = run_title("g19", "conus_rgb_natural_color_20260705");
        assert!(
            title.contains("GeoColor") && title.contains("2026-07-05"),
            "composite title: {title}"
        );
        // NaturalColor's title is GeoColor (daytime pseudo-true-color).
        let geo = run_title("g18", "fulldisk_rgb_geocolor_20260705");
        assert!(geo.contains("GeoColor"), "geocolor title: {geo}");
    }

    #[test]
    fn composite_style_options_lead_with_natural_color() {
        let options = goes_composite_style_options();
        assert_eq!(options.len(), GoesAbiRgbCompositeStyle::ALL.len());
        assert_eq!(options[0].0, "natural_color");
        assert!(options[0].1.contains("C01+C02+C03"), "{}", options[0].1);
        // Every offered slug parses back to a real style.
        for (slug, _) in &options {
            assert!(GoesAbiRgbCompositeStyle::parse(slug).is_some(), "slug {slug}");
        }
    }

    /// End-to-end proof against LIVE GOES open data: fetch the composite's
    /// bands, compose, store, load back, and export a PNG. Gated behind
    /// `BOWECHO_SAT_COMPOSITE_PROOF_PNG` so CI stays offline; run it to prove
    /// the natural-color path on real imagery (never synthetic-only).
    #[test]
    fn export_goes_composite_proof_png_when_env_is_set() {
        let Some(out) = std::env::var_os("BOWECHO_SAT_COMPOSITE_PROOF_PNG") else {
            return;
        };
        let store = std::env::var_os("BOWECHO_SAT_COMPOSITE_PROOF_STORE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("bowecho-composite-proof-store"));
        std::fs::create_dir_all(&store).expect("proof store dir");
        let spec = GoesCompositeSpec {
            satellite: std::env::var("BOWECHO_SAT_COMPOSITE_SAT")
                .unwrap_or_else(|_| "goes19".to_string()),
            sector: std::env::var("BOWECHO_SAT_COMPOSITE_SECTOR")
                .unwrap_or_else(|_| "conus".to_string()),
            style: std::env::var("BOWECHO_SAT_COMPOSITE_STYLE")
                .unwrap_or_else(|_| "natural_color".to_string()),
            downsample: std::env::var("BOWECHO_SAT_COMPOSITE_DOWNSAMPLE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(6usize),
            lookback_minutes: 240,
        };
        let sink = |response: SatResponse| {
            if let SatResponse::Note(message) = &response {
                eprintln!("COMPOSITE note: {message}");
            }
            true
        };
        let summary =
            ingest_latest_goes_composite(&store, &spec, &sink).expect("live composite ingest");
        eprintln!("COMPOSITE {summary}");

        let runs = scan_runs(&store);
        let run = runs
            .iter()
            .find(|run| run.key.run.contains("_rgb_"))
            .expect("a composite run was written");
        let hhmm = *run.frames.last().expect("composite run has a frame");
        let mut state = WorkerState::default();
        let frame = load_frame(&mut state, &store, &run.key, hhmm).expect("proof frame loads");

        let mut rgba = Vec::with_capacity(frame.image.pixels.len() * 4);
        for pixel in &frame.image.pixels {
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
        let image = image::RgbaImage::from_raw(
            frame.image.size[0] as u32,
            frame.image.size[1] as u32,
            rgba,
        )
        .expect("proof image dimensions match");
        if let Some(parent) = PathBuf::from(&out).parent() {
            std::fs::create_dir_all(parent).expect("proof png parent directory");
        }
        image.save(&out).expect("composite proof png writes");
        eprintln!(
            "COMPOSITE proof PNG {}x{} -> {}",
            frame.image.size[0],
            frame.image.size[1],
            PathBuf::from(&out).display()
        );
    }

    /// Deterministic synthetic hurricane IR brightness-temperature field
    /// (Kelvin): warm ocean, a warm eye, a very cold eyewall with embedded
    /// overshoots, a cold CDO, and log-spiral rainbands. Lets the enhanced-IR
    /// color table be verified/benched offline without live satellite data.
    fn synthetic_hurricane_bt(nx: usize, ny: usize) -> Vec<f32> {
        let (cx, cy) = (nx as f32 / 2.0, ny as f32 / 2.0);
        let mut values = vec![f32::NAN; nx * ny];
        for y in 0..ny {
            for x in 0..nx {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let r = (dx * dx + dy * dy).sqrt();
                let theta = dy.atan2(dx);
                let mut bt = if r < 16.0 {
                    278.0 - (16.0 - r) * 0.3 // warm subsident eye
                } else if r < 44.0 {
                    198.0 // deep-convective eyewall
                } else if r < 130.0 {
                    212.0 + (r - 44.0) * 0.14 // central dense overcast, warming out
                } else if r < 215.0 {
                    236.0 + (r - 130.0) * 0.22
                } else {
                    (256.0 + (r - 215.0) * 0.4).min(292.0) // ambient ocean
                };
                // Log-spiral rainbands, strongest in the mid annulus.
                if r > 55.0 && r < 245.0 {
                    let phase = 2.0 * theta - 3.4 * (r * 0.01 + 1.0).ln();
                    let band = phase.sin().max(0.0);
                    bt -= band * 24.0 * ((245.0 - r) / 190.0).clamp(0.0, 1.0);
                }
                // Cold overshoot cells embedded in the eyewall.
                if (20.0..44.0).contains(&r)
                    && (x as f32 * 0.55).sin() * (y as f32 * 0.5).cos() > 0.8
                {
                    bt -= 14.0;
                }
                // Fine texture (deterministic).
                bt += (((x * 131 + y * 197) % 101) as f32 - 50.0) * 0.06;
                values[y * nx + x] = bt.clamp(178.0, 300.0);
            }
        }
        values
    }

    #[test]
    fn print_frame_value_stats() {
        let Some(store) = std::env::var_os("BOWECHO_SAT_STATS_STORE") else {
            return;
        };
        let model = std::env::var("BOWECHO_SAT_STATS_MODEL").unwrap();
        let run = std::env::var("BOWECHO_SAT_STATS_RUN").unwrap();
        let hhmm: u16 = std::env::var("BOWECHO_SAT_STATS_HHMM")
            .unwrap()
            .parse()
            .unwrap();
        let run_dir = PathBuf::from(store).join(&model).join(&run);
        let reader = HourReader::open(&run_dir.join(frame_file_name(hhmm))).unwrap();
        let meta = reader.meta();
        let variable = meta
            .variables
            .iter()
            .find(|v| v.kind == "surface2d")
            .unwrap();
        let band = selector_band(&variable.selector, &variable.name).unwrap();
        let name = variable.name.clone();
        let values = reader.read_full_2d(&name).unwrap();
        let mut finite: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
        finite.sort_by(|a, b| a.total_cmp(b));
        let n = finite.len();
        let pct = |p: f32| finite[(((n - 1) as f32) * p) as usize];
        eprintln!(
            "STATS {model}/{run} band={band} var={name} finite={n}/{} min={:.2} p1={:.2} p50={:.2} p99={:.2} max={:.2} mean={:.2}",
            values.len(),
            finite[0],
            pct(0.01),
            pct(0.50),
            pct(0.99),
            finite[n - 1],
            finite.iter().sum::<f32>() / n as f32
        );
    }

    #[test]
    fn himawari_bt_out_of_kelvin_domain_still_colorizes() {
        // Real Himawari ahi_bt_c13 lands ~326-330, NOT Kelvin BT — verified
        // from a live full-disk frame. It must auto-stretch and colorize, not
        // render black (the regression this fixes).
        let values = vec![f32::NAN, 326.0, 327.5, 328.0, 329.0, 330.0];
        let pixels = render_sat_pixels("ahi_bt_c13", 13, &values, 3, 2, false, false);

        assert_eq!(pixels[0].a(), 0, "NaN stays transparent");
        let cold = pixels[1]; // 326 -> coldest -> colorful
        let warm = pixels[5]; // 330 -> warmest
        assert_ne!(cold, warm, "the range is stretched, not flat/black");
        assert!(
            u16::from(cold.r()) + u16::from(cold.g()) + u16::from(cold.b()) > 120,
            "coldest cloud is not black: {cold:?}"
        );
        assert!(
            !(cold.r() == cold.g() && cold.g() == cold.b()),
            "colorized, not grayscale: {cold:?}"
        );
    }

    #[test]
    fn synth_hurricane_ir_proof() {
        let Some(out) = std::env::var_os("BOWECHO_SAT_SYNTH_PNG") else {
            return;
        };
        let (nx, ny) = (512usize, 512usize);
        let bt = synthetic_hurricane_bt(nx, ny);

        let started = std::time::Instant::now();
        let pixels = render_sat_pixels("cmi_c13", 13, &bt, nx, ny, false, false);
        let render_ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!("enhanced-IR render of {nx}x{ny} took {render_ms:.2} ms");

        // Compose the hurricane above a 173..320 K color-bar strip.
        let bar_h = 28usize;
        let out_h = ny + bar_h;
        let mut rgba = vec![0u8; nx * out_h * 4];
        for (i, pixel) in pixels.iter().enumerate() {
            rgba[i * 4..i * 4 + 4].copy_from_slice(&[pixel.r(), pixel.g(), pixel.b(), 255]);
        }
        for row in 0..bar_h {
            for x in 0..nx {
                let k = 320.0 - (x as f32 / nx as f32) * (320.0 - 173.0);
                let [r, g, b, _] = rw_sat::palette::anchor_color(k, ENHANCED_IR);
                let idx = ((ny + row) * nx + x) * 4;
                rgba[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
        let img =
            image::RgbaImage::from_raw(nx as u32, out_h as u32, rgba).expect("proof image dims");
        if let Some(parent) = PathBuf::from(&out).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        img.save(&out).expect("synth proof png writes");
    }

    #[test]
    fn ir_map_overlay_alpha_fades_warm_by_temperature() {
        // Kelvin: warm surface, mid cloud, cold storm top.
        let values = vec![290.0, 255.0, 210.0];
        let overlay = render_sat_pixels("cmi_c13", 13, &values, 3, 1, false, true);

        assert_eq!(
            overlay[0].a(),
            0,
            "warm (+17 C) clears so radar shows through"
        );
        assert!(
            overlay[1].a() > 0 && overlay[1].a() < overlay[2].a(),
            "mid cloud is semi-transparent"
        );
        assert!(overlay[2].a() > 200, "cold storm top (-63 C) stays visible");

        // The full-screen player keeps the palette opaque (no BT fade).
        let player = render_sat_pixels("cmi_c13", 13, &values, 3, 1, false, false);
        assert!(player[0].a() > 200, "player keeps warm pixels opaque");
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
