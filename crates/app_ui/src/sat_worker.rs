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
use rw_sat::abi::{
    AbiFixedGrid, AbiSector, GoesAbiField, GoesAbiScene, read_goes_abi_field,
    read_goes_abi_field_window, read_goes_abi_scene,
};
use rw_sat::composite::{
    GoesAbiRgbCompositeStyle, bilinear_f32, bracket_axis, compose_rgb_pixels, values_on_base_grid,
};
use rw_sat::events::{SatError, SatEvent};
use rw_sat::follow::FollowConfig;
use rw_sat::geostationary::{SweepAngleAxis, lat_lon_to_scan_angles_fast};
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::himawari::{
    HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA, HimawariCalibrationInfo, HimawariDownloadManifest,
    HimawariLatestRequest, HimawariManifestSegment, HimawariProduct, HimawariSatellite,
    HimawariValueMode, assemble_hsd_segments, inspect_hsd_file, is_complete_segment_set,
    list_latest_segments, parse_segment_name, stage_download_manifest,
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

use crate::sat_plot::{SatellitePlotPalette, SatellitePlotSource};
use crate::sat_window::{
    AHI_NOMINAL_HEIGHT_M, AHI_NOMINAL_SEMI_MAJOR_M, AHI_NOMINAL_SEMI_MINOR_M,
    AHI_NOMINAL_SUB_LON_DEG, SatNativeWindow, ScanAngleRect, ahi_fldk_segment_range,
    ahi_lat_lon_to_scan_angles, ahi_window_crop, axis_crop_range, window_scan_angle_rect,
};

/// Requests from the UI thread.
#[derive(Debug, Clone)]
pub enum SatRequest {
    /// Validate a spec and build its one-line summary.
    Validate(SatFollowSpec),
    /// Enumerate the sat store's runs and frames.
    Scan,
    /// Enumerate the store, then select one just-written frame. Both
    /// responses are emitted by this worker in order so an external SimSat
    /// producer cannot race a UI-side scan/select pair.
    ScanAndSelect { key: SatRunKey, hhmm: u16 },
    /// Start a live follow session (one at a time).
    Follow(SatFollowSpec),
    /// One-shot current-hour ingest for quickly creating a playable loop.
    LoadLoop(SatFollowSpec),
    /// Read one stored frame and color it with its band palette.
    LoadFrame { key: SatRunKey, hhmm: u16 },
    /// Read a frame PLUS its run grid for the radar-map layer.
    LoadFrameForMap { key: SatRunKey, hhmm: u16 },
    /// Read one stored frame as raw grid-order science data for the native
    /// plotter. This never recovers values from the player's colored image.
    LoadFrameForPlot { key: SatRunKey, hhmm: u16 },
    /// Select the IR enhancement used when coloring stored BT frames
    /// (applies to subsequent LoadFrame/LoadFrameForMap/LoadFrameForPlot
    /// responses).
    SetIrEnhancement(IrEnhancement),
    /// Download/decode the latest Himawari AHI frame into the shared sat store.
    IngestLatestHimawari(HimawariQuickSpec),
    /// Fetch the ABI bands a composite needs (co-registered by scan time),
    /// compose a true/natural-color RGB, and write it as one composite frame.
    IngestLatestGoesComposite(GoesCompositeSpec),
    /// Fetch the co-registered Himawari AHI visible bands (B01/B02/B03),
    /// compose an AHI true-color RGB, and write it as one composite frame —
    /// the Himawari analogue of [`SatRequest::IngestLatestGoesComposite`].
    IngestLatestHimawariComposite(HimawariCompositeSpec),
    /// Native-window single-band Himawari IR ingest (tropical-card "🛰 IR"):
    /// window-cropped true-Kelvin BT, recolored live by the IR enhancement.
    IngestLatestHimawariIrWindow(HimawariIrWindowSpec),
    /// Native-window GOES IR ingest (tropical-card "🛰 IR"): window-cropped
    /// CMI BT baked through the current IR enhancement into a `_rgb_` frame.
    IngestLatestGoesIrWindow(GoesIrWindowSpec),
}

/// Himawari AHI visible RGB-composite recipe. Unlike GOES ABI (which lacks a
/// native green band and must synthesize one), AHI has a real 0.51 µm green
/// (B02), so true color is a direct band assignment: R = B03 (0.64 µm red),
/// G = B02 (0.51 µm green), B = B01 (0.47 µm blue). See the JMA Himawari
/// Standard Data User's Guide (v1.3) §4 for the AHI band table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HimawariCompositeStyle {
    TrueColor,
}

impl HimawariCompositeStyle {
    pub const ALL: [HimawariCompositeStyle; 1] = [Self::TrueColor];

    pub fn slug(self) -> &'static str {
        match self {
            Self::TrueColor => "true_color",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::TrueColor => "AHI True Color",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        Self::ALL
            .into_iter()
            .find(|style| style.slug() == normalized)
    }

    /// AHI bands the composite fetches (co-registered onto the base band's
    /// grid), ordered blue → green → red.
    pub fn required_bands(self) -> &'static [u8] {
        match self {
            Self::TrueColor => &[1, 2, 3],
        }
    }

    /// Band whose (1 km) fixed grid the composite is rendered on. B01/B02 are
    /// 1 km; B03 is 0.5 km and is resampled down onto this base.
    pub fn base_band(self) -> u8 {
        match self {
            Self::TrueColor => 1,
        }
    }

    /// Band whose grid a NATIVE-WINDOW composite is rendered on: the finest
    /// required band (B03, 0.5 km), so the windowed crop keeps full
    /// instrument resolution — the point of the window. The full-sector
    /// path stays on the 1 km base (a 0.5 km full-sector base is 4× the
    /// pixels for no visible gain at sector zoom).
    pub fn native_base_band(self) -> u8 {
        match self {
            Self::TrueColor => 3,
        }
    }

    /// `(red_band, green_band, blue_band)` display assignment.
    pub fn rgb_bands(self) -> (u8, u8, u8) {
        match self {
            Self::TrueColor => (3, 2, 1),
        }
    }
}

/// One-shot Himawari AHI visible RGB-composite ingest request (the AHI
/// analogue of [`GoesCompositeSpec`]). AHI full-disk visible bands are huge
/// (B03 is 22000×22000 at 0.5 km), so the fetch is bounded to a contiguous
/// range of full-disk segments — `segment_start`..`segment_start+segment_count`
/// (1-based, of 10) — which defaults to the tropical band that covers the
/// west-Pacific tropics (Guam / PGUA).
#[derive(Debug, Clone)]
pub struct HimawariCompositeSpec {
    /// Himawari satellite slug (`h9`, `h8`).
    pub satellite: String,
    /// Composite style slug (`true_color`).
    pub style: String,
    /// First full-disk segment to fetch (1-based, S01 = north limb).
    pub segment_start: u8,
    /// Number of contiguous segments to fetch.
    pub segment_count: u8,
    /// Compose the WHOLE disk: fetch all ten segments per band (ignoring
    /// `segment_start` / `segment_count`) and assemble on a stride-decimated
    /// grid without ever materializing a native-resolution plane (see
    /// [`assemble_ahi_fulldisk_counts`]). `downsample` 4 (the default) puts
    /// the 1 km B01/B02 base at 2750² (~4 km effective); 2 puts it at 5500²
    /// (~2 km). B03 (0.5 km) decodes at double the stride so it lands
    /// straight on ~the base resolution. Ignored when `window` is set —
    /// the native window wins.
    pub full_disk: bool,
    /// How far back to scan 10-min slots for the latest all-band scan.
    pub lookback_minutes: i64,
    /// Per-band decimation stride applied on ingest.
    pub downsample: usize,
    /// Native-resolution spatial window. When set, the ingest derives the
    /// segment range from the window (ignoring `segment_start` /
    /// `segment_count`), decodes at stride 1 regardless of `downsample`,
    /// crops every band to the window, and composes on the 0.5 km B03 grid
    /// (see [`HimawariCompositeStyle::native_base_band`]).
    pub window: Option<SatNativeWindow>,
    /// Pick the newest complete scan at/before this time instead of "now" —
    /// lets proofs/backfills pin an exact scan (e.g. the last daylight pass).
    pub as_of: Option<DateTime<Utc>>,
    /// Tropical-card correlation ticket: when set, the dispatcher reports
    /// this ingest's outcome on the card-outcome side channel (see
    /// [`CardOutcome`]) so the requesting storm card can clear its spinner.
    pub card_ticket: Option<u64>,
}

impl Default for HimawariCompositeSpec {
    fn default() -> Self {
        // S04-S05 of the full disk span roughly +20 N .. 0, covering the
        // west-Pacific tropics and Guam (13.5 N) — where the live typhoon sits.
        Self {
            satellite: "h9".to_string(),
            style: "true_color".to_string(),
            segment_start: 4,
            segment_count: 2,
            full_disk: false,
            lookback_minutes: 180,
            downsample: 4,
            window: None,
            as_of: None,
            card_ticket: None,
        }
    }
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
    /// Native-resolution spatial window. When set the ingest decodes ONLY
    /// the NetCDF hyperslab covering the window at stride 1 (ignoring
    /// `downsample`): CMI files are whole-sector, so the download is
    /// unchanged, but decode/compose/store stay window-sized.
    pub window: Option<SatNativeWindow>,
    /// Pick the newest all-band scan at/before this time instead of "now".
    pub as_of: Option<DateTime<Utc>>,
    /// Tropical-card correlation ticket (see [`CardOutcome`]).
    pub card_ticket: Option<u64>,
}

