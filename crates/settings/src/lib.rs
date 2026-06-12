//! Persisted application settings: a small JSON document under the platform
//! config directory. Loading is best-effort — a missing or unreadable file
//! yields defaults so the app always starts.

fn default_true() -> bool {
    true
}

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Site to load on startup (e.g. "KEAX"). None = built-in default.
    pub startup_site: Option<String>,
    /// Favorite site ids, in user order.
    pub favorites: Vec<String>,
    /// Live auto-refresh poll interval (seconds).
    pub polling_interval_seconds: u64,
    /// Number of saved pane-layout slots.
    pub saved_layout_slots: usize,
    /// Selected color table per family label (family.label() -> table name).
    /// Resolved on startup against built-ins ∪ user tables; a missing name
    /// (deleted .pal) falls back to the family default.
    pub palette_by_family: BTreeMap<String, String>,
    /// Per-product overrides (product label -> table name), beating the
    /// family binding for just that product (e.g. SRV vs VEL — both the
    /// Velocity family).
    #[serde(default)]
    pub palette_by_product: BTreeMap<String, String>,
    /// Multi-pane grid layout pane count from the last session (1, 2 or 4).
    pub grid_pane_count: usize,
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
    /// RAOB launch-site markers (the observed-soundings obs layer) —
    /// default off; clicking a marker fetches that station's sounding at
    /// the displayed radar time.
    #[serde(default)]
    pub overlay_raob: bool,
    /// Enabled SPC outlook kinds ("cat", "torn", "wind", "hail").
    #[serde(default)]
    pub overlay_spc_outlooks: Vec<String>,
    #[serde(default)]
    pub overlay_spc_reports: bool,
    /// Basemap style key: "dark" (vector), "satellite", "streets", "topo".
    #[serde(default = "default_basemap_style")]
    pub basemap_style: String,
    /// GR2-style bold town labels (white, heavy halo) readable over echoes.
    #[serde(default = "default_bold_labels")]
    pub bold_labels: bool,
    /// Map right-click: false (default) = open the lowest-beam radar menu;
    /// true = switch straight to the closest WSR-88D, no menu (field
    /// request: "i might sometimes want right click to just load closest
    /// radar").
    #[serde(default)]
    pub right_click_loads_nearest: bool,
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
    /// Product hotkeys: number-row key ("0"-"9") -> product label (e.g.
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
    /// Loop playback speed in percent of the 700 ms/frame baseline
    /// (100 = baseline, 200 = twice as fast). Drives history playback AND
    /// the GIF/MP4 recorder's frame timing, so exports match the screen.
    #[serde(default = "default_loop_speed_percent")]
    pub loop_speed_percent: u16,
    /// Extra archive scans loaded on each side of a clicked tornado
    /// track's window — context before touchdown and after lift (field
    /// request: a short track otherwise loads only a handful of frames).
    #[serde(default = "default_event_pad_frames")]
    pub event_pad_frames: u16,
    /// Last GR2A-style poll URL (mobile/research radar feeds) — typing it
    /// once per deployment is fine, once per session is not.
    #[serde(default)]
    pub poll_url: String,
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
}

fn default_units() -> String {
    "imperial".to_owned()
}

fn default_model_slug() -> String {
    "hrrr".to_owned()
}

fn default_loop_speed_percent() -> u16 {
    100
}

fn default_event_pad_frames() -> u16 {
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

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            overlay_obs: false,
            overlay_obs_metar: true,
            overlay_obs_mesonet: true,
            overlay_glm: false,
            overlay_raob: false,
            overlay_spc_outlooks: Vec::new(),
            overlay_spc_reports: false,
            startup_site: None,
            favorites: Vec::new(),
            polling_interval_seconds: 60,
            saved_layout_slots: 8,
            palette_by_family: BTreeMap::new(),
            palette_by_product: BTreeMap::new(),
            grid_pane_count: 1,
            placefiles: Vec::new(),
            basemap_style: default_basemap_style(),
            bold_labels: default_bold_labels(),
            right_click_loads_nearest: false,
            gate_filter_decidbz: None,
            model_keep_runs: default_model_keep_runs(),
            perf_hud: false,
            product_hotkeys: default_product_hotkeys(),
            smooth_display: false,
            smooth_display_mode: String::new(),
            loop_speed_percent: default_loop_speed_percent(),
            event_pad_frames: default_event_pad_frames(),
            poll_url: String::new(),
            intl_provider: String::new(),
            intl_site: String::new(),
            farm_georefs: Vec::new(),
            workspace_layout: None,
            data_dir: String::new(),
            sidebar_section_open: BTreeMap::new(),
            model_slug: default_model_slug(),
            units: default_units(),
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
        Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::from_json(&text))
            .unwrap_or_default()
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
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn add_favorite(&mut self, site: &str) {
        if !self.favorites.iter().any(|s| s.eq_ignore_ascii_case(site)) {
            self.favorites.push(site.to_ascii_uppercase());
        }
    }

    pub fn remove_favorite(&mut self, site: &str) {
        self.favorites.retain(|s| !s.eq_ignore_ascii_case(site));
    }

    pub fn is_favorite(&self, site: &str) -> bool {
        self.favorites.iter().any(|s| s.eq_ignore_ascii_case(site))
    }
}

