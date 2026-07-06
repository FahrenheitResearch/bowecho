//! Persisted application settings: a small JSON document under the platform
//! config directory. Loading is best-effort — a missing or unreadable file
//! yields defaults so the app always starts.

fn default_true() -> bool {
    true
}

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

mod brand;
pub use brand::*;

/// Product families used by the loop sweep controller. Keeping this model in
/// `settings` lets the primary radar, extra panes, and coordinated overlays
/// share one persisted policy without making the settings crate depend on UI
/// or radar-rendering types.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepProductGroup {
    #[default]
    Reflectivity,
    Velocity,
    DualPol,
    Other,
}

impl SweepProductGroup {
    pub const ALL: [Self; 4] = [
        Self::Reflectivity,
        Self::Velocity,
        Self::DualPol,
        Self::Other,
    ];
}

/// How a product family contributes cuts to an expanded radar timeline.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepPolicyMode {
    /// Do not expand a volume into per-cut timeline steps for this family.
    Off,
    /// Preserve BowEcho's existing complete-low-tilt behavior.
    #[default]
    AllLow,
    BaseOnly,
    SameLevel,
    /// Include complete, product-compatible cuts inside the inclusive range.
    Range,
}

/// One product-family sweep rule. Elevations are stored in hundredths of a
/// degree so `AppSettings` remains `Eq` and JSON round trips without float
/// noise.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SweepPolicy {
    pub mode: SweepPolicyMode,
    pub min_elevation_cdeg: u16,
    pub max_elevation_cdeg: u16,
}

impl SweepPolicy {
    pub fn range_cdeg(min_elevation_cdeg: u16, max_elevation_cdeg: u16) -> Self {
        Self {
            mode: SweepPolicyMode::Range,
            min_elevation_cdeg,
            max_elevation_cdeg,
        }
        .normalized()
    }

    pub fn normalized(mut self) -> Self {
        if self.min_elevation_cdeg > self.max_elevation_cdeg {
            std::mem::swap(&mut self.min_elevation_cdeg, &mut self.max_elevation_cdeg);
        }
        self
    }

    pub fn min_elevation_deg(self) -> f32 {
        f32::from(self.min_elevation_cdeg) / 100.0
    }

    pub fn max_elevation_deg(self) -> f32 {
        f32::from(self.max_elevation_cdeg) / 100.0
    }
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            mode: SweepPolicyMode::AllLow,
            min_elevation_cdeg: 0,
            max_elevation_cdeg: 140,
        }
    }
}

/// Product-family rules for one radar display target. Missing groups inherit
/// from the caller's fallback (legacy global mode for Primary, Primary for an
/// overriding extra pane).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SweepPolicySet {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub product_groups: BTreeMap<SweepProductGroup, SweepPolicy>,
}

