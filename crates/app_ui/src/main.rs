use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use app_ui::PanelLayout;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use color_tables::{ColorTable, ColorTableFamily, ColorTableSet, builtin_tables_for_family};
use data_source::{LEVEL2_ARCHIVE_BUCKET, RadarSite, RealtimeChunkType};
use eframe::egui;
use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType, RadarVolume};
use render2d::{
    ECHO_TOP_THRESHOLD_DBZ, StormCell, StormMotion, StormRelativePaletteCache, ViewportMomentCache,
    ViewportRasterOptions, ViewportSampleCache, VolumeDealiasCache, azimuthal_shear_grid,
    color_family_for_moment, composite_reflectivity_grid, dealias_velocity_grid,
    detect_rotation_sites, echo_top_grid, identify_storm_cells, mehs_grid, radial_divergence_grid,
    reflectivity_cross_section, storm_relative_velocity_mps, velocity_cross_section_cached,
    viewport_rgba_buffer_len, viewport_sample_cache_storage_upper_bound, vil_density_grid,
    vil_grid,
};
use serde::Deserialize;

mod basemap_data;
mod placefiles;

const MIN_DISPLAYABLE_RADIALS: usize = 180;
const DEFAULT_MAP_SCALE: f32 = 115.0;
const MIN_MAP_SCALE: f32 = 2.0;
const MAX_MAP_SCALE: f32 = 3_200.0;
const DEFAULT_RADAR_RANGE_KM: f32 = 460.0;
/// Top of the vertical cross-section (m above the radar) — shared by the
/// compute and the panel's height-axis labels so they can't drift.
const CROSS_SECTION_TOP_M: f32 = 18_000.0;
const DEFAULT_STORM_MOTION_DIRECTION_DEG: f32 = 45.0;
const DEFAULT_STORM_MOTION_SPEED_KT: f32 = 35.0;
const KNOT_TO_MPS: f32 = 0.514_444;
const VROT_ROW_RADIUS: usize = 2;
const VROT_GATE_RADIUS: usize = 4;
const SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS: u64 = 720 * 480;
const LOW_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS: f32 = 4.0;
const HIGH_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS: f32 = 0.25;
const LOW_END_SAMPLE_CACHE_BYTES: usize = 6 * 1024 * 1024;
const LOW_END_SAMPLE_CACHE_BUILD_BYTES: usize = LOW_END_SAMPLE_CACHE_BYTES * 2;
const MID_RANGE_SAMPLE_CACHE_BYTES: usize = 24 * 1024 * 1024;
const HIGH_END_SAMPLE_CACHE_BYTES: usize = 64 * 1024 * 1024;
const LOW_CORE_PREVIEW_THREADS: usize = 4;
const LOW_CORE_PREVIEW_RENDER_HEAD_START_MS: u64 = 8;
const ACTIVE_LOAD_POLL_MS: u64 = 8;
const LIVE_HAZARD_REFRESH_SECONDS: u64 = 60;
const PRIMARY_REALTIME_LEVEL2_REFRESH_SECONDS: u64 = 1;
const OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS: u64 = 5;
const MAX_RADAR_OVERLAY_LAYERS: usize = 10;
const DEFAULT_RADAR_OVERLAY_ALPHA: u8 = 210;
const MIN_RADAR_OVERLAY_ALPHA: u8 = 48;
const FRESH_RING_GREEN_SECONDS: i64 = 6 * 60;
const FRESH_RING_YELLOW_SECONDS: i64 = 10 * 60;
const FRESH_RING_RED_SECONDS: i64 = 15 * 60;
const PERF_SAMPLE_CAPACITY: usize = 96;
const STALE_LATEST_DISPLAY_CLEAR_SECONDS: i64 = 15 * 60;
const HISTORY_SIZE_OPTIONS: &[usize] = &[3, 5, 7, 10, 15, 20, 25, 30];
const DEFAULT_HISTORY_FRAME_LIMIT: usize = 7;
const HISTORY_LOOP_FRAME_MS: u64 = 700;
const LIVE_LOW_LEVEL_AUTO_ADVANCE_MAX_ELEVATION_DEG: f32 = 1.0;
const LIVE_LOW_LEVEL_AUTO_ADVANCE_MIN_SECONDS: i64 = 90;
const LIVE_COMPLETE_LOW_LEVEL_TILT_MIN_RADIALS: usize = 720;
const LIVE_COMPLETE_TILT_MIN_RADIALS: usize = 360;
const LIVE_COMPLETE_TILT_MIN_AZIMUTH_COVERAGE_DEG: f32 = 350.0;
const HISTORY_ARCHIVE_LOAD_MAX_PARALLELISM: usize = 6;
const ACTIVE_ALERTS_URL: &str = "https://api.weather.gov/alerts/active?status=actual";
const SPC_MD_INDEX_URL: &str = "https://www.spc.noaa.gov/products/md/";
const SPC_PRODUCT_BASE_URL: &str = "https://www.spc.noaa.gov";
const NWS_PRODUCT_API_BASE_URL: &str = "https://api.weather.gov/products/types";
const HOT_TEXT_PRODUCT_TYPES: &[&str] = &["TOR", "SVR", "SVS", "FFW", "FFS", "SMW", "SQW"];
const HOT_TEXT_PRODUCTS_MIN_PER_TYPE: usize = 4;
const HOT_TEXT_PRODUCTS_MAX_PER_TYPE: usize = 16;
const HOT_TEXT_PRODUCTS_RECENT_WINDOW_MINUTES: i64 = 60;
const HOT_TEXT_DETAIL_CACHE_MAX: usize = 512;
const HAZARD_CLICK_TOLERANCE_PX: f32 = 12.0;
const HAZARD_LABEL_CLICK_RADIUS_PX: f32 = 18.0;
const HAZARD_MAX_RENDER_LON_SPAN_DEG: f32 = 45.0;
const HAZARD_MAX_RENDER_LAT_SPAN_DEG: f32 = 30.0;
const HAZARD_MAX_RENDER_EDGE_KM: f32 = 2_500.0;
const MAP_DRAG_DEAD_ZONE_PX: f32 = 3.0;
const DEFAULT_HAZARD_FILL_ALPHA: u8 = 24;
const COLOR_STATUS_SCROLL_HEIGHT: f32 = 34.0;
const HAZARD_SUMMARY_SCROLL_HEIGHT: f32 = 86.0;
const HAZARD_DETAIL_SCROLL_HEIGHT: f32 = 150.0;
const TILT_LIST_SCROLL_HEIGHT: f32 = 168.0;
const HISTORY_STATUS_SCROLL_HEIGHT: f32 = 42.0;
const HISTORY_FRAME_BUTTONS_SCROLL_HEIGHT: f32 = 58.0;
const PANEL_BUTTON_HEIGHT: f32 = 24.0;
const SIDEBAR_DEFAULT_WIDTH: f32 = 380.0;
const SIDEBAR_MIN_WIDTH: f32 = 300.0;
const SIDEBAR_MAX_WIDTH: f32 = 560.0;
const DEFAULT_HIDDEN_HAZARD_FAMILIES: &[&str] = &[];
const HAZARD_FILTER_FAMILIES: &[(&str, &str)] = &[
    ("tornado", "TOR"),
    ("severe thunderstorm", "SVR"),
    ("flash flood", "FFW"),
    ("flood", "Flood"),
    ("special marine", "SMW"),
    ("snow squall", "SQW"),
    ("watch", "Watch"),
    ("mesoscale discussion", "MD"),
    ("special weather", "SPS"),
];
const BASEMAP_US_DETAIL_BOUNDS: &[[f32; 4]] = &[
    [-125.5, 24.0, -66.0, 50.3],
    [-171.0, 51.0, -129.0, 72.0],
    [-161.5, 18.5, -154.5, 23.0],
    [-68.5, 17.0, -64.0, 19.0],
];
const RAYON_NUM_THREADS_ENV: &str = "RAYON_NUM_THREADS";

fn main() -> eframe::Result {
    let input_path = std::env::args_os().nth(1).map(PathBuf::from);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1500.0, 950.0])
            .with_min_inner_size([1120.0, 700.0]),
        ..Default::default()
    };

    eframe::run_native(
        "BowEcho",
        native_options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, input_path)))),
    )
}

fn cache_dir(name: &str) -> PathBuf {
    app_cache_root()
        .join("level2")
        .join(sanitized_cache_segment(name))
}

fn app_cache_root() -> PathBuf {
    if let Ok(path) = std::env::var("RADAR_RS_ANALYST_CACHE_DIR")
        && !path.trim().is_empty()
    {
        return PathBuf::from(path);
    }

    #[cfg(windows)]
    if let Ok(path) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(path).join("BowEcho").join("cache");
    }

    #[cfg(not(windows))]
    if let Ok(path) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("bowecho");
    }

    #[cfg(not(windows))]
    if let Ok(path) = std::env::var("HOME") {
        return PathBuf::from(path).join(".cache").join("bowecho");
    }

    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("bowecho-cache")
}

fn sanitized_cache_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if segment.is_empty() {
        "unknown".to_owned()
    } else {
        segment
    }
}

fn should_preview_loads() -> bool {
    should_preview_loads_for_threads(effective_worker_threads())
}

fn should_preview_loads_for_threads(_threads: usize) -> bool {
    true
}

fn should_preview_block_bzip_loads_for_threads(_threads: usize) -> bool {
    // The block-bzip preview shares the full decode's pipeline now (engine
    // fast-path branch), so it is effectively free: enable it on every
    // machine for ~40ms first pixels rather than only on low-core CPUs.
    true
}

fn effective_worker_threads() -> usize {
    configured_rayon_threads_from(std::env::var(RAYON_NUM_THREADS_ENV).ok().as_deref())
        .or_else(|| thread::available_parallelism().ok().map(usize::from))
        .unwrap_or(1)
}

fn configured_rayon_threads_from(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|threads| *threads > 0)
}

fn preview_render_head_start(threads: usize) -> Duration {
    if threads <= LOW_CORE_PREVIEW_THREADS {
        Duration::from_millis(LOW_CORE_PREVIEW_RENDER_HEAD_START_MS)
    } else {
        Duration::ZERO
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_load_path_with_optional_preview(
    path: PathBuf,
    label: &str,
    total_start: Instant,
    mut timings: LoadTimings,
    sender: &mpsc::Sender<AsyncLoadResult>,
    preview_enabled: bool,
    status: FrameStatus,
    source_label: String,
) -> Result<DecodedLoad, String> {
    let read_start = Instant::now();
    let raw = std::fs::read(&path)
        .map_err(|err| format!("I/O error reading {}: {err}", path.display()))?;
    timings.read_ms = Some(read_start.elapsed().as_secs_f32() * 1000.0);

    if !preview_enabled {
        let decode_start = Instant::now();
        let mut volume =
            nexrad_io::decode_volume_from_bytes(&raw).map_err(|err| err.to_string())?;
        timings.decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
        volume.metadata.source_path = Some(path.display().to_string());
        return Ok(DecodedLoad {
            path,
            volume,
            timings: timings.finish(total_start),
            status,
            source_label,
        });
    }

    let worker_threads = effective_worker_threads();
    let preview_head_start = preview_render_head_start(worker_threads);
    let preview_path = path.clone();
    let preview_label = label.to_owned();
    let decode_start = Instant::now();
    let mut first_preview_ms = None;
    let mut send_preview = |mut preview: RadarVolume| {
        let preview_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
        first_preview_ms.get_or_insert(preview_ms);
        let mut preview_timings = timings;
        preview_timings.decode_ms = preview_ms;
        preview_timings.preview_ms = Some(preview_ms);
        preview.metadata.source_path = Some(preview_path.display().to_string());
        let sent = sender.send(AsyncLoadResult {
            label: preview_label.clone(),
            update: AsyncLoadUpdate::Preview(DecodedLoad {
                path: preview_path.clone(),
                volume: preview,
                timings: preview_timings.finish(total_start),
                status: FrameStatus::Preview,
                source_label: preview_label.clone(),
            }),
        });
        if sent.is_ok() && !preview_head_start.is_zero() {
            thread::sleep(preview_head_start);
        }
    };
    let mut volume = if raw.starts_with(&[0x1f, 0x8b]) {
        nexrad_io::decode_gzip_volume_from_bytes_with_preview(
            &raw,
            MIN_DISPLAYABLE_RADIALS,
            |preview| {
                send_preview(preview);
            },
        )
        .map_err(|err| err.to_string())?
    } else if should_preview_block_bzip_loads_for_threads(worker_threads) {
        nexrad_io::decode_volume_from_bytes_with_bzip_preview(
            &raw,
            MIN_DISPLAYABLE_RADIALS,
            |preview| {
                send_preview(preview);
            },
        )
        .map_err(|err| err.to_string())?
    } else {
        nexrad_io::decode_volume_from_bytes(&raw).map_err(|err| err.to_string())?
    };
    timings.decode_ms = decode_start.elapsed().as_secs_f32() * 1000.0;
    timings.preview_ms = first_preview_ms;
    volume.metadata.source_path = Some(path.display().to_string());
    Ok(DecodedLoad {
        path,
        volume,
        timings: timings.finish(total_start),
        status,
        source_label,
    })
}

struct ViewerApp {
    source_path: Option<PathBuf>,
    volume: Option<Arc<RadarVolume>>,
    selected_cut: usize,
    selected_product: DisplayProduct,
    frame_history: Vec<FrameHistoryEntry>,
    selected_frame_index: usize,
    history_frame_limit: usize,
    history_playing: bool,
    last_history_step: Option<Instant>,
    color_tables: ColorTableSet,
    flip_velocity_color_polarity: bool,
    unfold_velocity_display: bool,
    color_table_target: ColorTableFamily,
    color_table_path_text: String,
    color_table_status: String,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<TextureKey>,
    render_sender: mpsc::Sender<RenderRequest>,
    render_receiver: mpsc::Receiver<AsyncRenderResult>,
    render_recycle_sender: mpsc::Sender<RenderRecycleBuffer>,
    pending_render_key: Option<TextureKey>,
    map_center_lon: f32,
    map_center_lat: f32,
    map_scale: f32,
    radar_range_km: f32,
    load_timing: Option<LoadTimings>,
    active_load_started_at: Option<Instant>,
    first_data_ms: Option<f32>,
    first_texture_ms: Option<f32>,
    render_ms: Option<f32>,
    worker_ms: Option<f32>,
    texture_ms: Option<f32>,
    sample_cache_build_ms: Option<f32>,
    basemap_ms: Option<f32>,
    perf: PerfTelemetry,
    status: String,
    sites: Vec<RadarSite>,
    selected_site_index: usize,
    app_settings: settings::AppSettings,
    radar_layers: Vec<RadarOverlayLayer>,
    next_radar_layer_id: u64,
    site_catalog_receiver: Option<mpsc::Receiver<AsyncSiteCatalogResult>>,
    load_receiver: Option<mpsc::Receiver<AsyncLoadResult>>,
    hazard_receiver: Option<mpsc::Receiver<AsyncHazardResult>>,
    pending_site_id: Option<String>,
    cursor_readout: Option<CursorReadout>,
    /// Placefile overlays (GRLevelX-style community feeds).
    placefile_slots: Vec<PlacefileSlot>,
    placefile_url_input: String,
    placefile_shape_cache: std::cell::RefCell<ShapeCache<PlacefileDrawList>>,
    /// SCIT-style storm tracks: identification runs per volume on a
    /// background thread; association + motion fits are O(cells) on install.
    storm_tracks: Vec<StormTrack>,
    storm_tracks_site: String,
    next_storm_track_id: u32,
    storm_cells_volume_ptr: usize,
    storm_cells_receiver: Option<mpsc::Receiver<StormCellsResult>>,
    show_storm_tracks: bool,
    /// Rotation (meso/TVS) markers from the lowest velocity tilt (Stumpf et
    /// al. 1998 / Mitchell et al. 1998 thresholds on LLSD azimuthal shear).
    /// Detection runs on a BACKGROUND thread once per volume — the UI thread
    /// only spawns, polls, and draws (speed doctrine: zero hot-path cost).
    rotation_markers: Vec<RotationMarker>,
    rotation_markers_volume_ptr: usize,
    rotation_receiver: Option<mpsc::Receiver<(usize, Vec<RotationMarker>)>>,
    show_rotation_markers: bool,
    /// Hail environment for the Witt et al. 1998 MEHS product: melting-level
    /// and -20C heights above the radar, in km (set from a sounding).
    hail_freezing_level_km: f32,
    hail_minus20_level_km: f32,
    /// Render-time display thresholds per color-family label: values below
    /// (|values| below, for diverging families) draw transparent. Data is
    /// untouched — the inspector/readout still reports it.
    display_thresholds: BTreeMap<String, f32>,
    /// Floating inspector card at the cursor (Shift+click pins it to a spot).
    show_inspector_card: bool,
    pinned_inspector_lonlat: Option<(f32, f32)>,
    /// Bumped on every hazard_overlay assignment — exact invalidation for the
    /// hazard shape cache (content proxies like record counts can alias).
    hazard_overlay_generation: u64,
    // Multi-pane grid: layout + the extra synchronized panes (pane 0 is the
    // primary view's own state above).
    grid_layout: PanelLayout,
    extra_panes: Vec<ViewPane>,
    /// The last-clicked pane (0 = main). The sidebar product picker and tilt
    /// list drive this pane: the main pane edits the whole bunch, an extra
    /// pane edits itself independently.
    active_pane: usize,
    // Frame-cost caches (RefCell: draw fns take &self on the UI thread).
    basemap_shape_cache: std::cell::RefCell<ShapeCache<Vec<egui::Shape>>>,
    hazard_shape_cache: std::cell::RefCell<ShapeCache<HazardOverlayShapes>>,
    // Cross-section (RHI) draw mode + rendered section.
    cross_section_armed: bool,
    cross_section_a_lonlat: Option<(f32, f32)>,
    cross_section_b_lonlat: Option<(f32, f32)>,
    cross_section_texture: Option<egui::TextureHandle>,
    cross_section_signature: Option<u64>,
    cross_section_status: String,
    /// Height-axis ceiling of the current section (m); auto-scaled to the
    /// beam coverage along the drawn path so storms fill the panel.
    cross_section_top_m: f32,
    /// Signature of the user-controlled section inputs (endpoints, product,
    /// palette) and tilt count of the last volume sectioned — used to HOLD
    /// the section while a live volume streams in tilt-by-tilt instead of
    /// resetting it to a single-beam ribbon on every chunk.
    cross_section_user_signature: Option<u64>,
    cross_section_volume_cuts: usize,
    /// Per-volume dealias memo for velocity sections: endpoint drags pay the
    /// all-tilt dealias once per volume instead of every frame.
    cross_section_dealias_cache: VolumeDealiasCache,
    hazard_overlay: Option<HazardOverlay>,
    hazard_path_text: String,
    hazard_status: String,
    hazards_visible: bool,
    hazards_active_only: bool,
    hazard_fill_alpha: u8,
    hidden_hazard_families: BTreeSet<String>,
    realtime_level2_auto_refresh: bool,
    display_live_chunk_updates: bool,
    last_realtime_level2_refresh: Option<Instant>,
    live_refresh_skip_reason: Option<String>,
    live_hazard_auto_refresh: bool,
    show_performance_stats: bool,
    sidebar_tab: SidebarTab,
    last_live_hazard_refresh: Option<Instant>,
    selected_hazard_index: Option<usize>,
    storm_motion_direction_deg: f32,
    storm_motion_speed_kt: f32,
    dealiased_readout_cache: Option<DealiasedReadoutCache>,
}

struct RadarOverlayLayer {
    id: u64,
    site: RadarSite,
    source_path: Option<PathBuf>,
    volume: Option<Arc<RadarVolume>>,
    load_timing: Option<LoadTimings>,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<TextureKey>,
    render_sender: mpsc::Sender<RenderRequest>,
    render_receiver: mpsc::Receiver<AsyncRenderResult>,
    render_recycle_sender: mpsc::Sender<RenderRecycleBuffer>,
    pending_render_key: Option<TextureKey>,
    load_receiver: Option<mpsc::Receiver<AsyncLoadResult>>,
    status: String,
    last_realtime_level2_refresh: Option<Instant>,
    opacity: u8,
    visible: bool,
    radar_range_km: f32,
    render_ms: Option<f32>,
    worker_ms: Option<f32>,
    texture_ms: Option<f32>,
}

impl RadarOverlayLayer {
    fn new(id: u64, site: RadarSite) -> Self {
        let (render_sender, render_receiver, render_recycle_sender) = spawn_overlay_render_worker();
        let site_id = site.level2_id.clone();
        Self {
            id,
            site,
            source_path: None,
            volume: None,
            load_timing: None,
            texture: None,
            texture_key: None,
            render_sender,
            render_receiver,
            render_recycle_sender,
            pending_render_key: None,
            load_receiver: None,
            status: format!("Queued {site_id}"),
            last_realtime_level2_refresh: None,
            opacity: DEFAULT_RADAR_OVERLAY_ALPHA,
            visible: true,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
            render_ms: None,
            worker_ms: None,
            texture_ms: None,
        }
    }

    fn radar_location(&self) -> Option<(f32, f32)> {
        self.volume
            .as_ref()
            .and_then(|volume| Some((volume.site.latitude_deg?, volume.site.longitude_deg?)))
            .or_else(|| site_location(&self.site))
    }
}

struct AsyncLoadResult {
    label: String,
    update: AsyncLoadUpdate,
}

#[allow(clippy::large_enum_variant)]
enum AsyncLoadUpdate {
    Preview(DecodedLoad),
    History(DecodedLoadBatch, bool),
    Unchanged {
        timings: Option<LoadTimings>,
        reason: String,
    },
    Final(Result<DecodedLoadBatch, String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LatestLoadMode {
    User,
    Loop,
    AutoRefresh,
}

#[derive(Clone, Copy, Debug, Default)]
struct LoadTimings {
    total_ms: f32,
    lookup_ms: Option<f32>,
    lookup_cache_hit: Option<bool>,
    fetch_ms: Option<f32>,
    fetch_cache_hit: Option<bool>,
    read_ms: Option<f32>,
    decode_ms: f32,
    preview_ms: Option<f32>,
    realtime_poll_start_utc: Option<DateTime<Utc>>,
    realtime_poll_end_utc: Option<DateTime<Utc>>,
    realtime_volume_id: Option<u16>,
    realtime_chunk_count: Option<usize>,
    realtime_last_chunk_id: Option<u16>,
    realtime_last_chunk_type: Option<RealtimeChunkType>,
    realtime_complete: Option<bool>,
    realtime_total_size: Option<u64>,
    realtime_assembled_size: Option<u64>,
    realtime_last_modified_utc: Option<DateTime<Utc>>,
    realtime_volume_time_utc: Option<DateTime<Utc>>,
}

impl LoadTimings {
    fn finish(mut self, total_start: Instant) -> Self {
        self.total_ms = total_start.elapsed().as_secs_f32() * 1000.0;
        self
    }
}

#[derive(Clone, Debug)]
struct RealtimeLoadError {
    reason: String,
    timings: Option<LoadTimings>,
}

impl RealtimeLoadError {
    fn new(reason: String) -> Self {
        Self {
            reason,
            timings: None,
        }
    }

    fn with_timings(reason: String, timings: LoadTimings) -> Self {
        Self {
            reason,
            timings: Some(timings),
        }
    }
}

fn record_realtime_level2_metadata(
    timings: &mut LoadTimings,
    realtime: &data_source::RealtimeLevel2Volume,
) {
    let latest_chunk = realtime.chunks.last();
    timings.realtime_volume_id = Some(realtime.volume_id);
    timings.realtime_chunk_count = Some(realtime.chunks.len());
    timings.realtime_last_chunk_id = latest_chunk.map(|chunk| chunk.chunk_id);
    timings.realtime_last_chunk_type = latest_chunk.map(|chunk| chunk.chunk_type);
    timings.realtime_complete = Some(realtime.complete);
    timings.realtime_total_size = Some(realtime.total_size);
    timings.realtime_last_modified_utc = latest_chunk.and_then(|chunk| chunk.object.last_modified);
    timings.realtime_volume_time_utc = Some(realtime.volume_time);
}

struct AsyncSiteCatalogResult {
    result: Result<Vec<RadarSite>, String>,
}

struct AsyncHazardResult {
    update: AsyncHazardUpdate,
}

enum AsyncHazardUpdate {
    Preview(Result<HazardOverlay, String>),
    Final(Result<HazardOverlay, String>),
}

#[derive(Clone)]
struct DecodedLoad {
    path: PathBuf,
    volume: RadarVolume,
    timings: LoadTimings,
    status: FrameStatus,
    source_label: String,
}

#[derive(Clone)]
struct DecodedLoadBatch {
    frames: Vec<DecodedLoad>,
    selected_index: usize,
}

impl DecodedLoadBatch {
    fn single(decoded: DecodedLoad) -> Self {
        Self {
            frames: vec![decoded],
            selected_index: 0,
        }
    }

    fn into_selected(self) -> Option<DecodedLoad> {
        self.frames.into_iter().nth(self.selected_index)
    }
}

#[derive(Clone)]
struct FrameHistoryEntry {
    identity: FrameIdentity,
    path: PathBuf,
    volume: Arc<RadarVolume>,
    timings: Option<LoadTimings>,
    status: FrameStatus,
    source_label: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FrameIdentity {
    site_id: String,
    scan_time_utc: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameStatus {
    Local,
    Preview,
    LivePartial,
    LiveComplete,
    Complete,
    Stale,
}

impl FrameStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Preview => "preview",
            Self::LivePartial => "live partial",
            Self::LiveComplete => "live complete",
            Self::Complete => "complete",
            Self::Stale => "stale",
        }
    }
}

fn history_archive_load_parallelism() -> usize {
    history_archive_load_parallelism_for_threads(effective_worker_threads())
}

fn history_archive_load_parallelism_for_threads(threads: usize) -> usize {
    match threads {
        0..=2 => 1,
        3..=4 => 2,
        5..=7 => 3,
        _ => HISTORY_ARCHIVE_LOAD_MAX_PARALLELISM.min(threads),
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_archive_history_object(
    site_id: &str,
    object: data_source::S3Object,
    site_cache_dir: &Path,
    known_frame_paths: &BTreeSet<PathBuf>,
    archive_lookup_ms: Option<f32>,
    total_start: Instant,
    sender: &mpsc::Sender<AsyncLoadResult>,
    preview: bool,
) -> Result<Option<DecodedLoad>, String> {
    let mut timings = LoadTimings {
        lookup_ms: archive_lookup_ms,
        lookup_cache_hit: archive_lookup_ms.map(|_| false),
        ..Default::default()
    };

    let fetch_start = Instant::now();
    let downloaded = data_source::download_object(LEVEL2_ARCHIVE_BUCKET, object, site_cache_dir)
        .map_err(|err| err.to_string())?;
    timings.fetch_ms = Some(fetch_start.elapsed().as_secs_f32() * 1000.0);
    timings.fetch_cache_hit = Some(downloaded.cache_hit);

    if downloaded.cache_hit && known_frame_paths.contains(&downloaded.path) {
        return Ok(None);
    }

    let mut decoded = decode_load_path_with_optional_preview(
        downloaded.path,
        &format!("archive L2 {site_id}"),
        total_start,
        timings,
        sender,
        preview,
        FrameStatus::Complete,
        format!("archive L2 {site_id}"),
    )?;
    decoded.status =
        archive_frame_status(decoded.volume.volume_time.with_timezone(&Utc), Utc::now());
    if global_displayable_products(&decoded.volume).is_empty() {
        return Ok(None);
    }
    Ok(Some(decoded))
}

fn load_archive_history_objects_parallel(
    site_id: &str,
    site_cache_dir: &Path,
    objects: Vec<(usize, data_source::S3Object)>,
    known_frame_paths: &BTreeSet<PathBuf>,
    archive_lookup_ms: Option<f32>,
    total_start: Instant,
    sender: &mpsc::Sender<AsyncLoadResult>,
) -> (Vec<DecodedLoad>, Option<String>) {
    let parallelism = history_archive_load_parallelism();
    let priority_count = objects.len().min(parallelism.min(3));
    let mut decoded_frames = Vec::new();
    let mut first_error = None;
    let mut offset = 0;

    while offset < objects.len() {
        let chunk_size = if offset == 0 && priority_count > 0 {
            priority_count
        } else {
            parallelism
        };
        let end = offset.saturating_add(chunk_size).min(objects.len());
        let chunk = &objects[offset..end];
        let batch_results = thread::scope(|scope| {
            let mut workers = Vec::with_capacity(chunk.len());
            for (index, object) in chunk.iter().cloned() {
                let site_id = site_id.to_owned();
                let site_cache_dir = site_cache_dir.to_path_buf();
                let sender = sender.clone();
                workers.push(scope.spawn(move || {
                    decode_archive_history_object(
                        &site_id,
                        object,
                        &site_cache_dir,
                        known_frame_paths,
                        (index == 0).then_some(archive_lookup_ms).flatten(),
                        total_start,
                        &sender,
                        false,
                    )
                }));
            }

            let mut results = Vec::with_capacity(workers.len());
            for worker in workers {
                results.push(
                    worker
                        .join()
                        .map_err(|_| "archive history frame worker panicked".to_owned()),
                );
            }
            results
        });

        for result in batch_results {
            match result {
                Ok(Ok(Some(decoded))) => {
                    let _ = sender.send(AsyncLoadResult {
                        label: format!("L2 {site_id} loop frame"),
                        update: AsyncLoadUpdate::History(
                            DecodedLoadBatch {
                                frames: vec![decoded.clone()],
                                selected_index: 0,
                            },
                            false,
                        ),
                    });
                    decoded_frames.push(decoded);
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) | Err(err) => {
                    first_error.get_or_insert(err);
                }
            }
        }
        offset = end;
    }

    (decoded_frames, first_error)
}

#[allow(clippy::result_large_err, clippy::too_many_arguments)]
fn spawn_latest_level2_load_worker(
    site: RadarSite,
    mode: LatestLoadMode,
    current_source_path: Option<PathBuf>,
    known_frame_paths: BTreeSet<PathBuf>,
    current_frame_identity: Option<FrameIdentity>,
    history_limit: usize,
    display_live_chunk_updates: bool,
    sender: mpsc::Sender<AsyncLoadResult>,
) {
    thread::spawn(move || {
        let total_start = Instant::now();
        let site_id = site.level2_id.clone();
        let site_cache_dir = cache_dir(&site.level2_id);

        let final_update = (|| -> Result<AsyncLoadUpdate, String> {
            let history_limit = history_limit.max(1);
            let explicit_loop_load = mode == LatestLoadMode::Loop;
            let mut decoded_frames = Vec::new();
            let mut selected_identity = if explicit_loop_load {
                current_frame_identity.filter(|identity| identity.site_id == site_id)
            } else {
                None
            };
            let mut fallback_error = None;

            let should_load_realtime = !explicit_loop_load || selected_identity.is_none();
            if should_load_realtime {
                let realtime_result = (|| -> Result<DecodedLoad, RealtimeLoadError> {
                    let mut realtime_timings = LoadTimings::default();
                    let poll_start_utc = Utc::now();
                    realtime_timings.realtime_poll_start_utc = Some(poll_start_utc);
                    let lookup_start = Instant::now();
                    let realtime = data_source::latest_realtime_level2_volume(&site.level2_id)
                        .map_err(|err| RealtimeLoadError::new(err.to_string()))?;
                    realtime_timings.lookup_ms =
                        Some(lookup_start.elapsed().as_secs_f32() * 1000.0);
                    realtime_timings.lookup_cache_hit = Some(false);
                    record_realtime_level2_metadata(&mut realtime_timings, &realtime);

                    let fetch_start = Instant::now();
                    let downloaded =
                        data_source::download_realtime_volume(&realtime, &site_cache_dir).map_err(
                            |err| {
                                realtime_timings.realtime_poll_end_utc = Some(Utc::now());
                                RealtimeLoadError::with_timings(
                                    err.to_string(),
                                    realtime_timings.finish(total_start),
                                )
                            },
                        )?;
                    realtime_timings.fetch_ms = Some(fetch_start.elapsed().as_secs_f32() * 1000.0);
                    realtime_timings.fetch_cache_hit = Some(downloaded.cache_hit);
                    realtime_timings.realtime_assembled_size = downloaded
                        .path
                        .metadata()
                        .ok()
                        .map(|metadata| metadata.len());
                    if is_unchanged_realtime_refresh(
                        downloaded.cache_hit,
                        &downloaded.path,
                        current_source_path.as_deref(),
                    ) {
                        realtime_timings.realtime_poll_end_utc = Some(Utc::now());
                        return Err(RealtimeLoadError::with_timings(
                            "unchanged cache hit".to_owned(),
                            realtime_timings.finish(total_start),
                        ));
                    }

                    let decode_timings = realtime_timings;
                    let eager_realtime_display =
                        mode != LatestLoadMode::AutoRefresh || current_source_path.is_none();
                    let realtime_preview_enabled = should_preview_loads()
                        && eager_realtime_display
                        && display_live_chunk_updates;
                    let decoded = decode_load_path_with_optional_preview(
                        downloaded.path,
                        &format!("realtime L2 {site_id}"),
                        total_start,
                        decode_timings,
                        &sender,
                        realtime_preview_enabled,
                        if realtime.complete {
                            FrameStatus::LiveComplete
                        } else {
                            FrameStatus::LivePartial
                        },
                        format!("realtime L2 {site_id}"),
                    )
                    .map_err(|err| {
                        let mut timings = decode_timings;
                        timings.realtime_poll_end_utc = Some(Utc::now());
                        RealtimeLoadError::with_timings(err, timings.finish(total_start))
                    })?;
                    if global_displayable_products(&decoded.volume).is_empty() {
                        let mut timings = decoded.timings;
                        timings.realtime_poll_end_utc = Some(Utc::now());
                        return Err(RealtimeLoadError::with_timings(
                            "realtime chunks are not displayable yet".to_owned(),
                            timings,
                        ));
                    }
                    if !realtime.complete
                        && !display_live_chunk_updates
                        && !live_partial_has_complete_low_level_tilt(&decoded.volume)
                    {
                        let mut timings = decoded.timings;
                        timings.realtime_poll_end_utc = Some(Utc::now());
                        return Err(RealtimeLoadError::with_timings(
                            "waiting for complete live low-level tilt".to_owned(),
                            timings,
                        ));
                    }
                    let mut decoded = decoded;
                    decoded.timings.realtime_poll_end_utc = Some(Utc::now());
                    Ok(decoded)
                })();
                if let Ok(decoded) = realtime_result {
                    selected_identity = Some(frame_identity_for_volume(&decoded.volume));
                    if mode != LatestLoadMode::AutoRefresh || current_source_path.is_none() {
                        let _ = sender.send(AsyncLoadResult {
                            label: format!("L2 {site_id} current"),
                            update: AsyncLoadUpdate::History(
                                DecodedLoadBatch {
                                    frames: vec![decoded.clone()],
                                    selected_index: 0,
                                },
                                true,
                            ),
                        });
                    }
                    decoded_frames.push(decoded);
                } else if let Err(err) = realtime_result {
                    if mode == LatestLoadMode::AutoRefresh && current_source_path.is_some() {
                        return Ok(AsyncLoadUpdate::Unchanged {
                            timings: err.timings,
                            reason: err.reason,
                        });
                    }
                    fallback_error = Some(err.reason);
                }
            }

            let needs_archive_frames = explicit_loop_load || decoded_frames.is_empty();
            if needs_archive_frames {
                let archive_limit = if explicit_loop_load { history_limit } else { 1 };
                let archive_lookup_start = Instant::now();
                let mut archive_lookup_ms = None;
                let recent_archive_objects =
                    match data_source::recent_level2_objects(&site.level2_id, 7, archive_limit) {
                        Ok(objects) => {
                            archive_lookup_ms =
                                Some(archive_lookup_start.elapsed().as_secs_f32() * 1000.0);
                            objects
                        }
                        Err(err) => {
                            fallback_error.get_or_insert_with(|| err.to_string());
                            Vec::new()
                        }
                    };

                let mut remaining_archive_objects = Vec::new();
                for (index, object) in recent_archive_objects.into_iter().enumerate() {
                    if !explicit_loop_load && !decoded_frames.is_empty() {
                        break;
                    }
                    if selected_identity.is_none() {
                        let select_archive_frame = true;
                        match decode_archive_history_object(
                            &site_id,
                            object,
                            &site_cache_dir,
                            &known_frame_paths,
                            (index == 0).then_some(archive_lookup_ms).flatten(),
                            total_start,
                            &sender,
                            should_preview_loads(),
                        ) {
                            Ok(Some(decoded)) => {
                                selected_identity =
                                    Some(frame_identity_for_volume(&decoded.volume));
                                let _ = sender.send(AsyncLoadResult {
                                    label: format!("L2 {site_id} latest archive"),
                                    update: AsyncLoadUpdate::History(
                                        DecodedLoadBatch {
                                            frames: vec![decoded.clone()],
                                            selected_index: 0,
                                        },
                                        select_archive_frame,
                                    ),
                                });
                                decoded_frames.push(decoded);
                            }
                            Ok(None) => {}
                            Err(err) => {
                                fallback_error.get_or_insert(err);
                            }
                        }
                    } else if explicit_loop_load {
                        remaining_archive_objects.push((index, object));
                    }
                }

                if explicit_loop_load && !remaining_archive_objects.is_empty() {
                    let (history_frames, history_error) = load_archive_history_objects_parallel(
                        &site_id,
                        &site_cache_dir,
                        remaining_archive_objects,
                        &known_frame_paths,
                        archive_lookup_ms,
                        total_start,
                        &sender,
                    );
                    fallback_error = fallback_error.or(history_error);
                    for decoded in history_frames {
                        decoded_frames.push(decoded);
                    }
                }
            }

            if decoded_frames.is_empty() {
                if explicit_loop_load && selected_identity.is_some() {
                    return Ok(AsyncLoadUpdate::Final(Ok(DecodedLoadBatch {
                        frames: Vec::new(),
                        selected_index: 0,
                    })));
                }
                if mode == LatestLoadMode::AutoRefresh {
                    return Ok(AsyncLoadUpdate::Unchanged {
                        timings: None,
                        reason: fallback_error
                            .unwrap_or_else(|| "no displayable Level II scans found".to_owned()),
                    });
                }
                return Err(fallback_error
                    .unwrap_or_else(|| "no displayable Level II scans found".to_owned()));
            }

            if explicit_loop_load {
                return Ok(AsyncLoadUpdate::Final(Ok(DecodedLoadBatch {
                    frames: Vec::new(),
                    selected_index: 0,
                })));
            }

            decoded_frames.sort_by(|left, right| {
                frame_identity_for_volume(&left.volume)
                    .cmp(&frame_identity_for_volume(&right.volume))
            });
            let selected_index = selected_identity
                .and_then(|identity| {
                    decoded_frames
                        .iter()
                        .position(|decoded| frame_identity_for_volume(&decoded.volume) == identity)
                })
                .unwrap_or_else(|| decoded_frames.len().saturating_sub(1));

            Ok(AsyncLoadUpdate::Final(Ok(DecodedLoadBatch {
                frames: decoded_frames,
                selected_index,
            })))
        })();
        let update = final_update.unwrap_or_else(|err| AsyncLoadUpdate::Final(Err(err)));
        let _ = sender.send(AsyncLoadResult {
            label: format!("L2 {site_id}"),
            update,
        });
    });
}

struct AsyncRenderResult {
    key: TextureKey,
    /// Which view pane requested this render (0 = the primary view).
    pane: usize,
    result: Result<RenderedTexture, String>,
}

/// Background cell-identification result: (volume ptr, volume time, cells).
type StormCellsResult = (usize, DateTime<Utc>, Vec<StormCell>);

/// One tracked storm cell across volumes — the cross-volume half of SCIT
/// (Johnson et al. 1998, WAF 13(2)): nearest-to-prediction association and a
/// least-squares motion fit over the recent history.
struct StormTrack {
    id: u32,
    /// (volume time, east km, north km), most recent last; capped length.
    history: Vec<(DateTime<Utc>, f64, f64)>,
    max_dbz: f32,
    /// Fitted motion in m/s (east, north); None until two distinct times.
    motion_mps: Option<(f32, f32)>,
    /// Consecutive volumes without a match (track drops at 2).
    missed: u8,
}

impl StormTrack {
    /// Least-squares linear fit of position vs time over the last points.
    fn refit_motion(&mut self) {
        let n = self.history.len();
        if n < 2 {
            self.motion_mps = None;
            return;
        }
        let window = &self.history[n.saturating_sub(6)..];
        let t0 = window[0].0;
        let mut st = 0.0f64;
        let mut stt = 0.0f64;
        let mut se = 0.0f64;
        let mut sn = 0.0f64;
        let mut ste = 0.0f64;
        let mut stn = 0.0f64;
        let m = window.len() as f64;
        for (time, east, north) in window {
            let t = (*time - t0).num_milliseconds() as f64 / 1000.0;
            st += t;
            stt += t * t;
            se += east;
            sn += north;
            ste += t * east;
            stn += t * north;
        }
        let denom = m * stt - st * st;
        if denom.abs() < 1.0 {
            self.motion_mps = None;
            return;
        }
        let u = (m * ste - st * se) / denom * 1000.0; // km/s -> m/s
        let v = (m * stn - st * sn) / denom * 1000.0;
        // Reject unphysical fits (storms top out well under 50 m/s).
        if u.hypot(v) > 60.0 {
            self.motion_mps = None;
        } else {
            self.motion_mps = Some((u as f32, v as f32));
        }
    }
}

/// A rotation site detected on the lowest velocity tilt, geolocated.
#[derive(Clone, Copy, Debug)]
struct RotationMarker {
    lon: f32,
    lat: f32,
    /// Best rotational velocity across the column (m/s).
    vrot_mps: f32,
    /// 3D strength rank (Stumpf et al. 1998 scale).
    rank: u8,
    strength: render2d::RotationStrength,
}

/// An extra synchronized view pane in the multi-pane grid. Pane 0 is the
/// primary view (ViewerApp's own product/texture state, untouched); extra
/// panes are 1-based. All panes share the loaded volume, the geo transform
/// (pan/zoom stay in sync by construction), the selected tilt, and the ONE
/// render worker — renders are sequential on it, so total CPU parallelism
/// stays bounded at the core count no matter how many panes are open.
struct ViewPane {
    product: DisplayProduct,
    /// Independent tilt override; None = follow the main pane's tilt, so
    /// scrubbing the main tilt moves every un-pinned pane in sync.
    cut: Option<usize>,
    texture: Option<egui::TextureHandle>,
    texture_key: Option<TextureKey>,
    pending_render_key: Option<TextureKey>,
    render_ms: Option<f32>,
}

impl ViewPane {
    fn new(product: DisplayProduct) -> Self {
        Self {
            product,
            cut: None,
            texture: None,
            texture_key: None,
            pending_render_key: None,
            render_ms: None,
        }
    }
}

struct RenderRequest {
    key: TextureKey,
    /// Which view pane this render is for (0 = the primary view). The worker
    /// coalesces queued requests per pane: a newer request replaces the queued
    /// one for the SAME pane only, so one pane's traffic can't starve another.
    pane: usize,
    volume: Arc<RadarVolume>,
    cut: usize,
    product: DisplayProduct,
    render_dealiased_velocity: bool,
    plain_velocity_render_dealiased: bool,
    color_tables: ColorTableSet,
    storm_motion: StormMotion,
    hail_levels_m: (f32, f32),
    viewport_options: ViewportRasterOptions,
    radar_range_km: f32,
}

struct RenderedTexture {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    buffer_signature: RenderWorkerViewportSignature,
    render_ms: f32,
    worker_ms: f32,
    sample_cache_build_ms: Option<f32>,
    used_sample_cache: bool,
    radar_range_km: f32,
}

struct RenderRecycleBuffer {
    rgba: Vec<u8>,
    signature: Option<RenderWorkerViewportSignature>,
}

struct DealiasedReadoutCache {
    volume_ptr: usize,
    cut_index: usize,
    grid: Arc<MomentGrid>,
}

#[derive(Clone, Debug)]
struct HazardOverlay {
    source_label: String,
    query_time_utc: Option<String>,
    scanned_items: usize,
    parsed_items: usize,
    polygon_records: usize,
    error_count: usize,
    load_ms: f32,
    records: Vec<HazardRecord>,
}

#[derive(Clone, Debug, PartialEq)]
struct HazardRecord {
    event_id: String,
    label: String,
    event_family: String,
    action: String,
    lifecycle_status: Option<String>,
    office: String,
    headline: Option<String>,
    source_url: Option<String>,
    area: Option<String>,
    motion: Option<String>,
    details: Vec<String>,
    valid_start: Option<String>,
    valid_end: Option<String>,
    severity: Option<String>,
    certainty: Option<String>,
    urgency: Option<String>,
    tornado: Option<String>,
    hail_inches: Option<f32>,
    wind_mph: Option<u16>,
    damage_threat: Option<String>,
    points: Vec<HazardPoint>,
    bbox: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct HazardPoint {
    lon: f32,
    lat: f32,
}

#[derive(Clone, Debug)]
struct PerfTelemetry {
    decode: MetricSeries,
    direct_render: MetricSeries,
    cached_render: MetricSeries,
    worker: MetricSeries,
    texture: MetricSeries,
    cache_build: MetricSeries,
}

impl PerfTelemetry {
    fn new() -> Self {
        Self {
            decode: MetricSeries::new(),
            direct_render: MetricSeries::new(),
            cached_render: MetricSeries::new(),
            worker: MetricSeries::new(),
            texture: MetricSeries::new(),
            cache_build: MetricSeries::new(),
        }
    }

    fn record_decode(&mut self, ms: f32) {
        self.decode.push(ms);
    }

    fn record_render(
        &mut self,
        render_ms: f32,
        used_sample_cache: bool,
        worker_ms: f32,
        texture_ms: f32,
        sample_cache_build_ms: Option<f32>,
    ) {
        if used_sample_cache {
            self.cached_render.push(render_ms);
        } else {
            self.direct_render.push(render_ms);
        }
        self.worker.push(worker_ms);
        self.texture.push(texture_ms);
        if let Some(sample_cache_build_ms) = sample_cache_build_ms {
            self.cache_build.push(sample_cache_build_ms);
        }
    }
}

#[derive(Clone, Debug)]
struct MetricSeries {
    samples: [f32; PERF_SAMPLE_CAPACITY],
    start: usize,
    len: usize,
    latest: f32,
}

impl MetricSeries {
    fn new() -> Self {
        Self {
            samples: [0.0; PERF_SAMPLE_CAPACITY],
            start: 0,
            len: 0,
            latest: 0.0,
        }
    }

    fn push(&mut self, ms: f32) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        self.latest = ms;
        if self.len < PERF_SAMPLE_CAPACITY {
            let index = (self.start + self.len) % PERF_SAMPLE_CAPACITY;
            self.samples[index] = ms;
            self.len += 1;
        } else {
            self.samples[self.start] = ms;
            self.start = (self.start + 1) % PERF_SAMPLE_CAPACITY;
        }
    }

    fn summary(&self) -> Option<MetricSummary> {
        if self.len == 0 {
            return None;
        }

        let mut sorted = [0.0; PERF_SAMPLE_CAPACITY];
        for (target, source) in sorted.iter_mut().zip((0..self.len).map(|offset| {
            let index = (self.start + offset) % PERF_SAMPLE_CAPACITY;
            self.samples[index]
        })) {
            *target = source;
        }
        let sorted = &mut sorted[..self.len];
        sorted.sort_by(|a, b| a.total_cmp(b));

        Some(MetricSummary {
            latest: self.latest,
            min: sorted[0],
            p50: sorted[percentile_index(self.len, 50)],
            p95: sorted[percentile_index(self.len, 95)],
            max: sorted[self.len - 1],
            count: self.len,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MetricSummary {
    latest: f32,
    min: f32,
    p50: f32,
    p95: f32,
    max: f32,
    count: usize,
}

fn percentile_index(len: usize, percentile: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    ((len - 1) * percentile + 50) / 100
}

struct RenderWorkerMomentCache {
    volume_ptr: usize,
    cut: usize,
    moment: MomentType,
    dealiased_velocity: bool,
    derived: Option<DerivedProduct>,
    color_table_signature: u64,
    cache: ViewportMomentCache,
    storm_palette_cache: Option<RenderWorkerStormPaletteCache>,
}

struct RenderWorkerStormPaletteCache {
    storm_motion_key: (i16, i16),
    cache: Option<StormRelativePaletteCache>,
}

struct RenderWorkerSampleCache {
    signature: RenderWorkerSampleCacheSignature,
    cache: ViewportSampleCache,
}

#[derive(Clone, Copy, Debug)]
struct RenderWorkerCachePolicy {
    threads: usize,
    mode: RenderWorkerCacheMode,
    /// Highest pane count seen on this worker — multi-pane cycles N distinct
    /// (product, cut) keys through the LRU caches, so the entry capacity must
    /// be at least N or every pane render evicts another pane's caches.
    /// Byte budgets stay the hard cap; this only lifts the entry-count floor.
    min_entries: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderWorkerCacheMode {
    Primary,
    Overlay,
}

impl RenderWorkerCachePolicy {
    fn detect(mode: RenderWorkerCacheMode) -> Self {
        Self {
            threads: effective_worker_threads(),
            mode,
            min_entries: 1,
        }
    }

    /// Note a request's pane id; capped at the 4-pane grid maximum.
    fn note_pane(&mut self, pane: usize) {
        self.min_entries = self.min_entries.max((pane + 1).min(4));
    }

    fn should_speculatively_warm_sample_cache(&self, rendered: &RenderedTexture) -> bool {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return false;
        }
        if rendered.used_sample_cache {
            return false;
        }
        let pixels = rendered.width as u64 * rendered.height as u64;
        let min_render_ms = if self.threads <= 7 {
            LOW_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS
        } else {
            HIGH_END_SPECULATIVE_SAMPLE_CACHE_MIN_RENDER_MS
        };
        pixels >= SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS
            && rendered.render_ms >= min_render_ms
            && self.can_attempt_sample_cache_build(rendered.buffer_signature.viewport.dimensions())
    }

    #[cfg(test)]
    fn should_build_sample_cache_for_viewport(&self, viewport: ViewportKey) -> bool {
        self.can_store_sample_cache(viewport.dimensions())
    }

    fn should_build_sample_cache_for_moment_cache(
        &self,
        cache: &ViewportMomentCache,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<bool, String> {
        let upper_bound = cache
            .sample_cache_storage_upper_bound(volume, options)
            .map_err(|err| err.to_string())?;
        Ok(self.can_store_sample_cache_bytes(upper_bound))
    }

    fn should_prefetch_interaction_cache(&self, dimensions: (u32, u32)) -> bool {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return false;
        }
        let (width, height) = dimensions;
        let pixels = width as u64 * height as u64;
        self.threads >= 8
            && pixels >= SPECULATIVE_SAMPLE_CACHE_MIN_PIXELS
            && self.can_store_sample_cache(dimensions)
    }

    fn can_store_sample_cache(&self, dimensions: (u32, u32)) -> bool {
        let (width, height) = dimensions;
        let upper_bound = viewport_sample_cache_storage_upper_bound(ViewportRasterOptions {
            width,
            height,
            radar_x_px: 0.0,
            radar_y_px: 0.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        });
        self.can_store_sample_cache_bytes(upper_bound)
    }

    fn can_attempt_sample_cache_build(&self, dimensions: (u32, u32)) -> bool {
        let (width, height) = dimensions;
        let upper_bound = viewport_sample_cache_storage_upper_bound(ViewportRasterOptions {
            width,
            height,
            radar_x_px: 0.0,
            radar_y_px: 0.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        });
        upper_bound <= self.sample_cache_build_bytes()
    }

    fn can_store_sample_cache_bytes(&self, bytes: usize) -> bool {
        bytes <= self.sample_cache_bytes()
    }

    fn sample_cache_capacity(&self) -> usize {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return 1;
        }
        let thread_based = match self.threads {
            0..=7 => 1,
            8..=15 => 3,
            _ => 6,
        };
        thread_based.max(self.min_entries)
    }

    fn sample_cache_bytes(&self) -> usize {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return LOW_END_SAMPLE_CACHE_BYTES;
        }
        match self.threads {
            0..=7 => LOW_END_SAMPLE_CACHE_BYTES,
            8..=15 => MID_RANGE_SAMPLE_CACHE_BYTES,
            _ => HIGH_END_SAMPLE_CACHE_BYTES,
        }
    }

    fn sample_cache_build_bytes(&self) -> usize {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return LOW_END_SAMPLE_CACHE_BYTES;
        }
        match self.threads {
            0..=7 => LOW_END_SAMPLE_CACHE_BUILD_BYTES,
            _ => self.sample_cache_bytes(),
        }
    }

    fn direct_viewport_capacity(&self) -> usize {
        self.sample_cache_capacity().saturating_mul(2).max(1)
    }

    fn moment_cache_capacity(&self) -> usize {
        if self.mode == RenderWorkerCacheMode::Overlay {
            return 1;
        }
        self.sample_cache_capacity()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderWorkerViewportSignature {
    volume_ptr: usize,
    cut: usize,
    product: DisplayProduct,
    moment: MomentType,
    dealiased_velocity: bool,
    color_table_signature: u64,
    storm_motion_key: (i16, i16),
    hail_levels_key: (i16, i16),
    viewport: ViewportKey,
}

impl RenderWorkerViewportSignature {
    #[allow(clippy::too_many_arguments)]
    fn new(
        volume_ptr: usize,
        cut: usize,
        product: DisplayProduct,
        moment: MomentType,
        dealiased_velocity: bool,
        color_table_signature: u64,
        storm_motion_key: (i16, i16),
        hail_levels_key: (i16, i16),
        viewport: ViewportKey,
    ) -> Self {
        Self {
            volume_ptr,
            cut,
            product,
            moment,
            dealiased_velocity,
            color_table_signature,
            storm_motion_key,
            hail_levels_key,
            viewport,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RenderWorkerSampleCacheSignature {
    volume_ptr: usize,
    cut: usize,
    product: DisplayProduct,
    moment: MomentType,
    dealiased_velocity: bool,
    viewport: ViewportKey,
}

impl RenderWorkerSampleCacheSignature {
    fn new(
        volume_ptr: usize,
        cut: usize,
        product: DisplayProduct,
        moment: MomentType,
        dealiased_velocity: bool,
        viewport: ViewportKey,
    ) -> Self {
        Self {
            volume_ptr,
            cut,
            product,
            moment,
            dealiased_velocity,
            viewport,
        }
    }
}

fn spawn_render_worker() -> (
    mpsc::Sender<RenderRequest>,
    mpsc::Receiver<AsyncRenderResult>,
    mpsc::Sender<RenderRecycleBuffer>,
) {
    spawn_render_worker_with_mode(RenderWorkerCacheMode::Primary)
}

fn spawn_overlay_render_worker() -> (
    mpsc::Sender<RenderRequest>,
    mpsc::Receiver<AsyncRenderResult>,
    mpsc::Sender<RenderRecycleBuffer>,
) {
    spawn_render_worker_with_mode(RenderWorkerCacheMode::Overlay)
}

fn spawn_render_worker_with_mode(
    mode: RenderWorkerCacheMode,
) -> (
    mpsc::Sender<RenderRequest>,
    mpsc::Receiver<AsyncRenderResult>,
    mpsc::Sender<RenderRecycleBuffer>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<RenderRequest>();
    let (result_sender, result_receiver) = mpsc::channel::<AsyncRenderResult>();
    let (recycle_sender, recycle_receiver) = mpsc::channel::<RenderRecycleBuffer>();

    thread::spawn(move || {
        let mut cache_policy = RenderWorkerCachePolicy::detect(mode);
        let mut reusable_pixels = Vec::new();
        let mut reusable_pixels_signature: Option<RenderWorkerViewportSignature> = None;
        let mut moment_caches: Vec<RenderWorkerMomentCache> = Vec::new();
        let mut sample_caches: Vec<RenderWorkerSampleCache> = Vec::new();
        let mut last_direct_viewports: Vec<RenderWorkerViewportSignature> = Vec::new();
        // Queued requests, at most one per pane (newer replaces same-pane).
        // With a single pane this degenerates to exactly the old newest-only
        // coalescing; with several panes no pane's request is dropped.
        let mut deferred_requests: VecDeque<RenderRequest> = VecDeque::new();
        loop {
            if deferred_requests.is_empty() {
                match request_receiver.recv() {
                    Ok(request) => ViewerApp::merge_render_request(&mut deferred_requests, request),
                    Err(_) => break,
                }
            }
            for newer_request in request_receiver.try_iter() {
                ViewerApp::merge_render_request(&mut deferred_requests, newer_request);
            }
            let Some(request) = deferred_requests.pop_front() else {
                continue;
            };
            let requested_buffer_signature = RenderWorkerViewportSignature::new(
                Arc::as_ptr(&request.volume) as usize,
                request.cut,
                request.product.clone(),
                request.product.base_moment(),
                request.render_dealiased_velocity,
                request.key.color_table_signature,
                request.key.storm_motion_key,
                request.key.hail_levels_key,
                request.key.viewport,
            );
            while let Ok(recycled) = recycle_receiver.try_recv() {
                let recycled_matches =
                    recycled.signature.as_ref() == Some(&requested_buffer_signature);
                let current_matches =
                    reusable_pixels_signature.as_ref() == Some(&requested_buffer_signature);
                if reusable_pixels.is_empty()
                    || (recycled_matches && !current_matches)
                    || (recycled_matches == current_matches
                        && recycled.rgba.capacity() > reusable_pixels.capacity())
                {
                    reusable_pixels = recycled.rgba;
                    reusable_pixels_signature = recycled.signature;
                }
            }

            let key = request.key.clone();
            let pane = request.pane;
            cache_policy.note_pane(pane);
            let result = ViewerApp::render_viewport_payload(
                &request,
                &mut reusable_pixels,
                &mut reusable_pixels_signature,
                &mut moment_caches,
                &mut sample_caches,
                &mut last_direct_viewports,
                cache_policy,
            );
            let should_warm_sample_cache = result.as_ref().is_ok_and(|rendered| {
                cache_policy.should_speculatively_warm_sample_cache(rendered)
            });
            let should_prefetch_velocity_cache = result.as_ref().is_ok_and(|rendered| {
                ViewerApp::should_prefetch_velocity_interaction_cache(
                    &request,
                    rendered,
                    cache_policy,
                )
            });
            if result_sender
                .send(AsyncRenderResult { key, pane, result })
                .is_err()
            {
                break;
            }
            // Speculative cache warming only runs when no real request is
            // waiting — pending work (any pane) always wins over warming.
            if should_warm_sample_cache || should_prefetch_velocity_cache {
                match ViewerApp::queue_newer_render_requests(
                    &request_receiver,
                    &mut deferred_requests,
                ) {
                    Ok(_) => {}
                    Err(_) => break,
                }
                if deferred_requests.is_empty() && should_prefetch_velocity_cache {
                    match ViewerApp::queue_newer_render_requests(
                        &request_receiver,
                        &mut deferred_requests,
                    ) {
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if deferred_requests.is_empty() {
                        ViewerApp::warm_velocity_interaction_cache_after_direct_render(
                            &request,
                            &mut moment_caches,
                            &mut sample_caches,
                            cache_policy,
                        );
                    }
                }
                if deferred_requests.is_empty() && should_warm_sample_cache {
                    match ViewerApp::queue_newer_render_requests(
                        &request_receiver,
                        &mut deferred_requests,
                    ) {
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if deferred_requests.is_empty() {
                        ViewerApp::warm_sample_cache_after_direct_render(
                            &request,
                            &mut moment_caches,
                            &mut sample_caches,
                            &mut last_direct_viewports,
                            cache_policy,
                        );
                    }
                }
            }
        }
    });

    (request_sender, result_receiver, recycle_sender)
}

/// Computed products that are not a raw moment grid. The volume-wide ones
/// (composite/echo-top/VIL) walk every tilt and render on the lowest
/// reflectivity tilt's geometry; azimuthal shear is a per-cut velocity
/// derivative rendered on the selected cut.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DerivedProduct {
    CompositeReflectivity,
    EchoTops,
    Vil,
    VilDensity,
    Mehs,
    AzimuthalShear,
    Divergence,
}

impl DerivedProduct {
    fn label(self) -> &'static str {
        match self {
            Self::CompositeReflectivity => "CREF",
            Self::EchoTops => "ET",
            Self::Vil => "VIL",
            Self::VilDensity => "VILD",
            Self::Mehs => "MEHS",
            Self::AzimuthalShear => "AzShr",
            Self::Divergence => "Div",
        }
    }

    fn color_family(self) -> ColorTableFamily {
        match self {
            Self::CompositeReflectivity => ColorTableFamily::Reflectivity,
            Self::EchoTops => ColorTableFamily::EchoTops,
            Self::Vil => ColorTableFamily::Vil,
            Self::VilDensity => ColorTableFamily::VilDensity,
            Self::Mehs => ColorTableFamily::HailSize,
            // Divergence shares the diverging shear palette (convergence cool,
            // divergence warm).
            Self::AzimuthalShear | Self::Divergence => ColorTableFamily::AzimuthalShear,
        }
    }

    fn units(self) -> &'static str {
        match self {
            Self::CompositeReflectivity => "dBZ",
            Self::EchoTops => "m",
            Self::Vil => "kg/m²",
            Self::VilDensity => "g/m³",
            Self::Mehs => "mm",
            Self::AzimuthalShear | Self::Divergence => "×10⁻³/s",
        }
    }

    /// Source moment the product is derived from.
    fn base_moment(self) -> MomentType {
        match self {
            Self::AzimuthalShear | Self::Divergence => MomentType::Velocity,
            _ => MomentType::Reflectivity,
        }
    }

    /// True if computed from the whole volume (rendered on the base tilt);
    /// false for per-cut derivatives rendered on the selected tilt.
    fn is_volume_wide(self) -> bool {
        !matches!(self, Self::AzimuthalShear | Self::Divergence)
    }

    const ALL: [DerivedProduct; 7] = [
        Self::CompositeReflectivity,
        Self::EchoTops,
        Self::Vil,
        Self::VilDensity,
        Self::Mehs,
        Self::AzimuthalShear,
        Self::Divergence,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DisplayProduct {
    Moment(MomentType),
    DealiasedVelocity,
    StormRelativeVelocity,
    StormRelativeDealiasedVelocity,
    Derived(DerivedProduct),
}

impl DisplayProduct {
    fn label(&self) -> &str {
        match self {
            Self::Moment(moment) => moment.short_name(),
            Self::DealiasedVelocity => "DVEL",
            Self::StormRelativeVelocity => "SRV",
            Self::StormRelativeDealiasedVelocity => "DSRV",
            Self::Derived(d) => d.label(),
        }
    }

    fn base_moment(&self) -> MomentType {
        match self {
            Self::Moment(moment) => moment.clone(),
            Self::DealiasedVelocity
            | Self::StormRelativeVelocity
            | Self::StormRelativeDealiasedVelocity => MomentType::Velocity,
            // Derived products are computed from their source moment.
            Self::Derived(d) => d.base_moment(),
        }
    }

    fn derived(&self) -> Option<DerivedProduct> {
        match self {
            Self::Derived(d) => Some(*d),
            _ => None,
        }
    }

    fn is_storm_relative_velocity(&self) -> bool {
        matches!(
            self,
            Self::StormRelativeVelocity | Self::StormRelativeDealiasedVelocity
        )
    }

    fn uses_dealiased_velocity(&self) -> bool {
        matches!(
            self,
            Self::DealiasedVelocity | Self::StormRelativeDealiasedVelocity
        )
    }

    fn render_uses_dealiased_velocity(&self, unfold_plain_velocity: bool) -> bool {
        match self {
            Self::Moment(MomentType::Velocity) => unfold_plain_velocity,
            Self::DealiasedVelocity | Self::StormRelativeDealiasedVelocity => true,
            _ => false,
        }
    }

    fn color_family(&self) -> ColorTableFamily {
        match self {
            Self::Moment(moment) => color_family_for_moment(moment),
            Self::DealiasedVelocity
            | Self::StormRelativeVelocity
            | Self::StormRelativeDealiasedVelocity => ColorTableFamily::Velocity,
            Self::Derived(d) => d.color_family(),
        }
    }
}

#[derive(Clone, Debug)]
struct CursorReadout {
    site_id: String,
    volume_time_utc: DateTime<Utc>,
    product: DisplayProduct,
    cut: usize,
    value: f32,
    base_value: Option<f32>,
    vrot: Option<VrotProbe>,
    raw: Option<u16>,
    row: usize,
    gate: usize,
    gate_spacing_m: i32,
    range_km: f32,
    azimuth_deg: f32,
    source_azimuth_deg: f32,
    elevation_deg: f32,
    /// Beam-center height above the radar antenna (m), 4/3-Earth model.
    height_above_radar_m: f32,
    nyquist_velocity_mps: Option<f32>,
    realtime_volume_id: Option<u16>,
    realtime_last_chunk_id: Option<u16>,
    realtime_last_chunk_type: Option<RealtimeChunkType>,
}

#[derive(Clone, Copy, Debug)]
struct VrotProbe {
    delta_v_mps: f32,
    vrot_mps: f32,
    separation_km: f32,
    inbound: VrotGate,
    outbound: VrotGate,
}

#[derive(Clone, Copy, Debug)]
struct VrotGate {
    row: usize,
    gate: usize,
    value_mps: f32,
    azimuth_deg: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SidebarTab {
    Radar,
    Hazards,
    Colors,
    Stats,
}

const SIDEBAR_TABS: &[(SidebarTab, &str)] = &[
    (SidebarTab::Radar, "Radar"),
    (SidebarTab::Hazards, "Hazards"),
    (SidebarTab::Colors, "Colors"),
    (SidebarTab::Stats, "Stats"),
];

fn sidebar_tab_tooltip(tab: SidebarTab) -> &'static str {
    match tab {
        SidebarTab::Radar => "Radar site, overlays, product, and tilt controls",
        SidebarTab::Hazards => "Warnings, watches, mesoscale discussions, and alert filters",
        SidebarTab::Colors => "Built-in and custom radar color tables",
        SidebarTab::Stats => "Performance timings and render/load diagnostics",
    }
}

impl ViewerApp {
    fn new(cc: &eframe::CreationContext<'_>, source_path: Option<PathBuf>) -> Self {
        configure_style(&cc.egui_ctx);
        let app_settings = settings::AppSettings::load();
        let restored_grid_layout = match app_settings.grid_pane_count {
            2 => PanelLayout::TwoVertical,
            4 => PanelLayout::FourGrid,
            _ => PanelLayout::One,
        };
        let restored_placefile_slots: Vec<PlacefileSlot> = app_settings
            .placefiles
            .iter()
            .map(|entry| PlacefileSlot::new(entry.url.clone(), entry.enabled))
            .collect();
        let sites = data_source::fallback_sites();
        let selected_site_index = app_settings
            .startup_site
            .as_deref()
            .and_then(|id| {
                sites
                    .iter()
                    .position(|site| site.level2_id.eq_ignore_ascii_case(id))
            })
            .or_else(|| sites.iter().position(|site| site.level2_id == "KTLX"))
            .unwrap_or(0);
        let (map_center_lat, map_center_lon) = sites
            .get(selected_site_index)
            .and_then(site_location)
            .unwrap_or((35.33305, -97.27775));
        let (render_sender, render_receiver, render_recycle_sender) = spawn_render_worker();
        let hazard_path_text = String::new();

        let mut app = Self {
            source_path,
            volume: None,
            selected_cut: 0,
            selected_product: DisplayProduct::Moment(MomentType::Reflectivity),
            frame_history: Vec::new(),
            selected_frame_index: 0,
            history_frame_limit: DEFAULT_HISTORY_FRAME_LIMIT,
            history_playing: false,
            last_history_step: None,
            color_tables: ColorTableSet::default(),
            flip_velocity_color_polarity: false,
            unfold_velocity_display: true,
            color_table_target: ColorTableFamily::Velocity,
            color_table_path_text: String::new(),
            color_table_status:
                "Built-in GR2/NWS/Analyst reflectivity and Analyst velocity presets".to_owned(),
            texture: None,
            texture_key: None,
            render_sender,
            render_receiver,
            render_recycle_sender,
            pending_render_key: None,
            map_center_lon,
            map_center_lat,
            map_scale: DEFAULT_MAP_SCALE,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
            load_timing: None,
            active_load_started_at: None,
            first_data_ms: None,
            first_texture_ms: None,
            render_ms: None,
            worker_ms: None,
            texture_ms: None,
            sample_cache_build_ms: None,
            basemap_ms: None,
            perf: PerfTelemetry::new(),
            status: String::new(),
            sites,
            selected_site_index,
            app_settings,
            radar_layers: Vec::new(),
            next_radar_layer_id: 1,
            site_catalog_receiver: None,
            load_receiver: None,
            hazard_receiver: None,
            pending_site_id: None,
            cursor_readout: None,
            placefile_slots: restored_placefile_slots,
            placefile_url_input: String::new(),
            placefile_shape_cache: std::cell::RefCell::new(ShapeCache::new(8)),
            storm_tracks: Vec::new(),
            storm_tracks_site: String::new(),
            next_storm_track_id: 1,
            storm_cells_volume_ptr: 0,
            storm_cells_receiver: None,
            show_storm_tracks: true,
            rotation_markers: Vec::new(),
            rotation_markers_volume_ptr: 0,
            rotation_receiver: None,
            show_rotation_markers: true,
            hail_freezing_level_km: 3.2,
            hail_minus20_level_km: 6.4,
            display_thresholds: BTreeMap::new(),
            show_inspector_card: true,
            pinned_inspector_lonlat: None,
            hazard_overlay_generation: 0,
            grid_layout: restored_grid_layout,
            extra_panes: Vec::new(),
            active_pane: 0,
            basemap_shape_cache: std::cell::RefCell::new(ShapeCache::new(16)),
            hazard_shape_cache: std::cell::RefCell::new(ShapeCache::new(8)),
            cross_section_armed: false,
            cross_section_a_lonlat: None,
            cross_section_b_lonlat: None,
            cross_section_texture: None,
            cross_section_signature: None,
            cross_section_status: "Cross-section: arm, then click endpoint A then B".to_owned(),
            cross_section_top_m: CROSS_SECTION_TOP_M,
            cross_section_user_signature: None,
            cross_section_volume_cuts: 0,
            cross_section_dealias_cache: VolumeDealiasCache::new(),
            hazard_overlay: None,
            hazard_path_text,
            hazard_status: "No hazard polygons loaded".to_owned(),
            hazards_visible: true,
            hazards_active_only: true,
            hazard_fill_alpha: DEFAULT_HAZARD_FILL_ALPHA,
            hidden_hazard_families: default_hidden_hazard_families(),
            realtime_level2_auto_refresh: true,
            display_live_chunk_updates: false,
            last_realtime_level2_refresh: None,
            live_refresh_skip_reason: None,
            live_hazard_auto_refresh: true,
            show_performance_stats: false,
            sidebar_tab: SidebarTab::Radar,
            last_live_hazard_refresh: None,
            selected_hazard_index: None,
            storm_motion_direction_deg: DEFAULT_STORM_MOTION_DIRECTION_DEG,
            storm_motion_speed_kt: DEFAULT_STORM_MOTION_SPEED_KT,
            dealiased_readout_cache: None,
        };
        app.start_site_catalog_load(&cc.egui_ctx);
        app.load_volume(&cc.egui_ctx);
        app.load_live_hazards(&cc.egui_ctx);
        app
    }

    fn load_volume(&mut self, ctx: &egui::Context) {
        if let Some(path) = self.source_path.clone() {
            self.start_local_volume_load(path, ctx);
        } else if let Some(site) = self.selected_site().cloned() {
            self.start_latest_level2_load(site, ctx);
        } else {
            self.status = "Choose a radar site to load Level II data".to_owned();
        }
    }

    fn load_live_hazards(&mut self, ctx: &egui::Context) {
        if self.hazard_receiver.is_some() {
            return;
        }
        let query_time_utc = Utc::now();
        let (sender, receiver) = mpsc::channel();
        self.hazard_receiver = Some(receiver);
        self.last_live_hazard_refresh = Some(Instant::now());
        self.hazard_status = "Loading live hazards".to_owned();
        thread::spawn(move || {
            let result = load_live_hazard_overlay_with_preview(query_time_utc, |preview| {
                let _ = sender.send(AsyncHazardResult {
                    update: AsyncHazardUpdate::Preview(Ok(preview)),
                });
            });
            let _ = sender.send(AsyncHazardResult {
                update: AsyncHazardUpdate::Final(result),
            });
        });
        ctx.request_repaint_after(Duration::from_millis(25));
    }

    fn load_local_hazards(&mut self, ctx: &egui::Context) {
        if self.hazard_receiver.is_some() {
            return;
        }
        let trimmed_path = self.hazard_path_text.trim();
        if trimmed_path.is_empty() {
            self.hazard_status = "No local hazard path entered".to_owned();
            return;
        }
        let path = PathBuf::from(trimmed_path);
        let query_time_utc = self
            .volume
            .as_ref()
            .map(|volume| volume.volume_time.with_timezone(&Utc));
        let (sender, receiver) = mpsc::channel();
        self.hazard_receiver = Some(receiver);
        self.hazard_status = format!("Loading local hazards from {}", path.display());
        thread::spawn(move || {
            let result = load_hazard_overlay_from_path(&path, query_time_utc);
            let _ = sender.send(AsyncHazardResult {
                update: AsyncHazardUpdate::Final(result),
            });
        });
        ctx.request_repaint_after(Duration::from_millis(25));
    }

    fn maybe_refresh_live_hazards(&mut self, ctx: &egui::Context) {
        if !self.live_hazard_auto_refresh || self.hazard_receiver.is_some() {
            return;
        }
        let should_refresh = self.last_live_hazard_refresh.is_none_or(|last_refresh| {
            last_refresh.elapsed() >= Duration::from_secs(LIVE_HAZARD_REFRESH_SECONDS)
        });
        if should_refresh {
            self.load_live_hazards(ctx);
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn maybe_refresh_realtime_level2(&mut self, ctx: &egui::Context) {
        if !self.realtime_level2_auto_refresh {
            return;
        }
        if self.load_receiver.is_some() {
            self.live_refresh_skip_reason = Some("load receiver busy".to_owned());
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        let should_refresh = self
            .last_realtime_level2_refresh
            .is_none_or(|last_refresh| {
                last_refresh.elapsed()
                    >= Duration::from_secs(PRIMARY_REALTIME_LEVEL2_REFRESH_SECONDS)
            });
        if !should_refresh {
            ctx.request_repaint_after(Duration::from_secs(1));
            return;
        }
        let Some(site) = self.selected_site().cloned() else {
            self.live_refresh_skip_reason = Some("no selected site".to_owned());
            return;
        };
        self.live_refresh_skip_reason = None;
        self.start_latest_level2_load_with_mode(site, ctx, LatestLoadMode::AutoRefresh);
    }

    fn maybe_refresh_radar_layers(&mut self, ctx: &egui::Context) {
        if !self.realtime_level2_auto_refresh {
            return;
        }

        let mut requested_repaint = false;
        for (index, layer) in self.radar_layers.iter_mut().enumerate() {
            if !layer.visible || layer.load_receiver.is_some() {
                continue;
            }
            let refresh_after = Duration::from_secs(OVERLAY_REALTIME_LEVEL2_REFRESH_SECONDS)
                + Duration::from_millis((index as u64 % 8) * 350);
            let should_refresh = layer
                .last_realtime_level2_refresh
                .is_none_or(|last_refresh| last_refresh.elapsed() >= refresh_after);
            if should_refresh {
                Self::start_radar_layer_load(layer, LatestLoadMode::AutoRefresh, ctx);
                requested_repaint = true;
            }
        }

        if !requested_repaint && !self.radar_layers.is_empty() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    fn add_or_refresh_radar_layer(&mut self, site: RadarSite, ctx: &egui::Context) {
        if let Some(index) = self
            .radar_layers
            .iter()
            .position(|layer| layer.site.level2_id == site.level2_id)
        {
            let layer = &mut self.radar_layers[index];
            layer.visible = true;
            if layer.load_receiver.is_none() {
                Self::start_radar_layer_load(layer, LatestLoadMode::User, ctx);
            }
            self.status = format!("Refreshing overlay {}", site.level2_id);
            return;
        }

        if self.radar_layers.len() >= MAX_RADAR_OVERLAY_LAYERS {
            let remove_index = self
                .radar_layers
                .iter()
                .position(|layer| !layer.visible)
                .unwrap_or(0);
            self.radar_layers.remove(remove_index);
        }

        let id = self.next_radar_layer_id;
        self.next_radar_layer_id = self.next_radar_layer_id.saturating_add(1);
        let mut layer = RadarOverlayLayer::new(id, site.clone());
        Self::start_radar_layer_load(&mut layer, LatestLoadMode::User, ctx);
        self.status = format!("Added overlay {}", site.level2_id);
        self.radar_layers.push(layer);
    }

    fn start_radar_layer_load(
        layer: &mut RadarOverlayLayer,
        mode: LatestLoadMode,
        ctx: &egui::Context,
    ) {
        let site_id = layer.site.level2_id.clone();
        let (sender, receiver) = mpsc::channel();
        layer.load_receiver = Some(receiver);
        layer.last_realtime_level2_refresh = Some(Instant::now());
        layer.status = if mode == LatestLoadMode::AutoRefresh {
            format!("Refreshing {site_id}")
        } else {
            format!("Loading {site_id}")
        };
        let current_source_path = (mode == LatestLoadMode::AutoRefresh)
            .then(|| layer.source_path.clone())
            .flatten();
        spawn_latest_level2_load_worker(
            layer.site.clone(),
            mode,
            current_source_path,
            BTreeSet::new(),
            None,
            1,
            false,
            sender,
        );
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    fn poll_radar_layer_loads(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        for layer in &mut self.radar_layers {
            while let Some(result) = layer.load_receiver.as_ref().map(mpsc::Receiver::try_recv) {
                match result {
                    Ok(message) => {
                        saw_message = true;
                        match message.update {
                            AsyncLoadUpdate::Preview(decoded) => {
                                Self::install_radar_layer_volume(layer, decoded);
                                layer.status = format!("Preview {}", message.label);
                            }
                            AsyncLoadUpdate::History(batch, select_frame) => {
                                if select_frame && let Some(decoded) = batch.into_selected() {
                                    Self::install_radar_layer_volume(layer, decoded);
                                    layer.status = format!("Loaded {}", message.label);
                                }
                            }
                            AsyncLoadUpdate::Unchanged { timings, reason } => {
                                if let Some(timings) = timings {
                                    layer.load_timing = Some(timings);
                                }
                                layer.load_receiver = None;
                                layer.status = format!("Current {} ({reason})", message.label);
                                break;
                            }
                            AsyncLoadUpdate::Final(result) => {
                                layer.load_receiver = None;
                                match result {
                                    Ok(batch) => {
                                        if let Some(decoded) = batch.into_selected() {
                                            Self::install_radar_layer_volume(layer, decoded);
                                        }
                                        layer.status = format!("Loaded {}", message.label);
                                    }
                                    Err(err) => {
                                        layer.status =
                                            format!("Load failed for {}: {err}", message.label);
                                    }
                                }
                                break;
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        layer.load_receiver = None;
                        layer.status = "Layer load worker disconnected".to_owned();
                        saw_message = true;
                        break;
                    }
                }
            }
        }

        if saw_message {
            ctx.request_repaint();
        } else if self
            .radar_layers
            .iter()
            .any(|layer| layer.load_receiver.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
        }
    }

    fn install_radar_layer_volume(layer: &mut RadarOverlayLayer, decoded: DecodedLoad) {
        layer.source_path = Some(decoded.path);
        layer.load_timing = Some(decoded.timings);
        layer.volume = Some(Arc::new(decoded.volume));
        layer.pending_render_key = None;
        layer.render_ms = None;
        layer.worker_ms = None;
        layer.texture_ms = None;
    }

    fn clear_texture(&mut self) {
        self.texture = None;
        self.texture_key = None;
        self.pending_render_key = None;
        self.render_ms = None;
        self.worker_ms = None;
        self.texture_ms = None;
        self.sample_cache_build_ms = None;
    }

    fn clear_displayed_volume_for_pending_load(&mut self, ctx: &egui::Context) {
        self.volume = None;
        self.load_timing = None;
        self.dealiased_readout_cache = None;
        self.selected_cut = 0;
        self.clear_texture();
        // Cross-site load: extra panes must not keep the old site's imagery
        // (it would geo-anchor at the NEW site's location) or pinned tilts
        // (a different VCP renumbers the cuts).
        for pane in &mut self.extra_panes {
            pane.texture = None;
            pane.texture_key = None;
            pane.pending_render_key = None;
            pane.render_ms = None;
            pane.cut = None;
        }
        ctx.request_repaint();
    }

    fn clear_frame_history(&mut self) {
        self.frame_history.clear();
        self.selected_frame_index = 0;
        self.history_playing = false;
        self.last_history_step = None;
    }

    fn install_preview_volume(&mut self, decoded: DecodedLoad, ctx: &egui::Context) {
        let source_path = decoded.path;
        self.source_path = Some(source_path.clone());
        self.record_first_data_if_needed();
        self.install_volume_arc(
            Arc::new(decoded.volume),
            Some(decoded.timings),
            false,
            Some(source_path),
            decoded.status,
            ctx,
        );
    }

    fn install_decoded_load_batch(
        &mut self,
        batch: DecodedLoadBatch,
        record_final_decode: bool,
        select_loaded_frame: bool,
        ctx: &egui::Context,
    ) -> bool {
        if batch.frames.is_empty() {
            return false;
        }
        let selected_index = batch.selected_index.min(batch.frames.len() - 1);
        let selected_identity = frame_identity_for_volume(&batch.frames[selected_index].volume);
        let active_identity = self
            .volume
            .as_ref()
            .map(|volume| frame_identity_for_volume(volume.as_ref()));
        let selected_site_id = selected_identity.site_id.clone();
        if self
            .volume
            .as_ref()
            .is_some_and(|volume| volume.site.id != selected_site_id)
            || history_contains_other_site(&self.frame_history, &selected_site_id)
        {
            self.clear_frame_history();
        }

        for decoded in batch.frames {
            self.upsert_history_frame(decoded);
        }
        self.frame_history
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        self.trim_frame_history();

        let should_select_loaded_frame = select_loaded_frame && !self.history_playing;
        if should_select_loaded_frame {
            let next_index = self
                .frame_history
                .iter()
                .position(|frame| frame.identity == selected_identity)
                .unwrap_or_else(|| self.frame_history.len().saturating_sub(1));
            if should_defer_live_partial_selection_for_active_product(
                self.volume.as_deref(),
                &self.selected_product,
                self.frame_history.get(next_index),
            ) {
                self.selected_frame_index = active_identity
                    .clone()
                    .and_then(|identity| {
                        self.frame_history
                            .iter()
                            .position(|frame| frame.identity == identity)
                    })
                    .unwrap_or(self.selected_frame_index);
                self.status = format!(
                    "Waiting for {} in {}",
                    self.selected_product.label(),
                    selected_identity.site_id
                );
                ctx.request_repaint();
                return false;
            }
            self.select_history_frame(next_index, record_final_decode, ctx);
            true
        } else if let Some(active_identity) = active_identity
            && let Some(index) = self
                .frame_history
                .iter()
                .position(|frame| frame.identity == active_identity)
        {
            self.selected_frame_index = index;
            self.status = format!("Backfilled {}", selected_identity.site_id);
            ctx.request_repaint();
            false
        } else {
            ctx.request_repaint();
            false
        }
    }

    fn upsert_history_frame(&mut self, decoded: DecodedLoad) {
        let identity = frame_identity_for_volume(&decoded.volume);
        let frame = FrameHistoryEntry {
            identity: identity.clone(),
            path: decoded.path,
            volume: Arc::new(decoded.volume),
            timings: Some(decoded.timings),
            status: decoded.status,
            source_label: decoded.source_label,
        };
        if let Some(existing) = self
            .frame_history
            .iter_mut()
            .find(|candidate| candidate.identity == identity)
        {
            if live_partial_frame_has_new_data(&frame, existing) {
                *existing = frame;
            } else if frame.path == existing.path && frame.status == existing.status {
                existing.timings = frame.timings;
                existing.source_label = frame.source_label;
            } else if frame_status_priority(frame.status) > frame_status_priority(existing.status)
                || (frame_status_priority(frame.status) == frame_status_priority(existing.status)
                    && frame.path != existing.path)
            {
                *existing = frame;
            }
        } else {
            self.frame_history.push(frame);
        }
    }

    fn trim_frame_history(&mut self) {
        self.history_frame_limit = normalized_history_limit(self.history_frame_limit);
        while self.frame_history.len() > self.history_frame_limit {
            self.frame_history.remove(0);
        }
        self.selected_frame_index = self
            .selected_frame_index
            .min(self.frame_history.len().saturating_sub(1));
    }

    fn select_history_frame(
        &mut self,
        index: usize,
        record_final_decode: bool,
        ctx: &egui::Context,
    ) {
        let Some(frame) = self.frame_history.get(index).cloned() else {
            return;
        };
        self.record_first_data_if_needed();
        self.selected_frame_index = index;
        self.history_playing &= self.frame_history.len() > 1;
        self.source_path = Some(frame.path.clone());
        self.install_volume_arc(
            Arc::clone(&frame.volume),
            frame.timings,
            record_final_decode,
            Some(frame.path),
            frame.status,
            ctx,
        );
        self.status = self.selected_frame_status_text();
    }

    fn install_volume_arc(
        &mut self,
        volume: Arc<RadarVolume>,
        load_timing: Option<LoadTimings>,
        record_final_decode: bool,
        source_path: Option<PathBuf>,
        frame_status: FrameStatus,
        ctx: &egui::Context,
    ) {
        let previous_cut = self.selected_cut;
        let previous_product = self.selected_product.clone();
        let require_complete_live_cut =
            frame_status == FrameStatus::LivePartial && !self.display_live_chunk_updates;
        let (selected_cut, selected_product) = selection_for_installed_volume(
            self.volume.as_deref(),
            self.selected_cut,
            &self.selected_product,
            volume.as_ref(),
            !self.history_playing,
            self.display_live_chunk_updates,
            require_complete_live_cut,
        );
        if let Some(index) = self
            .sites
            .iter()
            .position(|site| site.level2_id == volume.site.id)
        {
            self.selected_site_index = index;
        }
        if record_final_decode && let Some(load_timing) = load_timing {
            self.perf.record_decode(load_timing.decode_ms);
        }
        let previous_volume_ptr = self
            .volume
            .as_ref()
            .map(|volume| Arc::as_ptr(volume) as usize);
        let next_volume_ptr = Arc::as_ptr(&volume) as usize;
        let same_volume = previous_volume_ptr == Some(next_volume_ptr);
        let keep_existing_texture = should_keep_texture_for_volume_install(
            self.volume.as_deref(),
            volume.as_ref(),
            same_volume,
        );
        let retarget_existing_texture = self.texture.is_some()
            && selected_cut == previous_cut
            && selected_product == previous_product
            && selected_cut_render_data_unchanged(
                self.volume.as_deref(),
                volume.as_ref(),
                selected_cut,
                &selected_product,
            );
        if let Some(source_path) = source_path {
            self.source_path = Some(source_path);
        }
        self.load_timing = load_timing;
        self.volume = Some(volume);
        self.dealiased_readout_cache = None;
        self.selected_cut = selected_cut;
        self.selected_product = selected_product;
        self.sanitize_selection();
        if keep_existing_texture {
            self.pending_render_key = None;
            if retarget_existing_texture && let Some(texture_key) = &mut self.texture_key {
                texture_key.volume_ptr = next_volume_ptr;
                texture_key.cut = self.selected_cut;
                texture_key.product = self.selected_product.clone();
            }
            self.render_ms = None;
            self.worker_ms = None;
            self.texture_ms = None;
            self.sample_cache_build_ms = None;
        } else {
            self.clear_texture();
        }
        // Extra panes: drop pending keys; on a volume change keep the old
        // texture on screen as a placeholder but poison its key (0 is never a
        // real Arc address) so the request guard re-renders even if the new
        // Arc lands at the old allocation (volume_ptr ABA). On a texture
        // reset, clear pane textures with the primary.
        for pane in &mut self.extra_panes {
            pane.pending_render_key = None;
            if !keep_existing_texture {
                pane.texture = None;
                pane.texture_key = None;
                pane.render_ms = None;
            } else if !same_volume && let Some(key) = &mut pane.texture_key {
                key.volume_ptr = 0;
            }
        }
        ctx.request_repaint();
    }

    fn selected_frame_status_text(&self) -> String {
        self.frame_history
            .get(self.selected_frame_index)
            .map(|frame| frame_status_text(frame, Utc::now()))
            .unwrap_or_else(|| "No Level II frame loaded".to_owned())
    }

    fn selected_frame(&self) -> Option<&FrameHistoryEntry> {
        self.frame_history.get(self.selected_frame_index)
    }

    fn current_history_paths(&self) -> BTreeSet<PathBuf> {
        self.frame_history
            .iter()
            .map(|frame| frame.path.clone())
            .collect()
    }

    fn maybe_advance_history_loop(&mut self, ctx: &egui::Context) {
        if !self.history_playing || self.frame_history.len() <= 1 {
            return;
        }
        let should_step = self.last_history_step.is_none_or(|last_step| {
            last_step.elapsed() >= Duration::from_millis(HISTORY_LOOP_FRAME_MS)
        });
        if should_step {
            let next_index = (self.selected_frame_index + 1) % self.frame_history.len();
            self.last_history_step = Some(Instant::now());
            self.select_history_frame(next_index, false, ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn set_history_frame_limit(&mut self, limit: usize, ctx: &egui::Context) {
        let active_identity = self
            .volume
            .as_ref()
            .map(|volume| frame_identity_for_volume(volume.as_ref()));
        self.history_frame_limit = normalized_history_limit(limit);
        self.trim_frame_history();
        if let Some(identity) = active_identity
            && let Some(index) = self
                .frame_history
                .iter()
                .position(|frame| frame.identity == identity)
        {
            self.selected_frame_index = index;
        } else {
            self.selected_frame_index = self.frame_history.len().saturating_sub(1);
        }
        ctx.request_repaint();
    }

    fn begin_primary_load_telemetry(&mut self) {
        self.active_load_started_at = Some(Instant::now());
        self.first_data_ms = None;
        self.first_texture_ms = None;
    }

    fn record_first_data_if_needed(&mut self) {
        if self.first_data_ms.is_none()
            && let Some(started_at) = self.active_load_started_at
        {
            self.first_data_ms = Some(started_at.elapsed().as_secs_f32() * 1000.0);
        }
    }

    fn poll_async_hazards(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.hazard_receiver.take() else {
            return;
        };
        let mut keep_receiver = true;
        loop {
            match receiver.try_recv() {
                Ok(message) => {
                    let changed = match message.update {
                        AsyncHazardUpdate::Preview(result) => {
                            self.install_hazard_result(result, true)
                        }
                        AsyncHazardUpdate::Final(result) => {
                            keep_receiver = false;
                            self.install_hazard_result(result, false)
                        }
                    };
                    if changed {
                        ctx.request_repaint();
                    }
                    if !keep_receiver {
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(50));
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    keep_receiver = false;
                    self.hazard_status = "Hazard loader disconnected".to_owned();
                    break;
                }
            }
        }
        if keep_receiver {
            self.hazard_receiver = Some(receiver);
        }
    }

    fn install_hazard_result(
        &mut self,
        result: Result<HazardOverlay, String>,
        updating: bool,
    ) -> bool {
        match result {
            Ok(overlay) => {
                if updating {
                    if !overlay.source_label.contains("NWS active alerts") {
                        return false;
                    }
                    if overlay.records.is_empty()
                        || self.hazard_overlay.as_ref().is_some_and(|existing| {
                            hazard_overlay_records_match(existing, &overlay)
                        })
                    {
                        return false;
                    }
                    let selected_event_id = self
                        .selected_hazard_record()
                        .map(|record| record.event_id.clone());
                    self.hazard_status = format!(
                        "Preview {} polygons from {} items in {:.1} ms",
                        overlay.records.len(),
                        overlay.parsed_items,
                        overlay.load_ms
                    );
                    self.selected_hazard_index = selected_hazard_index_for_event_id(
                        &overlay.records,
                        selected_event_id.as_deref(),
                    );
                    self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                    self.hazard_overlay = Some(overlay);
                    return true;
                }
                if let Some(existing) = &self.hazard_overlay
                    && hazard_overlay_records_match(existing, &overlay)
                {
                    if !updating
                        && (existing.source_label != overlay.source_label
                            || self.hazard_status.starts_with("Preview "))
                    {
                        self.hazard_status = format!(
                            "{} polygons from {} items in {:.1} ms",
                            overlay.records.len(),
                            overlay.parsed_items,
                            overlay.load_ms
                        );
                        self.hazard_overlay_generation =
                            self.hazard_overlay_generation.wrapping_add(1);
                        self.hazard_overlay = Some(overlay);
                        return true;
                    }
                    return false;
                }
                let overlay_change = self
                    .hazard_overlay
                    .as_ref()
                    .map(|existing| hazard_overlay_change(existing, &overlay));
                let selected_event_id = self
                    .selected_hazard_record()
                    .map(|record| record.event_id.clone());
                let phase = if updating { "Preview " } else { "" };
                let change_suffix = overlay_change
                    .filter(|change| !change.is_empty())
                    .map(|change| format!("; {}", change.status_text()))
                    .unwrap_or_default();
                self.hazard_status = format!(
                    "{}{} polygons from {} items in {:.1} ms{}",
                    phase,
                    overlay.records.len(),
                    overlay.parsed_items,
                    overlay.load_ms,
                    change_suffix
                );
                self.selected_hazard_index = selected_hazard_index_for_event_id(
                    &overlay.records,
                    selected_event_id.as_deref(),
                );
                self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                self.hazard_overlay = Some(overlay);
                true
            }
            Err(err) => {
                if self.hazard_overlay.is_some() {
                    self.hazard_status =
                        format!("Hazard refresh failed; keeping current polygons: {err}");
                    return true;
                }
                self.hazard_status = err;
                self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                self.hazard_overlay = None;
                self.selected_hazard_index = None;
                true
            }
        }
    }

    fn start_site_catalog_load(&mut self, ctx: &egui::Context) {
        if self.site_catalog_receiver.is_some() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.site_catalog_receiver = Some(receiver);
        thread::spawn(move || {
            let result = data_source::fetch_level2_radar_sites(7)
                .map(|sites| {
                    if sites.is_empty() {
                        data_source::fallback_sites()
                    } else {
                        sites
                    }
                })
                .map_err(|err| err.to_string());
            let _ = sender.send(AsyncSiteCatalogResult { result });
        });
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn poll_async_site_catalog(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.site_catalog_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(message) => {
                self.site_catalog_receiver = None;
                if let Ok(sites) = message.result {
                    self.install_site_catalog(sites);
                }
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.site_catalog_receiver = None;
            }
        }
    }

    fn install_site_catalog(&mut self, sites: Vec<RadarSite>) {
        if sites.is_empty() {
            return;
        }
        let current_site_id = self
            .volume
            .as_ref()
            .map(|volume| volume.site.id.clone())
            .or_else(|| self.selected_site().map(|site| site.level2_id.clone()));
        self.sites = sites;
        if let Some(site_id) = current_site_id
            && let Some(index) = self.sites.iter().position(|site| site.level2_id == site_id)
        {
            self.selected_site_index = index;
            return;
        }
        self.selected_site_index = self.selected_site_index.min(self.sites.len() - 1);
    }

    fn poll_async_load(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        loop {
            let Some(result) = self.load_receiver.as_ref().map(mpsc::Receiver::try_recv) else {
                return;
            };
            match result {
                Ok(message) => {
                    saw_message = true;
                    match message.update {
                        AsyncLoadUpdate::Preview(decoded) => {
                            self.install_preview_volume(decoded, ctx);
                            self.status = format!("Preview {}", message.label);
                        }
                        AsyncLoadUpdate::History(batch, select_frame) => {
                            let selected_loaded =
                                self.install_decoded_load_batch(batch, false, select_frame, ctx);
                            self.live_refresh_skip_reason = None;
                            if select_frame && selected_loaded {
                                self.status = format!("Loaded {}", message.label);
                            }
                        }
                        AsyncLoadUpdate::Unchanged { timings, reason } => {
                            if let Some(timings) = timings {
                                self.load_timing = Some(timings);
                            }
                            self.load_receiver = None;
                            self.pending_site_id = None;
                            self.live_refresh_skip_reason = Some(reason.clone());
                            self.status = format!("Current {} ({reason})", message.label);
                            ctx.request_repaint_after(Duration::from_secs(1));
                            return;
                        }
                        AsyncLoadUpdate::Final(result) => {
                            self.load_receiver = None;
                            self.pending_site_id = None;
                            match result {
                                Ok(batch) => {
                                    let selected_loaded =
                                        self.install_decoded_load_batch(batch, true, true, ctx);
                                    self.live_refresh_skip_reason = None;
                                    if selected_loaded {
                                        self.status = format!("Loaded {}", message.label);
                                    }
                                }
                                Err(err) => {
                                    self.status =
                                        format!("Load failed for {}: {err}", message.label);
                                }
                            }
                            ctx.request_repaint();
                            return;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if saw_message {
                        ctx.request_repaint();
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.load_receiver = None;
                    self.pending_site_id = None;
                    self.status = "L2 load worker disconnected".to_owned();
                    return;
                }
            }
        }
    }

    fn sanitize_selection(&mut self) {
        let Some(volume) = &self.volume else {
            return;
        };
        if volume.cuts.is_empty() {
            self.selected_cut = 0;
            return;
        }
        self.selected_cut = self.selected_cut.min(volume.cuts.len() - 1);
        for pane in &mut self.extra_panes {
            if let Some(cut) = pane.cut {
                pane.cut = Some(cut.min(volume.cuts.len() - 1));
            }
        }
        if is_displayable_on_cut(volume, self.selected_cut, &self.selected_product) {
            return;
        }
        let preferred = [
            DisplayProduct::Moment(MomentType::Reflectivity),
            DisplayProduct::Moment(MomentType::Velocity),
            DisplayProduct::DealiasedVelocity,
            DisplayProduct::StormRelativeVelocity,
            DisplayProduct::StormRelativeDealiasedVelocity,
            DisplayProduct::Moment(MomentType::SpectrumWidth),
            DisplayProduct::Moment(MomentType::DifferentialReflectivity),
            DisplayProduct::Moment(MomentType::CorrelationCoefficient),
            DisplayProduct::Moment(MomentType::DifferentialPhase),
        ];
        if let Some(product) = preferred
            .iter()
            .find(|product| is_displayable_on_cut(volume, self.selected_cut, product))
            .cloned()
        {
            self.selected_product = product;
        } else if let Some(product) = displayable_products(volume, self.selected_cut)
            .first()
            .cloned()
        {
            self.selected_product = product;
        }
    }

    fn handle_keyboard_navigation(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() {
            return;
        }

        if self.handle_product_hotkeys(ctx) {
            ctx.request_repaint();
            return;
        }

        let product_delta = ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                1
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                -1
            } else {
                0
            }
        });
        if product_delta != 0 {
            let stepped = match self.focused_extra_pane() {
                Some(slot) => self.step_pane_product(slot, product_delta),
                None => self.step_product(product_delta),
            };
            if stepped {
                ctx.request_repaint();
            }
            return;
        }

        let tilt_delta = ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                1
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                -1
            } else {
                0
            }
        });
        if tilt_delta != 0 {
            let stepped = match self.focused_extra_pane() {
                Some(slot) => self.step_pane_tilt(slot, tilt_delta),
                None => self.step_tilt(tilt_delta),
            };
            if stepped {
                ctx.request_repaint();
            }
        }
    }

    /// Number-row product hotkeys (customizable in config.json). Routes to
    /// the focused pane, like the arrow keys.
    fn handle_product_hotkeys(&mut self, ctx: &egui::Context) -> bool {
        const KEYS: [(egui::Key, &str); 10] = [
            (egui::Key::Num1, "1"),
            (egui::Key::Num2, "2"),
            (egui::Key::Num3, "3"),
            (egui::Key::Num4, "4"),
            (egui::Key::Num5, "5"),
            (egui::Key::Num6, "6"),
            (egui::Key::Num7, "7"),
            (egui::Key::Num8, "8"),
            (egui::Key::Num9, "9"),
            (egui::Key::Num0, "0"),
        ];
        let Some(volume) = self.volume.clone() else {
            return false;
        };
        for (key, name) in KEYS {
            if !ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key)) {
                continue;
            }
            let Some(label) = self.app_settings.product_hotkeys.get(name).cloned() else {
                return false;
            };
            let Some(product) = global_displayable_products(&volume)
                .into_iter()
                .find(|product| product.label().eq_ignore_ascii_case(&label))
            else {
                return false;
            };
            match self.focused_extra_pane() {
                Some(slot) => {
                    if self.extra_panes[slot].product != product {
                        self.extra_panes[slot].product = product;
                        self.extra_panes[slot].render_ms = None;
                    }
                }
                None => {
                    if self.selected_product != product {
                        if let Some(cut) =
                            best_cut_for_product(volume.as_ref(), self.selected_cut, &product)
                        {
                            self.selected_cut = cut;
                        }
                        self.selected_product = product;
                        self.sanitize_selection();
                        self.clear_texture();
                    }
                }
            }
            return true;
        }
        false
    }

    /// The extra-pane slot the sidebar/keyboard edits, when an extra pane is
    /// focused in a grid layout (None = the main pane / single-pane).
    fn focused_extra_pane(&self) -> Option<usize> {
        (self.grid_layout != PanelLayout::One
            && self.active_pane >= 1
            && self.active_pane - 1 < self.extra_panes.len())
        .then(|| self.active_pane - 1)
    }

    fn step_pane_product(&mut self, slot: usize, delta: isize) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return false;
        };
        let products = global_displayable_products(volume);
        let Some(next) =
            stepped_product(&products, &self.extra_panes[slot].product, delta).cloned()
        else {
            return false;
        };
        if self.extra_panes[slot].product == next {
            return false;
        }
        self.extra_panes[slot].product = next;
        self.extra_panes[slot].render_ms = None;
        true
    }

    /// Step the focused pane's tilt, pinning it (independent of main).
    fn step_pane_tilt(&mut self, slot: usize, delta: isize) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return false;
        };
        let product = self.extra_panes[slot].product.clone();
        let cuts = displayable_cuts_for_product(volume, &product);
        let current = self.extra_panes[slot].cut.unwrap_or(self.selected_cut);
        let Some(next_cut) = stepped_cut(&cuts, current, delta) else {
            return false;
        };
        if Some(next_cut) == self.extra_panes[slot].cut {
            return false;
        }
        self.extra_panes[slot].cut = Some(next_cut);
        true
    }

    fn step_product(&mut self, delta: isize) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return false;
        };
        let products = global_displayable_products(volume);
        let Some(next_product) = stepped_product(&products, &self.selected_product, delta).cloned()
        else {
            return false;
        };
        let Some(next_cut) = best_cut_for_product(volume, self.selected_cut, &next_product) else {
            return false;
        };
        if self.selected_product == next_product && self.selected_cut == next_cut {
            return false;
        }
        self.selected_product = next_product;
        self.selected_cut = next_cut;
        self.clear_texture();
        true
    }

    fn step_tilt(&mut self, delta: isize) -> bool {
        let Some(volume) = self.volume.as_ref() else {
            return false;
        };
        let cuts = displayable_cuts_for_product(volume, &self.selected_product);
        let Some(next_cut) = stepped_cut(&cuts, self.selected_cut, delta) else {
            return false;
        };
        if self.selected_cut == next_cut {
            return false;
        }
        self.selected_cut = next_cut;
        self.sanitize_selection();
        self.clear_texture();
        true
    }

    fn poll_async_render(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        // Frame budget: each install does a ColorImage conversion + texture
        // upload (~ms each); with several panes the drain is no longer one
        // message deep. Spill the rest to the next frame past the budget.
        let drain_start = Instant::now();
        loop {
            if saw_message && drain_start.elapsed() > Duration::from_millis(12) {
                ctx.request_repaint();
                break;
            }
            match self.render_receiver.try_recv() {
                Ok(message) => {
                    saw_message = true;
                    if message.pane != 0 {
                        self.install_pane_render_result(ctx, message);
                        continue;
                    }
                    let is_latest = self.pending_render_key.as_ref() == Some(&message.key);
                    match message.result {
                        Ok(rendered) if is_latest => {
                            self.pending_render_key = None;
                            self.install_rendered_texture(ctx, message.key, rendered);
                        }
                        Ok(rendered) => {
                            self.recycle_render_buffer(
                                rendered.rgba,
                                Some(rendered.buffer_signature),
                            );
                        }
                        Err(err) if is_latest => {
                            self.pending_render_key = None;
                            self.render_ms = None;
                            self.worker_ms = None;
                            self.texture_ms = None;
                            self.sample_cache_build_ms = None;
                            self.status = format!("Render failed: {err}");
                        }
                        Err(_) => {}
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_render_key = None;
                    for pane in &mut self.extra_panes {
                        pane.pending_render_key = None;
                    }
                    self.status = "Render worker disconnected".to_owned();
                    saw_message = true;
                    break;
                }
            }
        }
        if saw_message {
            ctx.request_repaint();
        } else if self.pending_render_key.is_some() {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }

    fn poll_radar_layer_renders(&mut self, ctx: &egui::Context) {
        let mut saw_message = false;
        let drain_start = Instant::now();
        for layer in &mut self.radar_layers {
            loop {
                if saw_message && drain_start.elapsed() > Duration::from_millis(12) {
                    ctx.request_repaint();
                    return;
                }
                match layer.render_receiver.try_recv() {
                    Ok(message) => {
                        saw_message = true;
                        let is_latest = layer.pending_render_key.as_ref() == Some(&message.key);
                        match message.result {
                            Ok(rendered) if is_latest => {
                                layer.pending_render_key = None;
                                Self::install_radar_layer_texture(
                                    ctx,
                                    layer,
                                    message.key,
                                    rendered,
                                );
                            }
                            Ok(rendered) => {
                                let _ = layer.render_recycle_sender.send(RenderRecycleBuffer {
                                    rgba: rendered.rgba,
                                    signature: Some(rendered.buffer_signature),
                                });
                            }
                            Err(err) if is_latest => {
                                layer.pending_render_key = None;
                                layer.render_ms = None;
                                layer.worker_ms = None;
                                layer.texture_ms = None;
                                layer.status = format!("Render failed: {err}");
                            }
                            Err(_) => {}
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        layer.pending_render_key = None;
                        layer.status = "Layer render worker disconnected".to_owned();
                        saw_message = true;
                        break;
                    }
                }
            }
        }

        if saw_message {
            ctx.request_repaint();
        } else if self
            .radar_layers
            .iter()
            .any(|layer| layer.pending_render_key.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }

    fn install_radar_layer_texture(
        ctx: &egui::Context,
        layer: &mut RadarOverlayLayer,
        key: TextureKey,
        rendered: RenderedTexture,
    ) {
        let RenderedTexture {
            width,
            height,
            rgba,
            buffer_signature,
            render_ms,
            worker_ms,
            radar_range_km,
            ..
        } = rendered;
        let texture_start = Instant::now();
        let color_image = radar_color_image_from_rgba([width, height], &rgba);
        let can_update_texture = layer
            .texture_key
            .as_ref()
            .is_some_and(|old_key| old_key.viewport.dimensions() == key.viewport.dimensions());
        if can_update_texture && let Some(texture) = &mut layer.texture {
            texture.set(color_image, radar_texture_options());
        } else {
            layer.texture = Some(ctx.load_texture(
                format!(
                    "radar-layer-{}-{}-{}-{}x{}",
                    layer.id,
                    key.cut,
                    key.product.label(),
                    key.viewport.width,
                    key.viewport.height
                ),
                color_image,
                radar_texture_options(),
            ));
        }
        layer.texture_key = Some(key);
        layer.render_ms = Some(render_ms);
        layer.worker_ms = Some(worker_ms);
        layer.texture_ms = Some(texture_start.elapsed().as_secs_f32() * 1000.0);
        layer.radar_range_km = radar_range_km;
        let _ = layer.render_recycle_sender.send(RenderRecycleBuffer {
            rgba,
            signature: Some(buffer_signature),
        });
        if layer.load_receiver.is_none() {
            layer.status = "Rendered".to_owned();
        }
    }

    fn recycle_render_buffer(
        &self,
        rgba: Vec<u8>,
        signature: Option<RenderWorkerViewportSignature>,
    ) {
        let _ = self
            .render_recycle_sender
            .send(RenderRecycleBuffer { rgba, signature });
    }

    fn start_render_request(&mut self, request: RenderRequest, ctx: &egui::Context) {
        let key = request.key.clone();
        match self.render_sender.send(request) {
            Ok(()) => {
                self.pending_render_key = Some(key);
                if self.load_receiver.is_none() {
                    self.status = "Rendering".to_owned();
                }
                ctx.request_repaint_after(Duration::from_millis(8));
            }
            Err(_) => {
                self.pending_render_key = None;
                self.status = "Render worker disconnected".to_owned();
            }
        }
    }

    fn request_texture_render(&mut self, ctx: &egui::Context, rect: egui::Rect) {
        let Some(volume) = self.volume.clone() else {
            return;
        };
        let Some((viewport_options, viewport_key)) = self.viewport_raster_options(ctx, rect) else {
            return;
        };
        let color_tables = self.render_color_tables_for_product(&self.selected_product);
        let color_table_signature =
            color_tables.signature_for_family(self.selected_product.color_family());
        let render_dealiased_velocity =
            self.product_render_uses_dealiased_velocity(&self.selected_product);
        let key = TextureKey {
            volume_ptr: Arc::as_ptr(&volume) as usize,
            cut: self.selected_cut,
            product: self.selected_product.clone(),
            render_dealiased_velocity,
            color_table_signature,
            storm_motion_key: self.storm_motion_key(),
            hail_levels_key: self.hail_levels_key(),
            viewport: viewport_key,
        };
        if self.texture_key.as_ref() == Some(&key) {
            return;
        }
        if self.pending_render_key.as_ref() == Some(&key) {
            ctx.request_repaint_after(Duration::from_millis(8));
            return;
        }

        self.start_render_request(
            RenderRequest {
                key,
                pane: 0,
                volume,
                cut: self.selected_cut,
                product: self.selected_product.clone(),
                render_dealiased_velocity,
                plain_velocity_render_dealiased: self.unfold_velocity_display,
                color_tables,
                storm_motion: self.current_storm_motion(),
                hail_levels_m: self.hail_levels_m(),
                viewport_options,
                radar_range_km: self
                    .selected_grid_range_km()
                    .unwrap_or(DEFAULT_RADAR_RANGE_KM),
            },
            ctx,
        );
    }

    fn product_render_uses_dealiased_velocity(&self, product: &DisplayProduct) -> bool {
        product.render_uses_dealiased_velocity(self.unfold_velocity_display)
    }

    fn hail_levels_m(&self) -> (f32, f32) {
        (
            self.hail_freezing_level_km * 1000.0,
            self.hail_minus20_level_km * 1000.0,
        )
    }

    /// Quantized hail-level key (0.1 km steps) for render/texture keys.
    fn hail_levels_key(&self) -> (i16, i16) {
        (
            (self.hail_freezing_level_km * 10.0).round() as i16,
            (self.hail_minus20_level_km * 10.0).round() as i16,
        )
    }

    fn render_color_tables_for_product(&self, product: &DisplayProduct) -> ColorTableSet {
        let mut color_tables = self.color_tables.clone();
        if self.flip_velocity_color_polarity && product.color_family() == ColorTableFamily::Velocity
        {
            let current = color_tables.for_family(ColorTableFamily::Velocity);
            color_tables.set_family(
                ColorTableFamily::Velocity,
                current.mirrored_values(format!("{} (flipped)", current.name())),
            );
        }
        // Display threshold: clamp the family's table at render time. The
        // clamp participates in the table signature, so render keys (and the
        // colorbar, which shows the cut) follow automatically.
        let family = product.color_family();
        if let Some(&threshold) = self.display_thresholds.get(family.label()) {
            let current = color_tables.for_family(family);
            color_tables.set_family(
                family,
                current
                    .with_display_threshold(Some(threshold), family_threshold_is_symmetric(family)),
            );
        }
        color_tables
    }

    fn request_radar_layer_renders(&mut self, ctx: &egui::Context, rect: egui::Rect) {
        let mut requests = Vec::new();
        for (index, layer) in self.radar_layers.iter().enumerate() {
            if !layer.visible {
                continue;
            }
            let Some(volume) = layer.volume.clone() else {
                continue;
            };
            let Some((radar_lat, radar_lon)) = layer.radar_location() else {
                continue;
            };
            let Some((viewport_options, viewport_key)) =
                self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
            else {
                continue;
            };
            let product = self.selected_product.clone();
            let Some(cut) = best_cut_for_product(volume.as_ref(), self.selected_cut, &product)
            else {
                continue;
            };
            let color_tables = self.render_color_tables_for_product(&product);
            let color_table_signature = color_tables.signature_for_family(product.color_family());
            let render_dealiased_velocity = self.product_render_uses_dealiased_velocity(&product);
            let key = TextureKey {
                volume_ptr: Arc::as_ptr(&volume) as usize,
                cut,
                product: product.clone(),
                render_dealiased_velocity,
                color_table_signature,
                storm_motion_key: self.storm_motion_key(),
                hail_levels_key: self.hail_levels_key(),
                viewport: viewport_key,
            };
            if layer.texture_key.as_ref() == Some(&key)
                || layer.pending_render_key.as_ref() == Some(&key)
            {
                continue;
            }
            let radar_range_km = selected_grid_range_km_for(volume.as_ref(), cut, &product)
                .unwrap_or(DEFAULT_RADAR_RANGE_KM);
            requests.push((
                index,
                RenderRequest {
                    key,
                    pane: 0,
                    volume,
                    cut,
                    product,
                    render_dealiased_velocity,
                    plain_velocity_render_dealiased: self.unfold_velocity_display,
                    color_tables,
                    storm_motion: self.current_storm_motion(),
                    hail_levels_m: self.hail_levels_m(),
                    viewport_options,
                    radar_range_km,
                },
            ));
        }

        for (index, request) in requests {
            if let Some(layer) = self.radar_layers.get_mut(index) {
                let key = request.key.clone();
                match layer.render_sender.send(request) {
                    Ok(()) => {
                        layer.pending_render_key = Some(key);
                        if layer.load_receiver.is_none() {
                            layer.status = "Rendering".to_owned();
                        }
                    }
                    Err(_) => {
                        layer.pending_render_key = None;
                        layer.status = "Layer render worker disconnected".to_owned();
                    }
                }
            }
        }

        if self
            .radar_layers
            .iter()
            .any(|layer| layer.pending_render_key.is_some())
        {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }

    /// Merge a request into the worker queue: a newer request replaces the
    /// queued one for the SAME pane (the old newest-only coalescing,
    /// per-pane), and never displaces other panes' requests.
    fn merge_render_request(queue: &mut VecDeque<RenderRequest>, request: RenderRequest) {
        if let Some(slot) = queue.iter_mut().find(|queued| queued.pane == request.pane) {
            *slot = request;
        } else {
            queue.push_back(request);
        }
    }

    /// Drain everything currently waiting on the channel into the worker
    /// queue (per-pane coalescing). Returns whether anything arrived.
    fn queue_newer_render_requests(
        receiver: &mpsc::Receiver<RenderRequest>,
        queue: &mut VecDeque<RenderRequest>,
    ) -> std::result::Result<bool, mpsc::TryRecvError> {
        match receiver.try_recv() {
            Ok(first) => {
                Self::merge_render_request(queue, first);
                for newer in receiver.try_iter() {
                    Self::merge_render_request(queue, newer);
                }
                Ok(true)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(false),
            Err(err @ mpsc::TryRecvError::Disconnected) => Err(err),
        }
    }

    fn render_viewport_payload(
        request: &RenderRequest,
        reusable_pixels: &mut Vec<u8>,
        reusable_pixels_signature: &mut Option<RenderWorkerViewportSignature>,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
    ) -> Result<RenderedTexture, String> {
        let worker_start = Instant::now();
        let required_len = viewport_rgba_buffer_len(request.viewport_options);
        if reusable_pixels.len() != required_len {
            reusable_pixels.resize(required_len, 0);
            *reusable_pixels_signature = None;
        }

        let volume_ptr = Arc::as_ptr(&request.volume) as usize;
        let base_moment = request.product.base_moment();
        let dealiased_velocity = request.render_dealiased_velocity;
        let derived = request.product.derived();
        // Volume-wide derived products (CREF/ET/VIL) are tilt-independent — they
        // always render on the base reflectivity tilt — so key their cache on a
        // constant instead of the selected cut. Otherwise stepping tilts would
        // miss the cache and re-run the full-volume column walk for a byte-
        // identical grid (the app's speed identity).
        let cache_cut = match derived {
            Some(d) if d.is_volume_wide() => usize::MAX,
            _ => request.cut,
        };
        let color_table_signature = request.key.color_table_signature;
        let cached_volume_ptr = moment_caches.first().map(|cached| cached.volume_ptr);
        if cached_volume_ptr.is_some_and(|cached_volume_ptr| cached_volume_ptr != volume_ptr) {
            moment_caches.clear();
            sample_caches.clear();
            last_direct_viewports.clear();
        }
        if Self::touch_moment_cache(
            moment_caches,
            volume_ptr,
            cache_cut,
            &base_moment,
            dealiased_velocity,
            derived,
            color_table_signature,
        )
        .is_none()
        {
            let cache = if let Some(d) = derived {
                build_derived_moment_cache(
                    request.volume.as_ref(),
                    d,
                    request.cut,
                    &request.color_tables,
                    request.hail_levels_m,
                )
            } else if dealiased_velocity {
                ViewportMomentCache::new_dealiased_velocity_with_color_tables(
                    request.volume.as_ref(),
                    request.cut,
                    &request.color_tables,
                )
                .map_err(|err| err.to_string())
            } else {
                ViewportMomentCache::new_with_color_tables(
                    request.volume.as_ref(),
                    request.cut,
                    base_moment.clone(),
                    &request.color_tables,
                )
                .map_err(|err| err.to_string())
            }?;
            Self::insert_moment_cache(
                moment_caches,
                cache_policy,
                RenderWorkerMomentCache {
                    volume_ptr,
                    cut: cache_cut,
                    moment: base_moment.clone(),
                    dealiased_velocity,
                    derived,
                    color_table_signature,
                    cache,
                    storm_palette_cache: None,
                },
            );
        }
        let moment_cache = moment_caches
            .last_mut()
            .expect("render cache is prepared before rendering");
        let cache = &moment_cache.cache;
        let viewport_signature = RenderWorkerViewportSignature::new(
            volume_ptr,
            request.cut,
            request.product.clone(),
            base_moment.clone(),
            dealiased_velocity,
            color_table_signature,
            request.key.storm_motion_key,
            request.key.hail_levels_key,
            request.key.viewport,
        );
        let sample_cache_signature = RenderWorkerSampleCacheSignature::new(
            volume_ptr,
            request.cut,
            request.product.clone(),
            base_moment.clone(),
            dealiased_velocity,
            request.key.viewport,
        );

        let start = Instant::now();
        let mut sample_cache_build_ms = None;
        let sample_cache_matches = Self::touch_sample_cache(sample_caches, &sample_cache_signature);
        if !sample_cache_matches
            && Self::has_direct_viewport(last_direct_viewports, &viewport_signature)
            && cache_policy.should_build_sample_cache_for_moment_cache(
                cache,
                request.volume.as_ref(),
                request.viewport_options,
            )?
        {
            let cache_build_start = Instant::now();
            let built_sample_cache = cache
                .build_sample_cache(request.volume.as_ref(), request.viewport_options)
                .map_err(|err| err.to_string())?;
            sample_cache_build_ms = Some(cache_build_start.elapsed().as_secs_f32() * 1000.0);
            Self::insert_sample_cache(
                sample_caches,
                cache_policy,
                sample_cache_signature.clone(),
                built_sample_cache,
            );
            Self::forget_direct_viewport(last_direct_viewports, &viewport_signature);
        }
        let matching_sample_cache = sample_caches
            .last()
            .filter(|cached| cached.signature == sample_cache_signature);
        let can_reuse_transparency = matching_sample_cache.is_some()
            && reusable_pixels_signature.as_ref() == Some(&viewport_signature);
        *reusable_pixels_signature = None;

        let (width, height, used_sample_cache) = if request.product.is_storm_relative_velocity() {
            let storm_motion_key = request.key.storm_motion_key;
            let palette_matches = moment_cache
                .storm_palette_cache
                .as_ref()
                .is_some_and(|cached| cached.storm_motion_key == storm_motion_key);
            if !palette_matches {
                moment_cache.storm_palette_cache = Some(RenderWorkerStormPaletteCache {
                    storm_motion_key,
                    cache: cache
                        .build_storm_relative_velocity_palette_cache(
                            request.volume.as_ref(),
                            request.storm_motion,
                        )
                        .map_err(|err| err.to_string())?,
                });
            }
            let palette_cache = moment_cache
                .storm_palette_cache
                .as_ref()
                .and_then(|cached| cached.cache.as_ref());
            if let Some(sample_cache) = matching_sample_cache {
                let dimensions = if can_reuse_transparency {
                    if let Some(palette_cache) = palette_cache {
                        cache.render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency_and_palette_cache(
                            request.volume.as_ref(),
                            request.storm_motion,
                            palette_cache,
                            &sample_cache.cache,
                            reusable_pixels,
                        )
                    } else {
                        cache
                            .render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
                                request.volume.as_ref(),
                                request.storm_motion,
                                &sample_cache.cache,
                                reusable_pixels,
                            )
                    }
                } else if let Some(palette_cache) = palette_cache {
                    cache.render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        palette_cache,
                        &sample_cache.cache,
                        reusable_pixels,
                    )
                } else {
                    cache.render_storm_relative_velocity_rgba_with_sample_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        &sample_cache.cache,
                        reusable_pixels,
                    )
                }
                .map_err(|err| err.to_string())?;
                (dimensions.0, dimensions.1, true)
            } else {
                let dimensions = if let Some(palette_cache) = palette_cache {
                    cache.render_storm_relative_velocity_rgba_into_with_palette_cache(
                        request.volume.as_ref(),
                        request.storm_motion,
                        palette_cache,
                        request.viewport_options,
                        reusable_pixels,
                    )
                } else {
                    cache.render_storm_relative_velocity_rgba_into(
                        request.volume.as_ref(),
                        request.storm_motion,
                        request.viewport_options,
                        reusable_pixels,
                    )
                }
                .map_err(|err| err.to_string())?;
                (dimensions.0, dimensions.1, false)
            }
        } else if let Some(sample_cache) = matching_sample_cache {
            let dimensions = if can_reuse_transparency {
                cache.render_moment_rgba_with_sample_cache_reusing_transparency(
                    request.volume.as_ref(),
                    &sample_cache.cache,
                    reusable_pixels,
                )
            } else {
                cache.render_moment_rgba_with_sample_cache(
                    request.volume.as_ref(),
                    &sample_cache.cache,
                    reusable_pixels,
                )
            }
            .map_err(|err| err.to_string())?;
            (dimensions.0, dimensions.1, true)
        } else {
            let dimensions = cache
                .render_moment_rgba_into(
                    request.volume.as_ref(),
                    request.viewport_options,
                    reusable_pixels,
                )
                .map_err(|err| err.to_string())?;
            (dimensions.0, dimensions.1, false)
        };
        let render_ms = start.elapsed().as_secs_f32() * 1000.0;
        if !used_sample_cache {
            Self::remember_direct_viewport(
                last_direct_viewports,
                cache_policy,
                viewport_signature.clone(),
            );
        }
        let rgba = std::mem::take(reusable_pixels);
        let worker_ms = worker_start.elapsed().as_secs_f32() * 1000.0;

        Ok(RenderedTexture {
            width: width as usize,
            height: height as usize,
            rgba,
            buffer_signature: viewport_signature,
            render_ms,
            worker_ms,
            sample_cache_build_ms,
            used_sample_cache,
            radar_range_km: request.radar_range_km,
        })
    }

    fn should_prefetch_velocity_interaction_cache(
        request: &RenderRequest,
        rendered: &RenderedTexture,
        cache_policy: RenderWorkerCachePolicy,
    ) -> bool {
        request.product.base_moment() != MomentType::Velocity
            && cache_policy
                .should_prefetch_interaction_cache(rendered.buffer_signature.viewport.dimensions())
            && Self::prefetch_velocity_cut(request).is_some()
    }

    fn prefetch_velocity_cut(request: &RenderRequest) -> Option<usize> {
        let product = DisplayProduct::Moment(MomentType::Velocity);
        if is_displayable_on_cut(request.volume.as_ref(), request.cut, &product) {
            Some(request.cut)
        } else {
            best_cut_for_product(request.volume.as_ref(), request.cut, &product)
        }
    }

    fn warm_sample_cache_after_direct_render(
        request: &RenderRequest,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let volume_ptr = Arc::as_ptr(&request.volume) as usize;
        let viewport_signature = RenderWorkerViewportSignature::new(
            volume_ptr,
            request.cut,
            request.product.clone(),
            request.product.base_moment(),
            request.render_dealiased_velocity,
            request.key.color_table_signature,
            request.key.storm_motion_key,
            request.key.hail_levels_key,
            request.key.viewport,
        );
        let sample_cache_signature = RenderWorkerSampleCacheSignature::new(
            volume_ptr,
            request.cut,
            request.product.clone(),
            request.product.base_moment(),
            request.render_dealiased_velocity,
            request.key.viewport,
        );
        if Self::touch_sample_cache(sample_caches, &sample_cache_signature) {
            return;
        }
        let Some(moment_index) = Self::touch_moment_cache(
            moment_caches,
            viewport_signature.volume_ptr,
            viewport_signature.cut,
            &viewport_signature.moment,
            request.render_dealiased_velocity,
            request.product.derived(),
            viewport_signature.color_table_signature,
        ) else {
            return;
        };
        let moment_cache = &moment_caches[moment_index];
        let Ok(should_build) = cache_policy.should_build_sample_cache_for_moment_cache(
            &moment_cache.cache,
            request.volume.as_ref(),
            request.viewport_options,
        ) else {
            return;
        };
        if !should_build {
            return;
        }
        let Ok(cache) = moment_cache
            .cache
            .build_sample_cache(request.volume.as_ref(), request.viewport_options)
        else {
            return;
        };
        Self::insert_sample_cache(sample_caches, cache_policy, sample_cache_signature, cache);
        Self::forget_direct_viewport(last_direct_viewports, &viewport_signature);
    }

    fn warm_velocity_interaction_cache_after_direct_render(
        request: &RenderRequest,
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let Some(cut) = Self::prefetch_velocity_cut(request) else {
            return;
        };

        if request.product.base_moment() == MomentType::Velocity
            || !cache_policy.should_prefetch_interaction_cache(request.key.viewport.dimensions())
        {
            return;
        }

        let volume_ptr = Arc::as_ptr(&request.volume) as usize;
        let velocity_color_table_signature = request
            .color_tables
            .signature_for_family(ColorTableFamily::Velocity);
        let velocity_render_dealiased = request.plain_velocity_render_dealiased;
        let sample_cache_signature = RenderWorkerSampleCacheSignature::new(
            volume_ptr,
            cut,
            DisplayProduct::Moment(MomentType::Velocity),
            MomentType::Velocity,
            velocity_render_dealiased,
            request.key.viewport,
        );

        if Self::touch_moment_cache(
            moment_caches,
            volume_ptr,
            cut,
            &MomentType::Velocity,
            velocity_render_dealiased,
            None,
            velocity_color_table_signature,
        )
        .is_none()
        {
            let cache = if velocity_render_dealiased {
                ViewportMomentCache::new_dealiased_velocity_with_color_tables(
                    request.volume.as_ref(),
                    cut,
                    &request.color_tables,
                )
            } else {
                ViewportMomentCache::new_with_color_tables(
                    request.volume.as_ref(),
                    cut,
                    MomentType::Velocity,
                    &request.color_tables,
                )
            };
            let Ok(cache) = cache else {
                return;
            };
            Self::insert_moment_cache(
                moment_caches,
                cache_policy,
                RenderWorkerMomentCache {
                    volume_ptr,
                    cut,
                    moment: MomentType::Velocity,
                    dealiased_velocity: velocity_render_dealiased,
                    derived: None,
                    color_table_signature: velocity_color_table_signature,
                    cache,
                    storm_palette_cache: None,
                },
            );
        }

        if !Self::touch_sample_cache(sample_caches, &sample_cache_signature)
            && let Some(moment_cache) = moment_caches.last()
            && let Ok(true) = cache_policy.should_build_sample_cache_for_moment_cache(
                &moment_cache.cache,
                request.volume.as_ref(),
                request.viewport_options,
            )
            && let Ok(cache) = moment_cache
                .cache
                .build_sample_cache(request.volume.as_ref(), request.viewport_options)
        {
            Self::insert_sample_cache(sample_caches, cache_policy, sample_cache_signature, cache);
        }

        let Some(moment_index) = Self::touch_moment_cache(
            moment_caches,
            volume_ptr,
            cut,
            &MomentType::Velocity,
            velocity_render_dealiased,
            None,
            velocity_color_table_signature,
        ) else {
            return;
        };
        let moment_cache = &mut moment_caches[moment_index];
        let storm_motion_key = request.key.storm_motion_key;
        let palette_matches = moment_cache
            .storm_palette_cache
            .as_ref()
            .is_some_and(|cached| cached.storm_motion_key == storm_motion_key);
        if palette_matches {
            return;
        }
        if let Ok(cache) = moment_cache
            .cache
            .build_storm_relative_velocity_palette_cache(
                request.volume.as_ref(),
                request.storm_motion,
            )
        {
            moment_cache.storm_palette_cache = Some(RenderWorkerStormPaletteCache {
                storm_motion_key,
                cache,
            });
        }
    }

    fn touch_moment_cache(
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        volume_ptr: usize,
        cut: usize,
        moment: &MomentType,
        dealiased_velocity: bool,
        derived: Option<DerivedProduct>,
        color_table_signature: u64,
    ) -> Option<usize> {
        let index = moment_caches.iter().position(|cached| {
            cached.volume_ptr == volume_ptr
                && cached.cut == cut
                && cached.moment == *moment
                && cached.dealiased_velocity == dealiased_velocity
                && cached.derived == derived
                && cached.color_table_signature == color_table_signature
        })?;
        let cached = moment_caches.remove(index);
        moment_caches.push(cached);
        Some(moment_caches.len() - 1)
    }

    fn insert_moment_cache(
        moment_caches: &mut Vec<RenderWorkerMomentCache>,
        cache_policy: RenderWorkerCachePolicy,
        cache: RenderWorkerMomentCache,
    ) {
        moment_caches.retain(|cached| {
            cached.volume_ptr != cache.volume_ptr
                || cached.cut != cache.cut
                || cached.moment != cache.moment
                || cached.dealiased_velocity != cache.dealiased_velocity
                || cached.derived != cache.derived
        });
        moment_caches.push(cache);
        while moment_caches.len() > cache_policy.moment_cache_capacity() {
            moment_caches.remove(0);
        }
    }

    fn touch_sample_cache(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        signature: &RenderWorkerSampleCacheSignature,
    ) -> bool {
        let Some(index) = sample_caches
            .iter()
            .position(|cached| &cached.signature == signature)
        else {
            return false;
        };
        let cached = sample_caches.remove(index);
        sample_caches.push(cached);
        true
    }

    fn insert_sample_cache(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
        signature: RenderWorkerSampleCacheSignature,
        cache: ViewportSampleCache,
    ) {
        sample_caches.retain(|cached| cached.signature != signature);
        sample_caches.push(RenderWorkerSampleCache { signature, cache });
        Self::trim_sample_caches(sample_caches, cache_policy);
    }

    fn trim_sample_caches(
        sample_caches: &mut Vec<RenderWorkerSampleCache>,
        cache_policy: RenderWorkerCachePolicy,
    ) {
        let capacity = cache_policy.sample_cache_capacity();
        let byte_budget = cache_policy.sample_cache_bytes();
        while sample_caches.len() > capacity
            || Self::sample_cache_storage_bytes(sample_caches) > byte_budget
        {
            if sample_caches.is_empty() {
                break;
            }
            sample_caches.remove(0);
        }
    }

    fn sample_cache_storage_bytes(sample_caches: &[RenderWorkerSampleCache]) -> usize {
        sample_caches
            .iter()
            .map(|cached| cached.cache.storage_bytes())
            .sum()
    }

    fn has_direct_viewport(
        last_direct_viewports: &[RenderWorkerViewportSignature],
        signature: &RenderWorkerViewportSignature,
    ) -> bool {
        last_direct_viewports
            .iter()
            .any(|last_direct| last_direct == signature)
    }

    fn remember_direct_viewport(
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        cache_policy: RenderWorkerCachePolicy,
        signature: RenderWorkerViewportSignature,
    ) {
        Self::forget_direct_viewport(last_direct_viewports, &signature);
        last_direct_viewports.push(signature);
        let capacity = cache_policy.direct_viewport_capacity();
        while last_direct_viewports.len() > capacity {
            last_direct_viewports.remove(0);
        }
    }

    fn forget_direct_viewport(
        last_direct_viewports: &mut Vec<RenderWorkerViewportSignature>,
        signature: &RenderWorkerViewportSignature,
    ) {
        last_direct_viewports.retain(|last_direct| last_direct != signature);
    }

    fn install_rendered_texture(
        &mut self,
        ctx: &egui::Context,
        key: TextureKey,
        rendered: RenderedTexture,
    ) {
        let RenderedTexture {
            width,
            height,
            rgba,
            buffer_signature,
            render_ms,
            worker_ms,
            sample_cache_build_ms,
            used_sample_cache,
            radar_range_km,
        } = rendered;
        let texture_start = Instant::now();
        let color_image = radar_color_image_from_rgba([width, height], &rgba);
        let can_update_texture = self
            .texture_key
            .as_ref()
            .is_some_and(|old_key| old_key.viewport.dimensions() == key.viewport.dimensions());
        if can_update_texture && let Some(texture) = &mut self.texture {
            texture.set(color_image, radar_texture_options());
        } else {
            self.texture = Some(ctx.load_texture(
                format!(
                    "radar-{}-{}-{}x{}",
                    key.cut,
                    key.product.label(),
                    key.viewport.width,
                    key.viewport.height
                ),
                color_image,
                radar_texture_options(),
            ));
        }
        let texture_ms = texture_start.elapsed().as_secs_f32() * 1000.0;
        self.texture_key = Some(key);
        if self.first_texture_ms.is_none()
            && let Some(started_at) = self.active_load_started_at
        {
            self.first_texture_ms = Some(started_at.elapsed().as_secs_f32() * 1000.0);
        }
        self.perf.record_render(
            render_ms,
            used_sample_cache,
            worker_ms,
            texture_ms,
            sample_cache_build_ms,
        );
        self.render_ms = Some(render_ms);
        self.worker_ms = Some(worker_ms);
        self.texture_ms = Some(texture_ms);
        self.sample_cache_build_ms = sample_cache_build_ms;
        self.radar_range_km = radar_range_km;
        self.recycle_render_buffer(rgba, Some(buffer_signature));
        if self.load_receiver.is_none() {
            self.status = "Rendered".to_owned();
        }
    }

    /// Resize the extra-pane list for a new grid layout. Existing panes keep
    /// their product; new panes default to the classic warning-desk quad
    /// (VEL → CC → ZDR after the primary REF), filtered by availability.
    fn sync_extra_panes(&mut self) {
        self.active_pane = self
            .active_pane
            .min(self.grid_layout.panel_count().saturating_sub(1));
        let wanted = self.grid_layout.panel_count().saturating_sub(1);
        if self.extra_panes.len() > wanted {
            self.extra_panes.truncate(wanted);
            return;
        }
        let available: Vec<DisplayProduct> = self
            .volume
            .as_deref()
            .map(global_displayable_products)
            .unwrap_or_default();
        let preferred = [
            DisplayProduct::Moment(MomentType::Velocity),
            DisplayProduct::Moment(MomentType::CorrelationCoefficient),
            DisplayProduct::Moment(MomentType::DifferentialReflectivity),
            DisplayProduct::Moment(MomentType::SpectrumWidth),
        ];
        while self.extra_panes.len() < wanted {
            let taken: Vec<&DisplayProduct> = self.extra_panes.iter().map(|p| &p.product).collect();
            let next = preferred
                .iter()
                .find(|p| {
                    !taken.contains(p)
                        && **p != self.selected_product
                        && (available.is_empty() || available.contains(p))
                })
                .cloned()
                .unwrap_or(DisplayProduct::Moment(MomentType::Reflectivity));
            self.extra_panes.push(ViewPane::new(next));
        }
    }

    /// Route a render result for an extra pane (1-based index) to its texture.
    fn install_pane_render_result(&mut self, ctx: &egui::Context, message: AsyncRenderResult) {
        let Some(pane_slot) = message.pane.checked_sub(1) else {
            return;
        };
        if pane_slot >= self.extra_panes.len() {
            // Pane was removed (layout shrank) — recycle the buffer.
            if let Ok(rendered) = message.result {
                self.recycle_render_buffer(rendered.rgba, Some(rendered.buffer_signature));
            }
            return;
        }
        let mut recycle: Option<(Vec<u8>, RenderWorkerViewportSignature)> = None;
        {
            let pane = &mut self.extra_panes[pane_slot];
            let is_latest = pane.pending_render_key.as_ref() == Some(&message.key);
            match message.result {
                Ok(rendered) if is_latest => {
                    pane.pending_render_key = None;
                    let RenderedTexture {
                        width,
                        height,
                        rgba,
                        buffer_signature,
                        render_ms,
                        ..
                    } = rendered;
                    let color_image = radar_color_image_from_rgba([width, height], &rgba);
                    let can_update = pane.texture_key.as_ref().is_some_and(|old| {
                        old.viewport.dimensions() == message.key.viewport.dimensions()
                    });
                    if can_update && let Some(texture) = &mut pane.texture {
                        texture.set(color_image, radar_texture_options());
                    } else {
                        pane.texture = Some(ctx.load_texture(
                            format!(
                                "pane{}-{}-{}x{}",
                                message.pane,
                                message.key.product.label(),
                                message.key.viewport.width,
                                message.key.viewport.height
                            ),
                            color_image,
                            radar_texture_options(),
                        ));
                    }
                    pane.texture_key = Some(message.key);
                    pane.render_ms = Some(render_ms);
                    recycle = Some((rgba, buffer_signature));
                }
                Ok(rendered) => {
                    recycle = Some((rendered.rgba, rendered.buffer_signature));
                }
                Err(_) if is_latest => {
                    pane.pending_render_key = None;
                    pane.render_ms = None;
                }
                Err(_) => {}
            }
        }
        if let Some((rgba, signature)) = recycle {
            self.recycle_render_buffer(rgba, Some(signature));
        }
    }

    /// Request a render for an extra pane (1-based), mirroring
    /// request_texture_render but reading the pane's product. Shares the
    /// volume, geo transform, tilt, and render worker with the primary view.
    fn request_pane_render(&mut self, ctx: &egui::Context, rect: egui::Rect, pane_number: usize) {
        let Some(pane_slot) = pane_number.checked_sub(1) else {
            return;
        };
        let Some(volume) = self.volume.clone() else {
            return;
        };
        let Some((viewport_options, viewport_key)) = self.viewport_raster_options(ctx, rect) else {
            return;
        };
        let Some((product, pane_cut)) = self
            .extra_panes
            .get(pane_slot)
            .map(|p| (p.product.clone(), p.cut))
        else {
            return;
        };
        let preferred_cut = pane_cut.unwrap_or(self.selected_cut);
        let Some(cut) = best_cut_for_product(volume.as_ref(), preferred_cut, &product) else {
            return;
        };
        let color_tables = self.render_color_tables_for_product(&product);
        let color_table_signature = color_tables.signature_for_family(product.color_family());
        let render_dealiased_velocity = self.product_render_uses_dealiased_velocity(&product);
        let key = TextureKey {
            volume_ptr: Arc::as_ptr(&volume) as usize,
            cut,
            product: product.clone(),
            render_dealiased_velocity,
            color_table_signature,
            storm_motion_key: self.storm_motion_key(),
            hail_levels_key: self.hail_levels_key(),
            viewport: viewport_key,
        };
        {
            let pane = &self.extra_panes[pane_slot];
            if pane.texture_key.as_ref() == Some(&key) {
                return;
            }
            if pane.pending_render_key.as_ref() == Some(&key) {
                ctx.request_repaint_after(Duration::from_millis(8));
                return;
            }
        }
        let radar_range_km = selected_grid_range_km_for(volume.as_ref(), cut, &product)
            .unwrap_or(DEFAULT_RADAR_RANGE_KM);
        let request = RenderRequest {
            key: key.clone(),
            pane: pane_number,
            volume,
            cut,
            product,
            render_dealiased_velocity,
            plain_velocity_render_dealiased: self.unfold_velocity_display,
            color_tables,
            storm_motion: self.current_storm_motion(),
            hail_levels_m: self.hail_levels_m(),
            viewport_options,
            radar_range_km,
        };
        match self.render_sender.send(request) {
            Ok(()) => {
                self.extra_panes[pane_slot].pending_render_key = Some(key);
                ctx.request_repaint_after(Duration::from_millis(8));
            }
            Err(_) => {
                self.extra_panes[pane_slot].pending_render_key = None;
            }
        }
    }

    /// Blit an extra pane's rendered texture into its cell, anchored to the
    /// shared geo transform (mirrors draw_radar_layer's texture path).
    fn draw_extra_pane_layer(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
        pane_number: usize,
    ) {
        if self.volume.is_none() {
            return;
        }
        let Some(pane) = pane_number
            .checked_sub(1)
            .and_then(|slot| self.extra_panes.get(slot))
        else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.radar_location() else {
            return;
        };
        if let Some(texture) = &pane.texture {
            let image_rect = pane
                .texture_key
                .as_ref()
                .map(|key| self.radar_texture_rect(ctx, rect, latitude_deg, longitude_deg, key))
                .unwrap_or(rect);
            painter.image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }

    fn viewport_raster_options(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
    ) -> Option<(ViewportRasterOptions, ViewportKey)> {
        let (radar_lat, radar_lon) = self.radar_location()?;
        self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
    }

    fn viewport_raster_options_for_location(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
        radar_lat: f32,
        radar_lon: f32,
    ) -> Option<(ViewportRasterOptions, ViewportKey)> {
        let pixels_per_point = ctx.pixels_per_point().max(1.0);
        let width = (rect.width() * pixels_per_point).round().max(1.0) as u32;
        let height = (rect.height() * pixels_per_point).round().max(1.0) as u32;
        let radar_position = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
        let radar_x_px = (radar_position.x - rect.left()) * pixels_per_point;
        let radar_y_px = (radar_position.y - rect.top()) * pixels_per_point;
        let km_per_px_y = 111.32 / (self.map_scale * pixels_per_point);
        let km_per_px_x = 111.32 * radar_lat.to_radians().cos().abs().max(0.02)
            / (self.map_scale * self.lon_screen_scale() * pixels_per_point);
        let options = ViewportRasterOptions {
            width,
            height,
            radar_x_px,
            radar_y_px,
            km_per_px_x,
            km_per_px_y,
        };
        let key = ViewportKey {
            width,
            height,
            radar_x_px: (radar_x_px * 8.0).round() as i32,
            radar_y_px: (radar_y_px * 8.0).round() as i32,
            km_per_px_x: (km_per_px_x * 1_000_000.0).round() as i32,
            km_per_px_y: (km_per_px_y * 1_000_000.0).round() as i32,
        };
        Some((options, key))
    }

    fn reset_view(&mut self) {
        self.map_scale = DEFAULT_MAP_SCALE;
        self.center_selected_site();
    }

    fn selected_site(&self) -> Option<&RadarSite> {
        self.sites.get(self.selected_site_index)
    }

    /// Remember the just-loaded site as the startup site (best-effort persist).
    fn remember_startup_site(&mut self) {
        let Some(id) = self.selected_site().map(|s| s.level2_id.clone()) else {
            return;
        };
        if self.app_settings.startup_site.as_deref() != Some(id.as_str()) {
            self.app_settings.startup_site = Some(id.clone());
            self.app_settings.add_favorite(&id);
            let _ = self.app_settings.save();
        }
    }

    fn selected_site_location(&self) -> Option<(f32, f32)> {
        self.selected_site().and_then(site_location)
    }

    fn radar_location(&self) -> Option<(f32, f32)> {
        self.loaded_volume_location()
            .or_else(|| self.selected_site_location())
    }

    fn center_selected_site(&mut self) {
        if let Some((latitude_deg, longitude_deg)) = self.selected_site_location() {
            self.center_map_on(latitude_deg, longitude_deg);
        }
    }

    fn center_map_on(&mut self, latitude_deg: f32, longitude_deg: f32) {
        if latitude_deg.is_finite() && longitude_deg.is_finite() {
            self.map_center_lat = latitude_deg.clamp(-85.0, 85.0);
            self.map_center_lon = normalize_lon(longitude_deg);
        }
    }

    fn loaded_volume_location(&self) -> Option<(f32, f32)> {
        let site = &self.volume.as_ref()?.site;
        Some((site.latitude_deg?, site.longitude_deg?))
    }

    fn selected_grid_range_km(&self) -> Option<f32> {
        let volume = self.volume.as_ref()?;
        selected_grid_range_km_for(volume, self.selected_cut, &self.selected_product)
    }

    fn current_storm_motion(&self) -> StormMotion {
        StormMotion {
            direction_deg: self.storm_motion_direction_deg.rem_euclid(360.0),
            speed_mps: self.storm_motion_speed_kt.max(0.0) * KNOT_TO_MPS,
        }
    }

    fn dealiased_velocity_readout_grid(
        &mut self,
        volume: &RadarVolume,
        cut_index: usize,
    ) -> Option<Arc<MomentGrid>> {
        let volume_ptr = volume as *const RadarVolume as usize;
        if let Some(cache) = &self.dealiased_readout_cache
            && cache.volume_ptr == volume_ptr
            && cache.cut_index == cut_index
        {
            return Some(Arc::clone(&cache.grid));
        }

        let cut = volume.cuts.get(cut_index)?;
        let source_grid = cut.moments.get(&MomentType::Velocity)?;
        let grid = Arc::new(dealias_velocity_grid(cut, source_grid));
        self.dealiased_readout_cache = Some(DealiasedReadoutCache {
            volume_ptr,
            cut_index,
            grid,
        });
        self.dealiased_readout_cache
            .as_ref()
            .map(|cache| Arc::clone(&cache.grid))
    }

    fn storm_motion_key(&self) -> (i16, i16) {
        (
            (self.storm_motion_direction_deg.rem_euclid(360.0) * 10.0).round() as i16,
            (self.storm_motion_speed_kt.max(0.0) * 10.0).round() as i16,
        )
    }

    fn start_local_volume_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local L2")
            .to_owned();
        self.begin_primary_load_telemetry();
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.pending_site_id = Some(label.clone());
        self.status = format!("Loading {label}");

        thread::spawn(move || {
            let total_start = Instant::now();
            let result = decode_load_path_with_optional_preview(
                path,
                &label,
                total_start,
                LoadTimings::default(),
                &sender,
                should_preview_loads(),
                FrameStatus::Local,
                format!("local {label}"),
            )
            .map(DecodedLoadBatch::single);
            let _ = sender.send(AsyncLoadResult {
                label,
                update: AsyncLoadUpdate::Final(result),
            });
        });
        ctx.request_repaint_after(Duration::from_millis(8));
    }

    fn load_latest_level2_for_selected_site(&mut self, ctx: &egui::Context) {
        let Some(site) = self.selected_site().cloned() else {
            self.status = "No site selected".to_owned();
            return;
        };

        self.start_latest_level2_load(site, ctx);
    }

    fn load_loop_history_for_selected_site(&mut self, ctx: &egui::Context) {
        let Some(site) = self.selected_site().cloned() else {
            self.status = "No site selected".to_owned();
            return;
        };

        self.start_latest_level2_load_with_mode(site, ctx, LatestLoadMode::Loop);
    }

    fn start_latest_level2_load(&mut self, site: RadarSite, ctx: &egui::Context) {
        self.start_latest_level2_load_with_mode(site, ctx, LatestLoadMode::User);
    }

    fn start_latest_level2_load_with_mode(
        &mut self,
        site: RadarSite,
        ctx: &egui::Context,
        mode: LatestLoadMode,
    ) {
        let site_id = site.level2_id.clone();
        if history_contains_other_site(&self.frame_history, &site_id) {
            self.clear_frame_history();
        }
        if mode == LatestLoadMode::User || self.volume.is_none() {
            self.begin_primary_load_telemetry();
        }
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.pending_site_id = Some(site_id.clone());
        self.last_realtime_level2_refresh = Some(Instant::now());
        self.status = match mode {
            LatestLoadMode::AutoRefresh => format!("Refreshing realtime L2 {site_id}"),
            LatestLoadMode::Loop => format!("Loading L2 loop {site_id}"),
            LatestLoadMode::User => format!("Loading latest L2 {site_id}"),
        };
        let current_source_path = (mode == LatestLoadMode::AutoRefresh)
            .then(|| self.source_path.clone())
            .flatten();
        let known_frame_paths =
            if matches!(mode, LatestLoadMode::AutoRefresh | LatestLoadMode::Loop) {
                self.current_history_paths()
            } else {
                BTreeSet::new()
            };
        let current_frame_identity = self
            .volume
            .as_ref()
            .filter(|volume| volume.site.id == site_id)
            .map(|volume| frame_identity_for_volume(volume.as_ref()));
        if should_clear_display_before_latest_load(
            mode,
            self.volume.as_deref(),
            &site_id,
            Utc::now(),
        ) {
            self.clear_displayed_volume_for_pending_load(ctx);
        }

        spawn_latest_level2_load_worker(
            site,
            mode,
            current_source_path,
            known_frame_paths,
            current_frame_identity,
            self.history_frame_limit,
            self.display_live_chunk_updates,
            sender,
        );
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_async_site_catalog(&ctx);
        self.poll_async_load(&ctx);
        self.poll_radar_layer_loads(&ctx);
        self.poll_async_render(&ctx);
        self.poll_radar_layer_renders(&ctx);
        self.poll_async_hazards(&ctx);
        self.maybe_refresh_realtime_level2(&ctx);
        self.maybe_refresh_radar_layers(&ctx);
        self.maybe_refresh_live_hazards(&ctx);
        self.maybe_advance_history_loop(&ctx);
        self.sanitize_selection();
        self.poll_rotation_markers(&ctx);
        self.poll_storm_tracks(&ctx);
        self.poll_placefiles(&ctx);
        self.handle_keyboard_navigation(&ctx);

        egui::Panel::top("top_bar")
            .exact_size(42.0)
            .show_inside(ui, |ui| self.top_bar(ui));

        egui::Panel::right("product_tilt_panel")
            .resizable(true)
            .default_size(SIDEBAR_DEFAULT_WIDTH)
            .size_range(SIDEBAR_MIN_WIDTH..=SIDEBAR_MAX_WIDTH)
            .show_inside(ui, |ui| self.side_panel(ui, &ctx));

        egui::Panel::bottom("status_bar")
            .exact_size(30.0)
            .show_inside(ui, |ui| self.status_bar(ui));

        if self.cross_section_armed || self.cross_section_a_lonlat.is_some() {
            egui::Panel::bottom("cross_section_panel")
                .resizable(true)
                .default_size(240.0)
                .size_range(120.0..=520.0)
                .show_inside(ui, |ui| self.cross_section_panel(ui));
        }

        egui::CentralPanel::default().show_inside(ui, |ui| self.map_canvas(ui));
    }
}

impl ViewerApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.heading("BowEcho");
            ui.separator();
            if fixed_action_button(ui, "Reset View", 90.0).clicked() {
                self.reset_view();
            }
            if fixed_action_button(ui, "Reload", 62.0).clicked() {
                self.load_volume(ui.ctx());
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Controls");
        ui.add_space(6.0);
        self.sidebar_tab_bar(ui);
        ui.separator();

        match self.sidebar_tab {
            SidebarTab::Radar => {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_radar_tab")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.radar_controls_panel(ui, ctx);
                    });
            }
            SidebarTab::Hazards => {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_hazards_tab")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.hazard_panel(ui);
                    });
            }
            SidebarTab::Colors => {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_colors_tab")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.color_table_panel(ui, ctx);
                    });
            }
            SidebarTab::Stats => {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_stats_tab")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.stats_panel(ui);
                    });
            }
        }
    }

    fn sidebar_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            for (tab, label) in SIDEBAR_TABS {
                let selected = self.sidebar_tab == *tab;
                let response = ui
                    .add_sized(
                        egui::vec2(67.0, PANEL_BUTTON_HEIGHT),
                        egui::Button::selectable(selected, *label),
                    )
                    .on_hover_text(sidebar_tab_tooltip(*tab));
                if response.clicked() {
                    self.sidebar_tab = *tab;
                }
            }
        });
    }

    fn radar_controls_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label("Layout");
            for (layout, label, hover) in [
                (PanelLayout::One, "1", "Single pane"),
                (
                    PanelLayout::TwoVertical,
                    "2",
                    "Two panes side by side (synced)",
                ),
                (
                    PanelLayout::FourGrid,
                    "4",
                    "Quad grid — REF / VEL / CC / ZDR (synced)",
                ),
            ] {
                if ui
                    .selectable_label(self.grid_layout == layout, label)
                    .on_hover_text(hover)
                    .clicked()
                    && self.grid_layout != layout
                {
                    self.grid_layout = layout;
                    self.sync_extra_panes();
                    self.app_settings.grid_pane_count = layout.panel_count();
                    let _ = self.app_settings.save();
                    ctx.request_repaint();
                }
            }
        });
        ui.add_space(6.0);
        ui.label("Level 2");
        ui.add_space(8.0);
        ui.label("Site");
        let selected_site_label = self
            .selected_site()
            .map(format_site_label)
            .unwrap_or_else(|| "None".to_owned());
        let mut selected_site_index = self.selected_site_index;
        egui::ComboBox::from_id_salt("site_combo")
            .selected_text(selected_site_label)
            .width(220.0)
            .show_ui(ui, |ui| {
                for (index, site) in self.sites.iter().enumerate() {
                    ui.selectable_value(&mut selected_site_index, index, format_site_label(site));
                }
            });
        if selected_site_index != self.selected_site_index {
            self.selected_site_index = selected_site_index;
        }

        ui.horizontal(|ui| {
            if fixed_action_button(ui, "Load Selected", 100.0).clicked()
                && self.load_receiver.is_none()
            {
                self.load_latest_level2_for_selected_site(ui.ctx());
                self.remember_startup_site();
            }
            if fixed_action_button(ui, "Load Loop", 82.0).clicked() && self.load_receiver.is_none()
            {
                self.load_loop_history_for_selected_site(ui.ctx());
                self.remember_startup_site();
            }
            if fixed_action_button(ui, "Center", 58.0).clicked() {
                self.center_selected_site();
            }
            ui.checkbox(&mut self.realtime_level2_auto_refresh, "Live");
            ui.checkbox(&mut self.display_live_chunk_updates, "Chunks")
                .on_hover_text(
                    "Display incomplete live chunk tilts before a full low-level tilt is available",
                );
        });

        self.radar_layers_panel(ui, ctx);

        ui.add_space(12.0);
        let Some(volume) = &self.volume else {
            ui.label(&self.status);
            return;
        };

        let site = volume.site.id.clone();
        let volume_time = volume
            .volume_time
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        let vcp = volume
            .vcp
            .as_ref()
            .map(|vcp| vcp.pattern.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let cut_count = volume.cuts.len();
        let decoded_radials = volume.metadata.decoded_radial_count;
        // The sidebar edits the FOCUSED pane: the main pane (or 1x1) edits the
        // shared state everyone follows; a focused extra pane edits itself.
        let editing_pane: Option<usize> = (self.grid_layout != PanelLayout::One
            && self.active_pane >= 1
            && self.active_pane - 1 < self.extra_panes.len())
        .then(|| self.active_pane - 1);
        let editing_product = editing_pane
            .map(|slot| self.extra_panes[slot].product.clone())
            .unwrap_or_else(|| self.selected_product.clone());
        let editing_cut = editing_pane
            .and_then(|slot| self.extra_panes[slot].cut)
            .unwrap_or(self.selected_cut);
        let product_buttons = global_displayable_products(volume)
            .into_iter()
            .map(|product| {
                let target_cut = if is_displayable_on_cut(volume, editing_cut, &product) {
                    Some(editing_cut)
                } else {
                    best_cut_for_product(volume, editing_cut, &product)
                };
                (product, target_cut)
            })
            .collect::<Vec<_>>();
        let cut_rows = volume
            .cuts
            .iter()
            .enumerate()
            .map(|(index, cut)| {
                (
                    index,
                    cut.elevation_deg,
                    cut.radials.len(),
                    cut_start_time_utc(volume, index),
                    index == editing_cut,
                    is_displayable_on_cut(volume, index, &editing_product),
                )
            })
            .collect::<Vec<_>>();

        ui.label("Level 2 Volume");
        ui.label(format!("Site {site}"));
        ui.label(format!("Start {volume_time}"));
        if let Some(frame) = self.selected_frame()
            && frame.identity.site_id == site
        {
            ui.label(format!("Status {}", frame.status.label()));
            if let Some(readout) = live_chunk_readout(frame, Utc::now()) {
                ui.label(readout);
            }
        }
        ui.label(format!("VCP {vcp}"));
        ui.label(format!("{cut_count} cuts, {decoded_radials} radials"));

        self.frame_history_panel(ui, ctx);

        ui.add_space(12.0);
        ui.label("Product");
        if let Some(slot) = editing_pane {
            ui.colored_label(
                egui::Color32::from_rgb(120, 168, 220),
                format!(
                    "Editing pane {} — click the main (top-left) pane to edit all",
                    slot + 2
                ),
            );
        }
        ui.horizontal_wrapped(|ui| {
            for (product, target_cut) in &product_buttons {
                let selected = editing_product == *product;
                let response = ui.selectable_label(selected, product.label());
                if response.clicked() {
                    if let Some(slot) = editing_pane {
                        // Keep the old texture anchored while the new product
                        // renders (the key's product change re-requests).
                        self.extra_panes[slot].product = product.clone();
                        self.extra_panes[slot].render_ms = None;
                        ctx.request_repaint();
                    } else {
                        self.selected_product = product.clone();
                        if let Some(cut_index) = target_cut {
                            self.selected_cut = *cut_index;
                        }
                        self.clear_texture();
                        ctx.request_repaint();
                    }
                }
            }
        });
        self.active_product_color_picker(ui, ctx);

        // Display threshold ("hide below"): declutters weak returns at render
        // time; diverging families (VEL, shear) hide |v| < threshold instead.
        {
            let family = editing_product.color_family();
            let family_label = family.label().to_owned();
            let mut enabled = self.display_thresholds.contains_key(&family_label);
            let symmetric = family_threshold_is_symmetric(family);
            ui.horizontal(|ui| {
                if ui
                    .checkbox(
                        &mut enabled,
                        if symmetric { "Hide |val| below" } else { "Hide below" },
                    )
                    .on_hover_text(
                        "Render-time threshold for this product family: weaker returns draw transparent. The data is untouched — the inspector still reads it, and the colorbar shows the cut.",
                    )
                    .changed()
                {
                    if enabled {
                        let default = default_display_threshold(family);
                        self.display_thresholds.insert(family_label.clone(), default);
                    } else {
                        self.display_thresholds.remove(&family_label);
                    }
                    ctx.request_repaint();
                }
                if let Some(threshold) = self.display_thresholds.get_mut(&family_label) {
                    let units = product_units(&editing_product);
                    if ui
                        .add(
                            egui::DragValue::new(threshold)
                                .speed(0.5)
                                .suffix(format!(" {units}")),
                        )
                        .changed()
                    {
                        ctx.request_repaint();
                    }
                }
            });
        }

        if self.selected_product.color_family() == ColorTableFamily::Velocity {
            if matches!(
                self.selected_product,
                DisplayProduct::Moment(MomentType::Velocity)
            ) {
                let changed = ui
                    .checkbox(&mut self.unfold_velocity_display, "Unfold VEL")
                    .on_hover_text(
                        "Use the dealiased continuity grid for base velocity display colors",
                    )
                    .changed();
                if changed {
                    self.clear_texture();
                    ctx.request_repaint();
                }
            }
            let changed = ui
                .checkbox(&mut self.flip_velocity_color_polarity, "Flip VEL colors")
                .on_hover_text("Diagnostic: color positive velocity values with the negative side of the active velocity table, and vice versa")
                .changed();
            if changed {
                self.clear_texture();
                ctx.request_repaint();
            }
        }

        ui.separator();
        if ui
            .checkbox(&mut self.show_rotation_markers, "Rotation markers")
            .on_hover_text(
                "NSSL MDA/TDA-style detection (Stumpf et al. 1998; Mitchell et al. 1998): QC-masked, vertically-continuous circulations only, on a background thread. Pale ring = weak, orange = moderate, double gold = mesocyclone (rank ≥ 5), red triangle = TVS. Zoom in for rank + Vrot.",
            )
            .changed()
        {
            self.rotation_markers_volume_ptr = 0;
            if !self.show_rotation_markers {
                self.rotation_markers.clear();
            }
            ctx.request_repaint();
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .checkbox(&mut self.show_storm_tracks, "Storm tracks")
                .on_hover_text(
                    "SCIT-style cell tracking (Johnson et al. 1998): composite-reflectivity cells identified per volume on a background thread, tracked across volumes with a least-squares motion fit; dots extrapolate +15/+30/+45 min.",
                )
                .changed()
            {
                if !self.show_storm_tracks {
                    self.storm_tracks.clear();
                }
                self.storm_cells_volume_ptr = 0;
                ctx.request_repaint();
            }
            if let Some((direction, speed_kt)) = self.storm_motion_from_tracks()
                && ui
                    .small_button("SRV←tracks")
                    .on_hover_text(format!(
                        "Set storm motion from the mean track motion ({direction:03.0}° / {speed_kt:.0} kt)"
                    ))
                    .clicked()
            {
                self.storm_motion_direction_deg = direction;
                self.storm_motion_speed_kt = speed_kt;
                self.clear_texture();
                ctx.request_repaint();
            }
        });

        ui.separator();
        egui::CollapsingHeader::new("Placefiles")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.placefile_url_input)
                            .hint_text("https://… placefile URL")
                            .desired_width(190.0),
                    );
                    if ui.button("Add").clicked() {
                        let url = self.placefile_url_input.trim().to_owned();
                        if url.starts_with("http")
                            && !self.placefile_slots.iter().any(|slot| slot.url == url)
                        {
                            self.placefile_slots.push(PlacefileSlot::new(url, true));
                            self.placefile_url_input.clear();
                            self.save_placefile_settings();
                            ctx.request_repaint();
                        }
                    }
                });
                let mut remove: Option<usize> = None;
                let mut changed = false;
                for (index, slot) in self.placefile_slots.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut slot.enabled, "").changed() {
                            changed = true;
                        }
                        let title = slot
                            .data
                            .as_ref()
                            .map(|p| p.title.clone())
                            .filter(|t| !t.is_empty())
                            .unwrap_or_else(|| slot.url.clone());
                        ui.label(egui::RichText::new(title).small())
                            .on_hover_text(format!(
                                "{}
{}",
                                slot.url, slot.status
                            ));
                        if ui.small_button("↻").on_hover_text("Refresh now").clicked() {
                            slot.next_refresh = Some(Instant::now());
                        }
                        if ui.small_button("✕").on_hover_text("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                if let Some(index) = remove {
                    self.placefile_slots.remove(index);
                    changed = true;
                }
                if changed {
                    self.save_placefile_settings();
                    ctx.request_repaint();
                }
            });

        ui.separator();
        egui::CollapsingHeader::new("Hotkeys")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak("←/→ product · ↑/↓ tilt (focused pane)");
                let mut bindings: Vec<(&String, &String)> =
                    self.app_settings.product_hotkeys.iter().collect();
                bindings.sort_by(|a, b| {
                    let order = |k: &str| {
                        if k == "0" {
                            10
                        } else {
                            k.parse::<u8>().unwrap_or(99)
                        }
                    };
                    order(a.0).cmp(&order(b.0))
                });
                for (key, label) in bindings {
                    ui.monospace(format!("{key}  →  {label}"));
                }
                if let Some(path) = settings::AppSettings::config_path() {
                    ui.weak(format!("customize in {}", path.display()));
                }
            });

        ui.separator();
        ui.checkbox(&mut self.show_inspector_card, "Inspector card")
            .on_hover_text(
                "Floating data card at the cursor (value, range/azimuth, beam height, Vrot; velocity products add a radial in/outbound arrow). Shift+click the map to pin it to a spot — it tracks pan/zoom and live updates; Shift+click it again to release.",
            );
        ui.horizontal(|ui| {
            let was_armed = self.cross_section_armed;
            ui.checkbox(&mut self.cross_section_armed, "Cross-section")
                .on_hover_text(
                    "Arm, then left-click two points on the map to draw a vertical cross-section below (right-click clears). Uses velocity when a velocity product is selected, else reflectivity.",
                );
            // Disarming with only endpoint A placed drops the dangling point so
            // the rubber band + panel don't linger; a completed A→B section stays.
            if was_armed
                && !self.cross_section_armed
                && self.cross_section_a_lonlat.is_some()
                && self.cross_section_b_lonlat.is_none()
            {
                self.cross_section_a_lonlat = None;
                self.cross_section_signature = None;
            }
            if fixed_action_button(ui, "Clear XS", 64.0).clicked() {
                self.cross_section_a_lonlat = None;
                self.cross_section_b_lonlat = None;
                self.cross_section_texture = None;
                self.cross_section_signature = None;
                self.cross_section_status = "Cross-section: arm, then click endpoint A then B".to_owned();
            }
        });

        if editing_product == DisplayProduct::Derived(DerivedProduct::Mehs) {
            ui.add_space(8.0);
            ui.label("Hail environment (above radar)");
            ui.horizontal(|ui| {
                ui.label("0°C");
                let f_changed = ui
                    .add(
                        egui::DragValue::new(&mut self.hail_freezing_level_km)
                            .range(0.5..=8.0)
                            .speed(0.1)
                            .suffix(" km"),
                    )
                    .on_hover_text(
                        "Melting-level height above the radar (from a sounding). MEHS follows Witt et al. 1998.",
                    )
                    .changed();
                ui.label("−20°C");
                let m_changed = ui
                    .add(
                        egui::DragValue::new(&mut self.hail_minus20_level_km)
                            .range(1.0..=14.0)
                            .speed(0.1)
                            .suffix(" km"),
                    )
                    .changed();
                if f_changed || m_changed {
                    self.hail_minus20_level_km = self
                        .hail_minus20_level_km
                        .max(self.hail_freezing_level_km + 0.1);
                    ctx.request_repaint();
                }
            });
        }

        if self.selected_product.is_storm_relative_velocity() {
            ui.add_space(8.0);
            ui.label("Storm Motion");
            let direction_changed = ui
                .add(
                    egui::DragValue::new(&mut self.storm_motion_direction_deg)
                        .range(0.0..=359.0)
                        .speed(1.0)
                        .suffix(" deg"),
                )
                .changed();
            let speed_changed = ui
                .add(
                    egui::DragValue::new(&mut self.storm_motion_speed_kt)
                        .range(0.0..=120.0)
                        .speed(1.0)
                        .suffix(" kt"),
                )
                .changed();
            if direction_changed || speed_changed {
                self.storm_motion_direction_deg = self.storm_motion_direction_deg.rem_euclid(360.0);
                self.clear_texture();
                ctx.request_repaint();
            }
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label("Tilt");
            if let Some(slot) = editing_pane {
                if self.extra_panes[slot].cut.is_some() {
                    if ui
                        .small_button("Follow main tilt")
                        .on_hover_text("Unpin this pane: it follows the main pane's tilt again")
                        .clicked()
                    {
                        self.extra_panes[slot].cut = None;
                        ctx.request_repaint();
                    }
                } else {
                    ui.weak("following main");
                }
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("tilt_list")
            .auto_shrink([false, false])
            .max_height(TILT_LIST_SCROLL_HEIGHT)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (
                    index,
                    elevation_deg,
                    radial_count,
                    start_time,
                    is_selected,
                    has_selected_product,
                ) in &cut_rows
                {
                    let label = format!(
                        "#{:02}  {:>4.2} deg  {:>4} radials  {}",
                        index,
                        elevation_deg,
                        radial_count,
                        start_time
                            .map(|time| time.format("%H:%M:%S").to_string())
                            .unwrap_or_else(|| "--:--:--".to_owned())
                    );
                    let response = ui
                        .add_enabled_ui(*has_selected_product, |ui| {
                            ui.add_sized(
                                egui::vec2(ui.available_width(), PANEL_BUTTON_HEIGHT),
                                egui::Button::selectable(*is_selected, label),
                            )
                        })
                        .inner;
                    if response.clicked() {
                        if let Some(slot) = editing_pane {
                            // Pin this pane to an independent tilt.
                            self.extra_panes[slot].cut = Some(*index);
                            ctx.request_repaint();
                        } else {
                            self.selected_cut = *index;
                            self.sanitize_selection();
                            self.clear_texture();
                            ctx.request_repaint();
                        }
                    }
                }
            });
    }

    fn frame_history_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label("History");
            let mut selected_limit = self.history_frame_limit;
            egui::ComboBox::from_id_salt("history_frame_limit")
                .selected_text(format!("{} frames", self.history_frame_limit))
                .width(92.0)
                .show_ui(ui, |ui| {
                    for limit in HISTORY_SIZE_OPTIONS {
                        ui.selectable_value(&mut selected_limit, *limit, format!("{limit} frames"));
                    }
                });
            if selected_limit != self.history_frame_limit {
                self.set_history_frame_limit(selected_limit, ctx);
            }
        });

        if self.frame_history.is_empty() {
            ui.label("No history frames loaded");
            return;
        }

        let selected_status_text = self.selected_frame_status_text();
        fixed_height_scroll(
            ui,
            "history_selected_frame_status",
            HISTORY_STATUS_SCROLL_HEIGHT,
            |ui| {
                wrapped_label(ui, &selected_status_text);
            },
        );

        let frame_count = self.frame_history.len();
        let mut next_frame_index = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(frame_count > 1, |ui| fixed_action_button(ui, "<", 28.0))
                .inner
                .on_hover_text("Previous frame")
                .clicked()
            {
                next_frame_index =
                    Some((self.selected_frame_index + frame_count - 1) % frame_count);
            }
            let play_label = if self.history_playing {
                "Pause"
            } else {
                "Play"
            };
            if ui
                .add_enabled_ui(frame_count > 1, |ui| {
                    fixed_action_button(ui, play_label, 54.0)
                })
                .inner
                .on_hover_text("Loop loaded history frames")
                .clicked()
            {
                self.history_playing = !self.history_playing;
                self.last_history_step = Some(Instant::now());
                ctx.request_repaint_after(Duration::from_millis(HISTORY_LOOP_FRAME_MS));
            }
            if ui
                .add_enabled_ui(frame_count > 1, |ui| fixed_action_button(ui, ">", 28.0))
                .inner
                .on_hover_text("Next frame")
                .clicked()
            {
                next_frame_index = Some((self.selected_frame_index + 1) % frame_count);
            }
        });

        let mut slider_index = self.selected_frame_index.min(frame_count - 1);
        let slider_response = ui
            .add_enabled_ui(frame_count > 1, |ui| {
                ui.add_sized(
                    egui::vec2(ui.available_width(), PANEL_BUTTON_HEIGHT),
                    egui::Slider::new(&mut slider_index, 0..=frame_count - 1).show_value(false),
                )
            })
            .inner
            .on_hover_text("Scrub decoded frame history")
            .changed();
        if slider_response {
            next_frame_index = Some(slider_index);
        }

        fixed_height_scroll(
            ui,
            "history_frame_buttons",
            HISTORY_FRAME_BUTTONS_SCROLL_HEIGHT,
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (index, frame) in self.frame_history.iter().enumerate() {
                        let label = compact_frame_label(frame, Utc::now());
                        let selected = index == self.selected_frame_index;
                        if ui
                            .add_sized(
                                egui::vec2(72.0, PANEL_BUTTON_HEIGHT),
                                egui::Button::selectable(selected, label),
                            )
                            .on_hover_text(frame_status_text(frame, Utc::now()))
                            .clicked()
                        {
                            next_frame_index = Some(index);
                        }
                    }
                });
            },
        );

        if let Some(index) = next_frame_index {
            self.history_playing = false;
            self.select_history_frame(index, false, ctx);
        }
    }

    fn active_product_color_picker(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let family = self.selected_product.color_family();
        let current_table = self.color_tables.for_family(family);
        let current_name = current_table.name().to_owned();
        let current_summary = color_table_summary(current_table);
        ui.add_space(6.0);
        ui.label("Color");
        egui::ComboBox::from_id_salt("active_product_color_preset")
            .selected_text(&current_name)
            .width(220.0)
            .show_ui(ui, |ui| {
                for table in builtin_tables_for_family(family) {
                    let table_name = table.name().to_owned();
                    if ui
                        .selectable_label(table_name == current_name, &table_name)
                        .clicked()
                    {
                        let summary = color_table_summary(&table);
                        self.color_table_target = family;
                        self.color_tables.set_family(family, table);
                        self.clear_texture();
                        self.color_table_status =
                            format!("Loaded {table_name} into {} ({summary})", family.label());
                        ctx.request_repaint();
                    }
                }
            });
        ui.label(current_summary);
    }

    fn radar_layers_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(format!("Overlays {}", self.radar_layers.len()));
            if ui
                .add_enabled_ui(!self.radar_layers.is_empty(), |ui| {
                    fixed_action_button(ui, "Clear", 52.0)
                })
                .inner
                .clicked()
            {
                self.radar_layers.clear();
                self.status = "Cleared radar overlays".to_owned();
                ctx.request_repaint();
            }
        });

        if self.radar_layers.is_empty() {
            ui.label("No overlays");
            return;
        }

        let mut remove_index = None;
        let mut center_site = None;
        let mut refresh_index = None;
        let mut promote_site = None;
        for (index, layer) in self.radar_layers.iter_mut().enumerate() {
            let state = if layer.volume.is_some() {
                "live"
            } else if layer.load_receiver.is_some() {
                "loading"
            } else {
                "queued"
            };
            let mut details = vec![layer.status.clone()];
            if let Some(path) = &layer.source_path {
                details.push(path.display().to_string());
            }
            if let Some(render_ms) = layer.render_ms {
                let texture_ms = layer.texture_ms.unwrap_or(0.0);
                details.push(format!(
                    "render {render_ms:.1} ms texture {texture_ms:.1} ms"
                ));
            }

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if ui.checkbox(&mut layer.visible, "").changed() {
                    ctx.request_repaint();
                }
                fixed_status_label(ui, &layer.site.level2_id, 42.0)
                    .on_hover_text(details.join("\n"));
                fixed_state_dot(ui, layer_state_color(state), state);
                if ui
                    .add_sized(
                        egui::vec2(48.0, PANEL_BUTTON_HEIGHT),
                        egui::Slider::new(&mut layer.opacity, MIN_RADAR_OVERLAY_ALPHA..=u8::MAX)
                            .show_value(false),
                    )
                    .on_hover_text(format!("Opacity {}", layer.opacity))
                    .changed()
                {
                    ctx.request_repaint();
                }
                if fixed_action_button(ui, "Go", 28.0)
                    .on_hover_text("Center map on this overlay radar")
                    .clicked()
                {
                    center_site = Some(layer.site.clone());
                }
                if fixed_action_button(ui, "Ref", 32.0)
                    .on_hover_text("Refresh this overlay radar")
                    .clicked()
                    && layer.load_receiver.is_none()
                {
                    refresh_index = Some(index);
                }
                if fixed_action_button(ui, "Pri", 30.0)
                    .on_hover_text("Make this radar the primary radar")
                    .clicked()
                {
                    promote_site = Some(layer.site.clone());
                }
                if fixed_action_button(ui, "x", 20.0)
                    .on_hover_text(details.join("\n"))
                    .clicked()
                {
                    remove_index = Some(index);
                }
            });
        }

        if let Some(index) = refresh_index
            && let Some(layer) = self.radar_layers.get_mut(index)
        {
            Self::start_radar_layer_load(layer, LatestLoadMode::User, ctx);
        }
        if let Some(site) = center_site
            && let Some((latitude_deg, longitude_deg)) = site_location(&site)
        {
            self.center_map_on(latitude_deg, longitude_deg);
        }
        if let Some(site) = promote_site {
            if let Some(index) = self
                .sites
                .iter()
                .position(|candidate| candidate.level2_id == site.level2_id)
            {
                self.selected_site_index = index;
            }
            self.start_latest_level2_load(site, ctx);
        }
        if let Some(index) = remove_index {
            self.radar_layers.remove(index);
            ctx.request_repaint();
        }
    }

    fn color_table_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.label("Colors");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("color_table_target")
                .selected_text(self.color_table_target.label())
                .width(150.0)
                .show_ui(ui, |ui| {
                    for family in [
                        ColorTableFamily::Velocity,
                        ColorTableFamily::Reflectivity,
                        ColorTableFamily::SpectrumWidth,
                        ColorTableFamily::CorrelationCoefficient,
                        ColorTableFamily::DifferentialReflectivity,
                        ColorTableFamily::Generic,
                    ] {
                        ui.selectable_value(&mut self.color_table_target, family, family.label());
                    }
                });
            if fixed_action_button(ui, "Current", 64.0).clicked() {
                self.color_table_target = self.selected_product.color_family();
            }
        });

        let table = self.color_tables.for_family(self.color_table_target);
        ui.label(format!(
            "{}: {}",
            self.color_table_target.label(),
            table.name()
        ));
        ui.label(color_table_summary(table));
        egui::ComboBox::from_id_salt("color_table_builtin_preset")
            .selected_text("Built-ins")
            .width(220.0)
            .show_ui(ui, |ui| {
                for table in builtin_tables_for_family(self.color_table_target) {
                    if ui.selectable_label(false, table.name()).clicked() {
                        let table_name = table.name().to_owned();
                        let summary = color_table_summary(&table);
                        self.color_tables.set_family(self.color_table_target, table);
                        self.clear_texture();
                        self.color_table_status = format!(
                            "Loaded {table_name} into {} ({summary})",
                            self.color_table_target.label()
                        );
                        ctx.request_repaint();
                    }
                }
            });
        ui.add(
            egui::TextEdit::singleline(&mut self.color_table_path_text)
                .desired_width(220.0)
                .hint_text("Color table path"),
        );
        ui.horizontal(|ui| {
            let has_path = !self.color_table_path_text.trim().is_empty();
            if fixed_disabled_action_button(ui, has_path, "Load Table", 84.0).clicked() {
                self.load_color_table_path(ctx);
            }
            if fixed_action_button(ui, "Reset Slot", 84.0).clicked() {
                self.reset_color_table_slot(ctx);
            }
        });
        fixed_height_scroll(ui, "color_table_status", COLOR_STATUS_SCROLL_HEIGHT, |ui| {
            wrapped_label(ui, &self.color_table_status);
        });
    }

    fn load_color_table_path(&mut self, ctx: &egui::Context) {
        let path_text = self.color_table_path_text.trim().trim_matches('"');
        if path_text.is_empty() {
            self.color_table_status = "Choose a color table path".to_owned();
            return;
        }
        let path = PathBuf::from(path_text);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                self.color_table_status = format!("Color table read failed: {err}");
                return;
            }
        };
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("Custom color table");
        let table = match parse_color_table_for_family(self.color_table_target, name, &text) {
            Ok(table) => table,
            Err(err) => {
                self.color_table_status = format!("Color table parse failed: {err}");
                return;
            }
        };
        let table_name = table.name().to_owned();
        let summary = color_table_summary(&table);
        self.color_tables.set_family(self.color_table_target, table);
        self.clear_texture();
        self.color_table_status = format!(
            "Loaded {table_name} into {} ({summary})",
            self.color_table_target.label()
        );
        ctx.request_repaint();
    }

    fn reset_color_table_slot(&mut self, ctx: &egui::Context) {
        let defaults = ColorTableSet::default();
        let table = defaults.for_family(self.color_table_target).clone();
        let table_name = table.name().to_owned();
        let summary = color_table_summary(&table);
        self.color_tables.set_family(self.color_table_target, table);
        self.clear_texture();
        self.color_table_status = format!(
            "Reset {} to {table_name} ({summary})",
            self.color_table_target.label()
        );
        ctx.request_repaint();
    }

    fn hazard_panel(&mut self, ui: &mut egui::Ui) {
        ui.label("Hazards");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.hazards_visible, "Show");
            ui.checkbox(&mut self.hazards_active_only, "Active");
            ui.checkbox(&mut self.live_hazard_auto_refresh, "Auto");
        });
        ui.horizontal_wrapped(|ui| {
            for (family, label) in HAZARD_FILTER_FAMILIES {
                let mut visible = !self.hidden_hazard_families.contains(*family);
                if ui.checkbox(&mut visible, *label).changed() {
                    if visible {
                        self.hidden_hazard_families.remove(*family);
                    } else {
                        self.hidden_hazard_families.insert((*family).to_owned());
                    }
                    if self
                        .selected_hazard_record()
                        .is_some_and(|record| !self.hazard_record_visible(record))
                    {
                        self.selected_hazard_index = None;
                    }
                    ui.ctx().request_repaint();
                }
            }
        });
        let mut fill_alpha = self.hazard_fill_alpha as f32;
        if ui
            .add(egui::Slider::new(&mut fill_alpha, 0.0..=80.0).text("Fill"))
            .changed()
        {
            self.hazard_fill_alpha = fill_alpha.round() as u8;
            ui.ctx().request_repaint();
        }
        ui.horizontal(|ui| {
            let loading = self.hazard_receiver.is_some();
            if fixed_action_button(ui, "Refresh Live", 96.0).clicked() && !loading {
                self.load_live_hazards(ui.ctx());
            }
            if fixed_action_button(ui, "Clear", 52.0).clicked() {
                self.hazard_overlay_generation = self.hazard_overlay_generation.wrapping_add(1);
                self.hazard_overlay = None;
                self.selected_hazard_index = None;
                self.hazard_status = "No hazard polygons loaded".to_owned();
            }
        });

        if let Some(record) = self.selected_hazard_record() {
            ui.add_space(6.0);
            let detail_lines = hazard_record_detail_lines(record);
            fixed_height_scroll(
                ui,
                "hazard_detail_text",
                HAZARD_DETAIL_SCROLL_HEIGHT,
                |ui| {
                    for line in &detail_lines {
                        wrapped_label(ui, line);
                    }
                },
            );
        }

        let summary_lines = self.hazard_summary_lines();
        fixed_height_scroll(
            ui,
            "hazard_summary_text",
            HAZARD_SUMMARY_SCROLL_HEIGHT,
            |ui| {
                for line in &summary_lines {
                    wrapped_label(ui, line);
                }
            },
        );

        egui::CollapsingHeader::new("Local file")
            .id_salt("hazard_local_path_loader")
            .default_open(false)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.hazard_path_text)
                        .desired_width(220.0)
                        .hint_text("Path"),
                );
                let loading = self.hazard_receiver.is_some();
                if fixed_action_button(ui, "Load Path", 82.0).clicked() && !loading {
                    self.load_local_hazards(ui.ctx());
                }
            });
    }

    fn stats_panel(&mut self, ui: &mut egui::Ui) {
        ui.label("Performance");
        ui.checkbox(&mut self.show_performance_stats, "Details");
        if let Some(render_ms) = self.render_ms {
            ui.label(format!("Render {render_ms:.1} ms"));
        }
        if let Some(worker_ms) = self.worker_ms {
            ui.label(format!("Worker {worker_ms:.1} ms"));
        }
        if let Some(texture_ms) = self.texture_ms {
            ui.label(format!("Texture {texture_ms:.1} ms"));
        }
        if let Some(load_timing) = &self.load_timing {
            ui.label(format!("Decode {:.1} ms", load_timing.decode_ms));
            ui.label(format!("Load {:.1} ms", load_timing.total_ms));
        }
        ui.label(format!("{} overlays", self.radar_layers.len()));
        ui.label(format!("{:.0} km range", self.radar_range_km));
        if self.show_performance_stats {
            ui.separator();
            self.timing_readout(ui);
        }
    }

    fn hazard_summary_lines(&self) -> Vec<String> {
        let mut lines = vec![self.hazard_status.clone()];
        if let Some(overlay) = &self.hazard_overlay {
            lines.push(format!(
                "{} scanned, {} parsed, {} polygons",
                overlay.scanned_items, overlay.parsed_items, overlay.polygon_records
            ));
            lines.push(overlay.source_label.clone());
            if overlay.error_count > 0 {
                let issue_label = if overlay.error_count == 1 {
                    "source issue"
                } else {
                    "source issues"
                };
                lines.push(format!("{} {issue_label}", overlay.error_count));
            }
            if let Some(query_time_utc) = &overlay.query_time_utc {
                lines.push(format!("At {query_time_utc}"));
            }
        }
        lines
    }

    fn selected_hazard_record(&self) -> Option<&HazardRecord> {
        let overlay = self.hazard_overlay.as_ref()?;
        let index = self.selected_hazard_index?;
        overlay.records.get(index)
    }

    fn hazard_record_visible(&self, record: &HazardRecord) -> bool {
        !self.hidden_hazard_families.contains(&record.event_family)
            && (!self.hazards_active_only || hazard_record_is_active_or_pending(record))
    }

    fn hazard_at_position(&self, rect: egui::Rect, position: egui::Pos2) -> Option<usize> {
        if !self.hazards_visible {
            return None;
        }
        let overlay = self.hazard_overlay.as_ref()?;
        let (lon, lat) = self.screen_to_lon_lat(rect, position);
        let point = HazardPoint { lon, lat };
        let mut best_containing = None::<(usize, f32, u8)>;
        let mut best_near = None::<(usize, f32, f32, u8)>;
        let mut best_label = None::<(usize, f32, f32, u8)>;
        for (index, record) in overlay.records.iter().enumerate() {
            if !self.hazard_record_visible(record) || !hazard_points_renderable(&record.points) {
                continue;
            }
            let screen_area = self.hazard_screen_area(rect, &record.points);
            let family_order = hazard_family_order(&record.event_family);
            if bbox_contains(record.bbox, point.lon, point.lat)
                && hazard_polygon_contains_point(&record.points, point)
            {
                let candidate = (index, screen_area, family_order);
                if best_containing.is_none_or(|best| {
                    candidate
                        .1
                        .total_cmp(&best.1)
                        .then_with(|| candidate.2.cmp(&best.2))
                        .is_lt()
                }) {
                    best_containing = Some(candidate);
                }
                continue;
            }

            let edge_distance = self.hazard_screen_edge_distance(rect, &record.points, position);
            if edge_distance <= HAZARD_CLICK_TOLERANCE_PX {
                let candidate = (index, edge_distance, screen_area, family_order);
                if best_near.is_none_or(|best| {
                    candidate
                        .1
                        .total_cmp(&best.1)
                        .then_with(|| candidate.2.total_cmp(&best.2))
                        .then_with(|| candidate.3.cmp(&best.3))
                        .is_lt()
                }) {
                    best_near = Some(candidate);
                }
            }

            if self.map_scale >= 62.0 {
                let label_center = self.hazard_screen_centroid(rect, &record.points);
                let label_distance = label_center.distance(position);
                if label_distance <= HAZARD_LABEL_CLICK_RADIUS_PX {
                    let candidate = (index, label_distance, screen_area, family_order);
                    if best_label.is_none_or(|best| {
                        candidate
                            .1
                            .total_cmp(&best.1)
                            .then_with(|| candidate.2.total_cmp(&best.2))
                            .then_with(|| candidate.3.cmp(&best.3))
                            .is_lt()
                    }) {
                        best_label = Some(candidate);
                    }
                }
            }
        }
        best_containing
            .map(|(index, _, _)| index)
            .or_else(|| best_near.map(|(index, _, _, _)| index))
            .or_else(|| best_label.map(|(index, _, _, _)| index))
    }

    fn hazard_screen_area(&self, rect: egui::Rect, points: &[HazardPoint]) -> f32 {
        if points.len() < 3 {
            return 0.0;
        }
        let mut area = 0.0f32;
        let mut previous = self.lon_lat_to_screen(
            rect,
            points[points.len() - 1].lon,
            points[points.len() - 1].lat,
        );
        for point in points {
            let current = self.lon_lat_to_screen(rect, point.lon, point.lat);
            area += previous.x * current.y - current.x * previous.y;
            previous = current;
        }
        area.abs() * 0.5
    }

    fn hazard_screen_centroid(&self, rect: egui::Rect, points: &[HazardPoint]) -> egui::Pos2 {
        let screen_points = points
            .iter()
            .map(|point| self.lon_lat_to_screen(rect, point.lon, point.lat))
            .collect::<Vec<_>>();
        polygon_screen_centroid(&screen_points)
    }

    fn hazard_screen_edge_distance(
        &self,
        rect: egui::Rect,
        points: &[HazardPoint],
        position: egui::Pos2,
    ) -> f32 {
        if points.len() < 2 {
            return f32::INFINITY;
        }
        let mut previous = self.lon_lat_to_screen(
            rect,
            points[points.len() - 1].lon,
            points[points.len() - 1].lat,
        );
        let mut best_distance_sq = f32::INFINITY;
        for point in points {
            let current = self.lon_lat_to_screen(rect, point.lon, point.lat);
            best_distance_sq =
                best_distance_sq.min(point_segment_distance_sq(position, previous, current));
            previous = current;
        }
        best_distance_sq.sqrt()
    }

    fn timing_readout(&self, ui: &mut egui::Ui) {
        if let Some(timing) = self.load_timing {
            ui.label(format!("Decode {:.1} ms", timing.decode_ms));
            ui.label(format!("Load {:.1} ms", timing.total_ms));
            if let Some(lookup_ms) = timing.lookup_ms {
                let source = if timing.lookup_cache_hit == Some(true) {
                    "cache"
                } else {
                    "net"
                };
                ui.label(format!("Lookup {:.1} ms {source}", lookup_ms));
            }
            if let Some(fetch_ms) = timing.fetch_ms {
                let source = if timing.fetch_cache_hit == Some(true) {
                    "cache"
                } else {
                    "net"
                };
                ui.label(format!("Fetch {:.1} ms {source}", fetch_ms));
            }
            if let Some(read_ms) = timing.read_ms {
                ui.label(format!("Read {:.1} ms", read_ms));
            }
            if let Some(preview_ms) = timing.preview_ms {
                ui.label(format!("Preview {:.1} ms", preview_ms));
            }
            if timing.realtime_volume_id.is_some() {
                ui.add_space(4.0);
                self.live_latency_readout(ui, timing);
            }
        }
        if let Some(reason) = &self.live_refresh_skip_reason {
            ui.label(format!("Live skip {reason}"));
        }
        if let Some(first_data_ms) = self.first_data_ms {
            ui.label(format!("First data {:.1} ms", first_data_ms));
        }
        if let Some(first_texture_ms) = self.first_texture_ms {
            ui.label(format!("First visible {:.1} ms", first_texture_ms));
        }
        if let Some(render_ms) = self.render_ms {
            ui.label(format!("Render {:.1} ms", render_ms));
        }
        if let Some(worker_ms) = self.worker_ms {
            ui.label(format!("Worker {:.1} ms", worker_ms));
        }
        if let Some(texture_ms) = self.texture_ms {
            ui.label(format!("Texture {:.1} ms", texture_ms));
        }
        if let Some(sample_cache_build_ms) = self.sample_cache_build_ms {
            ui.label(format!("Cache {:.1} ms", sample_cache_build_ms));
        }
        if let Some(basemap_ms) = self.basemap_ms {
            ui.label(format!("Map {:.1} ms", basemap_ms));
        }

        ui.add_space(6.0);
        self.perf_metric_readout(ui, "Decode", &self.perf.decode);
        self.perf_metric_readout(ui, "Direct", &self.perf.direct_render);
        self.perf_metric_readout(ui, "Cached", &self.perf.cached_render);
        self.perf_metric_readout(ui, "Worker", &self.perf.worker);
        self.perf_metric_readout(ui, "Texture", &self.perf.texture);
        self.perf_metric_readout(ui, "Cache build", &self.perf.cache_build);
    }

    fn live_latency_readout(&self, ui: &mut egui::Ui, timing: LoadTimings) {
        ui.label("Live Latency Debug");
        if let Some(site) = self.selected_site() {
            ui.label(format!(
                "Selected {} poll {}s",
                site.level2_id, PRIMARY_REALTIME_LEVEL2_REFRESH_SECONDS
            ));
        }
        if let Some(start) = timing.realtime_poll_start_utc {
            ui.label(format!("Poll start {}", format_utc_seconds(start)));
        }
        if let Some(end) = timing.realtime_poll_end_utc {
            ui.label(format!("Poll end {}", format_utc_seconds(end)));
        }
        let mut volume_line = String::new();
        if let Some(volume_id) = timing.realtime_volume_id {
            volume_line.push_str(&format!("Volume id {volume_id}"));
        }
        if let Some(chunk_count) = timing.realtime_chunk_count {
            volume_line.push_str(&format!(" chunks {chunk_count}"));
        }
        if let Some(complete) = timing.realtime_complete {
            volume_line.push_str(if complete { " complete" } else { " partial" });
        }
        if !volume_line.is_empty() {
            ui.label(volume_line);
        }
        if let Some(chunk_id) = timing.realtime_last_chunk_id {
            let chunk_type = timing
                .realtime_last_chunk_type
                .map(RealtimeChunkType::label)
                .unwrap_or("unknown");
            ui.label(format!("Last chunk {chunk_id} {chunk_type}"));
        }
        if let Some(total_size) = timing.realtime_total_size {
            ui.label(format!("Listed {}", compact_byte_label(total_size)));
        }
        if let Some(assembled_size) = timing.realtime_assembled_size {
            ui.label(format!("Assembled {}", compact_byte_label(assembled_size)));
        }
        let now_utc = Utc::now();
        if let Some(last_modified) = timing.realtime_last_modified_utc {
            ui.label(format!(
                "LastModified {} age {}",
                format_utc_seconds(last_modified),
                frame_age_label(last_modified, now_utc)
            ));
        }
        if let Some(volume_time) = timing.realtime_volume_time_utc {
            ui.label(format!(
                "Volume time {} age {}",
                format_utc_seconds(volume_time),
                frame_age_label(volume_time, now_utc)
            ));
        }
    }

    fn perf_metric_readout(&self, ui: &mut egui::Ui, label: &str, series: &MetricSeries) {
        if let Some(summary) = series.summary() {
            ui.label(format!(
                "{} {:.1} p50 {:.1} p95 {:.1} max {:.1} n{}",
                label, summary.latest, summary.p50, summary.p95, summary.max, summary.count
            ));
        }
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // The loader status ("Rendering", "Refreshing", download progress)
            // changes width constantly — give it a FIXED slot so it never
            // shoves the rest of the bar around.
            let height = ui.available_height();
            ui.add_sized([230.0, height], egui::Label::new(&self.status).truncate());
            ui.separator();
            // Stable metrics anchor to the right edge; the variable-width
            // hover readout absorbs the leftover middle and truncates.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(self.selected_frame_status_text());
                ui.separator();
                if !self.radar_layers.is_empty() {
                    ui.label(format!("{} overlays", self.radar_layers.len()));
                    ui.separator();
                }
                ui.label(format!("{:.0} km range", self.radar_range_km));
                ui.separator();
                ui.label(format!("map {:.0} px/deg", self.map_scale));
                ui.separator();
                let readout = if let Some(readout) = &self.cursor_readout {
                    format_cursor_readout(readout)
                } else {
                    format!(
                        "{} cut {}",
                        self.selected_product.label(),
                        self.selected_cut
                    )
                };
                ui.add(egui::Label::new(readout).truncate());
            });
        });
    }

    fn map_canvas(&mut self, ui: &mut egui::Ui) {
        let available = ui.available_size();
        // In grid mode the per-cell ui.interact widgets are the ONLY
        // interactive surface — the outer allocation must not sense clicks or
        // drags, or it contests the cells in egui's hit test.
        let sense = if self.grid_layout == PanelLayout::One {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(available, sense);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(7, 10, 14));

        // Hard guard: the 1×1 layout runs the original single-pane path
        // verbatim (byte-identical behavior); grids take the per-cell path.
        if self.grid_layout == PanelLayout::One {
            self.single_pane_canvas(ui, rect, &response, &painter);
        } else {
            self.grid_canvas(ui, rect);
        }
    }

    fn single_pane_canvas(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        response: &egui::Response,
        painter: &egui::Painter,
    ) {
        // When cross-section draw mode is armed, clicks place section endpoints
        // instead of panning / selecting (zoom stays live; endpoints are lon/lat
        // so they don't drift). All four mutating interactions are gated.
        let armed = self.cross_section_armed;

        if !armed && response.dragged() {
            let delta = response.drag_delta();
            if delta.length_sq() >= MAP_DRAG_DEAD_ZONE_PX * MAP_DRAG_DEAD_ZONE_PX {
                self.map_center_lon -= delta.x / self.lon_pixels_per_degree();
                self.map_center_lat += delta.y / self.map_scale;
                self.clamp_map_center();
            }
        }

        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let pointer = ui.input(|input| input.pointer.hover_pos());
                let before = pointer.map(|position| self.screen_to_lon_lat(rect, position));
                let factor = (1.0_f32 + scroll / 600.0).clamp(0.75, 1.35);
                self.map_scale = (self.map_scale * factor).clamp(MIN_MAP_SCALE, MAX_MAP_SCALE);
                if let (Some(position), Some((lon_before, lat_before))) = (pointer, before) {
                    let (lon_after, lat_after) = self.screen_to_lon_lat(rect, position);
                    self.map_center_lon += lon_before - lon_after;
                    self.map_center_lat += lat_before - lat_after;
                }
                self.clamp_map_center();
            }
        }
        let cursor_readout = response
            .hovered()
            .then(|| ui.input(|input| input.pointer.hover_pos()))
            .flatten()
            .and_then(|position| self.cursor_readout_at(rect, position));
        self.cursor_readout = cursor_readout;

        let basemap_start = Instant::now();
        self.draw_basemap(painter, rect);
        self.draw_graticule(painter, rect);
        let underlay_ms = basemap_start.elapsed().as_secs_f32() * 1000.0;
        self.request_radar_layer_renders(ui.ctx(), rect);
        self.request_texture_render(ui.ctx(), rect);
        self.draw_radar_overlay_layers(ui.ctx(), painter, rect);
        self.draw_radar_layer(ui.ctx(), painter, rect);
        let overlay_start = Instant::now();
        self.draw_basemap_overlay(painter, rect);
        self.draw_hazard_overlays(painter, rect, &self.selected_product.clone());
        self.draw_rotation_markers(painter, rect);
        self.draw_storm_tracks(painter, rect);
        self.draw_placefiles(painter, rect);
        self.basemap_ms = Some(underlay_ms + overlay_start.elapsed().as_secs_f32() * 1000.0);

        let site_points = self
            .sites
            .iter()
            .enumerate()
            .filter_map(|(index, site)| {
                let (latitude_deg, longitude_deg) = site_location(site)?;
                let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
                rect.expand(18.0)
                    .contains(position)
                    .then_some((index, position))
            })
            .collect::<Vec<_>>();

        let shift_held = ui.input(|input| input.modifiers.shift);
        if !armed
            && shift_held
            && response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            // Shift+click pins the inspector card to this geo point.
            self.toggle_inspector_pin(rect, pointer);
        }

        if !armed
            && !shift_held
            && response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some((index, _)) = site_points
                .iter()
                .filter_map(|(index, position)| {
                    let distance = position.distance(pointer);
                    (distance <= 12.0).then_some((*index, distance))
                })
                .min_by(|left, right| left.1.total_cmp(&right.1))
        {
            self.selected_site_index = index;
        }

        if !armed
            && response.secondary_clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(index) = self.nearest_site_to_position(rect, pointer)
            && let Some(site) = self.sites.get(index).cloned()
        {
            self.selected_site_index = index;
            if ui.input(|input| input.modifiers.ctrl) {
                self.add_or_refresh_radar_layer(site, ui.ctx());
            } else {
                self.start_latest_level2_load(site, ui.ctx());
            }
        }

        if !armed
            && !shift_held
            && response.clicked()
            && let Some(pointer) = response.interact_pointer_pos()
            && let Some(index) = self.hazard_at_position(rect, pointer)
        {
            self.selected_hazard_index = Some(index);
        }

        // Armed: left-click places endpoint A then B (restart after both set);
        // right-click clears.
        if armed {
            if response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let (lon, lat) = self.screen_to_lon_lat(rect, pointer);
                match (self.cross_section_a_lonlat, self.cross_section_b_lonlat) {
                    (Some(_), None) => {
                        self.cross_section_b_lonlat = Some((lon, lat));
                        self.cross_section_status =
                            "Section set — drag the map or re-click to redraw".to_owned();
                    }
                    _ => {
                        self.cross_section_a_lonlat = Some((lon, lat));
                        self.cross_section_b_lonlat = None;
                        self.cross_section_status = "Click endpoint B".to_owned();
                    }
                }
                self.cross_section_signature = None;
            }
            if response.secondary_clicked() {
                self.cross_section_a_lonlat = None;
                self.cross_section_b_lonlat = None;
                self.cross_section_texture = None;
                self.cross_section_signature = None;
                self.cross_section_status = "Cross-section: click endpoint A then B".to_owned();
            }
        }

        self.cross_section_handle_interactions(ui, rect, 0);

        self.draw_site_markers(painter, &site_points);
        self.draw_radar_layer_markers(painter, rect);
        self.draw_loaded_volume_marker(painter, rect);
        self.draw_colorbar(painter, rect);
        self.draw_mode_chip(painter, rect);
        self.draw_cross_section_line(painter, rect, response.hover_pos());
        self.draw_cursor_inspector(painter, rect, response.hover_pos());

        if self.texture.is_none()
            && self
                .radar_layers
                .iter()
                .all(|layer| layer.texture.is_none())
        {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                &self.status,
                egui::FontId::proportional(18.0),
                egui::Color32::from_rgb(210, 218, 230),
            );
        }
    }

    /// The multi-pane grid canvas: every cell is a synchronized view of the
    /// same volume/location/tilt with its own product. Pan/zoom in any cell
    /// moves all cells (shared geo transform); cell 0 is the primary view.
    fn grid_canvas(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        self.sync_extra_panes();
        let cells = pane_cell_rects(self.grid_layout, rect, 2.0);
        let armed = self.cross_section_armed;
        let ctx = ui.ctx().clone();
        let frame_start = Instant::now();
        let mut hovered_readout = None;
        let mut hovers: Vec<Option<egui::Pos2>> = Vec::with_capacity(cells.len());

        // Interaction pass: settle pan/zoom/clicks for EVERY cell first, so
        // the paint pass below renders all panes with the same final
        // transform (no one-frame shear between panes during a drag).
        for (cell_index, cell) in cells.iter().copied().enumerate() {
            let response = ui.interact(
                cell,
                ui.id().with(("grid-pane", cell_index)),
                egui::Sense::click_and_drag(),
            );

            // Focus: the last-clicked pane is what the sidebar edits (the
            // main pane edits the whole bunch; extra panes edit themselves).
            if response.clicked() || response.secondary_clicked() || response.drag_started() {
                self.active_pane = cell_index;
            }

            // Shared pan: dragging any cell moves every pane in sync.
            if !armed && response.dragged() {
                let delta = response.drag_delta();
                if delta.length_sq() >= MAP_DRAG_DEAD_ZONE_PX * MAP_DRAG_DEAD_ZONE_PX {
                    self.map_center_lon -= delta.x / self.lon_pixels_per_degree();
                    self.map_center_lat += delta.y / self.map_scale;
                    self.clamp_map_center();
                }
            }
            // Shared zoom, anchored at the cursor inside this cell.
            if response.hovered() {
                let scroll = ui.input(|input| input.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    let pointer = ui.input(|input| input.pointer.hover_pos());
                    let before = pointer.map(|position| self.screen_to_lon_lat(cell, position));
                    let factor = (1.0_f32 + scroll / 600.0).clamp(0.75, 1.35);
                    self.map_scale = (self.map_scale * factor).clamp(MIN_MAP_SCALE, MAX_MAP_SCALE);
                    if let (Some(position), Some((lon_before, lat_before))) = (pointer, before) {
                        let (lon_after, lat_after) = self.screen_to_lon_lat(cell, position);
                        self.map_center_lon += lon_before - lon_after;
                        self.map_center_lat += lat_before - lat_after;
                    }
                    self.clamp_map_center();
                }
                // The hover readout samples the PRIMARY product's data, so it
                // is only honest over the primary pane.
                if cell_index == 0 {
                    hovered_readout = ui
                        .input(|input| input.pointer.hover_pos())
                        .and_then(|position| self.cursor_readout_at(cell, position));
                }
            }

            let site_points = self
                .sites
                .iter()
                .enumerate()
                .filter_map(|(index, site)| {
                    let (latitude_deg, longitude_deg) = site_location(site)?;
                    let position = self.lon_lat_to_screen(cell, longitude_deg, latitude_deg);
                    cell.expand(18.0)
                        .contains(position)
                        .then_some((index, position))
                })
                .collect::<Vec<_>>();

            let shift_held = ui.input(|input| input.modifiers.shift);
            if !armed
                && shift_held
                && cell_index == 0
                && response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                // Shift+click pins the inspector (main pane only — its
                // readout samples the primary product).
                self.toggle_inspector_pin(cell, pointer);
            }
            if !armed
                && !shift_held
                && response.clicked()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                if let Some((index, _)) = site_points
                    .iter()
                    .filter_map(|(index, position)| {
                        let distance = position.distance(pointer);
                        (distance <= 12.0).then_some((*index, distance))
                    })
                    .min_by(|left, right| left.1.total_cmp(&right.1))
                {
                    self.selected_site_index = index;
                }
                if let Some(index) = self.hazard_at_position(cell, pointer) {
                    self.selected_hazard_index = Some(index);
                }
            }

            if !armed
                && response.secondary_clicked()
                && let Some(pointer) = response.interact_pointer_pos()
                && let Some(index) = self.nearest_site_to_position(cell, pointer)
                && let Some(site) = self.sites.get(index).cloned()
            {
                self.selected_site_index = index;
                if ui.input(|input| input.modifiers.ctrl) {
                    self.add_or_refresh_radar_layer(site, ui.ctx());
                } else {
                    self.start_latest_level2_load(site, ui.ctx());
                }
            }

            if armed {
                if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    let (lon, lat) = self.screen_to_lon_lat(cell, pointer);
                    match (self.cross_section_a_lonlat, self.cross_section_b_lonlat) {
                        (Some(_), None) => {
                            self.cross_section_b_lonlat = Some((lon, lat));
                            self.cross_section_status =
                                "Section set — drag the map or re-click to redraw".to_owned();
                        }
                        _ => {
                            self.cross_section_a_lonlat = Some((lon, lat));
                            self.cross_section_b_lonlat = None;
                            self.cross_section_status = "Click endpoint B".to_owned();
                        }
                    }
                    self.cross_section_signature = None;
                }
                if response.secondary_clicked() {
                    self.cross_section_a_lonlat = None;
                    self.cross_section_b_lonlat = None;
                    self.cross_section_texture = None;
                    self.cross_section_signature = None;
                    self.cross_section_status = "Cross-section: click endpoint A then B".to_owned();
                }
            }

            self.cross_section_handle_interactions(ui, cell, cell_index + 1);
            hovers.push(response.hover_pos());
        }

        // Paint pass: post-interaction transform; site markers recomputed
        // here so they land at the final positions too.
        for (cell_index, cell) in cells.iter().copied().enumerate() {
            let site_points = self
                .sites
                .iter()
                .enumerate()
                .filter_map(|(index, site)| {
                    let (latitude_deg, longitude_deg) = site_location(site)?;
                    let position = self.lon_lat_to_screen(cell, longitude_deg, latitude_deg);
                    cell.expand(18.0)
                        .contains(position)
                        .then_some((index, position))
                })
                .collect::<Vec<_>>();
            let cell_painter = ui.painter_at(cell);
            self.draw_basemap(&cell_painter, cell);
            self.draw_graticule(&cell_painter, cell);
            if cell_index == 0 {
                self.request_radar_layer_renders(&ctx, cell);
                self.request_texture_render(&ctx, cell);
                self.draw_radar_overlay_layers(&ctx, &cell_painter, cell);
                self.draw_radar_layer(&ctx, &cell_painter, cell);
            } else {
                self.request_pane_render(&ctx, cell, cell_index);
                self.draw_extra_pane_layer(&ctx, &cell_painter, cell, cell_index);
            }
            self.draw_basemap_overlay(&cell_painter, cell);
            // Hazard fills honor THIS pane's product (velocity panes suppress
            // fills so couplets stay readable).
            let fill_product = if cell_index == 0 {
                self.selected_product.clone()
            } else {
                self.extra_panes[cell_index - 1].product.clone()
            };
            self.draw_hazard_overlays(&cell_painter, cell, &fill_product);
            self.draw_rotation_markers(&cell_painter, cell);
            self.draw_storm_tracks(&cell_painter, cell);
            self.draw_placefiles(&cell_painter, cell);
            self.draw_site_markers(&cell_painter, &site_points);
            self.draw_radar_layer_markers(&cell_painter, cell);
            self.draw_loaded_volume_marker(&cell_painter, cell);
            if cell_index == 0 {
                self.draw_colorbar(&cell_painter, cell);
                self.draw_mode_chip(&cell_painter, cell);
            } else if self.extra_panes[cell_index - 1].texture.is_some() {
                // Each pane gets a legend for ITS product.
                self.draw_colorbar_for_product(&cell_painter, cell, &fill_product);
            }
            self.draw_cross_section_line(
                &cell_painter,
                cell,
                hovers.get(cell_index).copied().flatten(),
            );
            if cell_index > 0 {
                self.pane_product_chip(ui, &cell_painter, cell, cell_index);
            } else {
                self.draw_cursor_inspector(&cell_painter, cell, hovers.first().copied().flatten());
            }

            // Cell separator border (four segments — avoids StrokeKind churn);
            // the focused pane gets an accent border.
            let border = if cell_index == self.active_pane {
                egui::Stroke::new(1.6, egui::Color32::from_rgb(96, 150, 210))
            } else {
                egui::Stroke::new(1.0, egui::Color32::from_rgb(46, 52, 60))
            };
            cell_painter.line_segment([cell.left_top(), cell.right_top()], border);
            cell_painter.line_segment([cell.left_bottom(), cell.right_bottom()], border);
            cell_painter.line_segment([cell.left_top(), cell.left_bottom()], border);
            cell_painter.line_segment([cell.right_top(), cell.right_bottom()], border);
        }

        self.cursor_readout = hovered_readout;
        self.basemap_ms = Some(frame_start.elapsed().as_secs_f32() * 1000.0);
    }

    /// A small product chip in an extra pane's top-left corner: shows the
    /// pane's product; click cycles forward through the displayable products,
    /// right-click cycles backward.
    fn pane_product_chip(
        &mut self,
        ui: &egui::Ui,
        painter: &egui::Painter,
        cell: egui::Rect,
        pane_number: usize,
    ) {
        let Some(pane) = pane_number
            .checked_sub(1)
            .and_then(|slot| self.extra_panes.get(slot))
        else {
            return;
        };
        let mut label = pane.product.label().to_owned();
        if let (Some(cut), Some(volume)) = (pane.cut, self.volume.as_deref())
            && let Some(elevation) = volume.cuts.get(cut).map(|c| c.elevation_deg)
        {
            // Pinned tilt — this pane no longer follows the main tilt.
            label = format!("{label} · {elevation:.1}°");
        }
        let pos = egui::pos2(cell.left() + 10.0, cell.top() + 10.0);
        let width = 18.0 + label.chars().count() as f32 * 7.2;
        let chip = egui::Rect::from_min_size(pos, egui::vec2(width, 20.0));
        painter.rect_filled(chip, 4.0, egui::Color32::from_rgb(32, 40, 52));
        painter.text(
            chip.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::monospace(12.0),
            egui::Color32::from_rgb(214, 222, 232),
        );
        let response = ui
            .interact(
                chip,
                ui.id().with(("pane-product-chip", pane_number)),
                egui::Sense::click(),
            )
            .on_hover_text("Click: next product · Right-click: previous");
        let step: isize = if response.clicked() {
            1
        } else if response.secondary_clicked() {
            -1
        } else {
            0
        };
        if step != 0
            && let Some(volume) = self.volume.as_deref()
        {
            let products = global_displayable_products(volume);
            if products.is_empty() {
                return;
            }
            let pane = &mut self.extra_panes[pane_number - 1];
            let current = products
                .iter()
                .position(|product| *product == pane.product)
                .unwrap_or(0);
            let next = (current as isize + step).rem_euclid(products.len() as isize) as usize;
            // Keep the old texture (and its anchor key) on screen while the
            // new product renders — the request guard re-renders because the
            // key's product differs.
            pane.product = products[next].clone();
            pane.render_ms = None;
        }
    }

    /// Fetch/refresh placefiles on background threads and install results.
    /// The UI thread never blocks on the network.
    fn poll_placefiles(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        for slot in &mut self.placefile_slots {
            if let Some(receiver) = &slot.receiver {
                match receiver.try_recv() {
                    Ok(Ok(placefile)) => {
                        slot.receiver = None;
                        slot.next_refresh = Some(
                            now + Duration::from_secs(
                                u64::from(placefile.refresh_minutes.clamp(1, 60)) * 60,
                            ),
                        );
                        slot.status = format!(
                            "{} object(s){}",
                            placefile.objects.len(),
                            if placefile.skipped > 0 {
                                format!(" · {} skipped", placefile.skipped)
                            } else {
                                String::new()
                            }
                        );
                        slot.data = Some(placefile);
                        slot.generation = slot.generation.wrapping_add(1);
                        ctx.request_repaint();
                    }
                    Ok(Err(error)) => {
                        slot.receiver = None;
                        slot.next_refresh = Some(now + Duration::from_secs(120));
                        slot.status = format!("fetch failed: {error}");
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        slot.receiver = None;
                        slot.next_refresh = Some(now + Duration::from_secs(120));
                    }
                }
                continue;
            }
            if !slot.enabled {
                continue;
            }
            let due = slot
                .next_refresh
                .map(|at| now >= at)
                .unwrap_or(slot.data.is_none());
            if !due {
                continue;
            }
            let (sender, receiver) = mpsc::channel();
            slot.receiver = Some(receiver);
            slot.status = "fetching…".to_owned();
            slot.next_refresh = Some(now + Duration::from_secs(120));
            let url = slot.url.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                let result = data_source::fetch_text(&url)
                    .map(|text| placefiles::parse_placefile(&text))
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
                ctx.request_repaint();
            });
        }
    }

    /// Persist the current placefile list into settings.
    fn save_placefile_settings(&mut self) {
        self.app_settings.placefiles = self
            .placefile_slots
            .iter()
            .map(|slot| settings::PlacefileEntry {
                url: slot.url.clone(),
                enabled: slot.enabled,
            })
            .collect();
        let _ = self.app_settings.save();
    }

    /// Draw placefile overlays. Geometry is cached per (slot, generation,
    /// view); text labels draw live. Thresholds follow the GR convention:
    /// an object shows when the viewport spans fewer nautical miles than its
    /// threshold (999 = always).
    fn draw_placefiles(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.placefile_slots.is_empty() {
            return;
        }
        let viewport_nm = (rect.width() / self.map_scale).max(0.01) * 60.0 * 0.868_976; // deg -> nm
        for (index, slot) in self.placefile_slots.iter().enumerate() {
            let (true, Some(placefile)) = (slot.enabled, slot.data.as_ref()) else {
                continue;
            };
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            self.view_shape_key(4, rect).hash(&mut hasher);
            index.hash(&mut hasher);
            slot.generation.hash(&mut hasher);
            let key = hasher.finish();
            let mut cache = self.placefile_shape_cache.borrow_mut();
            let built = cache.get_or_insert_with(key, || {
                self.build_placefile_draw_list(placefile, rect, viewport_nm)
            });
            painter.extend(built.shapes.iter().cloned());
            for (position, text, size, color) in &built.labels {
                draw_halo_text(
                    painter,
                    *position,
                    egui::Align2::CENTER_BOTTOM,
                    text,
                    egui::FontId::proportional(*size),
                    *color,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 190),
                );
            }
        }
    }

    fn build_placefile_draw_list(
        &self,
        placefile: &placefiles::Placefile,
        rect: egui::Rect,
        viewport_nm: f32,
    ) -> PlacefileDrawList {
        let mut out = PlacefileDrawList {
            shapes: Vec::new(),
            labels: Vec::new(),
        };
        let visible = rect.expand(24.0);
        for object in &placefile.objects {
            if viewport_nm > object.threshold_nm() {
                continue;
            }
            match object {
                placefiles::PlacefileObject::Icon {
                    lat,
                    lon,
                    heading_deg,
                    label,
                    color,
                    ..
                } => {
                    let position = self.lon_lat_to_screen(rect, *lon, *lat);
                    if !visible.contains(position) {
                        continue;
                    }
                    let fill = egui::Color32::from_rgb(color[0], color[1], color[2]);
                    out.shapes
                        .push(egui::Shape::circle_filled(position, 4.5, fill));
                    out.shapes.push(egui::Shape::circle_stroke(
                        position,
                        4.5,
                        egui::Stroke::new(1.0, egui::Color32::BLACK),
                    ));
                    // Heading tick (0 = north, clockwise).
                    let angle = heading_deg.to_radians();
                    let direction = egui::vec2(angle.sin(), -angle.cos());
                    out.shapes.push(egui::Shape::line_segment(
                        [position + direction * 5.0, position + direction * 11.0],
                        egui::Stroke::new(2.0, fill),
                    ));
                    if let Some(label) = label
                        && self.map_scale >= 150.0
                    {
                        out.labels.push((
                            position + egui::vec2(0.0, -8.0),
                            label.clone(),
                            10.0,
                            egui::Color32::from_rgb(230, 235, 240),
                        ));
                    }
                }
                placefiles::PlacefileObject::Text {
                    lat,
                    lon,
                    size_px,
                    text,
                    color,
                    ..
                } => {
                    let position = self.lon_lat_to_screen(rect, *lon, *lat);
                    if !visible.contains(position) {
                        continue;
                    }
                    out.labels.push((
                        position,
                        text.clone(),
                        *size_px,
                        egui::Color32::from_rgb(color[0], color[1], color[2]),
                    ));
                }
                placefiles::PlacefileObject::Line {
                    width,
                    points,
                    color,
                    ..
                } => {
                    let screen: Vec<egui::Pos2> = points
                        .iter()
                        .map(|(lat, lon)| self.lon_lat_to_screen(rect, *lon, *lat))
                        .collect();
                    if screen.iter().any(|p| visible.contains(*p)) {
                        out.shapes.push(egui::Shape::line(
                            screen,
                            egui::Stroke::new(
                                *width,
                                egui::Color32::from_rgb(color[0], color[1], color[2]),
                            ),
                        ));
                    }
                }
                placefiles::PlacefileObject::Polygon { points, color, .. } => {
                    let screen: Vec<egui::Pos2> = points
                        .iter()
                        .map(|(lat, lon)| self.lon_lat_to_screen(rect, *lon, *lat))
                        .collect();
                    if !screen.iter().any(|p| visible.contains(*p)) {
                        continue;
                    }
                    let fill =
                        egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], 60);
                    let stroke = egui::Stroke::new(
                        1.5,
                        egui::Color32::from_rgb(color[0], color[1], color[2]),
                    );
                    if is_convex_screen_polygon(&screen) {
                        out.shapes
                            .push(egui::Shape::convex_polygon(screen, fill, stroke));
                    } else {
                        if let Some(mesh) = filled_polygon_mesh(&screen, fill) {
                            out.shapes.push(egui::Shape::mesh(mesh));
                        }
                        out.shapes.push(egui::Shape::closed_line(screen, stroke));
                    }
                }
            }
        }
        out
    }

    /// Kick background storm-cell identification per volume and fold results
    /// into the track history (SCIT association: nearest cell to each
    /// track's predicted position, then unmatched cells start new tracks).
    fn poll_storm_tracks(&mut self, ctx: &egui::Context) {
        if let Some(receiver) = &self.storm_cells_receiver {
            match receiver.try_recv() {
                Ok((volume_ptr, time, cells)) => {
                    self.storm_cells_receiver = None;
                    let current = self
                        .volume
                        .as_ref()
                        .map(|v| Arc::as_ptr(v) as usize)
                        .unwrap_or(0);
                    if volume_ptr == current {
                        self.associate_storm_cells(time, cells);
                        ctx.request_repaint();
                    } else {
                        self.storm_cells_volume_ptr = 0;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.storm_cells_receiver = None,
            }
        }
        if !self.show_storm_tracks {
            return;
        }
        let Some(volume) = self.volume.clone() else {
            return;
        };
        // Site change resets the history (tracks are radar-relative).
        if self.storm_tracks_site != volume.site.id {
            self.storm_tracks.clear();
            self.storm_tracks_site = volume.site.id.clone();
        }
        let volume_ptr = Arc::as_ptr(&volume) as usize;
        if volume_ptr == self.storm_cells_volume_ptr || self.storm_cells_receiver.is_some() {
            return;
        }
        self.storm_cells_volume_ptr = volume_ptr;
        let (sender, receiver) = mpsc::channel();
        self.storm_cells_receiver = Some(receiver);
        let time = volume.volume_time.with_timezone(&Utc);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let cells = identify_storm_cells(&volume);
            let _ = sender.send((volume_ptr, time, cells));
            ctx.request_repaint();
        });
    }

    fn associate_storm_cells(&mut self, time: DateTime<Utc>, cells: Vec<StormCell>) {
        const MATCH_LIMIT_KM: f64 = 16.0;
        let mut taken = vec![false; cells.len()];
        for track in &mut self.storm_tracks {
            let Some(&(last_time, last_east, last_north)) = track.history.last() else {
                continue;
            };
            // Same volume time (live chunk refinement): replace the last fix.
            let dt_s = (time - last_time).num_milliseconds() as f64 / 1000.0;
            let (predicted_east, predicted_north) = match (dt_s > 0.0, track.motion_mps) {
                (true, Some((u, v))) => (
                    last_east + u as f64 * dt_s / 1000.0,
                    last_north + v as f64 * dt_s / 1000.0,
                ),
                _ => (last_east, last_north),
            };
            let mut best: Option<(usize, f64)> = None;
            for (index, cell) in cells.iter().enumerate() {
                if taken[index] {
                    continue;
                }
                let distance = ((cell.east_km - predicted_east).powi(2)
                    + (cell.north_km - predicted_north).powi(2))
                .sqrt();
                if distance <= MATCH_LIMIT_KM && best.is_none_or(|(_, d)| distance < d) {
                    best = Some((index, distance));
                }
            }
            if let Some((index, _)) = best {
                taken[index] = true;
                let cell = &cells[index];
                if dt_s <= 0.0 {
                    if let Some(last) = track.history.last_mut() {
                        *last = (time, cell.east_km, cell.north_km);
                    }
                } else {
                    track.history.push((time, cell.east_km, cell.north_km));
                    if track.history.len() > 12 {
                        track.history.remove(0);
                    }
                }
                track.max_dbz = track.max_dbz.max(cell.max_dbz);
                track.missed = 0;
                track.refit_motion();
            } else if dt_s > 0.0 {
                track.missed = track.missed.saturating_add(1);
            }
        }
        self.storm_tracks.retain(|track| track.missed < 2);
        for (index, cell) in cells.iter().enumerate() {
            if taken[index] {
                continue;
            }
            self.storm_tracks.push(StormTrack {
                id: self.next_storm_track_id,
                history: vec![(time, cell.east_km, cell.north_km)],
                max_dbz: cell.max_dbz,
                motion_mps: None,
                missed: 0,
            });
            self.next_storm_track_id = self.next_storm_track_id.wrapping_add(1).max(1);
        }
    }

    /// Mean motion of established tracks → the SRV storm-motion fields.
    fn storm_motion_from_tracks(&self) -> Option<(f32, f32)> {
        let motions: Vec<(f32, f32)> = self
            .storm_tracks
            .iter()
            .filter(|t| t.history.len() >= 3)
            .filter_map(|t| t.motion_mps)
            .collect();
        if motions.is_empty() {
            return None;
        }
        let n = motions.len() as f32;
        let u = motions.iter().map(|(u, _)| u).sum::<f32>() / n;
        let v = motions.iter().map(|(_, v)| v).sum::<f32>() / n;
        let speed_mps = u.hypot(v);
        if speed_mps < 1.0 {
            return None;
        }
        // StormMotion.direction_deg = direction the storm moves TOWARD
        // (motion_component_away peaks looking down-motion).
        let direction = (u.atan2(v)).to_degrees().rem_euclid(360.0);
        Some((direction, speed_mps / KNOT_TO_MPS))
    }

    /// Draw track histories, current positions, and SCIT-style extrapolated
    /// positions at +15/+30/+45 min along the fitted motion.
    fn draw_storm_tracks(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.show_storm_tracks || self.storm_tracks.is_empty() {
            return;
        }
        let Some((radar_lat, radar_lon)) = self.radar_location() else {
            return;
        };
        let cos_lat = radar_lat.to_radians().cos().max(0.05);
        let to_screen = |east_km: f64, north_km: f64| -> egui::Pos2 {
            let lon = radar_lon + (east_km as f32) / (111.32 * cos_lat);
            let lat = radar_lat + (north_km as f32) / 111.32;
            self.lon_lat_to_screen(rect, lon, lat)
        };
        let line_color = egui::Color32::from_rgb(235, 240, 245);
        for track in &self.storm_tracks {
            if track.history.is_empty() {
                continue;
            }
            let points: Vec<egui::Pos2> = track
                .history
                .iter()
                .map(|&(_, e, n)| to_screen(e, n))
                .collect();
            let current = *points.last().expect("non-empty");
            if !rect.expand(40.0).contains(current) {
                continue;
            }
            if points.len() >= 2 {
                painter.add(egui::Shape::line(
                    points.clone(),
                    egui::Stroke::new(1.6, line_color),
                ));
            }
            painter.circle_filled(current, 3.5, line_color);
            painter.circle_stroke(current, 3.5, egui::Stroke::new(1.0, egui::Color32::BLACK));
            if let Some((u, v)) = track.motion_mps {
                let &(_, east, north) = track.history.last().expect("non-empty");
                let mut previous = current;
                for minutes in [15.0f64, 30.0, 45.0] {
                    let t = minutes * 60.0;
                    let position =
                        to_screen(east + u as f64 * t / 1000.0, north + v as f64 * t / 1000.0);
                    painter.line_segment(
                        [previous, position],
                        egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgba_unmultiplied(235, 240, 245, 110),
                        ),
                    );
                    painter.circle_stroke(position, 2.5, egui::Stroke::new(1.2, line_color));
                    previous = position;
                }
            }
            if self.map_scale >= 90.0 {
                let label = match track.motion_mps {
                    Some((u, v)) => {
                        let dir = (u.atan2(v)).to_degrees().rem_euclid(360.0);
                        let kt = u.hypot(v) / KNOT_TO_MPS;
                        format!(
                            "#{} {:.0}dBZ {:03.0}°/{:.0}kt",
                            track.id, track.max_dbz, dir, kt
                        )
                    }
                    None => format!("#{} {:.0}dBZ", track.id, track.max_dbz),
                };
                draw_halo_text(
                    painter,
                    current + egui::vec2(8.0, -8.0),
                    egui::Align2::LEFT_BOTTOM,
                    &label,
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_rgb(235, 240, 245),
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                );
            }
        }
    }

    /// Kick background rotation detection when the displayed volume changes
    /// and install finished results. Stale results (a newer volume arrived
    /// while detecting) are dropped by pointer check. The detection itself
    /// (dealias + LLSD + clustering, ~tens of ms) never touches the UI thread.
    fn poll_rotation_markers(&mut self, ctx: &egui::Context) {
        // Install any finished detection for the CURRENT volume.
        if let Some(receiver) = &self.rotation_receiver {
            match receiver.try_recv() {
                Ok((volume_ptr, markers)) => {
                    self.rotation_receiver = None;
                    let current = self
                        .volume
                        .as_ref()
                        .map(|v| Arc::as_ptr(v) as usize)
                        .unwrap_or(0);
                    if volume_ptr == current {
                        self.rotation_markers = markers;
                        ctx.request_repaint();
                    } else {
                        // Stale — re-kick below for the new volume.
                        self.rotation_markers_volume_ptr = 0;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.rotation_receiver = None,
            }
        }
        if !self.show_rotation_markers {
            return;
        }
        let Some(volume) = self.volume.clone() else {
            if !self.rotation_markers.is_empty() {
                self.rotation_markers.clear();
                self.rotation_markers_volume_ptr = 0;
            }
            return;
        };
        let volume_ptr = Arc::as_ptr(&volume) as usize;
        if volume_ptr == self.rotation_markers_volume_ptr || self.rotation_receiver.is_some() {
            return;
        }
        let Some((radar_lat, radar_lon)) = self.radar_location() else {
            return;
        };
        self.rotation_markers_volume_ptr = volume_ptr;
        let (sender, receiver) = mpsc::channel();
        self.rotation_receiver = Some(receiver);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let markers = detect_rotation_markers_for_volume(&volume, radar_lat, radar_lon);
            let _ = sender.send((volume_ptr, markers));
            ctx.request_repaint();
        });
    }

    /// Draw rotation markers: meso = gold ring (blue when anticyclonic),
    /// TVS = filled red triangle (NWS convention). Labels show shear in
    /// 10⁻³ s⁻¹ when zoomed in.
    fn draw_rotation_markers(&self, painter: &egui::Painter, rect: egui::Rect) {
        if !self.show_rotation_markers || self.rotation_markers.is_empty() {
            return;
        }
        for marker in &self.rotation_markers {
            let position = self.lon_lat_to_screen(rect, marker.lon, marker.lat);
            if !rect.expand(16.0).contains(position) {
                continue;
            }
            match marker.strength {
                render2d::RotationStrength::Tvs => {
                    let size = 9.0;
                    let points = vec![
                        position + egui::vec2(-size, -size),
                        position + egui::vec2(size, -size),
                        position + egui::vec2(0.0, size * 1.1),
                    ];
                    painter.add(egui::Shape::convex_polygon(
                        points,
                        egui::Color32::from_rgb(225, 32, 38),
                        egui::Stroke::new(1.5, egui::Color32::WHITE),
                    ));
                }
                render2d::RotationStrength::Mesocyclone => {
                    let color = egui::Color32::from_rgb(250, 200, 60);
                    painter.circle_stroke(position, 9.0, egui::Stroke::new(2.6, color));
                    painter.circle_stroke(position, 5.0, egui::Stroke::new(1.6, color));
                }
                render2d::RotationStrength::ModerateCirculation => {
                    painter.circle_stroke(
                        position,
                        8.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(240, 170, 50)),
                    );
                }
                render2d::RotationStrength::WeakCirculation => {
                    painter.circle_stroke(
                        position,
                        7.0,
                        egui::Stroke::new(1.4, egui::Color32::from_rgb(200, 180, 120)),
                    );
                }
            }
            if self.map_scale >= 120.0 {
                draw_halo_text(
                    painter,
                    position + egui::vec2(0.0, -14.0),
                    egui::Align2::CENTER_BOTTOM,
                    &format!("R{} {:.0} m/s", marker.rank, marker.vrot_mps),
                    egui::FontId::proportional(10.0),
                    egui::Color32::from_rgb(245, 240, 220),
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 200),
                );
            }
        }
    }

    /// Paint a LIVE / ARCHIVE / STALE mode chip top-left so a stale frame is
    /// never mistaken for live data (operational safety).
    fn draw_mode_chip(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(volume) = self.volume.as_ref() else {
            return;
        };
        let age_min = (Utc::now() - volume.volume_time.with_timezone(&Utc)).num_minutes();
        let live = self.realtime_level2_auto_refresh;
        // A live feed should refresh every few minutes; if it has not, flag stale.
        let (label, bg) = if live && age_min <= 8 {
            ("● LIVE".to_owned(), egui::Color32::from_rgb(26, 96, 44))
        } else if live {
            (
                format!("● LIVE · STALE {age_min}m"),
                egui::Color32::from_rgb(150, 40, 36),
            )
        } else {
            (
                format!("ARCHIVE · {age_min}m old"),
                egui::Color32::from_rgb(132, 96, 24),
            )
        };
        let pos = egui::pos2(rect.left() + 10.0, rect.top() + 10.0);
        let width = 16.0 + label.chars().count() as f32 * 7.2;
        let chip = egui::Rect::from_min_size(pos, egui::vec2(width, 20.0));
        painter.rect_filled(chip, 4.0, bg);
        painter.text(
            chip.center(),
            egui::Align2::CENTER_CENTER,
            &label,
            egui::FontId::proportional(12.0),
            egui::Color32::from_rgb(232, 236, 240),
        );
    }

    /// Paint an on-canvas color-scale legend for the active product, bottom-right.
    fn draw_colorbar(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.texture.is_none() {
            return;
        }
        self.draw_colorbar_for_product(painter, rect, &self.selected_product.clone());
    }

    fn draw_colorbar_for_product(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        product: &DisplayProduct,
    ) {
        let table = self.color_tables.for_family(product.color_family());
        let stops = table.stops();
        let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
            return;
        };
        let (vmin, vmax) = (first.value, last.value);
        if vmax <= vmin {
            return;
        }

        let bar_w = 16.0;
        let margin = 12.0;
        let bar_h = (rect.height() * 0.42).clamp(120.0, 360.0);
        let x0 = rect.right() - margin - bar_w;
        let top = rect.bottom() - margin - bar_h;
        let bottom = top + bar_h;

        // backing panel (semi-transparent) behind labels + bar
        let panel = egui::Rect::from_min_max(
            egui::pos2(x0 - 56.0, top - 20.0),
            egui::pos2(rect.right() - margin + 3.0, bottom + 6.0),
        );
        painter.rect_filled(
            panel,
            4.0,
            egui::Color32::from_rgba_unmultiplied(10, 12, 15, 200),
        );

        // gradient (top = vmax, bottom = vmin), ~1px steps
        let steps = bar_h.round().max(1.0) as usize;
        let step_h = bar_h / steps as f32;
        for i in 0..steps {
            let f = i as f32 / (steps.max(2) - 1) as f32;
            let v = vmax + (vmin - vmax) * f;
            let c = table.color_for_value(v);
            if c[3] == 0 {
                continue;
            }
            let y = top + bar_h * f;
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, y),
                    egui::pos2(x0 + bar_w, y + step_h + 1.0),
                ),
                0.0,
                egui::Color32::from_rgb(c[0], c[1], c[2]),
            );
        }

        // border (four segments — avoids StrokeKind API churn)
        let border = egui::Stroke::new(1.0, egui::Color32::from_gray(120));
        let (tl, tr) = (egui::pos2(x0, top), egui::pos2(x0 + bar_w, top));
        let (bl, br) = (egui::pos2(x0, bottom), egui::pos2(x0 + bar_w, bottom));
        for seg in [[tl, tr], [tr, br], [br, bl], [bl, tl]] {
            painter.line_segment(seg, border);
        }

        // value ticks + units
        let label_color = egui::Color32::from_rgb(214, 220, 228);
        let font = egui::FontId::proportional(11.0);
        let decimals = if (vmax - vmin) < 5.0 { 2 } else { 0 };
        for t in 0..=5 {
            let f = t as f32 / 5.0;
            let v = vmax + (vmin - vmax) * f;
            let y = top + bar_h * f;
            painter.text(
                egui::pos2(x0 - 5.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{v:.*}", decimals),
                font.clone(),
                label_color,
            );
        }
        painter.text(
            egui::pos2(x0 + bar_w * 0.5, top - 10.0),
            egui::Align2::CENTER_CENTER,
            product_units(product),
            font,
            label_color,
        );
    }

    /// Floating inspector card: a compact data card at the hover position (or
    /// at the Shift+click-pinned geo point, which tracks pan/zoom and updates
    /// with each new volume). Velocity products also get a radial arrow at
    /// the probed gate: pointing along the beam, away from the radar for
    /// outbound, toward it for inbound, colored from the active table.
    fn draw_cursor_inspector(
        &mut self,
        painter: &egui::Painter,
        rect: egui::Rect,
        hover: Option<egui::Pos2>,
    ) {
        if !self.show_inspector_card {
            return;
        }
        let (anchor, readout, pinned) = if let Some((lon, lat)) = self.pinned_inspector_lonlat {
            let position = self.lon_lat_to_screen(rect, lon, lat);
            if !rect.contains(position) {
                return;
            }
            let Some(readout) = self.cursor_readout_at(rect, position) else {
                return;
            };
            (position, readout, true)
        } else {
            let (Some(position), Some(readout)) = (hover, self.cursor_readout.clone()) else {
                return;
            };
            (position, readout, false)
        };

        // Velocity radial arrow at the probed gate.
        if readout.product.color_family() == ColorTableFamily::Velocity
            && readout.value.is_finite()
            && let Some((radar_lat, radar_lon)) = self.radar_location()
        {
            let radar_pos = self.lon_lat_to_screen(rect, radar_lon, radar_lat);
            let away = anchor - radar_pos;
            if away.length() > 1.0 {
                let direction = away.normalized() * if readout.value >= 0.0 { 1.0 } else { -1.0 };
                let color = {
                    let table = self.color_tables.for_family(ColorTableFamily::Velocity);
                    let c = table.color_for_value(readout.value);
                    egui::Color32::from_rgb(c[0], c[1], c[2])
                };
                let vector = direction * 26.0;
                painter.arrow(anchor, vector, egui::Stroke::new(4.0, egui::Color32::BLACK));
                painter.arrow(anchor, vector, egui::Stroke::new(2.0, color));
            }
        }
        if pinned {
            painter.circle_filled(anchor, 3.0, egui::Color32::from_rgb(255, 226, 120));
            painter.circle_stroke(anchor, 5.0, egui::Stroke::new(1.2, egui::Color32::BLACK));
        }

        // Card lines.
        let units = product_units(&readout.product);
        let mut lines = vec![format!(
            "{} {:.1}{}{}",
            readout.product.label(),
            readout.value,
            if units.is_empty() { "" } else { " " },
            units
        )];
        if let Some(base) = readout.base_value {
            let nyquist = readout
                .nyquist_velocity_mps
                .map(|n| format!(" · Nyq {n:.0}"))
                .unwrap_or_default();
            lines.push(format!("raw VEL {base:.1} m/s{nyquist}"));
        }
        lines.push(format!(
            "{:.1} km @ {:03.0}° · tilt {:.1}°",
            readout.range_km, readout.azimuth_deg, readout.elevation_deg
        ));
        lines.push(format!(
            "beam ↑ {:.0} m ({:.1} kft)",
            readout.height_above_radar_m,
            readout.height_above_radar_m * 0.003_280_84
        ));
        if let Some(probe) = readout.vrot {
            lines.push(format!(
                "Vrot {:.1} m/s · ΔV {:.1} · sep {:.2} km",
                probe.vrot_mps, probe.delta_v_mps, probe.separation_km
            ));
        }
        if pinned {
            lines.push("pinned — Shift+click to release".to_owned());
        }

        let font = egui::FontId::monospace(11.0);
        let text_color = egui::Color32::from_rgb(222, 228, 236);
        let galleys: Vec<_> = lines
            .iter()
            .map(|line| painter.layout_no_wrap(line.clone(), font.clone(), text_color))
            .collect();
        let width = galleys.iter().map(|g| g.size().x).fold(0.0f32, f32::max) + 14.0;
        let height = galleys.iter().map(|g| g.size().y + 2.0).sum::<f32>() + 10.0;
        let mut origin = anchor + egui::vec2(16.0, 14.0);
        if origin.x + width > rect.right() - 4.0 {
            origin.x = anchor.x - 16.0 - width;
        }
        if origin.y + height > rect.bottom() - 4.0 {
            origin.y = anchor.y - 14.0 - height;
        }
        let card = egui::Rect::from_min_size(origin, egui::vec2(width, height));
        painter.rect_filled(
            card,
            5.0,
            egui::Color32::from_rgba_unmultiplied(12, 15, 20, 232),
        );
        let border = egui::Stroke::new(
            1.0,
            if pinned {
                egui::Color32::from_rgb(255, 226, 120)
            } else {
                egui::Color32::from_rgb(60, 70, 84)
            },
        );
        painter.line_segment([card.left_top(), card.right_top()], border);
        painter.line_segment([card.right_top(), card.right_bottom()], border);
        painter.line_segment([card.right_bottom(), card.left_bottom()], border);
        painter.line_segment([card.left_bottom(), card.left_top()], border);
        let mut y = card.top() + 5.0;
        for galley in galleys {
            let size = galley.size();
            painter.galley(egui::pos2(card.left() + 7.0, y), galley, text_color);
            y += size.y + 2.0;
        }
    }

    /// Shift+click: pin the inspector to a geo point (or release a pin when
    /// clicking within a few pixels of it).
    fn toggle_inspector_pin(&mut self, rect: egui::Rect, pointer: egui::Pos2) {
        if let Some((lon, lat)) = self.pinned_inspector_lonlat {
            let current = self.lon_lat_to_screen(rect, lon, lat);
            if current.distance(pointer) <= 14.0 {
                self.pinned_inspector_lonlat = None;
                return;
            }
        }
        let (lon, lat) = self.screen_to_lon_lat(rect, pointer);
        self.pinned_inspector_lonlat = Some((lon, lat));
    }

    /// Draw the armed cross-section line + endpoint handles on the map (and a
    /// rubber-band from A to the cursor while only A is set).
    fn draw_cross_section_line(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        hover: Option<egui::Pos2>,
    ) {
        let accent = egui::Color32::from_rgb(255, 226, 120);
        let to_screen = |(lon, lat): (f32, f32)| self.lon_lat_to_screen(rect, lon, lat);
        let handle = |painter: &egui::Painter, p: egui::Pos2, label: &str| {
            painter.circle_filled(p, 4.5, accent);
            painter.circle_stroke(p, 4.5, egui::Stroke::new(1.0, egui::Color32::BLACK));
            painter.text(
                p + egui::vec2(0.0, -8.0),
                egui::Align2::CENTER_BOTTOM,
                label,
                egui::FontId::proportional(12.0),
                accent,
            );
        };
        match (self.cross_section_a_lonlat, self.cross_section_b_lonlat) {
            (Some(a), Some(b)) => {
                let (pa, pb) = (to_screen(a), to_screen(b));
                painter.line_segment([pa, pb], egui::Stroke::new(2.0, accent));
                handle(painter, pa, "A");
                handle(painter, pb, "B");
            }
            (Some(a), None) => {
                let pa = to_screen(a);
                if let Some(h) = hover {
                    painter.line_segment(
                        [pa, h],
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 160, 90)),
                    );
                }
                handle(painter, pa, "A");
            }
            _ => {}
        }
    }

    /// Draggable A/B endpoint handles for the section line: registered after
    /// the canvas response so they win the hit test; dragging one sweeps the
    /// section live (the signature recompute follows the endpoint).
    fn cross_section_handle_interactions(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        id_salt: usize,
    ) {
        for (which, endpoint) in [
            (0usize, self.cross_section_a_lonlat),
            (1usize, self.cross_section_b_lonlat),
        ] {
            let Some((lon, lat)) = endpoint else {
                continue;
            };
            let position = self.lon_lat_to_screen(rect, lon, lat);
            if !rect.expand(12.0).contains(position) {
                continue;
            }
            let handle = egui::Rect::from_center_size(position, egui::vec2(20.0, 20.0));
            let response = ui
                .interact(
                    handle,
                    ui.id().with(("xs-handle", id_salt, which)),
                    egui::Sense::drag(),
                )
                .on_hover_cursor(egui::CursorIcon::Grab)
                .on_hover_text("Drag to sweep the cross-section");
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let next = self.screen_to_lon_lat(rect, pointer);
                if which == 0 {
                    self.cross_section_a_lonlat = Some(next);
                } else {
                    self.cross_section_b_lonlat = Some(next);
                }
            }
        }
    }

    /// Recompute the cross-section texture if its inputs changed (signature
    /// guard avoids per-frame work).
    fn update_cross_section_texture(&mut self, ctx: &egui::Context) {
        let (Some(a), Some(b)) = (self.cross_section_a_lonlat, self.cross_section_b_lonlat) else {
            self.cross_section_texture = None;
            return;
        };
        let Some(volume) = self.volume.clone() else {
            self.cross_section_status = "No volume loaded".to_owned();
            self.cross_section_texture = None;
            return;
        };
        let Some((radar_lat, radar_lon)) = self.loaded_volume_location() else {
            return;
        };
        // Only reflectivity and (dealiased) velocity sections are supported.
        // Velocity-family products → velocity section; plain REF and the
        // reflectivity-derived volume products (CREF/ET/VIL/VILD) → REF section;
        // everything else (SW/ZDR/CC/PHI/KDP) has no section — show a hint
        // rather than a mislabeled reflectivity slice.
        let velocity = self.selected_product.base_moment() == MomentType::Velocity;
        let reflectivity = matches!(
            self.selected_product,
            DisplayProduct::Moment(MomentType::Reflectivity)
        ) || self
            .selected_product
            .derived()
            .is_some_and(|d| d.is_volume_wide());
        if !velocity && !reflectivity {
            self.cross_section_status =
                "Cross-section supports reflectivity & velocity products".to_owned();
            self.cross_section_texture = None;
            return;
        }
        let family = if velocity {
            ColorTableFamily::Velocity
        } else {
            ColorTableFamily::Reflectivity
        };
        let to_en = |(lon, lat): (f32, f32)| {
            let north = (lat - radar_lat) * 111.32;
            let east = (lon - radar_lon) * 111.32 * radar_lat.to_radians().cos();
            (east, north)
        };
        let (start, end) = (to_en(a), to_en(b));
        // Auto-scale the ceiling to the beam coverage at the section's far
        // end (highest tilt's beam height there, plus headroom) so a nearby
        // storm fills the panel instead of hugging the bottom of an 18 km box.
        let max_range_m = (start.0.hypot(start.1).max(end.0.hypot(end.1)) as f64) * 1000.0;
        let max_elevation = volume
            .cuts
            .iter()
            .map(|cut| cut.elevation_deg)
            .fold(0.5f32, f32::max);
        let coverage_top =
            radar_core::beam_height_above_radar_m(max_range_m, max_elevation as f64) as f32;
        let top_m = (coverage_top * 1.08).clamp(4_000.0, CROSS_SECTION_TOP_M);
        let (w, h) = (640usize, 256usize);

        // recompute guard (top_m derives from endpoints+volume, both hashed)
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        a.0.to_bits().hash(&mut hasher);
        a.1.to_bits().hash(&mut hasher);
        b.0.to_bits().hash(&mut hasher);
        b.1.to_bits().hash(&mut hasher);
        velocity.hash(&mut hasher);
        (Arc::as_ptr(&volume) as usize).hash(&mut hasher);
        self.color_tables
            .signature_for_family(family)
            .hash(&mut hasher);
        let sig = hasher.finish();
        if self.cross_section_signature == Some(sig) && self.cross_section_texture.is_some() {
            return;
        }
        // User-input-only signature (no volume identity): if the only thing
        // that changed is the volume AND it is a live partial still streaming
        // in with fewer tilts than the last full section, HOLD the current
        // section — recomputing per chunk collapses it to a one-beam ribbon.
        let mut user_hasher = std::collections::hash_map::DefaultHasher::new();
        a.0.to_bits().hash(&mut user_hasher);
        a.1.to_bits().hash(&mut user_hasher);
        b.0.to_bits().hash(&mut user_hasher);
        b.1.to_bits().hash(&mut user_hasher);
        velocity.hash(&mut user_hasher);
        self.color_tables
            .signature_for_family(family)
            .hash(&mut user_hasher);
        let user_sig = user_hasher.finish();
        let live_partial = self
            .selected_frame()
            .is_some_and(|frame| frame.status == FrameStatus::LivePartial);
        if live_partial
            && self.cross_section_texture.is_some()
            && self.cross_section_user_signature == Some(user_sig)
            && volume.cuts.len() < self.cross_section_volume_cuts
        {
            self.cross_section_status = format!(
                "holding last full section — live volume building ({}/{} tilts)",
                volume.cuts.len(),
                self.cross_section_volume_cuts
            );
            return;
        }

        let section = if velocity {
            velocity_cross_section_cached(
                &volume,
                &mut self.cross_section_dealias_cache,
                start,
                end,
                w,
                h,
                top_m,
            )
        } else {
            reflectivity_cross_section(&volume, start, end, w, h, top_m)
        };
        let Some(section) = section else {
            self.cross_section_status = "No data along section".to_owned();
            self.cross_section_texture = None;
            self.cross_section_signature = Some(sig);
            return;
        };

        let table = self.color_tables.for_family(family);
        let mut rgba = vec![0u8; section.width * section.height * 4];
        for (cell, value) in rgba.chunks_exact_mut(4).zip(section.values.iter()) {
            if value.is_finite() {
                cell.copy_from_slice(&table.color_for_value(*value));
            }
        }
        // from_rgba_unmultiplied: palette stops may carry partial alpha, which
        // would violate the premultiplied invariant of the radar texture helper.
        let image =
            egui::ColorImage::from_rgba_unmultiplied([section.width, section.height], &rgba);
        match &mut self.cross_section_texture {
            Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.cross_section_texture =
                    Some(ctx.load_texture("cross-section", image, egui::TextureOptions::LINEAR));
            }
        }
        self.cross_section_signature = Some(sig);
        self.cross_section_user_signature = Some(user_sig);
        self.cross_section_volume_cuts = volume.cuts.len();
        self.cross_section_top_m = top_m;
        self.cross_section_status = format!(
            "{} · {:.0} km long · top {:.0} km",
            if velocity { "Velocity" } else { "Reflectivity" },
            section.length_m / 1000.0,
            top_m / 1000.0,
        );
    }

    /// The resizable bottom cross-section panel: header + the rendered section
    /// with height (km) and distance (km) axis labels.
    fn cross_section_panel(&mut self, ui: &mut egui::Ui) {
        self.update_cross_section_texture(ui.ctx());
        ui.horizontal(|ui| {
            ui.strong("Cross-Section");
            ui.separator();
            ui.label(&self.cross_section_status);
        });
        let avail = ui.available_size();
        if avail.y < 24.0 {
            return;
        }
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(12, 14, 18));
        let Some(texture) = &self.cross_section_texture else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Arm cross-section and click two points on the map",
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(170, 178, 188),
            );
            return;
        };
        // Reserve a left gutter (height labels) + bottom strip (distance labels).
        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 38.0, rect.top() + 2.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 16.0),
        );
        painter.image(
            texture.id(),
            plot,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let label = egui::Color32::from_rgb(190, 196, 204);
        let font = egui::FontId::proportional(10.0);
        let top_km = self.cross_section_top_m / 1000.0;
        for k in 0..=3 {
            let frac = k as f32 / 3.0;
            let y = plot.top() + plot.height() * frac;
            painter.text(
                egui::pos2(plot.left() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{:.0}", top_km * (1.0 - frac)),
                font.clone(),
                label,
            );
        }
        painter.text(
            egui::pos2(rect.left() + 2.0, plot.top()),
            egui::Align2::LEFT_TOP,
            "km",
            font.clone(),
            label,
        );
        painter.text(
            egui::pos2(plot.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            "distance along section (km) — A → B",
            font,
            label,
        );
    }

    fn draw_radar_layer(&self, ctx: &egui::Context, painter: &egui::Painter, rect: egui::Rect) {
        let Some(volume) = self.volume.as_ref() else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.radar_location() else {
            return;
        };
        if let Some(texture) = &self.texture {
            let image_rect = self
                .texture_key
                .as_ref()
                .map(|key| self.radar_texture_rect(ctx, rect, latitude_deg, longitude_deg, key))
                .unwrap_or(rect);

            painter.image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        self.draw_range_ring(
            painter,
            rect,
            latitude_deg,
            longitude_deg,
            self.radar_range_km,
            egui::Stroke::new(
                1.8,
                freshness_ring_color(volume.volume_time.with_timezone(&Utc), Utc::now(), 230),
            ),
        );
    }

    fn draw_radar_overlay_layers(
        &self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
    ) {
        for layer in &self.radar_layers {
            if !layer.visible {
                continue;
            }
            let Some(volume) = layer.volume.as_ref() else {
                continue;
            };
            let Some((latitude_deg, longitude_deg)) = layer.radar_location() else {
                continue;
            };
            if let Some(texture) = &layer.texture {
                let image_rect = layer
                    .texture_key
                    .as_ref()
                    .map(|key| self.radar_texture_rect(ctx, rect, latitude_deg, longitude_deg, key))
                    .unwrap_or(rect);
                painter.image(
                    texture.id(),
                    image_rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::from_white_alpha(layer.opacity),
                );
            }
            self.draw_range_ring(
                painter,
                rect,
                latitude_deg,
                longitude_deg,
                layer.radar_range_km,
                egui::Stroke::new(
                    1.5,
                    freshness_ring_color(
                        volume.volume_time.with_timezone(&Utc),
                        Utc::now(),
                        layer.opacity,
                    ),
                ),
            );
        }
    }

    fn radar_texture_rect(
        &self,
        ctx: &egui::Context,
        rect: egui::Rect,
        radar_lat: f32,
        radar_lon: f32,
        texture_key: &TextureKey,
    ) -> egui::Rect {
        let Some((current, _)) =
            self.viewport_raster_options_for_location(ctx, rect, radar_lat, radar_lon)
        else {
            return rect;
        };
        anchored_radar_texture_rect(rect, ctx.pixels_per_point(), texture_key.viewport, current)
    }

    fn draw_hazard_overlays(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        fill_product: &DisplayProduct,
    ) {
        if !self.hazards_visible {
            return;
        }
        let Some(overlay) = &self.hazard_overlay else {
            return;
        };
        // Polygon projection + ear-clip tessellation is cached per view key:
        // idle repaints reuse it; pan/zoom/selection/content changes rebuild.
        // The generation counter invalidates exactly on overlay replacement.
        use std::hash::{Hash, Hasher};
        let _ = overlay;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.view_shape_key(2, rect).hash(&mut hasher);
        self.hazard_overlay_generation.hash(&mut hasher);
        self.selected_hazard_index.hash(&mut hasher);
        self.hazard_fill_alpha.hash(&mut hasher);
        fill_product.label().hash(&mut hasher);
        self.hazards_active_only.hash(&mut hasher);
        for family in &self.hidden_hazard_families {
            family.hash(&mut hasher);
        }
        let key = hasher.finish();
        let mut cache = self.hazard_shape_cache.borrow_mut();
        let built =
            cache.get_or_insert_with(key, || self.build_hazard_overlay_shapes(rect, fill_product));
        painter.extend(built.shapes.iter().cloned());
        for (center, label, selected) in &built.labels {
            draw_halo_text(
                painter,
                *center,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(if *selected { 12.0 } else { 11.0 }),
                egui::Color32::from_rgb(245, 248, 250),
                egui::Color32::from_rgba_unmultiplied(0, 0, 0, 210),
            );
        }
    }

    fn build_hazard_overlay_shapes(
        &self,
        rect: egui::Rect,
        fill_product: &DisplayProduct,
    ) -> HazardOverlayShapes {
        let mut out = HazardOverlayShapes {
            shapes: Vec::new(),
            labels: Vec::new(),
        };
        let Some(overlay) = &self.hazard_overlay else {
            return out;
        };
        let bounds = self.visible_geo_bounds(rect).expand(0.05);
        for (index, record) in overlay.records.iter().enumerate() {
            if !self.hazard_record_visible(record)
                || !hazard_points_renderable(&record.points)
                || !bounds.intersects_bbox(record.bbox)
            {
                continue;
            }
            let points = record
                .points
                .iter()
                .map(|point| self.lon_lat_to_screen(rect, point.lon, point.lat))
                .collect::<Vec<_>>();
            if points.len() < 3 {
                continue;
            }
            let selected = self.selected_hazard_index == Some(index);
            let color = hazard_color(record);
            let fill_alpha =
                hazard_fill_alpha_for_product(self.hazard_fill_alpha, selected, fill_product);
            let fill =
                egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), fill_alpha);
            let stroke = egui::Stroke::new(
                if selected { 2.4 } else { 1.5 },
                egui::Color32::from_rgba_unmultiplied(
                    color.r(),
                    color.g(),
                    color.b(),
                    if selected { 245 } else { 205 },
                ),
            );
            if is_convex_screen_polygon(&points) {
                out.shapes
                    .push(egui::Shape::convex_polygon(points.clone(), fill, stroke));
            } else {
                if let Some(mesh) = filled_polygon_mesh(&points, fill) {
                    out.shapes.push(egui::Shape::mesh(mesh));
                }
                out.shapes
                    .push(egui::Shape::closed_line(points.clone(), stroke));
            }
            let center = polygon_screen_centroid(&points);
            if rect.expand(24.0).contains(center) && self.map_scale >= 62.0 {
                out.labels.push((center, record.label.clone(), selected));
            }
        }
        out
    }

    /// Cache key for view-pure draw geometry: layer tag + cell rect + the
    /// shared geo transform. Bit-exact — any pan/zoom/resize changes it.
    fn view_shape_key(&self, tag: u8, rect: egui::Rect) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tag.hash(&mut hasher);
        rect.min.x.to_bits().hash(&mut hasher);
        rect.min.y.to_bits().hash(&mut hasher);
        rect.max.x.to_bits().hash(&mut hasher);
        rect.max.y.to_bits().hash(&mut hasher);
        self.map_center_lon.to_bits().hash(&mut hasher);
        self.map_center_lat.to_bits().hash(&mut hasher);
        self.map_scale.to_bits().hash(&mut hasher);
        hasher.finish()
    }

    fn draw_basemap(&self, painter: &egui::Painter, rect: egui::Rect) {
        // Polyline reprojection is cached per view key (pure in rect + geo
        // transform); idle repaints reuse the projected shapes.
        let key = self.view_shape_key(0, rect);
        let mut cache = self.basemap_shape_cache.borrow_mut();
        let shapes = cache.get_or_insert_with(key, || self.build_basemap_shapes(rect));
        painter.extend(shapes.iter().cloned());
    }

    fn build_basemap_shapes(&self, rect: egui::Rect) -> Vec<egui::Shape> {
        let mut sink = Vec::new();
        let bounds = self.visible_geo_bounds(rect).expand(0.25);
        let us_detail_visible = us_detail_visible(bounds);
        self.collect_basemap_line_shapes(
            rect,
            bounds,
            basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
            egui::Stroke::new(0.75, egui::Color32::from_rgb(31, 45, 57)),
            &mut sink,
        );

        if us_detail_visible && self.map_scale >= 38.0 {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                egui::Stroke::new(0.65, egui::Color32::from_rgb(24, 35, 46)),
                &mut sink,
            );
        }
        if us_detail_visible {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                egui::Stroke::new(1.05, egui::Color32::from_rgb(41, 58, 73)),
                &mut sink,
            );
        }

        if self.map_scale >= 36.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.collect_basemap_line_shapes(
                        rect,
                        bounds,
                        layer.admin_lines,
                        egui::Stroke::new(0.85, egui::Color32::from_rgb(36, 52, 65)),
                        &mut sink,
                    );
                }
            }
        }
        sink
    }

    fn draw_basemap_overlay(&self, painter: &egui::Painter, rect: egui::Rect) {
        let bounds = self.visible_geo_bounds(rect).expand(0.15);
        // Lines are view-pure and cached; labels (font layout + collision
        // budgets) stay live each frame.
        let key = self.view_shape_key(1, rect);
        let mut cache = self.basemap_shape_cache.borrow_mut();
        let shapes = cache.get_or_insert_with(key, || self.build_basemap_overlay_shapes(rect));
        painter.extend(shapes.iter().cloned());
        drop(cache);

        let mut occupied = Vec::with_capacity(128);
        self.draw_world_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_regional_place_labels(painter, rect, bounds, &mut occupied);
        self.draw_admin_labels(painter, rect, bounds, &mut occupied);
    }

    fn build_basemap_overlay_shapes(&self, rect: egui::Rect) -> Vec<egui::Shape> {
        let mut sink = Vec::new();
        let bounds = self.visible_geo_bounds(rect).expand(0.15);
        let us_detail_visible = us_detail_visible(bounds);
        if self.map_scale >= 18.0 {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
                egui::Stroke::new(
                    0.85,
                    egui::Color32::from_rgba_unmultiplied(102, 126, 145, 84),
                ),
                &mut sink,
            );
        }

        if us_detail_visible && self.map_scale >= 76.0 {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LINES,
                egui::Stroke::new(
                    0.55,
                    egui::Color32::from_rgba_unmultiplied(92, 112, 128, 92),
                ),
                &mut sink,
            );
        }
        if us_detail_visible {
            self.collect_basemap_line_shapes(
                rect,
                bounds,
                basemap_data::BASEMAP_US_STATE_LINES,
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(126, 150, 170, 116),
                ),
                &mut sink,
            );
        }

        if self.map_scale >= 74.0 {
            for layer in REGIONAL_BASEMAP_LAYERS {
                if bounds.intersects_bbox(layer.bounds) {
                    self.collect_basemap_line_shapes(
                        rect,
                        bounds,
                        layer.admin_lines,
                        egui::Stroke::new(
                            0.75,
                            egui::Color32::from_rgba_unmultiplied(112, 136, 154, 96),
                        ),
                        &mut sink,
                    );
                }
            }
        }
        sink
    }

    fn collect_basemap_line_shapes(
        &self,
        rect: egui::Rect,
        bounds: GeoBounds,
        lines: &[basemap_data::BasemapLine],
        stroke: egui::Stroke,
        sink: &mut Vec<egui::Shape>,
    ) {
        for line in lines {
            if bounds.intersects_bbox(line.bbox)
                && let Some(shape) = self.geo_line_shape(rect, line.points, stroke)
            {
                sink.push(shape);
            }
        }
    }

    fn geo_line_shape(
        &self,
        rect: egui::Rect,
        coordinates: &[(f32, f32)],
        stroke: egui::Stroke,
    ) -> Option<egui::Shape> {
        if coordinates.len() < 2 {
            return None;
        }
        let simplify_px_sq = basemap_line_simplification_px(self.map_scale).powi(2);
        let mut points = Vec::with_capacity(coordinates.len());
        for (index, (longitude_deg, latitude_deg)) in coordinates.iter().enumerate() {
            let point = self.lon_lat_to_screen(rect, *longitude_deg, *latitude_deg);
            let is_endpoint = index == 0 || index + 1 == coordinates.len();
            if !is_endpoint
                && simplify_px_sq > 0.0
                && points
                    .last()
                    .is_some_and(|last: &egui::Pos2| last.distance_sq(point) < simplify_px_sq)
            {
                continue;
            }
            points.push(point);
        }
        (points.len() >= 2).then(|| egui::Shape::line(points, stroke))
    }

    fn draw_world_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = world_place_label_rank(self.map_scale) else {
            return;
        };
        self.draw_place_label_set(
            painter,
            rect,
            bounds,
            PlaceLabelSet {
                labels: basemap_data::BASEMAP_WORLD_PLACE_LABELS,
                max_rank,
                max_labels: world_label_budget(self.map_scale),
            },
            occupied,
        );
    }

    fn draw_regional_place_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let Some(max_rank) = place_label_rank(self.map_scale) else {
            return;
        };
        let max_labels = label_budget(self.map_scale);
        if us_detail_visible(bounds) {
            self.draw_place_label_set(
                painter,
                rect,
                bounds,
                PlaceLabelSet {
                    labels: basemap_data::BASEMAP_US_PLACE_LABELS,
                    max_rank,
                    max_labels,
                },
                occupied,
            );
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_place_label_set(
                    painter,
                    rect,
                    bounds,
                    PlaceLabelSet {
                        labels: layer.place_labels,
                        max_rank,
                        max_labels,
                    },
                    occupied,
                );
            }
        }
    }

    fn draw_place_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        place_labels: PlaceLabelSet,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(if self.map_scale >= 190.0 { 12.0 } else { 11.0 });
        let text_color = egui::Color32::from_rgb(198, 207, 214);
        let halo_color = egui::Color32::from_rgba_unmultiplied(3, 5, 8, 210);
        let dot_color = egui::Color32::from_rgb(118, 143, 158);
        let mut drawn = 0usize;

        for label in place_labels.labels {
            if label.rank > place_labels.max_rank || !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(32.0).contains(position) {
                continue;
            }
            let text_position = egui::pos2(position.x + 4.0, position.y - 1.0);
            let label_rect = left_label_rect(text_position, label.name, font.size).expand(2.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            painter.circle_filled(position, 1.5, dot_color);
            draw_halo_text(
                painter,
                text_position,
                egui::Align2::LEFT_CENTER,
                label.name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= place_labels.max_labels {
                break;
            }
        }
    }

    fn draw_admin_labels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        occupied: &mut Vec<egui::Rect>,
    ) {
        if self.map_scale < 118.0 {
            return;
        }
        let max_labels = if self.map_scale >= 220.0 { 72 } else { 36 };
        if us_detail_visible(bounds) {
            self.draw_admin_label_set(
                painter,
                rect,
                bounds,
                basemap_data::BASEMAP_US_COUNTY_LABELS,
                max_labels,
                occupied,
            );
        }
        for layer in REGIONAL_BASEMAP_LAYERS {
            if bounds.intersects_bbox(layer.bounds) {
                self.draw_admin_label_set(
                    painter,
                    rect,
                    bounds,
                    layer.admin_labels,
                    max_labels,
                    occupied,
                );
            }
        }
    }

    fn draw_admin_label_set(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        bounds: GeoBounds,
        labels: &[basemap_data::BasemapLabel],
        max_labels: usize,
        occupied: &mut Vec<egui::Rect>,
    ) {
        let font = egui::FontId::proportional(10.0);
        let text_color = egui::Color32::from_rgba_unmultiplied(150, 164, 176, 184);
        let halo_color = egui::Color32::from_rgba_unmultiplied(2, 4, 7, 180);
        let mut drawn = 0usize;

        for label in labels {
            if !bounds.contains(label.lon, label.lat) {
                continue;
            }
            let position = self.lon_lat_to_screen(rect, label.lon, label.lat);
            if !rect.expand(24.0).contains(position) {
                continue;
            }
            let label_rect = centered_label_rect(position, label.name, font.size).expand(5.0);
            if !rect.expand(80.0).intersects(label_rect) || overlaps_any(occupied, label_rect) {
                continue;
            }
            draw_halo_text(
                painter,
                position,
                egui::Align2::CENTER_CENTER,
                label.name,
                font.clone(),
                text_color,
                halo_color,
            );
            occupied.push(label_rect);
            drawn += 1;
            if drawn >= max_labels {
                break;
            }
        }
    }

    fn draw_graticule(&self, painter: &egui::Painter, rect: egui::Rect) {
        let (west, north) = self.screen_to_lon_lat(rect, rect.left_top());
        let (east, south) = self.screen_to_lon_lat(rect, rect.right_bottom());
        let lon_min = west.min(east);
        let lon_max = west.max(east);
        let lat_min = south.min(north).clamp(-85.0, 85.0);
        let lat_max = south.max(north).clamp(-85.0, 85.0);
        let step = graticule_step(rect.width() / self.lon_pixels_per_degree());
        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(28, 38, 50));
        let label_color = egui::Color32::from_rgb(92, 108, 124);

        let mut lon = (lon_min / step).floor() * step;
        while lon <= lon_max {
            let top = self.lon_lat_to_screen(rect, lon, lat_max);
            let bottom = self.lon_lat_to_screen(rect, lon, lat_min);
            painter.line_segment([top, bottom], stroke);
            painter.text(
                egui::pos2(top.x + 4.0, rect.top() + 6.0),
                egui::Align2::LEFT_TOP,
                format!("{:.0}", normalize_lon(lon)),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lon += step;
        }

        let mut lat = (lat_min / step).floor() * step;
        while lat <= lat_max {
            let left = self.lon_lat_to_screen(rect, lon_min, lat);
            let right = self.lon_lat_to_screen(rect, lon_max, lat);
            painter.line_segment([left, right], stroke);
            painter.text(
                egui::pos2(rect.left() + 6.0, left.y - 2.0),
                egui::Align2::LEFT_CENTER,
                format!("{lat:.0}"),
                egui::FontId::monospace(10.0),
                label_color,
            );
            lat += step;
        }
    }

    fn draw_range_ring(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        latitude_deg: f32,
        longitude_deg: f32,
        range_km: f32,
        stroke: egui::Stroke,
    ) {
        let (lat_radius, lon_radius) = range_radius_deg(latitude_deg, range_km);
        let mut points = Vec::with_capacity(97);
        for index in 0..=96 {
            let angle = index as f32 / 96.0 * std::f32::consts::TAU;
            let latitude = latitude_deg + lat_radius * angle.sin();
            let longitude = longitude_deg + lon_radius * angle.cos();
            points.push(self.lon_lat_to_screen(rect, longitude, latitude));
        }
        painter.add(egui::Shape::line(points, stroke));
    }

    fn draw_site_markers(&self, painter: &egui::Painter, site_points: &[(usize, egui::Pos2)]) {
        for (index, position) in site_points {
            let selected = *index == self.selected_site_index;
            let fill = if selected {
                egui::Color32::from_rgb(88, 210, 245)
            } else {
                egui::Color32::from_rgb(106, 132, 154)
            };
            let radius = if selected { 5.5 } else { 3.0 };
            painter.circle_filled(*position, radius, fill);
            if selected {
                painter.circle_stroke(
                    *position,
                    10.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(236, 246, 255)),
                );
                if let Some(site) = self.sites.get(*index) {
                    painter.text(
                        *position + egui::vec2(12.0, -10.0),
                        egui::Align2::LEFT_CENTER,
                        &site.level2_id,
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_rgb(238, 246, 255),
                    );
                }
            }
        }
    }

    fn draw_loaded_volume_marker(&self, painter: &egui::Painter, rect: egui::Rect) {
        let Some(volume) = &self.volume else {
            return;
        };
        let Some((latitude_deg, longitude_deg)) = self.loaded_volume_location() else {
            return;
        };
        let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
        if !rect.expand(18.0).contains(position) {
            return;
        }

        painter.circle_filled(position, 6.0, egui::Color32::from_rgb(88, 230, 245));
        painter.circle_stroke(
            position,
            11.0,
            egui::Stroke::new(1.8, egui::Color32::from_rgb(244, 252, 255)),
        );
        painter.text(
            position + egui::vec2(12.0, -10.0),
            egui::Align2::LEFT_CENTER,
            &volume.site.id,
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(244, 252, 255),
        );
    }

    fn draw_radar_layer_markers(&self, painter: &egui::Painter, rect: egui::Rect) {
        for layer in &self.radar_layers {
            if !layer.visible {
                continue;
            }
            let Some((latitude_deg, longitude_deg)) = layer.radar_location() else {
                continue;
            };
            let position = self.lon_lat_to_screen(rect, longitude_deg, latitude_deg);
            if !rect.expand(18.0).contains(position) {
                continue;
            }
            let color = egui::Color32::from_rgba_unmultiplied(88, 190, 245, layer.opacity);
            painter.circle_filled(position, 4.5, color);
            painter.circle_stroke(
                position,
                8.5,
                egui::Stroke::new(
                    1.3,
                    egui::Color32::from_rgba_unmultiplied(214, 242, 255, layer.opacity),
                ),
            );
            painter.text(
                position + egui::vec2(10.0, 10.0),
                egui::Align2::LEFT_CENTER,
                &layer.site.level2_id,
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgba_unmultiplied(214, 242, 255, layer.opacity),
            );
        }
    }

    fn nearest_site_to_position(&self, rect: egui::Rect, position: egui::Pos2) -> Option<usize> {
        let (target_lon, target_lat) = self.screen_to_lon_lat(rect, position);
        nearest_site_index(&self.sites, target_lat, target_lon)
    }

    fn cursor_readout_at(
        &mut self,
        rect: egui::Rect,
        position: egui::Pos2,
    ) -> Option<CursorReadout> {
        let volume = self.volume.clone()?;
        let selected_cut = self.selected_cut;
        let selected_product = self.selected_product.clone();
        // Derived products are not read from a raw cut moment grid here: volume
        // products (CREF/ET/VIL) are tilt-independent, and per-cut derivatives
        // (shear/divergence) would need a costly per-hover recompute. Skip the
        // hover readout for all of them (the colorbar conveys the scale).
        if selected_product.derived().is_some() {
            return None;
        }
        let cut = volume.cuts.get(selected_cut)?;
        let base_moment = selected_product.base_moment();
        let source_grid = cut.moments.get(&base_moment)?;
        let dealiased_grid = selected_product
            .uses_dealiased_velocity()
            .then(|| self.dealiased_velocity_readout_grid(volume.as_ref(), selected_cut))
            .flatten();
        let grid = dealiased_grid.as_deref().unwrap_or(source_grid);
        let (radar_lat, radar_lon) = self.loaded_volume_location()?;
        let (target_lon, target_lat) = self.screen_to_lon_lat(rect, position);
        let lat_km = (target_lat - radar_lat) * 111.32;
        let lon_km = (target_lon - radar_lon) * 111.32 * radar_lat.to_radians().cos();
        let range_km = lat_km.hypot(lon_km);
        let max_range_km = grid_range_km(grid)?;
        if range_km > max_range_km {
            return None;
        }

        let mut azimuth_deg = lon_km.atan2(lat_km).to_degrees();
        if azimuth_deg < 0.0 {
            azimuth_deg += 360.0;
        }
        let (row, radial_index) = nearest_grid_row(cut, grid, azimuth_deg)?;
        let gate = gate_for_range(grid, range_km)?;
        let base_value = grid.scaled_value(row, gate)?;
        let raw = (!selected_product.uses_dealiased_velocity())
            .then(|| grid_raw_value(grid, row, gate))
            .flatten();
        let radial = cut.radials.get(radial_index)?;
        let value = if selected_product.is_storm_relative_velocity() {
            storm_relative_velocity_mps(base_value, radial.azimuth_deg, self.current_storm_motion())
        } else {
            base_value
        };
        let storm_motion = self.current_storm_motion();
        let vrot = velocity_vrot_probe(cut, grid, row, gate, &selected_product, storm_motion);
        let load_timing = self.load_timing;
        // Beam-center height from the gate's true slant range (4/3-Earth model;
        // Doviak & Zrnić 1993, eq. 2.28b), not the screen-derived ground range.
        let slant_range_m = grid.gate_range.first_gate_m as f64
            + gate as f64 * grid.gate_range.gate_spacing_m as f64;
        let height_above_radar_m =
            radar_core::beam_height_above_radar_m(slant_range_m, cut.elevation_deg as f64) as f32;
        Some(CursorReadout {
            site_id: volume.site.id.clone(),
            volume_time_utc: volume.volume_time.with_timezone(&Utc),
            product: selected_product.clone(),
            cut: selected_cut,
            value,
            base_value: selected_product
                .is_storm_relative_velocity()
                .then_some(base_value),
            vrot,
            raw,
            row,
            gate,
            gate_spacing_m: grid.gate_range.gate_spacing_m,
            range_km,
            azimuth_deg,
            source_azimuth_deg: radial.azimuth_deg,
            elevation_deg: cut.elevation_deg,
            height_above_radar_m,
            nyquist_velocity_mps: radial.nyquist_velocity_mps,
            realtime_volume_id: load_timing.and_then(|timing| timing.realtime_volume_id),
            realtime_last_chunk_id: load_timing.and_then(|timing| timing.realtime_last_chunk_id),
            realtime_last_chunk_type: load_timing
                .and_then(|timing| timing.realtime_last_chunk_type),
        })
    }

    fn lon_lat_to_screen(
        &self,
        rect: egui::Rect,
        longitude_deg: f32,
        latitude_deg: f32,
    ) -> egui::Pos2 {
        egui::pos2(
            rect.center().x
                + longitude_delta_deg(longitude_deg, self.map_center_lon)
                    * self.lon_pixels_per_degree(),
            rect.center().y - (latitude_deg - self.map_center_lat) * self.map_scale,
        )
    }

    fn screen_to_lon_lat(&self, rect: egui::Rect, position: egui::Pos2) -> (f32, f32) {
        (
            normalize_lon(
                self.map_center_lon + (position.x - rect.center().x) / self.lon_pixels_per_degree(),
            ),
            self.map_center_lat - (position.y - rect.center().y) / self.map_scale,
        )
    }

    fn visible_geo_bounds(&self, rect: egui::Rect) -> GeoBounds {
        let (west, north) = self.screen_to_lon_lat(rect, rect.left_top());
        let (east, south) = self.screen_to_lon_lat(rect, rect.right_bottom());
        GeoBounds {
            west: west.min(east),
            east: west.max(east),
            south: south.min(north).clamp(-85.0, 85.0),
            north: south.max(north).clamp(-85.0, 85.0),
        }
    }

    fn clamp_map_center(&mut self) {
        self.map_center_lon = normalize_lon(self.map_center_lon);
        self.map_center_lat = self.map_center_lat.clamp(-85.0, 85.0);
    }

    fn lon_screen_scale(&self) -> f32 {
        self.map_center_lat.to_radians().cos().abs().max(0.02)
    }

    fn lon_pixels_per_degree(&self) -> f32 {
        self.map_scale * self.lon_screen_scale()
    }
}

#[derive(Clone, Copy, Debug)]
struct GeoBounds {
    west: f32,
    south: f32,
    east: f32,
    north: f32,
}

#[derive(Clone, Copy)]
struct RegionalBasemapLayer {
    bounds: [f32; 4],
    admin_lines: &'static [basemap_data::BasemapLine],
    admin_labels: &'static [basemap_data::BasemapLabel],
    place_labels: &'static [basemap_data::BasemapLabel],
}

#[derive(Clone, Copy)]
struct PlaceLabelSet {
    labels: &'static [basemap_data::BasemapLabel],
    max_rank: u8,
    max_labels: usize,
}

const REGIONAL_BASEMAP_LAYERS: &[RegionalBasemapLayer] = &[
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_CANADA_BOUNDS,
        admin_lines: basemap_data::BASEMAP_CANADA_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_CANADA_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_CANADA_PLACE_LABELS,
    },
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_MEXICO_BOUNDS,
        admin_lines: basemap_data::BASEMAP_MEXICO_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_MEXICO_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_MEXICO_PLACE_LABELS,
    },
    RegionalBasemapLayer {
        bounds: basemap_data::BASEMAP_JAPAN_BOUNDS,
        admin_lines: basemap_data::BASEMAP_JAPAN_ADMIN_LINES,
        admin_labels: basemap_data::BASEMAP_JAPAN_ADMIN_LABELS,
        place_labels: basemap_data::BASEMAP_JAPAN_PLACE_LABELS,
    },
];

fn us_detail_visible(bounds: GeoBounds) -> bool {
    if !bounds.intersects_bbox(basemap_data::BASEMAP_US_BOUNDS) {
        return false;
    }
    BASEMAP_US_DETAIL_BOUNDS
        .iter()
        .any(|us_bounds| bounds.intersects_bbox(*us_bounds))
}

fn basemap_line_simplification_px(map_scale: f32) -> f32 {
    if map_scale < 24.0 {
        0.75
    } else if map_scale < 96.0 {
        0.45
    } else {
        0.0
    }
}

impl GeoBounds {
    fn expand(self, degrees: f32) -> Self {
        Self {
            west: self.west - degrees,
            south: self.south - degrees,
            east: self.east + degrees,
            north: self.north + degrees,
        }
    }

    fn contains(self, longitude_deg: f32, latitude_deg: f32) -> bool {
        longitude_deg >= self.west
            && longitude_deg <= self.east
            && latitude_deg >= self.south
            && latitude_deg <= self.north
    }

    fn intersects_bbox(self, bbox: [f32; 4]) -> bool {
        bbox[2] >= self.west
            && bbox[0] <= self.east
            && bbox[3] >= self.south
            && bbox[1] <= self.north
    }
}

/// Tiny keyed cache for draw geometry that is pure in the view state
/// (projected basemap polylines, tessellated hazard polygons). Idle repaints —
/// texture arrivals, hovers, animations — reuse entries instead of
/// reprojecting / re-ear-clipping every frame; any pan/zoom/content change
/// alters the key and falls through to a rebuild. Keys include the cell rect,
/// so multi-pane grids cache one entry per pane. LRU, capacity-capped.
struct ShapeCache<V> {
    entries: Vec<(u64, V)>,
    capacity: usize,
}

impl<V> ShapeCache<V> {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    fn get_or_insert_with(&mut self, key: u64, build: impl FnOnce() -> V) -> &V {
        if let Some(position) = self.entries.iter().position(|(k, _)| *k == key) {
            let entry = self.entries.remove(position);
            self.entries.push(entry);
        } else {
            if self.entries.len() >= self.capacity {
                self.entries.remove(0);
            }
            self.entries.push((key, build()));
        }
        &self.entries.last().expect("just pushed").1
    }
}

/// A loaded placefile overlay: URL + parsed content + refresh bookkeeping.
/// Fetch + parse run on background threads; the UI polls a channel.
struct PlacefileSlot {
    url: String,
    enabled: bool,
    data: Option<placefiles::Placefile>,
    /// Bumped on every successful install — exact shape-cache invalidation.
    generation: u64,
    next_refresh: Option<Instant>,
    status: String,
    receiver: Option<mpsc::Receiver<std::result::Result<placefiles::Placefile, String>>>,
}

impl PlacefileSlot {
    fn new(url: String, enabled: bool) -> Self {
        Self {
            url,
            enabled,
            data: None,
            generation: 0,
            next_refresh: None,
            status: "queued".to_owned(),
            receiver: None,
        }
    }
}

/// Cached placefile draw geometry: shapes plus live-drawn text labels
/// (position, text, size px, color).
struct PlacefileDrawList {
    shapes: Vec<egui::Shape>,
    labels: Vec<(egui::Pos2, String, f32, egui::Color32)>,
}

/// Cached hazard-overlay geometry: the (possibly ear-clipped) polygon shapes
/// plus label anchors (centroid, text, selected) drawn live each frame.
struct HazardOverlayShapes {
    shapes: Vec<egui::Shape>,
    labels: Vec<(egui::Pos2, String, bool)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextureKey {
    volume_ptr: usize,
    cut: usize,
    product: DisplayProduct,
    render_dealiased_velocity: bool,
    color_table_signature: u64,
    storm_motion_key: (i16, i16),
    hail_levels_key: (i16, i16),
    viewport: ViewportKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ViewportKey {
    width: u32,
    height: u32,
    radar_x_px: i32,
    radar_y_px: i32,
    km_per_px_x: i32,
    km_per_px_y: i32,
}

impl ViewportKey {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn anchored_radar_texture_rect(
    rect: egui::Rect,
    pixels_per_point: f32,
    rendered: ViewportKey,
    current: ViewportRasterOptions,
) -> egui::Rect {
    let pixels_per_point = pixels_per_point.max(1.0);
    let rendered_radar_x_px = rendered.radar_x_px as f32 / 8.0;
    let rendered_radar_y_px = rendered.radar_y_px as f32 / 8.0;
    let rendered_km_per_px_x = rendered.km_per_px_x as f32 / 1_000_000.0;
    let rendered_km_per_px_y = rendered.km_per_px_y as f32 / 1_000_000.0;
    let scale_x = positive_ratio(rendered_km_per_px_x, current.km_per_px_x);
    let scale_y = positive_ratio(rendered_km_per_px_y, current.km_per_px_y);
    let left_px = current.radar_x_px - rendered_radar_x_px * scale_x;
    let top_px = current.radar_y_px - rendered_radar_y_px * scale_y;
    egui::Rect::from_min_size(
        egui::pos2(
            rect.left() + left_px / pixels_per_point,
            rect.top() + top_px / pixels_per_point,
        ),
        egui::vec2(
            rendered.width as f32 * scale_x / pixels_per_point,
            rendered.height as f32 * scale_y / pixels_per_point,
        ),
    )
}

fn positive_ratio(numerator: f32, denominator: f32) -> f32 {
    if numerator.is_finite() && denominator.is_finite() && numerator > 0.0 && denominator > 0.0 {
        numerator / denominator
    } else {
        1.0
    }
}

fn freshness_ring_color(
    volume_time_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
    alpha: u8,
) -> egui::Color32 {
    let age_seconds = now_utc
        .signed_duration_since(volume_time_utc)
        .num_seconds()
        .max(0);
    let (start, end, t) = if age_seconds <= FRESH_RING_GREEN_SECONDS {
        ((65, 238, 104), (65, 238, 104), 0.0)
    } else if age_seconds <= FRESH_RING_YELLOW_SECONDS {
        (
            (65, 238, 104),
            (238, 218, 62),
            ratio_between(
                age_seconds,
                FRESH_RING_GREEN_SECONDS,
                FRESH_RING_YELLOW_SECONDS,
            ),
        )
    } else if age_seconds <= FRESH_RING_RED_SECONDS {
        (
            (238, 218, 62),
            (246, 76, 48),
            ratio_between(
                age_seconds,
                FRESH_RING_YELLOW_SECONDS,
                FRESH_RING_RED_SECONDS,
            ),
        )
    } else {
        ((246, 76, 48), (205, 34, 48), 1.0)
    };
    let (r, g, b) = lerp_rgb(start, end, t);
    egui::Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

fn ratio_between(value: i64, start: i64, end: i64) -> f32 {
    if end <= start {
        return 1.0;
    }
    ((value - start) as f32 / (end - start) as f32).clamp(0.0, 1.0)
}

fn lerp_rgb(start: (u8, u8, u8), end: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    (
        lerp_u8(start.0, end.0, t),
        lerp_u8(start.1, end.1, t),
        lerp_u8(start.2, end.2, t),
    )
}

fn lerp_u8(start: u8, end: u8, t: f32) -> u8 {
    (start as f32 + (end as f32 - start as f32) * t.clamp(0.0, 1.0)).round() as u8
}

fn site_location(site: &RadarSite) -> Option<(f32, f32)> {
    Some((site.latitude_deg?, site.longitude_deg?))
}

fn format_site_label(site: &RadarSite) -> String {
    match &site.name {
        Some(name) if !name.is_empty() => format!("{} {}", site.level2_id, name),
        _ => site.level2_id.clone(),
    }
}

fn range_radius_deg(latitude_deg: f32, range_km: f32) -> (f32, f32) {
    let lat_radius = range_km / 111.32;
    let lon_scale = (111.32 * latitude_deg.to_radians().cos().abs()).max(22.0);
    (lat_radius, range_km / lon_scale)
}

fn grid_range_km(grid: &MomentGrid) -> Option<f32> {
    let first_gate_m = grid.gate_range.first_gate_m.max(0) as f32;
    let gate_spacing_m = grid.gate_range.gate_spacing_m.max(0) as f32;
    let range_km = (first_gate_m + gate_spacing_m * grid.gate_range.gate_count as f32) / 1000.0;
    (range_km > 0.0).then_some(range_km)
}

fn gate_for_range(grid: &MomentGrid, range_km: f32) -> Option<usize> {
    let spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    let gate = ((range_km * 1000.0 - grid.gate_range.first_gate_m as f32) / spacing_m).round();
    if gate < 0.0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }
    Some(gate as usize)
}

fn nearest_grid_row(
    cut: &ElevationCut,
    grid: &MomentGrid,
    azimuth_deg: f32,
) -> Option<(usize, usize)> {
    let row_count = grid.radial_indices.len();
    if row_count == 0 {
        return None;
    }
    let threshold_deg = (360.0 / row_count as f32 * 0.55).clamp(0.35, 0.8);
    grid.radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, radial_index)| {
            let radial = cut.radials.get(*radial_index)?;
            let delta = angle_delta_deg(azimuth_deg, radial.azimuth_deg);
            (delta <= threshold_deg).then_some((row, *radial_index, delta))
        })
        .min_by(|left, right| left.2.total_cmp(&right.2))
        .map(|(row, radial_index, _)| (row, radial_index))
}

fn grid_raw_value(grid: &MomentGrid, row: usize, gate: usize) -> Option<u16> {
    let index = row
        .checked_mul(grid.gate_range.gate_count)?
        .checked_add(gate)?;
    match &grid.storage {
        MomentStorage::U8(values) => values.get(index).map(|value| u16::from(*value)),
        MomentStorage::U16(values) => values.get(index).copied(),
        MomentStorage::F32(_) => None,
    }
}

fn velocity_vrot_probe(
    cut: &ElevationCut,
    grid: &MomentGrid,
    center_row: usize,
    center_gate: usize,
    product: &DisplayProduct,
    storm_motion: StormMotion,
) -> Option<VrotProbe> {
    if product.base_moment() != MomentType::Velocity {
        return None;
    }
    if grid.gate_range.gate_count == 0 || grid.radial_indices.is_empty() {
        return None;
    }

    let row_count = grid.radial_indices.len();
    let gate_start = center_gate.saturating_sub(VROT_GATE_RADIUS);
    let gate_end = center_gate
        .saturating_add(VROT_GATE_RADIUS)
        .min(grid.gate_range.gate_count - 1);
    let mut inbound: Option<VelocitySample> = None;
    let mut outbound: Option<VelocitySample> = None;

    for row_delta in -(VROT_ROW_RADIUS as isize)..=(VROT_ROW_RADIUS as isize) {
        let row = (center_row as isize + row_delta).rem_euclid(row_count as isize) as usize;
        for gate in gate_start..=gate_end {
            let Some(sample) = velocity_sample(cut, grid, row, gate, product, storm_motion) else {
                continue;
            };
            if sample.value_mps < 0.0
                && inbound
                    .map(|current| sample.value_mps < current.value_mps)
                    .unwrap_or(true)
            {
                inbound = Some(sample);
            } else if sample.value_mps > 0.0
                && outbound
                    .map(|current| sample.value_mps > current.value_mps)
                    .unwrap_or(true)
            {
                outbound = Some(sample);
            }
        }
    }

    let inbound = inbound?;
    let outbound = outbound?;
    let delta_v_mps = outbound.value_mps - inbound.value_mps;
    let separation_km = (outbound.x_km - inbound.x_km).hypot(outbound.y_km - inbound.y_km);
    Some(VrotProbe {
        delta_v_mps,
        vrot_mps: delta_v_mps.abs() * 0.5,
        separation_km,
        inbound: inbound.vrot_gate(),
        outbound: outbound.vrot_gate(),
    })
}

#[derive(Clone, Copy)]
struct VelocitySample {
    row: usize,
    gate: usize,
    value_mps: f32,
    azimuth_deg: f32,
    x_km: f32,
    y_km: f32,
}

impl VelocitySample {
    fn vrot_gate(self) -> VrotGate {
        VrotGate {
            row: self.row,
            gate: self.gate,
            value_mps: self.value_mps,
            azimuth_deg: self.azimuth_deg,
        }
    }
}

fn velocity_sample(
    cut: &ElevationCut,
    grid: &MomentGrid,
    row: usize,
    gate: usize,
    product: &DisplayProduct,
    storm_motion: StormMotion,
) -> Option<VelocitySample> {
    let radial_index = *grid.radial_indices.get(row)?;
    let radial = cut.radials.get(radial_index)?;
    let base_velocity_mps = grid.scaled_value(row, gate)?;
    let value_mps = if product.is_storm_relative_velocity() {
        storm_relative_velocity_mps(base_velocity_mps, radial.azimuth_deg, storm_motion)
    } else {
        base_velocity_mps
    };
    let range_km = gate_center_range_km(grid, gate);
    let azimuth_rad = radial.azimuth_deg.to_radians();
    Some(VelocitySample {
        row,
        gate,
        value_mps,
        azimuth_deg: radial.azimuth_deg,
        x_km: range_km * azimuth_rad.sin(),
        y_km: range_km * azimuth_rad.cos(),
    })
}

fn gate_center_range_km(grid: &MomentGrid, gate: usize) -> f32 {
    let first_gate_m = grid.gate_range.first_gate_m.max(0) as f32;
    let spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    (first_gate_m + spacing_m * gate as f32) / 1000.0
}

fn default_hidden_hazard_families() -> BTreeSet<String> {
    DEFAULT_HIDDEN_HAZARD_FAMILIES
        .iter()
        .map(|family| (*family).to_owned())
        .collect()
}

fn parse_color_table_for_family(
    family: ColorTableFamily,
    name: &str,
    text: &str,
) -> Result<ColorTable, color_tables::ColorTableError> {
    match family {
        ColorTableFamily::Reflectivity
        | ColorTableFamily::Velocity
        | ColorTableFamily::SpectrumWidth => ColorTable::parse_stepped(name, text),
        ColorTableFamily::CorrelationCoefficient
        | ColorTableFamily::DifferentialReflectivity
        | ColorTableFamily::EchoTops
        | ColorTableFamily::Vil
        | ColorTableFamily::VilDensity
        | ColorTableFamily::HailSize
        | ColorTableFamily::AzimuthalShear
        | ColorTableFamily::DifferentialPhase
        | ColorTableFamily::SpecificDifferentialPhase
        | ColorTableFamily::Generic => ColorTable::parse(name, text),
    }
}

fn color_table_summary(table: &ColorTable) -> String {
    let range = table
        .stops()
        .first()
        .zip(table.stops().last())
        .map(|(first, last)| format!("range {:.1}..{:.1}", first.value, last.value))
        .unwrap_or_else(|| "range unavailable".to_owned());
    let mode = color_table_mode_summary(table);
    format!(
        "{} stops, {}, {}, {range}",
        table.stops().len(),
        mode,
        color_table_units_summary(table)
    )
}

fn color_table_mode_summary(table: &ColorTable) -> String {
    table
        .step_size()
        .map(|step| format!("{}, step {:.2}", table.sample_mode_label(), step))
        .unwrap_or_else(|| table.sample_mode_label().to_owned())
}

fn color_table_units_summary(table: &ColorTable) -> String {
    let Some(units) = table
        .units()
        .map(str::trim)
        .filter(|units| !units.is_empty())
    else {
        return "units unknown, assuming native".to_owned();
    };
    match units.to_ascii_lowercase().as_str() {
        "kt" | "kts" | "knot" | "knots" | "mph" | "mi/h" => {
            format!("units {units} -> m/s")
        }
        _ => format!("units {units}"),
    }
}

fn hazard_record_is_active_or_pending(record: &HazardRecord) -> bool {
    matches!(
        record.lifecycle_status.as_deref(),
        Some("Active") | Some("Pending") | None
    )
}

fn live_hazard_record_is_current(record: &HazardRecord) -> bool {
    matches!(
        record.lifecycle_status.as_deref(),
        Some("Active") | Some("Pending")
    )
}

fn active_alert_event_ids(records: &[HazardRecord]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| record.action == "ALERT")
        .filter(|record| live_hazard_record_is_current(record))
        .map(|record| base_hazard_event_id(&record.event_id).to_owned())
        .collect()
}

fn live_hazard_record_has_authoritative_source(
    record: &HazardRecord,
    active_alert_event_ids: &BTreeSet<String>,
) -> bool {
    !live_warning_requires_active_alert(record)
        || active_alert_event_ids.contains(base_hazard_event_id(&record.event_id))
}

fn live_warning_requires_active_alert(record: &HazardRecord) -> bool {
    matches!(
        record.event_family.as_str(),
        "tornado"
            | "severe thunderstorm"
            | "flash flood"
            | "flood"
            | "special marine"
            | "snow squall"
    )
}

fn base_hazard_event_id(event_id: &str) -> &str {
    event_id.split_once('#').map_or(event_id, |(base, _)| base)
}

fn fixed_height_scroll(
    ui: &mut egui::Ui,
    id: &'static str,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::LEFT),
        |ui| {
            ui.set_min_size(egui::vec2(width, height));
            egui::ScrollArea::vertical()
                .id_salt(id)
                .auto_shrink([false, false])
                .max_height(height)
                .show(ui, |ui| {
                    ui.set_width(width);
                    add_contents(ui);
                });
        },
    );
}

fn wrapped_label(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(text).wrap());
}

fn fixed_action_button(ui: &mut egui::Ui, label: &str, width: f32) -> egui::Response {
    ui.add_sized(
        egui::vec2(width, PANEL_BUTTON_HEIGHT),
        egui::Button::new(label),
    )
}

fn fixed_disabled_action_button(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    width: f32,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| fixed_action_button(ui, label, width))
        .inner
}

fn fixed_status_label(ui: &mut egui::Ui, text: &str, width: f32) -> egui::Response {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(width, PANEL_BUTTON_HEIGHT), egui::Sense::hover());
    ui.put(rect, egui::Label::new(text).truncate())
}

fn fixed_state_dot(ui: &mut egui::Ui, color: egui::Color32, hover_text: &str) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(14.0, PANEL_BUTTON_HEIGHT), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
    response.on_hover_text(hover_text);
}

fn layer_state_color(state: &str) -> egui::Color32 {
    match state {
        "loading" => egui::Color32::from_rgb(238, 218, 62),
        "live" => egui::Color32::from_rgb(65, 238, 104),
        _ => egui::Color32::from_rgb(106, 132, 154),
    }
}

fn hazard_record_detail_lines(record: &HazardRecord) -> Vec<String> {
    let mut lines = vec![
        record.label.clone(),
        record.event_id.clone(),
        format!("{} {}", record.office, record.action),
    ];
    if let Some(status) = &record.lifecycle_status {
        lines.push(status.clone());
    }
    if let Some(headline) = &record.headline {
        lines.push(headline.clone());
    }
    if let Some(area) = &record.area {
        lines.push(format!("Area {area}"));
    }
    if let Some(motion) = &record.motion {
        lines.push(format!("Motion {motion}"));
    }
    lines.extend(record.details.iter().cloned());
    if record.severity.is_some() || record.certainty.is_some() || record.urgency.is_some() {
        let severity = record.severity.as_deref().unwrap_or("-");
        let certainty = record.certainty.as_deref().unwrap_or("-");
        let urgency = record.urgency.as_deref().unwrap_or("-");
        lines.push(format!("{severity} / {certainty} / {urgency}"));
    }
    if let Some(source_url) = &record.source_url {
        lines.push(source_url.clone());
    }
    if let Some(tornado) = &record.tornado {
        lines.push(format!("Tornado {tornado}"));
    }
    if let Some(hail_inches) = record.hail_inches {
        lines.push(format!("Hail {:.2} in", hail_inches));
    }
    if let Some(wind_mph) = record.wind_mph {
        lines.push(format!("Wind {wind_mph} mph"));
    }
    if let Some(damage_threat) = &record.damage_threat {
        lines.push(format!("Damage {damage_threat}"));
    }
    if let Some(valid_start) = &record.valid_start {
        lines.push(format!("From {valid_start}"));
    }
    if let Some(valid_end) = &record.valid_end {
        lines.push(format!("Until {valid_end}"));
    }
    lines
}

fn angle_delta_deg(left: f32, right: f32) -> f32 {
    let delta = (left - right).abs().rem_euclid(360.0);
    delta.min(360.0 - delta)
}

fn moment_units(moment: &MomentType) -> &'static str {
    match moment {
        MomentType::Reflectivity => "dBZ",
        MomentType::Velocity | MomentType::SpectrumWidth => "m/s",
        MomentType::DifferentialReflectivity => "dB",
        MomentType::CorrelationCoefficient => "rho",
        MomentType::DifferentialPhase => "deg",
        MomentType::SpecificDifferentialPhase => "deg/km",
        MomentType::Unknown(_) => "",
    }
}

fn product_units(product: &DisplayProduct) -> &'static str {
    match product {
        DisplayProduct::Moment(moment) => moment_units(moment),
        DisplayProduct::DealiasedVelocity
        | DisplayProduct::StormRelativeVelocity
        | DisplayProduct::StormRelativeDealiasedVelocity => "m/s",
        DisplayProduct::Derived(d) => d.units(),
    }
}

/// Subdivide the map canvas into per-pane cell rects for a layout. `One`
/// returns exactly `[outer]` (no inset/gap) so single-pane stays byte-identical
/// to today. `TwoVertical` splits left|right; `FourGrid` is 2×2.
#[allow(dead_code)] // wired into map_canvas in the multi-pane grid step
fn pane_cell_rects(layout: PanelLayout, outer: egui::Rect, gap: f32) -> Vec<egui::Rect> {
    match layout {
        PanelLayout::One => vec![outer],
        PanelLayout::TwoVertical => {
            let w = ((outer.width() - gap) * 0.5).max(0.0);
            vec![
                egui::Rect::from_min_size(outer.min, egui::vec2(w, outer.height())),
                egui::Rect::from_min_size(
                    egui::pos2(outer.min.x + w + gap, outer.min.y),
                    egui::vec2(w, outer.height()),
                ),
            ]
        }
        PanelLayout::FourGrid => {
            let w = ((outer.width() - gap) * 0.5).max(0.0);
            let h = ((outer.height() - gap) * 0.5).max(0.0);
            let (x0, y0) = (outer.min.x, outer.min.y);
            let (x1, y1) = (x0 + w + gap, y0 + h + gap);
            let cell = egui::vec2(w, h);
            vec![
                egui::Rect::from_min_size(egui::pos2(x0, y0), cell),
                egui::Rect::from_min_size(egui::pos2(x1, y0), cell),
                egui::Rect::from_min_size(egui::pos2(x0, y1), cell),
                egui::Rect::from_min_size(egui::pos2(x1, y1), cell),
            ]
        }
    }
}

/// Compute a volume-derived product and wrap it in a render cache on the
/// lowest reflectivity tilt's geometry. Used by the render worker.
/// Background worker: detect rotation sites on the lowest velocity tilt and
/// geolocate them relative to the radar.
fn detect_rotation_markers_for_volume(
    volume: &RadarVolume,
    radar_lat: f32,
    radar_lon: f32,
) -> Vec<RotationMarker> {
    let cos_lat = radar_lat.to_radians().cos().max(0.05);
    detect_rotation_sites(volume)
        .into_iter()
        .map(|site| {
            let az = (site.azimuth_deg as f64).to_radians();
            let range_km = site.ground_range_m / 1000.0;
            let east_km = range_km * az.sin();
            let north_km = range_km * az.cos();
            RotationMarker {
                lon: radar_lon + (east_km as f32) / (111.32 * cos_lat),
                lat: radar_lat + (north_km as f32) / 111.32,
                vrot_mps: site.vrot_mps,
                rank: site.rank,
                strength: site.strength,
            }
        })
        .collect()
}

fn build_derived_moment_cache(
    volume: &RadarVolume,
    derived: DerivedProduct,
    selected_cut: usize,
    color_tables: &ColorTableSet,
    hail_levels_m: (f32, f32),
) -> std::result::Result<ViewportMomentCache, String> {
    let (geometry_cut, grid) = if derived.is_volume_wide() {
        // Volume products render on the lowest reflectivity tilt.
        let base_idx = volume
            .cuts
            .iter()
            .enumerate()
            .filter(|(_, c)| c.moments.contains_key(&MomentType::Reflectivity))
            .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
            .map(|(i, _)| i)
            .ok_or_else(|| "no reflectivity for derived product".to_owned())?;
        let grid = match derived {
            DerivedProduct::CompositeReflectivity => composite_reflectivity_grid(volume),
            DerivedProduct::EchoTops => echo_top_grid(volume, ECHO_TOP_THRESHOLD_DBZ),
            DerivedProduct::Vil => vil_grid(volume),
            DerivedProduct::VilDensity => vil_density_grid(volume),
            DerivedProduct::Mehs => mehs_grid(volume, hail_levels_m.0, hail_levels_m.1),
            DerivedProduct::AzimuthalShear | DerivedProduct::Divergence => {
                unreachable!("velocity derivatives are per-cut")
            }
        };
        (base_idx, grid)
    } else {
        // Per-cut derivative (azimuthal shear) on the selected tilt's velocity.
        let cut = volume
            .cuts
            .get(selected_cut)
            .ok_or_else(|| "selected cut missing for derived product".to_owned())?;
        let velocity = cut
            .moments
            .get(&MomentType::Velocity)
            .ok_or_else(|| "selected cut has no velocity for this product".to_owned())?;
        let grid = match derived {
            DerivedProduct::AzimuthalShear => azimuthal_shear_grid(cut, velocity),
            DerivedProduct::Divergence => radial_divergence_grid(cut, velocity),
            _ => unreachable!("only velocity derivatives are per-cut"),
        };
        (selected_cut, Some(grid))
    };
    let grid = grid.ok_or_else(|| format!("{} compute failed", derived.label()))?;
    ViewportMomentCache::new_derived(
        volume,
        geometry_cut,
        grid,
        derived.color_family(),
        color_tables,
    )
    .map_err(|err| err.to_string())
}

/// A sensible starting threshold per family when the user first enables the
/// clamp (REF: cut clear-air clutter; VEL/shear: cut near-zero noise).
fn default_display_threshold(family: ColorTableFamily) -> f32 {
    match family {
        ColorTableFamily::Reflectivity => 20.0,
        ColorTableFamily::Velocity => 5.0,
        ColorTableFamily::AzimuthalShear => 2.0,
        ColorTableFamily::SpectrumWidth => 2.0,
        _ => 0.0,
    }
}

/// Diverging families clamp on |value| so both strong inbound and outbound
/// survive a threshold; sequential families clamp from below.
fn family_threshold_is_symmetric(family: ColorTableFamily) -> bool {
    matches!(
        family,
        ColorTableFamily::Velocity | ColorTableFamily::AzimuthalShear
    )
}

fn format_cursor_readout(readout: &CursorReadout) -> String {
    let raw = readout
        .raw
        .map(|raw| raw.to_string())
        .unwrap_or_else(|| "-".to_owned());
    let units = product_units(&readout.product);
    let value = if units.is_empty() {
        format!("{:.1}", readout.value)
    } else {
        format!("{:.1} {units}", readout.value)
    };
    let base_value = readout
        .base_value
        .map(|value| format!(" VEL {:.1} m/s", value))
        .unwrap_or_default();
    let vrot = readout
        .vrot
        .map(|probe| {
            format!(
                " Vrot {:.1} m/s dV {:.1} sep {:.2} km in r{}/g{} {:05.1} {:.1} out r{}/g{} {:05.1} {:.1}",
                probe.vrot_mps,
                probe.delta_v_mps,
                probe.separation_km,
                probe.inbound.row,
                probe.inbound.gate,
                probe.inbound.azimuth_deg,
                probe.inbound.value_mps,
                probe.outbound.row,
                probe.outbound.gate,
                probe.outbound.azimuth_deg,
                probe.outbound.value_mps
            )
        })
        .unwrap_or_default();
    let nyquist = readout
        .nyquist_velocity_mps
        .map(|nyquist| format!(" Nyq {:.1} m/s", nyquist))
        .unwrap_or_default();
    let realtime = match (
        readout.realtime_volume_id,
        readout.realtime_last_chunk_id,
        readout.realtime_last_chunk_type,
    ) {
        (Some(volume_id), Some(chunk_id), Some(chunk_type)) => {
            format!(" rt v{volume_id:03} c{chunk_id:03} {chunk_type:?}")
        }
        (Some(volume_id), Some(chunk_id), None) => {
            format!(" rt v{volume_id:03} c{chunk_id:03}")
        }
        (Some(volume_id), None, _) => format!(" rt v{volume_id:03}"),
        _ => String::new(),
    };
    let height = format!(
        " hgt {:.0} m ({:.1} kft)",
        readout.height_above_radar_m,
        readout.height_above_radar_m * 0.003_280_84,
    );
    format!(
        "{} {} {} cut {} {} raw {} row {} gate {} @ {} m{}{} az {:05.1} src {:05.1} range {:.1} km elev {:.2}{}{}{}",
        readout.site_id,
        readout.volume_time_utc.format("%H:%M:%S"),
        readout.product.label(),
        readout.cut,
        value,
        raw,
        readout.row,
        readout.gate,
        readout.gate_spacing_m,
        base_value,
        vrot,
        readout.azimuth_deg,
        readout.source_azimuth_deg,
        readout.range_km,
        readout.elevation_deg,
        height,
        nyquist,
        realtime
    )
}

fn graticule_step(visible_degrees: f32) -> f32 {
    if visible_degrees > 140.0 {
        30.0
    } else if visible_degrees > 80.0 {
        20.0
    } else if visible_degrees > 40.0 {
        10.0
    } else if visible_degrees > 16.0 {
        5.0
    } else if visible_degrees > 6.0 {
        2.0
    } else if visible_degrees > 2.0 {
        1.0
    } else if visible_degrees > 0.7 {
        0.5
    } else {
        0.25
    }
}

fn world_place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 10.0 {
        None
    } else if map_scale < 28.0 {
        Some(0)
    } else if map_scale < 58.0 {
        Some(1)
    } else {
        None
    }
}

fn world_label_budget(map_scale: f32) -> usize {
    if map_scale < 28.0 { 18 } else { 36 }
}

fn place_label_rank(map_scale: f32) -> Option<u8> {
    if map_scale < 24.0 {
        None
    } else if map_scale < 42.0 {
        Some(0)
    } else if map_scale < 72.0 {
        Some(2)
    } else if map_scale < 130.0 {
        Some(4)
    } else if map_scale < 230.0 {
        Some(5)
    } else {
        Some(6)
    }
}

fn label_budget(map_scale: f32) -> usize {
    if map_scale < 72.0 {
        28
    } else if map_scale < 130.0 {
        54
    } else if map_scale < 230.0 {
        92
    } else {
        140
    }
}

fn left_label_rect(position: egui::Pos2, text: &str, font_size: f32) -> egui::Rect {
    let width = estimated_label_width(text, font_size);
    let height = font_size + 5.0;
    egui::Rect::from_min_size(
        egui::pos2(position.x, position.y - height * 0.5),
        egui::vec2(width, height),
    )
}

fn centered_label_rect(position: egui::Pos2, text: &str, font_size: f32) -> egui::Rect {
    let width = estimated_label_width(text, font_size);
    let height = font_size + 5.0;
    egui::Rect::from_center_size(position, egui::vec2(width, height))
}

fn estimated_label_width(text: &str, font_size: f32) -> f32 {
    text.chars().count() as f32 * font_size * 0.58 + 8.0
}

fn overlaps_any(existing: &[egui::Rect], candidate: egui::Rect) -> bool {
    existing.iter().any(|rect| rect.intersects(candidate))
}

fn draw_halo_text(
    painter: &egui::Painter,
    position: egui::Pos2,
    align: egui::Align2,
    text: &str,
    font: egui::FontId,
    text_color: egui::Color32,
    halo_color: egui::Color32,
) {
    for offset in [
        egui::vec2(-1.0, 0.0),
        egui::vec2(1.0, 0.0),
        egui::vec2(0.0, -1.0),
        egui::vec2(0.0, 1.0),
    ] {
        painter.text(position + offset, align, text, font.clone(), halo_color);
    }
    painter.text(position, align, text, font, text_color);
}

fn normalize_lon(longitude_deg: f32) -> f32 {
    let mut longitude_deg = longitude_deg;
    while longitude_deg > 180.0 {
        longitude_deg -= 360.0;
    }
    while longitude_deg < -180.0 {
        longitude_deg += 360.0;
    }
    longitude_deg
}

fn longitude_delta_deg(longitude_deg: f32, reference_longitude_deg: f32) -> f32 {
    normalize_lon(longitude_deg - reference_longitude_deg)
}

fn haversine_km(lat_a: f32, lon_a: f32, lat_b: f32, lon_b: f32) -> f32 {
    let earth_radius_km = 6371.0_f32;
    let d_lat = (lat_b - lat_a).to_radians();
    let d_lon = (lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let a = (d_lat / 2.0).sin().powi(2) + lat_a.cos() * lat_b.cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * earth_radius_km * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

fn nearest_site_index(sites: &[RadarSite], target_lat: f32, target_lon: f32) -> Option<usize> {
    sites
        .iter()
        .enumerate()
        .filter_map(|(index, site)| {
            let (latitude_deg, longitude_deg) = site_location(site)?;
            let distance_km = haversine_km(target_lat, target_lon, latitude_deg, longitude_deg);
            Some((index, distance_km))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, _)| index)
}

fn product_order(available: &std::collections::BTreeSet<MomentType>) -> Vec<DisplayProduct> {
    let mut ordered = Vec::new();
    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::SpectrumWidth,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
        MomentType::DifferentialPhase,
        MomentType::SpecificDifferentialPhase,
    ] {
        if available.contains(&moment) {
            if moment == MomentType::Velocity {
                ordered.push(DisplayProduct::Moment(MomentType::Velocity));
                ordered.push(DisplayProduct::DealiasedVelocity);
                ordered.push(DisplayProduct::StormRelativeVelocity);
                ordered.push(DisplayProduct::StormRelativeDealiasedVelocity);
            } else {
                ordered.push(DisplayProduct::Moment(moment));
            }
        }
    }
    for moment in available {
        let product = DisplayProduct::Moment(moment.clone());
        if !ordered.contains(&product) {
            ordered.push(product);
        }
    }
    ordered
}

fn global_displayable_products(volume: &RadarVolume) -> Vec<DisplayProduct> {
    let mut available = std::collections::BTreeSet::new();
    for cut_index in 0..volume.cuts.len() {
        available.extend(
            displayable_products(volume, cut_index)
                .into_iter()
                .map(|product| product.base_moment()),
        );
    }
    let mut products = product_order(&available);
    // product_order only knows raw moments; append derived products (CREF/ET/
    // VIL/AzShr/Div) here so they are reachable from the picker + keyboard cycle,
    // mirroring displayable_products. Offered when their source moment exists.
    for d in DerivedProduct::ALL {
        if available.contains(&d.base_moment()) {
            products.push(DisplayProduct::Derived(d));
        }
    }
    products
}

struct LiveHazardSourceMessage {
    source_label: String,
    result: Result<SpcMdLoad, String>,
}

fn load_live_hazard_overlay_with_preview<F>(
    query_time_utc: DateTime<Utc>,
    mut on_preview: F,
) -> Result<HazardOverlay, String>
where
    F: FnMut(HazardOverlay),
{
    let start = Instant::now();
    let mut records = Vec::new();
    let mut scanned_items = 0usize;
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;
    let mut source_labels = Vec::<String>::new();
    let mut first_error = None::<String>;

    thread::scope(|scope| {
        let (source_sender, source_receiver) = mpsc::channel::<LiveHazardSourceMessage>();

        let active_sender = source_sender.clone();
        scope.spawn(move || {
            send_live_hazard_source_load(
                active_sender,
                "NWS active alerts".to_owned(),
                "NWS active alert worker panicked",
                || load_weather_gov_active_alerts(query_time_utc),
            );
        });

        for &product_type in HOT_TEXT_PRODUCT_TYPES {
            let hot_text_sender = source_sender.clone();
            scope.spawn(move || {
                send_live_hazard_source_load(
                    hot_text_sender,
                    format!("NWS {product_type} text"),
                    "Hot NWS product type worker panicked",
                    || fetch_hot_text_product_type(product_type, query_time_utc),
                );
            });
        }

        let spc_md_sender = source_sender.clone();
        scope.spawn(move || {
            send_live_hazard_source_load(
                spc_md_sender,
                "SPC current MDs".to_owned(),
                "SPC MD fetch worker panicked",
                || load_spc_mesoscale_discussions(query_time_utc),
            );
        });

        drop(source_sender);

        for message in source_receiver {
            match message.result {
                Ok(mut load) => {
                    scanned_items += load.scanned_items;
                    parsed_items += load.parsed_items;
                    error_count += load.error_count;
                    if !load.records.is_empty() {
                        if !source_labels
                            .iter()
                            .any(|label| label == &message.source_label)
                        {
                            source_labels.push(message.source_label);
                        }
                        records.append(&mut load.records);
                        on_preview(build_live_hazard_overlay(
                            source_labels.join(" + "),
                            query_time_utc,
                            scanned_items,
                            parsed_items,
                            error_count,
                            start,
                            records.clone(),
                        ));
                    }
                }
                Err(err) => {
                    error_count += 1;
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
    });

    if records.is_empty()
        && let Some(err) = first_error
    {
        return Err(err);
    }

    Ok(build_live_hazard_overlay(
        "NWS active alerts + hot NWS text + SPC current MDs".to_owned(),
        query_time_utc,
        scanned_items,
        parsed_items,
        error_count,
        start,
        records,
    ))
}

fn send_live_hazard_source_load<F>(
    sender: mpsc::Sender<LiveHazardSourceMessage>,
    source_label: String,
    panic_message: &'static str,
    loader: F,
) where
    F: FnOnce() -> Result<SpcMdLoad, String>,
{
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(loader))
        .unwrap_or_else(|_| Err(panic_message.to_owned()));
    let _ = sender.send(LiveHazardSourceMessage {
        source_label,
        result,
    });
}

fn load_weather_gov_active_alerts(query_time_utc: DateTime<Utc>) -> Result<SpcMdLoad, String> {
    let text = data_source::fetch_text(ACTIVE_ALERTS_URL)
        .map_err(|err| format!("Live hazard fetch failed: {err}"))?;
    let collection: WeatherAlertFeatureCollection = serde_json::from_str(&text)
        .map_err(|err| format!("Live hazard JSON parse failed: {err}"))?;
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    for feature in &collection.features {
        match parse_weather_alert_feature(feature, query_time_utc) {
            Ok(mut feature_records) => {
                if !feature_records.is_empty() {
                    parsed_items += 1;
                    records.append(&mut feature_records);
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: collection.features.len(),
        parsed_items,
        error_count,
        records,
    })
}

fn build_live_hazard_overlay(
    source_label: String,
    query_time_utc: DateTime<Utc>,
    scanned_items: usize,
    parsed_items: usize,
    error_count: usize,
    start: Instant,
    mut records: Vec<HazardRecord>,
) -> HazardOverlay {
    let active_alert_event_ids = active_alert_event_ids(&records);
    dedupe_hazard_records(&mut records);
    records.retain(|record| {
        live_hazard_record_is_current(record)
            && live_hazard_record_has_authoritative_source(record, &active_alert_event_ids)
    });
    sort_hazard_records(&mut records);

    HazardOverlay {
        source_label,
        query_time_utc: Some(format_utc_seconds(query_time_utc)),
        scanned_items,
        parsed_items,
        polygon_records: records.len(),
        error_count,
        load_ms: start.elapsed().as_secs_f32() * 1000.0,
        records,
    }
}

fn load_hazard_overlay_from_path(
    path: &Path,
    query_time_utc: Option<DateTime<Utc>>,
) -> Result<HazardOverlay, String> {
    let start = Instant::now();
    let files = collect_hazard_files(path)?;
    let mut records = Vec::new();
    let mut parsed_files = 0usize;
    let mut errors = 0usize;

    for file in &files {
        match std::fs::read(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let before = records.len();
                records.extend(parse_hazard_records_from_text(file, &text, query_time_utc));
                if records.len() > before {
                    parsed_files += 1;
                }
            }
            Err(_) => {
                errors += 1;
            }
        }
    }

    sort_hazard_records(&mut records);

    Ok(HazardOverlay {
        source_label: path.display().to_string(),
        query_time_utc: query_time_utc.map(format_utc_seconds),
        scanned_items: files.len(),
        parsed_items: parsed_files,
        polygon_records: records.len(),
        error_count: errors,
        load_ms: start.elapsed().as_secs_f32() * 1000.0,
        records,
    })
}

fn sort_hazard_records(records: &mut [HazardRecord]) {
    records.sort_by(|left, right| {
        hazard_family_order(&left.event_family)
            .cmp(&hazard_family_order(&right.event_family))
            .then_with(|| left.valid_end.cmp(&right.valid_end))
            .then_with(|| left.label.cmp(&right.label))
    });
}

fn selected_hazard_index_for_event_id(
    records: &[HazardRecord],
    selected_event_id: Option<&str>,
) -> Option<usize> {
    let selected_event_id = selected_event_id?;
    records
        .iter()
        .position(|record| record.event_id == selected_event_id)
}

fn hazard_overlay_records_match(left: &HazardOverlay, right: &HazardOverlay) -> bool {
    left.records == right.records
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HazardOverlayChange {
    added: usize,
    removed: usize,
    geometry_changed: usize,
}

impl HazardOverlayChange {
    fn is_empty(self) -> bool {
        self.added == 0 && self.removed == 0 && self.geometry_changed == 0
    }

    fn status_text(self) -> String {
        format!(
            "+{} -{} {} moved",
            self.added, self.removed, self.geometry_changed
        )
    }
}

fn hazard_overlay_change(left: &HazardOverlay, right: &HazardOverlay) -> HazardOverlayChange {
    let left_records = left
        .records
        .iter()
        .map(|record| (record.event_id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let right_records = right
        .records
        .iter()
        .map(|record| (record.event_id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut change = HazardOverlayChange::default();

    for (event_id, right_record) in &right_records {
        match left_records.get(event_id) {
            Some(left_record) if hazard_record_geometry_matches(left_record, right_record) => {}
            Some(_) => change.geometry_changed += 1,
            None => change.added += 1,
        }
    }
    for event_id in left_records.keys() {
        if !right_records.contains_key(event_id) {
            change.removed += 1;
        }
    }

    change
}

fn hazard_record_geometry_matches(left: &HazardRecord, right: &HazardRecord) -> bool {
    left.bbox == right.bbox && left.points == right.points
}

fn dedupe_hazard_records(records: &mut Vec<HazardRecord>) {
    let mut unique = Vec::<HazardRecord>::with_capacity(records.len());
    for record in records.drain(..) {
        if let Some(existing) = unique
            .iter_mut()
            .find(|existing| existing.event_id == record.event_id)
        {
            *existing = merge_duplicate_hazard_record(existing, &record);
        } else {
            unique.push(record);
        }
    }
    *records = unique;
}

fn merge_duplicate_hazard_record(
    existing: &HazardRecord,
    candidate: &HazardRecord,
) -> HazardRecord {
    let detail_source =
        if hazard_record_detail_score(candidate) >= hazard_record_detail_score(existing) {
            candidate
        } else {
            existing
        };
    let geometry_source = if existing.action == "ALERT" {
        existing
    } else if candidate.action == "ALERT" {
        candidate
    } else {
        detail_source
    };
    let fallback_source = if std::ptr::eq(detail_source, existing) {
        candidate
    } else {
        existing
    };

    let mut merged = detail_source.clone();
    merged.points = geometry_source.points.clone();
    merged.bbox = geometry_source.bbox;
    if merged.source_url.is_none() {
        merged.source_url = fallback_source.source_url.clone();
    }
    if merged.headline.is_none() {
        merged.headline = fallback_source.headline.clone();
    }
    if merged.area.is_none() {
        merged.area = fallback_source.area.clone();
    }
    if merged.motion.is_none() {
        merged.motion = fallback_source.motion.clone();
    }
    if merged.valid_start.is_none() {
        merged.valid_start = fallback_source.valid_start.clone();
    }
    if merged.valid_end.is_none() {
        merged.valid_end = fallback_source.valid_end.clone();
    }
    if merged.lifecycle_status.is_none() {
        merged.lifecycle_status = fallback_source.lifecycle_status.clone();
    }
    merged.lifecycle_status = preferred_lifecycle_status(
        existing.lifecycle_status.as_deref(),
        candidate.lifecycle_status.as_deref(),
    );
    if merged.severity.is_none() {
        merged.severity = fallback_source.severity.clone();
    }
    if merged.certainty.is_none() {
        merged.certainty = fallback_source.certainty.clone();
    }
    if merged.urgency.is_none() {
        merged.urgency = fallback_source.urgency.clone();
    }
    if merged.tornado.is_none() {
        merged.tornado = fallback_source.tornado.clone();
    }
    if merged.hail_inches.is_none() {
        merged.hail_inches = fallback_source.hail_inches;
    }
    if merged.wind_mph.is_none() {
        merged.wind_mph = fallback_source.wind_mph;
    }
    if merged.damage_threat.is_none() {
        merged.damage_threat = fallback_source.damage_threat.clone();
    }
    merged
}

fn preferred_lifecycle_status(left: Option<&str>, right: Option<&str>) -> Option<String> {
    [left, right]
        .into_iter()
        .flatten()
        .max_by_key(|status| lifecycle_status_priority(status))
        .map(str::to_owned)
}

fn lifecycle_status_priority(status: &str) -> u8 {
    match status {
        "Active" => 4,
        "Pending" => 3,
        "Canceled" => 1,
        "Expired" => 0,
        _ => 2,
    }
}

fn hazard_record_detail_score(record: &HazardRecord) -> usize {
    usize::from(record.source_url.is_some())
        + usize::from(record.area.is_some())
        + usize::from(record.motion.is_some())
        + record.details.len()
        + usize::from(record.headline.is_some())
        + usize::from(record.tornado.is_some())
        + usize::from(record.hail_inches.is_some())
        + usize::from(record.wind_mph.is_some())
        + usize::from(record.damage_threat.is_some())
}

struct SpcMdLoad {
    scanned_items: usize,
    parsed_items: usize,
    error_count: usize,
    records: Vec<HazardRecord>,
}

fn fetch_hot_text_product_type(
    product_type: &str,
    query_time_utc: DateTime<Utc>,
) -> Result<SpcMdLoad, String> {
    let url = format!("{NWS_PRODUCT_API_BASE_URL}/{product_type}");
    let text = data_source::fetch_text(&url)
        .map_err(|err| format!("NWS {product_type} product list fetch failed: {err}"))?;
    let collection: NwsProductCollection = serde_json::from_str(&text)
        .map_err(|err| format!("NWS {product_type} product list parse failed: {err}"))?;
    let summaries = select_hot_text_summaries(collection.products, query_time_utc);
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    let detail_results = thread::scope(|scope| {
        let workers = summaries
            .iter()
            .map(|summary| scope.spawn(move || fetch_nws_product_detail(summary)))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err("NWS product detail worker panicked".to_owned()))
            })
            .collect::<Vec<_>>()
    });

    for (summary, detail_result) in summaries.iter().zip(detail_results) {
        match detail_result {
            Ok(detail) => {
                let before = records.len();
                let mut parsed = parse_hazard_records_from_text(
                    Path::new(product_type),
                    &detail.product_text,
                    Some(query_time_utc),
                );
                for record in &mut parsed {
                    record.source_url = Some(summary.url.clone());
                    if record.headline.is_none() {
                        record.headline = Some(detail.product_name.clone());
                    }
                    record.details.push(format!(
                        "Issued {}",
                        format_utc_seconds(detail.issuance_time)
                    ));
                }
                records.append(&mut parsed);
                if records.len() > before {
                    parsed_items += 1;
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: summaries.len(),
        parsed_items,
        error_count,
        records,
    })
}

fn select_hot_text_summaries(
    mut products: Vec<NwsProductSummary>,
    query_time_utc: DateTime<Utc>,
) -> Vec<NwsProductSummary> {
    products.sort_by_key(|product| std::cmp::Reverse(product.issuance_time));
    let recent_start =
        query_time_utc - chrono::Duration::minutes(HOT_TEXT_PRODUCTS_RECENT_WINDOW_MINUTES);
    let near_future = query_time_utc + chrono::Duration::minutes(5);
    let mut selected = Vec::with_capacity(HOT_TEXT_PRODUCTS_MIN_PER_TYPE);

    for (index, summary) in products.into_iter().enumerate() {
        let is_recent =
            summary.issuance_time >= recent_start && summary.issuance_time <= near_future;
        if index < HOT_TEXT_PRODUCTS_MIN_PER_TYPE || is_recent {
            selected.push(summary);
            if selected.len() >= HOT_TEXT_PRODUCTS_MAX_PER_TYPE {
                break;
            }
        } else if summary.issuance_time < recent_start {
            break;
        }
    }

    selected
}

fn fetch_nws_product_detail(summary: &NwsProductSummary) -> Result<NwsProductDetail, String> {
    if let Ok(cache) = nws_product_detail_cache().lock()
        && let Some(detail) = cache.get(&summary.url).cloned()
    {
        return Ok(detail);
    }

    let text = data_source::fetch_text(&summary.url)
        .map_err(|err| format!("NWS product detail fetch failed: {err}"))?;
    let detail: NwsProductDetail = serde_json::from_str(&text)
        .map_err(|err| format!("NWS product detail parse failed: {err}"))?;
    if let Ok(mut cache) = nws_product_detail_cache().lock() {
        if cache.len() >= HOT_TEXT_DETAIL_CACHE_MAX
            && let Some(first_key) = cache.keys().next().cloned()
        {
            cache.remove(&first_key);
        }
        cache.insert(summary.url.clone(), detail.clone());
    }
    Ok(detail)
}

fn nws_product_detail_cache() -> &'static Mutex<BTreeMap<String, NwsProductDetail>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, NwsProductDetail>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn load_spc_mesoscale_discussions(query_time_utc: DateTime<Utc>) -> Result<SpcMdLoad, String> {
    let index_html = data_source::fetch_text(SPC_MD_INDEX_URL)
        .map_err(|err| format!("SPC MD index fetch failed: {err}"))?;
    let links = spc_md_product_links(&index_html);
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    for url in &links {
        match data_source::fetch_text(url) {
            Ok(html) => {
                if let Some(record) = parse_spc_md_product_page(url, &html, query_time_utc) {
                    parsed_items += 1;
                    records.push(record);
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: links.len(),
        parsed_items,
        error_count,
        records,
    })
}

fn spc_md_product_links(index_html: &str) -> Vec<String> {
    let mut links = Vec::new();
    for part in index_html.split("href=\"").skip(1) {
        let Some(end) = part.find('"') else {
            continue;
        };
        let href = &part[..end];
        let url = if href.starts_with("/products/md/md") && href.ends_with(".html") {
            Some(format!("{SPC_PRODUCT_BASE_URL}{href}"))
        } else if href.starts_with("md") && href.ends_with(".html") {
            Some(format!("{SPC_MD_INDEX_URL}{href}"))
        } else {
            None
        };
        if let Some(url) = url
            && !links.contains(&url)
        {
            links.push(url);
        }
    }
    links
}

fn parse_spc_md_product_page(
    source_url: &str,
    html: &str,
    _query_time_utc: DateTime<Utc>,
) -> Option<HazardRecord> {
    let text = extract_preformatted_text(html).unwrap_or(html);
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let points = parse_lat_lon_points(&lines);
    if points.len() < 3 {
        return None;
    }
    let upper = text.to_ascii_uppercase();
    let number = first_number_after(&upper, "MESOSCALE DISCUSSION")?;
    let label = format!("MD {number}");
    let area = strip_prefixed_line(&lines, "Areas affected...");
    let concerning = strip_prefixed_line(&lines, "Concerning...");
    let valid = find_prefixed_line(&lines, "Valid ");
    let watch_probability = strip_prefixed_line(&lines, "Probability of Watch Issuance...");
    let peak_wind = find_prefixed_line(&lines, "MOST PROBABLE PEAK WIND GUST...");
    let peak_hail = find_prefixed_line(&lines, "MOST PROBABLE PEAK HAIL SIZE...");
    let mut details = Vec::new();
    if let Some(valid) = valid {
        details.push(valid);
    }
    if let Some(watch_probability) = watch_probability {
        details.push(format!("Watch issuance {watch_probability}"));
    }
    if let Some(peak_wind) = peak_wind {
        details.push(peak_wind);
    }
    if let Some(peak_hail) = peak_hail {
        details.push(peak_hail);
    }

    Some(HazardRecord {
        event_id: format!("spc-md-{number}"),
        label,
        event_family: "mesoscale discussion".to_owned(),
        action: "SPC".to_owned(),
        lifecycle_status: Some("Active".to_owned()),
        office: "SPC".to_owned(),
        headline: concerning,
        source_url: Some(source_url.to_owned()),
        area,
        motion: None,
        details,
        valid_start: None,
        valid_end: None,
        severity: None,
        certainty: None,
        urgency: None,
        tornado: None,
        hail_inches: None,
        wind_mph: None,
        damage_threat: None,
        bbox: hazard_bbox(&points),
        points,
    })
}

fn extract_preformatted_text(html: &str) -> Option<&str> {
    let start = html.find("<pre>")? + "<pre>".len();
    let end = html[start..].find("</pre>")? + start;
    Some(html[start..end].trim())
}

fn strip_prefixed_line(lines: &[&str], prefix: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

#[derive(Debug, Deserialize)]
struct NwsProductCollection {
    #[serde(rename = "@graph", default)]
    products: Vec<NwsProductSummary>,
}

#[derive(Clone, Debug, Deserialize)]
struct NwsProductSummary {
    #[serde(rename = "@id")]
    url: String,
    #[serde(rename = "issuanceTime")]
    issuance_time: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
struct NwsProductDetail {
    #[serde(rename = "issuanceTime")]
    issuance_time: DateTime<Utc>,
    #[serde(rename = "productName")]
    product_name: String,
    #[serde(rename = "productText")]
    product_text: String,
}

#[derive(Debug, Deserialize)]
struct WeatherAlertFeatureCollection {
    #[serde(default)]
    features: Vec<WeatherAlertFeature>,
}

#[derive(Debug, Deserialize)]
struct WeatherAlertFeature {
    id: Option<String>,
    #[serde(rename = "@id")]
    at_id: Option<String>,
    geometry: Option<WeatherAlertGeometry>,
    #[serde(default)]
    properties: WeatherAlertProperties,
}

#[derive(Debug, Deserialize)]
struct WeatherAlertGeometry {
    #[serde(rename = "type")]
    geometry_type: String,
    coordinates: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct WeatherAlertProperties {
    id: Option<String>,
    #[serde(rename = "@id")]
    at_id: Option<String>,
    event: Option<String>,
    headline: Option<String>,
    description: Option<String>,
    #[serde(rename = "areaDesc")]
    area_desc: Option<String>,
    #[serde(rename = "senderName")]
    sender_name: Option<String>,
    severity: Option<String>,
    certainty: Option<String>,
    urgency: Option<String>,
    effective: Option<String>,
    onset: Option<String>,
    expires: Option<String>,
    ends: Option<String>,
    #[serde(default)]
    parameters: BTreeMap<String, Vec<String>>,
}

fn parse_weather_alert_feature(
    feature: &WeatherAlertFeature,
    query_time_utc: DateTime<Utc>,
) -> Result<Vec<HazardRecord>, String> {
    let Some(geometry) = &feature.geometry else {
        return Ok(Vec::new());
    };
    let rings = weather_alert_geometry_rings(geometry)?;
    let event = feature
        .properties
        .event
        .as_deref()
        .unwrap_or("Weather Alert");
    let event_family = weather_alert_family(event);
    let tags = parse_weather_alert_tags(&feature.properties.parameters);
    let valid_start = parse_alert_time(
        feature
            .properties
            .onset
            .as_deref()
            .or(feature.properties.effective.as_deref()),
    );
    let valid_end = parse_alert_time(
        feature
            .properties
            .ends
            .as_deref()
            .or(feature.properties.expires.as_deref()),
    );
    let lifecycle_status =
        hazard_lifecycle_status("ALERT", valid_start, valid_end, Some(query_time_utc));
    let valid_start_text = valid_start.map(format_utc_seconds);
    let valid_end_text = valid_end.map(format_utc_seconds);
    let label = weather_alert_label(event, &event_family, &feature.properties.parameters, &tags);
    let event_id = feature
        .properties
        .parameters
        .get("VTEC")
        .and_then(|values| values.first())
        .and_then(|vtec| parse_vtec_alert_event_id(vtec))
        .or_else(|| {
            feature
                .properties
                .id
                .as_deref()
                .or(feature.id.as_deref())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| event.to_owned());
    let office = feature
        .properties
        .sender_name
        .clone()
        .or_else(|| weather_alert_parameter(&feature.properties.parameters, "AWIPSidentifier"))
        .unwrap_or_else(|| "NWS".to_owned());
    let headline = feature
        .properties
        .headline
        .clone()
        .or_else(|| weather_alert_parameter(&feature.properties.parameters, "NWSheadline"))
        .or_else(|| feature.properties.area_desc.clone())
        .or_else(|| feature.properties.description.clone());
    let source_url = weather_alert_source_url(feature);
    let area = feature.properties.area_desc.clone();
    let motion = weather_alert_parameter(&feature.properties.parameters, "eventMotionDescription");
    let label_count = rings.len();

    Ok(rings
        .into_iter()
        .enumerate()
        .filter(|(_, points)| points.len() >= 3)
        .map(|(index, points)| HazardRecord {
            event_id: if label_count > 1 {
                format!("{event_id}#{index}")
            } else {
                event_id.clone()
            },
            label: if label_count > 1 {
                format!("{} {}", label, index + 1)
            } else {
                label.clone()
            },
            event_family: event_family.clone(),
            action: "ALERT".to_owned(),
            lifecycle_status: lifecycle_status.clone(),
            office: office.clone(),
            headline: headline.clone(),
            source_url: source_url.clone(),
            area: area.clone(),
            motion: motion.clone(),
            details: Vec::new(),
            valid_start: valid_start_text.clone(),
            valid_end: valid_end_text.clone(),
            severity: feature.properties.severity.clone(),
            certainty: feature.properties.certainty.clone(),
            urgency: feature.properties.urgency.clone(),
            tornado: tags.tornado.clone(),
            hail_inches: tags.hail_inches,
            wind_mph: tags.wind_mph,
            damage_threat: tags.damage_threat.clone(),
            bbox: hazard_bbox(&points),
            points,
        })
        .collect())
}

fn weather_alert_geometry_rings(
    geometry: &WeatherAlertGeometry,
) -> Result<Vec<Vec<HazardPoint>>, String> {
    match geometry.geometry_type.as_str() {
        "Polygon" => Ok(parse_polygon_coordinate_value(&geometry.coordinates)
            .into_iter()
            .take(1)
            .collect()),
        "MultiPolygon" => {
            let mut polygons = Vec::new();
            let Some(multi_polygon) = geometry.coordinates.as_array() else {
                return Err("multipolygon coordinates are not an array".to_owned());
            };
            for polygon in multi_polygon {
                if let Some(outer_ring) = parse_polygon_coordinate_value(polygon).into_iter().next()
                {
                    polygons.push(outer_ring);
                }
            }
            Ok(polygons)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_polygon_coordinate_value(value: &serde_json::Value) -> Vec<Vec<HazardPoint>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ring| {
            let mut points = ring
                .as_array()?
                .iter()
                .filter_map(|coordinate| {
                    let pair = coordinate.as_array()?;
                    let lon = pair.first()?.as_f64()? as f32;
                    let lat = pair.get(1)?.as_f64()? as f32;
                    Some(HazardPoint { lon, lat })
                })
                .collect::<Vec<_>>();
            if points.len() > 1
                && let (Some(first), Some(last)) = (points.first(), points.last())
                && (first.lon - last.lon).abs() <= f32::EPSILON
                && (first.lat - last.lat).abs() <= f32::EPSILON
            {
                points.pop();
            }
            (points.len() >= 3).then_some(points)
        })
        .collect()
}

fn weather_alert_family(event: &str) -> String {
    let upper = event.to_ascii_uppercase();
    if upper.contains("TORNADO") {
        "tornado".to_owned()
    } else if upper.contains("SEVERE THUNDERSTORM") {
        "severe thunderstorm".to_owned()
    } else if upper.contains("FLASH FLOOD") {
        "flash flood".to_owned()
    } else if upper.contains("FLOOD") {
        "flood".to_owned()
    } else if upper.contains("SPECIAL MARINE") {
        "special marine".to_owned()
    } else if upper.contains("SNOW SQUALL") {
        "snow squall".to_owned()
    } else if upper.contains("WATCH") {
        "watch".to_owned()
    } else if upper.contains("SPECIAL WEATHER") {
        "special weather".to_owned()
    } else {
        "alert".to_owned()
    }
}

fn parse_weather_alert_tags(parameters: &BTreeMap<String, Vec<String>>) -> ParsedWarningTags {
    ParsedWarningTags {
        tornado: weather_alert_parameter(parameters, "tornadoDetection"),
        hail_inches: weather_alert_parameter(parameters, "maxHailSize")
            .as_deref()
            .and_then(parse_leading_float),
        wind_mph: weather_alert_parameter(parameters, "maxWindGust")
            .as_deref()
            .and_then(parse_leading_u16),
        damage_threat: weather_alert_parameter(parameters, "tornadoDamageThreat")
            .or_else(|| weather_alert_parameter(parameters, "thunderstormDamageThreat")),
    }
}

fn weather_alert_parameter(
    parameters: &BTreeMap<String, Vec<String>>,
    key: &str,
) -> Option<String> {
    parameters
        .get(key)
        .and_then(|values| values.first())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn weather_alert_source_url(feature: &WeatherAlertFeature) -> Option<String> {
    [
        feature.properties.at_id.as_deref(),
        feature.at_id.as_deref(),
        feature.id.as_deref(),
        feature.properties.id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.starts_with("http://") || value.starts_with("https://"))
    .map(str::to_owned)
}

fn parse_alert_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|time| time.with_timezone(&Utc))
}

fn format_utc_seconds(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn weather_alert_label(
    _event: &str,
    event_family: &str,
    parameters: &BTreeMap<String, Vec<String>>,
    tags: &ParsedWarningTags,
) -> String {
    if let Some(vtec) = weather_alert_parameter(parameters, "VTEC")
        && let Some((phenomenon, event_tracking_number)) = parse_vtec_alert_identity(&vtec)
    {
        return hazard_label(
            hazard_family_from_phenomenon(&phenomenon),
            &event_tracking_number,
            tags,
        );
    }
    let prefix = match event_family {
        "tornado" => "TOR",
        "severe thunderstorm" => "SVR",
        "flash flood" => "FFW",
        "flood" => "FLW",
        "special marine" => "SMW",
        "snow squall" => "SQW",
        "watch" => "WATCH",
        "special weather" => "SPS",
        _ => "ALERT",
    };
    if let Some(tornado) = &tags.tornado {
        format!("{prefix} {tornado}")
    } else {
        prefix.to_owned()
    }
}

fn parse_vtec_alert_identity(vtec: &str) -> Option<(String, String)> {
    let parts = vtec.trim_matches('/').split('.').collect::<Vec<_>>();
    if parts.len() < 6 || parts.first().copied() != Some("O") {
        return None;
    }
    Some((parts.get(3)?.to_string(), parts.get(5)?.to_string()))
}

fn parse_vtec_alert_event_id(vtec: &str) -> Option<String> {
    let parts = vtec.trim_matches('/').split('.').collect::<Vec<_>>();
    if parts.len() < 6 || parts.first().copied() != Some("O") {
        return None;
    }
    Some(format!(
        "{}.{}.{}.{}",
        parts.get(2)?,
        parts.get(3)?,
        parts.get(4)?,
        parts.get(5)?
    ))
}

fn collect_hazard_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(format!("Hazard path not found: {}", path.display()));
    }

    let mut files = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("Cannot read hazard dir {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| err.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn parse_hazard_records_from_text(
    path: &Path,
    text: &str,
    query_time_utc: Option<DateTime<Utc>>,
) -> Vec<HazardRecord> {
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let heading = lines
        .iter()
        .find(|line| looks_like_wmo_heading(line.trim()))
        .map(|line| line.trim().to_owned());
    let awips_id = heading
        .as_deref()
        .and_then(|heading| lines.iter().position(|line| line.trim() == heading))
        .and_then(|index| lines.get(index + 1))
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty());

    let mut records = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let Some(vtec) = parse_warning_vtec_line(line) else {
            continue;
        };
        let segment_end = lines
            .iter()
            .enumerate()
            .skip(line_index + 1)
            .find_map(|(index, candidate)| (candidate.trim() == "$$").then_some(index))
            .unwrap_or(lines.len());
        let segment = &lines[line_index..segment_end];
        let points = parse_lat_lon_points(segment);
        if points.len() < 3 {
            continue;
        }
        let bbox = hazard_bbox(&points);
        let tags = parse_warning_tags(segment);
        let event_family = hazard_family_from_phenomenon(&vtec.phenomenon).to_owned();
        let lifecycle_status =
            hazard_lifecycle_status(&vtec.action, vtec.start_time, vtec.end_time, query_time_utc);
        let label = hazard_label(&event_family, &vtec.event_tracking_number, &tags);
        records.push(HazardRecord {
            event_id: format!(
                "{}.{}.{}.{}",
                vtec.office, vtec.phenomenon, vtec.significance, vtec.event_tracking_number
            ),
            label,
            event_family,
            action: vtec.action,
            lifecycle_status,
            office: vtec.office,
            headline: find_warning_headline(segment)
                .or(awips_id.clone())
                .or(heading.clone()),
            source_url: None,
            area: None,
            motion: find_prefixed_line(segment, "TIME...MOT...LOC"),
            details: Vec::new(),
            valid_start: vtec.start_time.map(format_utc_seconds),
            valid_end: vtec.end_time.map(format_utc_seconds),
            severity: None,
            certainty: None,
            urgency: None,
            tornado: tags.tornado,
            hail_inches: tags.hail_inches,
            wind_mph: tags.wind_mph,
            damage_threat: tags.damage_threat,
            points,
            bbox,
        });
    }
    if records.is_empty()
        && let Some(record) = parse_generic_lat_lon_hazard(path, &lines, heading, awips_id)
    {
        records.push(record);
    }
    records
}

fn parse_generic_lat_lon_hazard(
    path: &Path,
    lines: &[&str],
    heading: Option<String>,
    awips_id: Option<String>,
) -> Option<HazardRecord> {
    let points = parse_lat_lon_points(lines);
    if points.len() < 3 {
        return None;
    }
    let text = lines.join("\n").to_ascii_uppercase();
    let event_family = classify_generic_hazard_family(&text, awips_id.as_deref());
    let label = generic_hazard_label(&event_family, &text, awips_id.as_deref(), path);
    let headline = find_generic_headline(lines, &event_family)
        .or(awips_id)
        .or(heading);
    Some(HazardRecord {
        event_id: format!(
            "{}:{}",
            event_family.replace(' ', "-"),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("text-polygon")
        ),
        label,
        event_family,
        action: "TEXT".to_owned(),
        lifecycle_status: None,
        office: generic_office_from_heading(lines).unwrap_or_else(|| "NWS".to_owned()),
        headline,
        source_url: None,
        area: None,
        motion: find_prefixed_line(lines, "TIME...MOT...LOC"),
        details: Vec::new(),
        valid_start: None,
        valid_end: None,
        severity: None,
        certainty: None,
        urgency: None,
        tornado: None,
        hail_inches: None,
        wind_mph: None,
        damage_threat: None,
        bbox: hazard_bbox(&points),
        points,
    })
}

#[derive(Clone, Debug)]
struct ParsedWarningVtec {
    action: String,
    office: String,
    phenomenon: String,
    significance: String,
    event_tracking_number: String,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default)]
struct ParsedWarningTags {
    tornado: Option<String>,
    hail_inches: Option<f32>,
    wind_mph: Option<u16>,
    damage_threat: Option<String>,
}

fn parse_warning_vtec_line(line: &str) -> Option<ParsedWarningVtec> {
    let trimmed = line.trim();
    if !trimmed.starts_with("/O.") || !trimmed.ends_with('/') {
        return None;
    }
    let content = trimmed.trim_matches('/');
    let parts = content.split('.').collect::<Vec<_>>();
    if parts.len() < 7 || parts.first().copied() != Some("O") || parts.get(4) != Some(&"W") {
        return None;
    }
    let times = parts[6].split('-').collect::<Vec<_>>();
    Some(ParsedWarningVtec {
        action: parts[1].to_owned(),
        office: parts[2].to_owned(),
        phenomenon: parts[3].to_owned(),
        significance: parts[4].to_owned(),
        event_tracking_number: parts[5].to_owned(),
        start_time: times.first().and_then(|value| parse_vtec_time(value)),
        end_time: times.get(1).and_then(|value| parse_vtec_time(value)),
    })
}

fn parse_vtec_time(value: &str) -> Option<DateTime<Utc>> {
    let datetime = NaiveDateTime::parse_from_str(value, "%y%m%dT%H%MZ").ok()?;
    Some(Utc.from_utc_datetime(&datetime))
}

fn parse_lat_lon_points(lines: &[&str]) -> Vec<HazardPoint> {
    let Some(start_index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("LAT...LON"))
    else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for line in &lines[start_index..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "$$" {
            break;
        }
        if trimmed.contains("...") && !trimmed.starts_with("LAT...LON") {
            break;
        }
        let body = trimmed.strip_prefix("LAT...LON").unwrap_or(trimmed);
        for token in body.split_whitespace() {
            if token.as_bytes().iter().all(u8::is_ascii_digit) {
                tokens.push(token);
            }
        }
    }
    if tokens.iter().all(|token| token.len() >= 8) {
        tokens
            .iter()
            .filter_map(|token| parse_compact_lat_lon_token(token))
            .collect()
    } else {
        tokens
            .chunks_exact(2)
            .filter_map(|pair| {
                let lat = parse_coordinate_hundredths(pair[0], false)?;
                let lon = parse_coordinate_hundredths(pair[1], true)?;
                Some(HazardPoint { lon, lat })
            })
            .collect()
    }
}

fn parse_coordinate_hundredths(value: &str, west_longitude: bool) -> Option<f32> {
    let number = value.parse::<i32>().ok()?;
    let coordinate = number as f32 / 100.0;
    Some(if west_longitude {
        -coordinate
    } else {
        coordinate
    })
}

fn parse_compact_lat_lon_token(value: &str) -> Option<HazardPoint> {
    if value.len() < 8 || !value.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    let lat = parse_coordinate_hundredths(&value[..4], false)?;
    let lon = parse_coordinate_hundredths(&value[4..], true)?;
    Some(HazardPoint { lon, lat })
}

fn parse_warning_tags(lines: &[&str]) -> ParsedWarningTags {
    let mut tags = ParsedWarningTags::default();
    for line in lines {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("TORNADO...") {
            tags.tornado = Some(value.trim().to_owned());
        } else if let Some(value) = trimmed.strip_prefix("MAX HAIL SIZE...") {
            tags.hail_inches = parse_leading_float(value);
        } else if let Some(value) = trimmed.strip_prefix("MAX WIND GUST...") {
            tags.wind_mph = parse_leading_u16(value);
        } else if let Some(value) = trimmed
            .strip_prefix("TORNADO DAMAGE THREAT...")
            .or_else(|| trimmed.strip_prefix("THUNDERSTORM DAMAGE THREAT..."))
            .or_else(|| trimmed.strip_prefix("TSTM DAMAGE THREAT..."))
        {
            tags.damage_threat = Some(value.trim().to_owned());
        }
    }
    tags
}

fn parse_leading_float(value: &str) -> Option<f32> {
    value
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<f32>().ok())
}

fn parse_leading_u16(value: &str) -> Option<u16> {
    value
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<u16>().ok())
}

fn find_warning_headline(lines: &[&str]) -> Option<String> {
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        ((trimmed.ends_with("Warning") || trimmed.ends_with("Statement"))
            && !trimmed.starts_with('*'))
        .then(|| trimmed.to_owned())
    })
}

fn find_generic_headline(lines: &[&str], event_family: &str) -> Option<String> {
    let needle = event_family.to_ascii_uppercase();
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        let upper = trimmed.to_ascii_uppercase();
        (!trimmed.is_empty() && upper.contains(&needle)).then(|| trimmed.to_owned())
    })
}

fn find_prefixed_line(lines: &[&str], prefix: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .starts_with(prefix)
            .then(|| trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

fn generic_office_from_heading(lines: &[&str]) -> Option<String> {
    lines
        .iter()
        .find(|line| looks_like_wmo_heading(line.trim()))
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
}

fn looks_like_wmo_heading(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(ttaaii) = parts.next() else {
        return false;
    };
    let Some(cccc) = parts.next() else {
        return false;
    };
    let Some(time) = parts.next() else {
        return false;
    };
    ttaaii.len() == 6
        && cccc.len() == 4
        && time.len() == 6
        && ttaaii.as_bytes().iter().all(u8::is_ascii_alphanumeric)
        && cccc.as_bytes().iter().all(u8::is_ascii_alphabetic)
        && time.as_bytes().iter().all(u8::is_ascii_digit)
}

fn classify_generic_hazard_family(text: &str, awips_id: Option<&str>) -> String {
    let awips_id = awips_id.unwrap_or_default().to_ascii_uppercase();
    if text.contains("MESOSCALE DISCUSSION") || awips_id.contains("MCD") {
        "mesoscale discussion".to_owned()
    } else if text.contains("TORNADO WATCH")
        || text.contains("SEVERE THUNDERSTORM WATCH")
        || text.contains("WATCH OUTLINE UPDATE")
        || awips_id.starts_with("SEL")
        || awips_id.starts_with("SAW")
    {
        "watch".to_owned()
    } else if text.contains("LOCAL STORM REPORT") || awips_id.starts_with("LSR") {
        "local storm report".to_owned()
    } else {
        "text polygon".to_owned()
    }
}

fn generic_hazard_label(
    event_family: &str,
    text: &str,
    awips_id: Option<&str>,
    path: &Path,
) -> String {
    match event_family {
        "mesoscale discussion" => first_number_after(text, "MESOSCALE DISCUSSION")
            .map(|number| format!("MD {number}"))
            .unwrap_or_else(|| "MD".to_owned()),
        "watch" => first_number_after(text, "WATCH NUMBER")
            .or_else(|| first_number_after(text, "WATCH OUTLINE UPDATE FOR WS"))
            .map(|number| format!("WATCH {number}"))
            .unwrap_or_else(|| "WATCH".to_owned()),
        "local storm report" => "LSR".to_owned(),
        _ => awips_id
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "POLYGON".to_owned()),
    }
}

fn first_number_after(text: &str, marker: &str) -> Option<String> {
    let offset = text.find(marker)? + marker.len();
    text[offset..]
        .split(|character: char| !character.is_ascii_digit())
        .find(|token| !token.is_empty())
        .map(|token| {
            let trimmed = token.trim_start_matches('0');
            if trimmed.is_empty() { "0" } else { trimmed }.to_owned()
        })
}

fn hazard_lifecycle_status(
    action: &str,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    query_time_utc: Option<DateTime<Utc>>,
) -> Option<String> {
    if matches!(action, "CAN" | "EXP") {
        return Some(
            if action == "CAN" {
                "Canceled"
            } else {
                "Expired"
            }
            .to_owned(),
        );
    }
    let query_time_utc = query_time_utc?;
    if let Some(start_time) = start_time
        && query_time_utc < start_time
    {
        return Some("Pending".to_owned());
    }
    if let Some(end_time) = end_time
        && query_time_utc >= end_time
    {
        return Some("Expired".to_owned());
    }
    Some("Active".to_owned())
}

fn hazard_family_from_phenomenon(phenomenon: &str) -> &'static str {
    match phenomenon {
        "TO" => "tornado",
        "SV" => "severe thunderstorm",
        "FF" => "flash flood",
        "MA" => "special marine",
        "SQ" => "snow squall",
        "FL" | "FA" => "flood",
        _ => "warning",
    }
}

fn hazard_family_order(family: &str) -> u8 {
    match family {
        "tornado" => 0,
        "severe thunderstorm" => 1,
        "flash flood" => 2,
        "special marine" => 3,
        "snow squall" => 4,
        "flood" => 5,
        "watch" => 6,
        "mesoscale discussion" => 7,
        "local storm report" => 8,
        "special weather" => 9,
        _ => 9,
    }
}

fn hazard_label(
    event_family: &str,
    event_tracking_number: &str,
    tags: &ParsedWarningTags,
) -> String {
    let prefix = match event_family {
        "tornado" => "TOR",
        "severe thunderstorm" => "SVR",
        "flash flood" => "FFW",
        "flood" => "FLW",
        "special marine" => "SMW",
        "snow squall" => "SQW",
        _ => "WRN",
    };
    if let Some(tornado) = &tags.tornado {
        format!("{prefix} {event_tracking_number} {tornado}")
    } else {
        format!("{prefix} {event_tracking_number}")
    }
}

fn hazard_color(record: &HazardRecord) -> egui::Color32 {
    match record.event_family.as_str() {
        "tornado" => egui::Color32::from_rgb(248, 62, 82),
        "severe thunderstorm" => egui::Color32::from_rgb(246, 183, 57),
        "flash flood" => egui::Color32::from_rgb(78, 218, 108),
        "flood" => egui::Color32::from_rgb(76, 190, 124),
        "special marine" => egui::Color32::from_rgb(70, 190, 238),
        "snow squall" => egui::Color32::from_rgb(170, 210, 255),
        "watch" => egui::Color32::from_rgb(235, 92, 245),
        "mesoscale discussion" => egui::Color32::from_rgb(95, 174, 255),
        "local storm report" => egui::Color32::from_rgb(245, 245, 245),
        "special weather" => egui::Color32::from_rgb(245, 220, 72),
        "text polygon" => egui::Color32::from_rgb(190, 178, 255),
        _ => egui::Color32::from_rgb(232, 232, 96),
    }
}

fn hazard_fill_alpha_for_product(base_alpha: u8, selected: bool, product: &DisplayProduct) -> u8 {
    if product.base_moment() == MomentType::Velocity {
        0
    } else if selected {
        base_alpha.saturating_add(20).min(100)
    } else {
        base_alpha
    }
}

fn hazard_bbox(points: &[HazardPoint]) -> [f32; 4] {
    let mut west = f32::INFINITY;
    let mut south = f32::INFINITY;
    let mut east = f32::NEG_INFINITY;
    let mut north = f32::NEG_INFINITY;
    for point in points {
        west = west.min(point.lon);
        east = east.max(point.lon);
        south = south.min(point.lat);
        north = north.max(point.lat);
    }
    [west, south, east, north]
}

fn hazard_points_renderable(points: &[HazardPoint]) -> bool {
    if points.len() < 3 {
        return false;
    }
    if points.iter().any(|point| {
        !point.lon.is_finite()
            || !point.lat.is_finite()
            || point.lon < -180.0
            || point.lon > 180.0
            || point.lat < -90.0
            || point.lat > 90.0
    }) {
        return false;
    }

    let bbox = hazard_bbox(points);
    if bbox[2] - bbox[0] > HAZARD_MAX_RENDER_LON_SPAN_DEG
        || bbox[3] - bbox[1] > HAZARD_MAX_RENDER_LAT_SPAN_DEG
    {
        return false;
    }

    let mut previous = points[points.len() - 1];
    for current in points {
        if hazard_point_distance_km(previous, *current) > HAZARD_MAX_RENDER_EDGE_KM {
            return false;
        }
        previous = *current;
    }
    true
}

fn hazard_point_distance_km(a: HazardPoint, b: HazardPoint) -> f32 {
    const EARTH_RADIUS_KM: f32 = 6_371.0;
    let lat1 = a.lat.to_radians();
    let lat2 = b.lat.to_radians();
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();
    let half_dlat = (dlat * 0.5).sin();
    let half_dlon = (dlon * 0.5).sin();
    let h = half_dlat * half_dlat + lat1.cos() * lat2.cos() * half_dlon * half_dlon;
    2.0 * EARTH_RADIUS_KM * h.clamp(0.0, 1.0).sqrt().asin()
}

fn bbox_contains(bbox: [f32; 4], lon: f32, lat: f32) -> bool {
    lon >= bbox[0] && lon <= bbox[2] && lat >= bbox[1] && lat <= bbox[3]
}

fn hazard_polygon_contains_point(points: &[HazardPoint], point: HazardPoint) -> bool {
    if points.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for current in points {
        let crosses = (current.lat > point.lat) != (previous.lat > point.lat);
        if crosses {
            let lon_at_lat = (previous.lon - current.lon) * (point.lat - current.lat)
                / (previous.lat - current.lat)
                + current.lon;
            if point.lon < lon_at_lat {
                inside = !inside;
            }
        }
        previous = *current;
    }
    inside
}

fn is_convex_screen_polygon(points: &[egui::Pos2]) -> bool {
    if points.len() < 4 {
        return true;
    }
    let mut sign = 0.0f32;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let c = points[(index + 2) % points.len()];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross.abs() <= f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if sign != cross.signum() {
            return false;
        }
    }
    true
}

fn filled_polygon_mesh(points: &[egui::Pos2], fill: egui::Color32) -> Option<egui::epaint::Mesh> {
    if fill == egui::Color32::TRANSPARENT {
        return None;
    }
    let points = cleaned_screen_polygon(points);
    let triangles = triangulate_screen_polygon(&points)?;
    let mut mesh = egui::epaint::Mesh::default();
    for point in &points {
        mesh.colored_vertex(*point, fill);
    }
    for [a, b, c] in triangles {
        mesh.add_triangle(a as u32, b as u32, c as u32);
    }
    Some(mesh)
}

fn cleaned_screen_polygon(points: &[egui::Pos2]) -> Vec<egui::Pos2> {
    let mut cleaned = Vec::<egui::Pos2>::with_capacity(points.len());
    for point in points {
        if cleaned
            .last()
            .is_none_or(|previous| previous.distance_sq(*point) > 0.01)
        {
            cleaned.push(*point);
        }
    }
    if cleaned.len() > 1
        && cleaned
            .first()
            .zip(cleaned.last())
            .is_some_and(|(first, last)| first.distance_sq(*last) <= 0.01)
    {
        cleaned.pop();
    }
    cleaned
}

fn triangulate_screen_polygon(points: &[egui::Pos2]) -> Option<Vec<[usize; 3]>> {
    if points.len() < 3 || points.len() > u32::MAX as usize {
        return None;
    }
    let winding = polygon_signed_area(points).signum();
    if winding == 0.0 {
        return None;
    }

    let mut indices = (0..points.len()).collect::<Vec<_>>();
    let mut triangles = Vec::<[usize; 3]>::with_capacity(points.len().saturating_sub(2));
    let max_iterations = points.len() * points.len();
    let mut iterations = 0usize;

    while indices.len() > 3 && iterations < max_iterations {
        iterations += 1;
        let mut clipped = false;
        for current in 0..indices.len() {
            let previous = indices[(current + indices.len() - 1) % indices.len()];
            let index = indices[current];
            let next = indices[(current + 1) % indices.len()];
            if !is_ear_candidate(points, &indices, previous, index, next, winding) {
                continue;
            }
            triangles.push([previous, index, next]);
            indices.remove(current);
            clipped = true;
            break;
        }
        if !clipped {
            return None;
        }
    }

    if indices.len() == 3 {
        triangles.push([indices[0], indices[1], indices[2]]);
    }
    (!triangles.is_empty()).then_some(triangles)
}

fn is_ear_candidate(
    points: &[egui::Pos2],
    indices: &[usize],
    previous: usize,
    index: usize,
    next: usize,
    winding: f32,
) -> bool {
    let a = points[previous];
    let b = points[index];
    let c = points[next];
    let cross = cross_points(a, b, c);
    if cross.abs() <= f32::EPSILON || cross.signum() != winding {
        return false;
    }
    !indices.iter().any(|candidate| {
        let candidate = *candidate;
        candidate != previous
            && candidate != index
            && candidate != next
            && point_in_triangle(points[candidate], a, b, c)
    })
}

fn polygon_signed_area(points: &[egui::Pos2]) -> f32 {
    let mut area = 0.0f32;
    for index in 0..points.len() {
        let current = points[index];
        let next = points[(index + 1) % points.len()];
        area += current.x * next.y - next.x * current.y;
    }
    area * 0.5
}

fn cross_points(a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn point_in_triangle(point: egui::Pos2, a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> bool {
    let ab = cross_points(a, b, point);
    let bc = cross_points(b, c, point);
    let ca = cross_points(c, a, point);
    let has_negative = ab < -f32::EPSILON || bc < -f32::EPSILON || ca < -f32::EPSILON;
    let has_positive = ab > f32::EPSILON || bc > f32::EPSILON || ca > f32::EPSILON;
    !(has_negative && has_positive)
}

fn polygon_screen_centroid(points: &[egui::Pos2]) -> egui::Pos2 {
    let mut sum = egui::Vec2::ZERO;
    for point in points {
        sum += point.to_vec2();
    }
    let scale = 1.0 / points.len().max(1) as f32;
    egui::pos2(sum.x * scale, sum.y * scale)
}

fn point_segment_distance_sq(point: egui::Pos2, start: egui::Pos2, end: egui::Pos2) -> f32 {
    let segment = end - start;
    let length_sq = segment.length_sq();
    if length_sq <= f32::EPSILON {
        return point.distance_sq(start);
    }
    let t = ((point - start).dot(segment) / length_sq).clamp(0.0, 1.0);
    point.distance_sq(start + segment * t)
}

fn displayable_products(volume: &RadarVolume, cut_index: usize) -> Vec<DisplayProduct> {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return Vec::new();
    };
    let available = cut
        .moments
        .values()
        .filter(|grid| grid.radial_count() >= displayable_radial_threshold(cut.radials.len()))
        .map(|grid| grid.moment.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut products = product_order(&available);
    // Derived products are offered wherever their source moment is present
    // (reflectivity volume products on REF cuts; azimuthal shear on velocity
    // cuts).
    for d in DerivedProduct::ALL {
        if available.contains(&d.base_moment()) {
            products.push(DisplayProduct::Derived(d));
        }
    }
    products
}

fn cut_start_time_utc(volume: &RadarVolume, cut_index: usize) -> Option<DateTime<Utc>> {
    let cut = volume.cuts.get(cut_index)?;
    cut.radials
        .iter()
        .filter_map(|radial| {
            radial_collection_time_from_volume_time_utc(volume.volume_time, radial.time_offset_ms)
        })
        .min()
}

fn radial_collection_time_from_volume_time_utc(
    volume_time: DateTime<Utc>,
    time_offset_ms: i32,
) -> Option<DateTime<Utc>> {
    let midnight = volume_time
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| Utc.from_utc_datetime(&naive))?;
    let milliseconds = chrono::Duration::milliseconds(time_offset_ms as i64);
    let midnight_candidate = midnight + milliseconds;
    let relative_candidate = volume_time + milliseconds;
    let midnight_delta = (midnight_candidate - volume_time).num_milliseconds().abs();
    let relative_delta = (relative_candidate - volume_time).num_milliseconds().abs();
    Some(if midnight_delta <= relative_delta {
        midnight_candidate
    } else {
        relative_candidate
    })
}

fn displayable_cuts_for_product(volume: &RadarVolume, product: &DisplayProduct) -> Vec<usize> {
    (0..volume.cuts.len())
        .filter(|index| is_displayable_on_cut(volume, *index, product))
        .collect()
}

fn live_partial_has_complete_low_level_tilt(volume: &RadarVolume) -> bool {
    volume.cuts.iter().enumerate().any(|(index, cut)| {
        is_complete_live_low_level_tilt(cut) && !displayable_products(volume, index).is_empty()
    })
}

fn is_complete_live_low_level_tilt(cut: &ElevationCut) -> bool {
    is_live_low_level_tilt(cut)
        && cut.radials.len() >= LIVE_COMPLETE_LOW_LEVEL_TILT_MIN_RADIALS
        && live_tilt_azimuth_coverage_deg(cut) >= LIVE_COMPLETE_TILT_MIN_AZIMUTH_COVERAGE_DEG
}

fn is_complete_live_tilt(cut: &ElevationCut) -> bool {
    cut.radials.len() >= LIVE_COMPLETE_TILT_MIN_RADIALS
        && live_tilt_azimuth_coverage_deg(cut) >= LIVE_COMPLETE_TILT_MIN_AZIMUTH_COVERAGE_DEG
}

fn live_tilt_azimuth_coverage_deg(cut: &ElevationCut) -> f32 {
    let mut azimuths = cut
        .radials
        .iter()
        .map(|radial| radial.azimuth_deg.rem_euclid(360.0))
        .filter(|azimuth| azimuth.is_finite())
        .collect::<Vec<_>>();
    if azimuths.len() < 2 {
        return 0.0;
    }
    azimuths.sort_by(|left, right| left.total_cmp(right));
    azimuths.dedup_by(|left, right| (*left - *right).abs() < 0.05);
    if azimuths.len() < 2 {
        return 0.0;
    }

    let mut max_gap = 0.0_f32;
    for pair in azimuths.windows(2) {
        max_gap = max_gap.max(pair[1] - pair[0]);
    }
    let wrap_gap = azimuths[0] + 360.0 - azimuths[azimuths.len() - 1];
    max_gap = max_gap.max(wrap_gap);
    360.0 - max_gap
}

fn is_live_low_level_tilt(cut: &ElevationCut) -> bool {
    cut.elevation_deg <= LIVE_LOW_LEVEL_AUTO_ADVANCE_MAX_ELEVATION_DEG
}

fn is_allowed_live_low_level_tilt(cut: &ElevationCut, allow_incomplete: bool) -> bool {
    if allow_incomplete {
        is_live_low_level_tilt(cut)
    } else {
        is_complete_live_low_level_tilt(cut)
    }
}

fn stepped_product<'a>(
    products: &'a [DisplayProduct],
    current: &DisplayProduct,
    delta: isize,
) -> Option<&'a DisplayProduct> {
    stepped_slice_value(products, current, delta)
}

fn stepped_cut(cuts: &[usize], current: usize, delta: isize) -> Option<usize> {
    stepped_slice_value(cuts, &current, delta).copied()
}

fn stepped_slice_value<'a, T: PartialEq>(
    values: &'a [T],
    current: &T,
    delta: isize,
) -> Option<&'a T> {
    if values.is_empty() {
        return None;
    }
    let current_index = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0);
    let next_index = (current_index as isize + delta).rem_euclid(values.len() as isize) as usize;
    values.get(next_index)
}

fn is_displayable_on_cut(volume: &RadarVolume, cut_index: usize, product: &DisplayProduct) -> bool {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return false;
    };
    let base_moment = product.base_moment();
    let Some(grid) = cut.moments.get(&base_moment) else {
        return false;
    };
    grid.radial_count() >= displayable_radial_threshold(cut.radials.len())
}

fn displayable_radial_threshold(cut_radials: usize) -> usize {
    MIN_DISPLAYABLE_RADIALS.min((cut_radials / 2).max(1))
}

fn should_keep_texture_for_volume_install(
    previous_volume: Option<&RadarVolume>,
    next_volume: &RadarVolume,
    same_volume: bool,
) -> bool {
    same_volume || previous_volume.is_some_and(|previous| previous.site.id == next_volume.site.id)
}

fn selected_cut_render_data_unchanged(
    previous_volume: Option<&RadarVolume>,
    next_volume: &RadarVolume,
    selected_cut: usize,
    selected_product: &DisplayProduct,
) -> bool {
    let Some(previous_volume) = previous_volume else {
        return false;
    };
    if frame_identity_for_volume(previous_volume) != frame_identity_for_volume(next_volume) {
        return false;
    }
    let Some(previous_cut) = previous_volume.cuts.get(selected_cut) else {
        return false;
    };
    let Some(next_cut) = next_volume.cuts.get(selected_cut) else {
        return false;
    };
    if (previous_cut.elevation_deg - next_cut.elevation_deg).abs() > 0.05 {
        return false;
    }
    let base_moment = selected_product.base_moment();
    let Some(previous_grid) = previous_cut.moments.get(&base_moment) else {
        return false;
    };
    let Some(next_grid) = next_cut.moments.get(&base_moment) else {
        return false;
    };
    previous_cut.radials.len() == next_cut.radials.len()
        && previous_grid.radial_count() == next_grid.radial_count()
        && previous_grid.gate_range == next_grid.gate_range
}

fn selection_for_installed_volume(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
    allow_low_level_auto_advance: bool,
    allow_incomplete_live_chunk_advance: bool,
    require_complete_live_cut: bool,
) -> (usize, DisplayProduct) {
    let same_site = previous_volume.is_some_and(|previous| previous.site.id == volume.site.id);
    if same_site
        && allow_low_level_auto_advance
        && let Some(next_cut) = latest_newer_low_level_cut(
            previous_volume,
            previous_cut,
            previous_product,
            volume,
            allow_incomplete_live_chunk_advance,
        )
    {
        return (next_cut, previous_product.clone());
    }
    if same_site
        && is_displayable_on_live_candidate_cut(
            volume,
            previous_cut,
            previous_product,
            require_complete_live_cut,
        )
    {
        return (previous_cut, previous_product.clone());
    }
    if same_site
        && let Some(cut) = best_cut_for_product_with_live_filter(
            volume,
            previous_cut,
            previous_product,
            require_complete_live_cut,
        )
    {
        return (cut, previous_product.clone());
    }

    default_selection_for_volume_with_live_filter(volume, require_complete_live_cut)
}

fn latest_newer_low_level_cut(
    previous_volume: Option<&RadarVolume>,
    previous_cut: usize,
    previous_product: &DisplayProduct,
    volume: &RadarVolume,
    allow_incomplete_live_chunk_advance: bool,
) -> Option<usize> {
    let previous_volume = previous_volume?;
    if frame_identity_for_volume(previous_volume) != frame_identity_for_volume(volume) {
        return None;
    }
    let previous_cut_data = previous_volume.cuts.get(previous_cut)?;
    if !is_allowed_live_low_level_tilt(previous_cut_data, allow_incomplete_live_chunk_advance) {
        return None;
    }
    let previous_time = cut_start_time_utc(previous_volume, previous_cut)?;

    (0..volume.cuts.len())
        .filter(|cut_index| {
            volume.cuts.get(*cut_index).is_some_and(|cut| {
                is_allowed_live_low_level_tilt(cut, allow_incomplete_live_chunk_advance)
            }) && is_displayable_on_cut(volume, *cut_index, previous_product)
        })
        .filter_map(|cut_index| {
            let cut_time = cut_start_time_utc(volume, cut_index)?;
            ((cut_time - previous_time).num_seconds() >= LIVE_LOW_LEVEL_AUTO_ADVANCE_MIN_SECONDS)
                .then_some((cut_index, cut_time))
        })
        .max_by_key(|(_, cut_time)| *cut_time)
        .map(|(cut_index, _)| cut_index)
}

fn should_defer_live_partial_selection_for_active_product(
    active_volume: Option<&RadarVolume>,
    selected_product: &DisplayProduct,
    candidate: Option<&FrameHistoryEntry>,
) -> bool {
    let Some(active_volume) = active_volume else {
        return false;
    };
    let Some(candidate) = candidate else {
        return false;
    };
    if candidate.status != FrameStatus::LivePartial
        || active_volume.site.id != candidate.identity.site_id
        || !volume_has_displayable_product(active_volume, selected_product)
    {
        return false;
    }

    !volume_has_displayable_product_with_live_filter(
        candidate.volume.as_ref(),
        selected_product,
        true,
    )
}

fn volume_has_displayable_product(volume: &RadarVolume, product: &DisplayProduct) -> bool {
    volume_has_displayable_product_with_live_filter(volume, product, false)
}

fn volume_has_displayable_product_with_live_filter(
    volume: &RadarVolume,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    (0..volume.cuts.len()).any(|cut_index| {
        is_displayable_on_live_candidate_cut(volume, cut_index, product, require_complete_live_cut)
    })
}

fn default_selection_for_volume_with_live_filter(
    volume: &RadarVolume,
    require_complete_live_cut: bool,
) -> (usize, DisplayProduct) {
    let reflectivity = DisplayProduct::Moment(MomentType::Reflectivity);
    if is_displayable_on_live_candidate_cut(volume, 0, &reflectivity, require_complete_live_cut) {
        return (0, reflectivity);
    }

    for cut_index in 0..volume.cuts.len() {
        let Some(cut) = volume.cuts.get(cut_index) else {
            continue;
        };
        if require_complete_live_cut && !is_complete_live_tilt(cut) {
            continue;
        }
        if let Some(product) = displayable_products(volume, cut_index).first().cloned() {
            return (cut_index, product);
        }
    }

    (0, reflectivity)
}

fn best_cut_for_product_with_live_filter(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> Option<usize> {
    let current_elevation = volume.cuts.get(current_cut).map(|cut| cut.elevation_deg);
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            is_displayable_on_live_candidate_cut(volume, *index, product, require_complete_live_cut)
        })
        .min_by(|(left_index, left_cut), (right_index, right_cut)| {
            let left_delta = current_elevation
                .map(|elevation| (left_cut.elevation_deg - elevation).abs())
                .unwrap_or(*left_index as f32);
            let right_delta = current_elevation
                .map(|elevation| (right_cut.elevation_deg - elevation).abs())
                .unwrap_or(*right_index as f32);
            left_delta.total_cmp(&right_delta)
        })
        .map(|(index, _)| index)
}

fn is_displayable_on_live_candidate_cut(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
    require_complete_live_cut: bool,
) -> bool {
    if !is_displayable_on_cut(volume, cut_index, product) {
        return false;
    }
    !require_complete_live_cut
        || volume
            .cuts
            .get(cut_index)
            .is_some_and(is_complete_live_tilt)
}

fn should_clear_display_for_latest_load(
    volume: Option<&RadarVolume>,
    site_id: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    let Some(volume) = volume else {
        return false;
    };
    if volume.site.id != site_id {
        return true;
    }

    now_utc
        .signed_duration_since(volume.volume_time.with_timezone(&Utc))
        .num_seconds()
        > STALE_LATEST_DISPLAY_CLEAR_SECONDS
}

fn should_clear_display_before_latest_load(
    mode: LatestLoadMode,
    volume: Option<&RadarVolume>,
    site_id: &str,
    now_utc: DateTime<Utc>,
) -> bool {
    mode != LatestLoadMode::AutoRefresh
        && should_clear_display_for_latest_load(volume, site_id, now_utc)
}

fn normalized_history_limit(limit: usize) -> usize {
    if HISTORY_SIZE_OPTIONS.contains(&limit) {
        limit
    } else {
        DEFAULT_HISTORY_FRAME_LIMIT
    }
}

fn frame_identity_for_volume(volume: &RadarVolume) -> FrameIdentity {
    FrameIdentity {
        site_id: volume.site.id.clone(),
        scan_time_utc: volume.volume_time.with_timezone(&Utc),
    }
}

fn archive_frame_status(volume_time_utc: DateTime<Utc>, now_utc: DateTime<Utc>) -> FrameStatus {
    if now_utc.signed_duration_since(volume_time_utc).num_seconds()
        > STALE_LATEST_DISPLAY_CLEAR_SECONDS
    {
        FrameStatus::Stale
    } else {
        FrameStatus::Complete
    }
}

fn frame_status_priority(status: FrameStatus) -> u8 {
    match status {
        FrameStatus::Preview => 0,
        FrameStatus::LivePartial => 1,
        FrameStatus::Complete | FrameStatus::Stale => 2,
        FrameStatus::LiveComplete | FrameStatus::Local => 3,
    }
}

fn live_partial_frame_has_new_data(
    incoming: &FrameHistoryEntry,
    existing: &FrameHistoryEntry,
) -> bool {
    incoming.status == FrameStatus::LivePartial
        && existing.status == FrameStatus::LivePartial
        && incoming.path == existing.path
        && (incoming.volume.metadata.decoded_radial_count
            > existing.volume.metadata.decoded_radial_count
            || volume_total_radials(incoming.volume.as_ref())
                > volume_total_radials(existing.volume.as_ref()))
}

fn volume_total_radials(volume: &RadarVolume) -> usize {
    volume.cuts.iter().map(|cut| cut.radials.len()).sum()
}

fn frame_status_text(frame: &FrameHistoryEntry, now_utc: DateTime<Utc>) -> String {
    let live_chunk = live_chunk_readout(frame, now_utc)
        .map(|readout| format!(" {readout}"))
        .unwrap_or_default();
    format!(
        "{} {} {} age {}{} ({})",
        frame.identity.site_id,
        frame.identity.scan_time_utc.format("%Y-%m-%d %H:%M:%S UTC"),
        frame.status.label(),
        frame_age_label(frame.identity.scan_time_utc, now_utc),
        live_chunk,
        frame.source_label
    )
}

fn live_chunk_readout(frame: &FrameHistoryEntry, now_utc: DateTime<Utc>) -> Option<String> {
    if !matches!(
        frame.status,
        FrameStatus::LivePartial | FrameStatus::LiveComplete
    ) {
        return None;
    }
    let timings = frame.timings?;
    let last_modified = timings.realtime_last_modified_utc?;
    let chunk_count = timings.realtime_chunk_count.unwrap_or_default();
    let chunk_id = timings.realtime_last_chunk_id.unwrap_or_default();
    let chunk_type = timings
        .realtime_last_chunk_type
        .map(RealtimeChunkType::label)
        .unwrap_or("chunk");
    Some(format!(
        "last chunk {} age {} chunks {} id {} {}",
        last_modified.format("%H:%M:%S UTC"),
        frame_age_label(last_modified, now_utc),
        chunk_count,
        chunk_id,
        chunk_type
    ))
}

fn compact_frame_label(frame: &FrameHistoryEntry, now_utc: DateTime<Utc>) -> String {
    format!(
        "{} {}",
        frame.identity.scan_time_utc.format("%H:%M"),
        short_frame_status_label(frame.status, frame.identity.scan_time_utc, now_utc)
    )
}

fn history_contains_other_site(history: &[FrameHistoryEntry], site_id: &str) -> bool {
    history
        .iter()
        .any(|frame| frame.identity.site_id != site_id)
}

fn short_frame_status_label(
    status: FrameStatus,
    scan_time_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
) -> &'static str {
    match status {
        FrameStatus::LivePartial => "partial",
        FrameStatus::LiveComplete => "live",
        FrameStatus::Complete => "done",
        FrameStatus::Stale => "stale",
        FrameStatus::Local => "local",
        FrameStatus::Preview => {
            if now_utc
                .signed_duration_since(scan_time_utc)
                .num_seconds()
                .max(0)
                > STALE_LATEST_DISPLAY_CLEAR_SECONDS
            {
                "preview-old"
            } else {
                "preview"
            }
        }
    }
}

fn frame_age_label(scan_time_utc: DateTime<Utc>, now_utc: DateTime<Utc>) -> String {
    let age_seconds = now_utc
        .signed_duration_since(scan_time_utc)
        .num_seconds()
        .max(0);
    if age_seconds < 90 {
        format!("{age_seconds}s")
    } else if age_seconds < 2 * 3600 {
        format!("{}m", age_seconds / 60)
    } else {
        format!("{:.1}h", age_seconds as f32 / 3600.0)
    }
}

fn compact_byte_label(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f32 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f32 / (1024.0 * 1024.0))
    }
}

fn is_unchanged_realtime_refresh(
    cache_hit: bool,
    downloaded_path: &Path,
    current_source_path: Option<&Path>,
) -> bool {
    cache_hit && current_source_path.is_some_and(|current| current == downloaded_path)
}

fn selected_grid_range_km_for(
    volume: &RadarVolume,
    cut_index: usize,
    product: &DisplayProduct,
) -> Option<f32> {
    let cut = volume.cuts.get(cut_index)?;
    let grid = cut.moments.get(&product.base_moment())?;
    grid_range_km(grid)
}

fn best_cut_for_product(
    volume: &RadarVolume,
    current_cut: usize,
    product: &DisplayProduct,
) -> Option<usize> {
    let current_elevation = volume.cuts.get(current_cut).map(|cut| cut.elevation_deg);
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(index, _)| is_displayable_on_cut(volume, *index, product))
        .min_by(|(left_index, left_cut), (right_index, right_cut)| {
            let left_delta = current_elevation
                .map(|elevation| (left_cut.elevation_deg - elevation).abs())
                .unwrap_or(*left_index as f32);
            let right_delta = current_elevation
                .map(|elevation| (right_cut.elevation_deg - elevation).abs())
                .unwrap_or(*right_index as f32);
            left_delta.total_cmp(&right_delta)
        })
        .map(|(index, _)| index)
}

fn configure_style(ctx: &egui::Context) {
    use egui::Color32;
    let mut style = (*ctx.global_style()).clone();
    // Snappy = fast: no widget animations (the app's identity is speed).
    style.animation_time = 0.0;
    style.visuals = egui::Visuals::dark();

    // GR2 "warning-desk" look: near-black, low-chroma neutral panels so the
    // saturated REF/VEL/CC/ZDR palettes pop. This is the single biggest cheap
    // lever for reading as a pro radar tool.
    let panel = Color32::from_rgb(14, 15, 17);
    let raised = Color32::from_rgb(22, 24, 27);
    let sunken = Color32::from_rgb(9, 10, 12);
    style.visuals.panel_fill = panel;
    style.visuals.window_fill = panel;
    style.visuals.extreme_bg_color = sunken; // text edits, sliders troughs
    style.visuals.faint_bg_color = Color32::from_rgb(20, 22, 25); // table striping
    style.visuals.window_stroke = egui::Stroke::new(1.0, Color32::from_rgb(40, 44, 50));
    // Desaturated light text, not pure white.
    style.visuals.override_text_color = Some(Color32::from_rgb(205, 210, 216));

    // Low-chroma muted-blue selection/active accents.
    style.visuals.selection.bg_fill = Color32::from_rgb(38, 74, 108);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, Color32::from_rgb(120, 170, 210));
    let w = &mut style.visuals.widgets;
    w.noninteractive.bg_fill = panel;
    w.inactive.bg_fill = raised;
    w.inactive.weak_bg_fill = raised;
    w.hovered.bg_fill = Color32::from_rgb(36, 46, 58);
    w.hovered.weak_bg_fill = Color32::from_rgb(36, 46, 58);
    w.active.bg_fill = Color32::from_rgb(48, 88, 126);
    w.active.weak_bg_fill = Color32::from_rgb(48, 88, 126);
    w.open.bg_fill = raised;

    // Tighten density toward GR2's information-dense layout.
    style.spacing.button_padding = egui::vec2(5.0, 2.0);
    style.spacing.item_spacing = egui::vec2(4.0, 3.0);
    style.spacing.interact_size.y = 18.0;
    ctx.set_global_style(style);
}

fn radar_texture_options() -> egui::TextureOptions {
    egui::TextureOptions::NEAREST
}

fn radar_color_image_from_rgba(size: [usize; 2], rgba: &[u8]) -> egui::ColorImage {
    assert_eq!(
        size[0] * size[1] * 4,
        rgba.len(),
        "size: {:?}, rgba.len(): {}",
        size,
        rgba.len()
    );
    debug_assert_eq!(std::mem::size_of::<egui::Color32>(), 4);
    debug_assert_eq!(std::mem::align_of::<egui::Color32>(), 4);
    debug_assert!(radar_rgba_is_premultiplied_compatible(rgba));

    let mut pixels = Vec::<egui::Color32>::with_capacity(size[0] * size[1]);
    // SAFETY: Color32 is a repr(C), 4-byte-aligned wrapper over [u8; 4] in egui 0.34.
    // We allocate Color32 storage, copy whole RGBA texels into it, then set the exact texel length.
    unsafe {
        let dst = pixels.as_mut_ptr().cast::<u8>();
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), dst, rgba.len());
        pixels.set_len(size[0] * size[1]);
    }
    egui::ColorImage::new(size, pixels)
}

fn radar_rgba_is_premultiplied_compatible(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|pixel| match pixel[3] {
        0 => pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0,
        255 => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_site_index_prefers_closest_coordinate() {
        let sites = vec![
            RadarSite::new("KFWS").with_location(
                Some("Fort Worth".to_owned()),
                Some(32.573),
                Some(-97.303),
            ),
            RadarSite::new("KTLX").with_location(
                Some("Norman".to_owned()),
                Some(35.333),
                Some(-97.278),
            ),
        ];

        let index = nearest_site_index(&sites, 35.4, -97.2).expect("nearest station");
        assert_eq!(sites[index].level2_id, "KTLX");
    }

    #[test]
    fn haversine_is_zero_for_same_point() {
        assert!(haversine_km(35.333, -97.278, 35.333, -97.278) < 0.001);
    }

    #[test]
    fn gate_for_range_uses_selected_gate_spacing() {
        let grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            radar_core::GateRange {
                first_gate_m: 500,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        assert_eq!(gate_for_range(&grid, 0.50), Some(0));
        assert_eq!(gate_for_range(&grid, 0.75), Some(1));
        assert_eq!(gate_for_range(&grid, 1.25), Some(3));
        assert_eq!(gate_for_range(&grid, 1.50), None);
    }

    #[test]
    fn color_summary_reports_quantized_step_size() {
        let table = ColorTable::parse("summary", "step: 5\ncolor: 0 0 0 0\ncolor: 10 255 255 255")
            .expect("table parses");

        let summary = color_table_summary(&table);

        assert!(summary.contains("quantized stepped, step 5.00"));
    }

    #[test]
    fn velocity_products_draw_hazard_polygons_outline_only() {
        assert_eq!(
            hazard_fill_alpha_for_product(50, false, &DisplayProduct::Moment(MomentType::Velocity)),
            0
        );
        assert_eq!(
            hazard_fill_alpha_for_product(50, true, &DisplayProduct::StormRelativeVelocity),
            0
        );
        assert_eq!(
            hazard_fill_alpha_for_product(
                50,
                false,
                &DisplayProduct::Moment(MomentType::Reflectivity)
            ),
            50
        );
        assert_eq!(
            hazard_fill_alpha_for_product(
                50,
                true,
                &DisplayProduct::Moment(MomentType::Reflectivity)
            ),
            70
        );
    }

    #[test]
    fn vrot_probe_uses_source_velocity_gates() {
        let gate_range = radar_core::GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: 5,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(test_radial(0.0, gate_range.clone()));
        cut.radials.push(test_radial(1.0, gate_range.clone()));

        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        grid.push_u8_row_slice(0, &[64, 54, 54, 64, 64])
            .expect("first velocity row");
        grid.push_u8_row_slice(1, &[64, 64, 84, 84, 64])
            .expect("second velocity row");

        let probe = velocity_vrot_probe(
            &cut,
            &grid,
            0,
            2,
            &DisplayProduct::Moment(MomentType::Velocity),
            StormMotion {
                direction_deg: 0.0,
                speed_mps: 0.0,
            },
        )
        .expect("vrot probe");

        assert_eq!(probe.delta_v_mps, 30.0);
        assert_eq!(probe.vrot_mps, 15.0);
        assert!(probe.separation_km > 0.0);
        assert_eq!(probe.inbound.row, 0);
        assert_eq!(probe.inbound.gate, 1);
        assert_eq!(probe.inbound.value_mps, -10.0);
        assert_eq!(probe.outbound.row, 1);
        assert_eq!(probe.outbound.gate, 2);
        assert_eq!(probe.outbound.value_mps, 20.0);
    }

    #[test]
    fn cursor_readout_uses_dealiased_velocity_grid_for_dvel() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.volume = Some(Arc::new(test_aliased_velocity_volume()));
        app.selected_cut = 0;
        app.selected_product = DisplayProduct::DealiasedVelocity;
        app.map_center_lat = 35.0;
        app.map_center_lon = -97.0;
        app.map_scale = 1000.0;

        let rect = test_map_rect();
        let target_lat = 35.0 + 20.0 / 111.32;
        let position = app.lon_lat_to_screen(rect, -97.0, target_lat);
        let readout = app.cursor_readout_at(rect, position).expect("DVEL readout");

        assert_eq!(readout.product, DisplayProduct::DealiasedVelocity);
        assert_eq!(readout.gate, 2);
        assert!((readout.value - 11.0).abs() < 0.01, "{readout:?}");
        assert_eq!(readout.raw, None);
        assert!(app.dealiased_readout_cache.is_some());
    }

    #[test]
    fn cursor_readout_format_reports_source_gate_provenance() {
        let readout = CursorReadout {
            site_id: "KTLX".to_owned(),
            volume_time_utc: Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap(),
            product: DisplayProduct::Moment(MomentType::Velocity),
            cut: 1,
            value: 22.5,
            base_value: None,
            vrot: None,
            raw: Some(86),
            row: 42,
            gate: 123,
            gate_spacing_m: 250,
            range_km: 31.2,
            azimuth_deg: 181.2,
            source_azimuth_deg: 180.9,
            elevation_deg: 0.48,
            height_above_radar_m: 900.0,
            nyquist_velocity_mps: Some(32.0),
            realtime_volume_id: Some(12),
            realtime_last_chunk_id: Some(34),
            realtime_last_chunk_type: None,
        };

        let formatted = format_cursor_readout(&readout);

        assert!(formatted.contains("KTLX 01:30:00"));
        assert!(formatted.contains("row 42 gate 123"));
        assert!(formatted.contains("az 181.2 src 180.9"));
        assert!(formatted.contains("raw 86"));
        assert!(formatted.contains("rt v012 c034"));
    }

    #[test]
    fn cursor_readout_format_reports_vrot_gate_endpoints() {
        let readout = CursorReadout {
            site_id: "KTLX".to_owned(),
            volume_time_utc: Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap(),
            product: DisplayProduct::Moment(MomentType::Velocity),
            cut: 1,
            value: 22.5,
            base_value: None,
            vrot: Some(VrotProbe {
                delta_v_mps: 42.0,
                vrot_mps: 21.0,
                separation_km: 1.25,
                inbound: VrotGate {
                    row: 4,
                    gate: 100,
                    value_mps: -18.0,
                    azimuth_deg: 210.5,
                },
                outbound: VrotGate {
                    row: 6,
                    gate: 103,
                    value_mps: 24.0,
                    azimuth_deg: 212.0,
                },
            }),
            raw: Some(86),
            row: 5,
            gate: 101,
            gate_spacing_m: 250,
            range_km: 31.2,
            azimuth_deg: 211.2,
            source_azimuth_deg: 211.0,
            elevation_deg: 0.48,
            height_above_radar_m: 900.0,
            nyquist_velocity_mps: Some(32.0),
            realtime_volume_id: None,
            realtime_last_chunk_id: None,
            realtime_last_chunk_type: None,
        };

        let formatted = format_cursor_readout(&readout);

        assert!(formatted.contains("Vrot 21.0 m/s dV 42.0 sep 1.25 km"));
        assert!(formatted.contains("in r4/g100 210.5 -18.0"));
        assert!(formatted.contains("out r6/g103 212.0 24.0"));
    }

    #[test]
    fn cache_policy_scales_with_cpu_budget() {
        let low = test_cache_policy(4);
        let mid = test_cache_policy(8);
        let high = test_cache_policy(16);

        assert_eq!(low.sample_cache_capacity(), 1);
        assert_eq!(low.moment_cache_capacity(), 1);
        assert_eq!(low.sample_cache_bytes(), LOW_END_SAMPLE_CACHE_BYTES);
        assert_eq!(mid.sample_cache_capacity(), 3);
        assert_eq!(mid.moment_cache_capacity(), 3);
        assert_eq!(mid.sample_cache_bytes(), MID_RANGE_SAMPLE_CACHE_BYTES);
        assert_eq!(high.sample_cache_capacity(), 6);
        assert_eq!(high.moment_cache_capacity(), 6);
        assert_eq!(high.sample_cache_bytes(), HIGH_END_SAMPLE_CACHE_BYTES);
    }

    #[test]
    fn sample_cache_signature_ignores_color_table_signature() {
        let viewport = ViewportKey {
            width: 800,
            height: 600,
            radar_x_px: 4_000,
            radar_y_px: 3_000,
            km_per_px_x: 160_000,
            km_per_px_y: 160_000,
        };

        let first_pixels = RenderWorkerViewportSignature::new(
            10,
            1,
            DisplayProduct::Moment(MomentType::Reflectivity),
            MomentType::Reflectivity,
            false,
            123,
            (0, 0),
            (32, 64),
            viewport,
        );
        let second_pixels = RenderWorkerViewportSignature::new(
            10,
            1,
            DisplayProduct::Moment(MomentType::Reflectivity),
            MomentType::Reflectivity,
            false,
            456,
            (0, 0),
            (32, 64),
            viewport,
        );
        assert_ne!(first_pixels, second_pixels);

        let first_samples = RenderWorkerSampleCacheSignature::new(
            10,
            1,
            DisplayProduct::Moment(MomentType::Reflectivity),
            MomentType::Reflectivity,
            false,
            viewport,
        );
        let second_samples = RenderWorkerSampleCacheSignature::new(
            10,
            1,
            DisplayProduct::Moment(MomentType::Reflectivity),
            MomentType::Reflectivity,
            false,
            viewport,
        );
        assert_eq!(first_samples, second_samples);
    }

    #[test]
    fn plain_velocity_render_unfolds_by_default_but_can_show_raw() {
        let velocity = DisplayProduct::Moment(MomentType::Velocity);
        assert!(velocity.render_uses_dealiased_velocity(true));
        assert!(!velocity.render_uses_dealiased_velocity(false));
        assert!(DisplayProduct::DealiasedVelocity.render_uses_dealiased_velocity(false));
        assert!(!DisplayProduct::StormRelativeVelocity.render_uses_dealiased_velocity(false));
    }

    #[test]
    fn radar_overlay_layer_starts_visible_with_independent_workers() {
        let site = RadarSite::new("KTLX");
        let layer = RadarOverlayLayer::new(7, site);

        assert_eq!(layer.id, 7);
        assert_eq!(layer.site.level2_id, "KTLX");
        assert!(layer.visible);
        assert_eq!(layer.opacity, DEFAULT_RADAR_OVERLAY_ALPHA);
        assert!(layer.volume.is_none());
        assert!(layer.texture.is_none());
        assert!(layer.load_receiver.is_none());
        assert!(layer.pending_render_key.is_none());
    }

    #[test]
    fn selected_grid_range_tracks_product_cut() {
        let volume = test_aliased_velocity_volume();
        let product = DisplayProduct::Moment(MomentType::Velocity);
        let range = selected_grid_range_km_for(&volume, 0, &product).expect("velocity range");

        assert!(range > 0.0);
    }

    #[test]
    fn rayon_thread_cap_overrides_machine_budget() {
        assert_eq!(configured_rayon_threads_from(Some("2")), Some(2));
        assert_eq!(configured_rayon_threads_from(Some(" 4 ")), Some(4));
        assert_eq!(configured_rayon_threads_from(Some("0")), None);
        assert_eq!(configured_rayon_threads_from(Some("not-a-number")), None);
        assert_eq!(configured_rayon_threads_from(None), None);
    }

    #[test]
    fn preview_policy_enables_fast_first_pixels_for_all_cpu_budgets() {
        assert!(should_preview_loads_for_threads(1));
        assert!(should_preview_loads_for_threads(LOW_CORE_PREVIEW_THREADS));
        assert!(should_preview_loads_for_threads(
            LOW_CORE_PREVIEW_THREADS + 1
        ));
        assert!(should_preview_loads_for_threads(64));
    }

    #[test]
    fn history_archive_parallelism_scales_with_cpu_budget() {
        assert_eq!(history_archive_load_parallelism_for_threads(1), 1);
        assert_eq!(history_archive_load_parallelism_for_threads(4), 2);
        assert_eq!(history_archive_load_parallelism_for_threads(6), 3);
        assert_eq!(
            history_archive_load_parallelism_for_threads(16),
            HISTORY_ARCHIVE_LOAD_MAX_PARALLELISM
        );
    }

    #[test]
    fn block_bzip_preview_policy_enables_every_cpu_budget() {
        assert!(should_preview_block_bzip_loads_for_threads(1));
        assert!(should_preview_block_bzip_loads_for_threads(
            LOW_CORE_PREVIEW_THREADS
        ));
        assert!(should_preview_block_bzip_loads_for_threads(
            LOW_CORE_PREVIEW_THREADS + 1
        ));
        assert!(should_preview_block_bzip_loads_for_threads(64));
    }

    #[test]
    fn preview_head_start_is_only_for_low_core_budgets() {
        assert_eq!(
            preview_render_head_start(LOW_CORE_PREVIEW_THREADS),
            Duration::from_millis(LOW_CORE_PREVIEW_RENDER_HEAD_START_MS)
        );
        assert_eq!(
            preview_render_head_start(LOW_CORE_PREVIEW_THREADS + 1),
            Duration::ZERO
        );
    }

    #[test]
    fn cache_policy_warms_slow_low_end_direct_renders() {
        let low = test_cache_policy(2);
        let mid = test_cache_policy(8);

        assert!(!low.should_speculatively_warm_sample_cache(&test_rendered_texture(3.5, false)));
        assert!(low.should_speculatively_warm_sample_cache(&test_rendered_texture(4.0, false)));
        assert!(mid.should_speculatively_warm_sample_cache(&test_rendered_texture(0.25, false)));
        assert!(!mid.should_speculatively_warm_sample_cache(&test_rendered_texture(8.0, true)));
    }

    #[test]
    fn cache_policy_skips_sample_caches_that_cannot_fit_budget() {
        let low = test_cache_policy(2);
        let high = test_cache_policy(16);

        assert!(!low.should_build_sample_cache_for_viewport(test_viewport_key(1920, 1080)));
        assert!(
            low.should_speculatively_warm_sample_cache(&test_rendered_texture_with_size(
                4.0, false, 1920, 1080
            ))
        );
        assert!(high.should_build_sample_cache_for_viewport(test_viewport_key(3840, 2160)));
    }

    #[test]
    fn cache_policy_uses_exact_radar_footprint_for_active_cache_builds() {
        let low = test_cache_policy(2);
        let volume = test_ref_then_velocity_volume();
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let options = ViewportRasterOptions {
            width: 1920,
            height: 1080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        };

        assert!(!low.should_build_sample_cache_for_viewport(test_viewport_key(1920, 1080)));
        assert!(
            low.should_build_sample_cache_for_moment_cache(&cache, &volume, options)
                .expect("sample cache footprint estimate")
        );
    }

    #[test]
    fn cache_policy_prefetches_interaction_cache_only_with_cpu_budget() {
        let low = test_cache_policy(4);
        let mid = test_cache_policy(8);
        let high = test_cache_policy(16);

        assert!(!low.should_prefetch_interaction_cache((1320, 820)));
        assert!(mid.should_prefetch_interaction_cache((1320, 820)));
        assert!(!mid.should_prefetch_interaction_cache((320, 240)));
        assert!(high.should_prefetch_interaction_cache((3840, 2160)));
    }

    #[test]
    fn overlay_cache_policy_keeps_background_radars_lightweight() {
        let overlay = test_overlay_cache_policy(16);

        assert_eq!(overlay.sample_cache_capacity(), 1);
        assert_eq!(overlay.moment_cache_capacity(), 1);
        assert_eq!(overlay.sample_cache_bytes(), LOW_END_SAMPLE_CACHE_BYTES);
        assert!(!overlay.should_prefetch_interaction_cache((3840, 2160)));
        assert!(
            !overlay.should_speculatively_warm_sample_cache(&test_rendered_texture_with_size(
                20.0, false, 1920, 1080
            ))
        );
    }

    #[test]
    fn velocity_prefetch_targets_nearest_displayable_velocity_cut() {
        let volume = Arc::new(test_ref_then_velocity_volume());
        let color_tables = ColorTableSet::default();
        let color_table_signature =
            color_tables.signature_for_family(ColorTableFamily::Reflectivity);
        let request = RenderRequest {
            key: TextureKey {
                volume_ptr: Arc::as_ptr(&volume) as usize,
                cut: 0,
                product: DisplayProduct::Moment(MomentType::Reflectivity),
                render_dealiased_velocity: false,
                color_table_signature,
                storm_motion_key: (450, 350),
                hail_levels_key: (32, 64),
                viewport: test_viewport_key(1320, 820),
            },
            pane: 0,
            volume,
            cut: 0,
            product: DisplayProduct::Moment(MomentType::Reflectivity),
            render_dealiased_velocity: false,
            plain_velocity_render_dealiased: true,
            color_tables,
            hail_levels_m: (3200.0, 6400.0),
            storm_motion: StormMotion {
                direction_deg: 45.0,
                speed_mps: 35.0 * KNOT_TO_MPS,
            },
            viewport_options: ViewportRasterOptions {
                width: 1320,
                height: 820,
                radar_x_px: 660.0,
                radar_y_px: 410.0,
                km_per_px_x: 0.16,
                km_per_px_y: 0.16,
            },
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
        };

        assert_eq!(ViewerApp::prefetch_velocity_cut(&request), Some(1));
        assert!(ViewerApp::should_prefetch_velocity_interaction_cache(
            &request,
            &test_rendered_texture_with_size(1.0, false, 1320, 820),
            test_cache_policy(8),
        ));
    }

    #[test]
    fn product_keyboard_step_wraps_display_products() {
        let products = vec![
            DisplayProduct::Moment(MomentType::Reflectivity),
            DisplayProduct::Moment(MomentType::Velocity),
            DisplayProduct::StormRelativeVelocity,
        ];

        assert_eq!(
            stepped_product(
                &products,
                &DisplayProduct::Moment(MomentType::Reflectivity),
                1
            ),
            Some(&DisplayProduct::Moment(MomentType::Velocity))
        );
        assert_eq!(
            stepped_product(&products, &DisplayProduct::StormRelativeVelocity, 1),
            Some(&DisplayProduct::Moment(MomentType::Reflectivity))
        );
        assert_eq!(
            stepped_product(
                &products,
                &DisplayProduct::Moment(MomentType::Reflectivity),
                -1
            ),
            Some(&DisplayProduct::StormRelativeVelocity)
        );
    }

    #[test]
    fn velocity_cut_exposes_dealiased_products() {
        let volume = test_ref_then_velocity_volume();
        let products = displayable_products(&volume, 1);

        assert!(products.contains(&DisplayProduct::Moment(MomentType::Velocity)));
        assert!(products.contains(&DisplayProduct::DealiasedVelocity));
        assert!(products.contains(&DisplayProduct::StormRelativeVelocity));
        assert!(products.contains(&DisplayProduct::StormRelativeDealiasedVelocity));
    }

    #[test]
    fn tilt_keyboard_step_wraps_displayable_cuts() {
        let cuts = vec![0, 2, 4];

        assert_eq!(stepped_cut(&cuts, 0, 1), Some(2));
        assert_eq!(stepped_cut(&cuts, 4, 1), Some(0));
        assert_eq!(stepped_cut(&cuts, 0, -1), Some(4));
    }

    #[test]
    fn same_site_install_preserves_velocity_selection() {
        let previous = test_ref_then_velocity_volume();
        let next = previous.clone();

        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &DisplayProduct::Moment(MomentType::Velocity),
                &next,
                true,
                false,
                false,
            ),
            (1, DisplayProduct::Moment(MomentType::Velocity))
        );
        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &DisplayProduct::StormRelativeVelocity,
                &next,
                true,
                false,
                false,
            ),
            (1, DisplayProduct::StormRelativeVelocity)
        );
    }

    #[test]
    fn same_site_live_update_advances_to_newer_low_level_sails_sweep() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let previous = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        let next = test_reflectivity_sails_volume_with_radials(
            &[(0.5, 0), (1.8, 60_000), (0.6, 180_000), (0.5, 360_000)],
            720,
        );

        assert_eq!(
            selection_for_installed_volume(Some(&previous), 0, &product, &next, true, false, false),
            (3, product.clone())
        );
        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &product,
                &next,
                false,
                false,
                false,
            ),
            (0, product)
        );
    }

    #[test]
    fn low_level_auto_advance_ignores_short_lag_and_high_tilts() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let previous = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        let short_lag =
            test_reflectivity_sails_volume_with_radials(&[(0.5, 0), (0.7, 75_000)], 720);
        let high_tilt =
            test_reflectivity_sails_volume_with_radials(&[(0.5, 0), (1.8, 180_000)], 720);

        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &product,
                &short_lag,
                true,
                false,
                false,
            ),
            (0, product.clone())
        );
        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                0,
                &product,
                &high_tilt,
                true,
                false,
                false,
            ),
            (0, product)
        );
    }

    #[test]
    fn low_level_auto_advance_ignores_incomplete_chunk_tilt_by_default() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let previous = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        let next = test_reflectivity_sails_volume_mixed_radials(&[
            (0.5, 0, 720),
            (1.8, 60_000, 360),
            (0.6, 180_000, 240),
        ]);

        assert!(live_partial_has_complete_low_level_tilt(&next));
        assert_eq!(
            selection_for_installed_volume(Some(&previous), 0, &product, &next, true, false, false),
            (0, product)
        );
    }

    #[test]
    fn low_level_auto_advance_allows_incomplete_chunk_tilt_when_enabled() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let previous = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        let next = test_reflectivity_sails_volume_mixed_radials(&[
            (0.5, 0, 720),
            (1.8, 60_000, 360),
            (0.6, 180_000, 240),
        ]);

        assert_eq!(
            selection_for_installed_volume(Some(&previous), 0, &product, &next, true, true, false),
            (2, product)
        );
    }

    #[test]
    fn live_partial_complete_low_level_tilt_gate_rejects_chunk_only_volume() {
        let chunk_only = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 240);
        let tail_chunk = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 719);
        let mut sector_chunk = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        set_cut_azimuth_span(&mut sector_chunk, 0, 20.0, 175.0);
        let full_low = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);

        assert!(!live_partial_has_complete_low_level_tilt(&chunk_only));
        assert!(!live_partial_has_complete_low_level_tilt(&tail_chunk));
        assert!(!live_partial_has_complete_low_level_tilt(&sector_chunk));
        assert!(live_partial_has_complete_low_level_tilt(&full_low));
    }

    #[test]
    fn live_partial_selection_skips_sector_chunk_cut_when_chunks_are_off() {
        let product = DisplayProduct::Moment(MomentType::Reflectivity);
        let mut volume =
            test_reflectivity_sails_volume_with_radials(&[(0.5, 0), (0.6, 180_000)], 720);
        set_cut_azimuth_span(&mut volume, 0, 20.0, 175.0);

        assert_eq!(
            selection_for_installed_volume(None, 0, &product, &volume, true, false, true),
            (1, product)
        );
    }

    #[test]
    fn different_site_install_starts_from_default_reflectivity() {
        let previous = test_ref_then_velocity_volume();
        let mut next = previous.clone();
        next.site.id = "OTHER".to_owned();

        assert_eq!(
            selection_for_installed_volume(
                Some(&previous),
                1,
                &DisplayProduct::Moment(MomentType::Velocity),
                &next,
                true,
                false,
                false,
            ),
            (0, DisplayProduct::Moment(MomentType::Reflectivity))
        );
    }

    #[test]
    fn same_site_refresh_keeps_existing_texture_until_replacement_render() {
        let previous = test_ref_then_velocity_volume();
        let next = previous.clone();
        let mut different = previous.clone();
        different.site.id = "OTHER".to_owned();

        assert!(should_keep_texture_for_volume_install(
            Some(&previous),
            &next,
            false
        ));
        assert!(should_keep_texture_for_volume_install(
            Some(&previous),
            &different,
            true
        ));
        assert!(!should_keep_texture_for_volume_install(
            Some(&previous),
            &different,
            false
        ));
    }

    #[test]
    fn history_scope_detects_frames_from_other_sites() {
        let scan_time = Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap();
        let decoded = test_decoded_live_partial(PathBuf::from("KTLX-live"), "KTLX", scan_time, 10);
        let frame = FrameHistoryEntry {
            identity: frame_identity_for_volume(&decoded.volume),
            path: decoded.path,
            volume: Arc::new(decoded.volume),
            timings: Some(decoded.timings),
            status: decoded.status,
            source_label: decoded.source_label,
        };

        assert!(!history_contains_other_site(
            std::slice::from_ref(&frame),
            "KTLX"
        ));
        assert!(history_contains_other_site(&[frame], "KFTG"));
    }

    #[test]
    fn installing_new_site_batch_drops_previous_site_history() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let scan_time = Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap();
        app.upsert_history_frame(test_decoded_live_partial(
            PathBuf::from("KTLX-live"),
            "KTLX",
            scan_time,
            10,
        ));
        app.history_playing = true;

        let ctx = egui::Context::default();
        app.install_decoded_load_batch(
            DecodedLoadBatch {
                frames: vec![test_decoded_live_partial(
                    PathBuf::from("KFTG-live"),
                    "KFTG",
                    scan_time + chrono::Duration::minutes(3),
                    10,
                )],
                selected_index: 0,
            },
            false,
            true,
            &ctx,
        );

        assert_eq!(app.frame_history.len(), 1);
        assert_eq!(app.frame_history[0].identity.site_id, "KFTG");
        assert!(!app.history_playing);
    }

    #[test]
    fn live_update_does_not_steal_selection_while_history_is_playing() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let scan_time = Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap();
        app.upsert_history_frame(test_decoded_live_partial(
            PathBuf::from("KTLX-0130"),
            "KTLX",
            scan_time,
            10,
        ));
        app.upsert_history_frame(test_decoded_live_partial(
            PathBuf::from("KTLX-0133"),
            "KTLX",
            scan_time + chrono::Duration::minutes(3),
            10,
        ));
        app.frame_history
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        app.selected_frame_index = 0;
        app.volume = Some(Arc::clone(&app.frame_history[0].volume));
        app.history_playing = true;

        let ctx = egui::Context::default();
        app.install_decoded_load_batch(
            DecodedLoadBatch {
                frames: vec![test_decoded_live_partial(
                    PathBuf::from("KTLX-0136"),
                    "KTLX",
                    scan_time + chrono::Duration::minutes(6),
                    10,
                )],
                selected_index: 0,
            },
            false,
            true,
            &ctx,
        );

        assert!(app.history_playing);
        assert_eq!(app.selected_frame_index, 0);
        assert_eq!(
            app.frame_history[app.selected_frame_index]
                .identity
                .scan_time_utc,
            scan_time
        );
    }

    #[test]
    fn live_partial_without_selected_velocity_does_not_switch_visible_frame_to_reflectivity() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let scan_time = Utc.with_ymd_and_hms(2026, 6, 8, 1, 30, 0).unwrap();
        let mut previous = test_ref_then_velocity_volume();
        previous.site.id = "KTLX".to_owned();
        previous.volume_time = scan_time;
        let previous_frame = FrameHistoryEntry {
            identity: frame_identity_for_volume(&previous),
            path: PathBuf::from("KTLX-0130"),
            volume: Arc::new(previous.clone()),
            timings: Some(LoadTimings::default()),
            status: FrameStatus::LiveComplete,
            source_label: "realtime L2 KTLX".to_owned(),
        };
        app.volume = Some(Arc::clone(&previous_frame.volume));
        app.selected_cut = 1;
        app.selected_product = DisplayProduct::Moment(MomentType::Velocity);
        app.frame_history.push(previous_frame);

        let mut next = test_reflectivity_sails_volume_with_radials(&[(0.5, 0)], 720);
        next.site.id = "KTLX".to_owned();
        next.volume_time = scan_time + chrono::Duration::minutes(3);

        let ctx = egui::Context::default();
        let selected = app.install_decoded_load_batch(
            DecodedLoadBatch {
                frames: vec![test_decoded_from_volume(
                    PathBuf::from("KTLX-0133-partial"),
                    next,
                    FrameStatus::LivePartial,
                )],
                selected_index: 0,
            },
            true,
            true,
            &ctx,
        );

        assert!(!selected);
        assert_eq!(
            app.selected_product,
            DisplayProduct::Moment(MomentType::Velocity)
        );
        assert_eq!(app.selected_cut, 1);
        assert_eq!(
            app.volume
                .as_ref()
                .map(|volume| volume.volume_time.with_timezone(&Utc)),
            Some(scan_time)
        );
        assert!(app.status.contains("Waiting for VEL"));
        assert_eq!(app.frame_history.len(), 2);
    }

    #[test]
    fn same_scan_unrelated_tilt_growth_reuses_selected_cut_texture() {
        let mut previous = test_reflectivity_sails_volume(&[(0.5, 0)]);
        previous.metadata.source_path = Some("KTLX20260608_010000_RT001_V06".to_owned());
        let mut next = test_reflectivity_sails_volume(&[(0.5, 0), (1.8, 60_000), (2.4, 120_000)]);
        next.metadata.source_path = previous.metadata.source_path.clone();

        assert!(selected_cut_render_data_unchanged(
            Some(&previous),
            &next,
            0,
            &DisplayProduct::Moment(MomentType::Reflectivity)
        ));

        let mut changed_selected_cut = next.clone();
        changed_selected_cut.cuts[0].radials.push(test_radial(
            1.0,
            radar_core::GateRange {
                first_gate_m: 500,
                gate_spacing_m: 250,
                gate_count: 3,
            },
        ));
        assert!(!selected_cut_render_data_unchanged(
            Some(&previous),
            &changed_selected_cut,
            0,
            &DisplayProduct::Moment(MomentType::Reflectivity)
        ));
    }

    #[test]
    fn installing_volume_preserves_user_map_view() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let selected_site =
            RadarSite::new("TEST").with_location(Some("Test".to_owned()), Some(35.0), Some(-97.0));
        app.sites = vec![RadarSite::new("OTHER"), selected_site];
        app.selected_site_index = 0;
        app.map_center_lat = 41.25;
        app.map_center_lon = -101.75;
        app.map_scale = 432.1;

        let ctx = egui::Context::default();
        app.install_volume_arc(
            Arc::new(test_aliased_velocity_volume()),
            None,
            false,
            None,
            FrameStatus::Local,
            &ctx,
        );

        assert_eq!(app.selected_site_index, 1);
        assert_eq!(app.map_center_lat, 41.25);
        assert_eq!(app.map_center_lon, -101.75);
        assert_eq!(app.map_scale, 432.1);
    }

    #[test]
    fn latest_load_clears_different_or_stale_display() {
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 23, 0, 0).unwrap();
        let mut fresh = RadarVolume::new(
            radar_core::RadarSite::new("KTLX"),
            now - chrono::Duration::minutes(5),
        );

        assert!(!should_clear_display_for_latest_load(
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(should_clear_display_for_latest_load(
            Some(&fresh),
            "KGGW",
            now
        ));

        fresh.volume_time = now - chrono::Duration::minutes(16);
        assert!(should_clear_display_for_latest_load(
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(!should_clear_display_before_latest_load(
            LatestLoadMode::AutoRefresh,
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(should_clear_display_before_latest_load(
            LatestLoadMode::User,
            Some(&fresh),
            "KTLX",
            now
        ));
        assert!(!should_clear_display_for_latest_load(None, "KTLX", now));
    }

    #[test]
    fn freshness_ring_color_tracks_scan_age() {
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 23, 0, 0).unwrap();

        let fresh = freshness_ring_color(now - chrono::Duration::minutes(2), now, 210);
        let yellow = freshness_ring_color(now - chrono::Duration::minutes(10), now, 210);
        let red = freshness_ring_color(now - chrono::Duration::minutes(15), now, 210);

        assert_eq!(
            fresh,
            egui::Color32::from_rgba_unmultiplied(65, 238, 104, 210)
        );
        assert_eq!(
            yellow,
            egui::Color32::from_rgba_unmultiplied(238, 218, 62, 210)
        );
        assert_eq!(red, egui::Color32::from_rgba_unmultiplied(246, 76, 48, 210));
    }

    #[test]
    fn freshness_ring_color_preserves_overlay_alpha() {
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 23, 0, 0).unwrap();
        let color = freshness_ring_color(now - chrono::Duration::minutes(20), now, 123);

        assert_eq!(color.a(), 123);
        assert!(color.r() > color.g());
    }

    #[test]
    fn unchanged_realtime_refresh_requires_cache_hit_and_same_path() {
        let current = Path::new("KTLX20260608_003703_RT081_V06");
        let other = Path::new("KTLX20260608_003718_RT081_V06");

        assert!(is_unchanged_realtime_refresh(true, current, Some(current)));
        assert!(!is_unchanged_realtime_refresh(
            false,
            current,
            Some(current)
        ));
        assert!(!is_unchanged_realtime_refresh(true, other, Some(current)));
        assert!(!is_unchanged_realtime_refresh(true, current, None));
    }

    #[test]
    fn live_partial_history_upsert_replaces_same_path_when_radials_increase() {
        let mut app = test_viewer_app_with_hazards(Vec::new());
        let path = PathBuf::from("KTLX20260608_195512_RT035_V06");
        let scan_time = Utc
            .with_ymd_and_hms(2026, 6, 8, 19, 55, 12)
            .single()
            .expect("valid scan time");

        app.upsert_history_frame(test_decoded_live_partial(
            path.clone(),
            "KTLX",
            scan_time,
            120,
        ));
        app.upsert_history_frame(test_decoded_live_partial(path, "KTLX", scan_time, 240));

        assert_eq!(app.frame_history.len(), 1);
        assert_eq!(
            app.frame_history[0].volume.metadata.decoded_radial_count,
            240
        );
    }

    #[test]
    fn direct_viewport_lru_keeps_newest_signatures() {
        let policy = test_cache_policy(4);
        let mut signatures = Vec::new();
        let first = test_viewport_signature(1);
        let second = test_viewport_signature(2);
        let third = test_viewport_signature(3);

        ViewerApp::remember_direct_viewport(&mut signatures, policy, first.clone());
        ViewerApp::remember_direct_viewport(&mut signatures, policy, second.clone());
        ViewerApp::remember_direct_viewport(&mut signatures, policy, third.clone());

        assert_eq!(signatures, vec![second, third]);
        assert!(!ViewerApp::has_direct_viewport(&signatures, &first));
    }

    #[test]
    fn radar_color_image_bulk_copy_preserves_rendered_texels() {
        let rgba = [
            0, 0, 0, 0, //
            255, 32, 16, 255, //
            4, 128, 255, 255, //
            0, 0, 0, 0,
        ];

        let image = radar_color_image_from_rgba([2, 2], &rgba);

        assert_eq!(image.pixels[0].to_array(), [0, 0, 0, 0]);
        assert_eq!(image.pixels[1].to_array(), [255, 32, 16, 255]);
        assert_eq!(image.pixels[2].to_array(), [4, 128, 255, 255]);
        assert_eq!(image.pixels[3].to_array(), [0, 0, 0, 0]);
    }

    #[test]
    fn radar_texture_options_preserve_gate_pixels() {
        let options = radar_texture_options();

        assert_eq!(options.magnification, egui::TextureFilter::Nearest);
        assert_eq!(options.minification, egui::TextureFilter::Nearest);
        assert_eq!(options.wrap_mode, egui::TextureWrapMode::ClampToEdge);
        assert_eq!(options.mipmap_mode, None);
    }

    #[test]
    fn radar_rgba_compatibility_rejects_non_rendered_alpha() {
        assert!(radar_rgba_is_premultiplied_compatible(&[
            0, 0, 0, 0, 16, 32, 48, 255
        ]));
        assert!(!radar_rgba_is_premultiplied_compatible(&[16, 0, 0, 0]));
        assert!(!radar_rgba_is_premultiplied_compatible(&[16, 32, 48, 128]));
    }

    #[test]
    fn metric_series_tracks_latest_percentiles_and_ring_capacity() {
        let mut series = MetricSeries::new();
        series.push(f32::NAN);
        series.push(-1.0);
        assert_eq!(series.summary(), None);

        for sample in 0..100 {
            series.push(sample as f32);
        }

        let summary = series.summary().expect("summary");
        assert_eq!(summary.count, PERF_SAMPLE_CAPACITY);
        assert_eq!(summary.latest, 99.0);
        assert_eq!(summary.min, 4.0);
        assert_eq!(summary.p50, 52.0);
        assert_eq!(summary.p95, 94.0);
        assert_eq!(summary.max, 99.0);
    }

    #[test]
    fn perf_telemetry_splits_direct_and_cached_render_samples() {
        let mut perf = PerfTelemetry::new();

        perf.record_decode(42.0);
        perf.record_render(8.0, false, 9.0, 2.0, Some(11.0));
        perf.record_render(0.5, true, 0.8, 1.5, None);

        assert_eq!(perf.decode.summary().expect("decode").latest, 42.0);
        assert_eq!(perf.direct_render.summary().expect("direct").latest, 8.0);
        assert_eq!(perf.cached_render.summary().expect("cached").latest, 0.5);
        assert_eq!(perf.worker.summary().expect("worker").count, 2);
        assert_eq!(perf.texture.summary().expect("texture").p95, 2.0);
        assert_eq!(
            perf.cache_build.summary().expect("cache build").latest,
            11.0
        );
    }

    #[test]
    fn basemap_regional_packs_have_real_content() {
        assert_eq!(REGIONAL_BASEMAP_LAYERS.len(), 3);
        assert!(basemap_data::BASEMAP_WORLD_COUNTRY_LINES.len() > 1_000);
        assert!(basemap_data::BASEMAP_WORLD_COUNTRY_LINES.len() < 2_000);
        assert!(basemap_data::BASEMAP_US_COUNTY_LINES.len() > 4_000);
        assert!(basemap_data::BASEMAP_US_PLACE_LABELS.len() > 500);

        for layer in REGIONAL_BASEMAP_LAYERS {
            assert!(layer.admin_lines.len() > 50);
            assert!(layer.admin_labels.len() > 10);
            assert!(layer.place_labels.len() > 50);
        }
    }

    #[test]
    fn basemap_detail_layers_are_gated_by_viewport() {
        let central_us = GeoBounds {
            west: -101.0,
            south: 35.0,
            east: -90.0,
            north: 40.0,
        };
        let canada_interior = GeoBounds {
            west: -111.0,
            south: 51.0,
            east: -100.0,
            north: 55.0,
        };
        let mexico_city = GeoBounds {
            west: -101.0,
            south: 18.0,
            east: -97.0,
            north: 21.0,
        };
        let japan_kanto = GeoBounds {
            west: 138.0,
            south: 34.0,
            east: 141.0,
            north: 37.0,
        };
        let alaska = GeoBounds {
            west: -154.0,
            south: 58.0,
            east: -149.0,
            north: 62.0,
        };

        assert!(us_detail_visible(central_us));
        assert!(us_detail_visible(alaska));
        assert!(!us_detail_visible(canada_interior));
        assert!(!us_detail_visible(mexico_city));
        assert!(!us_detail_visible(japan_kanto));

        assert_eq!(active_regional_layer_count(central_us), 0);
        assert_eq!(active_regional_layer_count(canada_interior), 1);
        assert_eq!(active_regional_layer_count(mexico_city), 1);
        assert_eq!(active_regional_layer_count(japan_kanto), 1);
        assert_eq!(active_regional_layer_count(alaska), 0);
    }

    #[test]
    fn basemap_culling_keeps_representative_views_bounded() {
        let central_us = GeoBounds {
            west: -101.0,
            south: 35.0,
            east: -90.0,
            north: 40.0,
        };
        let canada_interior = GeoBounds {
            west: -111.0,
            south: 51.0,
            east: -100.0,
            north: 55.0,
        };
        let japan_kanto = GeoBounds {
            west: 138.0,
            south: 34.0,
            east: 141.0,
            north: 37.0,
        };

        let us_counties =
            basemap_line_candidates(central_us, basemap_data::BASEMAP_US_COUNTY_LINES);
        assert!(us_counties.lines < 400);
        assert!(us_counties.points < 8_000);

        let canada_admin =
            basemap_line_candidates(canada_interior, basemap_data::BASEMAP_CANADA_ADMIN_LINES);
        assert!(canada_admin.lines > 0);
        assert!(canada_admin.lines < 60);
        assert!(canada_admin.points < 5_000);
        assert!(!us_detail_visible(canada_interior));

        let japan_admin =
            basemap_line_candidates(japan_kanto, basemap_data::BASEMAP_JAPAN_ADMIN_LINES);
        assert!(japan_admin.lines > 0);
        assert!(japan_admin.lines < 40);
        assert!(japan_admin.points < 2_000);
        assert!(!us_detail_visible(japan_kanto));
    }

    #[test]
    fn hot_text_summary_selection_keeps_recent_bursts_bounded() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 21, 10, 0)
            .single()
            .expect("valid query time");
        let recent_burst = (0..(HOT_TEXT_PRODUCTS_MAX_PER_TYPE + 4))
            .map(|index| {
                test_nws_product_summary(
                    index,
                    query_time - chrono::Duration::minutes(index as i64),
                )
            })
            .collect::<Vec<_>>();
        let selected = select_hot_text_summaries(recent_burst, query_time);

        assert_eq!(selected.len(), HOT_TEXT_PRODUCTS_MAX_PER_TYPE);
        assert_eq!(selected.first().unwrap().url, "https://example.test/0");
        assert_eq!(
            selected.last().unwrap().url,
            format!(
                "https://example.test/{}",
                HOT_TEXT_PRODUCTS_MAX_PER_TYPE - 1
            )
        );
    }

    #[test]
    fn hot_text_summary_selection_keeps_minimum_for_quiet_types() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 21, 10, 0)
            .single()
            .expect("valid query time");
        let quiet_type = (0..12)
            .map(|index| {
                test_nws_product_summary(
                    index,
                    query_time - chrono::Duration::minutes(180 + index as i64),
                )
            })
            .collect::<Vec<_>>();
        let selected = select_hot_text_summaries(quiet_type, query_time);

        assert_eq!(selected.len(), HOT_TEXT_PRODUCTS_MIN_PER_TYPE);
    }

    #[test]
    fn hazard_parser_extracts_warning_polygon_and_tags() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 4, 21, 16, 25, 0)
            .single()
            .expect("valid query time");
        let records = parse_hazard_records_from_text(
            Path::new("tor.txt"),
            SAMPLE_TORNADO_WARNING,
            Some(query_time),
        );

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_family, "tornado");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.tornado.as_deref(), Some("RADAR INDICATED"));
        assert_eq!(record.hail_inches, Some(1.0));
        assert_eq!(record.points.len(), 6);
        assert_eq!(record.points[0].lat, 42.15);
        assert_eq!(record.points[0].lon, -88.50);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -88.20,
                lat: 42.03
            }
        ));
    }

    #[test]
    fn weather_gov_alert_parser_extracts_live_polygon_shape() {
        let collection: WeatherAlertFeatureCollection =
            serde_json::from_str(SAMPLE_ACTIVE_ALERT_GEOJSON).expect("active alert sample");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 19, 30, 0)
            .single()
            .expect("valid query time");
        let records = parse_weather_alert_feature(&collection.features[0], query_time)
            .expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_family, "tornado");
        assert_eq!(record.label, "TOR 0045 RADAR INDICATED");
        assert_eq!(record.event_id, "KSGF.TO.W.0045");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.severity.as_deref(), Some("Extreme"));
        assert_eq!(record.certainty.as_deref(), Some("Observed"));
        assert_eq!(record.urgency.as_deref(), Some("Immediate"));
        assert_eq!(record.points.len(), 4);
        assert_eq!(record.points[0].lon, -94.10);
        assert_eq!(record.points[0].lat, 37.40);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -94.00,
                lat: 37.33
            }
        ));
    }

    #[test]
    fn hazard_click_prefers_smaller_warning_inside_broad_discussion() {
        let rect = test_map_rect();
        let warning = test_hazard_record(
            "KSHV.SV.W.0200",
            "SVR 0200",
            "severe thunderstorm",
            square_hazard_points(-0.4, -0.4, 0.4, 0.4),
        );
        let discussion = test_hazard_record(
            "spc-md-1014",
            "MD 1014",
            "mesoscale discussion",
            square_hazard_points(-5.0, -5.0, 5.0, 5.0),
        );
        let app = test_viewer_app_with_hazards(vec![warning, discussion]);

        assert_eq!(app.hazard_at_position(rect, rect.center()), Some(0));
    }

    #[test]
    fn hazard_click_tolerance_selects_visible_thin_polygon_edge() {
        let rect = test_map_rect();
        let warning = test_hazard_record(
            "KSGF.TO.W.0045",
            "TOR 0045",
            "tornado",
            square_hazard_points(-0.1, -0.1, 0.1, 0.1),
        );
        let app = test_viewer_app_with_hazards(vec![warning]);
        let right_edge = app.lon_lat_to_screen(rect, 0.1, 0.0);
        let near_edge = right_edge + egui::vec2(HAZARD_CLICK_TOLERANCE_PX - 1.0, 0.0);
        let far_edge = right_edge + egui::vec2(HAZARD_CLICK_TOLERANCE_PX + 2.0, 0.0);

        assert_eq!(app.hazard_at_position(rect, near_edge), Some(0));
        assert_eq!(app.hazard_at_position(rect, far_edge), None);
    }

    #[test]
    fn hazard_geometry_accepts_broad_regional_polygon() {
        let points = square_hazard_points(-101.0, 34.0, -90.0, 41.0);

        assert!(hazard_points_renderable(&points));
    }

    #[test]
    fn hazard_geometry_rejects_cross_country_edge_artifact() {
        let points = square_hazard_points(-74.0, 41.0, 10.0, 42.0);

        assert!(!hazard_points_renderable(&points));
    }

    #[test]
    fn hazard_click_ignores_rejected_artifact_geometry() {
        let rect = test_map_rect();
        let artifact = test_hazard_record(
            "spc-md-artifact",
            "MD BAD",
            "mesoscale discussion",
            square_hazard_points(-74.0, -0.1, 10.0, 0.1),
        );
        let app = test_viewer_app_with_hazards(vec![artifact]);

        assert_eq!(app.hazard_at_position(rect, rect.center()), None);
    }

    #[test]
    fn hazard_click_selects_visible_label_target_for_skinny_polygon() {
        let rect = test_map_rect();
        let warning = test_hazard_record(
            "KFWD.FF.W.0009",
            "FLW 0009",
            "flash flood",
            square_hazard_points(-0.001, -0.1, 0.001, 0.1),
        );
        let app = test_viewer_app_with_hazards(vec![warning]);
        let label_center = app.hazard_screen_centroid(
            rect,
            &app.hazard_overlay.as_ref().unwrap().records[0].points,
        );
        let label_hit = label_center + egui::vec2(HAZARD_CLICK_TOLERANCE_PX + 2.0, 0.0);
        let label_miss = label_center + egui::vec2(HAZARD_LABEL_CLICK_RADIUS_PX + 2.0, 0.0);

        assert_eq!(app.hazard_at_position(rect, label_hit), Some(0));
        assert_eq!(app.hazard_at_position(rect, label_miss), None);
    }

    #[test]
    fn hazard_refresh_ignores_unchanged_overlay_records() {
        let warning = test_hazard_record(
            "KSGF.TO.W.0045",
            "TOR 0045",
            "tornado",
            square_hazard_points(-0.1, -0.1, 0.1, 0.1),
        );
        let mut app = test_viewer_app_with_hazards(vec![warning.clone()]);

        assert!(!app.install_hazard_result(Ok(test_hazard_overlay(vec![warning])), false));
    }

    #[test]
    fn hazard_refresh_preview_does_not_mutate_existing_overlay() {
        let existing = test_hazard_record(
            "KSGF.TO.W.0045",
            "TOR 0045",
            "tornado",
            square_hazard_points(-0.1, -0.1, 0.1, 0.1),
        );
        let incoming = test_hazard_record(
            "KSGF.SV.W.0324",
            "SVR 0324",
            "severe thunderstorm",
            square_hazard_points(0.3, 0.3, 0.5, 0.5),
        );
        let mut app = test_viewer_app_with_hazards(vec![existing]);

        assert!(!app.install_hazard_result(Ok(test_hazard_overlay(vec![incoming])), true));
        assert_eq!(app.hazard_overlay.as_ref().unwrap().records.len(), 1);
    }

    #[test]
    fn hazard_preview_does_not_seed_empty_overlay() {
        let incoming = test_hazard_record(
            "KSGF.SV.W.0324",
            "SVR 0324",
            "severe thunderstorm",
            square_hazard_points(0.3, 0.3, 0.5, 0.5),
        );
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.hazard_overlay = None;

        assert!(!app.install_hazard_result(Ok(test_hazard_overlay(vec![incoming])), true));
        assert!(app.hazard_overlay.is_none());
    }

    #[test]
    fn live_overlay_drops_expired_records() {
        let start = Instant::now();
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 22, 20, 0)
            .single()
            .expect("valid query time");
        let mut active = test_hazard_record(
            "active",
            "SVR 0001",
            "severe thunderstorm",
            square_hazard_points(0.0, 0.0, 1.0, 1.0),
        );
        active.action = "ALERT".to_owned();
        active.source_url = Some("https://api.weather.gov/alerts/active".to_owned());
        let mut expired = test_hazard_record(
            "expired",
            "SVR 0002",
            "severe thunderstorm",
            square_hazard_points(2.0, 2.0, 3.0, 3.0),
        );
        expired.lifecycle_status = Some("Expired".to_owned());

        let overlay = build_live_hazard_overlay(
            "test".to_owned(),
            query_time,
            2,
            2,
            0,
            start,
            vec![expired, active],
        );

        assert_eq!(overlay.records.len(), 1);
        assert_eq!(overlay.records[0].event_id, "active");
    }

    #[test]
    fn live_overlay_drops_unknown_lifecycle_records() {
        let start = Instant::now();
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 22, 20, 0)
            .single()
            .expect("valid query time");
        let mut unknown = test_hazard_record(
            "unknown",
            "SVR 0002",
            "severe thunderstorm",
            square_hazard_points(2.0, 2.0, 3.0, 3.0),
        );
        unknown.lifecycle_status = None;

        let overlay =
            build_live_hazard_overlay("test".to_owned(), query_time, 1, 1, 0, start, vec![unknown]);

        assert!(overlay.records.is_empty());
    }

    #[test]
    fn active_alert_status_wins_over_expired_duplicate_text() {
        let start = Instant::now();
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 22, 20, 0)
            .single()
            .expect("valid query time");
        let mut alert = test_hazard_record(
            "KBYZ.SV.W.0027",
            "SVR 0027",
            "severe thunderstorm",
            square_hazard_points(-1.0, -1.0, 1.0, 1.0),
        );
        alert.action = "ALERT".to_owned();
        alert.lifecycle_status = Some("Active".to_owned());
        let mut text = test_hazard_record(
            "KBYZ.SV.W.0027",
            "SVR 0027",
            "severe thunderstorm",
            square_hazard_points(-20.0, -20.0, 20.0, 20.0),
        );
        text.lifecycle_status = Some("Expired".to_owned());
        text.details.push("Richer text detail".to_owned());

        let overlay = build_live_hazard_overlay(
            "test".to_owned(),
            query_time,
            2,
            2,
            0,
            start,
            vec![alert, text],
        );

        assert_eq!(overlay.records.len(), 1);
        assert_eq!(
            overlay.records[0].lifecycle_status.as_deref(),
            Some("Active")
        );
        assert_eq!(overlay.records[0].details, ["Richer text detail"]);
    }

    #[test]
    fn live_overlay_drops_standalone_product_warning_without_active_alert() {
        let start = Instant::now();
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 9, 1, 41, 0)
            .single()
            .expect("valid query time");
        let mut product = test_hazard_record(
            "KCYS.TO.W.0009",
            "TOR 0009",
            "tornado",
            square_hazard_points(-104.4, 41.5, -104.2, 41.7),
        );
        product.action = "NEW".to_owned();
        product.lifecycle_status = Some("Active".to_owned());
        product.source_url = Some(
            "https://api.weather.gov/products/cc98313c-d3b5-4512-b62d-1bf184c3c7ce".to_owned(),
        );

        let overlay =
            build_live_hazard_overlay("test".to_owned(), query_time, 1, 1, 0, start, vec![product]);

        assert!(overlay.records.is_empty());
    }

    #[test]
    fn product_warning_only_enriches_matching_active_alert() {
        let start = Instant::now();
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 9, 1, 20, 0)
            .single()
            .expect("valid query time");
        let mut alert = test_hazard_record(
            "KCYS.TO.W.0009",
            "TOR 0009",
            "tornado",
            square_hazard_points(-104.4, 41.5, -104.2, 41.7),
        );
        alert.action = "ALERT".to_owned();
        alert.lifecycle_status = Some("Active".to_owned());
        alert.source_url = Some("https://api.weather.gov/alerts/1".to_owned());
        let mut product = test_hazard_record(
            "KCYS.TO.W.0009",
            "TOR 0009",
            "tornado",
            square_hazard_points(-106.0, 40.0, -103.0, 43.0),
        );
        product.action = "NEW".to_owned();
        product.lifecycle_status = Some("Active".to_owned());
        product.source_url = Some(
            "https://api.weather.gov/products/cc98313c-d3b5-4512-b62d-1bf184c3c7ce".to_owned(),
        );
        product.details.push("Richer product text".to_owned());

        let overlay = build_live_hazard_overlay(
            "test".to_owned(),
            query_time,
            2,
            2,
            0,
            start,
            vec![alert.clone(), product],
        );

        assert_eq!(overlay.records.len(), 1);
        assert_eq!(overlay.records[0].bbox, alert.bbox);
        assert_eq!(overlay.records[0].details, ["Richer product text"]);
    }

    #[test]
    fn active_alert_geometry_wins_duplicate_text_record() {
        let mut alert = test_hazard_record(
            "KBYZ.SV.W.0027",
            "SVR 0027",
            "severe thunderstorm",
            square_hazard_points(-1.0, -1.0, 1.0, 1.0),
        );
        alert.action = "ALERT".to_owned();
        alert.source_url = Some("https://api.weather.gov/alerts/1".to_owned());
        let mut text = test_hazard_record(
            "KBYZ.SV.W.0027",
            "SVR 0027",
            "severe thunderstorm",
            square_hazard_points(-20.0, -20.0, 20.0, 20.0),
        );
        text.action = "NEW".to_owned();
        text.details.push("Richer text detail".to_owned());
        let mut records = vec![alert.clone(), text];

        dedupe_hazard_records(&mut records);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].bbox, alert.bbox);
        assert_eq!(records[0].details, ["Richer text detail"]);
    }

    #[test]
    fn nonconvex_warning_polygon_can_be_filled() {
        let points = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(4.0, 0.0),
            egui::pos2(4.0, 4.0),
            egui::pos2(2.0, 2.0),
            egui::pos2(0.0, 4.0),
        ];

        let mesh = filled_polygon_mesh(&points, egui::Color32::from_rgb(255, 200, 0))
            .expect("nonconvex polygon triangulates");

        assert_eq!(mesh.indices.len(), 9);
        assert_eq!(mesh.vertices.len(), 5);
    }

    #[test]
    fn map_projection_equalizes_local_lat_lon_kilometers() {
        let rect = test_map_rect();
        let mut app = test_viewer_app_with_hazards(Vec::new());
        app.map_center_lat = 35.0;
        app.map_center_lon = -97.0;
        app.map_scale = 100.0;

        let center = app.lon_lat_to_screen(rect, -97.0, 35.0);
        let north = app.lon_lat_to_screen(rect, -97.0, 36.0);
        let east = app.lon_lat_to_screen(rect, -97.0 + 1.0 / app.lon_screen_scale(), 35.0);

        assert!((center.distance(north) - center.distance(east)).abs() < 0.01);
    }

    #[test]
    fn stale_radar_texture_rect_moves_with_map_pan() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let rendered = ViewportKey {
            width: 100,
            height: 100,
            radar_x_px: 50 * 8,
            radar_y_px: 50 * 8,
            km_per_px_x: 1_000_000,
            km_per_px_y: 1_000_000,
        };
        let current = ViewportRasterOptions {
            width: 100,
            height: 100,
            radar_x_px: 60.0,
            radar_y_px: 45.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        };

        let image_rect = anchored_radar_texture_rect(rect, 1.0, rendered, current);

        assert!((image_rect.left() - 10.0).abs() < 0.01);
        assert!((image_rect.top() + 5.0).abs() < 0.01);
        assert!((image_rect.width() - 100.0).abs() < 0.01);
        assert!((image_rect.height() - 100.0).abs() < 0.01);
    }

    #[test]
    fn stale_radar_texture_rect_scales_around_site_on_zoom() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let rendered = ViewportKey {
            width: 100,
            height: 100,
            radar_x_px: 50 * 8,
            radar_y_px: 50 * 8,
            km_per_px_x: 1_000_000,
            km_per_px_y: 1_000_000,
        };
        let current = ViewportRasterOptions {
            width: 100,
            height: 100,
            radar_x_px: 50.0,
            radar_y_px: 50.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let image_rect = anchored_radar_texture_rect(rect, 1.0, rendered, current);

        assert!((image_rect.left() + 50.0).abs() < 0.01);
        assert!((image_rect.top() + 50.0).abs() < 0.01);
        assert!((image_rect.width() - 200.0).abs() < 0.01);
        assert!((image_rect.height() - 200.0).abs() < 0.01);
    }

    #[test]
    fn hazard_refresh_selection_matches_event_id_in_new_overlay() {
        let records = vec![
            test_hazard_record(
                "first",
                "TOR 0001",
                "tornado",
                square_hazard_points(-1.0, -1.0, -0.5, -0.5),
            ),
            test_hazard_record(
                "second",
                "SVR 0002",
                "severe thunderstorm",
                square_hazard_points(0.5, 0.5, 1.0, 1.0),
            ),
        ];

        assert_eq!(
            selected_hazard_index_for_event_id(&records, Some("second")),
            Some(1)
        );
        assert_eq!(
            selected_hazard_index_for_event_id(&records, Some("missing")),
            None
        );
        assert_eq!(selected_hazard_index_for_event_id(&records, None), None);
    }

    #[test]
    fn spc_md_product_parser_extracts_compact_polygon_and_click_details() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 19, 30, 0)
            .single()
            .expect("valid query time");
        let record = parse_spc_md_product_page(
            "https://www.spc.noaa.gov/products/md/md1015.html",
            SAMPLE_SPC_MD_HTML,
            query_time,
        )
        .expect("spc md record");

        assert_eq!(record.event_family, "mesoscale discussion");
        assert_eq!(record.label, "MD 1015");
        assert_eq!(
            record.headline.as_deref(),
            Some("Severe potential...Watch unlikely")
        );
        assert_eq!(record.area.as_deref(), Some("portions of the Mid-Atlantic"));
        assert_eq!(
            record.source_url.as_deref(),
            Some("https://www.spc.noaa.gov/products/md/md1015.html")
        );
        assert!(
            record
                .details
                .iter()
                .any(|line| line.contains("Watch issuance 5 percent"))
        );
        assert_eq!(record.points[0].lat, 36.37);
        assert_eq!(record.points[0].lon, -75.80);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -77.2,
                lat: 37.0
            }
        ));
    }

    #[test]
    fn hazard_parser_marks_expired_against_query_time() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 4, 21, 17, 0, 0)
            .single()
            .expect("valid query time");
        let records = parse_hazard_records_from_text(
            Path::new("tor.txt"),
            SAMPLE_TORNADO_WARNING,
            Some(query_time),
        );

        assert_eq!(records[0].lifecycle_status.as_deref(), Some("Expired"));
    }

    #[test]
    fn hazard_parser_extracts_mesoscale_discussion_polygon() {
        let records =
            parse_hazard_records_from_text(Path::new("mcd.txt"), SAMPLE_MESOSCALE_DISCUSSION, None);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "mesoscale discussion");
        assert_eq!(records[0].label, "MD 123");
        assert_eq!(records[0].office, "KWNS");
        assert_eq!(records[0].points.len(), 4);
    }

    #[test]
    fn hazard_parser_extracts_watch_polygon() {
        let records = parse_hazard_records_from_text(Path::new("watch.txt"), SAMPLE_WATCH, None);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "watch");
        assert_eq!(records[0].label, "WATCH 44");
        assert_eq!(records[0].points[0].lon, -97.50);
    }

    fn test_decoded_live_partial(
        path: PathBuf,
        site: &str,
        scan_time: DateTime<Utc>,
        decoded_radials: usize,
    ) -> DecodedLoad {
        let mut volume = RadarVolume::new(radar_core::RadarSite::new(site), scan_time);
        volume.metadata.decoded_radial_count = decoded_radials;
        DecodedLoad {
            path,
            volume,
            timings: LoadTimings::default(),
            status: FrameStatus::LivePartial,
            source_label: format!("realtime L2 {site}"),
        }
    }

    fn test_decoded_from_volume(
        path: PathBuf,
        volume: RadarVolume,
        status: FrameStatus,
    ) -> DecodedLoad {
        let site = volume.site.id.clone();
        DecodedLoad {
            path,
            volume,
            timings: LoadTimings::default(),
            status,
            source_label: format!("realtime L2 {site}"),
        }
    }

    fn test_viewer_app_with_hazards(records: Vec<HazardRecord>) -> ViewerApp {
        let (render_sender, _render_request_receiver) = mpsc::channel::<RenderRequest>();
        let (_render_result_sender, render_receiver) = mpsc::channel::<AsyncRenderResult>();
        let (render_recycle_sender, _render_recycle_receiver) =
            mpsc::channel::<RenderRecycleBuffer>();
        ViewerApp {
            source_path: None,
            volume: None,
            selected_cut: 0,
            selected_product: DisplayProduct::Moment(MomentType::Reflectivity),
            frame_history: Vec::new(),
            selected_frame_index: 0,
            history_frame_limit: DEFAULT_HISTORY_FRAME_LIMIT,
            history_playing: false,
            last_history_step: None,
            color_tables: ColorTableSet::default(),
            flip_velocity_color_polarity: false,
            unfold_velocity_display: true,
            color_table_target: ColorTableFamily::Velocity,
            color_table_path_text: String::new(),
            color_table_status: String::new(),
            texture: None,
            texture_key: None,
            render_sender,
            render_receiver,
            render_recycle_sender,
            pending_render_key: None,
            map_center_lon: 0.0,
            map_center_lat: 0.0,
            map_scale: 100.0,
            radar_range_km: DEFAULT_RADAR_RANGE_KM,
            load_timing: None,
            active_load_started_at: None,
            first_data_ms: None,
            first_texture_ms: None,
            render_ms: None,
            worker_ms: None,
            texture_ms: None,
            sample_cache_build_ms: None,
            basemap_ms: None,
            perf: PerfTelemetry::new(),
            status: String::new(),
            sites: Vec::new(),
            selected_site_index: 0,
            app_settings: settings::AppSettings::default(),
            radar_layers: Vec::new(),
            next_radar_layer_id: 1,
            site_catalog_receiver: None,
            load_receiver: None,
            hazard_receiver: None,
            pending_site_id: None,
            cursor_readout: None,
            placefile_slots: Vec::new(),
            placefile_url_input: String::new(),
            placefile_shape_cache: std::cell::RefCell::new(ShapeCache::new(8)),
            storm_tracks: Vec::new(),
            storm_tracks_site: String::new(),
            next_storm_track_id: 1,
            storm_cells_volume_ptr: 0,
            storm_cells_receiver: None,
            show_storm_tracks: true,
            rotation_markers: Vec::new(),
            rotation_markers_volume_ptr: 0,
            rotation_receiver: None,
            show_rotation_markers: true,
            hail_freezing_level_km: 3.2,
            hail_minus20_level_km: 6.4,
            display_thresholds: BTreeMap::new(),
            show_inspector_card: true,
            pinned_inspector_lonlat: None,
            hazard_overlay_generation: 0,
            grid_layout: PanelLayout::One,
            extra_panes: Vec::new(),
            active_pane: 0,
            basemap_shape_cache: std::cell::RefCell::new(ShapeCache::new(16)),
            hazard_shape_cache: std::cell::RefCell::new(ShapeCache::new(8)),
            cross_section_armed: false,
            cross_section_a_lonlat: None,
            cross_section_b_lonlat: None,
            cross_section_texture: None,
            cross_section_signature: None,
            cross_section_status: "Cross-section: arm, then click endpoint A then B".to_owned(),
            cross_section_top_m: CROSS_SECTION_TOP_M,
            cross_section_user_signature: None,
            cross_section_volume_cuts: 0,
            cross_section_dealias_cache: VolumeDealiasCache::new(),
            hazard_overlay: Some(test_hazard_overlay(records)),
            hazard_path_text: String::new(),
            hazard_status: String::new(),
            hazards_visible: true,
            hazards_active_only: true,
            hazard_fill_alpha: DEFAULT_HAZARD_FILL_ALPHA,
            hidden_hazard_families: default_hidden_hazard_families(),
            realtime_level2_auto_refresh: false,
            display_live_chunk_updates: false,
            last_realtime_level2_refresh: None,
            live_refresh_skip_reason: None,
            live_hazard_auto_refresh: false,
            show_performance_stats: false,
            sidebar_tab: SidebarTab::Radar,
            last_live_hazard_refresh: None,
            selected_hazard_index: None,
            storm_motion_direction_deg: DEFAULT_STORM_MOTION_DIRECTION_DEG,
            storm_motion_speed_kt: DEFAULT_STORM_MOTION_SPEED_KT,
            dealiased_readout_cache: None,
        }
    }

    fn test_hazard_overlay(records: Vec<HazardRecord>) -> HazardOverlay {
        HazardOverlay {
            source_label: "test".to_owned(),
            query_time_utc: None,
            scanned_items: records.len(),
            parsed_items: records.len(),
            polygon_records: records.len(),
            error_count: 0,
            load_ms: 0.0,
            records,
        }
    }

    fn test_hazard_record(
        event_id: &str,
        label: &str,
        event_family: &str,
        points: Vec<HazardPoint>,
    ) -> HazardRecord {
        HazardRecord {
            event_id: event_id.to_owned(),
            label: label.to_owned(),
            event_family: event_family.to_owned(),
            action: "NEW".to_owned(),
            lifecycle_status: Some("Active".to_owned()),
            office: "KOUN".to_owned(),
            headline: None,
            source_url: None,
            area: None,
            motion: None,
            details: Vec::new(),
            valid_start: None,
            valid_end: None,
            severity: None,
            certainty: None,
            urgency: None,
            tornado: None,
            hail_inches: None,
            wind_mph: None,
            damage_threat: None,
            bbox: hazard_bbox(&points),
            points,
        }
    }

    fn square_hazard_points(west: f32, south: f32, east: f32, north: f32) -> Vec<HazardPoint> {
        vec![
            HazardPoint {
                lon: west,
                lat: south,
            },
            HazardPoint {
                lon: east,
                lat: south,
            },
            HazardPoint {
                lon: east,
                lat: north,
            },
            HazardPoint {
                lon: west,
                lat: north,
            },
        ]
    }

    fn test_map_rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 1000.0))
    }

    fn test_nws_product_summary(index: usize, issuance_time: DateTime<Utc>) -> NwsProductSummary {
        NwsProductSummary {
            url: format!("https://example.test/{index}"),
            issuance_time,
        }
    }

    fn test_ref_then_velocity_volume() -> RadarVolume {
        let gate_range = radar_core::GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: 3,
        };
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );

        let mut reflectivity_cut = ElevationCut::new(0.26, Some(1));
        reflectivity_cut
            .radials
            .push(test_radial(0.0, gate_range.clone()));
        let mut reflectivity_grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            2.0,
            66.0,
            Some(0),
            Some(1),
        );
        reflectivity_grid
            .push_u8_row_slice(0, &[66, 80, 90])
            .expect("reflectivity row");
        reflectivity_cut
            .moments
            .insert(MomentType::Reflectivity, reflectivity_grid);
        volume.cuts.push(reflectivity_cut);

        let mut velocity_cut = ElevationCut::new(0.26, Some(2));
        velocity_cut
            .radials
            .push(test_radial(0.0, gate_range.clone()));
        let mut velocity_grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        velocity_grid
            .push_u8_row_slice(0, &[64, 74, 54])
            .expect("velocity row");
        velocity_cut
            .moments
            .insert(MomentType::Velocity, velocity_grid);
        volume.cuts.push(velocity_cut);

        volume
    }

    #[test]
    fn pane_cell_rects_one_is_byte_identical() {
        let outer = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 600.0));
        // The whole point: single-pane must be the full rect, untouched.
        assert_eq!(pane_cell_rects(PanelLayout::One, outer, 2.0), vec![outer]);

        let two = pane_cell_rects(PanelLayout::TwoVertical, outer, 2.0);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].min, outer.min);
        assert_eq!(two[1].max, outer.max);
        assert!(two[1].min.x > two[0].max.x); // left | right with a gap

        let four = pane_cell_rects(PanelLayout::FourGrid, outer, 2.0);
        assert_eq!(four.len(), 4);
        assert_eq!(four[0].min, outer.min);
        assert_eq!(four[3].max, outer.max);
    }

    #[test]
    fn derived_products_are_selectable_from_the_global_list() {
        // Regression guard for the critical bug where CREF/ET/VIL/AzShr/Div
        // compiled + rendered but were unreachable from the picker/keyboard
        // cycle because global_displayable_products collapsed them to their base
        // moment. They must appear: reflectivity products from the REF cut,
        // velocity derivatives from the VEL cut.
        let volume = test_ref_then_velocity_volume();
        let products = global_displayable_products(&volume);
        for d in DerivedProduct::ALL {
            assert!(
                products.contains(&DisplayProduct::Derived(d)),
                "{:?} missing from the selectable product list",
                d
            );
        }
    }

    fn test_reflectivity_sails_volume(cuts: &[(f32, i32)]) -> RadarVolume {
        test_reflectivity_sails_volume_with_radials(cuts, 1)
    }

    fn test_reflectivity_sails_volume_with_radials(
        cuts: &[(f32, i32)],
        radial_count: usize,
    ) -> RadarVolume {
        let cuts = cuts
            .iter()
            .map(|(elevation, offset)| (*elevation, *offset, radial_count))
            .collect::<Vec<_>>();
        test_reflectivity_sails_volume_mixed_radials(&cuts)
    }

    fn test_reflectivity_sails_volume_mixed_radials(cuts: &[(f32, i32, usize)]) -> RadarVolume {
        let gate_range = radar_core::GateRange {
            first_gate_m: 500,
            gate_spacing_m: 250,
            gate_count: 3,
        };
        let mut volume = RadarVolume::new(
            radar_core::RadarSite::new("TEST"),
            chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
        );
        for (index, (elevation_deg, time_offset_ms, radial_count)) in cuts.iter().enumerate() {
            let mut cut = ElevationCut::new(*elevation_deg, Some(index as u8));
            let mut grid = MomentGrid::new_u8(
                MomentType::Reflectivity,
                gate_range.clone(),
                2.0,
                66.0,
                Some(0),
                Some(1),
            );
            for row in 0..*radial_count {
                let mut radial = test_radial(row as f32, gate_range.clone());
                radial.elevation_deg = *elevation_deg;
                radial.time_offset_ms = *time_offset_ms + row as i32;
                cut.radials.push(radial);
                grid.push_u8_row_slice(row, &[66, 80, 90])
                    .expect("reflectivity row");
            }
            cut.moments.insert(MomentType::Reflectivity, grid);
            volume.cuts.push(cut);
        }
        volume
    }

    fn set_cut_azimuth_span(
        volume: &mut RadarVolume,
        cut_index: usize,
        start_deg: f32,
        span_deg: f32,
    ) {
        let Some(cut) = volume.cuts.get_mut(cut_index) else {
            return;
        };
        let radial_count = cut.radials.len().max(1) as f32;
        for (index, radial) in cut.radials.iter_mut().enumerate() {
            radial.azimuth_deg = start_deg + span_deg * (index as f32 / radial_count);
        }
    }

    fn test_aliased_velocity_volume() -> RadarVolume {
        let gate_range = radar_core::GateRange {
            first_gate_m: 0,
            gate_spacing_m: 10_000,
            gate_count: 3,
        };
        let mut site = radar_core::RadarSite::new("TEST");
        site.latitude_deg = Some(35.0);
        site.longitude_deg = Some(-97.0);
        let mut volume = RadarVolume::new(site, chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut radial = test_radial(0.0, gate_range.clone());
        radial.nyquist_velocity_mps = Some(10.0);
        cut.radials.push(radial);

        let mut velocity_grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        velocity_grid
            .push_u8_row_slice(0, &[64, 72, 55])
            .expect("velocity row");
        cut.moments.insert(MomentType::Velocity, velocity_grid);
        volume.cuts.push(cut);
        volume
    }

    fn test_radial(azimuth_deg: f32, gate_range: radar_core::GateRange) -> radar_core::Radial {
        radar_core::Radial {
            azimuth_deg,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range,
            nyquist_velocity_mps: Some(32.0),
            radial_status: None,
        }
    }

    fn test_viewport_signature(width: u32) -> RenderWorkerViewportSignature {
        RenderWorkerViewportSignature::new(
            1,
            width as usize,
            DisplayProduct::Moment(MomentType::Velocity),
            MomentType::Velocity,
            false,
            0,
            (0, 0),
            (32, 64),
            test_viewport_key(width, 100),
        )
    }

    fn test_viewport_key(width: u32, height: u32) -> ViewportKey {
        ViewportKey {
            width,
            height,
            radar_x_px: 0,
            radar_y_px: 0,
            km_per_px_x: 1,
            km_per_px_y: 1,
        }
    }

    fn test_rendered_texture(render_ms: f32, used_sample_cache: bool) -> RenderedTexture {
        test_rendered_texture_with_size(render_ms, used_sample_cache, 720, 480)
    }

    fn test_cache_policy(threads: usize) -> RenderWorkerCachePolicy {
        RenderWorkerCachePolicy {
            threads,
            mode: RenderWorkerCacheMode::Primary,
            min_entries: 1,
        }
    }

    fn test_overlay_cache_policy(threads: usize) -> RenderWorkerCachePolicy {
        RenderWorkerCachePolicy {
            threads,
            mode: RenderWorkerCacheMode::Overlay,
            min_entries: 1,
        }
    }

    #[test]
    fn cache_capacity_scales_with_pane_count() {
        // A quad grid cycles 4 distinct (product, cut) keys through the one
        // shared worker; capacities below 4 would evict another pane's caches
        // on every render. Single-pane stays at the thread-based size.
        let mut policy = test_cache_policy(12); // thread-based capacity 3
        assert_eq!(policy.sample_cache_capacity(), 3);
        policy.note_pane(3); // pane id 3 => 4 panes in use
        assert_eq!(policy.sample_cache_capacity(), 4);
        assert_eq!(policy.moment_cache_capacity(), 4);
        policy.note_pane(99); // capped at the 4-pane grid maximum
        assert_eq!(policy.sample_cache_capacity(), 4);
        // Overlay workers are unaffected.
        let mut overlay = test_overlay_cache_policy(12);
        overlay.note_pane(3);
        assert_eq!(overlay.sample_cache_capacity(), 1);
    }

    fn test_rendered_texture_with_size(
        render_ms: f32,
        used_sample_cache: bool,
        width: u32,
        height: u32,
    ) -> RenderedTexture {
        RenderedTexture {
            width: width as usize,
            height: height as usize,
            rgba: Vec::new(),
            buffer_signature: RenderWorkerViewportSignature::new(
                1,
                1,
                DisplayProduct::Moment(MomentType::Velocity),
                MomentType::Velocity,
                false,
                0,
                (0, 0),
                (32, 64),
                test_viewport_key(width, height),
            ),
            render_ms,
            worker_ms: render_ms,
            sample_cache_build_ms: None,
            used_sample_cache,
            radar_range_km: 460.0,
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct BasemapLineCandidates {
        lines: usize,
        points: usize,
    }

    fn basemap_line_candidates(
        bounds: GeoBounds,
        lines: &[basemap_data::BasemapLine],
    ) -> BasemapLineCandidates {
        let mut candidates = BasemapLineCandidates {
            lines: 0,
            points: 0,
        };
        for line in lines {
            if bounds.intersects_bbox(line.bbox) {
                candidates.lines += 1;
                candidates.points += line.points.len();
            }
        }
        candidates
    }

    fn active_regional_layer_count(bounds: GeoBounds) -> usize {
        REGIONAL_BASEMAP_LAYERS
            .iter()
            .filter(|layer| bounds.intersects_bbox(layer.bounds))
            .count()
    }

    const SAMPLE_TORNADO_WARNING: &str = r#"401
WUUS53 KLOT 211600
TORLOT
ILC031-043-197-211630-
/O.NEW.KLOT.TO.W.0001.260421T1600Z-260421T1630Z/

BULLETIN - EAS ACTIVATION REQUESTED
Tornado Warning
National Weather Service Chicago IL
1100 AM CDT Tue Apr 21 2026

LAT...LON 4215 8850 4203 8820 4194 8810 4198 8786 4213 8784 4222 8839
TIME...MOT...LOC 1600Z 265DEG 31KT 4208 8837
TORNADO...RADAR INDICATED
MAX HAIL SIZE...1.00 IN

$$
"#;

    const SAMPLE_MESOSCALE_DISCUSSION: &str = r#"ACUS11 KWNS 211600
SWOMCD
SPC MCD 211600

Mesoscale Discussion 0123
NWS Storm Prediction Center Norman OK
1100 AM CDT Tue Apr 21 2026

Areas affected...northern Illinois

LAT...LON 4215 8850 4194 8810 4198 8786 4222 8839

$$
"#;

    const SAMPLE_WATCH: &str = r#"WWUS20 KWNS 211600
SEL4
SPC WW 211600

URGENT - IMMEDIATE BROADCAST REQUESTED
Tornado Watch Number 44
NWS Storm Prediction Center Norman OK
1100 AM CDT Tue Apr 21 2026

WATCH OUTLINE UPDATE FOR WS 44
LAT...LON 3500 9750 3520 9500 3350 9440 3320 9700

$$
"#;

    const SAMPLE_ACTIVE_ALERT_GEOJSON: &str = r#"{
  "features": [
    {
      "id": "urn:test:tor",
      "geometry": {
        "type": "Polygon",
        "coordinates": [[
          [-94.10, 37.40],
          [-93.90, 37.38],
          [-93.92, 37.25],
          [-94.12, 37.26],
          [-94.10, 37.40]
        ]]
      },
      "properties": {
        "id": "urn:test:tor",
        "event": "Tornado Warning",
        "senderName": "NWS Springfield MO",
        "headline": "Tornado Warning issued June 7 at 2:09PM CDT until June 7 at 3:00PM CDT by NWS Springfield MO",
        "effective": "2026-06-07T14:09:00-05:00",
        "expires": "2026-06-07T15:00:00-05:00",
        "ends": "2026-06-07T15:00:00-05:00",
        "severity": "Extreme",
        "certainty": "Observed",
        "urgency": "Immediate",
        "parameters": {
          "VTEC": ["/O.NEW.KSGF.TO.W.0045.260607T1909Z-260607T2000Z/"],
          "tornadoDetection": ["RADAR INDICATED"],
          "maxHailSize": ["0.00"]
        }
      }
    }
  ]
}"#;

    const SAMPLE_SPC_MD_HTML: &str = r#"<html><body><pre>
   Mesoscale Discussion 1015
   NWS Storm Prediction Center Norman OK
   0159 PM CDT Sun Jun 07 2026

   Areas affected...portions of the Mid-Atlantic

   Concerning...Severe potential...Watch unlikely

   Valid 071859Z - 072100Z
   Probability of Watch Issuance...5 percent

   SUMMARY...Widely scattered thunderstorms may pose a localized risk
   for strong/damaging wind gusts and perhaps small hail this
   afternoon. Watch issuance is not expected.

   LAT...LON   36377580 36277612 36247691 36287734 36357769 36547819
               36707854 36887880 37087908 37467941 37857947 38347939
               38487907 38427845 38227760 38097690 37967599 37867534
               37727542 37317567 36987586 36747583 36497571 36377580

   MOST PROBABLE PEAK WIND GUST...UP TO 60 MPH
</pre></body></html>"#;
}