impl Default for GoesCompositeSpec {
    fn default() -> Self {
        Self {
            satellite: "goes19".to_string(),
            sector: "conus".to_string(),
            style: "natural_color".to_string(),
            downsample: 4,
            lookback_minutes: 180,
            window: None,
            as_of: None,
            card_ticket: None,
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

/// One-shot NATIVE-WINDOW Himawari AHI IR ingest (the tropical-card "🛰 IR"
/// path for Himawari-covered storms): download only the full-disk segments
/// covering `window`, decode only the window's pixels of one IR band at
/// stride 1, calibrate to true Kelvin BT, and write a single-band frame.
/// The stored plane is brightness temperature, so the IR-enhancement picker
/// recolors it live at load time exactly like every other IR band frame.
#[derive(Debug, Clone)]
pub struct HimawariIrWindowSpec {
    /// Himawari satellite slug (`h9`, `h8`).
    pub satellite: String,
    /// AHI IR band (7-16; the card requests 13, Clean IR 10.4 µm).
    pub band: u8,
    /// Native-resolution spatial window (required — this request exists to
    /// serve a storm-centered crop; full-disk IR is `IngestLatestHimawari`).
    pub window: SatNativeWindow,
    /// How far back to scan 10-min slots for the latest scan.
    pub lookback_minutes: i64,
    /// Pick the newest scan at/before this time instead of "now".
    pub as_of: Option<DateTime<Utc>>,
    /// Tropical-card correlation ticket (see [`CardOutcome`]).
    pub card_ticket: Option<u64>,
}

/// One-shot NATIVE-WINDOW GOES ABI IR ingest (the tropical-card "🛰 IR"
/// path for GOES-covered storms): download the latest whole-sector CMI file
/// for one IR band, decode ONLY the window's hyperslab at stride 1
/// ([`read_goes_abi_window`] — the v0.29.3 native-window machinery), color
/// the Kelvin BT through the worker's CURRENT IR enhancement, and write the
/// result as a baked three-plane `_rgb_` frame.
///
/// Why baked: the app's run-key display filter admits GOES runs
/// unconditionally only for `_rgb_` (baked three-plane) run families, and
/// the store contract "`_rgb_` in the run name ⇔ the frame holds
/// `rgb_r/g/b` planes" must hold. The cost is that the enhancement is
/// fixed at ingest time — switching the IR-enhancement picker later
/// recolors single-band BT frames but not these; press the card button
/// again to bake the new curve.
#[derive(Debug, Clone)]
pub struct GoesIrWindowSpec {
    /// Satellite slug (`goes19`, `goes18`, `goes16`).
    pub satellite: String,
    /// Sector slug (`fulldisk` covers any storm the satellite can see).
    pub sector: String,
    /// ABI IR band (7-16; the card requests 13, Clean IR 10.3 µm).
    pub band: u8,
    /// Native-resolution spatial window (required, as above).
    pub window: SatNativeWindow,
    /// How far back to scan hour prefixes for the latest scan.
    pub lookback_minutes: i64,
    /// Pick the newest scan at/before this time instead of "now".
    pub as_of: Option<DateTime<Utc>>,
    /// Tropical-card correlation ticket (see [`CardOutcome`]).
    pub card_ticket: Option<u64>,
}

/// Completed one-shot ingest report on the card-only side channel: the
/// tropical storm cards need "my request finished (ok/err)" to clear their
/// one-press spinner, and the main response pump only runs while the
/// Satellite window is open — a dedicated channel keeps the card state
/// honest without new [`SatResponse`] variants (main.rs's pump match is
/// owned by the parallel extraction work and must not grow arms here).
#[derive(Debug, Clone)]
pub struct CardOutcome {
    /// The `card_ticket` the requesting spec carried.
    pub ticket: u64,
    /// The ingest's summary line (`Ok`) or failure message (`Err`).
    pub result: Result<String, String>,
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
    /// A raw, georeferenced frame for the shared native plot surface.
    PlotFrame {
        key: SatRunKey,
        hhmm: u16,
        result: Box<Result<SatellitePlotSource, String>>,
    },
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
        /// The frame rendered through the legacy pre-calibration stretch,
        /// so the selected IR enhancement did not apply (see
        /// [`render_sat_pixels`]); `false` on errors.
        legacy: bool,
        result: Box<Result<SatFrameImage, String>>,
    },
}

/// Handle to the satellite worker.
pub struct SatWorker {
    tx: Sender<SatRequest>,
    rx: Receiver<SatResponse>,
    card_rx: Receiver<CardOutcome>,
    cancel: Arc<AtomicBool>,
    _thread: JoinHandle<()>,
}

impl SatWorker {
    /// Spawn the worker. `store_root` is the sat store root (frames land
    /// and are read from here); `notify` wakes the UI after every response.
    pub fn spawn(store_root: PathBuf, notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = channel::<SatRequest>();
        let (resp_tx, resp_rx) = channel::<SatResponse>();
        let (card_tx, card_rx) = channel::<CardOutcome>();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(notify);
        let thread = std::thread::Builder::new()
            .name("rw-sat-worker".to_string())
            .spawn(move || {
                rw_ingest::throttle::set_current_thread_background_priority();
                worker_loop(
                    store_root,
                    &req_rx,
                    &resp_tx,
                    &card_tx,
                    &notify,
                    &worker_cancel,
                );
            })
            .expect("spawn sat worker thread");
        Self {
            tx: req_tx,
            rx: resp_rx,
            card_rx,
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

    /// Non-blocking poll for the next tropical-card ingest outcome
    /// (drained by the storm-card driver every frame, independent of
    /// whether the Satellite window's response pump is running).
    pub fn try_recv_card_outcome(&self) -> Option<CardOutcome> {
        self.card_rx.try_recv().ok()
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
    if model.eq_ignore_ascii_case("simsat") {
        return simsat_run_title(run);
    }
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
        for style in HimawariCompositeStyle::ALL {
            let marker = format!("_rgb_{}_", style.slug());
            if let Some(pos) = run.find(&marker) {
                let sector = &run[..pos];
                return format!("{model} · {sector} {} · {day}", style.title());
            }
        }
        // Tropical-card enhanced-IR window runs: `<sector>_rgb_ir<band>_<day>`.
        if let Some(pos) = run.find("_rgb_ir") {
            let band = run[pos + "_rgb_ir".len()..]
                .split('_')
                .next()
                .and_then(|raw| raw.parse::<u8>().ok());
            if let Some(band) = band {
                let sector = &run[..pos];
                return format!("{model} · {sector} Enhanced IR C{band:02} · {day}");
            }
        }
        return format!("{model} · {run}");
    }
    // The band token normally follows the sector, but windowed single-band
    // runs (`fulldisk_win135n1448e800_c13_<day>`) carry the window token in
    // between — take the first `c<band>` token wherever it sits and keep
    // everything before it (incl. the window token) as the sector label.
    let tokens: Vec<&str> = run.split('_').collect();
    let band_idx = tokens
        .iter()
        .skip(1)
        .position(|token| {
            token
                .strip_prefix('c')
                .is_some_and(|raw| raw.parse::<u8>().is_ok())
        })
        .map(|idx| idx + 1);
    let (sector, band) = match band_idx {
        Some(idx) => (
            tokens[..idx].join("_"),
            tokens[idx]
                .strip_prefix('c')
                .and_then(|raw| raw.parse::<u8>().ok()),
        ),
        None => (tokens.first().copied().unwrap_or(run).to_string(), None),
    };
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

/// Product/view-qualified SimSat run names are deliberately machine-stable;
/// turn their tokens into the compact source/product/view labels an operator
/// needs in the shared Satellite picker. The parser is order-independent so
/// both `<source>_<product>_<view>_...` and older sector-first names remain
/// readable.
fn simsat_run_title(run: &str) -> String {
    let normalized = run.to_ascii_lowercase();
    let tokens = normalized.split('_').collect::<Vec<_>>();
    let source = tokens.iter().find_map(|token| match *token {
        "hrrr" => Some("HRRR"),
        "wrf" | "wrfout" => Some("WRF"),
        "rrfs" => Some("RRFS"),
        _ => None,
    });
    let product = if normalized.contains("geocolor") {
        Some("GeoColor".to_string())
    } else if normalized.contains("sandwich") {
        Some("Sandwich".to_string())
    } else if normalized.contains("natural_color") || normalized.contains("true_color") {
        Some("True Color".to_string())
    } else if normalized.contains("visible") {
        Some("Visible".to_string())
    } else if let Some(band) = tokens.iter().find_map(|token| {
        token
            .strip_prefix("wv")
            .and_then(|raw| raw.parse::<u8>().ok())
    }) {
        Some(format!("Water Vapor C{band:02}"))
    } else if normalized.contains("water_vapor")
        || tokens
            .iter()
            .any(|token| matches!(*token, "wv" | "watervapor"))
    {
        Some("Water Vapor".to_string())
    } else if let Some(band) = tokens.iter().find_map(|token| {
        token
            .strip_prefix("ir")
            .and_then(|raw| raw.parse::<u8>().ok())
    }) {
        Some(format!("IR C{band:02}"))
    } else if tokens
        .iter()
        .any(|token| matches!(*token, "ir" | "infrared"))
    {
        Some("Infrared".to_string())
    } else if tokens.contains(&"pw") {
        Some("Precipitable Water".to_string())
    } else if tokens.contains(&"ctt") {
        Some("Cloud-top Temperature".to_string())
    } else if tokens.contains(&"cod") {
        Some("Cloud Optical Depth".to_string())
    } else if let Some(band) = tokens.iter().find_map(|token| {
        token
            .strip_prefix('c')
            .filter(|raw| raw.len() <= 2)
            .and_then(|raw| raw.parse::<u8>().ok())
    }) {
        Some(format!("C{band:02}"))
    } else if normalized.contains("derived") {
        Some("Derived".to_string())
    } else if normalized.contains("_rgb_") || normalized.starts_with("rgb_") {
        Some("RGB".to_string())
    } else {
        None
    };
    let view = if tokens
        .iter()
        .any(|token| matches!(*token, "geo" | "geos" | "geostationary"))
    {
        Some("GEO")
    } else if normalized.contains("top_down")
        || tokens
            .iter()
            .any(|token| matches!(*token, "topdown" | "map"))
    {
        Some("TOP-DOWN")
    } else if tokens.contains(&"perspective") {
        Some("PERSPECTIVE")
    } else {
        None
    };
    let day = run_day(run).map(|day| day.format("%Y-%m-%d").to_string());

    let mut parts = vec!["SimSat".to_string()];
    if let Some(source) = source {
        parts.push(source.to_string());
    }
    if let Some(product) = product {
        parts.push(product);
    }
    if let Some(view) = view {
        parts.push(view.to_string());
    }
    if let Some(day) = day {
        parts.push(day);
    }
    if parts.len() == 1 {
        parts.push(run.replace('_', " "));
    }
    parts.join(" · ")
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
    /// User-selected IR enhancement, applied at frame-coloring time.
    ir_enhancement: IrEnhancement,
    /// The most recently served MAP/native-plot grid, content-addressed by
    /// `GridFile.hash` (sha256 of the grid file bytes). Map playback
    /// requests a frame per step, and a full-disk `grid.rwg` is a ~240 MB
    /// read + hash per open — but every frame of a run, and successive
    /// runs of the same product, reference bit-identical grids, so one
    /// held `Arc` serves them all (the UI layers share the same `Arc`, so
    /// steady-state memory is unchanged). Player-only sessions never pay for
    /// it.
    map_grid: Option<Arc<GridFile>>,
}

struct ColoredSatFrame {
    frame: SatFrameImage,
    /// The frame predates the true-Kelvin AHI calibration and rendered
    /// through the legacy percentile stretch — the selected IR enhancement
    /// did NOT apply (see [`render_sat_pixels`]). Surfaced to the UI so the
    /// enhancement picker does not look silently dead on stored frames.
    legacy: bool,
}

/// Frame + run grid for the radar-map layer. Map playback re-requests a
/// frame per step, so the run grid is served from the content-addressed
/// `WorkerState::map_grid` cache — the per-frame ~240 MB full-disk
/// `grid.rwg` read + sha256 only happens when the grid identity actually
/// changes (a different product/sector), never between frames of a run.
fn load_frame_for_map(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<SatMapFrame, String> {
    let colored = load_colored_frame(state, store_root, key, hhmm, true)?;
    let run_dir = store_root.join(&key.model).join(&key.run);
    // Present after any successful load (the frame/run hash agreement check
    // depends on it); the run's grid hash is the cache identity.
    let info = state
        .grids
        .get(&(key.model.clone(), key.run.clone()))
        .ok_or_else(|| format!("{key}: run grid facts missing after frame load"))?;
    let flip_rows = info.flip_rows;
    let hash = info.hash.clone();
    let grid = load_frame_grid(
        state,
        &run_dir,
        &hash,
        colored.frame.image.size[0],
        colored.frame.image.size[1],
    )?;
    Ok(SatMapFrame {
        key: key.clone(),
        hhmm,
        image: colored.frame.image,
        grid,
        flip_rows,
    })
}

/// Open/reuse the full geolocation grid for a map or native-plot frame.
/// The frame hash is authoritative; a stale/mismatched run grid is an error,
/// never a reason to project values against the wrong coordinates.
fn load_frame_grid(
    state: &mut WorkerState,
    run_dir: &Path,
    expected_hash: &str,
    nx: usize,
    ny: usize,
) -> Result<Arc<GridFile>, String> {
    if let Some(cached) = state
        .map_grid
        .as_ref()
        .filter(|grid| grid.hash == expected_hash && grid.nx == nx && grid.ny == ny)
    {
        return Ok(Arc::clone(cached));
    }
    let opened =
        Arc::new(GridFile::open(&run_dir.join("grid.rwg")).map_err(|error| error.to_string())?);
    if opened.hash != expected_hash {
        return Err(format!(
            "frame grid hash {expected_hash} does not match run grid {}",
            opened.hash
        ));
    }
    if opened.nx != nx || opened.ny != ny {
        return Err(format!(
            "run grid {}x{} does not match frame {nx}x{ny}",
            opened.nx, opened.ny
        ));
    }
    state.map_grid = Some(Arc::clone(&opened));
    Ok(opened)
}

/// Read one stored frame as native-plot data. Scalar values and composite
/// colors remain in STORAGE row order; unlike the player path, no north-up
/// row flip occurs here because the renderer receives the matching grid mesh.
fn load_frame_for_plot(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<SatellitePlotSource, String> {
    let run_dir = store_root.join(&key.model).join(&key.run);
    let reader = HourReader::open(&run_dir.join(frame_file_name(hhmm)))
        .map_err(|error| error.to_string())?;
    let meta = reader.meta();
    let (nx, ny) = (meta.nx, meta.ny);
    let grid = load_frame_grid(state, &run_dir, &meta.grid_hash, nx, ny)?;
    let title = format!("{} · {hhmm:04}Z", run_title(&key.model, &key.run));
    let subtitle_left = format!("{}/{}", key.model, key.run);
    let subtitle_right = format!("{} · {hhmm:04}Z", key.model.to_ascii_uppercase());

    if meta
        .variables
        .iter()
        .any(|variable| variable.name == COMPOSITE_R_VAR)
    {
        let r = reader
            .read_full_2d(COMPOSITE_R_VAR)
            .map_err(|error| error.to_string())?;
        let g = reader
            .read_full_2d(COMPOSITE_G_VAR)
            .map_err(|error| error.to_string())?;
        let b = reader
            .read_full_2d(COMPOSITE_B_VAR)
            .map_err(|error| error.to_string())?;
        let expected = nx
            .checked_mul(ny)
            .ok_or_else(|| format!("satellite frame {nx}x{ny} overflows cell count"))?;
        if r.len() != expected || g.len() != expected || b.len() != expected {
            return Err(format!(
                "{key}/t{hhmm:04}: RGB planes do not match {nx}x{ny} frame"
            ));
        }
        let pixels = r
            .into_iter()
            .zip(g)
            .zip(b)
            .map(|((r, g), b)| {
                if r.is_finite() && g.is_finite() && b.is_finite() {
                    rustwx_render::Color::rgba(
                        r.round().clamp(0.0, 255.0) as u8,
                        g.round().clamp(0.0, 255.0) as u8,
                        b.round().clamp(0.0, 255.0) as u8,
                        255,
                    )
                } else {
                    rustwx_render::Color::TRANSPARENT
                }
            })
            .collect();
        return SatellitePlotSource::rgba_from_store(
            title,
            subtitle_left,
            subtitle_right,
            pixels,
            grid,
        );
    }

    let variable = meta
        .variables
        .iter()
        .find(|variable| variable.kind == "surface2d")
        .cloned()
        .ok_or_else(|| format!("{key}/t{hhmm:04} holds no 2D variable"))?;
    let values = reader
        .read_full_2d(&variable.name)
        .map_err(|error| error.to_string())?;
    let band = selector_band(&variable.selector, &variable.name);
    let palette = band.and_then(|band| {
        if variable.name.starts_with("ahi_bt_")
            && (8..=16).contains(&band)
            && legacy_pseudo_bt(&values)
        {
            finite_percentile_range(&values, 0.02, 0.98).map(|(lo, hi)| {
                SatellitePlotPalette::from_remapped_satellite_anchors(
                    lo,
                    hi,
                    DYN_COLD_K,
                    DYN_WARM_K,
                    ENHANCED_IR,
                )
            })
        } else {
            let anchors = if (7..=16).contains(&band) {
                ir_enhancement_anchors(band, state.ir_enhancement)
            } else {
                band_anchors(band)
            };
            Some(SatellitePlotPalette::from_satellite_anchors(anchors))
        }
    });
    SatellitePlotSource::scalar_from_store(
        title,
        subtitle_left,
        subtitle_right,
        variable.name,
        variable.units,
        variable.selector,
        values,
        grid,
        palette,
    )
}

/// Read one stored frame and color it with its band's production palette
/// (NaN off-earth pixels stay transparent).
fn load_frame(
    state: &mut WorkerState,
    store_root: &Path,
    key: &SatRunKey,
    hhmm: u16,
) -> Result<ColoredSatFrame, String> {
    load_colored_frame(state, store_root, key, hhmm, false)
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
        // A run whose frames declare a grid hash we already hold (successive
        // runs write bit-identical grids) yields its facts from the cached
        // map grid — no ~240 MB full-disk re-read at, e.g., midnight
        // rollover. Content equality (sha256) makes flip_rows equal too.
        let info = match state
            .map_grid
            .as_ref()
            .filter(|cached| cached.hash == meta.grid_hash)
        {
            Some(cached) => GridInfo {
                hash: cached.hash.clone(),
                flip_rows: cached.lat_descending() == Some(false),
            },
            None => {
                let grid =
                    GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| err.to_string())?;
                let info = GridInfo {
                    hash: grid.hash.clone(),
                    flip_rows: grid.lat_descending() == Some(false),
                };
                if map_overlay {
                    // The map path needs the full grid right after this
                    // returns (`load_frame_for_map`): keep the copy we just
                    // paid to read instead of opening the file twice.
                    state.map_grid = Some(Arc::new(grid));
                }
                info
            }
        };
        state.grids.insert(grid_key.clone(), info);
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
    let (pixels, legacy) = if is_composite {
        let r = reader
            .read_full_2d(COMPOSITE_R_VAR)
            .map_err(|err| err.to_string())?;
        let g = reader
            .read_full_2d(COMPOSITE_G_VAR)
            .map_err(|err| err.to_string())?;
        let b = reader
            .read_full_2d(COMPOSITE_B_VAR)
            .map_err(|err| err.to_string())?;
        (
            render_composite_pixels(&r, &g, &b, nx, ny, flip_rows),
            false,
        )
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
        render_sat_pixels(
            &name,
            band,
            &values,
            nx,
            ny,
            flip_rows,
            map_overlay,
            state.ir_enhancement,
        )
    };

    Ok(ColoredSatFrame {
        frame: SatFrameImage {
            key: key.clone(),
            hhmm,
            image: ColorImage::new([nx, ny], pixels),
            read_ms: started.elapsed().as_secs_f32() * 1000.0,
        },
        legacy,
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
        let grid_row = if flip_rows {
            ny - 1 - image_row
        } else {
            image_row
        };
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

/// Returns the colored pixels plus whether the frame took the LEGACY
/// pre-calibration stretch (in which case `enhancement` did not apply —
/// callers surface that so the enhancement picker does not look dead).
#[allow(clippy::too_many_arguments)]
fn render_sat_pixels(
    variable_name: &str,
    band: u8,
    values: &[f32],
    nx: usize,
    ny: usize,
    flip_rows: bool,
    map_overlay: bool,
    enhancement: IrEnhancement,
) -> (Vec<Color32>, bool) {
    // Both GOES ABI and (since the true-count block-5 calibration in
    // `ingest_latest_himawari`) Himawari AHI store REAL Kelvin brightness
    // temperature, so every IR band renders through absolute-temperature
    // anchor tables. Frames written by the pre-calibration AHI path hold
    // raw-ish pseudo-BT (~326-330 K flat) that an absolute palette would
    // clamp to one warm color — detect that implausible distribution and
    // keep the old percentile stretch for just those legacy frames.
    //
    // B07 (3.9 µm) is EXEMPT from the detector: daytime shortwave-IR BT
    // legitimately reaches 330-400 K from reflected sunlight, so a
    // correctly calibrated daylight B07 disk can median past the threshold
    // and would silently drop to the percentile stretch (ignoring the
    // selected enhancement, and flickering across a loop as the median
    // crosses it). For bands 8-16 a >320 K median is physically impossible
    // on a real disk, so the heuristic stays. Legacy stored B07 frames now
    // render through the absolute palette and look wrong-ish until they
    // age out of the rolling store — accepted.
    let ir_band = (7..=16).contains(&band);
    if variable_name.starts_with("ahi_bt_") && (8..=16).contains(&band) && legacy_pseudo_bt(values)
    {
        eprintln!(
            "sat: {variable_name} median > {LEGACY_PSEUDO_BT_MEDIAN_K} K (pre-calibration \
             pseudo-BT frame) — falling back to the legacy percentile stretch"
        );
        return (
            render_ahi_legacy_stretch(values, nx, ny, flip_rows, map_overlay),
            true,
        );
    }
    let static_anchors = if ir_band {
        ir_enhancement_anchors(band, enhancement)
    } else {
        band_anchors(band)
    };

    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if flip_rows {
            ny - 1 - image_row
        } else {
            image_row
        };
        for &value in &values[grid_row * nx..(grid_row + 1) * nx] {
            let [r, g, b, a] = anchor_color(value, static_anchors);
            let alpha = if map_overlay && ir_band {
                bt_overlay_alpha(value)
            } else {
                a
            };
            pixels.push(Color32::from_rgba_unmultiplied(r, g, b, alpha));
        }
    }
    (pixels, false)
}

/// Median BT above which a stored AHI "brightness temperature" plane cannot
/// be real Kelvin: an Earth full disk medians ~270-290 K (mostly ocean),
/// while the pre-v0.29.3 pseudo-BT frames cluster flat at ~326-330 K.
const LEGACY_PSEUDO_BT_MEDIAN_K: f32 = 320.0;

/// Whether a stored AHI BT plane predates the true-Kelvin calibration
/// (sampled finite median warmer than any plausible Earth disk).
fn legacy_pseudo_bt(values: &[f32]) -> bool {
    let stride = (values.len() / 4096).max(1);
    let mut sample: Vec<f32> = values
        .iter()
        .step_by(stride)
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    if sample.is_empty() {
        return false;
    }
    let mid = sample.len() / 2;
    let (_, median, _) = sample.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    *median > LEGACY_PSEUDO_BT_MEDIAN_K
}

/// The pre-v0.29.3 Himawari display path: p2..p98 auto-stretch through the
/// colorful span of [`ENHANCED_IR`]. Kept ONLY for stored frames written
/// before AHI IR was calibrated to true Kelvin (see [`render_sat_pixels`])
/// and for the before/after proof export — new ingests never take it.
fn render_ahi_legacy_stretch(
    values: &[f32],
    nx: usize,
    ny: usize,
    flip_rows: bool,
    map_overlay: bool,
) -> Vec<Color32> {
    let Some((lo, hi)) = finite_percentile_range(values, 0.02, 0.98) else {
        return vec![Color32::TRANSPARENT; nx * ny];
    };
    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if flip_rows {
            ny - 1 - image_row
        } else {
            image_row
        };
        for &value in &values[grid_row * nx..(grid_row + 1) * nx] {
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
        }
    }
    pixels
}

/// Coldest/warmest pseudo-Kelvin the legacy Himawari stretch maps its
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

/// User-selectable IR enhancement for Kelvin brightness-temperature bands
/// (GOES ABI and Himawari AHI bands 7-16). `Cimss` keeps the established
/// per-band behavior ([`ENHANCED_IR`] on the longwave window, production
/// palettes elsewhere); the others are the classic NOAA absolute-temperature
/// enhancement curves, applied to every IR band on both satellites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IrEnhancement {
    /// CIMSS-style rainbow on 13-15, production palettes elsewhere (default).
    #[default]
    Cimss,
    /// NESDIS BD curve — the stepped Dvorak tropical-cyclone enhancement.
    Bd,
    /// NOAA/SSD AVN color IR enhancement.
    Avn,
    /// NOAA/SSD Funktop (Ted Funk precipitation) enhancement.
    Funktop,
    /// NOAA/SSD RB rainbow IR enhancement.
    Rainbow,
    /// Plain unenhanced IR grayscale (cold = white).
    Grayscale,
}

impl IrEnhancement {
    pub const ALL: [IrEnhancement; 6] = [
        Self::Cimss,
        Self::Bd,
        Self::Avn,
        Self::Funktop,
        Self::Rainbow,
        Self::Grayscale,
    ];

    /// Stable settings key (persisted in `AppSettings::sat_ir_enhancement`).
    pub fn slug(self) -> &'static str {
        match self {
            Self::Cimss => "cimss",
            Self::Bd => "bd",
            Self::Avn => "avn",
            Self::Funktop => "funktop",
            Self::Rainbow => "rainbow",
            Self::Grayscale => "gray",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cimss => "CIMSS ramp (default)",
            Self::Bd => "BD curve (Dvorak)",
            Self::Avn => "AVN",
            Self::Funktop => "Funktop",
            Self::Rainbow => "Rainbow",
            Self::Grayscale => "Grayscale",
        }
    }

    /// Parse a settings slug; unknown values keep the default enhancement.
    pub fn parse(value: &str) -> Self {
        let normalized = value.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|enhancement| enhancement.slug() == normalized)
            .unwrap_or_default()
    }
}

/// The anchor table an IR band renders through for the given enhancement.
/// `Cimss` preserves the per-band production behavior; every other curve is
/// an absolute-Kelvin table shared by all IR bands (the classic curves are
/// defined on the longwave window, but every ABI/AHI band 7-16 is Kelvin BT,
/// so they apply cleanly — shortwave 3.9 µm solar-contaminated pixels above
/// the warm anchor simply clamp dark).
fn ir_enhancement_anchors(band: u8, enhancement: IrEnhancement) -> rw_sat::palette::Anchors {
    match enhancement {
        IrEnhancement::Cimss => {
            enhanced_anchors_for_band(band).unwrap_or_else(|| band_anchors(band))
        }
        IrEnhancement::Bd => BD_CURVE,
        IrEnhancement::Avn => AVN_IR,
        IrEnhancement::Funktop => FUNKTOP_IR,
        IrEnhancement::Rainbow => RAINBOW_IR,
        IrEnhancement::Grayscale => GRAYSCALE_IR,
    }
}

/// NESDIS BD curve (Dvorak Enhanced-IR tropical-cyclone enhancement) over
/// brightness temperature. Breakpoints are the canonical NESDIS gray-shade
/// boundaries used by the Dvorak/ADT technique — OFF WHITE ramp to -30.2 °C,
/// then DG -30.2..-41.2, MG -41.2..-53.2, LG -53.2..-63.2, B -63.2..-69.6,
/// W -69.6..-75.2, CMG -75.2..-80.2, CDG colder than -80.2 (Dvorak 1984 /
/// Velden-Olander-Zehr; rounded ranges documented at
/// <https://tropic.ssec.wisc.edu/misc/other/faq/faq_enhance.html>, AWIPS
/// recreation at <https://rammb.cira.colostate.edu/training/visit/training_sessions/goes_enhancement_color_tables_in_awips/web/ins_BD.html>).
/// Gray levels are transcribed from the operational NOAA/SSD BD colorbar
/// (GOES-West tropical loops). Steps are HARD: duplicate anchor values pin
/// each bin flat, and an exact boundary temperature belongs to the COLDER
/// bin (tested).
const BD_CURVE: rw_sat::palette::Anchors = &[
    (164.15, [88, 88, 88]),    // -109.0 C: cold dark gray floor
    (192.95, [88, 88, 88]),    //  -80.2 C: CDG (repeat gray, <= -80.2)
    (192.95, [136, 136, 136]), //  -80.2..-75.2 C: cold medium gray
    (197.95, [136, 136, 136]),
    (197.95, [255, 255, 255]), //  -75.2..-69.6 C: white
    (203.55, [255, 255, 255]),
    (203.55, [0, 0, 0]), //  -69.6..-63.2 C: black
    (209.95, [0, 0, 0]),
    (209.95, [160, 160, 160]), //  -63.2..-53.2 C: light gray
    (219.95, [160, 160, 160]),
    (219.95, [112, 112, 112]), //  -53.2..-41.2 C: medium gray
    (231.95, [112, 112, 112]),
    (231.95, [64, 64, 64]), //  -41.2..-30.2 C: dark gray
    (242.95, [64, 64, 64]),
    (242.95, [200, 200, 200]), //  -30.2 -> +9.0 C: off-white scene ramp
    (282.15, [110, 110, 110]),
    (282.15, [255, 255, 255]), //   +9.0 -> +28.0 C: warm repeat ramp
    (301.15, [0, 0, 0]),
    (330.0, [0, 0, 0]), // hottest surface stays black
];

/// NOAA/SSD AVN color IR enhancement (aviation/tropical): warm grayscale
/// ramp, then blue → yellow → orange → red cold-cloud steps, an anvil-core
/// gray, and white for the coldest overshoots. Breakpoints/colors transcribed
/// from the operational NOAA/SSD "AVNCOLOR IR" product colorbar (GOES-West
/// tropical Pacific loops, e.g. <https://www.ssd.noaa.gov/goes/west/tpac/h5-loop-avn.html>),
/// calibrated against the BD colorbar's known NESDIS breakpoints.
const AVN_IR: rw_sat::palette::Anchors = &[
    (163.15, [255, 255, 255]), // -110.0 C
    (170.15, [255, 255, 255]), // -103.0 C: coldest overshoot white
    (170.15, [88, 88, 88]),    // -103.0..-78.0 C: anvil-core gray
    (195.15, [88, 88, 88]),
    (195.15, [240, 0, 0]), //  -78.0..-70.5 C: red
    (202.65, [240, 0, 0]),
    (202.65, [200, 118, 10]),  //  -70.5 C: step to orange…
    (218.65, [250, 183, 0]),   //  -54.5 C: …brightening warmward
    (218.65, [250, 250, 5]),   //  -54.5 C: step to yellow…
    (234.65, [160, 158, 0]),   //  -38.5 C: …darkening warmward
    (234.65, [0, 120, 175]),   //  -38.5 C: step to blue…
    (258.65, [0, 158, 245]),   //  -14.5 C: …brightening warmward
    (258.65, [255, 255, 255]), // -14.5 C: step to the warm grayscale ramp
    (281.65, [130, 130, 130]), //  +8.5 C
    (305.15, [0, 0, 0]),       // +32.0 C: warm surface black
];

/// NOAA/SSD Funktop enhancement (Ted Funk, precipitation/tropical analysis):
/// warm grayscale, yellow-olive, navy → cyan, dark red, pink, then green
/// fading to white at the coldest tops. Breakpoints/colors transcribed from
/// the operational NOAA/SSD "Funktop" product colorbar (GOES-West tropical
/// Pacific loops, e.g. <https://www.ssd.noaa.gov/goes/west/tpac/h5-loop-ft.html>),
/// calibrated against the BD colorbar's known NESDIS breakpoints.
const FUNKTOP_IR: rw_sat::palette::Anchors = &[
    (163.15, [250, 250, 250]), // -110.0 C: deep-cold white
    (182.15, [235, 250, 235]), //  -91.0 C
    (195.15, [0, 255, 20]),    //  -78.0 C: bright green
    (195.15, [255, 133, 133]), //  -78.0 C: step to pink…
    (202.65, [255, 85, 85]),   //  -70.5 C: …deepening warmward
    (202.65, [240, 0, 0]),     //  -70.5 C: step to red…
    (215.15, [75, 0, 0]),      //  -58.0 C: …darkening warmward
    (215.15, [10, 240, 255]),  //  -58.0 C: step to cyan…
    (234.65, [5, 10, 125]),    //  -38.5 C: …to navy warmward
    (234.65, [245, 240, 0]),   //  -38.5 C: step to yellow…
    (254.15, [100, 100, 0]),   //  -19.0 C: …to olive warmward
    (254.15, [222, 222, 222]), //  -19.0 C: step to the warm grayscale ramp
    (305.15, [30, 30, 30]),    //  +32.0 C
    (320.15, [0, 0, 0]),       //  +47.0 C: hottest surface black
];

/// NOAA/SSD RB "rainbow" IR enhancement: a smooth magenta → blue → cyan →
/// green → yellow → orange → red sweep from warm to cold, a white band near
/// -87..-90 °C, and a repeat dark-to-white ramp for the very coldest tops.
/// Transcribed from the operational NOAA/SSD "RBTOP" product colorbar
/// (GOES-West tropical Pacific loops), calibrated against the BD colorbar's
/// known NESDIS breakpoints.
const RAINBOW_IR: rw_sat::palette::Anchors = &[
    (164.15, [255, 255, 255]), // -109.0 C: repeat ramp ends white
    (182.65, [10, 10, 10]),    //  -90.5 C: repeat ramp starts near black
    (182.65, [250, 250, 252]), //  -90.5..-86.5 C: white band
    (186.65, [250, 250, 252]),
    (186.65, [240, 5, 0]),   //  -86.5 C: brightest red
    (196.15, [190, 0, 2]),   //  -77.0 C
    (204.65, [122, 0, 0]),   //  -68.5 C: darkest red-brown
    (212.15, [150, 57, 0]),  //  -61.0 C
    (221.15, [185, 112, 4]), //  -52.0 C: orange-brown
    (230.15, [210, 170, 0]), //  -43.0 C
    (240.15, [252, 252, 0]), //  -33.0 C: yellow peak
    (249.15, [160, 200, 5]), //  -24.0 C
    (259.15, [12, 120, 10]), //  -14.0 C: green
    (268.15, [0, 180, 115]), //   -5.0 C
    (277.15, [0, 250, 250]), //   +4.0 C: cyan
    (289.15, [0, 105, 175]), //  +16.0 C
    (295.65, [0, 5, 120]),   //  +22.5 C: deep blue
    (305.15, [140, 0, 195]), //  +32.0 C: magenta (warm clamp)
];

/// Plain unenhanced IR grayscale: cold cloud tops white, warm surface black.
const GRAYSCALE_IR: rw_sat::palette::Anchors = &[
    (173.15, [255, 255, 255]), // -100 C
    (330.0, [0, 0, 0]),        //  +57 C
];

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
pub(crate) fn ahi_scan_angles_to_lat_lon(
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
/// `mesh` is an optional precomputed `(lat, lon)` navigation mesh: the
/// full-disk mesh is multi-second and ~60-120 MB at downsample 4 (~1 GB
/// at 1), and the IR ingest already computes it for off-earth masking —
/// passing it in bakes the EXACT mesh the mask used instead of navigating
/// the whole disk a second time. `None` navigates here as before.
fn write_himawari_grid_frame(
    store_root: &Path,
    field: &SatelliteGridField,
    written_unix: u64,
    mesh: Option<(Vec<f32>, Vec<f32>)>,
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
    if let Some((lat, lon)) = &mesh
        && (lat.len() != nx.saturating_mul(ny) || lon.len() != nx.saturating_mul(ny))
    {
        return Err(format!(
            "precomputed AHI mesh {}x{} does not match grid {nx}x{ny}",
            lat.len(),
            lon.len()
        ));
    }
    let (lat, lon) = mesh.unwrap_or_else(|| ahi_lat_lon_mesh(scene));
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
    let mut field = assemble_hsd_segments(
        &paths,
        HimawariValueMode::BrightnessTemperature,
        spec.downsample.max(1),
    )
    .map_err(|err| err.to_string())?;
    // IR bands: replace rw-sat's BT plane (computed from right-shifted
    // counts, so it lands flat at ~326-330 K — see ahi_true_counts_on_grid)
    // with TRUE Kelvin from the raw counts + block-5 calibration, the same
    // true-count fix the visible composite path uses. Stored AHI IR is real
    // brightness temperature from here on and renders through the same
    // absolute-Kelvin palettes as GOES.
    // The sweep=y navigation mesh is computed ONCE here (multi-second and
    // ~60-120 MB at downsample 4): it masks off-earth IR pixels below AND
    // is handed to the writer as the frame's baked geometry, so mask and
    // stored mesh cannot diverge.
    let mesh = ahi_lat_lon_mesh(&field.scene);
    if (7..=16).contains(&band) {
        let true_counts = ahi_true_counts_on_grid(&paths, spec.downsample.max(1))?;
        if true_counts.len() != field.values.len() {
            return Err(format!(
                "AHI B{band:02} true-count length {} does not match the assembled grid {}",
                true_counts.len(),
                field.values.len()
            ));
        }
        let header = inspect_hsd_file(&paths[0]).map_err(|err| err.to_string())?;
        let calibration = header
            .calibration
            .ok_or_else(|| format!("AHI B{band:02} HSD header carries no calibration block #5"))?;
        field.values = ahi_counts_to_brightness_temperature(&true_counts, &calibration)?;
        // Mask off-earth pixels: a few limb/space counts pass the sentinel
        // check but their scan angles miss the earth (near-zero radiance,
        // BT < 100 K noise). The navigation mesh is authoritative — blank
        // anything it cannot geolocate so absolute-Kelvin palettes render
        // clean space instead of cold speckle blocks.
        for (value, lat) in field.values.iter_mut().zip(&mesh.0) {
            if !lat.is_finite() {
                *value = f32::NAN;
            }
        }
    }
    let field = field;
    let nx = field.scene.fixed_grid.nx;
    let ny = field.scene.fixed_grid.ny;
    // AHI is CF sweep_angle_axis "y": write through the local sweep=y
    // writer so the stored mesh is real AHI navigation (rw-sat's writer
    // navigates with the GOES "x" convention; see write_himawari_grid_frame),
    // reusing the mesh computed for the mask above.
    let frame = write_himawari_grid_frame(
        store_root,
        &field,
        Utc::now().timestamp().max(0) as u64,
        Some(mesh),
    )?;
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

/// AHI full-disk scans arrive every 10 minutes; round `now` down to the slot.
/// (Local mirror of rw-sat's private `round_down_scan_time`.)
fn round_down_ahi_scan_time(time: DateTime<Utc>, cadence_minutes: i64) -> DateTime<Utc> {
    let cadence = cadence_minutes.max(1) as u32;
    let minute = time.minute() - (time.minute() % cadence);
    time.with_second(0)
        .and_then(|t| t.with_nanosecond(0))
        .and_then(|t| t.with_minute(minute))
        .unwrap_or(time)
}

/// The picked scan's per-band segment objects (the subrange the composite
/// fetches), all sharing one scan time.
struct HimawariScanPick {
    scan_time: DateTime<Utc>,
    prefix: String,
    /// band -> its segment objects, ordered by segment index.
    by_band: HashMap<u8, Vec<S3Object>>,
}

/// Newest AHI full-disk scan for which EVERY required visible band has all of
/// segments `seg_start..seg_start+seg_count` present. All bands of one scan
/// share the same `HS_H09_<date>_<time>` filename timestamp, so the scan keys
/// line up exactly across bands (mirrors GOES [`latest_common_scan`]).
fn latest_himawari_visible_scan(
    satellite: HimawariSatellite,
    bands: &[u8],
    seg_start: u8,
    seg_count: u8,
    lookback_minutes: i64,
    as_of: DateTime<Utc>,
) -> Result<HimawariScanPick, String> {
    let agent = build_agent();
    let product = HimawariProduct::AhiL1bFldk;
    let cadence = product.cadence_minutes();
    let lookback = lookback_minutes.max(cadence);
    let mut scan_time = round_down_ahi_scan_time(as_of, cadence);
    let stop = scan_time - chrono::Duration::minutes(lookback);
    let wanted: Vec<u8> = (seg_start..seg_start.saturating_add(seg_count)).collect();

    while scan_time >= stop {
        let prefix = product.scan_prefix(scan_time);
        let objects = list_s3_objects(&agent, satellite.bucket(), &prefix, None)
            .map_err(|err| err.to_string())?;
        let mut by_band: HashMap<u8, Vec<(u8, S3Object)>> = HashMap::new();
        for object in objects {
            let Some(name) = parse_segment_name(object_filename(&object.key)) else {
                continue;
            };
            if name.satellite != satellite
                || name.scan_time != scan_time
                || !bands.contains(&name.band)
                || !wanted.contains(&name.segment_index)
            {
                continue;
            }
            by_band
                .entry(name.band)
                .or_default()
                .push((name.segment_index, object));
        }
        let complete = bands.iter().all(|band| {
            by_band.get(band).is_some_and(|segs| {
                let mut idxs: Vec<u8> = segs.iter().map(|(idx, _)| *idx).collect();
                idxs.sort_unstable();
                idxs.dedup();
                idxs == wanted
            })
        });
        if complete {
            let mut out: HashMap<u8, Vec<S3Object>> = HashMap::new();
            for (band, mut segs) in by_band {
                segs.sort_by_key(|(idx, _)| *idx);
                out.insert(band, segs.into_iter().map(|(_, object)| object).collect());
            }
            return Ok(HimawariScanPick {
                scan_time,
                prefix,
                by_band: out,
            });
        }
        scan_time -= chrono::Duration::minutes(cadence);
    }

    Err(format!(
        "no {} AHI scan in the last {} min has bands {:?} for segments S{:02}..S{:02}",
        satellite.slug(),
        lookback,
        bands,
        seg_start,
        seg_start.saturating_add(seg_count).saturating_sub(1)
    ))
}

/// One staged AHI HSD segment's decode plan (the header pass of
/// [`ahi_true_counts_on_grid`]): everything the strided row reads need,
/// with the parsed header itself dropped.
struct AhiSegmentPlan {
    path: PathBuf,
    sequence: u8,
    columns: usize,
    lines: usize,
    data_start: u64,
    little: bool,
    /// Block-5 sentinel counts decoded to NaN (error / outside-scan).
    sentinels: (u16, u16),
}

/// Local row indices of one segment that survive the GLOBAL stride: of the
/// concatenated grid's rows `offset..offset + lines`, those whose global
/// index is a multiple of `step` — exactly the rows concatenate-then-
/// `step_by(step)` keeps, however the segment heights fall against the
/// stride.
fn ahi_strided_local_rows(offset: usize, lines: usize, step: usize) -> Vec<usize> {
    let step = step.max(1);
    let first = offset.div_ceil(step) * step;
    (first..offset + lines)
        .step_by(step)
        .map(|global| global - offset)
        .collect()
}

/// Assemble true raw counts from a band's staged segments onto rw-sat's
/// downsampled grid — segments sorted by sequence number, concatenated
/// row-major, then stride-`downsample` subsampled exactly as rw-sat's
/// `assemble_hsd_segments` + `downsample_satellite_field` do, so the values
/// line up 1:1 with the [`SatelliteGridField`] grid we keep for geolocation.
///
/// The 16-bit read bypasses rw-sat's `HimawariValueMode::Count`, which
/// right-shifts every value by `bits_per_pixel - valid_bits_per_pixel` (5 for
/// the visible bands). That shift assumes the count is stored LEFT-justified,
/// but the NOAA-hosted HSD files store it RIGHT-justified (a live H09 B01
/// segment holds raw values like 19/20/110 — not multiples of 32), so
/// rw-sat's count comes out 32× too small and clouds render black. The JMA
/// HSD User's Guide (v1.3) §4 stores the `valid_bits_per_pixel`-bit count in
/// the low bits, so the raw 16-bit value IS the count (outside the
/// error/outside-scan sentinels). Verified on live data: with this read a
/// Cat-5 eyewall reaches reflectance ~1 (white); rw-sat's shifted count
/// capped the whole disk near black.
///
/// Memory/IO discipline: only the strided rows are ever read (one seek per
/// selected row), so a full-disk B03 at stride 8 touches ~120 MB of its
/// ~1.9 GB of segment data and the largest full-resolution allocation
/// anywhere is a single row (44 KB) — what makes the whole-disk composite
/// feasible on the follow thread.
fn ahi_true_counts_on_grid(paths: &[PathBuf], downsample: usize) -> Result<Vec<f32>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let step = downsample.max(1);
    // Header pass: sequence order, shape, sentinels, data offsets.
    let mut segments: Vec<AhiSegmentPlan> = Vec::with_capacity(paths.len());
    for path in paths {
        let header = inspect_hsd_file(path).map_err(|err| err.to_string())?;
        let calibration = header
            .calibration
            .as_ref()
            .ok_or("AHI HSD segment is missing calibration block #5")?;
        segments.push(AhiSegmentPlan {
            path: path.clone(),
            sequence: header
                .segment
                .as_ref()
                .map(|segment| segment.sequence_number)
                .unwrap_or(0),
            columns: usize::from(header.data.columns),
            lines: usize::from(header.data.lines),
            data_start: u64::from(header.total_header_length),
            little: matches!(
                header.byte_order,
                rw_sat::himawari::HimawariByteOrder::Little
            ),
            sentinels: (
                calibration.error_pixel_count,
                calibration.outside_scan_count,
            ),
        });
    }
    segments.sort_by_key(|segment| segment.sequence);
    let nx = segments
        .first()
        .map(|segment| segment.columns)
        .ok_or("no AHI segments to assemble")?;
    let xs: Vec<usize> = (0..nx).step_by(step).collect();
    let total_lines: usize = segments.iter().map(|segment| segment.lines).sum();
    let mut out = Vec::with_capacity(total_lines.div_ceil(step).saturating_mul(xs.len()));
    let mut offset = 0usize;
    let mut row = vec![0u8; nx * 2];
    for segment in &segments {
        if segment.columns != nx {
            return Err("inconsistent AHI segment width".to_string());
        }
        let mut file = std::fs::File::open(&segment.path).map_err(|err| err.to_string())?;
        let needed = segment.data_start + (segment.lines * nx * 2) as u64;
        let have = file.metadata().map_err(|err| err.to_string())?.len();
        if have < needed {
            return Err(format!(
                "AHI HSD data block short in {}: need {} bytes, have {}",
                segment.path.display(),
                needed,
                have
            ));
        }
        for local in ahi_strided_local_rows(offset, segment.lines, step) {
            file.seek(SeekFrom::Start(
                segment.data_start + (local * nx * 2) as u64,
            ))
            .map_err(|err| err.to_string())?;
            file.read_exact(&mut row).map_err(|err| err.to_string())?;
            for &x in &xs {
                let o = x * 2;
                let raw = if segment.little {
                    u16::from_le_bytes([row[o], row[o + 1]])
                } else {
                    u16::from_be_bytes([row[o], row[o + 1]])
                };
                out.push(
                    if raw == segment.sentinels.0 || raw == segment.sentinels.1 {
                        f32::NAN
                    } else {
                        f32::from(raw)
                    },
                );
            }
        }
        offset += segment.lines;
    }
    Ok(out)
}

/// Download and stage (bunzip) one band's HSD segments, returning the
/// staged raw paths. The download/manifest/staging half of the band fetch,
/// shared by the full-sector assemble and the native-window assemble.
#[allow(clippy::too_many_arguments)]
fn stage_himawari_band_segments(
    satellite: HimawariSatellite,
    scan_time: DateTime<Utc>,
    prefix: &str,
    band: u8,
    objects: &[S3Object],
    cache_root: &Path,
    source_root: &Path,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let agent = build_agent();
    let manifest_dir = source_root.join("manifest");
    let raw_dir = source_root.join("raw");
    std::fs::create_dir_all(&manifest_dir).map_err(|err| err.to_string())?;

    let mut manifest_segments = Vec::with_capacity(objects.len());
    let mut total_bytes = 0_u64;
    for object in objects {
        let name = parse_segment_name(object_filename(&object.key))
            .ok_or_else(|| format!("unparseable Himawari key {}", object.key))?;
        send(SatResponse::DownloadStarted {
            id: object.key.clone(),
            label: format!(
                "{} B{:02} S{:02}/{:02}",
                satellite.platform(),
                name.band,
                name.segment_index,
                name.segment_count
            ),
            bytes: object.size_bytes,
        });
        let started = Instant::now();
        let downloaded = download_object(&agent, satellite.bucket(), cache_root, object, true)
            .map_err(|err| err.to_string())?;
        send(SatResponse::DownloadDone {
            id: object.key.clone(),
            ms: started.elapsed().as_millis(),
            cache_hit: downloaded.cache_hit,
        });
        total_bytes = total_bytes.saturating_add(object.size_bytes);
        manifest_segments.push(HimawariManifestSegment {
            band: name.band,
            segment_index: name.segment_index,
            segment_count: name.segment_count,
            product: name.product.clone(),
            resolution: name.resolution.clone(),
            key: object.key.clone(),
            url: object_url(satellite.bucket(), &object.key),
            last_modified: object.last_modified.clone(),
            size_bytes: object.size_bytes,
            cache_path: downloaded.path.display().to_string(),
            cache_hit: downloaded.cache_hit,
        });
    }

    let manifest_path = manifest_dir.join(format!(
        "{}_ahi-l1b-fldk_b{band:02}_{}.json",
        satellite.slug(),
        scan_time.format("%Y%m%dT%H%M%SZ")
    ));
    let manifest = HimawariDownloadManifest {
        schema: HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA.to_string(),
        satellite: satellite.slug().to_string(),
        platform: satellite.platform().to_string(),
        bucket: satellite.bucket().to_string(),
        product: HimawariProduct::AhiL1bFldk.slug().to_string(),
        scan_time_utc: scan_time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        prefix: prefix.to_string(),
        band,
        segments_downloaded: manifest_segments.len(),
        segments_available: manifest_segments.len(),
        source_complete: true,
        allow_partial: false,
        total_downloaded_bytes: total_bytes,
        cache_root: cache_root.display().to_string(),
        segments: manifest_segments,
    };
    std::fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;

    let staged =
        stage_download_manifest(&manifest_path, &raw_dir).map_err(|err| err.to_string())?;
    Ok(staged
        .segments
        .iter()
        .map(|segment| PathBuf::from(&segment.raw_path))
        .collect::<Vec<_>>())
}

/// Download one band's staged segments and assemble them into a raw-count
/// [`SatelliteGridField`], returning the band's HSD calibration alongside. The
/// grid/scene (geolocation) comes from rw-sat's `assemble_hsd_segments`; the
/// values are replaced with the *true* raw counts from [`ahi_true_counts_on_grid`]
/// (see there for why rw-sat's own counts are unusable).
/// Reflectance is derived downstream via [`ahi_counts_to_reflectance`].
///
/// With a `window`, the whole full-segment assemble is skipped for
/// [`assemble_ahi_window_counts`]: only the window's pixels are decoded, at
/// native resolution (`downsample` is not applied to windowed fetches).
/// With `full_disk`, the assemble is [`assemble_ahi_fulldisk_counts`]:
/// the same strided true-count read plus a header-built scene, so a
/// ten-segment band never materializes a native-resolution plane.
#[allow(clippy::too_many_arguments)]
fn fetch_himawari_band_counts(
    satellite: HimawariSatellite,
    scan_time: DateTime<Utc>,
    prefix: &str,
    band: u8,
    objects: &[S3Object],
    cache_root: &Path,
    source_root: &Path,
    downsample: usize,
    window: Option<SatNativeWindow>,
    full_disk: bool,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<(SatelliteGridField, HimawariCalibrationInfo), String> {
    let paths = stage_himawari_band_segments(
        satellite,
        scan_time,
        prefix,
        band,
        objects,
        cache_root,
        source_root,
        send,
    )?;
    if let Some(window) = window {
        return assemble_ahi_window_counts(&paths, window);
    }
    if full_disk {
        return assemble_ahi_fulldisk_counts(&paths, downsample);
    }
    // rw-sat's assemble gives us the correct grid/scene (geolocation), but its
    // Count values are right-shifted (see ahi_true_counts_on_grid); replace them
    // with the true raw counts read on the same downsampled grid.
    let mut field = assemble_hsd_segments(&paths, HimawariValueMode::Count, downsample.max(1))
        .map_err(|err| err.to_string())?;
    let true_counts = ahi_true_counts_on_grid(&paths, downsample.max(1))?;
    if true_counts.len() != field.values.len() {
        return Err(format!(
            "AHI B{band:02} true-count length {} does not match the assembled grid {}",
            true_counts.len(),
            field.values.len()
        ));
    }
    field.values = true_counts;
    let header = inspect_hsd_file(&paths[0]).map_err(|err| err.to_string())?;
    let calibration = header
        .calibration
        .ok_or_else(|| format!("AHI B{band:02} HSD header carries no calibration block #5"))?;
    Ok((field, calibration))
}

/// Pixel padding applied to every native-window crop: keeps the bilinear
/// cross-band resample fed right up to the window edge and absorbs
/// sub-pixel rect-vs-axis rounding.
const WINDOW_CROP_PAD_PX: usize = 2;

/// HSD Modified Julian Day → UTC (epoch 1858-11-17T00:00Z). Local mirror of
/// rw-sat's private `mjd_to_datetime`, needed because the windowed assemble
/// builds its scene straight from HSD headers.
fn ahi_mjd_to_datetime(mjd: f64) -> Result<DateTime<Utc>, String> {
    if !mjd.is_finite() {
        return Err(format!("invalid HSD MJD {mjd}"));
    }
    let millis = (mjd * 86_400_000.0).round();
    if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
        return Err(format!("HSD MJD {mjd} out of range"));
    }
    let epoch = chrono::NaiveDate::from_ymd_opt(1858, 11, 17)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or("failed to construct the MJD epoch")?;
    let naive = epoch
        .checked_add_signed(chrono::Duration::milliseconds(millis as i64))
        .ok_or_else(|| format!("HSD MJD {mjd} out of range"))?;
    Ok(DateTime::from_naive_utc_and_offset(naive, Utc))
}

/// HSD satellite name → store model slug ("Himawari-9" → "h9"), matching
/// rw-sat's private `himawari_model_slug` so windowed runs land in the same
/// model dir as full-sector runs.
fn ahi_model_slug(name: &str) -> String {
    match name.trim() {
        "Himawari-8" => "h8".to_string(),
        "Himawari-9" => "h9".to_string(),
        value => value.to_ascii_lowercase().replace([' ', '-'], ""),
    }
}

/// HSD observation area → sector slug ("FLDK" → "fulldisk"), matching
/// rw-sat's private `himawari_sector_slug` so header-built scenes stamp the
/// same sector families rw-sat's assembler does.
fn ahi_sector_slug(area: &str) -> String {
    match area {
        "FLDK" => "fulldisk".to_string(),
        value if value.starts_with("JP") => "japan".to_string(),
        value if value.starts_with("R3") => "target".to_string(),
        value if value.starts_with("R4") => "landmark4".to_string(),
        value if value.starts_with("R5") => "landmark5".to_string(),
        value => value.to_ascii_lowercase(),
    }
}

/// Sector token for an assembled segment set, matching rw-sat's
/// `assemble_hsd_segments` naming: the plain area slug when the set is the
/// whole scan (every segment, starting at line 1) — `fulldisk`, the token
/// the whole-disk composite's run family hangs off — else the subset form
/// `fulldisk_s04_05of10` today's target-region runs carry.
fn ahi_sector_token(
    area: &str,
    first_seq: u8,
    last_seq: u8,
    total_segments: u8,
    complete_from_line_one: bool,
) -> String {
    let base = ahi_sector_slug(area);
    if complete_from_line_one {
        base
    } else {
        format!("{base}_s{first_seq:02}_{last_seq:02}of{total_segments:02}")
    }
}

/// The window's scan-angle rect under the CF sweep=y AHI navigation.
fn ahi_window_rect(
    height_m: f64,
    semi_major_m: f64,
    semi_minor_m: f64,
    sub_lon_deg: f64,
    window: SatNativeWindow,
) -> Option<ScanAngleRect> {
    window_scan_angle_rect(window, |lat, lon| {
        ahi_lat_lon_to_scan_angles(height_m, semi_major_m, semi_minor_m, sub_lon_deg, lat, lon)
    })
}

/// Which contiguous full-disk segments a window needs, from the NOMINAL
/// Himawari geometry (this runs before any file exists locally; the pixel
/// crop afterwards uses the downloaded header's own projection block).
fn himawari_window_segments(window: SatNativeWindow) -> Result<(u8, u8), String> {
    let rect = ahi_window_rect(
        AHI_NOMINAL_HEIGHT_M,
        AHI_NOMINAL_SEMI_MAJOR_M,
        AHI_NOMINAL_SEMI_MINOR_M,
        AHI_NOMINAL_SUB_LON_DEG,
        window,
    )
    .ok_or_else(|| {
        format!(
            "window {} is outside Himawari's view of the earth",
            window.run_slug()
        )
    })?;
    Ok(ahi_fldk_segment_range(&rect))
}

/// Assemble ONLY the window's pixels from a band's staged HSD segments, at
/// native resolution: parse each header, decode the cropped row/column
/// block of TRUE raw counts (the same right-justified 16-bit read as
/// [`ahi_true_counts_on_grid`] — see there for why rw-sat's shifted counts
/// are unusable), and build the scene with cropped scan-angle axes from the
/// header's own projection block #3. A full segment is only ever held as
/// raw bytes; the f32 arrays stay window-sized, which is what makes
/// downsample-1 (B03: 22000-pixel rows) tractable on the follow thread.
///
/// The scene's `sector` carries the window token
/// (`fulldisk_<win…>`), so windowed frames open their own run-dir family
/// and successive scans of the same window loop in the player.
/// Read the contiguous byte span of segment rows
/// `first_row..first_row + row_count` (zero-based within the segment's data
/// block, rows of `columns` big/little-endian u16 samples) by seeking past
/// the header and everything north of the span. Validates the file holds
/// all `seg_lines` declared rows first, so a truncated download fails as
/// loudly as the old whole-file read did.
fn read_ahi_row_span(
    path: &Path,
    data_start: u64,
    columns: usize,
    seg_lines: usize,
    first_row: usize,
    row_count: usize,
) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    if first_row + row_count > seg_lines {
        return Err(format!(
            "AHI row span {first_row}+{row_count} exceeds the segment's {seg_lines} lines"
        ));
    }
    let row_bytes = columns as u64 * 2;
    let needed = data_start + seg_lines as u64 * row_bytes;
    let mut file = std::fs::File::open(path).map_err(|err| err.to_string())?;
    let have = file.metadata().map_err(|err| err.to_string())?.len();
    if have < needed {
        return Err(format!(
            "AHI HSD data block short in {}: need {} bytes, have {}",
            path.display(),
            needed,
            have
        ));
    }
    file.seek(SeekFrom::Start(data_start + first_row as u64 * row_bytes))
        .map_err(|err| err.to_string())?;
    let mut bytes = vec![0u8; row_count * columns * 2];
    file.read_exact(&mut bytes).map_err(|err| err.to_string())?;
    Ok(bytes)
}

fn assemble_ahi_window_counts(
    paths: &[PathBuf],
    window: SatNativeWindow,
) -> Result<(SatelliteGridField, HimawariCalibrationInfo), String> {
    if paths.is_empty() {
        return Err("no staged AHI segments to window".to_string());
    }
    let mut headers = Vec::with_capacity(paths.len());
    for path in paths {
        let header = inspect_hsd_file(path).map_err(|err| err.to_string())?;
        headers.push((path.clone(), header));
    }
    headers.sort_by_key(|(_, header)| {
        header
            .segment
            .as_ref()
            .map(|segment| segment.sequence_number)
            .unwrap_or(0)
    });

    let first = &headers[0].1;
    let projection = first
        .projection
        .as_ref()
        .ok_or("AHI HSD header is missing projection block #3")?;
    let calibration = first
        .calibration
        .clone()
        .ok_or("AHI HSD header is missing calibration block #5")?;
    let first_segment = first
        .segment
        .as_ref()
        .ok_or("AHI HSD header is missing segment block #7")?;
    let band = u8::try_from(calibration.band_number)
        .map_err(|_| format!("unsupported AHI band {}", calibration.band_number))?;
    let columns = usize::from(first.data.columns);
    let first_line = u32::from(first_segment.first_line_number);

    // Contiguity + shape checks across the fetched segments (mirrors
    // rw-sat's assemble validation).
    let mut expected_first_line = first_line;
    let mut total_lines = 0usize;
    for (_, header) in &headers {
        if usize::from(header.data.columns) != columns {
            return Err("inconsistent AHI segment width".to_string());
        }
        let info = header
            .segment
            .as_ref()
            .ok_or("AHI HSD header is missing segment block #7")?;
        if u32::from(info.first_line_number) != expected_first_line {
            return Err(format!(
                "AHI segments are not contiguous: expected first line {expected_first_line}, \
                 got {} in S{:02}",
                info.first_line_number, info.sequence_number
            ));
        }
        expected_first_line += u32::from(header.data.lines);
        total_lines += usize::from(header.data.lines);
    }

    let rect = ahi_window_rect(
        (projection.satellite_distance_km - projection.equatorial_radius_km) * 1000.0,
        projection.equatorial_radius_km * 1000.0,
        projection.polar_radius_km * 1000.0,
        projection.sub_lon_degrees,
        window,
    )
    .ok_or_else(|| {
        format!(
            "window {} is outside Himawari's view of the earth",
            window.run_slug()
        )
    })?;
    let crop = ahi_window_crop(
        f64::from(projection.cfac),
        f64::from(projection.coff),
        f64::from(projection.lfac),
        f64::from(projection.loff),
        columns,
        first_line,
        total_lines,
        &rect,
        WINDOW_CROP_PAD_PX,
    )?;

    // Decode the cropped block, segment by segment, north to south.
    let crop_last_line = crop.line_start + crop.line_count as u32 - 1;
    let mut values = Vec::with_capacity(crop.line_count.saturating_mul(crop.col_count));
    for (path, header) in &headers {
        let seg_first = u32::from(
            header
                .segment
                .as_ref()
                .expect("validated above")
                .first_line_number,
        );
        let seg_last = seg_first + u32::from(header.data.lines) - 1;
        let overlap_first = crop.line_start.max(seg_first);
        let overlap_last = crop_last_line.min(seg_last);
        if overlap_first > overlap_last {
            continue;
        }
        let sentinels = header
            .calibration
            .as_ref()
            .map(|cal| (cal.error_pixel_count, cal.outside_scan_count))
            .unwrap_or((
                calibration.error_pixel_count,
                calibration.outside_scan_count,
            ));
        // Read ONLY the overlapping row span: a window needs 10-30 MB of
        // rows out of a ~97 MB full-resolution segment, so seeking to the
        // contiguous span keeps the fetch-side memory/IO window-sized too.
        let bytes = read_ahi_row_span(
            path,
            u64::from(header.total_header_length),
            columns,
            usize::from(header.data.lines),
            (overlap_first - seg_first) as usize,
            (overlap_last - overlap_first + 1) as usize,
        )?;
        let little = matches!(
            header.byte_order,
            rw_sat::himawari::HimawariByteOrder::Little
        );
        for line in overlap_first..=overlap_last {
            let row = (line - overlap_first) as usize;
            let row_start = row * columns * 2;
            for col in crop.col_start..crop.col_start + crop.col_count {
                let o = row_start + col * 2;
                let raw = if little {
                    u16::from_le_bytes([bytes[o], bytes[o + 1]])
                } else {
                    u16::from_be_bytes([bytes[o], bytes[o + 1]])
                };
                values.push(if raw == sentinels.0 || raw == sentinels.1 {
                    f32::NAN
                } else {
                    f32::from(raw)
                });
            }
        }
    }
    if values.len() != crop.line_count * crop.col_count {
        return Err(format!(
            "AHI window decode produced {} values for a {}x{} crop",
            values.len(),
            crop.col_count,
            crop.line_count
        ));
    }

    let last = &headers[headers.len() - 1].1;
    let start_time_utc = ahi_mjd_to_datetime(first.observation_start_mjd)?;
    let end_time_utc = ahi_mjd_to_datetime(last.observation_end_mjd)?;
    let scene = SatelliteGridScene {
        model: ahi_model_slug(&first.satellite_name),
        satellite: first.satellite_name.clone(),
        provider: "jma".to_string(),
        instrument: "ahi".to_string(),
        product: format!("AHI-L1b-{}", first.observation_area),
        sector: format!("fulldisk_{}", window.run_slug()),
        band,
        layer: format!("count_c{band:02}"),
        source_variable: "HSD count".to_string(),
        start_time_utc,
        end_time_utc,
        projection: SatelliteProjection {
            perspective_point_height_m: (projection.satellite_distance_km
                - projection.equatorial_radius_km)
                * 1000.0,
            semi_major_axis_m: projection.equatorial_radius_km * 1000.0,
            semi_minor_axis_m: projection.polar_radius_km * 1000.0,
            longitude_of_projection_origin_deg: projection.sub_lon_degrees,
            // Mirrors rw-sat's assembler stamp; every consumer of these
            // scenes navigates through the local CF sweep=y path regardless
            // (see write_himawari_grid_frame).
            sweep_angle_axis: SweepAngleAxis::X,
        },
        fixed_grid: AbiFixedGrid {
            nx: crop.col_count,
            ny: crop.line_count,
            x_scan_rad: crop.x_scan_rad,
            y_scan_rad: crop.y_scan_rad,
        },
        metadata: serde_json::json!({
            "source_format": "himawari_standard_data",
            "value_mode": "count",
            "downsample": 1,
            "native_window": {
                "center_lat_deg": window.center_lat_deg,
                "center_lon_deg": window.center_lon_deg,
                "size_km": window.size_km,
                "col_start": crop.col_start,
                "line_start": crop.line_start,
            },
        }),
    };
    Ok((
        SatelliteGridField {
            scene,
            variable_name: format!("ahi_count_c{band:02}"),
            units: "count".to_string(),
            values,
        },
        calibration,
    ))
}

/// Per-band decode stride for the FULL-DISK composite: bands finer than the
/// 1 km B01/B02 base double the stride so every band decodes straight to
/// ~base resolution (B03 is AHI's only sub-km band, 0.5 km / 22000² native);
/// the bilinear cross-band resample then only corrects the sub-pixel
/// registration offset instead of reducing a 4×-the-pixels plane.
fn ahi_fulldisk_band_stride(band: u8, base_stride: usize) -> usize {
    if band == 3 {
        base_stride.saturating_mul(2)
    } else {
        base_stride
    }
}

/// Assemble a band's staged full-disk segments as TRUE raw counts on a
/// stride-`downsample` grid, without ever materializing a native-resolution
/// plane: values come from the same strided read the target-region composite
/// uses ([`ahi_true_counts_on_grid`]), while the scene is built straight
/// from the HSD headers the way [`assemble_ahi_window_counts`] does. The
/// scan-angle axes are byte-identical to rw-sat's `assemble_hsd_segments` +
/// downsample (the CGMS mapping at every stride-th column/line), so
/// successive scans reuse one run dir and the baked lat/lon mesh is the
/// established AHI navigation.
///
/// rw-sat's assembler is unusable at this scale: it decodes every segment to
/// full-resolution f32 before decimating, which for a ten-segment B03 is
/// two ~1.9 GB buffers. This assemble peaks at one 44 KB row plus the
/// strided output plane (~30 MB at the default stride).
fn assemble_ahi_fulldisk_counts(
    paths: &[PathBuf],
    downsample: usize,
) -> Result<(SatelliteGridField, HimawariCalibrationInfo), String> {
    if paths.is_empty() {
        return Err("no staged AHI segments to assemble".to_string());
    }
    let step = downsample.max(1);
    let mut headers = Vec::with_capacity(paths.len());
    for path in paths {
        let header = inspect_hsd_file(path).map_err(|err| err.to_string())?;
        headers.push(header);
    }
    headers.sort_by_key(|header| {
        header
            .segment
            .as_ref()
            .map(|segment| segment.sequence_number)
            .unwrap_or(0)
    });

    let first = &headers[0];
    let projection = first
        .projection
        .as_ref()
        .ok_or("AHI HSD header is missing projection block #3")?;
    let calibration = first
        .calibration
        .clone()
        .ok_or("AHI HSD header is missing calibration block #5")?;
    let first_segment = first
        .segment
        .as_ref()
        .ok_or("AHI HSD header is missing segment block #7")?;
    let band = u8::try_from(calibration.band_number)
        .map_err(|_| format!("unsupported AHI band {}", calibration.band_number))?;
    let columns = usize::from(first.data.columns);
    let first_line = u32::from(first_segment.first_line_number);
    let total_segments = first_segment.total_segments;

    // Contiguity + shape checks across the fetched segments (mirrors
    // rw-sat's assemble validation, like the window assemble does).
    let mut expected_first_line = first_line;
    let mut total_lines = 0usize;
    for header in &headers {
        if usize::from(header.data.columns) != columns {
            return Err("inconsistent AHI segment width".to_string());
        }
        let info = header
            .segment
            .as_ref()
            .ok_or("AHI HSD header is missing segment block #7")?;
        if u32::from(info.first_line_number) != expected_first_line {
            return Err(format!(
                "AHI segments are not contiguous: expected first line {expected_first_line}, \
                 got {} in S{:02}",
                info.first_line_number, info.sequence_number
            ));
        }
        expected_first_line += u32::from(header.data.lines);
        total_lines += usize::from(header.data.lines);
    }

    let values = ahi_true_counts_on_grid(paths, step)?;

    // Strided scan-angle axes: the CGMS normalized geostationary mapping at
    // every stride-th column/line, byte-matching rw-sat's (private)
    // `himawari_column_scan_rad` / `himawari_line_scan_rad` + `step_by`.
    let cfac = f64::from(projection.cfac);
    let coff = f64::from(projection.coff);
    let lfac = f64::from(projection.lfac);
    let loff = f64::from(projection.loff);
    let x_scan_rad: Vec<f64> = (0..columns)
        .step_by(step)
        .map(|col| ((col as f64 + 1.0 - coff) * 65_536.0 / cfac).to_radians())
        .collect();
    let y_scan_rad: Vec<f64> = (0..total_lines)
        .step_by(step)
        .map(|row| ((loff - f64::from(first_line + row as u32)) * 65_536.0 / lfac).to_radians())
        .collect();
    let (nx, ny) = (x_scan_rad.len(), y_scan_rad.len());
    if values.len() != nx.saturating_mul(ny) {
        return Err(format!(
            "AHI full-disk decode produced {} values for a {nx}x{ny} grid",
            values.len()
        ));
    }

    let complete_from_line_one = headers.len() == usize::from(total_segments) && first_line == 1;
    let first_seq = first_segment.sequence_number;
    let last_seq = headers
        .last()
        .and_then(|header| header.segment.as_ref())
        .map(|segment| segment.sequence_number)
        .unwrap_or(first_seq);
    let sector = ahi_sector_token(
        &first.observation_area,
        first_seq,
        last_seq,
        total_segments,
        complete_from_line_one,
    );

    let last = &headers[headers.len() - 1];
    let start_time_utc = ahi_mjd_to_datetime(first.observation_start_mjd)?;
    let end_time_utc = ahi_mjd_to_datetime(last.observation_end_mjd)?;
    let scene = SatelliteGridScene {
        model: ahi_model_slug(&first.satellite_name),
        satellite: first.satellite_name.clone(),
        provider: "jma".to_string(),
        instrument: "ahi".to_string(),
        product: format!("AHI-L1b-{}", first.observation_area),
        sector,
        band,
        layer: format!("count_c{band:02}"),
        source_variable: "HSD count".to_string(),
        start_time_utc,
        end_time_utc,
        projection: SatelliteProjection {
            perspective_point_height_m: (projection.satellite_distance_km
                - projection.equatorial_radius_km)
                * 1000.0,
            semi_major_axis_m: projection.equatorial_radius_km * 1000.0,
            semi_minor_axis_m: projection.polar_radius_km * 1000.0,
            longitude_of_projection_origin_deg: projection.sub_lon_degrees,
            // Mirrors rw-sat's assembler stamp; every consumer of these
            // scenes navigates through the local CF sweep=y path regardless
            // (see write_himawari_grid_frame).
            sweep_angle_axis: SweepAngleAxis::X,
        },
        fixed_grid: AbiFixedGrid {
            nx,
            ny,
            x_scan_rad,
            y_scan_rad,
        },
        metadata: serde_json::json!({
            "source_format": "himawari_standard_data",
            "value_mode": "count",
            "downsample": step,
            "segments": headers.len(),
        }),
    };
    Ok((
        SatelliteGridField {
            scene,
            variable_name: format!("ahi_count_c{band:02}"),
            units: "count".to_string(),
            values,
        },
        calibration,
    ))
}

/// Convert AHI raw counts to TRUE brightness temperature (Kelvin) with the
/// segment's block-5 infrared calibration (JMA HSD User's Guide v1.3 §4.4):
/// radiance = `slope · count + intercept` [W/(m² sr µm)], the effective
/// blackbody temperature Te from the inverse Planck function at the band's
/// central wavelength (block-5 physical constants), then the brightness
/// temperature `Tb = c0 + c1·Te + c2·Te²` (block-5 correction coefficients).
/// Same scheme rw-sat's BrightnessTemperature mode implements — but fed the
/// *true* right-justified counts from [`ahi_true_counts_on_grid`] instead of
/// rw-sat's shifted ones, which push radiance toward the intercept and land
/// BT flat at ~326-330 K (what forced the old display-side percentile hack).
/// Non-finite counts (error / off-disk) and non-physical radiance stay NaN.
fn ahi_counts_to_brightness_temperature(
    counts: &[f32],
    calibration: &HimawariCalibrationInfo,
) -> Result<Vec<f32>, String> {
    let constants = calibration.physical_constants.as_ref().ok_or_else(|| {
        format!(
            "AHI B{:02} block-5 calibration carries no infrared Planck constants",
            calibration.band_number
        )
    })?;
    let slope = calibration.count_to_radiance_slope;
    let intercept = calibration.count_to_radiance_intercept;
    let [c0, c1, c2] = calibration.planck_or_albedo_coefficients;
    let wavelength_m = calibration.central_wavelength_um * 1.0e-6;
    let planck_lead = 2.0 * constants.planck_constant_j_s * constants.speed_of_light_m_s.powi(2);
    let planck_term = constants.planck_constant_j_s * constants.speed_of_light_m_s
        / constants.boltzmann_constant_j_k;
    Ok(counts
        .iter()
        .map(|&count| {
            if !count.is_finite() {
                return f32::NAN;
            }
            let radiance_per_um = slope * f64::from(count) + intercept;
            if !(radiance_per_um.is_finite() && radiance_per_um > 0.0) {
                return f32::NAN;
            }
            let radiance_per_m = radiance_per_um * 1.0e6;
            let log_arg = planck_lead / (radiance_per_m * wavelength_m.powi(5)) + 1.0;
            if !(log_arg.is_finite() && log_arg > 1.0) {
                return f32::NAN;
            }
            let effective = planck_term / (wavelength_m * log_arg.ln());
            let brightness = c0 + c1 * effective + c2 * effective * effective;
            if brightness.is_finite() {
                brightness as f32
            } else {
                f32::NAN
            }
        })
        .collect())
}

/// Convert AHI raw counts to reflectance (albedo) using the HSD visible
/// calibration (JMA HSD User's Guide §4.3): radiance = `slope · count +
/// intercept`, reflectance = `c' · radiance`, where `c'` is the block-5
/// count-to-albedo coefficient (`planck_or_albedo_coefficients[0]`; verified
/// ≈ 0.00156 for a live H09 B01 segment). Non-finite counts (error / off-disk
/// pixels) stay NaN → transparent; negative radiance (dark ocean / night)
/// clamps to 0 → opaque near-black.
fn ahi_counts_to_reflectance(counts: &[f32], calibration: &HimawariCalibrationInfo) -> Vec<f32> {
    let slope = calibration.count_to_radiance_slope;
    let intercept = calibration.count_to_radiance_intercept;
    let cprime = calibration.planck_or_albedo_coefficients[0];
    counts
        .iter()
        .map(|&count| {
            if !count.is_finite() {
                return f32::NAN;
            }
            let radiance = slope * f64::from(count) + intercept;
            ((radiance * cprime).max(0.0)) as f32
        })
        .collect()
}

/// Whether two AHI fixed grids are the same scan-angle mesh (identity
/// resample). Mirrors rw-sat composite's `same_fixed_grid`.
fn same_ahi_fixed_grid(a: &AbiFixedGrid, b: &AbiFixedGrid) -> bool {
    a.nx == b.nx
        && a.ny == b.ny
        && a.x_scan_rad.len() == b.x_scan_rad.len()
        && a.y_scan_rad.len() == b.y_scan_rad.len()
        && a.x_scan_rad
            .iter()
            .zip(&b.x_scan_rad)
            .all(|(p, q)| (p - q).abs() <= 1.0e-12)
        && a.y_scan_rad
            .iter()
            .zip(&b.y_scan_rad)
            .all(|(p, q)| (p - q).abs() <= 1.0e-12)
}

/// Resample a band's values onto the base band's fixed grid (bilinear in
/// scan-angle space; identity when the grids already match). This is how the
/// 0.5 km B03 red lands on the 1 km base and vice versa — the AHI counterpart
/// of rw-sat's [`values_on_base_grid`], which is typed to GOES scenes only.
fn resample_ahi_to_base(
    src_grid: &AbiFixedGrid,
    src_values: &[f32],
    base_grid: &AbiFixedGrid,
) -> Vec<f32> {
    if same_ahi_fixed_grid(src_grid, base_grid) {
        return src_values.to_vec();
    }
    let x_map: Vec<Option<(usize, usize, f32)>> = base_grid
        .x_scan_rad
        .iter()
        .map(|&value| bracket_axis(&src_grid.x_scan_rad, value))
        .collect();
    let y_map: Vec<Option<(usize, usize, f32)>> = base_grid
        .y_scan_rad
        .iter()
        .map(|&value| bracket_axis(&src_grid.y_scan_rad, value))
        .collect();
    let mut out = vec![f32::NAN; base_grid.nx.saturating_mul(base_grid.ny)];
    for (j, y_bracket) in y_map.iter().enumerate() {
        let Some((j0, j1, fy)) = *y_bracket else {
            continue;
        };
        for (i, x_bracket) in x_map.iter().enumerate() {
            let Some((i0, i1, fx)) = *x_bracket else {
                continue;
            };
            let idx = |yy: usize, xx: usize| yy * src_grid.nx + xx;
            out[j * base_grid.nx + i] = bilinear_f32(
                src_values[idx(j0, i0)],
                src_values[idx(j0, i1)],
                src_values[idx(j1, i0)],
                src_values[idx(j1, i1)],
                fx,
                fy,
            );
        }
    }
    out
}

/// Reflectance (0..~1) → display channel byte, matching GOES `visible_component`
/// (gamma 2.2 over 0..1). Kept as f32 so it drops straight into the shared
/// `rgb_r/g/b` composite planes.
fn ahi_visible_component(reflectance: f32) -> f32 {
    let scaled = reflectance.clamp(0.0, 1.0).powf(1.0 / 2.2);
    (scaled * 255.0).round().clamp(0.0, 255.0)
}

/// The three baked composite planes `(rgb_r, rgb_g, rgb_b)` (NaN = transparent).
type CompositePlanes = (Vec<f32>, Vec<f32>, Vec<f32>);

/// Compose AHI true color from per-band reflectance planes (all already on the
/// base grid). Any band non-finite at a pixel → transparent; otherwise each
/// display channel is the gamma-mapped reflectance of its assigned band. The
/// three returned planes are the same `rgb_r/g/b` f32 layout the GOES
/// composite path introduced (NaN = transparent).
fn compose_ahi_true_color(
    style: HimawariCompositeStyle,
    planes: &HashMap<u8, Vec<f32>>,
    len: usize,
) -> Result<CompositePlanes, String> {
    let (red_band, green_band, blue_band) = style.rgb_bands();
    let red = planes
        .get(&red_band)
        .ok_or_else(|| format!("missing B{red_band:02} plane"))?;
    let green = planes
        .get(&green_band)
        .ok_or_else(|| format!("missing B{green_band:02} plane"))?;
    let blue = planes
        .get(&blue_band)
        .ok_or_else(|| format!("missing B{blue_band:02} plane"))?;
    if red.len() < len || green.len() < len || blue.len() < len {
        return Err("AHI composite plane shorter than the base grid".to_string());
    }
    let (mut r, mut g, mut b) = (
        Vec::with_capacity(len),
        Vec::with_capacity(len),
        Vec::with_capacity(len),
    );
    for idx in 0..len {
        let (rv, gv, bv) = (red[idx], green[idx], blue[idx]);
        if rv.is_finite() && gv.is_finite() && bv.is_finite() {
            r.push(ahi_visible_component(rv));
            g.push(ahi_visible_component(gv));
            b.push(ahi_visible_component(bv));
        } else {
            r.push(f32::NAN);
            g.push(f32::NAN);
            b.push(f32::NAN);
        }
    }
    Ok((r, g, b))
}

/// The generic satellite selector for a baked AHI RGB-composite frame: the
/// base band on the AHI sweep=y projection, plus a `composite` block naming
/// the style and its source bands (mirrors [`composite_selector`] and
/// [`himawari_selector`]).
fn himawari_composite_selector(
    scene: &SatelliteGridScene,
    style: HimawariCompositeStyle,
) -> serde_json::Value {
    let (red_band, green_band, blue_band) = style.rgb_bands();
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
            // The base band's grid the planes live on: B01 (1 km) for full
            // sectors, B03 (0.5 km) for native windows.
            "band": scene.band,
            "layer": format!("rgb_{}", style.slug()),
            "source_variable": "HSD count",
            "composite": {
                "style": style.slug(),
                "title": style.title(),
                "bands": [red_band, green_band, blue_band],
            },
            "scan_start_utc": scene.start_time_utc.to_rfc3339(),
            "scan_end_utc": scene.end_time_utc.to_rfc3339(),
            "projection": projection,
        }
    })
}

/// Write a baked AHI RGB composite as one store frame: three `rgb_r/g/b` f32
/// planes on the AHI sweep=y per-pixel mesh (see [`ahi_lat_lon_mesh`] for why
/// AHI is navigated locally, not through rw-sat's GOES-convention writer),
/// following the same store contract as [`write_himawari_grid_frame`] /
/// [`write_goes_composite_frame`]. Composite runs are
/// `<sector>_rgb_<style>_<YYYYMMDD>`.
#[allow(clippy::too_many_arguments)]
fn write_himawari_composite_frame(
    store_root: &Path,
    scene: &SatelliteGridScene,
    style: HimawariCompositeStyle,
    r: &[f32],
    g: &[f32],
    b: &[f32],
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    let model = sanitize_store_token(&scene.model);
    let sector = sanitize_store_token(&scene.sector);
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
    let (lat, lon) = ahi_lat_lon_mesh(scene);
    let shape = GridShape::new(nx, ny).map_err(|err| err.to_string())?;
    let grid = LatLonGrid::new(shape, lat, lon).map_err(|err| err.to_string())?;

    // Reuse the run dir whose stored grid is bit-identical, else take the
    // first free suffixed name — the store rule that keeps grid changes honest.
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
    let selector = himawari_composite_selector(scene, style);
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
        variable: COMPOSITE_R_VAR.to_string(),
    })
}