/// Advanced loop sweep configuration. Missing extra-pane entries mean
/// "Follow primary"; map keys are zero-based extra-pane slots (0 = Pane 2).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopSweepControl {
    pub primary: SweepPolicySet,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_pane_overrides: BTreeMap<u8, SweepPolicySet>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// App identity / Brand Kit. The untouched BowEcho value is omitted so
    /// existing config.json files stay byte-small and backwards compatible.
    #[serde(default, skip_serializing_if = "BrandConfig::is_default")]
    pub brand: BrandConfig,
    /// Site to load on startup (e.g. "KEAX"). None = built-in default.
    pub startup_site: Option<String>,
    /// The primary display owner from the last session as ONE encoded
    /// `SiteRef` settings key (`"KTLX"` | `"intl:smhi:angelholm"`, case
    /// preserved for `:`-keys). Absent = pre-v0.29 behavior (bare
    /// `startup_site` only; intl sessions resumed as a stale US site).
    /// New key only — a v0.29 file opened by v0.28 ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_owner_site: Option<String>,
    /// Favorite site ids, in user order.
    pub favorites: Vec<String>,
    /// Live auto-refresh poll interval (seconds).
    pub polling_interval_seconds: u64,
    /// Selected color table per family label (family.label() -> table name).
    /// Resolved on startup against built-ins ∪ user tables; a missing name
    /// (deleted .pal) falls back to the family default.
    pub palette_by_family: BTreeMap<String, String>,
    /// Per-product overrides (product label -> table name), beating the
    /// family binding for just that product (e.g. SRV vs VEL — both the
    /// Velocity family).
    #[serde(default)]
    pub palette_by_product: BTreeMap<String, String>,
    /// Active built-in/user appearance profile name. The live style document
    /// remains the source of truth; this records which profile it started
    /// from so the UI can show "modified" after manual tweaks.
    #[serde(default = "default_style_profile")]
    pub style_profile: String,
    /// Multi-pane grid layout pane count from the last session (1, 2, 3, or 4).
    pub grid_pane_count: usize,
    /// When true, focused extra panes own their radar source/history/load
    /// controls instead of always following the primary pane.
    #[serde(default)]
    pub independent_panels: bool,
    /// Explicit per-pane site pins, keyed by zero-based extra-pane slot
    /// (0 = Pane 2), encoded as `SiteRef` settings keys (`"KTLX"` |
    /// `"intl:smhi:angelholm"`; bare ids parse as US forever, `:`-keys
    /// keep their case). Absent = pre-v0.29 behavior (no pin restore).
    /// New key only — a v0.29 file opened by v0.28 ignores it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_pane_pins: BTreeMap<u8, String>,
    /// Placefile URLs (GRLevelX-style overlays) with per-file enable flags.
    pub placefiles: Vec<PlacefileEntry>,
    /// Default overlay toggles, restored at startup (user request: "let
    /// people save what overlays they want default").
    #[serde(default)]
    pub overlay_obs: bool,
    #[serde(default = "default_true")]
    pub overlay_obs_metar: bool,
    #[serde(default = "default_true")]
    pub overlay_obs_mesonet: bool,
    #[serde(default)]
    pub overlay_glm: bool,
    /// GOES GLM display filter: draw flashes from the trailing N minutes.
    /// This is separate from the fixed live/stale newest-flash health gate.
    #[serde(default = "default_glm_show_last_minutes")]
    pub glm_show_last_minutes: u16,
    /// RAOB launch-site markers (the observed-soundings obs layer) —
    /// default off; clicking a marker fetches that station's sounding at
    /// the displayed radar time.
    #[serde(default)]
    pub overlay_raob: bool,
    /// Draw the tropical-cyclone layer: active storm positions, forecast
    /// track, and cone of uncertainty (NHC CurrentStorms.json + GDACS).
    /// Default off.
    #[serde(default)]
    pub show_tropical: bool,
    /// Enabled outlook kinds ("cat", "torn", "wind", "hail", "estofex").
    #[serde(default)]
    pub overlay_spc_outlooks: Vec<String>,
    #[serde(default)]
    pub overlay_spc_reports: bool,
    #[serde(default)]
    pub overlay_mping_reports: bool,
    /// Basemap style key: "dark" (vector), "satellite", "streets", "topo".
    #[serde(default = "default_basemap_style")]
    pub basemap_style: String,
    /// Basemap boundary-line brightness percentage. 100 preserves the
    /// shipped look; lower values dim satellite/administrative outlines.
    #[serde(default = "default_basemap_line_brightness_percent")]
    pub basemap_line_brightness_percent: u16,
    /// Basemap boundary-line thickness percentage. 100 preserves the
    /// shipped look.
    #[serde(default = "default_basemap_line_thickness_percent")]
    pub basemap_line_thickness_percent: u16,
    /// Low-end rendering mode for the basemap: skip dense county/regional
    /// administrative detail while keeping countries, states, cities, and
    /// radar/weather layers intact.
    #[serde(default)]
    pub basemap_lightweight: bool,
    /// GR2-style bold town labels (white, heavy halo) readable over echoes.
    #[serde(default = "default_bold_labels")]
    pub bold_labels: bool,
    /// Draw latitude/longitude graticule lines and edge labels on the basemap.
    #[serde(default = "default_true")]
    pub show_lat_lon_grid: bool,
    /// Draw CONUS radar site/TDWR labels next to radar markers. Markers
    /// stay visible/clickable when labels are off.
    #[serde(default = "default_true")]
    pub show_radar_labels: bool,
    /// Radar marker label style: "full-box", "id-box", or "text".
    #[serde(default = "default_radar_label_style")]
    pub radar_label_style: String,
    /// Draw compact warning-polygon labels such as "SVR 0653" on the map.
    /// Polygons remain visible/clickable when labels are off.
    #[serde(default = "default_true")]
    pub show_hazard_labels: bool,
    /// Map right-click: false (default) = open the lowest-beam radar menu;
    /// true = switch straight to the closest WSR-88D, no menu (field
    /// request: "i might sometimes want right click to just load closest
    /// radar").
    #[serde(default)]
    pub right_click_loads_nearest: bool,
    /// Plain left-click on empty map space drops a coordinate marker.
    /// Default off so normal map clicks do not leave accidental markers.
    #[serde(default)]
    pub map_click_drops_coordinate_marker: bool,
    /// Reflectivity gate filter threshold in deci-dBZ; None = off. Hides
    /// non-REF gates whose co-located reflectivity is weaker (GR2-style
    /// GateFilter).
    #[serde(default)]
    pub gate_filter_decidbz: Option<i16>,
    /// Model store retention: keep the newest N runs (0 = unlimited).
    /// Default 2 so lightweight users never accumulate SSD bloat.
    #[serde(default = "default_model_keep_runs")]
    pub model_keep_runs: u8,
    /// Perf HUD: floating per-frame timing overlay on the map (decode /
    /// render / layer raster / FPS / time-to-first-pixels). Debug aid,
    /// default off.
    #[serde(default)]
    pub perf_hud: bool,
    /// Flash/mark newly issued, current warnings until the operator clicks or
    /// acknowledges them. Default on to preserve the existing documentation
    /// workflow.
    #[serde(default = "default_true")]
    pub alert_flash_enabled: bool,
    /// Warning families that may latch the visual NEW/flash state. Empty =
    /// all supported warning families.
    #[serde(default)]
    pub alert_flash_families: Vec<String>,
    /// Play an operator alert sound when a newly issued, current warning is
    /// latched in the Severe tab. Default off so existing installs stay quiet.
    #[serde(default)]
    pub alert_sound_enabled: bool,
    /// Optional custom `.wav` file for the warning alert. Empty = platform
    /// system alert sound.
    #[serde(default)]
    pub alert_sound_path: String,
    /// Warning families that may play the alert. Empty = all supported
    /// warning families, keeping old configs sparse.
    #[serde(default)]
    pub alert_sound_families: Vec<String>,
    /// Current-alert list sort mode in the Severe tab. Kept as a string so
    /// older/newer builds can preserve unknown operator preferences.
    #[serde(default = "default_current_alert_sort")]
    pub current_alert_sort: String,
    /// Current-alert list type filter in the Severe tab. "all" preserves
    /// the existing family checkbox behavior; specific keys provide fast
    /// one-control narrowing such as TOR-only newest-first.
    #[serde(default = "default_current_alert_filter")]
    pub current_alert_filter: String,
    /// Maximum SCIT storm cells fed into the storm-track associator each scan.
    /// Lower values reduce linear-mode clutter without touching radar render
    /// resolution or algorithm caches.
    #[serde(default = "default_storm_track_max_tracks")]
    pub storm_track_max_tracks: u16,
    /// Minimum peak composite reflectivity for storm-track cells, in
    /// tenths of dBZ.
    #[serde(default = "default_storm_track_min_dbz_tenths")]
    pub storm_track_min_dbz_tenths: u16,
    /// Product hotkeys: number-row key ("0"-"9") or letter ("A"-"Z") ->
    /// product label (e.g.
    /// "REF", "VEL", "SRV", "RHO", "ZDR", "SW", "CREF", "ET", "VIL", "VILD",
    /// "PHI", "KDP", "AzShr", "Div"). Edit in config.json to customize.
    pub product_hotkeys: BTreeMap<String, String>,
    /// Legacy display-smoothing flag (the old Settings ▸ Display ▸ Smooth
    /// display checkbox). Superseded by `smooth_display_mode` but still
    /// READ (an old config with `smooth_display=true` maps to "soften")
    /// and still WRITTEN (true for any non-native mode) so older builds
    /// opening a newer config keep a smoothed look.
    #[serde(default)]
    pub smooth_display: bool,
    /// Display smoothing mode (Settings ▸ Display ▸ Smoothing):
    /// "native" (no smoothing), "soften" (3×3 binomial over the polar
    /// grid — the legacy Smooth display), or "interpolated" (bilinear
    /// polar upsampling — inter-gate interpolation). Empty = derive from
    /// the legacy `smooth_display` bool, so old configs keep their
    /// setting unchanged.
    #[serde(default)]
    pub smooth_display_mode: String,
    /// Vertical cross-section cleanup mode: "smoothed" fills short path
    /// sampling gaps and blends horizontally; "native" shows raw section
    /// samples. Smoothed is the historical/default behavior.
    #[serde(default = "default_cross_section_smoothing")]
    pub cross_section_smoothing: String,
    /// Loop playback speed in percent of the 700 ms/frame baseline
    /// (100 = baseline, 200 = twice as fast). Screen playback only —
    /// media exports have their own speed control.
    #[serde(default = "default_loop_speed_percent")]
    pub loop_speed_percent: u16,
    /// Scroll-wheel zoom speed in percent. 100 keeps the classic
    /// pre-v0.28.1 step; the 150 default zooms half again faster (field
    /// feedback). Applied as an exponent on the per-notch zoom factor so
    /// the feel scales smoothly in log-zoom space.
    #[serde(default = "default_zoom_speed_percent")]
    pub zoom_speed_percent: u16,
    /// Primary radar-history memory budget, in GiB. Big super-res loops (a
    /// Cat-5 hurricane at 0.5°/250 m can run ~60-100 MB per decoded volume)
    /// otherwise silently trim to whatever fits the default 8 GiB. Raising
    /// this lets high-RAM machines hold longer loops; lowering it protects
    /// small machines. Clamped to the offered choices (4/8/16/24/32).
    #[serde(default = "default_radar_history_budget_gib")]
    pub radar_history_budget_gib: u16,
    /// During loop playback, step through complete low-level SAILS/MESO-SAILS
    /// sweeps inside each volume before moving to the next volume. The loop
    /// frame count remains the scan/volume count shown in the UI.
    #[serde(default)]
    pub loop_low_sweeps: bool,
    /// Low-sweep loop cut filter: "all" keeps every complete low-level cut;
    /// "base" keeps only the lowest elevation bucket to avoid TDWR/Sails jumps.
    #[serde(default = "default_loop_low_sweep_filter")]
    pub loop_low_sweep_filter: String,
    /// Optional product- and pane-specific sweep rules. `None` is the legacy
    /// v0.25.7 behavior driven only by `loop_low_sweeps` and
    /// `loop_low_sweep_filter`; it is omitted from JSON so existing configs
    /// remain byte-small and load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_sweep_control: Option<LoopSweepControl>,
    /// Remember the last selected tilt independently for each radar product.
    #[serde(default = "default_true")]
    pub remember_product_tilts: bool,
    /// Show derived products in the product picker and keyboard product cycle.
    /// Default on to preserve existing installs; users can hide CREF/ET/VIL
    /// and computed advanced products when they want a raw-moment-only row.
    #[serde(default = "default_true")]
    pub show_derived_products: bool,
    /// During live updates, advance to each newly completed low-level sweep
    /// instead of waiting for a scan-separated low-level revisit.
    #[serde(default)]
    pub live_low_sweep_auto_advance: bool,
    /// Minimum age gap, in seconds, before live display advances to a newer
    /// complete low-level sweep within the same scan.
    #[serde(default = "default_live_low_sweep_auto_advance_seconds")]
    pub live_low_sweep_auto_advance_seconds: u16,
    /// Draw a small screen-center capture reticle over map panes.
    #[serde(default)]
    pub show_center_crosshair: bool,
    /// Free/manual viewport recording frame rate. Loop exports still preserve
    /// their own export-speed cadence; this controls the live viewport recorder.
    #[serde(default = "default_record_fps")]
    pub record_fps: u16,
    /// Loop export speed in percent of the 700 ms/frame baseline. Kept
    /// separate from screen playback speed so operators can scrub at 64x
    /// without accidentally creating one-second share loops, but can still
    /// intentionally compress very long low-sweep exports.
    #[serde(default = "default_loop_record_speed_percent")]
    pub loop_record_speed_percent: u16,
    /// Legacy symmetric event-loop context. New configurations write the
    /// explicit before/after fields below; keeping this value lets older JSON
    /// (including custom non-default padding) migrate without losing intent.
    #[serde(default = "default_event_pad_frames")]
    pub event_pad_frames: u16,
    /// Explicit scans before a clicked event/report/track window. `None`
    /// inherits `event_pad_frames`, preserving old symmetric configurations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_before_scans: Option<u16>,
    /// Explicit scans after a clicked event/report/track window. `None`
    /// inherits `event_pad_frames`, preserving old symmetric configurations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_after_scans: Option<u16>,
    /// Hard cap for an event-click archive loop. This is intentionally
    /// independent of the Archive browser's manual `archive_frame_count`.
    #[serde(default = "default_event_max_frames")]
    pub event_max_frames: u16,
    /// Automatically load valid-time warnings for the actual radar loop
    /// window produced by an event click.
    #[serde(default = "default_true")]
    pub event_sync_warnings: bool,
    /// When a tornado track is clicked from the Event day layer, optionally
    /// kick a sounding-grade model ingest for the init covering that track
    /// time so skew-T soundings are ready without manual date/cycle math.
    #[serde(default = "default_true")]
    pub event_track_auto_model: bool,
    /// Model slug for `event_track_auto_model`. Kept separate from the
    /// Download panel's general model pick because a user may prefer HRRR
    /// for storm events while browsing other model products elsewhere.
    #[serde(default = "default_model_slug")]
    pub event_track_model_slug: String,
    /// When a tornado track click loads an archive loop, keep the camera
    /// centered on the estimated tornado position as frames advance.
    #[serde(default = "default_true")]
    pub event_track_camera_follow: bool,
    /// Archive browser click mode: true = load a loop ending at the chosen
    /// scan, false = load only that scan.
    #[serde(default = "default_true")]
    pub archive_load_loop: bool,
    /// Archive browser "Fetch N scans" control. Clamped by the UI to its
    /// supported range when read.
    #[serde(default = "default_archive_frame_count")]
    pub archive_frame_count: u16,
    /// After an explicit live "Load Latest", backfill this many prior
    /// Level II frames in the background so a short loop is available
    /// almost immediately. Auto-refresh does not use this.
    #[serde(default = "default_live_preload_frame_count")]
    pub live_preload_frame_count: u16,
    /// Last GR2A-style poll URL (mobile/research radar feeds) — typing it
    /// once per deployment is fine, once per session is not.
    #[serde(default)]
    pub poll_url: String,
    /// User-saved GR2A-style poll roots plus map positions for private,
    /// lab, or field radars that are not in BowEcho's built-in catalogs.
    #[serde(default)]
    pub custom_poll_links: Vec<CustomPollLinkEntry>,
    /// Last international live-feed selection, mirroring `poll_url`: the
    /// data_source international provider id (e.g. "smhi") plus its
    /// provider-scoped site id (e.g. "angelholm"), so the DATA tab's
    /// International Start can resume the feed next session.
    #[serde(default)]
    pub intl_provider: String,
    #[serde(default)]
    pub intl_site: String,
    /// FARM quicklook map-drape georeferences, one per sensor id —
    /// auto-located or manually pinned deployment positions survive
    /// restarts (re-located automatically when the scan id changes).
    #[serde(default)]
    pub farm_georefs: Vec<FarmGeorefEntry>,
    /// Dockable-workspace layout: versioned JSON ({"version", "tree",
    /// "viewers", "prefer_docked"}) built and parsed by app_ui/src/dock.rs
    /// — opaque here so settings stays UI-crate-free. None = the default
    /// map-only layout; parse failures fall back to it (best-effort, like
    /// everything else in this file).
    #[serde(default)]
    pub workspace_layout: Option<serde_json::Value>,
    /// Native model sounding view/layout/style state, built by rw-ui and
    /// kept opaque here so settings stays independent of egui/rw-ui.
    #[serde(default)]
    pub sounding_view_state: Option<serde_json::Value>,
    /// User-edited model field-plot color tables (rw-ui `StyleOverrideSettings`,
    /// serialized). Kept opaque here for the same reason as the sounding state.
    #[serde(default)]
    pub model_style_overrides: Option<serde_json::Value>,
    /// Data-folder override: where caches and stores live (Level II
    /// cache, model/sat/GLM stores, tiles, georefs). Empty = platform
    /// default. Read once at startup; Settings says "restart to apply".
    #[serde(default)]
    pub data_dir: String,
    /// Sidebar section open/closed memory (section id -> open). eframe is
    /// built without the `persistence` feature, so egui's own collapsing
    /// state dies with the process — this map is what survives restarts.
    #[serde(default)]
    pub sidebar_section_open: BTreeMap<String, bool>,
    /// Last-used NWP model slug for the Download panel / one-click ingest
    /// ("hrrr", "gfs", ...). Unknown or no-longer-supported slugs fall
    /// back to HRRR at use sites.
    #[serde(default = "default_model_slug")]
    pub model_slug: String,
    /// Readout unit system: "imperial" (the default — US-born app) or
    /// "metric". Unknown values read as imperial at use sites
    /// (app_ui/src/units.rs); kept as a string so `AppSettings` stays
    /// UI-crate-free and Eq-derivable.
    #[serde(default = "default_units")]
    pub units: String,
    /// Operator-facing timestamp display zone. "utc" preserves the
    /// historical BowEcho behavior; app_ui also accepts US zones such as
    /// "eastern", "central", "mountain", and "pacific". Data fetch keys
    /// remain UTC regardless of this display preference.
    #[serde(default = "default_time_zone")]
    pub time_zone: String,
}