fn default_basemap_style() -> String {
    "dark".to_owned()
}

fn default_bold_labels() -> bool {
    true
}

/// Platform bowecho config root (`%APPDATA%\bowecho` on Windows, the
/// XDG/Library equivalents elsewhere). NOT created here — callers that
/// write under it create what they need. Shared with the `styles` crate
/// so styles.json/color_tables/ sit beside config.json.
pub fn bowecho_config_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("bowecho"))
}

/// Directory for the on-disk raster tile cache.
pub fn tile_cache_dir() -> Option<PathBuf> {
    if let Some(root) = data_dir_override() {
        return Some(root.join("tiles"));
    }
    bowecho_config_dir().map(|dir| dir.join("tiles"))
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
    bowecho_dir("color_tables")
}

/// Platform-correct bowecho data root (config dir scoped, or the user's
/// data-folder override). Created on use.
fn bowecho_dir(leaf: &str) -> PathBuf {
    let dir = data_dir_override()
        .map(|root| root.join(leaf))
        .or_else(|| bowecho_config_dir().map(|dir| dir.join(leaf)))
        .unwrap_or_else(|| PathBuf::from("bowecho-data").join(leaf));
    let _ = std::fs::create_dir_all(&dir);
    dir
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

/// BowEcho-owned GLM lightning store (own dir per app — writer locks make
/// sharing safe, but separate stores avoid pruning-policy fights).
pub fn glm_store_dir() -> PathBuf {
    bowecho_dir("glm-store")
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
    screenshots_dir_from(
        std::env::var("BOWECHO_SCREENSHOT_DIR").ok(),
        std::env::var("USERPROFILE").ok(),
        std::env::var("HOME").ok(),
    )
}

fn screenshots_dir_from(
    override_dir: Option<String>,
    userprofile: Option<String>,
    home: Option<String>,
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
    base.join("Pictures").join("BowEcho")
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
    fn default_has_eight_layout_slots() {
        assert_eq!(AppSettings::default().saved_layout_slots, 8);
    }

    #[test]
    fn screenshots_dir_prefers_override_then_pictures() {
        assert_eq!(
            screenshots_dir_from(
                Some("D:\\captures".to_owned()),
                Some("C:\\Users\\test".to_owned()),
                None,
            ),
            PathBuf::from("D:\\captures")
        );
        assert_eq!(
            screenshots_dir_from(None, Some("C:\\Users\\test".to_owned()), None),
            PathBuf::from("C:\\Users\\test")
                .join("Pictures")
                .join("BowEcho")
        );
        assert_eq!(
            screenshots_dir_from(None, None, Some("/home/test".to_owned())),
            PathBuf::from("/home/test").join("Pictures").join("BowEcho")
        );
    }

    #[test]
    fn json_round_trips_all_fields() {
        let mut s = AppSettings {
            startup_site: Some("KEAX".to_owned()),
            polling_interval_seconds: 30,
            perf_hud: true,
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
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(back.favorites, vec!["KTWX".to_owned()]);
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
    fn unknown_or_missing_fields_fall_back_to_default() {
        let s = AppSettings::from_json(r#"{ "startup_site": "KDMX", "bogus": 1 }"#);
        assert_eq!(s.startup_site.as_deref(), Some("KDMX"));
        assert_eq!(s.polling_interval_seconds, 60);
        assert_eq!(s.saved_layout_slots, 8);
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
    fn malformed_json_yields_default() {
        assert_eq!(
            AppSettings::from_json("not json {{"),
            AppSettings::default()
        );
    }
}