/// Fetch the co-registered AHI visible bands the composite needs (the newest
/// scan that has the requested full-disk segment range for all of them),
/// convert each to reflectance, resample onto the base band's grid, compose
/// AHI true color per pixel, and write one composite frame into the sat store.
/// This is the Himawari counterpart of [`ingest_latest_goes_composite`]; it
/// reuses the same `rgb_r/g/b` frame storage so the frames play through the
/// existing composite render path.
fn ingest_latest_himawari_composite(
    store_root: &Path,
    spec: &HimawariCompositeSpec,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<String, String> {
    let satellite = HimawariSatellite::parse(&spec.satellite)
        .ok_or_else(|| format!("unknown Himawari satellite '{}'", spec.satellite))?;
    let style = HimawariCompositeStyle::parse(&spec.style)
        .ok_or_else(|| format!("unknown Himawari composite style '{}'", spec.style))?;
    let bands = style.required_bands();
    let window = spec.window.map(SatNativeWindow::clamped);
    // The native window wins over the full-disk scope: a window composes on
    // the finest band's grid at stride 1 and fetches only the segments it
    // intersects. Full disk fetches every segment and assembles lean; the
    // target-region path keeps the configured segment range and decimation.
    let full_disk = spec.full_disk && window.is_none();
    let base_band = match window {
        Some(_) => style.native_base_band(),
        None => style.base_band(),
    };
    let downsample = if window.is_some() {
        1
    } else if full_disk {
        // Never below stride 2 on the whole disk: stride 1 would put the
        // 11000² B01 base (~480 MB per f32 plane, several such planes at
        // once) on the follow thread for no display gain. Stride 2 is the
        // 5500² / ~2 km option; the default 4 is 2750² / ~4 km.
        spec.downsample.max(2)
    } else {
        spec.downsample.max(1)
    };
    let (seg_start, seg_count) = match window {
        Some(window) => himawari_window_segments(window)?,
        None if full_disk => (1, 10), // the whole disk: S01..S10
        None => {
            let start = spec.segment_start.clamp(1, 10);
            (start, spec.segment_count.clamp(1, 11 - start))
        }
    };
    let cache_root = store_root.join("cache");
    let source_root = store_root.join("sources").join("himawari");

    let pick = latest_himawari_visible_scan(
        satellite,
        bands,
        seg_start,
        seg_count,
        spec.lookback_minutes.max(10),
        spec.as_of.unwrap_or_else(Utc::now),
    )?;

    // Fetch + assemble each band as raw counts on its native grid, keeping the
    // per-band calibration for the reflectance conversion.
    let mut fields: HashMap<u8, SatelliteGridField> = HashMap::with_capacity(bands.len());
    let mut calibrations: HashMap<u8, HimawariCalibrationInfo> =
        HashMap::with_capacity(bands.len());
    for &band in bands {
        let objects = pick
            .by_band
            .get(&band)
            .ok_or_else(|| format!("the picked scan is missing AHI B{band:02}"))?;
        // Full disk decodes sub-km bands at a doubled stride so every band
        // lands directly at ~base resolution (see ahi_fulldisk_band_stride).
        let band_downsample = if full_disk {
            ahi_fulldisk_band_stride(band, downsample)
        } else {
            downsample
        };
        let (field, calibration) = fetch_himawari_band_counts(
            satellite,
            pick.scan_time,
            &pick.prefix,
            band,
            objects,
            &cache_root,
            &source_root,
            band_downsample,
            window,
            full_disk,
            send,
        )?;
        fields.insert(band, field);
        calibrations.insert(band, calibration);
    }

    // Base grid = the base band's grid (1 km B01 for full sectors, 0.5 km
    // B03 for native windows); resample every band's reflectance onto it,
    // then compose true color per pixel.
    let base_scene = fields
        .get(&base_band)
        .ok_or_else(|| format!("composite base band B{base_band:02} was not fetched"))?
        .scene
        .clone();
    let base_grid = base_scene.fixed_grid.clone();
    let (nx, ny) = (base_grid.nx, base_grid.ny);
    let len = nx.saturating_mul(ny);

    let mut planes: HashMap<u8, Vec<f32>> = HashMap::with_capacity(bands.len());
    for &band in bands {
        // Take the band's counts out of the map so each count plane frees
        // as soon as its reflectance lands on the base grid — at the 2 km
        // full disk that is ~121 MB per band that would otherwise stack.
        let field = fields
            .remove(&band)
            .ok_or_else(|| format!("composite band B{band:02} was not fetched"))?;
        let reflectance = ahi_counts_to_reflectance(&field.values, &calibrations[&band]);
        let on_base = resample_ahi_to_base(&field.scene.fixed_grid, &reflectance, &base_grid);
        planes.insert(band, on_base);
    }
    let (r, g, b) = compose_ahi_true_color(style, &planes, len)?;

    let frame = write_himawari_composite_frame(
        store_root,
        &base_scene,
        style,
        &r,
        &g,
        &b,
        Utc::now().timestamp().max(0) as u64,
    )?;

    for objects in pick.by_band.values() {
        for object in objects {
            send(SatResponse::FrameWritten {
                id: object.key.clone(),
                run: frame.run.clone(),
                hhmm: frame.hhmm,
                bytes: frame.bytes,
                encode_ms: frame.encode_ms,
            });
        }
    }
    send(SatResponse::Runs(scan_runs(store_root)));
    send(SatResponse::SelectFrame {
        key: SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        },
        hhmm: frame.hhmm,
    });

    let lit = r.iter().filter(|value| value.is_finite()).count();
    Ok(format!(
        "Himawari {} {} {}: scan {} · S{:02}..S{:02} · {}x{} · {:.0}% lit · wrote {}/{}/t{:04}",
        satellite.platform(),
        base_scene.sector,
        style.title(),
        pick.scan_time.format("%Y-%m-%d %H:%MZ"),
        seg_start,
        seg_start + seg_count - 1,
        nx,
        ny,
        100.0 * lit as f64 / len.max(1) as f64,
        frame.model,
        frame.run,
        frame.hhmm
    ))
}