fn default_units() -> String {
    "imperial".to_owned()
}

fn default_time_zone() -> String {
    "utc".to_owned()
}

fn default_model_slug() -> String {
    "hrrr".to_owned()
}

fn default_loop_speed_percent() -> u16 {
    100
}

fn default_loop_low_sweep_filter() -> String {
    "all".to_owned()
}

fn default_record_fps() -> u16 {
    30
}

fn default_loop_record_speed_percent() -> u16 {
    100
}

fn default_storm_track_max_tracks() -> u16 {
    16
}

fn default_storm_track_min_dbz_tenths() -> u16 {
    350
}

fn default_cross_section_smoothing() -> String {
    "smoothed".to_owned()
}

fn default_event_pad_frames() -> u16 {
    5
}

fn default_zoom_speed_percent() -> u16 {
    150
}

/// Default primary radar-history memory budget in GiB (matches the engine's
/// historical `PRIMARY_HISTORY_BYTE_BUDGET`). See `radar_history_budget_gib`.
pub fn default_radar_history_budget_gib() -> u16 {
    8
}

fn default_event_max_frames() -> u16 {
    24
}

fn default_archive_frame_count() -> u16 {
    10
}

fn default_live_preload_frame_count() -> u16 {
    5
}

fn default_glm_show_last_minutes() -> u16 {
    5
}

/// A persisted FARM drape georeference. Coordinates are stored as scaled
/// integers (microdegrees etc.) so `AppSettings` stays `Eq`-derivable.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FarmGeorefEntry {
    pub sensor_id: u32,
    /// Radar deployment latitude/longitude in microdegrees.
    pub lat_e6: i64,
    pub lon_e6: i64,
    /// Quicklook image scale in millipixels per km.
    pub px_per_km_e3: i64,
    /// Radar pixel in the quicklook, tenths of a pixel.
    pub radar_px_x_e1: i64,
    pub radar_px_y_e1: i64,
    /// Tick-lattice spacing in meters (the plot's "Nkm ticks").
    pub tick_m: u32,
    /// Scan id the fix belongs to (deployment moves get a new id).
    pub scan_id: String,
    /// True when the user pinned the position by hand.
    pub manual: bool,
}

/// A user-saved custom radar poll root and marker. Coordinates are stored
/// as microdegrees so `AppSettings` remains `Eq`-derivable.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomPollLinkEntry {
    /// Human label shown in the DATA tab and map hover chip.
    pub label: String,
    /// Optional short site id, e.g. ARMOR or FWLX.
    pub site_id: String,
    /// Marker latitude/longitude in microdegrees.
    pub lat_e6: i64,
    pub lon_e6: i64,
    /// GR2A-style root URL. BowEcho polls `{poll_url}/dir.list`, or a
    /// single site discovered from `{poll_url}/grlevel2.cfg`.
    pub poll_url: String,
}

/// A persisted placefile reference.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlacefileEntry {
    pub url: String,
    pub enabled: bool,
    /// Draw the file's Text/Place statements (names above icons). Off =
    /// dots only (the SpotterNetwork preference).
    #[serde(default = "default_true")]
    pub show_text: bool,
}

/// Default number-row bindings (the classic analyst loadout).
pub fn default_product_hotkeys() -> BTreeMap<String, String> {
    [
        ("1", "REF"),
        ("2", "VEL"),
        ("3", "SRV"),
        ("4", "RHO"),
        ("5", "ZDR"),
        ("6", "SW"),
        ("7", "CREF"),
        ("8", "ET"),
        ("9", "VIL"),
        ("0", "VILD"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn default_model_keep_runs() -> u8 {
    2
}

fn default_style_profile() -> String {
    "BowEcho default".to_owned()
}

pub fn default_live_low_sweep_auto_advance_seconds() -> u16 {
    60
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            brand: BrandConfig::default(),
            overlay_obs: false,
            overlay_obs_metar: true,
            overlay_obs_mesonet: true,
            overlay_glm: false,
            glm_show_last_minutes: default_glm_show_last_minutes(),
            overlay_raob: false,
            show_tropical: false,
            overlay_spc_outlooks: Vec::new(),
            overlay_spc_reports: false,
            overlay_mping_reports: false,
            startup_site: None,
            display_owner_site: None,
            favorites: Vec::new(),
            polling_interval_seconds: 60,
            palette_by_family: BTreeMap::new(),
            palette_by_product: BTreeMap::new(),
            style_profile: default_style_profile(),
            grid_pane_count: 1,
            independent_panels: false,
            extra_pane_pins: BTreeMap::new(),
            placefiles: Vec::new(),
            basemap_style: default_basemap_style(),
            basemap_line_brightness_percent: default_basemap_line_brightness_percent(),
            basemap_line_thickness_percent: default_basemap_line_thickness_percent(),
            basemap_lightweight: false,
            bold_labels: default_bold_labels(),
            show_lat_lon_grid: true,
            show_radar_labels: true,
            radar_label_style: default_radar_label_style(),
            show_hazard_labels: true,
            right_click_loads_nearest: false,
            map_click_drops_coordinate_marker: false,
            gate_filter_decidbz: None,
            model_keep_runs: default_model_keep_runs(),
            perf_hud: false,
            alert_flash_enabled: true,
            alert_flash_families: Vec::new(),
            alert_sound_enabled: false,
            alert_sound_path: String::new(),
            alert_sound_families: Vec::new(),
            current_alert_sort: default_current_alert_sort(),
            current_alert_filter: default_current_alert_filter(),
            storm_track_max_tracks: default_storm_track_max_tracks(),
            storm_track_min_dbz_tenths: default_storm_track_min_dbz_tenths(),
            product_hotkeys: default_product_hotkeys(),
            smooth_display: false,
            smooth_display_mode: String::new(),
            cross_section_smoothing: default_cross_section_smoothing(),
            loop_speed_percent: default_loop_speed_percent(),
            zoom_speed_percent: default_zoom_speed_percent(),
            radar_history_budget_gib: default_radar_history_budget_gib(),
            loop_low_sweeps: false,
            loop_low_sweep_filter: default_loop_low_sweep_filter(),
            loop_sweep_control: None,
            remember_product_tilts: true,
            show_derived_products: true,
            live_low_sweep_auto_advance: false,
            live_low_sweep_auto_advance_seconds: default_live_low_sweep_auto_advance_seconds(),
            show_center_crosshair: false,
            record_fps: default_record_fps(),
            loop_record_speed_percent: default_loop_record_speed_percent(),
            event_pad_frames: default_event_pad_frames(),
            event_before_scans: None,
            event_after_scans: None,
            event_max_frames: default_event_max_frames(),
            event_sync_warnings: true,
            event_track_auto_model: true,
            event_track_model_slug: default_model_slug(),
            event_track_camera_follow: true,
            archive_load_loop: true,
            archive_frame_count: default_archive_frame_count(),
            live_preload_frame_count: default_live_preload_frame_count(),
            poll_url: String::new(),
            custom_poll_links: Vec::new(),
            intl_provider: String::new(),
            intl_site: String::new(),
            farm_georefs: Vec::new(),
            workspace_layout: None,
            sounding_view_state: None,
            model_style_overrides: None,
            data_dir: String::new(),
            sidebar_section_open: BTreeMap::new(),
            model_slug: default_model_slug(),
            units: default_units(),
            time_zone: default_time_zone(),
        }
    }
}

impl AppSettings {
    /// Platform config file path: `%APPDATA%\bowecho\config.json` on
    /// Windows, `$XDG_CONFIG_HOME`/`~/.config/...` on Linux,
    /// `~/Library/Application Support/...` on macOS.
    pub fn config_path() -> Option<PathBuf> {
        Some(bowecho_config_dir()?.join("config.json"))
    }

    /// Load settings from `config_path()`, falling back to defaults on any
    /// missing-file / parse error.
    pub fn load() -> Self {
        Self::load_with_distribution_default(None, None)
    }

    /// Load settings while allowing the executable package to provide an
    /// opt-in default Brand Kit for first run. A readable config remains the
    /// source of truth; legacy JSON without `brand` still resolves to BowEcho.
    pub fn load_with_distribution_default(
        build_brand: Option<&str>,
        build_namespace: Option<&str>,
    ) -> Self {
        Self::config_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| Self::from_json(&text))
            .unwrap_or_else(|| Self {
                brand: BrandConfig::distribution_default_from(build_brand, build_namespace),
                ..Self::default()
            })
    }

    /// Persist to `config_path()`, creating the parent directory. Returns an
    /// error string on failure (callers may log and ignore).
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path().ok_or_else(|| "no config directory".to_owned())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, self.to_json()).map_err(|e| e.to_string())
    }

    pub fn from_json(text: &str) -> Self {
        let mut settings: Self = serde_json::from_str(text).unwrap_or_default();
        settings.brand = settings.brand.normalized_for_load();
        settings
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Effective event-loop context before the event window. Old configs only
    /// have `event_pad_frames`, so `None` deliberately inherits that value.
    pub fn effective_event_before_scans(&self) -> u16 {
        self.event_before_scans.unwrap_or(self.event_pad_frames)
    }

    /// Effective event-loop context after the event window. Old configs only
    /// have `event_pad_frames`, so `None` deliberately inherits that value.
    pub fn effective_event_after_scans(&self) -> u16 {
        self.event_after_scans.unwrap_or(self.event_pad_frames)
    }

    /// Favorite keys come in two shapes (v0.29 spec §1.1): bare US site
    /// ids (`"KTLX"`), which keep the historical uppercase-on-write /
    /// case-insensitive-compare behavior byte-for-byte, and namespaced
    /// `SiteRef` keys (`"intl:ord:deess"`, recognized by the `':'`),
    /// which are stored VERBATIM and compared case-sensitively — ORD/SMHI
    /// site ids are case-significant lowercase, so uppercasing would
    /// corrupt them on write.
    pub fn add_favorite(&mut self, site: &str) {
        if site.contains(':') {
            if !self.favorites.iter().any(|s| s == site) {
                self.favorites.push(site.to_owned());
            }
        } else if !self.favorites.iter().any(|s| s.eq_ignore_ascii_case(site)) {
            self.favorites.push(site.to_ascii_uppercase());
        }
    }

    pub fn remove_favorite(&mut self, site: &str) {
        if site.contains(':') {
            self.favorites.retain(|s| s != site);
        } else {
            self.favorites.retain(|s| !s.eq_ignore_ascii_case(site));
        }
    }

    pub fn is_favorite(&self, site: &str) -> bool {
        if site.contains(':') {
            self.favorites.iter().any(|s| s == site)
        } else {
            self.favorites.iter().any(|s| s.eq_ignore_ascii_case(site))
        }
    }
}

fn default_basemap_style() -> String {
    "dark".to_owned()
}

fn default_basemap_line_brightness_percent() -> u16 {
    100
}