/// Native-window single-band Himawari IR ingest (the tropical-card "🛰 IR"
/// path): fetch only the full-disk segments covering the window, decode only
/// the window's pixels at stride 1 ([`assemble_ahi_window_counts`] via
/// [`fetch_himawari_band_counts`]), convert the true raw counts to real
/// Kelvin BT with the header's block-5 calibration (the same fix
/// [`ingest_latest_himawari`] applies), and write ONE single-band frame.
/// The stored plane is brightness temperature, so the IR-enhancement picker
/// (BD, AVN, …) recolors it live at load time; the run family carries the
/// window token (`fulldisk_<win…>_c13_<day>`) so successive scans of the
/// same storm window loop in the player.
fn ingest_latest_himawari_ir_window(
    store_root: &Path,
    spec: &HimawariIrWindowSpec,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<String, String> {
    let satellite = HimawariSatellite::parse(&spec.satellite)
        .ok_or_else(|| format!("unknown Himawari satellite '{}'", spec.satellite))?;
    let band = spec.band;
    if !(7..=16).contains(&band) {
        return Err(format!(
            "AHI B{band:02} is not an IR band (7-16) — the IR window ingest stores Kelvin BT"
        ));
    }
    let window = spec.window.clamped();
    let (seg_start, seg_count) = himawari_window_segments(window)?;
    let cache_root = store_root.join("cache");
    let source_root = store_root.join("sources").join("himawari");

    let pick = latest_himawari_visible_scan(
        satellite,
        &[band],
        seg_start,
        seg_count,
        spec.lookback_minutes.max(10),
        spec.as_of.unwrap_or_else(Utc::now),
    )?;
    let objects = pick
        .by_band
        .get(&band)
        .ok_or_else(|| format!("the picked scan is missing AHI B{band:02}"))?;

    let (mut field, calibration) = fetch_himawari_band_counts(
        satellite,
        pick.scan_time,
        &pick.prefix,
        band,
        objects,
        &cache_root,
        &source_root,
        1,
        Some(window),
        false,
        send,
    )?;
    field.values = ahi_counts_to_brightness_temperature(&field.values, &calibration)?;
    // Re-stamp the count-mode field as calibrated BT so the load path
    // renders it through the absolute-Kelvin IR enhancements (variable
    // naming mirrors rw-sat's `HimawariValueMode::BrightnessTemperature`).
    field.variable_name = format!("ahi_bt_c{band:02}");
    field.units = "K".to_string();
    field.scene.layer = format!("bt_c{band:02}");
    field.scene.source_variable = "HSD count -> BT (block-5 calibration)".to_string();
    if let Some(mode) = field.scene.metadata.get_mut("value_mode") {
        *mode = serde_json::json!("brightness_temperature");
    }

    // The navigation mesh masks the few limb/space pixels a padded window
    // crop can include AND becomes the frame's baked geometry (same
    // mask-equals-stored-mesh discipline as the full-disk IR ingest).
    let mesh = ahi_lat_lon_mesh(&field.scene);
    for (value, lat) in field.values.iter_mut().zip(&mesh.0) {
        if !lat.is_finite() {
            *value = f32::NAN;
        }
    }
    let (nx, ny) = (field.scene.fixed_grid.nx, field.scene.fixed_grid.ny);
    let frame = write_himawari_grid_frame(
        store_root,
        &field,
        Utc::now().timestamp().max(0) as u64,
        Some(mesh),
    )?;

    for object in objects {
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
    Ok(format!(
        "Himawari {} B{band:02} IR window {}: scan {} · {}x{} @ native res · wrote {}/{}/t{:04}",
        satellite.platform(),
        window.run_slug(),
        pick.scan_time.format("%Y-%m-%d %H:%MZ"),
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
    not_after: Option<DateTime<Utc>>,
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
                    || not_after.is_some_and(|cutoff| parsed.start_time_utc > cutoff)
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

/// Decode ONLY the pixels of a GOES CMI file covering `window`, at native
/// resolution: scene axes → scan-angle rect (rw-sat's sweep=x forward
/// navigation) → NetCDF hyperslab read. The whole-sector array is never
/// materialized, so a 0.5 km C02 native window stays window-sized in
/// memory. The cropped scene keeps its own scan-angle axes (its lat/lon
/// mesh is exactly the matching slice of the full sector's), and the
/// sector name carries the window token so windowed frames get their own
/// run-dir family.
fn read_goes_abi_window(path: &Path, window: SatNativeWindow) -> Result<GoesAbiField, String> {
    let scene = read_goes_abi_scene(path).map_err(|err| err.to_string())?;
    let projection = &scene.projection;
    let rect = window_scan_angle_rect(window, |lat, lon| {
        lat_lon_to_scan_angles_fast(
            projection.perspective_point_height_m,
            projection.semi_major_axis_m,
            projection.semi_minor_axis_m,
            projection.longitude_of_projection_origin_deg,
            projection.sweep_angle_axis,
            lat,
            lon,
        )
    })
    .ok_or_else(|| {
        format!(
            "window {} is outside this satellite's view of the earth",
            window.run_slug()
        )
    })?;
    let (x_start, x_count) = axis_crop_range(
        &scene.fixed_grid.x_scan_rad,
        rect.x_min,
        rect.x_max,
        WINDOW_CROP_PAD_PX,
    )
    .ok_or_else(|| format!("window {} misses this sector east-west", window.run_slug()))?;
    let (y_start, y_count) = axis_crop_range(
        &scene.fixed_grid.y_scan_rad,
        rect.y_min,
        rect.y_max,
        WINDOW_CROP_PAD_PX,
    )
    .ok_or_else(|| {
        format!(
            "window {} misses this sector north-south",
            window.run_slug()
        )
    })?;
    let mut field = read_goes_abi_field_window(path, "CMI", x_start, x_count, y_start, y_count)
        .map_err(|err| err.to_string())?;
    field.scene.sector = AbiSector::Unknown(format!(
        "{}_{}",
        sector_slug(&field.scene.sector),
        window.run_slug()
    ));
    Ok(field)
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
    let window = spec.window.map(SatNativeWindow::clamped);
    // Native windows decode at stride 1 (the crop is what keeps them small).
    let downsample = if window.is_some() {
        1
    } else {
        spec.downsample.max(1)
    };
    let cache_dir = store_root.join("cache");

    // Recent hour prefixes to scan for the newest all-band scan.
    let now = spec.as_of.unwrap_or_else(Utc::now);
    let hour_span = (spec.lookback_minutes.max(20) / 60) + 2;
    let hours: Vec<DateTime<Utc>> = (0..hour_span)
        .map(|i| now - chrono::Duration::hours(i))
        .collect();

    let (scan_start, objects) =
        latest_common_scan(&bucket, abi_product, &satellite, &bands, &hours, spec.as_of)?;

    // Download + decode every required band: the native window decodes only
    // its hyperslab at stride 1, the full sector decodes whole then decimates.
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
        let field = match window {
            Some(window) => read_goes_abi_window(&downloaded.path, window)?,
            None => downsample_field(
                read_goes_abi_field(&downloaded.path, "CMI").map_err(|err| err.to_string())?,
                downsample,
            ),
        };
        fields.insert(band, field);
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
        // Carries the window token for native-window runs.
        sector_slug(&base_scene.sector),
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

/// Bake an IR-enhancement palette over a Kelvin BT plane into three
/// `rgb_r/g/b` planes (`[0, 255]` f32) for a baked `_rgb_` frame. NaN
/// (off-earth / fill) stays NaN in all three planes — the composite render
/// path shows those pixels transparent, matching the vis composites'
/// behavior on the map. Returns the planes plus the count of lit pixels.
fn bake_ir_planes(
    values: &[f32],
    band: u8,
    enhancement: IrEnhancement,
) -> (Vec<f32>, Vec<f32>, Vec<f32>, usize) {
    let anchors = ir_enhancement_anchors(band, enhancement);
    let len = values.len();
    let (mut r, mut g, mut b) = (
        Vec::with_capacity(len),
        Vec::with_capacity(len),
        Vec::with_capacity(len),
    );
    let mut lit = 0usize;
    for &value in values {
        if value.is_finite() {
            let [red, green, blue, _] = anchor_color(value, anchors);
            r.push(f32::from(red));
            g.push(f32::from(green));
            b.push(f32::from(blue));
            lit += 1;
        } else {
            r.push(f32::NAN);
            g.push(f32::NAN);
            b.push(f32::NAN);
        }
    }
    (r, g, b, lit)
}

/// Native-window GOES IR ingest (the tropical-card "🛰 IR" path): download
/// the latest whole-sector CMI file for `spec.band`, decode ONLY the
/// window's hyperslab at stride 1 ([`read_goes_abi_window`] — the v0.29.3
/// native-window machinery), color the Kelvin BT through `enhancement`
/// ([`ir_enhancement_anchors`], the same tables the live render uses), and
/// write the result as a baked `_rgb_` frame
/// (`<sector>_<win…>_rgb_ir<band>_<day>`).
///
/// Baked-vs-live tradeoff: see [`GoesIrWindowSpec`]. The enhancement used
/// is stamped in the frame selector's `enhanced_ir` block.
fn ingest_latest_goes_ir_window(
    store_root: &Path,
    spec: &GoesIrWindowSpec,
    enhancement: IrEnhancement,
    send: &impl Fn(SatResponse) -> bool,
) -> Result<String, String> {
    let band = spec.band;
    if !(7..=16).contains(&band) {
        return Err(format!(
            "ABI C{band:02} is not an IR band (7-16) — the IR window ingest colors Kelvin BT"
        ));
    }
    let bucket = bucket_for_satellite(&spec.satellite).map_err(|err| err.to_string())?;
    let sector =
        Sector::parse(&spec.sector).ok_or_else(|| format!("unknown sector '{}'", spec.sector))?;
    let satellite = GoesSatellite::parse(&spec.satellite);
    let window = spec.window.clamped();
    let cache_dir = store_root.join("cache");

    let now = spec.as_of.unwrap_or_else(Utc::now);
    let hour_span = (spec.lookback_minutes.max(20) / 60) + 2;
    let hours: Vec<DateTime<Utc>> = (0..hour_span)
        .map(|i| now - chrono::Duration::hours(i))
        .collect();
    let (scan_start, objects) = latest_common_scan(
        &bucket,
        sector.abi_product(),
        &satellite,
        &[band],
        &hours,
        spec.as_of,
    )?;
    let object = &objects[&band];

    let agent = build_agent();
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
    let field = read_goes_abi_window(&downloaded.path, window)?;

    let len = field.values.len();
    let (r, g, b, lit) = bake_ir_planes(&field.values, band, enhancement);

    let scene = &field.scene;
    let projection = &scene.projection;
    let sweep = match projection.sweep_angle_axis {
        SweepAngleAxis::X => "x",
        SweepAngleAxis::Y => "y",
    };
    let selector = serde_json::json!({
        "satellite": {
            "provider": "noaa",
            "instrument": "abi",
            "satellite": scene.satellite.as_str(),
            "product": scene.product,
            "band": band,
            "layer": format!("rgb_ir{band:02}"),
            "source_variable": "CMI",
            "enhanced_ir": {
                "band": band,
                "enhancement": enhancement.slug(),
                "enhancement_label": enhancement.label(),
                "native_window": {
                    "center_lat_deg": window.center_lat_deg,
                    "center_lon_deg": window.center_lon_deg,
                    "size_km": window.size_km,
                },
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
    });
    let frame = write_goes_rgb_frame(
        store_root,
        scene,
        &format!("rgb_ir{band:02}"),
        selector,
        &r,
        &g,
        &b,
        Utc::now().timestamp().max(0) as u64,
    )?;

    send(SatResponse::FrameWritten {
        id: object.key.clone(),
        run: frame.run.clone(),
        hhmm: frame.hhmm,
        bytes: frame.bytes,
        encode_ms: frame.encode_ms,
    });
    send(SatResponse::Runs(scan_runs(store_root)));
    send(SatResponse::SelectFrame {
        key: SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        },
        hhmm: frame.hhmm,
    });

    Ok(format!(
        "GOES {} {} C{band:02} IR window ({}): scan {} · {}x{} @ native res · {:.0}% on-earth · wrote {}/{}/t{:04}",
        satellite.as_str(),
        sector_slug(&scene.sector),
        enhancement.label(),
        scan_start.format("%Y-%m-%d %H:%MZ"),
        scene.fixed_grid.nx,
        scene.fixed_grid.ny,
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
fn write_goes_composite_frame(
    store_root: &Path,
    scene: &GoesAbiScene,
    style: GoesAbiRgbCompositeStyle,
    r: &[f32],
    g: &[f32],
    b: &[f32],
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    write_goes_rgb_frame(
        store_root,
        scene,
        &format!("rgb_{}", style.slug()),
        composite_selector(scene, style),
        r,
        g,
        b,
        written_unix,
    )
}

/// The baked-RGB store writer behind [`write_goes_composite_frame`] and the
/// tropical-card enhanced-IR window frames: `family` is the run-name token
/// between sector and day (`rgb_<style>` / `rgb_ir13`) and MUST contain
/// `rgb` — the run-name⇔content contract is "`_rgb_` in the name ⇔ the
/// frame holds three baked `rgb_r/g/b` planes", and the app's run-key
/// display filter admits these runs by that token.
#[allow(clippy::too_many_arguments)]
fn write_goes_rgb_frame(
    store_root: &Path,
    scene: &GoesAbiScene,
    family: &str,
    selector: serde_json::Value,
    r: &[f32],
    g: &[f32],
    b: &[f32],
    written_unix: u64,
) -> Result<WrittenFrame, String> {
    debug_assert!(family.starts_with("rgb"), "baked family token: {family}");
    let model = scene.satellite.as_str().to_ascii_lowercase();
    let sector = sector_slug(&scene.sector);
    let day = scene.start_time_utc.format("%Y%m%d").to_string();
    let hhmm = (scene.start_time_utc.hour() * 100 + scene.start_time_utc.minute()) as u16;
    let run_base = format!("{sector}_{family}_{day}");

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
        variable: COMPOSITE_R_VAR.to_string(),
    })
}

fn worker_loop(
    store_root: PathBuf,
    requests: &Receiver<SatRequest>,
    responses: &Sender<SatResponse>,
    card_outcomes: &Sender<CardOutcome>,
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
    // Report a card-ticketed one-shot ingest's outcome on the side channel
    // (no-op for unticketed requests — the regular panel buttons).
    let send_card = |ticket: Option<u64>, result: &Result<String, String>| {
        if let Some(ticket) = ticket {
            let _ = card_outcomes.send(CardOutcome {
                ticket,
                result: result.clone(),
            });
            notify();
        }
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
            SatRequest::ScanAndSelect { key, hhmm } => {
                if !send(SatResponse::Runs(scan_runs(&store_root))) {
                    return;
                }
                if !send(SatResponse::SelectFrame { key, hhmm }) {
                    return;
                }
            }
            SatRequest::LoadFrame { key, hhmm } => {
                let result = load_frame(&mut state, &store_root, &key, hhmm);
                let legacy = result
                    .as_ref()
                    .map(|colored| colored.legacy)
                    .unwrap_or(false);
                if !send(SatResponse::Frame {
                    key,
                    hhmm,
                    legacy,
                    result: Box::new(result.map(|colored| colored.frame)),
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
            SatRequest::LoadFrameForPlot { key, hhmm } => {
                let result = load_frame_for_plot(&mut state, &store_root, &key, hhmm);
                if !send(SatResponse::PlotFrame {
                    key,
                    hhmm,
                    result: Box::new(result),
                }) {
                    return;
                }
            }
            SatRequest::SetIrEnhancement(enhancement) => {
                state.ir_enhancement = enhancement;
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
                let scope = match &spec.window {
                    Some(window) => format!(" · native window {}", window.run_slug()),
                    None => String::new(),
                };
                send(SatResponse::Note(format!(
                    "GOES composite: locating latest {} {} {}{scope}",
                    spec.satellite, spec.sector, spec.style
                )));
                let result = ingest_latest_goes_composite(&store_root, &spec, &send);
                send_card(spec.card_ticket, &result);
                match result {
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
            SatRequest::IngestLatestHimawariComposite(spec) => {
                let scope = match &spec.window {
                    Some(window) => format!("native window {}", window.run_slug()),
                    None if spec.full_disk => "full disk".to_string(),
                    None => format!(
                        "S{:02}..S{:02}",
                        spec.segment_start,
                        spec.segment_start
                            .saturating_add(spec.segment_count)
                            .saturating_sub(1)
                    ),
                };
                send(SatResponse::Note(format!(
                    "Himawari composite: locating latest {} {} ({scope})",
                    spec.satellite, spec.style,
                )));
                let result = ingest_latest_himawari_composite(&store_root, &spec, &send);
                send_card(spec.card_ticket, &result);
                match result {
                    Ok(summary) => {
                        send(SatResponse::Note(summary));
                        send(SatResponse::Runs(scan_runs(&store_root)));
                    }
                    Err(message) => {
                        send(SatResponse::Note(format!(
                            "Himawari composite failed: {message}"
                        )));
                    }
                }
            }
            SatRequest::IngestLatestHimawariIrWindow(spec) => {
                send(SatResponse::Note(format!(
                    "Himawari IR window: locating latest {} B{:02} · {}",
                    spec.satellite,
                    spec.band,
                    spec.window.run_slug()
                )));
                let result = ingest_latest_himawari_ir_window(&store_root, &spec, &send);
                send_card(spec.card_ticket, &result);
                match result {
                    Ok(summary) => {
                        send(SatResponse::Note(summary));
                        send(SatResponse::Runs(scan_runs(&store_root)));
                    }
                    Err(message) => {
                        send(SatResponse::Note(format!(
                            "Himawari IR window failed: {message}"
                        )));
                    }
                }
            }
            SatRequest::IngestLatestGoesIrWindow(spec) => {
                send(SatResponse::Note(format!(
                    "GOES IR window: locating latest {} {} B{:02} · {}",
                    spec.satellite,
                    spec.sector,
                    spec.band,
                    spec.window.run_slug()
                )));
                let result =
                    ingest_latest_goes_ir_window(&store_root, &spec, state.ir_enhancement, &send);
                send_card(spec.card_ticket, &result);
                match result {
                    Ok(summary) => {
                        send(SatResponse::Note(summary));
                        send(SatResponse::Runs(scan_runs(&store_root)));
                    }
                    Err(message) => {
                        send(SatResponse::Note(format!(
                            "GOES IR window failed: {message}"
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
        let frame = load_frame(&mut state, &dir, &key, 1851)
            .expect("frame loads")
            .frame;
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
    fn native_plot_load_keeps_simsat_scalar_selector_and_storage_rows() {
        let dir = test_dir("simsat-plot-scalar");
        let model = "simsat";
        let run = "hrrr_20260710_t20z_ir13_geo_c13_20260710";
        let hhmm = 2100;
        let run_dir = dir.join(model).join(run);
        std::fs::create_dir_all(&run_dir).unwrap();
        // Row zero is SOUTH: a player image would flip this, while native
        // plot values must remain byte-for-byte parallel to this mesh.
        let lat = vec![30.0, 30.0, 31.0, 31.0];
        let lon = vec![-101.0, -100.0, -101.0, -100.0];
        let grid = LatLonGrid {
            shape: GridShape { nx: 2, ny: 2 },
            lat_deg: lat.clone(),
            lon_deg: lon.clone(),
        };
        let grid_hash = write_grid(&run_dir.join("grid.rwg"), &grid, None).unwrap();
        let selector = serde_json::json!({
            "satellite": {
                "provider": "simsat",
                "instrument": "synthetic-ir",
                "product": "surface_bt",
                "band": 13
            }
        });
        let values = vec![290.0, 291.0, 210.0, f32::NAN];
        let mut writer = HourWriter::new(model, run, hhmm, 2, 2, &grid_hash, "test");
        writer
            .add_surface2d("ahi_bt_c13", "K", selector.clone(), &values)
            .unwrap();
        writer.finish(&run_dir.join(frame_file_name(hhmm))).unwrap();

        let key = SatRunKey {
            model: model.to_string(),
            run: run.to_string(),
        };
        let source = load_frame_for_plot(&mut WorkerState::default(), &dir, &key, hhmm)
            .expect("SimSat scalar plot frame loads");
        assert_eq!(source.grid.lat, lat);
        assert_eq!(source.grid.lon, lon);
        assert_eq!(source.grid.lat_descending(), Some(false));
        assert!(source.title.contains("SimSat") && source.title.contains("2100Z"));
        match &source.raster {
            crate::sat_plot::SatellitePlotRaster::Scalar {
                variable,
                units,
                selector: loaded_selector,
                values: loaded_values,
                palette,
            } => {
                assert_eq!(variable, "ahi_bt_c13");
                assert_eq!(units, "K");
                assert_eq!(loaded_selector, &selector);
                assert_eq!(
                    &loaded_values[..3],
                    &values[..3],
                    "raw storage rows were flipped"
                );
                assert!(loaded_values[3].is_nan());
                assert!(palette.is_some(), "C13 selector resolves the IR palette");
            }
            other => panic!("expected scalar plot payload, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_plot_composite_keeps_rgb_storage_rows_and_nan_alpha() {
        let dir = test_dir("simsat-plot-rgb");
        let model = "simsat";
        let run = "hrrr_20260710_t20z_geocolor_geo_rgb_goese_20260710";
        let hhmm = 2100;
        let run_dir = dir.join(model).join(run);
        std::fs::create_dir_all(&run_dir).unwrap();
        let grid = LatLonGrid {
            shape: GridShape { nx: 2, ny: 2 },
            lat_deg: vec![30.0, 30.0, 31.0, 31.0],
            lon_deg: vec![-101.0, -100.0, -101.0, -100.0],
        };
        let grid_hash = write_grid(&run_dir.join("grid.rwg"), &grid, None).unwrap();
        let mut writer = HourWriter::new(model, run, hhmm, 2, 2, &grid_hash, "test");
        writer
            .add_surface2d(
                COMPOSITE_R_VAR,
                "rgb8",
                serde_json::json!({"satellite": {"provider": "simsat"}}),
                &[255.0, 0.0, 0.0, f32::NAN],
            )
            .unwrap();
        writer
            .add_surface2d(
                COMPOSITE_G_VAR,
                "rgb8",
                serde_json::Value::Null,
                &[0.0, 255.0, 0.0, f32::NAN],
            )
            .unwrap();
        writer
            .add_surface2d(
                COMPOSITE_B_VAR,
                "rgb8",
                serde_json::Value::Null,
                &[0.0, 0.0, 255.0, f32::NAN],
            )
            .unwrap();
        writer.finish(&run_dir.join(frame_file_name(hhmm))).unwrap();
        let key = SatRunKey {
            model: model.to_string(),
            run: run.to_string(),
        };
        let source = load_frame_for_plot(&mut WorkerState::default(), &dir, &key, hhmm)
            .expect("SimSat RGB plot frame loads");
        match &source.raster {
            crate::sat_plot::SatellitePlotRaster::Rgba { pixels } => {
                assert_eq!(pixels[0], rustwx_render::Color::rgba(255, 0, 0, 255));
                assert_eq!(pixels[1], rustwx_render::Color::rgba(0, 255, 0, 255));
                assert_eq!(pixels[2], rustwx_render::Color::rgba(0, 0, 255, 255));
                assert_eq!(pixels[3], rustwx_render::Color::TRANSPARENT);
            }
            other => panic!("expected RGB plot payload, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_frames_share_one_grid_read_within_and_across_runs() {
        let dir = test_dir("map-grid-cache");
        let written = write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 13), 1).unwrap();
        let key = SatRunKey {
            model: written.model.clone(),
            run: written.run.clone(),
        };
        write_band_frame(&dir, &synthetic_field(8, 6, 18, 56, 13), 2).unwrap();

        let mut state = WorkerState::default();
        let first = load_frame_for_map(&mut state, &dir, &key, 1851).expect("first map frame");
        let second = load_frame_for_map(&mut state, &dir, &key, 1856).expect("second map frame");
        assert!(
            Arc::ptr_eq(&first.grid, &second.grid),
            "frames of one run must share ONE opened grid, not re-read ~240 MB per step"
        );
        assert!(!first.grid.hash.is_empty(), "store grids carry a sha256");

        // Band 8 writes a DIFFERENT run over the same scan geometry — a
        // bit-identical grid, hence the same content hash. Deleting its
        // grid.rwg proves the cross-run load is served entirely from the
        // content-addressed cache (no grid read at run rollover).
        let written_b = write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 8), 3).unwrap();
        let key_b = SatRunKey {
            model: written_b.model.clone(),
            run: written_b.run.clone(),
        };
        assert_ne!(key.run, key_b.run, "distinct runs");
        std::fs::remove_file(dir.join(&key_b.model).join(&key_b.run).join("grid.rwg")).unwrap();
        let cross = load_frame_for_map(&mut state, &dir, &key_b, 1851).expect("cross-run frame");
        assert!(
            Arc::ptr_eq(&first.grid, &cross.grid),
            "identical grid content must be served from the cache"
        );
        assert_eq!(cross.flip_rows, first.flip_rows);

        // A cold worker (empty cache) must still need the file: the cache
        // is an optimization, never a fallback data source.
        let mut cold = WorkerState::default();
        assert!(load_frame_for_map(&mut cold, &dir, &key_b, 1851).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn himawari_ir_is_colorized_not_grayscale() {
        // Kelvin brightness temps: NaN, warm surface, a -60 C top, a -40 C top.
        let values = vec![f32::NAN, 300.0, 213.0, 233.0];
        let (pixels, legacy) = render_sat_pixels(
            "cmi_c13",
            13,
            &values,
            4,
            1,
            false,
            false,
            IrEnhancement::Cimss,
        );

        assert!(
            !legacy,
            "true-Kelvin GOES IR never takes the legacy stretch"
        );
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

    /// The windowed decode seeks to its row span instead of reading the
    /// whole ~97 MB segment: on a synthetic segment the seek math must hand
    /// back exactly the bytes the old full read indexed, and a truncated
    /// file must still fail with the full byte accounting.
    #[test]
    fn ahi_row_span_read_matches_the_full_read() {
        let dir = test_dir("ahi-row-span");
        let path = dir.join("segment.bin");
        let (data_start, columns, seg_lines) = (64usize, 8usize, 10usize);
        // Fake header, then rows of u16 values encoding (row, col) so any
        // off-by-one in the seek arithmetic shows up in the decoded values.
        let mut file_bytes = vec![0xAA_u8; data_start];
        for row in 0..seg_lines {
            for col in 0..columns {
                file_bytes.extend_from_slice(&((row * 100 + col) as u16).to_le_bytes());
            }
        }
        std::fs::write(&path, &file_bytes).unwrap();

        // Middle span (rows 3..=6): byte-identical to the slice the old
        // whole-file read would have indexed at data_start + row*columns*2.
        let span = read_ahi_row_span(&path, data_start as u64, columns, seg_lines, 3, 4)
            .expect("span reads");
        assert_eq!(span.len(), 4 * columns * 2);
        assert_eq!(
            span.as_slice(),
            &file_bytes[data_start + 3 * columns * 2..data_start + 7 * columns * 2]
        );
        // Decoded first/last samples land on the expected rows/cols.
        assert_eq!(u16::from_le_bytes([span[0], span[1]]), 300);
        let last = span.len() - 2;
        assert_eq!(u16::from_le_bytes([span[last], span[last + 1]]), 607);

        // Full span and edge spans work; an out-of-segment span is refused.
        let all = read_ahi_row_span(&path, data_start as u64, columns, seg_lines, 0, seg_lines)
            .expect("full span reads");
        assert_eq!(all.as_slice(), &file_bytes[data_start..]);
        assert!(read_ahi_row_span(&path, data_start as u64, columns, seg_lines, 7, 4).is_err());

        // A truncated segment still fails loudly with byte accounting.
        std::fs::write(&path, &file_bytes[..file_bytes.len() - 1]).unwrap();
        let error = read_ahi_row_span(&path, data_start as u64, columns, seg_lines, 3, 4)
            .expect_err("short file is refused");
        assert!(error.contains("data block short"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
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
        let frame = write_himawari_grid_frame(&dir, &field, 7, None).expect("frame writes");
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
        let first =
            write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 0, vec![0.0, 0.04]), 7, None)
                .expect("first frame");
        assert!(first.created_run);
        let second =
            write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 10, vec![0.0, 0.04]), 8, None)
                .expect("second frame joins the run");
        assert!(!second.created_run);
        assert_eq!(second.run, first.run);
        assert_eq!(
            second.grid_hash, first.grid_hash,
            "grid written once per run"
        );

        // A different fixed grid (e.g. another downsample) forks the run.
        let moved =
            write_himawari_grid_frame(&dir, &synthetic_ahi_field(2, 20, vec![0.0, 0.05]), 9, None)
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
        let frame = load_frame(&mut state, &dir, &key, 210)
            .expect("t0210 loads")
            .frame;
        assert_eq!(frame.image.size, [2, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ingest computes the navigation mesh once (for the off-earth
    /// mask) and hands it to the writer: a frame written with the
    /// precomputed mesh must land in the SAME run dir with the SAME grid
    /// hash as one whose writer navigated for itself — proof the mask and
    /// the baked geometry are one and the same mesh.
    #[test]
    fn himawari_writer_accepts_the_ingests_precomputed_mesh() {
        let dir = test_dir("ahi-shared-mesh");
        let field = synthetic_ahi_field(2, 0, vec![0.0, 0.04]);
        let self_navigated = write_himawari_grid_frame(&dir, &field, 7, None).expect("first frame");

        let later = synthetic_ahi_field(2, 10, vec![0.0, 0.04]);
        let mesh = ahi_lat_lon_mesh(&later.scene);
        let shared =
            write_himawari_grid_frame(&dir, &later, 8, Some(mesh)).expect("precomputed-mesh frame");
        assert_eq!(shared.run, self_navigated.run, "same mesh, same run dir");
        assert_eq!(shared.grid_hash, self_navigated.grid_hash);
        assert!(!shared.created_run);

        // A mesh that does not match the grid is refused loudly instead of
        // baking bogus geometry.
        let wrong = Some((vec![0.0_f32], vec![0.0_f32]));
        assert!(write_himawari_grid_frame(&dir, &later, 9, wrong).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Loaded frames carry the legacy-stretch flag so the UI can say WHY
    /// the IR enhancement picker has no effect on pre-calibration stored
    /// frames (yesterday's store is full of them) instead of looking dead.
    #[test]
    fn loaded_legacy_frames_carry_the_legacy_flag() {
        let dir = test_dir("ahi-legacy-flag");
        // A pre-calibration pseudo-BT plane: flat ~326-330 "Kelvin".
        let mut stale = synthetic_ahi_field(2, 0, vec![0.0, 0.04]);
        stale.values = vec![326.0, 327.5, 328.0, 329.5];
        let written = write_himawari_grid_frame(&dir, &stale, 7, None).expect("legacy writes");
        let key = SatRunKey {
            model: written.model.clone(),
            run: written.run.clone(),
        };
        let mut state = WorkerState::default();
        let colored = load_frame(&mut state, &dir, &key, written.hhmm).expect("legacy loads");
        assert!(colored.legacy, "pseudo-BT frame is flagged for the UI");

        // A true-Kelvin frame in the same run is NOT flagged.
        let mut real = synthetic_ahi_field(2, 10, vec![0.0, 0.04]);
        real.values = vec![195.0, 233.0, 273.0, 288.0];
        let written = write_himawari_grid_frame(&dir, &real, 8, None).expect("real writes");
        let colored = load_frame(&mut state, &dir, &key, written.hhmm).expect("real loads");
        assert!(!colored.legacy, "true-Kelvin frame renders enhanced");
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
        let frame = load_frame(&mut state, &store, &key, hhmm)
            .expect("proof frame loads")
            .frame;
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
        let loaded = load_frame(&mut state, &dir, &key, frame.hhmm)
            .expect("composite loads")
            .frame;
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
    fn simsat_run_titles_decode_source_product_and_view_tokens() {
        let geo = run_title(
            "simsat",
            "hrrr_20260710_t20z_geocolor_geo_rgb_goese_20260710",
        );
        assert!(
            geo.contains("SimSat")
                && geo.contains("HRRR")
                && geo.contains("GeoColor")
                && geo.contains("GEO")
                && geo.contains("2026-07-10"),
            "{geo}"
        );
        let thermal = run_title("simsat", "wrfout_d03_wv08_topdown_c08_20250621");
        assert!(
            thermal.contains("WRF")
                && thermal.contains("Water Vapor C08")
                && thermal.contains("TOP-DOWN"),
            "{thermal}"
        );
    }

    #[test]
    fn composite_style_options_lead_with_natural_color() {
        let options = goes_composite_style_options();
        assert_eq!(options.len(), GoesAbiRgbCompositeStyle::ALL.len());
        assert_eq!(options[0].0, "natural_color");
        assert!(options[0].1.contains("C01+C02+C03"), "{}", options[0].1);
        // Every offered slug parses back to a real style.
        for (slug, _) in &options {
            assert!(
                GoesAbiRgbCompositeStyle::parse(slug).is_some(),
                "slug {slug}"
            );
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
            window: None,
            as_of: None,
            card_ticket: None,
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
        let frame = load_frame(&mut state, &store, &run.key, hhmm)
            .expect("proof frame loads")
            .frame;

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
    fn legacy_pseudo_bt_frames_fall_back_to_the_percentile_stretch() {
        // Frames written by the pre-calibration AHI path store ~326-330
        // (raw-ish pseudo-BT, verified from a live full-disk frame). An
        // absolute-Kelvin palette would clamp them to one flat warm color;
        // the median>320 K detector must route them through the legacy
        // stretch so they still colorize.
        let values = vec![f32::NAN, 326.0, 327.5, 328.0, 329.0, 330.0];
        let (pixels, legacy) = render_sat_pixels(
            "ahi_bt_c13",
            13,
            &values,
            3,
            2,
            false,
            false,
            IrEnhancement::Cimss,
        );

        assert!(legacy, "the pseudo-BT fallback is flagged for the UI");
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
        // The detector keys on the median, not the presence of warm pixels:
        // a real-Kelvin disk with a hot desert pixel is NOT legacy.
        assert!(legacy_pseudo_bt(&values), "flat ~328 K plane is legacy");
        assert!(
            !legacy_pseudo_bt(&[f32::NAN, 195.0, 233.0, 273.0, 288.0, 325.0]),
            "true-Kelvin distribution is not legacy"
        );

        // B07 (3.9 µm) is exempt: a daytime shortwave-IR disk legitimately
        // medians past 320 K (solar reflection reaches 330-400 K), so a
        // correctly calibrated frame must keep the selected absolute
        // enhancement instead of flapping into the percentile stretch.
        let daytime_b07 = vec![f32::NAN, 285.0, 330.0, 345.0, 360.0, 395.0];
        assert!(
            legacy_pseudo_bt(&daytime_b07),
            "daytime B07 medians past the threshold — exactly why it is exempt"
        );
        for enhancement in IrEnhancement::ALL {
            let (ahi, ahi_legacy) = render_sat_pixels(
                "ahi_bt_c07",
                7,
                &daytime_b07,
                3,
                2,
                false,
                false,
                enhancement,
            );
            let (goes, _) =
                render_sat_pixels("cmi_c07", 7, &daytime_b07, 3, 2, false, false, enhancement);
            assert!(
                !ahi_legacy,
                "{enhancement:?}: B07 is exempt from the detector"
            );
            assert_eq!(
                ahi, goes,
                "{enhancement:?}: B07 must render absolute, never the legacy stretch"
            );
        }
    }

    #[test]
    fn true_kelvin_ahi_renders_exactly_like_goes() {
        // Post-calibration AHI IR is real Kelvin, so the same values through
        // `ahi_bt_c13` and `cmi_c13` must produce identical pixels — the
        // percentile hack is gone from the calibrated path.
        let values = vec![f32::NAN, 195.0, 210.0, 233.0, 273.0, 295.0];
        for enhancement in IrEnhancement::ALL {
            let ahi = render_sat_pixels("ahi_bt_c13", 13, &values, 3, 2, false, false, enhancement);
            let goes = render_sat_pixels("cmi_c13", 13, &values, 3, 2, false, false, enhancement);
            assert_eq!(ahi, goes, "{enhancement:?} diverged between AHI and GOES");
        }
    }

    #[test]
    fn ir_enhancement_slugs_round_trip_and_unknown_falls_back() {
        for enhancement in IrEnhancement::ALL {
            assert_eq!(IrEnhancement::parse(enhancement.slug()), enhancement);
        }
        assert_eq!(IrEnhancement::parse(" BD "), IrEnhancement::Bd);
        assert_eq!(IrEnhancement::parse("no-such"), IrEnhancement::Cimss);
        assert_eq!(IrEnhancement::parse(""), IrEnhancement::Cimss);
    }

    #[test]
    fn bd_curve_is_stepped_at_the_nesdis_breakpoints() {
        let gray = |bt: f32| {
            let [r, g, b, a] = anchor_color(bt, BD_CURVE);
            assert_eq!(a, 255);
            assert!(r == g && g == b, "BD is grayscale at {bt} K: {r},{g},{b}");
            r
        };
        // Hard steps: an exact boundary belongs to the COLDER bin, and one
        // hundredth of a Kelvin warmer flips the shade with no blending.
        assert_eq!(gray(209.95), 0, "-63.2 C is (cold-side) black");
        assert_eq!(gray(209.96), 160, "just warmer than -63.2 C is light gray");
        assert_eq!(gray(219.95), 160, "-53.2 C is (cold-side) light gray");
        assert_eq!(gray(219.96), 112, "just warmer than -53.2 C is medium gray");
        assert_eq!(gray(231.96), 64, "just warmer than -41.2 C is dark gray");
        assert_eq!(gray(192.95), 88, "-80.2 C and colder repeat cold dark gray");
        assert_eq!(gray(170.0), 88, "deep cold stays cold dark gray");
        assert_eq!(gray(193.0), 136, "-80.15 C is cold medium gray");
        assert_eq!(gray(200.0), 255, "-73 C is white");
        // Flat within a bin — stepped, not a gradient.
        assert_eq!(gray(220.5), gray(231.0), "medium-gray bin is flat");
        // The warm scene is a ramp, not a step (off-white -> mid gray).
        let ramp_cold = gray(250.0);
        let ramp_warm = gray(275.0);
        assert!(
            ramp_cold > ramp_warm && ramp_warm > 110,
            "off-white ramp descends warmward: {ramp_cold} -> {ramp_warm}"
        );
        assert_eq!(gray(310.0), 0, "hot surface clamps black");
    }

    #[test]
    fn avn_and_funktop_step_at_their_curve_boundaries() {
        // AVN: yellow -> blue step at -38.5 C (234.65 K).
        let [r, g, b, _] = anchor_color(234.60, AVN_IR);
        assert!(
            r > 130 && g > 130 && b < 40,
            "cold side is yellow: {r},{g},{b}"
        );
        let [r, g, b, _] = anchor_color(234.70, AVN_IR);
        assert!(b > 150 && r < 20, "warm side is blue: {r},{g},{b}");
        // AVN: coldest overshoots are white above the anvil gray.
        assert_eq!(anchor_color(168.0, AVN_IR), [255, 255, 255, 255]);
        let [r, g, b, _] = anchor_color(185.0, AVN_IR);
        assert!(
            r == 88 && g == 88 && b == 88,
            "anvil-core gray: {r},{g},{b}"
        );

        // Funktop: dark red (cold side) -> cyan (warm side) step at -58.0 C
        // (215.15 K): colder than -58 is the red deep-convection bin, warmer
        // is the cyan mid-cloud ramp.
        let [r, g, b, _] = anchor_color(215.10, FUNKTOP_IR);
        assert!(
            r > 50 && g < 30 && b < 30,
            "cold side is dark red: {r},{g},{b}"
        );
        let [r, g, b, _] = anchor_color(215.20, FUNKTOP_IR);
        assert!(g > 200 && b > 220, "warm side is cyan: {r},{g},{b}");
        // Funktop: pink band between -70.5 and -78 C.
        let [r, g, b, _] = anchor_color(199.0, FUNKTOP_IR);
        assert!(
            r > 240 && g > 60 && g < 150 && b > 60,
            "pink band: {r},{g},{b}"
        );
    }

    #[test]
    fn ahi_ir_counts_calibrate_to_true_kelvin() {
        // Block-5 calibration read from a REAL Himawari-9 B13 segment
        // (HS_H09_20260706_0600_B13_FLDK_R20_S0710): expected temperatures
        // are hand-computed from the JMA HSD §4.4 scheme with these exact
        // coefficients (inverse Planck at 10.4074 um, then the quadratic
        // Te -> Tb correction).
        let calibration = HimawariCalibrationInfo {
            band_number: 13,
            central_wavelength_um: 10.4074,
            valid_bits_per_pixel: 12,
            error_pixel_count: 65535,
            outside_scan_count: 65534,
            count_to_radiance_slope: -0.0037525074318633814,
            count_to_radiance_intercept: 15.197657722429469,
            planck_or_albedo_coefficients: [
                -0.118260812197365,
                1.00101143081895,
                -1.80800453227613e-6,
            ],
            inverse_planck_coefficients: Some([
                0.118211645089097,
                0.998989021775474,
                1.80702978762378e-6,
            ]),
            physical_constants: Some(rw_sat::himawari::HimawariPhysicalConstants {
                speed_of_light_m_s: 299_792_458.0,
                planck_constant_j_s: 6.62606957e-34,
                boltzmann_constant_j_k: 1.3806488e-23,
            }),
        };
        let counts = [400.0, 1500.0, 2500.0, 3500.0, f32::NAN, 4096.0];
        let bt = ahi_counts_to_brightness_temperature(&counts, &calibration)
            .expect("IR calibration converts");
        let expected: [f64; 4] = [323.044884, 298.340541, 269.602675, 224.425914];
        for (index, want) in expected.iter().enumerate() {
            assert!(
                (f64::from(bt[index]) - want).abs() < 5.0e-3,
                "count {} -> {} K, want {want} K",
                counts[index],
                bt[index]
            );
        }
        assert!(bt[4].is_nan(), "error-sentinel count stays NaN");
        assert!(
            bt[5].is_nan(),
            "negative radiance (count past the intercept) is non-physical"
        );

        // A visible band (no Planck constants) refuses IR calibration.
        let visible = HimawariCalibrationInfo {
            band_number: 1,
            inverse_planck_coefficients: None,
            physical_constants: None,
            ..calibration
        };
        assert!(ahi_counts_to_brightness_temperature(&counts, &visible).is_err());
    }

    #[test]
    fn synth_hurricane_ir_proof() {
        let Some(out) = std::env::var_os("BOWECHO_SAT_SYNTH_PNG") else {
            return;
        };
        let (nx, ny) = (512usize, 512usize);
        let bt = synthetic_hurricane_bt(nx, ny);

        let started = std::time::Instant::now();
        let (pixels, _) = render_sat_pixels(
            "cmi_c13",
            13,
            &bt,
            nx,
            ny,
            false,
            false,
            IrEnhancement::Cimss,
        );
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
        let (overlay, _) = render_sat_pixels(
            "cmi_c13",
            13,
            &values,
            3,
            1,
            false,
            true,
            IrEnhancement::Cimss,
        );

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
        let (player, _) = render_sat_pixels(
            "cmi_c13",
            13,
            &values,
            3,
            1,
            false,
            false,
            IrEnhancement::Cimss,
        );
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

    #[test]
    fn scan_and_select_emits_runs_before_selection() {
        let dir = test_dir("worker-scan-select");
        let written = write_band_frame(&dir, &synthetic_field(8, 6, 18, 51, 13), 1).unwrap();
        let key = SatRunKey {
            model: written.model,
            run: written.run,
        };
        let worker = SatWorker::spawn(dir.clone(), || {});
        worker.send(SatRequest::ScanAndSelect {
            key: key.clone(),
            hhmm: written.hhmm,
        });
        assert!(
            matches!(
                worker
                    .rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("scan responds first"),
                SatResponse::Runs(_)
            ),
            "ScanAndSelect must publish the refreshed listing first"
        );
        match worker
            .rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("selection responds second")
        {
            SatResponse::SelectFrame {
                key: selected,
                hhmm,
            } => {
                assert_eq!(selected, key);
                assert_eq!(hhmm, written.hhmm);
            }
            other => panic!("expected SelectFrame second, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- Himawari AHI true-color composite ----------------------------------

    fn synthetic_visible_calibration(
        slope: f64,
        intercept: f64,
        cprime: f64,
    ) -> HimawariCalibrationInfo {
        HimawariCalibrationInfo {
            band_number: 1,
            central_wavelength_um: 0.47,
            valid_bits_per_pixel: 11,
            error_pixel_count: 65535,
            outside_scan_count: 65534,
            count_to_radiance_slope: slope,
            count_to_radiance_intercept: intercept,
            planck_or_albedo_coefficients: [cprime, 0.0, 0.0],
            inverse_planck_coefficients: None,
            physical_constants: None,
        }
    }

    #[test]
    fn himawari_composite_style_round_trips_and_assigns_true_color_bands() {
        for style in HimawariCompositeStyle::ALL {
            assert_eq!(HimawariCompositeStyle::parse(style.slug()), Some(style));
        }
        // True color assigns R=B03 (red) G=B02 (real green) B=B01 (blue),
        // base grid = B01 (1 km); fetches all three visible bands.
        let style = HimawariCompositeStyle::TrueColor;
        assert_eq!(style.rgb_bands(), (3, 2, 1));
        assert_eq!(style.base_band(), 1);
        assert_eq!(style.required_bands(), &[1, 2, 3]);
        assert_eq!(
            HimawariCompositeStyle::parse("true-color"),
            Some(HimawariCompositeStyle::TrueColor)
        );
        assert_eq!(HimawariCompositeStyle::parse("nope"), None);
    }

    #[test]
    fn ahi_counts_convert_to_reflectance_and_clamp_dark() {
        // radiance = 0.1*count - 1.0; reflectance = 0.01*radiance.
        let calibration = synthetic_visible_calibration(0.1, -1.0, 0.01);
        let counts = vec![f32::NAN, 5.0, 20.0, 100.0];
        let reflectance = ahi_counts_to_reflectance(&counts, &calibration);
        assert!(reflectance[0].is_nan(), "error/off-disk stays NaN");
        // count 5 -> radiance -0.5 -> negative -> clamps to 0 (opaque near-black).
        assert_eq!(reflectance[1], 0.0);
        // count 20 -> radiance 1.0 -> reflectance 0.01.
        assert!((reflectance[2] - 0.01).abs() < 1e-6, "{}", reflectance[2]);
        // count 100 -> radiance 9.0 -> reflectance 0.09.
        assert!((reflectance[3] - 0.09).abs() < 1e-6, "{}", reflectance[3]);
    }

    #[test]
    fn ahi_true_color_round_trips_and_greens_vegetation() {
        let dir = test_dir("ahi-composite");
        // 2x2 AHI scene (sweep=y mesh), row-major reflectance:
        // [0] vegetation, [1] bright cloud, [2] dark ocean, [3] off-earth.
        let scene = synthetic_ahi_field(2, 0, vec![0.0, 0.04]).scene;
        let (nx, ny) = (scene.fixed_grid.nx, scene.fixed_grid.ny);
        let len = nx * ny;
        assert_eq!(len, 4);

        let style = HimawariCompositeStyle::TrueColor;
        let mut planes: HashMap<u8, Vec<f32>> = HashMap::new();
        // B03 red, B02 green, B01 blue (off-earth = NaN in blue).
        planes.insert(3, vec![0.10, 0.90, 0.02, 0.05]);
        planes.insert(2, vec![0.22, 0.90, 0.03, 0.05]);
        planes.insert(1, vec![0.08, 0.90, 0.06, f32::NAN]);
        let (r, g, b) = compose_ahi_true_color(style, &planes, len).expect("compose");

        let frame = write_himawari_composite_frame(&dir, &scene, style, &r, &g, &b, 1).unwrap();
        assert_eq!(frame.model, "h9");
        assert!(
            frame.run.contains("_rgb_true_color_"),
            "composite run naming: {}",
            frame.run
        );
        assert!(frame.created_run);

        // Load back through the exact player path (composite branch). The AHI
        // mesh stores north-first (y descends), so rows are NOT flipped and
        // pixel index == compose index.
        let mut state = WorkerState::default();
        let key = SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        };
        let loaded = load_frame(&mut state, &dir, &key, frame.hhmm)
            .expect("composite loads")
            .frame;
        assert_eq!(loaded.image.size, [nx, ny]);

        let veg = loaded.image.pixels[0];
        assert_eq!(veg.a(), 255, "lit composite pixel is opaque");
        assert!(
            veg.g() > veg.r() && veg.g() > veg.b(),
            "vegetation renders green (real B02 green): {veg:?}"
        );
        let cloud = loaded.image.pixels[1];
        assert_eq!(cloud.a(), 255);
        assert!(
            cloud.r() > 150 && cloud.g() > 150 && cloud.b() > 150,
            "cloud is bright: {cloud:?}"
        );
        let ocean = loaded.image.pixels[2];
        assert_eq!(
            ocean.a(),
            255,
            "dark ocean stays opaque (not a transparent hole)"
        );
        assert!(ocean.b() > ocean.r(), "dark ocean skews blue: {ocean:?}");
        assert_eq!(
            loaded.image.pixels[3].a(),
            0,
            "off-earth composite pixel is transparent"
        );

        // Self-describing: composite style + AHI sweep=y projection.
        let stored = rw_sat::store::read_frame(&dir, &frame.model, &frame.run, frame.hhmm).unwrap();
        assert_eq!(
            stored.selector["satellite"]["composite"]["style"],
            "true_color"
        );
        assert_eq!(
            stored.selector["satellite"]["projection"]["sweep_angle_axis"],
            "y"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ahi_composite_run_title_is_recognized() {
        let title = run_title("h9", "fulldisk_s04_05of10_rgb_true_color_20260705");
        assert!(
            title.contains("AHI True Color") && title.contains("2026-07-05"),
            "AHI composite title: {title}"
        );
    }

    // ---- Native-resolution window --------------------------------------------

    /// The pre-download plan for a Guam-centered window: segments S04-S05
    /// (the same tropical band the full-sector default fetches) and the
    /// 0.5 km B03 base grid.
    #[test]
    fn himawari_window_plan_selects_segments_and_finest_base() {
        let window = SatNativeWindow {
            center_lat_deg: 13.5,
            center_lon_deg: 144.8,
            size_km: 800.0,
        };
        assert_eq!(himawari_window_segments(window), Ok((4, 2)));
        assert_eq!(HimawariCompositeStyle::TrueColor.native_base_band(), 3);

        // A window on the far side of the earth is rejected with the token
        // in the message.
        let far = SatNativeWindow {
            center_lat_deg: 20.0,
            center_lon_deg: -39.3,
            size_km: 800.0,
        };
        let err = himawari_window_segments(far).unwrap_err();
        assert!(err.contains(&far.run_slug()), "{err}");
    }

    /// Windowed composite frames open their own run-dir family: the sector
    /// token carries the window, the store writes/loads round-trip, and the
    /// player titles stay recognizable.
    #[test]
    fn windowed_composite_run_names_carry_the_window_token() {
        let dir = test_dir("win-runs");
        let window = SatNativeWindow {
            center_lat_deg: 29.5,
            center_lon_deg: -95.4,
            size_km: 600.0,
        };

        // GOES: the windowed read renames the sector before the write.
        let mut scene = synthetic_field(2, 2, 18, 51, 2).scene;
        scene.sector = AbiSector::Unknown(format!(
            "{}_{}",
            sector_slug(&scene.sector),
            window.run_slug()
        ));
        let planes = [10.0, 200.0, 30.0, f32::NAN];
        let frame = write_goes_composite_frame(
            &dir,
            &scene,
            GoesAbiRgbCompositeStyle::NaturalColor,
            &planes,
            &planes,
            &planes,
            1,
        )
        .expect("windowed GOES composite writes");
        assert!(
            frame
                .run
                .starts_with("conus_win295n954w600_rgb_natural_color_"),
            "run: {}",
            frame.run
        );
        let title = run_title("g19", &frame.run);
        assert!(
            title.contains("GeoColor") && title.contains("conus_win295n954w600"),
            "title: {title}"
        );

        // Himawari: the windowed assemble stamps `fulldisk_<win…>`.
        let guam = SatNativeWindow {
            center_lat_deg: 13.5,
            center_lon_deg: 144.8,
            size_km: 800.0,
        };
        let mut ahi_scene = synthetic_ahi_field(2, 0, vec![0.0, 0.04]).scene;
        ahi_scene.sector = format!("fulldisk_{}", guam.run_slug());
        let frame = write_himawari_composite_frame(
            &dir,
            &ahi_scene,
            HimawariCompositeStyle::TrueColor,
            &planes,
            &planes,
            &planes,
            1,
        )
        .expect("windowed AHI composite writes");
        assert!(
            frame
                .run
                .starts_with("fulldisk_win135n1448e800_rgb_true_color_"),
            "run: {}",
            frame.run
        );

        // Both load back through the exact player path.
        let mut state = WorkerState::default();
        let key = SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        };
        let loaded = load_frame(&mut state, &dir, &key, frame.hhmm)
            .expect("windowed frame loads")
            .frame;
        assert_eq!(loaded.image.size, [2, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HSD MJD conversion (the windowed assemble reads scan times straight
    /// from headers): MJD 60000 is 2023-02-25T00:00Z, +0.5 day is noon.
    #[test]
    fn ahi_mjd_conversion_and_model_slug() {
        let midnight = ahi_mjd_to_datetime(60_000.0).expect("valid MJD");
        assert_eq!(midnight.to_rfc3339(), "2023-02-25T00:00:00+00:00");
        let noon = ahi_mjd_to_datetime(60_000.5).expect("valid MJD");
        assert_eq!(noon.to_rfc3339(), "2023-02-25T12:00:00+00:00");
        assert!(ahi_mjd_to_datetime(f64::NAN).is_err());

        assert_eq!(ahi_model_slug("Himawari-9"), "h9");
        assert_eq!(ahi_model_slug("Himawari-8"), "h8");
        assert_eq!(ahi_model_slug("GK-2A"), "gk2a");
    }

    #[test]
    fn ahi_resample_is_identity_on_matching_grid() {
        let grid = AbiFixedGrid {
            nx: 2,
            ny: 2,
            x_scan_rad: vec![0.0, 0.04],
            y_scan_rad: vec![0.12, 0.0],
        };
        let values = vec![1.0, 2.0, 3.0, 4.0];
        let out = resample_ahi_to_base(&grid, &values, &grid);
        assert_eq!(out, values, "same grid resamples to identity");
    }

    // ---- Full-disk true color ------------------------------------------------

    /// The strided per-segment row selection is EXACTLY concat-then-stride:
    /// whatever rows the old whole-grid `step_by` kept, the per-segment
    /// walk keeps — however the segment heights fall against the stride.
    /// This is what keeps the target-region composite byte-identical after
    /// the lean rewrite of `ahi_true_counts_on_grid`.
    #[test]
    fn ahi_strided_local_rows_match_concat_then_stride() {
        let heights = [5usize, 3, 6, 1, 7];
        let total: usize = heights.iter().sum();
        for step in [1usize, 2, 3, 4, 8] {
            let want: Vec<(usize, usize)> = (0..total)
                .step_by(step)
                .map(|global| {
                    let mut offset = 0;
                    for (segment, &lines) in heights.iter().enumerate() {
                        if global < offset + lines {
                            return (segment, global - offset);
                        }
                        offset += lines;
                    }
                    unreachable!("global row {global} beyond the concatenated grid")
                })
                .collect();
            let mut got: Vec<(usize, usize)> = Vec::new();
            let mut offset = 0usize;
            for (segment, &lines) in heights.iter().enumerate() {
                for local in ahi_strided_local_rows(offset, lines, step) {
                    got.push((segment, local));
                }
                offset += lines;
            }
            assert_eq!(got, want, "step {step}");
        }
    }

    #[test]
    fn fulldisk_plan_doubles_b03_stride_and_keeps_region_defaults() {
        // The full-disk scope is opt-in: the default spec still composes
        // the west-Pacific target region exactly as before.
        let spec = HimawariCompositeSpec::default();
        assert!(!spec.full_disk);
        assert_eq!((spec.segment_start, spec.segment_count), (4, 2));
        assert_eq!(spec.downsample, 4);

        // Full-disk strides: the 1 km B01/B02 keep the base stride; the
        // 0.5 km B03 doubles so it decodes straight to ~base resolution.
        assert_eq!(ahi_fulldisk_band_stride(1, 4), 4);
        assert_eq!(ahi_fulldisk_band_stride(2, 4), 4);
        assert_eq!(ahi_fulldisk_band_stride(3, 4), 8);
        assert_eq!(ahi_fulldisk_band_stride(3, 2), 4);

        // Grid math: the default stride 4 puts the whole disk at 2750² on
        // the 1 km base, with B03 arriving at the same 2750²; stride 2 is
        // the 5500² (~2 km) option.
        assert_eq!((0..11_000).step_by(4).count(), 2750);
        assert_eq!((0..22_000).step_by(8).count(), 2750);
        assert_eq!((0..11_000).step_by(2).count(), 5500);
        assert_eq!((0..22_000).step_by(4).count(), 5500);
    }

    /// Sector tokens mirror rw-sat's assembler naming, so the whole-disk
    /// composite opens the `fulldisk` run family while segment subsets keep
    /// today's target-region token (byte-identical run names for the
    /// existing button).
    #[test]
    fn ahi_sector_tokens_mirror_rw_sat_naming() {
        assert_eq!(ahi_sector_token("FLDK", 1, 10, 10, true), "fulldisk");
        assert_eq!(
            ahi_sector_token("FLDK", 4, 5, 10, false),
            "fulldisk_s04_05of10"
        );
        assert_eq!(ahi_sector_slug("JP01"), "japan");
        assert_eq!(ahi_sector_slug("R301"), "target");
    }

    #[test]
    fn fulldisk_composite_run_title_is_recognized() {
        let title = run_title("h9", "fulldisk_rgb_true_color_20260706");
        assert!(
            title.contains("AHI True Color")
                && title.contains("fulldisk")
                && title.contains("2026-07-06"),
            "full-disk composite title: {title}"
        );
    }

    /// End-to-end proof against LIVE Himawari open data: fetch the visible
    /// bands, compose AHI true color, store, load back, and export a PNG.
    /// Gated behind `BOWECHO_SAT_HIMAWARI_COMPOSITE_PROOF_PNG` so CI stays
    /// offline; run it to prove the natural/true-color path on real imagery
    /// (never synthetic-only). Daytime scans only (visible bands).
    #[test]
    fn export_himawari_composite_proof_png_when_env_is_set() {
        let Some(out) = std::env::var_os("BOWECHO_SAT_HIMAWARI_COMPOSITE_PROOF_PNG") else {
            return;
        };
        let store = std::env::var_os("BOWECHO_SAT_HIMAWARI_COMPOSITE_PROOF_STORE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("bowecho-ahi-composite-proof-store"));
        std::fs::create_dir_all(&store).expect("proof store dir");
        let env_usize = |key: &str, default: usize| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let spec = HimawariCompositeSpec {
            satellite: std::env::var("BOWECHO_SAT_HIMAWARI_COMPOSITE_SAT")
                .unwrap_or_else(|_| "h9".to_string()),
            style: "true_color".to_string(),
            segment_start: env_usize("BOWECHO_SAT_HIMAWARI_COMPOSITE_SEG_START", 4) as u8,
            segment_count: env_usize("BOWECHO_SAT_HIMAWARI_COMPOSITE_SEG_COUNT", 2) as u8,
            full_disk: false,
            lookback_minutes: 240,
            downsample: env_usize("BOWECHO_SAT_HIMAWARI_COMPOSITE_DOWNSAMPLE", 6),
            window: None,
            as_of: None,
            card_ticket: None,
        };
        let sink = |response: SatResponse| {
            if let SatResponse::Note(message) = &response {
                eprintln!("AHI COMPOSITE note: {message}");
            }
            true
        };
        let summary = ingest_latest_himawari_composite(&store, &spec, &sink)
            .expect("live AHI composite ingest");
        eprintln!("AHI COMPOSITE {summary}");

        let runs = scan_runs(&store);
        let run = runs
            .iter()
            .find(|run| run.key.run.contains("_rgb_true_color_"))
            .expect("an AHI composite run was written");
        let hhmm = *run.frames.last().expect("composite run has a frame");
        let mut state = WorkerState::default();
        let frame = load_frame(&mut state, &store, &run.key, hhmm)
            .expect("proof frame loads")
            .frame;

        let (mut lit, mut rsum, mut gsum, mut bsum) = (0u64, 0u64, 0u64, 0u64);
        let mut rgba = Vec::with_capacity(frame.image.pixels.len() * 4);
        for pixel in &frame.image.pixels {
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
            if pixel.a() > 0 {
                lit += 1;
                rsum += u64::from(pixel.r());
                gsum += u64::from(pixel.g());
                bsum += u64::from(pixel.b());
            }
        }
        assert!(lit > 0, "the true-color frame has lit pixels");
        eprintln!(
            "AHI COMPOSITE {}x{} lit={lit} mean rgb=({:.1},{:.1},{:.1})",
            frame.image.size[0],
            frame.image.size[1],
            rsum as f64 / lit as f64,
            gsum as f64 / lit as f64,
            bsum as f64 / lit as f64,
        );
        let image = image::RgbaImage::from_raw(
            frame.image.size[0] as u32,
            frame.image.size[1] as u32,
            rgba,
        )
        .expect("proof image dimensions match");
        if let Some(parent) = PathBuf::from(&out).parent() {
            std::fs::create_dir_all(parent).expect("proof png parent directory");
        }
        image.save(&out).expect("AHI composite proof png writes");
        eprintln!(
            "AHI COMPOSITE proof PNG {}x{} -> {}",
            frame.image.size[0],
            frame.image.size[1],
            PathBuf::from(&out).display()
        );
    }

    // ---- Native-window live A/B proof ----------------------------------------

    fn save_color_image_png(image: &ColorImage, path: &Path) {
        let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
        for pixel in &image.pixels {
            rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
        }
        let out = image::RgbaImage::from_raw(image.size[0] as u32, image.size[1] as u32, rgba)
            .expect("proof image dimensions match");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("proof png parent dir");
        }
        out.save(path).expect("proof png writes");
        eprintln!(
            "WINDOW PROOF wrote {}x{} -> {}",
            image.size[0],
            image.size[1],
            path.display()
        );
    }

    /// LIVE A/B proof of the native-resolution window: over ONE pinned scan,
    /// ingest (a) today's default path (downsample 4) and (b) the native
    /// window, then export three PNGs — `<prefix>_default_ds4.png` (the
    /// whole default frame), `<prefix>_default_ds4_zoom.png` (the window cut
    /// out of the ds4 frame and nearest-neighbor upscaled to the native
    /// frame's scale: exactly what today's app shows zoomed into a typhoon
    /// eye) and `<prefix>_native_window.png`. Gated behind
    /// `BOWECHO_SAT_WINDOW_PROOF_DIR` so CI stays offline.
    ///
    /// Env: BOWECHO_SAT_WINDOW_PROOF_DIR (out dir; store lands under
    /// `<dir>/store`), BOWECHO_SAT_WINDOW_SOURCE (`himawari` default |
    /// `goes`), BOWECHO_SAT_WINDOW_LAT/LON/KM (default 13.5 / 144.8 / 800),
    /// BOWECHO_SAT_WINDOW_AS_OF (RFC3339 scan pin; default now — pick a
    /// DAYLIGHT pass, true color is dark at night),
    /// BOWECHO_SAT_WINDOW_PREFIX (default the source),
    /// BOWECHO_SAT_WINDOW_GOES_SAT/SECTOR (goes19 / conus).
    #[test]
    fn export_sat_native_window_proof_when_env_is_set() {
        let Some(dir) = std::env::var_os("BOWECHO_SAT_WINDOW_PROOF_DIR").map(PathBuf::from) else {
            return;
        };
        let env_f64 = |key: &str, default: f64| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let source =
            std::env::var("BOWECHO_SAT_WINDOW_SOURCE").unwrap_or_else(|_| "himawari".to_string());
        let prefix = std::env::var("BOWECHO_SAT_WINDOW_PREFIX").unwrap_or_else(|_| source.clone());
        let window = SatNativeWindow {
            center_lat_deg: env_f64("BOWECHO_SAT_WINDOW_LAT", 13.5),
            center_lon_deg: env_f64("BOWECHO_SAT_WINDOW_LON", 144.8),
            size_km: env_f64("BOWECHO_SAT_WINDOW_KM", 800.0),
        }
        .clamped();
        let as_of = std::env::var("BOWECHO_SAT_WINDOW_AS_OF").ok().map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .expect("BOWECHO_SAT_WINDOW_AS_OF must be RFC3339")
                .with_timezone(&Utc)
        });
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("proof store dir");
        let sink = |response: SatResponse| {
            if let SatResponse::Note(message) = &response {
                eprintln!("WINDOW PROOF note: {message}");
            }
            true
        };

        let slug = window.run_slug();
        // The store dir may hold runs from earlier proof invocations of the
        // OTHER source; every lookup below filters by this model.
        let expected_model = if source == "goes" {
            let base = GoesCompositeSpec {
                satellite: std::env::var("BOWECHO_SAT_WINDOW_GOES_SAT")
                    .unwrap_or_else(|_| "goes19".to_string()),
                sector: std::env::var("BOWECHO_SAT_WINDOW_GOES_SECTOR")
                    .unwrap_or_else(|_| "conus".to_string()),
                style: "natural_color".to_string(),
                downsample: 4,
                lookback_minutes: 360,
                window: None,
                as_of,
                card_ticket: None,
            };
            let summary =
                ingest_latest_goes_composite(&store, &base, &sink).expect("default GOES ingest");
            eprintln!("WINDOW PROOF default: {summary}");
            let windowed = GoesCompositeSpec {
                window: Some(window),
                ..base.clone()
            };
            let summary = ingest_latest_goes_composite(&store, &windowed, &sink)
                .expect("windowed GOES ingest");
            eprintln!("WINDOW PROOF native: {summary}");
            GoesSatellite::parse(&base.satellite)
                .as_str()
                .to_ascii_lowercase()
        } else {
            // The default fetch uses the SAME segment range the window
            // needs, so the A/B pair covers the same ground with today's
            // default processing vs the native window.
            let (seg_start, seg_count) =
                himawari_window_segments(window).expect("window visible from Himawari");
            let base = HimawariCompositeSpec {
                segment_start: seg_start,
                segment_count: seg_count,
                lookback_minutes: 360,
                downsample: 4,
                window: None,
                as_of,
                ..HimawariCompositeSpec::default()
            };
            let summary =
                ingest_latest_himawari_composite(&store, &base, &sink).expect("default AHI ingest");
            eprintln!("WINDOW PROOF default: {summary}");
            let windowed = HimawariCompositeSpec {
                window: Some(window),
                ..base.clone()
            };
            let summary = ingest_latest_himawari_composite(&store, &windowed, &sink)
                .expect("windowed AHI ingest");
            eprintln!("WINDOW PROOF native: {summary}");
            HimawariSatellite::parse(&base.satellite)
                .expect("himawari satellite parses")
                .slug()
                .to_string()
        };

        // Locate the two freshly written composite runs (this source's
        // model only; windowed runs carry the window token).
        let runs = scan_runs(&store);
        let windowed_run = runs
            .iter()
            .find(|run| run.key.model == expected_model && run.key.run.contains(&slug))
            .expect("a windowed composite run was written");
        let default_run = runs
            .iter()
            .find(|run| {
                run.key.model == expected_model
                    && run.key.run.contains("_rgb_")
                    && !run.key.run.contains("_win")
            })
            .expect("a default composite run was written");

        let mut state = WorkerState::default();
        let native_hhmm = *windowed_run.frames.last().expect("windowed run has frames");
        let native = load_frame_for_map(&mut state, &store, &windowed_run.key, native_hhmm)
            .expect("native frame loads");
        let default_hhmm = *default_run.frames.last().expect("default run has frames");
        let whole = load_frame_for_map(&mut state, &store, &default_run.key, default_hhmm)
            .expect("default frame loads");

        save_color_image_png(
            &native.image,
            &dir.join(format!("{prefix}_native_window.png")),
        );
        save_color_image_png(&whole.image, &dir.join(format!("{prefix}_default_ds4.png")));

        // Cut the same window out of the default frame (per-pixel grid
        // lookup, honoring display row order) and nearest-neighbor upscale
        // to the native frame's scale.
        let (dlat, dlon) = window.half_extent_deg();
        let (nx, ny) = (whole.image.size[0], whole.image.size[1]);
        let (mut row_min, mut row_max, mut col_min, mut col_max) =
            (usize::MAX, 0usize, usize::MAX, 0usize);
        for grid_row in 0..ny {
            let image_row = if whole.flip_rows {
                ny - 1 - grid_row
            } else {
                grid_row
            };
            for col in 0..nx {
                let idx = grid_row * nx + col;
                let (lat, lon) = (
                    f64::from(whole.grid.lat[idx]),
                    f64::from(whole.grid.lon[idx]),
                );
                if !(lat.is_finite() && lon.is_finite()) {
                    continue;
                }
                let dlon_here = (lon - window.center_lon_deg + 180.0).rem_euclid(360.0) - 180.0;
                if (lat - window.center_lat_deg).abs() <= dlat && dlon_here.abs() <= dlon {
                    row_min = row_min.min(image_row);
                    row_max = row_max.max(image_row);
                    col_min = col_min.min(col);
                    col_max = col_max.max(col);
                }
            }
        }
        assert!(
            row_min <= row_max && col_min <= col_max,
            "the default frame covers the window"
        );
        let crop_w = col_max - col_min + 1;
        let crop_h = row_max - row_min + 1;
        let scale = ((native.image.size[0] as f64) / (crop_w as f64))
            .round()
            .clamp(1.0, 16.0) as usize;
        let (out_w, out_h) = (crop_w * scale, crop_h * scale);
        let mut pixels = Vec::with_capacity(out_w * out_h);
        for out_row in 0..out_h {
            let src_row = row_min + out_row / scale;
            for out_col in 0..out_w {
                let src_col = col_min + out_col / scale;
                pixels.push(whole.image.pixels[src_row * nx + src_col]);
            }
        }
        let zoom = ColorImage::new([out_w, out_h], pixels);
        save_color_image_png(&zoom, &dir.join(format!("{prefix}_default_ds4_zoom.png")));
        eprintln!(
            "WINDOW PROOF summary: native {}x{} vs default crop {crop_w}x{crop_h} upscaled {scale}x",
            native.image.size[0], native.image.size[1],
        );
    }

    // ---- Full-disk true-color live proof --------------------------------------

    /// End-to-end FULL-DISK proof on LIVE Himawari open data: ingest one
    /// whole-disk true-color frame through the strided full-disk assemble,
    /// prove the loop contract by ingesting the PREVIOUS 10-minute scan into
    /// the same run, and export the disk PNG plus a 2× zoom crop of a
    /// daylight region. Gated behind `BOWECHO_SAT_FULLDISK_PROOF_DIR` so CI
    /// stays offline.
    ///
    /// Env: BOWECHO_SAT_FULLDISK_PROOF_DIR (out dir; store lands under
    /// `<dir>/store`), BOWECHO_SAT_FULLDISK_AS_OF (RFC3339 scan pin; default
    /// now — pick a time with the WPac/Australia side lit, and not within
    /// 10 min after 00:00Z: the loop assert needs both scans on one UTC day),
    /// BOWECHO_SAT_FULLDISK_DOWNSAMPLE (default 4 → 2750²; 2 → 5500²),
    /// BOWECHO_SAT_FULLDISK_ZOOM_LAT/LON/HALF_DEG (crop center / half-size,
    /// default 0 / 130 / 15 — Indonesia / New Guinea daytime convection).
    #[test]
    fn export_himawari_fulldisk_truecolor_proof_when_env_is_set() {
        let Some(dir) = std::env::var_os("BOWECHO_SAT_FULLDISK_PROOF_DIR").map(PathBuf::from)
        else {
            return;
        };
        let env_f64 = |key: &str, default: f64| {
            std::env::var(key)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let as_of = std::env::var("BOWECHO_SAT_FULLDISK_AS_OF").ok().map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .expect("BOWECHO_SAT_FULLDISK_AS_OF must be RFC3339")
                .with_timezone(&Utc)
        });
        let store = dir.join("store");
        std::fs::create_dir_all(&store).expect("proof store dir");
        let sink = |response: SatResponse| {
            if let SatResponse::Note(message) = &response {
                eprintln!("FULLDISK note: {message}");
            }
            true
        };

        let spec = HimawariCompositeSpec {
            full_disk: true,
            downsample: env_f64("BOWECHO_SAT_FULLDISK_DOWNSAMPLE", 4.0) as usize,
            lookback_minutes: 360,
            as_of,
            ..HimawariCompositeSpec::default()
        };
        let started = Instant::now();
        let summary = ingest_latest_himawari_composite(&store, &spec, &sink)
            .expect("live full-disk AHI composite ingest");
        eprintln!(
            "FULLDISK ingest #1 in {:.1}s: {summary}",
            started.elapsed().as_secs_f64()
        );

        let (key, first_hhmm) = {
            let runs = scan_runs(&store);
            let run = runs
                .iter()
                .find(|run| run.key.run.starts_with("fulldisk_rgb_true_color_"))
                .expect("a full-disk composite run was written");
            (
                run.key.clone(),
                *run.frames.last().expect("run has a frame"),
            )
        };
        let scan1 = rw_sat::store::frame_time(&key.run, first_hhmm).expect("frame time parses");

        // Loop proof: pin just before scan #1 so the previous 10-minute slot
        // is picked; its frame must stack into the SAME run dir (which also
        // proves the header-built grid is bit-identical across scans).
        let second = HimawariCompositeSpec {
            as_of: Some(scan1 - chrono::Duration::minutes(1)),
            ..spec.clone()
        };
        let started = Instant::now();
        let summary = ingest_latest_himawari_composite(&store, &second, &sink)
            .expect("second full-disk scan ingests");
        eprintln!(
            "FULLDISK ingest #2 in {:.1}s: {summary}",
            started.elapsed().as_secs_f64()
        );
        let runs = scan_runs(&store);
        let fulldisk_runs: Vec<_> = runs
            .iter()
            .filter(|run| run.key.run.starts_with("fulldisk_rgb_true_color_"))
            .collect();
        assert_eq!(
            fulldisk_runs.len(),
            1,
            "both scans share one loopable run family"
        );
        assert!(
            fulldisk_runs[0].frames.len() >= 2,
            "the second scan joined the run: {:?}",
            fulldisk_runs[0].frames
        );

        let mut state = WorkerState::default();
        let whole = load_frame_for_map(&mut state, &store, &key, first_hhmm)
            .expect("full-disk frame loads");
        let (nx, ny) = (whole.image.size[0], whole.image.size[1]);
        let lit = whole.image.pixels.iter().filter(|p| p.a() > 0).count();
        // The earth disk fills ~78% of the square frame (night side is
        // opaque near-black, off-earth is transparent).
        assert!(
            lit > nx * ny / 2,
            "the disk is composed: {lit} lit of {}",
            nx * ny
        );
        eprintln!(
            "FULLDISK {nx}x{ny}, {:.0}% lit, ~{:.0} MB per f32 plane, \
             ~{:.0} MB peak compose transient by array math (count planes + \
             reflectance/base planes + RGB planes + lat/lon mesh)",
            100.0 * lit as f64 / (nx * ny).max(1) as f64,
            (nx * ny * 4) as f64 / 1.0e6,
            (nx * ny * 4) as f64 * 9.0 / 1.0e6,
        );
        save_color_image_png(&whole.image, &dir.join("fulldisk_true_color.png"));

        // 2× nearest-neighbor zoom of a daylight region (per-pixel grid
        // lookup honoring display row order, like the window proof).
        let (zoom_lat, zoom_lon, half_deg) = (
            env_f64("BOWECHO_SAT_FULLDISK_ZOOM_LAT", 0.0),
            env_f64("BOWECHO_SAT_FULLDISK_ZOOM_LON", 130.0),
            env_f64("BOWECHO_SAT_FULLDISK_ZOOM_HALF_DEG", 15.0),
        );
        let (mut row_min, mut row_max, mut col_min, mut col_max) =
            (usize::MAX, 0usize, usize::MAX, 0usize);
        for grid_row in 0..ny {
            let image_row = if whole.flip_rows {
                ny - 1 - grid_row
            } else {
                grid_row
            };
            for col in 0..nx {
                let idx = grid_row * nx + col;
                let (lat, lon) = (
                    f64::from(whole.grid.lat[idx]),
                    f64::from(whole.grid.lon[idx]),
                );
                if !(lat.is_finite() && lon.is_finite()) {
                    continue;
                }
                let dlon = (lon - zoom_lon + 180.0).rem_euclid(360.0) - 180.0;
                if (lat - zoom_lat).abs() <= half_deg && dlon.abs() <= half_deg {
                    row_min = row_min.min(image_row);
                    row_max = row_max.max(image_row);
                    col_min = col_min.min(col);
                    col_max = col_max.max(col);
                }
            }
        }
        assert!(
            row_min <= row_max && col_min <= col_max,
            "the zoom box lands on the disk"
        );
        let (crop_w, crop_h) = (col_max - col_min + 1, row_max - row_min + 1);
        let mut pixels = Vec::with_capacity(crop_w * crop_h * 4);
        for out_row in 0..crop_h * 2 {
            let src_row = row_min + out_row / 2;
            for out_col in 0..crop_w * 2 {
                let src_col = col_min + out_col / 2;
                pixels.push(whole.image.pixels[src_row * nx + src_col]);
            }
        }
        let zoom = ColorImage::new([crop_w * 2, crop_h * 2], pixels);
        save_color_image_png(&zoom, &dir.join("fulldisk_true_color_zoom.png"));
    }

    /// Load one stored BT frame, print/assert absolute-Kelvin validation
    /// stats (coldest convective top on the disk + the clear-sky warm end
    /// inside a known tropical ocean box), and export the proof PNG set:
    /// the legacy auto-stretch "before" plus BD / CIMSS / Funktop / AVN.
    fn export_ir_proof_set(
        store: &Path,
        key: &SatRunKey,
        hhmm: u16,
        out_dir: &Path,
        stem: &str,
        ocean_box: (f32, f32, f32, f32), // lat_min, lat_max, lon_min, lon_max
    ) {
        let run_dir = store.join(&key.model).join(&key.run);
        let reader = HourReader::open(&run_dir.join(frame_file_name(hhmm))).expect("frame opens");
        let meta = reader.meta();
        let variable = meta
            .variables
            .iter()
            .find(|var| var.kind == "surface2d")
            .expect("frame holds a 2D variable");
        let band = selector_band(&variable.selector, &variable.name).expect("band selector");
        let name = variable.name.clone();
        let (nx, ny) = (meta.nx, meta.ny);
        let values = reader.read_full_2d(&name).expect("frame values read");

        let grid = GridFile::open(&run_dir.join("grid.rwg")).expect("run grid opens");
        let flip_rows = grid.lat_descending() == Some(false);
        let mut coldest = f32::INFINITY;
        let mut ocean: Vec<f32> = Vec::new();
        for (index, &bt) in values.iter().enumerate() {
            if !bt.is_finite() {
                continue;
            }
            // Stats over GEOLOCATED pixels only: a handful of limb/space
            // pixels carry valid-looking counts whose radiance is near zero
            // (BT < 100 K); their scan angles miss the earth, so the stored
            // mesh marks them NaN and they must not pollute the coldest-top
            // validation.
            let (lat, lon) = (grid.lat[index], grid.lon[index]);
            if !(lat.is_finite() && lon.is_finite()) {
                continue;
            }
            coldest = coldest.min(bt);
            if lat >= ocean_box.0 && lat <= ocean_box.1 && lon >= ocean_box.2 && lon <= ocean_box.3
            {
                ocean.push(bt);
            }
        }
        ocean.sort_by(|a, b| a.total_cmp(b));
        let warm = ocean
            .get((ocean.len().saturating_sub(1)) * 99 / 100)
            .copied()
            .unwrap_or(f32::NAN);
        eprintln!(
            "IRPAL {stem} {nx}x{ny} band={band} var={name}: coldest top {coldest:.2} K, \
             clear-sky tropical-ocean p99 {warm:.2} K ({} box samples)",
            ocean.len()
        );
        assert!(
            (160.0..=235.0).contains(&coldest),
            "coldest convective top is plausible Kelvin: {coldest}"
        );
        assert!(
            (280.0..=300.0).contains(&warm),
            "clear-sky tropical ocean warm end is plausible Kelvin: {warm}"
        );

        let save = |suffix: &str, pixels: Vec<Color32>| {
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for pixel in &pixels {
                rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
            }
            let image =
                image::RgbaImage::from_raw(nx as u32, ny as u32, rgba).expect("proof image dims");
            let path = out_dir.join(format!("{stem}_{suffix}.png"));
            image.save(&path).expect("proof png writes");
            eprintln!("IRPAL wrote {}", path.display());
        };
        save(
            "before_stretch",
            render_ahi_legacy_stretch(&values, nx, ny, flip_rows, false),
        );
        for (suffix, enhancement) in [
            ("bd", IrEnhancement::Bd),
            ("cimss", IrEnhancement::Cimss),
            ("funktop", IrEnhancement::Funktop),
            ("avn", IrEnhancement::Avn),
        ] {
            save(
                suffix,
                render_sat_pixels(&name, band, &values, nx, ny, flip_rows, false, enhancement).0,
            );
        }
    }

    /// End-to-end proof on LIVE open data for the true-Kelvin + enhancement
    /// work: ingest the latest Himawari-9 B13 full disk through the block-5
    /// calibration and the latest GOES-19 full-disk C13, validate absolute
    /// BT (clear-sky tropical ocean ~285-295 K, coldest convective top
    /// ~180-210 K), and export before/after enhancement PNGs. Gated behind
    /// `BOWECHO_IR_PALETTE_PROOF_DIR` so CI stays offline.
    #[test]
    fn export_ir_palette_proof_pngs_when_env_is_set() {
        let Some(out_dir) = std::env::var_os("BOWECHO_IR_PALETTE_PROOF_DIR").map(PathBuf::from)
        else {
            return;
        };
        std::fs::create_dir_all(&out_dir).expect("proof output dir");
        let store = std::env::temp_dir().join("bowecho-ir-palette-proof-store");
        std::fs::create_dir_all(&store).expect("proof store dir");
        let sink = |response: SatResponse| {
            if let SatResponse::Note(message) = &response {
                eprintln!("IRPAL note: {message}");
            }
            true
        };

        // Himawari-9 B13 full disk through the true-Kelvin ingest.
        let himawari_spec = HimawariQuickSpec {
            band: 13,
            downsample: 4,
            ..HimawariQuickSpec::default()
        };
        let summary =
            ingest_latest_himawari(&store, &himawari_spec, &sink).expect("live Himawari ingest");
        eprintln!("IRPAL himawari: {summary}");
        let runs = scan_runs(&store);
        let run = runs
            .iter()
            .find(|run| run.key.model == "h9" || run.key.model == "h8")
            .expect("a Himawari run was written");
        let hhmm = *run.frames.last().expect("Himawari run has a frame");
        // West-Pacific tropical ocean east of the Philippines (Guam-ish).
        export_ir_proof_set(
            &store,
            &run.key,
            hhmm,
            &out_dir,
            "himawari_b13",
            (5.0, 20.0, 135.0, 160.0),
        );

        // GOES-19 C13 full disk: one poll through the follow engine (the
        // same path LoadLoop uses).
        let mut goes_spec = spec();
        goes_spec.sector = "fulldisk".to_string();
        goes_spec.layer = "c13".to_string();
        goes_spec.downsample = 4;
        let mut config = follow_config(&goes_spec, &store).expect("GOES spec resolves");
        config.max_polls = Some(1);
        config.max_frames = Some(1);
        config.poll_interval = Some(Duration::from_secs(1));
        config.jitter_frac = 0.0;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut goes_sink = |event: SatEvent| {
            if let SatEvent::Info { message } = &event {
                eprintln!("IRPAL goes: {message}");
            }
        };
        rw_sat::follow(&config, &mut goes_sink, &cancel).expect("live GOES follow");
        let runs = scan_runs(&store);
        let run = runs
            .iter()
            .find(|run| run.key.model == "g19")
            .expect("a GOES-19 run was written");
        let hhmm = *run.frames.last().expect("GOES run has a frame");
        // Tropical Atlantic ocean northeast of South America.
        export_ir_proof_set(
            &store,
            &run.key,
            hhmm,
            &out_dir,
            "goes19_b13",
            (5.0, 20.0, -55.0, -40.0),
        );
    }

    fn tc_card_window(lat: f64, lon: f64) -> SatNativeWindow {
        SatNativeWindow {
            center_lat_deg: lat,
            center_lon_deg: lon,
            size_km: 1000.0,
        }
    }

    /// The tropical-card side channel reports EXACTLY the ticketed one-shot
    /// ingests, in order: an unticketed request (a regular panel button)
    /// must stay silent, and outcomes carry the request's own ticket. Both
    /// failures here are offline validation failures (non-IR band), so no
    /// network is touched.
    #[test]
    fn card_outcomes_report_only_ticketed_ingests() {
        let dir = test_dir("card-outcomes");
        let worker = SatWorker::spawn(dir.clone(), || {});
        worker.send(SatRequest::IngestLatestGoesIrWindow(GoesIrWindowSpec {
            satellite: "goes19".to_string(),
            sector: "fulldisk".to_string(),
            band: 2,
            window: tc_card_window(25.0, -80.0),
            lookback_minutes: 60,
            as_of: None,
            card_ticket: Some(41),
        }));
        worker.send(SatRequest::IngestLatestHimawariIrWindow(
            HimawariIrWindowSpec {
                satellite: "h9".to_string(),
                band: 2,
                window: tc_card_window(13.5, 144.8),
                lookback_minutes: 60,
                as_of: None,
                card_ticket: None,
            },
        ));
        worker.send(SatRequest::IngestLatestHimawariIrWindow(
            HimawariIrWindowSpec {
                satellite: "h9".to_string(),
                band: 2,
                window: tc_card_window(13.5, 144.8),
                lookback_minutes: 60,
                as_of: None,
                card_ticket: Some(43),
            },
        ));

        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while outcomes.len() < 2 && Instant::now() < deadline {
            match worker.try_recv_card_outcome() {
                Some(outcome) => outcomes.push(outcome),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(outcomes.len(), 2, "two ticketed ingests, two outcomes");
        assert_eq!(outcomes[0].ticket, 41);
        assert!(
            outcomes[0]
                .result
                .as_ref()
                .expect_err("C02 is not an IR band")
                .contains("not an IR band"),
            "{:?}",
            outcomes[0].result
        );
        // The middle (unticketed) request produced nothing: the next
        // outcome is the third request's.
        assert_eq!(outcomes[1].ticket, 43, "unticketed ingests stay silent");
        assert!(outcomes[1].result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The GOES enhanced-IR bake + write path end to end (offline): BD
    /// colors over Kelvin BT, NaN stays transparent, the run lands in the
    /// `_rgb_ir13_` family (the token that passes the app's composite
    /// display filter), and the player load path renders it.
    #[test]
    fn goes_ir_window_bake_writes_a_recognized_rgb_frame() {
        let dir = test_dir("ir-window-bake");
        let (nx, ny) = (2usize, 2usize);
        let mut field = synthetic_field(nx, ny, 6, 30, 13);
        // Overshooting top, eyewall cold, warm ocean, off-earth.
        field.values = vec![190.0, 220.0, 295.0, f32::NAN];

        let (r, g, b, lit) = bake_ir_planes(&field.values, 13, IrEnhancement::Bd);
        assert_eq!(lit, 3, "three finite BT pixels");
        assert!(
            r[3].is_nan() && g[3].is_nan() && b[3].is_nan(),
            "off-earth stays NaN in all planes"
        );

        let selector = serde_json::json!({ "satellite": {
            "band": 13,
            "enhanced_ir": { "enhancement": IrEnhancement::Bd.slug() },
        }});
        let frame =
            write_goes_rgb_frame(&dir, &field.scene, "rgb_ir13", selector, &r, &g, &b, 1).unwrap();
        assert_eq!(frame.model, "g19");
        assert!(
            frame.run.contains("_rgb_ir13_"),
            "enhanced-IR family naming: {}",
            frame.run
        );
        let title = run_title(&frame.model, &frame.run);
        assert!(title.contains("Enhanced IR C13"), "{title}");

        // Loads through the exact player path (composite branch: the frame
        // holds rgb_r/g/b planes, honoring the _rgb_ naming contract).
        let mut state = WorkerState::default();
        let key = SatRunKey {
            model: frame.model.clone(),
            run: frame.run.clone(),
        };
        let loaded = load_frame(&mut state, &dir, &key, frame.hhmm)
            .expect("baked IR frame loads")
            .frame;
        assert_eq!(loaded.image.size, [nx, ny]);
        // North-first synthetic grid: no row flip, indices map 1:1.
        assert_eq!(loaded.image.pixels[0].a(), 255, "cold top is opaque");
        assert_eq!(
            loaded.image.pixels[3].a(),
            0,
            "off-earth pixel renders transparent"
        );
        assert_ne!(
            loaded.image.pixels[0], loaded.image.pixels[2],
            "BD colors 190 K and 295 K differently"
        );
        let stored = rw_sat::store::read_frame(&dir, &frame.model, &frame.run, frame.hhmm).unwrap();
        assert_eq!(
            stored.selector["satellite"]["enhanced_ir"]["enhancement"],
            "bd"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Offline validation of the IR-window ingests: non-IR bands are
    /// refused before any network touch (both satellites), and the run-title
    /// band scan understands windowed single-band run names.
    #[test]
    fn ir_window_specs_validate_bands_and_titles_offline() {
        let dir = test_dir("ir-window-validate");
        let sink = |_: SatResponse| true;
        let goes = GoesIrWindowSpec {
            satellite: "goes19".to_string(),
            sector: "fulldisk".to_string(),
            band: 2,
            window: tc_card_window(25.0, -80.0),
            lookback_minutes: 60,
            as_of: None,
            card_ticket: None,
        };
        let err = ingest_latest_goes_ir_window(&dir, &goes, IrEnhancement::Bd, &sink)
            .expect_err("C02 refused");
        assert!(err.contains("not an IR band"), "{err}");

        let himawari = HimawariIrWindowSpec {
            satellite: "h9".to_string(),
            band: 3,
            window: tc_card_window(13.5, 144.8),
            lookback_minutes: 60,
            as_of: None,
            card_ticket: None,
        };
        let err =
            ingest_latest_himawari_ir_window(&dir, &himawari, &sink).expect_err("B03 refused");
        assert!(err.contains("not an IR band"), "{err}");

        // A windowed single-band Himawari run titles with its band + the
        // full windowed sector token (two windows never share a title).
        let title = run_title("h9", "fulldisk_win135n1448e800_c13_20260707");
        assert!(
            title.contains("C13") && title.contains("win135n1448e800"),
            "{title}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