fn default_basemap_line_thickness_percent() -> u16 {
    100
}

fn default_bold_labels() -> bool {
    true
}

fn default_radar_label_style() -> String {
    "id-box".to_owned()
}

fn default_current_alert_sort() -> String {
    "priority".to_owned()
}

fn default_current_alert_filter() -> String {
    "all".to_owned()
}

/// Platform bowecho config root (`%APPDATA%\bowecho` on Windows, the
/// XDG/Library equivalents elsewhere). NOT created here — callers that
/// write under it create what they need. Shared with the `styles` crate
/// so styles.json/color_tables/ sit beside config.json.
pub fn bowecho_config_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("bowecho"))
}

/// Runtime-selected storage namespace for caches/stores/user data. The
/// settings and styles documents deliberately stay in the legacy BowEcho root
/// so an opt-in namespace can always be found and reverted.
static STORAGE_NAMESPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_storage_namespace(namespace: Option<String>) {
    let namespace = namespace
        .as_deref()
        .and_then(sanitize_namespace)
        .unwrap_or_else(|| DEFAULT_STORAGE_NAMESPACE.to_owned());
    let _ = STORAGE_NAMESPACE.set(namespace);
}

pub fn active_storage_namespace() -> String {
    STORAGE_NAMESPACE
        .get()
        .cloned()
        .unwrap_or_else(|| DEFAULT_STORAGE_NAMESPACE.to_owned())
}

pub fn active_storage_root() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(active_storage_namespace()))
}

pub fn storage_root_for_namespace(namespace: &str) -> Option<PathBuf> {
    let namespace = sanitize_namespace(namespace)?;
    config_dir().map(|dir| dir.join(namespace))
}

/// Config-document path for an explicitly named storage namespace — the
/// settings home for second frontends (miniDerecho: B4,
/// docs/miniderecho-spec.md §1). Additive: BowEcho's own settings document
/// deliberately stays at `bowecho_config_dir()/config.json`
/// ([`AppSettings::config_path`]), and this helper never reads the
/// `BOWECHO_DATA_DIR` override.
pub fn config_path_for_namespace(namespace: &str) -> Option<PathBuf> {
    Some(storage_root_for_namespace(namespace)?.join("config.json"))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageImportSummary {
    pub files_copied: usize,
    pub directories_created: usize,
    pub files_skipped: usize,
}

/// Explicit, non-destructive namespace migration helper. Files are copied,
/// never moved; existing destination files and symlinks are skipped.
pub fn import_storage_tree(
    source: &Path,
    destination: &Path,
) -> Result<StorageImportSummary, String> {
    if !source.is_dir() {
        return Err(format!(
            "storage import source is not a directory: {}",
            source.display()
        ));
    }
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let source = source.canonicalize().map_err(|error| error.to_string())?;
    let destination = destination
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if destination == source || destination.starts_with(&source) {
        return Err("storage import destination cannot be inside the source tree".to_owned());
    }

    let mut summary = StorageImportSummary::default();
    import_storage_tree_inner(&source, &destination, &mut summary)?;
    Ok(summary)
}

fn import_storage_tree_inner(
    source: &Path,
    destination: &Path,
    summary: &mut StorageImportSummary,
) -> Result<(), String> {
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            if !target.exists() {
                std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
                summary.directories_created += 1;
            }
            import_storage_tree_inner(&entry.path(), &target, summary)?;
        } else if file_type.is_file() {
            if target.exists() {
                summary.files_skipped += 1;
            } else {
                std::fs::copy(entry.path(), &target).map_err(|error| error.to_string())?;
                summary.files_copied += 1;
            }
        }
    }
    Ok(())
}

/// Directory for the on-disk raster tile cache.
pub fn tile_cache_dir() -> Option<PathBuf> {
    if let Some(root) = data_dir_override() {
        return Some(root.join("tiles"));
    }
    active_storage_root().map(|dir| dir.join("tiles"))
}

/// User-chosen data root override (field report: "not so wealthy in
/// terms of localappdata storage"). Set ONCE at startup from
/// `AppSettings.data_dir` / `BOWECHO_DATA_DIR`; changes apply on
/// restart so live stores never move under their workers. config.json
/// and styles.json deliberately stay at the default config path — they
/// are where the override itself is read from.
static DATA_DIR_OVERRIDE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

pub fn set_data_dir_override(dir: Option<PathBuf>) {
    let _ = DATA_DIR_OVERRIDE.set(dir);
}

pub fn data_dir_override() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("BOWECHO_DATA_DIR")
        && !env.trim().is_empty()
    {
        return Some(PathBuf::from(env));
    }
    DATA_DIR_OVERRIDE.get().cloned().flatten()
}

/// User color tables ("My tables"): `%APPDATA%\bowecho\color_tables\*.pal`.
/// Imported .pal files are COPIED here so a palette choice survives the
/// original file moving. Created on use.
pub fn color_tables_dir() -> PathBuf {
    let dir = color_tables_dir_path();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Path for user color tables without creating it. Use this for diagnostics
/// so permission errors can name the exact folder that failed.
pub fn color_tables_dir_path() -> PathBuf {
    bowecho_dir_path("color_tables")
}

/// Ensure the user color-table folder exists, returning the underlying IO
/// error instead of hiding it behind a later save/import failure.
pub fn ensure_color_tables_dir() -> std::io::Result<PathBuf> {
    let dir = color_tables_dir_path();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Platform-correct bowecho data root (config dir scoped, or the user's
/// data-folder override). Created on use.
fn bowecho_dir(leaf: &str) -> PathBuf {
    let dir = bowecho_dir_path(leaf);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn bowecho_dir_path(leaf: &str) -> PathBuf {
    data_dir_override()
        .map(|root| root.join(leaf))
        .or_else(|| active_storage_root().map(|dir| dir.join(leaf)))
        .unwrap_or_else(|| PathBuf::from("bowecho-data").join(leaf))
}

/// Model (rw-store) root. Dev convenience: when the local rusty-weather
/// checkout's store exists (the dev machine), share it; everyone else gets
/// a per-user app-data store — NEVER a hardcoded path that resolves
/// read-only on other systems (v0.8.0 macOS "os error 30").
pub fn model_store_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let dev = PathBuf::from("C:/Users/drew/rusty-weather/store");
        if dev.is_dir() {
            return dev;
        }
    }
    bowecho_dir("model-store")
}

/// Raw GRIB download cache for in-app ingest.
pub fn model_cache_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let dev = PathBuf::from("C:/Users/drew/rusty-weather/out/rw_batch/cache");
        if dev.is_dir() {
            return dev;
        }
    }
    bowecho_dir("model-cache")
}

/// Crash log destination (config dir; None when no config dir resolves).
pub fn panic_log_path() -> Option<PathBuf> {
    bowecho_config_dir().map(|root| {
        let _ = std::fs::create_dir_all(&root);
        root.join("panic.log")
    })
}

/// GOES rolling store (always bowecho's own — no cross-process sharing).
pub fn sat_store_dir() -> PathBuf {
    bowecho_dir("sat-store")
}

/// BowEcho-owned GLM lightning store.
///
/// The live GLM follower is a writer. Use a per-process child store so two
/// BowEcho exes running during release testing cannot block each other on the
/// same satellite writer lock. Old per-process stores are tiny and are cleaned
/// opportunistically.
pub fn glm_store_dir() -> PathBuf {
    let root = bowecho_dir("glm-store");
    cleanup_old_glm_session_dirs(&root);
    let dir = root
        .join("sessions")
        .join(format!("pid-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn cleanup_old_glm_session_dirs(root: &Path) {
    const KEEP_FOR: Duration = Duration::from_secs(3 * 24 * 3600);
    let sessions = root.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with("pid-") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).is_ok_and(|age| age > KEEP_FOR) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// WoFS drape georeference cache: calibration OCRs ~20 sounding PNGs
/// (8–18 s); the result is per-run and stable, so it persists across
/// restarts.
pub fn wofs_georef_dir() -> PathBuf {
    bowecho_dir("wofs-georef")
}

/// Saved map-annotation sets: named, geo-anchored JSON documents (one file
/// per set) written by the annotate toolbar's Save/Load.
pub fn annotations_dir() -> PathBuf {
    bowecho_dir("annotations")
}

/// Where screenshots and loop recordings land: a user-visible media folder
/// (`~/Pictures/BowEcho`), NOT the config dir — these files exist to be
/// shared. `BOWECHO_SCREENSHOT_DIR` overrides. Created on demand by callers.
pub fn screenshots_dir() -> PathBuf {
    screenshots_dir_for_folder("BowEcho")
}

pub fn screenshots_dir_for_brand(brand: &BrandConfig) -> PathBuf {
    screenshots_dir_for_folder(&brand.output_folder_name())
}

fn screenshots_dir_for_folder(folder: &str) -> PathBuf {
    screenshots_dir_from(
        std::env::var("BOWECHO_SCREENSHOT_DIR").ok(),
        std::env::var("USERPROFILE").ok(),
        std::env::var("HOME").ok(),
        folder,
    )
}

fn screenshots_dir_from(
    override_dir: Option<String>,
    userprofile: Option<String>,
    home: Option<String>,
    folder: &str,
) -> PathBuf {
    if let Some(dir) = override_dir
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let base = userprofile
        .filter(|value| !value.trim().is_empty())
        .or(home.filter(|value| !value.trim().is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Pictures").join(folder)
}

fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_default_and_empty_settings_json_keep_bowecho_identity() {
        let direct = BrandConfig::default();
        let settings = AppSettings::from_json("{}");

        assert_eq!(settings.brand, direct);
        assert_eq!(settings.brand.resolved_display_name(), "BowEcho");
        assert_eq!(settings.brand.filename_prefix(), "bowecho");
        assert_eq!(settings.brand.output_folder_name(), "BowEcho");
        assert!(!settings.brand.use_custom_storage_namespace);
    }

    #[test]
    fn brand_default_stays_sparse_in_app_settings_json() {
        assert!(!AppSettings::default().to_json().contains("\"brand\""));
    }

    #[test]
    fn config_path_for_namespace_lands_inside_that_namespace_root() {
        let path = config_path_for_namespace("miniderecho").expect("config dir available in CI");
        assert_eq!(
            path,
            storage_root_for_namespace("miniderecho")
                .expect("namespace root")
                .join("config.json")
        );
        // Never BowEcho's settings document (B4: the settings-document trap).
        assert_ne!(AppSettings::config_path(), Some(path));
        // Rejected namespaces resolve to nothing rather than a stray file.
        assert_eq!(config_path_for_namespace("   "), None);
    }

    #[test]
    fn brand_storage_import_copies_without_overwriting() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "bowecho-brand-import-{}-{nonce}",
            std::process::id()
        ));
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::create_dir_all(source.join("nested")).expect("source tree");
        std::fs::create_dir_all(&destination).expect("destination tree");
        std::fs::write(source.join("keep.txt"), b"new").expect("source file");
        std::fs::write(source.join("nested/copied.txt"), b"copied").expect("nested file");
        std::fs::write(destination.join("keep.txt"), b"existing").expect("existing file");

        let summary = import_storage_tree(&source, &destination).expect("safe copy import");

        assert_eq!(summary.files_copied, 1);
        assert_eq!(summary.files_skipped, 1);
        assert_eq!(
            std::fs::read(destination.join("keep.txt")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(destination.join("nested/copied.txt")).unwrap(),
            b"copied"
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn screenshots_dir_prefers_override_then_pictures() {
        assert_eq!(
            screenshots_dir_from(
                Some("D:\\captures".to_owned()),
                Some("C:\\Users\\test".to_owned()),
                None,
                "BowEcho",
            ),
            PathBuf::from("D:\\captures")
        );
        assert_eq!(
            screenshots_dir_from(None, Some("C:\\Users\\test".to_owned()), None, "BowEcho"),
            PathBuf::from("C:\\Users\\test")
                .join("Pictures")
                .join("BowEcho")
        );
        assert_eq!(
            screenshots_dir_from(None, None, Some("/home/test".to_owned()), "BowEcho"),
            PathBuf::from("/home/test").join("Pictures").join("BowEcho")
        );
    }

    #[test]
    fn json_round_trips_all_fields() {
        let mut s = AppSettings {
            startup_site: Some("KEAX".to_owned()),
            polling_interval_seconds: 30,
            perf_hud: true,
            map_click_drops_coordinate_marker: true,
            alert_flash_enabled: false,
            alert_flash_families: vec!["tornado".to_owned()],
            alert_sound_enabled: true,
            alert_sound_path: "C:\\alerts\\tor.wav".to_owned(),
            alert_sound_families: vec!["tornado".to_owned(), "severe thunderstorm".to_owned()],
            archive_load_loop: false,
            archive_frame_count: 17,
            live_preload_frame_count: 6,
            loop_low_sweeps: true,
            loop_low_sweep_filter: "base".to_owned(),
            loop_sweep_control: Some(LoopSweepControl {
                primary: SweepPolicySet {
                    product_groups: BTreeMap::from([
                        (
                            SweepProductGroup::Reflectivity,
                            SweepPolicy::range_cdeg(110, 150),
                        ),
                        (SweepProductGroup::Velocity, SweepPolicy::range_cdeg(13, 83)),
                    ]),
                },
                extra_pane_overrides: BTreeMap::from([(
                    0,
                    SweepPolicySet {
                        product_groups: BTreeMap::from([(
                            SweepProductGroup::DualPol,
                            SweepPolicy {
                                mode: SweepPolicyMode::BaseOnly,
                                ..SweepPolicy::default()
                            },
                        )]),
                    },
                )]),
            }),
            show_derived_products: false,
            live_low_sweep_auto_advance: true,
            live_low_sweep_auto_advance_seconds: 30,
            show_center_crosshair: true,
            intl_provider: "smhi".to_owned(),
            intl_site: "angelholm".to_owned(),
            ..Default::default()
        };
        s.add_favorite("ktwx");
        s.add_favorite("KTWX"); // dedup, case-insensitive
        s.palette_by_family.insert(
            "Velocity / SRV".to_owned(),
            "Analyst Velocity HD".to_owned(),
        );
        s.palette_by_product
            .insert("SRV".to_owned(), "Balance VEL (CVD-safe)".to_owned());
        s.style_profile = "Chase dark".to_owned();
        s.custom_poll_links.push(CustomPollLinkEntry {
            label: "ARMOR".to_owned(),
            site_id: "ARMOR".to_owned(),
            lat_e6: 34_646_000,
            lon_e6: -86_772_000,
            poll_url: "http://192.0.2.10/armor".to_owned(),
        });
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(back.favorites, vec!["KTWX".to_owned()]);
    }

    #[test]
    fn custom_poll_links_default_and_round_trip() {
        assert!(AppSettings::from_json("{}").custom_poll_links.is_empty());

        let settings = AppSettings {
            custom_poll_links: vec![CustomPollLinkEntry {
                label: "FWLX".to_owned(),
                site_id: "FWLX".to_owned(),
                lat_e6: 30_123_456,
                lon_e6: -97_654_321,
                poll_url: "http://198.51.100.7:8080".to_owned(),
            }],
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.custom_poll_links, settings.custom_poll_links);
    }

    #[test]
    fn radar_labels_default_on_and_round_trip() {
        assert!(AppSettings::from_json("{}").show_radar_labels);
        assert_eq!(AppSettings::from_json("{}").radar_label_style, "id-box");

        let settings = AppSettings {
            show_radar_labels: false,
            radar_label_style: "id-box".to_owned(),
            show_hazard_labels: false,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(!back.show_radar_labels);
        assert_eq!(back.radar_label_style, "id-box");
        assert!(!back.show_hazard_labels);
        assert!(AppSettings::from_json("{}").show_hazard_labels);
    }

    #[test]
    fn map_click_coordinate_marker_defaults_off_and_round_trips() {
        assert!(!AppSettings::from_json("{}").map_click_drops_coordinate_marker);

        let settings = AppSettings {
            map_click_drops_coordinate_marker: true,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(back.map_click_drops_coordinate_marker);
    }

    #[test]
    fn lightweight_basemap_defaults_off_and_round_trips() {
        assert!(!AppSettings::from_json("{}").basemap_lightweight);

        let settings = AppSettings {
            basemap_lightweight: true,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(back.basemap_lightweight);
    }

    #[test]
    fn style_profile_defaults_and_round_trips() {
        assert_eq!(
            AppSettings::from_json("{}").style_profile,
            "BowEcho default"
        );

        let settings = AppSettings {
            style_profile: "Accessibility (CVD-safe)".to_owned(),
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.style_profile, "Accessibility (CVD-safe)");
    }

    #[test]
    fn archive_controls_default_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert!(old.archive_load_loop);
        assert_eq!(old.archive_frame_count, 10);
        assert_eq!(old.live_preload_frame_count, 5);
        assert!(!old.loop_low_sweeps);
        assert_eq!(old.loop_low_sweep_filter, "all");
        assert_eq!(old.loop_sweep_control, None);
        assert!(old.remember_product_tilts);
        assert!(old.show_derived_products);
        assert!(!old.live_low_sweep_auto_advance);
        assert_eq!(
            old.live_low_sweep_auto_advance_seconds,
            default_live_low_sweep_auto_advance_seconds()
        );
        assert!(!old.show_center_crosshair);

        let settings = AppSettings {
            archive_load_loop: false,
            archive_frame_count: 24,
            live_preload_frame_count: 4,
            loop_low_sweeps: true,
            loop_low_sweep_filter: "base".to_owned(),
            remember_product_tilts: false,
            show_derived_products: false,
            live_low_sweep_auto_advance: true,
            live_low_sweep_auto_advance_seconds: 10,
            show_center_crosshair: true,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(!back.archive_load_loop);
        assert_eq!(back.archive_frame_count, 24);
        assert_eq!(back.live_preload_frame_count, 4);
        assert!(back.loop_low_sweeps);
        assert_eq!(back.loop_low_sweep_filter, "base");
        assert_eq!(back.loop_sweep_control, None);
        assert!(!back.remember_product_tilts);
        assert!(!back.show_derived_products);
        assert!(back.live_low_sweep_auto_advance);
        assert_eq!(back.live_low_sweep_auto_advance_seconds, 10);
        assert!(back.show_center_crosshair);
    }

    #[test]
    fn legacy_low_sweep_config_defaults_to_current_behavior() {
        let settings = AppSettings::from_json(
            r#"{
                "loop_low_sweeps": true,
                "loop_low_sweep_filter": "same"
            }"#,
        );

        assert!(settings.loop_low_sweeps);
        assert_eq!(settings.loop_low_sweep_filter, "same");
        assert_eq!(settings.loop_sweep_control, None);
        assert!(
            !settings.to_json().contains("loop_sweep_control"),
            "legacy/default advanced state should stay sparse"
        );
    }

    #[test]
    fn advanced_low_sweep_control_round_trips_product_and_pane_ranges() {
        let settings = AppSettings {
            loop_low_sweeps: true,
            loop_sweep_control: Some(LoopSweepControl {
                primary: SweepPolicySet {
                    product_groups: BTreeMap::from([(
                        SweepProductGroup::Reflectivity,
                        SweepPolicy::range_cdeg(110, 150),
                    )]),
                },
                extra_pane_overrides: BTreeMap::from([(
                    0,
                    SweepPolicySet {
                        product_groups: BTreeMap::from([(
                            SweepProductGroup::Velocity,
                            SweepPolicy::range_cdeg(13, 83),
                        )]),
                    },
                )]),
            }),
            ..Default::default()
        };

        let json = settings.to_json();
        let back = AppSettings::from_json(&json);

        assert_eq!(back, settings);
        assert!(json.contains("loop_sweep_control"));
        assert!(json.contains("reflectivity"));
        assert!(json.contains("velocity"));
    }

    #[test]
    fn storm_track_filters_default_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert_eq!(old.storm_track_max_tracks, 16);
        assert_eq!(old.storm_track_min_dbz_tenths, 350);

        let settings = AppSettings {
            storm_track_max_tracks: 24,
            storm_track_min_dbz_tenths: 420,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.storm_track_max_tracks, 24);
        assert_eq!(back.storm_track_min_dbz_tenths, 420);
    }

    #[test]
    fn record_fps_defaults_to_30_and_round_trips() {
        let old = AppSettings::from_json("{}");
        assert_eq!(old.record_fps, 30);
        assert_eq!(old.loop_record_speed_percent, 100);

        let settings = AppSettings {
            record_fps: 60,
            loop_record_speed_percent: 1600,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.record_fps, 60);
        assert_eq!(back.loop_record_speed_percent, 1600);
    }

    #[test]
    fn independent_panels_default_and_round_trip() {
        assert!(!AppSettings::from_json("{}").independent_panels);

        let settings = AppSettings {
            independent_panels: true,
            grid_pane_count: 4,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(back.independent_panels);
        assert_eq!(back.grid_pane_count, 4);
    }

    /// v0.29 Phase 3: the display owner persists as ONE encoded settings
    /// key. Absent = legacy (None, nothing written); `:`-keys keep case.
    #[test]
    fn display_owner_site_defaults_absent_and_round_trips_with_case() {
        let old = AppSettings::from_json("{}");
        assert!(old.display_owner_site.is_none());
        assert!(
            !old.to_json().contains("display_owner_site"),
            "legacy files stay byte-small: the absent owner is not written"
        );

        let settings = AppSettings {
            display_owner_site: Some("intl:smhi:angelholm".to_owned()),
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());
        assert_eq!(
            back.display_owner_site.as_deref(),
            Some("intl:smhi:angelholm")
        );
    }

    /// v0.29 Phase 3: explicit pane pins persist as `SiteRef` settings
    /// keys. Absent key = legacy behavior (empty map, nothing written);
    /// `:`-keys keep their case through a round trip.
    #[test]
    fn extra_pane_pins_default_empty_and_round_trip_with_case_intact() {
        let old = AppSettings::from_json("{}");
        assert!(old.extra_pane_pins.is_empty());
        assert!(
            !old.to_json().contains("extra_pane_pins"),
            "legacy files stay byte-small: the empty map is not written"
        );

        let settings = AppSettings {
            extra_pane_pins: BTreeMap::from([
                (0, "KTLX".to_owned()),
                (2, "intl:smhi:angelholm".to_owned()),
            ]),
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());
        assert_eq!(back.extra_pane_pins, settings.extra_pane_pins);
        assert_eq!(
            back.extra_pane_pins.get(&2).map(String::as_str),
            Some("intl:smhi:angelholm"),
            "intl keys survive with case intact"
        );
    }

    #[test]
    fn event_archive_context_defaults_legacy_migration_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert_eq!(old.event_pad_frames, 5);
        assert_eq!(old.effective_event_before_scans(), 5);
        assert_eq!(old.effective_event_after_scans(), 5);
        assert_eq!(old.event_max_frames, 24);
        assert!(old.event_sync_warnings);

        let legacy = AppSettings::from_json(r#"{"event_pad_frames":12}"#);
        assert_eq!(legacy.effective_event_before_scans(), 12);
        assert_eq!(legacy.effective_event_after_scans(), 12);

        let settings = AppSettings {
            event_pad_frames: 12,
            event_before_scans: Some(3),
            event_after_scans: Some(7),
            event_max_frames: 36,
            event_sync_warnings: false,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.event_pad_frames, 12);
        assert_eq!(back.event_before_scans, Some(3));
        assert_eq!(back.event_after_scans, Some(7));
        assert_eq!(back.effective_event_before_scans(), 3);
        assert_eq!(back.effective_event_after_scans(), 7);
        assert_eq!(back.event_max_frames, 36);
        assert!(!back.event_sync_warnings);
    }

    #[test]
    fn event_track_model_settings_default_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert!(old.event_track_auto_model);
        assert_eq!(old.event_track_model_slug, "hrrr");
        assert!(old.event_track_camera_follow);

        let settings = AppSettings {
            event_track_auto_model: false,
            event_track_model_slug: "rap".to_owned(),
            event_track_camera_follow: false,
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert!(!back.event_track_auto_model);
        assert_eq!(back.event_track_model_slug, "rap");
        assert!(!back.event_track_camera_follow);
    }

    #[test]
    fn data_dir_default_and_round_trip() {
        assert_eq!(AppSettings::from_json("{}").data_dir, "");

        let settings = AppSettings {
            data_dir: "D:\\BowEchoData".to_owned(),
            ..Default::default()
        };
        let back = AppSettings::from_json(&settings.to_json());

        assert_eq!(back.data_dir, "D:\\BowEchoData");
    }

    #[test]
    fn workspace_layout_round_trips_as_opaque_json() {
        let s = AppSettings {
            workspace_layout: Some(serde_json::json!({
                "version": 1,
                "tree": {"root": 0},
                "viewers": {"Wofs": "docked"},
                "prefer_docked": ["Wofs"],
            })),
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back, s);
        // Absent → None (older configs).
        assert_eq!(AppSettings::from_json("{}").workspace_layout, None);
    }

    #[test]
    fn sounding_view_state_round_trips_as_opaque_json() {
        let s = AppSettings {
            sounding_view_state: Some(serde_json::json!({
                "zooms": {"scene": 1.4},
                "overlays": {"cape_cin_fill": true, "ml_parcel_style": "dotted"}
            })),
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(AppSettings::from_json("{}").sounding_view_state, None);
    }

    #[test]
    fn unknown_or_missing_fields_fall_back_to_default() {
        // "saved_layout_slots" is a removed legacy key: old configs that
        // still carry it must keep loading.
        let s = AppSettings::from_json(
            r#"{ "startup_site": "KDMX", "saved_layout_slots": 8, "bogus": 1 }"#,
        );
        assert_eq!(s.startup_site.as_deref(), Some("KDMX"));
        assert_eq!(s.polling_interval_seconds, 60);
    }

    #[test]
    fn smooth_display_mode_round_trips_and_defaults_empty() {
        // Older configs predate the mode string: it stays empty (the app
        // derives the mode from the legacy bool), and the bool is intact.
        let old = AppSettings::from_json(r#"{ "smooth_display": true }"#);
        assert!(old.smooth_display);
        assert_eq!(old.smooth_display_mode, "");
        let s = AppSettings {
            smooth_display: true,
            smooth_display_mode: "interpolated".to_owned(),
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back.smooth_display_mode, "interpolated");
        assert!(back.smooth_display);
    }

    #[test]
    fn overlay_raob_defaults_off_and_round_trips() {
        // Older configs have no overlay_raob field — the layer stays off.
        assert!(!AppSettings::from_json("{}").overlay_raob);
        let s = AppSettings {
            overlay_raob: true,
            ..Default::default()
        };
        assert!(AppSettings::from_json(&s.to_json()).overlay_raob);
    }

    #[test]
    fn model_slug_defaults_to_hrrr_and_round_trips() {
        // Older configs have no model_slug field — default to HRRR.
        assert_eq!(AppSettings::from_json("{}").model_slug, "hrrr");
        let s = AppSettings {
            model_slug: "gfs".to_owned(),
            ..Default::default()
        };
        assert_eq!(AppSettings::from_json(&s.to_json()).model_slug, "gfs");
    }

    #[test]
    fn units_default_to_imperial_and_round_trip() {
        // Older configs have no units field — default to imperial.
        assert_eq!(AppSettings::from_json("{}").units, "imperial");
        let s = AppSettings {
            units: "metric".to_owned(),
            ..Default::default()
        };
        assert_eq!(AppSettings::from_json(&s.to_json()).units, "metric");
    }

    #[test]
    fn time_zone_defaults_to_utc_and_round_trips() {
        // Older configs have no time_zone field — keep legacy UTC labels.
        assert_eq!(AppSettings::from_json("{}").time_zone, "utc");
        let s = AppSettings {
            time_zone: "eastern".to_owned(),
            ..Default::default()
        };
        assert_eq!(AppSettings::from_json(&s.to_json()).time_zone, "eastern");
    }

    #[test]
    fn alert_sound_settings_default_quiet_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert!(!old.alert_sound_enabled);
        assert!(old.alert_sound_path.is_empty());
        assert!(old.alert_sound_families.is_empty());

        let s = AppSettings {
            alert_sound_enabled: true,
            alert_sound_path: "C:\\alerts\\warn.wav".to_owned(),
            alert_sound_families: vec!["tornado".to_owned()],
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert!(back.alert_sound_enabled);
        assert_eq!(back.alert_sound_path, "C:\\alerts\\warn.wav");
        assert_eq!(back.alert_sound_families, vec!["tornado".to_owned()]);
    }

    #[test]
    fn alert_flash_settings_default_on_and_round_trip() {
        let old = AppSettings::from_json("{}");
        assert!(old.alert_flash_enabled);
        assert!(old.alert_flash_families.is_empty());

        let s = AppSettings {
            alert_flash_enabled: false,
            alert_flash_families: vec!["flash flood".to_owned()],
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert!(!back.alert_flash_enabled);
        assert_eq!(back.alert_flash_families, vec!["flash flood".to_owned()]);
    }

    #[test]
    fn current_alert_sort_defaults_to_priority_and_round_trips() {
        assert_eq!(AppSettings::from_json("{}").current_alert_sort, "priority");
        assert_eq!(AppSettings::from_json("{}").current_alert_filter, "all");

        let s = AppSettings {
            current_alert_sort: "newest".to_owned(),
            current_alert_filter: "tornado".to_owned(),
            ..Default::default()
        };
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back.current_alert_sort, "newest");
        assert_eq!(back.current_alert_filter, "tornado");
    }

    #[test]
    fn malformed_json_yields_default() {
        assert_eq!(
            AppSettings::from_json("not json {{"),
            AppSettings::default()
        );
    }

    /// v0.29 spec §1.1: namespaced `SiteRef` favorites (`':'`-keys) are
    /// stored VERBATIM and compared case-sensitively — the old blanket
    /// `to_ascii_uppercase()` would corrupt `"intl:ord:deess"` to
    /// `"INTL:ORD:DEESS"` on the very first intl star, and ORD/SMHI site
    /// ids are case-significant lowercase.
    #[test]
    fn namespaced_favorites_store_verbatim_and_compare_case_sensitively() {
        let mut s = AppSettings::default();
        s.add_favorite("intl:ord:deess");
        assert_eq!(s.favorites, vec!["intl:ord:deess".to_owned()]);
        assert!(s.is_favorite("intl:ord:deess"));
        assert!(
            !s.is_favorite("INTL:ORD:DEESS"),
            "namespaced keys never case-fold"
        );

        s.add_favorite("intl:ord:deess"); // exact duplicate: no-op
        assert_eq!(s.favorites.len(), 1);
        s.add_favorite("intl:jma:TAKA"); // uppercase intl id survives
        assert_eq!(
            s.favorites,
            vec!["intl:ord:deess".to_owned(), "intl:jma:TAKA".to_owned()]
        );

        s.remove_favorite("INTL:ORD:DEESS"); // wrong case: no-op
        assert!(s.is_favorite("intl:ord:deess"));
        s.remove_favorite("intl:ord:deess");
        assert!(!s.is_favorite("intl:ord:deess"));
        assert_eq!(s.favorites, vec!["intl:jma:TAKA".to_owned()]);
    }

    /// Bare keys keep the historical behavior byte-for-byte: uppercase on
    /// write, case-insensitive compare and removal.
    #[test]
    fn bare_favorites_keep_the_uppercasing_behavior_exactly() {
        let mut s = AppSettings::default();
        s.add_favorite("ktlx");
        assert_eq!(s.favorites, vec!["KTLX".to_owned()]);
        assert!(s.is_favorite("kTlX"));
        s.add_favorite("KTLX"); // dedupe, case-insensitive
        assert_eq!(s.favorites.len(), 1);
        s.remove_favorite("Ktlx");
        assert!(s.favorites.is_empty());
    }

    /// Both favorite shapes coexist in one list and survive the JSON
    /// round trip with case intact.
    #[test]
    fn mixed_favorites_round_trip_with_case_intact() {
        let mut s = AppSettings::default();
        s.add_favorite("ktlx");
        s.add_favorite("intl:smhi:angelholm");
        s.add_favorite("TOKC");
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(
            back.favorites,
            vec![
                "KTLX".to_owned(),
                "intl:smhi:angelholm".to_owned(),
                "TOKC".to_owned()
            ]
        );
        assert!(back.is_favorite("intl:smhi:angelholm"));
        assert!(back.is_favorite("ktlx"));
        // A bare query never matches a namespaced key and vice versa.
        assert!(!back.is_favorite("angelholm"));
        assert!(!back.is_favorite("intl:smhi:ANGELHOLM"));
    }
}
