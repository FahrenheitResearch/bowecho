//! BowEcho style registry: every customizable map style (hazard polygon
//! colors/widths, SPC alphas, GLM ramp, obs plot colors, range rings,
//! radar-age thresholds, …) resolved from a sparse user-override document
//! persisted to `styles.json` beside `config.json`.
//!
//! Design (docs/customization-spec.md §1):
//! - **Sparse overrides, not full documents.** Every slot resolves as
//!   built-in default ← user override; only overrides serialize. Default
//!   improvements flow to everyone who hasn't touched that specific slot,
//!   and "reset" = delete the key.
//! - **`StyleSettings` is the document; `StyleRegistry` is the hot path.**
//!   The registry is a fully-resolved flat structure rebuilt on every edit
//!   (cheap), so draw code does plain field reads — no per-frame map
//!   lookups for fixed groups.
//! - Colors are `[r, g, b, a]` u8 — NOT egui types; this crate stays
//!   UI-toolkit-agnostic and `app_ui` converts.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const STYLES_SCHEMA: u32 = 1;

/// RGBA color, straight u8 channels (alpha 255 = opaque).
pub type Rgba = [u8; 4];

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

// ---------------------------------------------------------------------------
// Override document (what styles.json stores)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleSettings {
    /// Schema version; 0 in old files is treated as 1.
    pub schema: u32,
    /// Base map canvas style.
    #[serde(skip_serializing_if = "is_default")]
    pub map: MapStyleOverride,
    /// Hazard polygon overrides. Keys: family ids ("tornado",
    /// "severe-thunderstorm", "flash-flood", "flood", "special-marine",
    /// "snow-squall", "watch", "mesoscale-discussion", "local-storm-report",
    /// "special-weather", "text-polygon", "other") AND escalation subkeys
    /// ("tornado/catastrophic", "tornado/considerable",
    ///  "severe-thunderstorm/destructive", "flash-flood/catastrophic").
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hazards: BTreeMap<String, PolygonStyleOverride>,
    #[serde(skip_serializing_if = "is_default")]
    pub hazard_global: HazardGlobalOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub spc: SpcStyleOverride,
    /// Storm-report marker overrides. Keys: "tornado" | "wind" | "hail".
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub reports: BTreeMap<String, MarkerStyleOverride>,
    #[serde(skip_serializing_if = "is_default")]
    pub placefiles: PlacefileStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub obs: ObsStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub range_rings: RangeRingStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub labels: LabelStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub radar_age: RadarAgeStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub glm: GlmStyleOverride,
    #[serde(skip_serializing_if = "is_default")]
    pub drapes: DrapeStyleOverride,
}

impl Default for StyleSettings {
    fn default() -> Self {
        Self {
            schema: STYLES_SCHEMA,
            map: MapStyleOverride::default(),
            hazards: BTreeMap::new(),
            hazard_global: HazardGlobalOverride::default(),
            spc: SpcStyleOverride::default(),
            reports: BTreeMap::new(),
            placefiles: PlacefileStyleOverride::default(),
            obs: ObsStyleOverride::default(),
            range_rings: RangeRingStyleOverride::default(),
            labels: LabelStyleOverride::default(),
            radar_age: RadarAgeStyleOverride::default(),
            glm: GlmStyleOverride::default(),
            drapes: DrapeStyleOverride::default(),
        }
    }
}

impl StyleSettings {
    /// Best-effort parse; corrupt input yields all defaults so the app
    /// always starts (same contract as `AppSettings::from_json`).
    pub fn from_json(text: &str) -> Self {
        let mut settings: Self = serde_json::from_str(text).unwrap_or_default();
        settings.schema = STYLES_SCHEMA;
        settings
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MapStyleOverride {
    /// Opaque canvas fill behind vector lines and while raster tiles load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<Rgba>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolygonStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_width: Option<f32>,
    /// None = stroke color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<Rgba>,
    /// None = the global hazard fill alpha.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_alpha: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dash: Option<DashPattern>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DashPattern {
    #[default]
    Solid,
    Dashed {
        dash: f32,
        gap: f32,
    },
    Dotted,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HazardGlobalOverride {
    /// Default polygon fill alpha (the Warnings-panel slider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_alpha: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_width_scale: Option<f32>,
    /// Extra stroke width for the selected polygon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_width_boost: Option<f32>,
    /// Stroke alpha for unselected / selected polygons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_alpha: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_alpha_selected: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_font_px: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_font_selected_px: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpcStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outlook_fill_alpha: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outlook_stroke_alpha: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outlook_stroke_width: Option<f32>,
    /// True (default) = draw exactly the fill colors SPC publishes; false =
    /// `outlook_colors` (keyed by LABEL: TSTM/MRGL/SLGT/ENH/MDT/HIGH and
    /// probability labels) override the fills.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_spc_published_colors: Option<bool>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outlook_colors: BTreeMap<String, Rgba>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkerStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_px: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outline_width: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlacefileStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_width_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_size_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_show_text: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObsStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metar_dot: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesonet_dot: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dewpoint_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gust_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub station_id_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barb_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barb_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_font_px: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_font_px: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declutter_cell_px: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RingColorMode {
    /// Color the data-edge ring by scan age (the freshness ring).
    #[default]
    Age,
    /// Plain fixed-color ring.
    Fixed { color: Rgba },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RangeRingStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_mode: Option<RingColorMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_selected_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_idle_color: Option<Rgba>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LabelStyleOverride {
    /// Multiplier on the town-label size tiers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub town_font_scale: Option<f32>,
    /// Halo behind warning labels and map markers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_halo_color: Option<Rgba>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadarAgeStyleOverride {
    /// Scan-age thresholds in seconds, ascending (green → yellow → red).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub green_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yellow_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub red_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aging_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expired_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glyph_arc_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glyph_arc_radius_px: Option<f32>,
    /// LIVE/STALE chip threshold (separate question from the ring: "is the
    /// feed refreshing" vs "how old is this scan").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_chip_seconds: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlmStyleOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aged_color: Option<Rgba>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_scale: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DrapeStyleOverride {
    /// Initial opacities for NEW layers; live per-layer opacity stays on
    /// the layer rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radar_opacity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goes_opacity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_opacity: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_overlay_alpha: Option<u8>,
}

// ---------------------------------------------------------------------------
// Resolved styles (what draw code reads)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct MapStyle {
    pub background_color: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolygonStyle {
    pub stroke_color: Rgba,
    pub stroke_width: f32,
    pub fill_color: Rgba,
    /// None = use `HazardGlobalStyle::fill_alpha`.
    pub fill_alpha: Option<u8>,
    pub dash: DashPattern,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HazardGlobalStyle {
    pub fill_alpha: u8,
    pub stroke_width_scale: f32,
    pub selected_width_boost: f32,
    pub stroke_alpha: u8,
    pub stroke_alpha_selected: u8,
    pub label_font_px: f32,
    pub label_font_selected_px: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpcStyle {
    pub outlook_fill_alpha: u8,
    pub outlook_stroke_alpha: u8,
    pub outlook_stroke_width: f32,
    pub use_spc_published_colors: bool,
    pub outlook_colors: BTreeMap<String, Rgba>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MarkerStyle {
    pub color: Rgba,
    pub size_px: f32,
    pub outline_color: Rgba,
    pub outline_width: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlacefileStyle {
    pub line_width_scale: f32,
    pub icon_scale: f32,
    pub text_size_scale: f32,
    pub default_show_text: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObsStyle {
    pub metar_dot: Rgba,
    pub mesonet_dot: Rgba,
    pub temp_color: Rgba,
    pub dewpoint_color: Rgba,
    pub gust_color: Rgba,
    pub station_id_color: Rgba,
    pub barb_color: Rgba,
    pub barb_width: f32,
    pub value_font_px: f32,
    pub small_font_px: f32,
    pub declutter_cell_px: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeRingStyle {
    pub primary_width: f32,
    pub overlay_width: f32,
    pub color_mode: RingColorMode,
    pub site_selected_color: Rgba,
    pub site_idle_color: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LabelStyle {
    pub town_font_scale: f32,
    pub warning_halo_color: Rgba,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RadarAgeStyle {
    pub green_seconds: i64,
    pub yellow_seconds: i64,
    pub red_seconds: i64,
    pub fresh_color: Rgba,
    pub aging_color: Rgba,
    pub stale_color: Rgba,
    pub expired_color: Rgba,
    pub ring_enabled: bool,
    pub glyph_arc_enabled: bool,
    pub glyph_arc_radius_px: f32,
    pub stale_chip_seconds: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlmStyle {
    pub fresh_color: Rgba,
    pub aged_color: Rgba,
    pub size_scale: f32,
    pub window_minutes: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DrapeStyle {
    pub radar_opacity: f32,
    pub goes_opacity: f32,
    pub model_opacity: f32,
    pub min_overlay_alpha: u8,
}

// ---------------------------------------------------------------------------
// Built-in defaults (the audited hard-coded constants — landing the
// registry is pixel-identical; see docs/customization-spec.md §0/§1.3)
// ---------------------------------------------------------------------------

/// Hazard family ids in display order.
pub const HAZARD_FAMILIES: &[&str] = &[
    "tornado",
    "severe-thunderstorm",
    "flash-flood",
    "flood",
    "special-marine",
    "snow-squall",
    "watch",
    "mesoscale-discussion",
    "local-storm-report",
    "special-weather",
    "text-polygon",
    "other",
];

/// Damage-threat escalation subkeys with distinct built-in defaults.
pub const HAZARD_ESCALATIONS: &[&str] = &[
    "tornado/catastrophic",
    "tornado/considerable",
    "severe-thunderstorm/destructive",
    "flash-flood/catastrophic",
];

impl Default for MapStyle {
    fn default() -> Self {
        Self {
            background_color: [7, 10, 14, 255],
        }
    }
}

/// Built-in stroke color per hazard key (family or escalation subkey).
/// The damage-threat escalation follows the operational color language:
/// Tornado Emergency purple, PDS tornado magenta, destructive SVR deep
/// orange (the `hazard_color` table this replaces).
pub fn default_hazard_stroke_color(key: &str) -> Rgba {
    match key {
        "tornado" => [248, 62, 82, 255],
        "tornado/catastrophic" => [150, 50, 250, 255],
        "tornado/considerable" => [255, 64, 175, 255],
        "severe-thunderstorm" => [246, 183, 57, 255],
        "severe-thunderstorm/destructive" => [252, 122, 28, 255],
        "flash-flood" | "flash-flood/catastrophic" => [78, 218, 108, 255],
        "flood" => [76, 190, 124, 255],
        "special-marine" => [70, 190, 238, 255],
        "snow-squall" => [170, 210, 255, 255],
        "watch" => [235, 92, 245, 255],
        "mesoscale-discussion" => [95, 174, 255, 255],
        "local-storm-report" => [245, 245, 245, 255],
        "special-weather" => [245, 220, 72, 255],
        "text-polygon" => [190, 178, 255, 255],
        _ => [232, 232, 96, 255],
    }
}

pub const DEFAULT_HAZARD_STROKE_WIDTH: f32 = 1.5;
pub const DEFAULT_HAZARD_FILL_ALPHA: u8 = 24;

impl Default for HazardGlobalStyle {
    fn default() -> Self {
        Self {
            fill_alpha: DEFAULT_HAZARD_FILL_ALPHA,
            stroke_width_scale: 1.0,
            selected_width_boost: 0.9,
            stroke_alpha: 205,
            stroke_alpha_selected: 245,
            label_font_px: 11.0,
            label_font_selected_px: 12.0,
        }
    }
}

impl Default for SpcStyle {
    fn default() -> Self {
        Self {
            outlook_fill_alpha: 36,
            outlook_stroke_alpha: 230,
            outlook_stroke_width: 2.0,
            use_spc_published_colors: true,
            outlook_colors: BTreeMap::new(),
        }
    }
}

/// Built-in storm-report markers. Outline fields exist for all three kinds
/// (schema normalization); today only tornado draws one, so wind/hail
/// default to width 0 — pixel-identical.
pub fn default_report_marker(kind: &str) -> MarkerStyle {
    match kind {
        "tornado" => MarkerStyle {
            color: [235, 60, 60, 255],
            size_px: 5.0,
            outline_color: [0, 0, 0, 255],
            outline_width: 1.0,
        },
        "wind" => MarkerStyle {
            color: [90, 140, 245, 255],
            size_px: 3.5,
            outline_color: [0, 0, 0, 255],
            outline_width: 0.0,
        },
        _ => MarkerStyle {
            color: [80, 200, 100, 255],
            size_px: 3.5,
            outline_color: [0, 0, 0, 255],
            outline_width: 0.0,
        },
    }
}

impl Default for PlacefileStyle {
    fn default() -> Self {
        Self {
            line_width_scale: 1.0,
            icon_scale: 1.0,
            text_size_scale: 1.0,
            default_show_text: true,
        }
    }
}

impl Default for ObsStyle {
    fn default() -> Self {
        Self {
            metar_dot: [210, 214, 220, 255],
            mesonet_dot: [214, 176, 96, 255],
            temp_color: [255, 120, 110, 255],
            dewpoint_color: [120, 235, 130, 255],
            gust_color: [255, 196, 110, 255],
            station_id_color: [190, 196, 204, 180],
            barb_color: [205, 212, 222, 255],
            barb_width: 1.2,
            value_font_px: 11.0,
            small_font_px: 9.0,
            declutter_cell_px: 88.0,
        }
    }
}

impl Default for RangeRingStyle {
    fn default() -> Self {
        Self {
            primary_width: 1.8,
            overlay_width: 1.5,
            color_mode: RingColorMode::Age,
            site_selected_color: [88, 210, 245, 255],
            site_idle_color: [106, 132, 154, 255],
        }
    }
}

impl Default for LabelStyle {
    fn default() -> Self {
        Self {
            town_font_scale: 1.0,
            warning_halo_color: [0, 0, 0, 210],
        }
    }
}

impl Default for RadarAgeStyle {
    fn default() -> Self {
        Self {
            green_seconds: 6 * 60,
            yellow_seconds: 10 * 60,
            red_seconds: 15 * 60,
            fresh_color: [65, 238, 104, 255],
            aging_color: [238, 218, 62, 255],
            stale_color: [246, 76, 48, 255],
            expired_color: [205, 34, 48, 255],
            ring_enabled: true,
            glyph_arc_enabled: true,
            glyph_arc_radius_px: 9.0,
            stale_chip_seconds: 8 * 60,
        }
    }
}

impl Default for GlmStyle {
    fn default() -> Self {
        Self {
            fresh_color: [255, 235, 120, 235],
            aged_color: [255, 75, 10, 85],
            size_scale: 1.0,
            window_minutes: 10,
        }
    }
}

impl Default for DrapeStyle {
    fn default() -> Self {
        Self {
            radar_opacity: 1.0,
            goes_opacity: 0.85,
            model_opacity: 0.65,
            min_overlay_alpha: 48,
        }
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Fully-resolved styles + a signature over the override document for
/// shape-cache keys. Rebuild on every edit (`from_settings`); hold one in
/// the app and hand `&` into draw code.
pub struct StyleRegistry {
    map: MapStyle,
    hazards: BTreeMap<String, PolygonStyle>,
    hazard_fallback: PolygonStyle,
    hazard_global: HazardGlobalStyle,
    spc: SpcStyle,
    report_tornado: MarkerStyle,
    report_wind: MarkerStyle,
    report_hail: MarkerStyle,
    placefiles: PlacefileStyle,
    obs: ObsStyle,
    range_rings: RangeRingStyle,
    labels: LabelStyle,
    radar_age: RadarAgeStyle,
    glm: GlmStyle,
    drapes: DrapeStyle,
    signature: u64,
}

impl Default for StyleRegistry {
    fn default() -> Self {
        Self::from_settings(&StyleSettings::default())
    }
}

impl StyleRegistry {
    pub fn from_settings(settings: &StyleSettings) -> Self {
        let map_override = &settings.map;
        let map_default = MapStyle::default();
        let map = MapStyle {
            background_color: map_override
                .background_color
                .unwrap_or(map_default.background_color),
        };

        let mut hazards = BTreeMap::new();
        for family in HAZARD_FAMILIES {
            hazards.insert((*family).to_owned(), resolve_polygon(settings, family));
        }
        for key in HAZARD_ESCALATIONS {
            hazards.insert((*key).to_owned(), resolve_polygon(settings, key));
        }
        // User overrides on keys we don't know yet still resolve (forward
        // compatibility with future family ids).
        for key in settings.hazards.keys() {
            hazards
                .entry(key.clone())
                .or_insert_with(|| resolve_polygon(settings, key));
        }
        let hazard_fallback = resolve_polygon(settings, "other");

        let g = &settings.hazard_global;
        let gd = HazardGlobalStyle::default();
        let hazard_global = HazardGlobalStyle {
            fill_alpha: g.fill_alpha.unwrap_or(gd.fill_alpha),
            stroke_width_scale: g.stroke_width_scale.unwrap_or(gd.stroke_width_scale),
            selected_width_boost: g.selected_width_boost.unwrap_or(gd.selected_width_boost),
            stroke_alpha: g.stroke_alpha.unwrap_or(gd.stroke_alpha),
            stroke_alpha_selected: g.stroke_alpha_selected.unwrap_or(gd.stroke_alpha_selected),
            label_font_px: g.label_font_px.unwrap_or(gd.label_font_px),
            label_font_selected_px: g
                .label_font_selected_px
                .unwrap_or(gd.label_font_selected_px),
        };

        let s = &settings.spc;
        let sd = SpcStyle::default();
        let spc = SpcStyle {
            outlook_fill_alpha: s.outlook_fill_alpha.unwrap_or(sd.outlook_fill_alpha),
            outlook_stroke_alpha: s.outlook_stroke_alpha.unwrap_or(sd.outlook_stroke_alpha),
            outlook_stroke_width: s.outlook_stroke_width.unwrap_or(sd.outlook_stroke_width),
            use_spc_published_colors: s
                .use_spc_published_colors
                .unwrap_or(sd.use_spc_published_colors),
            outlook_colors: s.outlook_colors.clone(),
        };

        let resolve_marker = |kind: &str| -> MarkerStyle {
            let d = default_report_marker(kind);
            let Some(o) = settings.reports.get(kind) else {
                return d;
            };
            MarkerStyle {
                color: o.color.unwrap_or(d.color),
                size_px: o.size_px.unwrap_or(d.size_px),
                outline_color: o.outline_color.unwrap_or(d.outline_color),
                outline_width: o.outline_width.unwrap_or(d.outline_width),
            }
        };
        let report_tornado = resolve_marker("tornado");
        let report_wind = resolve_marker("wind");
        let report_hail = resolve_marker("hail");

        let p = &settings.placefiles;
        let pd = PlacefileStyle::default();
        let placefiles = PlacefileStyle {
            line_width_scale: p.line_width_scale.unwrap_or(pd.line_width_scale),
            icon_scale: p.icon_scale.unwrap_or(pd.icon_scale),
            text_size_scale: p.text_size_scale.unwrap_or(pd.text_size_scale),
            default_show_text: p.default_show_text.unwrap_or(pd.default_show_text),
        };

        let o = &settings.obs;
        let od = ObsStyle::default();
        let obs = ObsStyle {
            metar_dot: o.metar_dot.unwrap_or(od.metar_dot),
            mesonet_dot: o.mesonet_dot.unwrap_or(od.mesonet_dot),
            temp_color: o.temp_color.unwrap_or(od.temp_color),
            dewpoint_color: o.dewpoint_color.unwrap_or(od.dewpoint_color),
            gust_color: o.gust_color.unwrap_or(od.gust_color),
            station_id_color: o.station_id_color.unwrap_or(od.station_id_color),
            barb_color: o.barb_color.unwrap_or(od.barb_color),
            barb_width: o.barb_width.unwrap_or(od.barb_width),
            value_font_px: o.value_font_px.unwrap_or(od.value_font_px),
            small_font_px: o.small_font_px.unwrap_or(od.small_font_px),
            declutter_cell_px: o.declutter_cell_px.unwrap_or(od.declutter_cell_px),
        };

        let r = &settings.range_rings;
        let rd = RangeRingStyle::default();
        let range_rings = RangeRingStyle {
            primary_width: r.primary_width.unwrap_or(rd.primary_width),
            overlay_width: r.overlay_width.unwrap_or(rd.overlay_width),
            color_mode: r.color_mode.unwrap_or(rd.color_mode),
            site_selected_color: r.site_selected_color.unwrap_or(rd.site_selected_color),
            site_idle_color: r.site_idle_color.unwrap_or(rd.site_idle_color),
        };

        let l = &settings.labels;
        let ld = LabelStyle::default();
        let labels = LabelStyle {
            town_font_scale: l.town_font_scale.unwrap_or(ld.town_font_scale),
            warning_halo_color: l.warning_halo_color.unwrap_or(ld.warning_halo_color),
        };

        let a = &settings.radar_age;
        let ad = RadarAgeStyle::default();
        let radar_age = RadarAgeStyle {
            green_seconds: a.green_seconds.unwrap_or(ad.green_seconds),
            yellow_seconds: a.yellow_seconds.unwrap_or(ad.yellow_seconds),
            red_seconds: a.red_seconds.unwrap_or(ad.red_seconds),
            fresh_color: a.fresh_color.unwrap_or(ad.fresh_color),
            aging_color: a.aging_color.unwrap_or(ad.aging_color),
            stale_color: a.stale_color.unwrap_or(ad.stale_color),
            expired_color: a.expired_color.unwrap_or(ad.expired_color),
            ring_enabled: a.ring_enabled.unwrap_or(ad.ring_enabled),
            glyph_arc_enabled: a.glyph_arc_enabled.unwrap_or(ad.glyph_arc_enabled),
            glyph_arc_radius_px: a.glyph_arc_radius_px.unwrap_or(ad.glyph_arc_radius_px),
            stale_chip_seconds: a.stale_chip_seconds.unwrap_or(ad.stale_chip_seconds),
        };

        let m = &settings.glm;
        let md = GlmStyle::default();
        let glm = GlmStyle {
            fresh_color: m.fresh_color.unwrap_or(md.fresh_color),
            aged_color: m.aged_color.unwrap_or(md.aged_color),
            size_scale: m.size_scale.unwrap_or(md.size_scale),
            window_minutes: m.window_minutes.unwrap_or(md.window_minutes),
        };

        let d = &settings.drapes;
        let dd = DrapeStyle::default();
        let drapes = DrapeStyle {
            radar_opacity: d.radar_opacity.unwrap_or(dd.radar_opacity),
            goes_opacity: d.goes_opacity.unwrap_or(dd.goes_opacity),
            model_opacity: d.model_opacity.unwrap_or(dd.model_opacity),
            min_overlay_alpha: d.min_overlay_alpha.unwrap_or(dd.min_overlay_alpha),
        };

        Self {
            map,
            hazards,
            hazard_fallback,
            hazard_global,
            spc,
            report_tornado,
            report_wind,
            report_hail,
            placefiles,
            obs,
            range_rings,
            labels,
            radar_age,
            glm,
            drapes,
            signature: signature_of(settings),
        }
    }

    pub fn map(&self) -> &MapStyle {
        &self.map
    }

    /// Resolved polygon style for a hazard record. `family` accepts the
    /// app's space-separated ids ("severe thunderstorm") or the registry's
    /// hyphenated keys; `damage_threat` is the raw IBW tag (any case).
    /// Resolution: `family/threat` → `family` → "other".
    pub fn hazard_polygon(&self, family: &str, damage_threat: Option<&str>) -> &PolygonStyle {
        let family_key = normalize_key(family);
        if let Some(threat) = damage_threat {
            let escalation_key = format!("{family_key}/{}", normalize_key(threat));
            if let Some(style) = self.hazards.get(&escalation_key) {
                return style;
            }
        }
        self.hazards
            .get(&family_key)
            .unwrap_or(&self.hazard_fallback)
    }

    pub fn hazard_global(&self) -> &HazardGlobalStyle {
        &self.hazard_global
    }

    pub fn spc(&self) -> &SpcStyle {
        &self.spc
    }

    /// `kind`: "tornado" | "wind" | "hail" (anything else gets hail's).
    pub fn report_marker(&self, kind: &str) -> &MarkerStyle {
        match kind {
            "tornado" => &self.report_tornado,
            "wind" => &self.report_wind,
            _ => &self.report_hail,
        }
    }

    pub fn placefiles(&self) -> &PlacefileStyle {
        &self.placefiles
    }

    pub fn obs(&self) -> &ObsStyle {
        &self.obs
    }

    pub fn range_rings(&self) -> &RangeRingStyle {
        &self.range_rings
    }

    pub fn labels(&self) -> &LabelStyle {
        &self.labels
    }

    pub fn radar_age(&self) -> &RadarAgeStyle {
        &self.radar_age
    }

    pub fn glm(&self) -> &GlmStyle {
        &self.glm
    }

    pub fn drapes(&self) -> &DrapeStyle {
        &self.drapes
    }

    /// Hash over the override document — hash it into any shape cache that
    /// bakes styled geometry so edits repaint. Stable within a session.
    pub fn signature(&self) -> u64 {
        self.signature
    }
}

/// Resolve one hazard key. Each *property* resolves independently through
/// escalation override → family override → built-in default for the key
/// (an escalation stroke_color override still inherits the family's
/// fill_alpha override).
fn resolve_polygon(settings: &StyleSettings, key: &str) -> PolygonStyle {
    let family_key = key.split('/').next().unwrap_or(key);
    let escalation = (key != family_key)
        .then(|| settings.hazards.get(key))
        .flatten();
    let family = settings.hazards.get(family_key);
    let pick = |f: fn(&PolygonStyleOverride) -> Option<Rgba>| -> Option<Rgba> {
        escalation.and_then(f).or_else(|| family.and_then(f))
    };
    let stroke_color = pick(|o| o.stroke_color).unwrap_or(default_hazard_stroke_color(key));
    let stroke_width = escalation
        .and_then(|o| o.stroke_width)
        .or_else(|| family.and_then(|o| o.stroke_width))
        .unwrap_or(DEFAULT_HAZARD_STROKE_WIDTH);
    let fill_color = pick(|o| o.fill_color).unwrap_or(stroke_color);
    let fill_alpha = escalation
        .and_then(|o| o.fill_alpha)
        .or_else(|| family.and_then(|o| o.fill_alpha));
    let dash = escalation
        .and_then(|o| o.dash)
        .or_else(|| family.and_then(|o| o.dash))
        .unwrap_or(DashPattern::Solid);
    PolygonStyle {
        stroke_color,
        stroke_width,
        fill_color,
        fill_alpha,
        dash,
    }
}

/// Lowercase + spaces→hyphens, so the app's "severe thunderstorm" and the
/// document's "severe-thunderstorm" address the same slot.
fn normalize_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(' ', "-")
}

fn signature_of(settings: &StyleSettings) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(settings)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Persistence: %APPDATA%\bowecho\styles.json (+ .bak before migrations)
// ---------------------------------------------------------------------------

pub fn styles_path() -> Option<PathBuf> {
    settings::bowecho_config_dir().map(|dir| dir.join("styles.json"))
}

/// Result of loading the styles document.
pub struct LoadedStyles {
    pub settings: StyleSettings,
    /// File written by a newer BowEcho (schema > ours): we loaded what
    /// parses but saving is disabled to protect it.
    pub newer_schema: bool,
}

/// Best-effort load — missing/corrupt file ⇒ all defaults, app always
/// starts. Older schemas are migrated in place (with a `.bak` copy first).
pub fn load() -> LoadedStyles {
    match styles_path() {
        Some(path) => load_from_path(&path),
        None => LoadedStyles {
            settings: StyleSettings::default(),
            newer_schema: false,
        },
    }
}

pub fn load_from_path(path: &Path) -> LoadedStyles {
    load_with_migrations(path, STYLES_SCHEMA, MIGRATIONS)
}

/// Persist the document; refuses nothing (callers gate on `newer_schema`).
pub fn save(settings: &StyleSettings) -> Result<(), String> {
    let path = styles_path().ok_or_else(|| "no config directory".to_owned())?;
    save_to_path(settings, &path)
}

pub fn save_to_path(settings: &StyleSettings, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut on_disk = settings.clone();
    on_disk.schema = STYLES_SCHEMA;
    std::fs::write(path, on_disk.to_json()).map_err(|e| e.to_string())
}

/// Stepwise schema migrations: `(from_version, transform)` — the transform
/// rewrites a `from_version` document into `from_version + 1`. Operating
/// on `serde_json::Value` keeps renames explicit. New `Option` slots never
/// need one; migrations are only for renames/semantic changes.
type Migration = (u32, fn(&mut serde_json::Value));
const MIGRATIONS: &[Migration] = &[];

fn load_with_migrations(path: &Path, target_schema: u32, migrations: &[Migration]) -> LoadedStyles {
    let defaults = || LoadedStyles {
        settings: StyleSettings::default(),
        newer_schema: false,
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return defaults();
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return defaults();
    };
    // 0 in old files ⇒ 1 (the schema field predates itself).
    let schema = match value.get("schema").and_then(|v| v.as_u64()).unwrap_or(0) as u32 {
        0 => 1,
        v => v,
    };
    if schema > target_schema {
        // Do NOT rewrite the file: load what parses (serde tolerates
        // unknown fields), flag the session so saving is disabled.
        let mut settings: StyleSettings = serde_json::from_value(value).unwrap_or_default();
        settings.schema = STYLES_SCHEMA;
        return LoadedStyles {
            settings,
            newer_schema: true,
        };
    }
    if schema < target_schema {
        // Copy to styles.json.bak once, then migrate stepwise and rewrite.
        let _ = std::fs::write(path.with_extension("json.bak"), &text);
        let mut at = schema;
        while at < target_schema {
            if let Some((_, migrate)) = migrations.iter().find(|(from, _)| *from == at) {
                migrate(&mut value);
            }
            at += 1;
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("schema".to_owned(), serde_json::json!(target_schema));
        }
        let mut settings: StyleSettings = serde_json::from_value(value).unwrap_or_default();
        settings.schema = STYLES_SCHEMA;
        let _ = save_to_path(&settings, path);
        return LoadedStyles {
            settings,
            newer_schema: false,
        };
    }
    let mut settings: StyleSettings = serde_json::from_value(value).unwrap_or_default();
    settings.schema = STYLES_SCHEMA;
    LoadedStyles {
        settings,
        newer_schema: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_styles_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bowecho-styles-test-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("styles.json")
    }

    #[test]
    fn default_document_serializes_sparse() {
        let json = serde_json::to_string(&StyleSettings::default()).unwrap();
        assert_eq!(json, r#"{"schema":1}"#);
    }

    #[test]
    fn round_trips_only_overrides() {
        let mut settings = StyleSettings::default();
        settings.hazards.insert(
            "tornado".to_owned(),
            PolygonStyleOverride {
                stroke_width: Some(3.0),
                ..Default::default()
            },
        );
        settings.hazard_global.fill_alpha = Some(60);
        let json = settings.to_json();
        // Sparse: untouched groups and unset properties never serialize.
        assert!(!json.contains("stroke_color"));
        assert!(!json.contains("radar_age"));
        let back = StyleSettings::from_json(&json);
        assert_eq!(back, settings);
    }

    #[test]
    fn property_level_inheritance_through_escalation_chain() {
        let mut settings = StyleSettings::default();
        settings.hazards.insert(
            "tornado".to_owned(),
            PolygonStyleOverride {
                fill_alpha: Some(60),
                ..Default::default()
            },
        );
        settings.hazards.insert(
            "tornado/considerable".to_owned(),
            PolygonStyleOverride {
                stroke_color: Some([1, 2, 3, 255]),
                ..Default::default()
            },
        );
        let registry = StyleRegistry::from_settings(&settings);
        // Escalation override wins for its own property…
        let pds = registry.hazard_polygon("tornado", Some("CONSIDERABLE"));
        assert_eq!(pds.stroke_color, [1, 2, 3, 255]);
        // …while other properties inherit the family override…
        assert_eq!(pds.fill_alpha, Some(60));
        // …and untouched properties stay built-in.
        assert_eq!(pds.stroke_width, DEFAULT_HAZARD_STROKE_WIDTH);
        // Sibling escalation untouched by the override keeps its own default.
        let tor_e = registry.hazard_polygon("tornado", Some("CATASTROPHIC"));
        assert_eq!(tor_e.stroke_color, [150, 50, 250, 255]);
        assert_eq!(tor_e.fill_alpha, Some(60));
    }

    #[test]
    fn default_registry_pins_legacy_constants() {
        let registry = StyleRegistry::default();
        assert_eq!(registry.map().background_color, [7, 10, 14, 255]);
        // The operational escalation colors (Tornado Emergency purple, PDS
        // magenta, destructive SVR deep orange) and the base table.
        assert_eq!(
            registry
                .hazard_polygon("tornado", Some("CATASTROPHIC"))
                .stroke_color,
            [150, 50, 250, 255]
        );
        assert_eq!(
            registry
                .hazard_polygon("tornado", Some("CONSIDERABLE"))
                .stroke_color,
            [255, 64, 175, 255]
        );
        assert_eq!(
            registry
                .hazard_polygon("severe thunderstorm", Some("DESTRUCTIVE"))
                .stroke_color,
            [252, 122, 28, 255]
        );
        assert_eq!(
            registry.hazard_polygon("tornado", None).stroke_color,
            [248, 62, 82, 255]
        );
        assert_eq!(
            registry.hazard_polygon("watch", None).stroke_color,
            [235, 92, 245, 255]
        );
        // Unknown family falls back to the "other" yellow.
        assert_eq!(
            registry.hazard_polygon("volcano", None).stroke_color,
            [232, 232, 96, 255]
        );
        // Stroke geometry: 1.5 normal, +0.9 selected boost = 2.4; alphas
        // 205/245; global fill alpha 24.
        let global = registry.hazard_global();
        assert_eq!(registry.hazard_polygon("tornado", None).stroke_width, 1.5);
        assert_eq!(global.selected_width_boost, 0.9);
        assert_eq!(global.stroke_alpha, 205);
        assert_eq!(global.stroke_alpha_selected, 245);
        assert_eq!(global.fill_alpha, 24);
    }

    #[test]
    fn map_background_override_round_trips_and_resolves() {
        let mut settings = StyleSettings::default();
        settings.map.background_color = Some([12, 24, 36, 255]);

        let json = settings.to_json();
        assert!(json.contains("background_color"));

        let back = StyleSettings::from_json(&json);
        assert_eq!(back, settings);
        assert_eq!(
            StyleRegistry::from_settings(&back).map().background_color,
            [12, 24, 36, 255]
        );
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let settings = StyleSettings::from_json(
            r#"{"schema":1,"bogus_future_group":{"x":1},"hazard_global":{"fill_alpha":50,"bogus":2}}"#,
        );
        assert_eq!(settings.hazard_global.fill_alpha, Some(50));
    }

    #[test]
    fn malformed_json_yields_defaults() {
        assert_eq!(
            StyleSettings::from_json("not json {{"),
            StyleSettings::default()
        );
    }

    #[test]
    fn newer_schema_loads_without_rewriting_the_file() {
        let path = temp_styles_path("newer");
        let original = r#"{"schema":99,"hazard_global":{"fill_alpha":42},"from_the_future":true}"#;
        std::fs::write(&path, original).unwrap();
        let loaded = load_from_path(&path);
        assert!(loaded.newer_schema);
        assert_eq!(loaded.settings.hazard_global.fill_alpha, Some(42));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migration_writes_backup_then_rewrites() {
        fn one_to_two(value: &mut serde_json::Value) {
            // Synthetic rename: hazard_global.fill_alpha -> fill_alpha_v2
            // (exercises the machinery; real migrations land with schema 2).
            if let Some(global) = value
                .get_mut("hazard_global")
                .and_then(|v| v.as_object_mut())
                && let Some(alpha) = global.remove("fill_alpha")
            {
                global.insert("fill_alpha_v2".to_owned(), alpha);
            }
        }
        let path = temp_styles_path("migrate");
        let original = r#"{"schema":1,"hazard_global":{"fill_alpha":42}}"#;
        std::fs::write(&path, original).unwrap();
        let loaded = load_with_migrations(&path, 2, &[(1, one_to_two)]);
        assert!(!loaded.newer_schema);
        // The backup preserves the pre-migration bytes.
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.bak")).unwrap(),
            original
        );
        // The migration ran (the renamed key is unknown to schema-1 structs,
        // so the override reverts to default — exactly what the rename asked).
        assert_eq!(loaded.settings.hazard_global.fill_alpha, None);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("json.bak"));
    }

    #[test]
    fn schema_zero_treated_as_one_without_backup() {
        let path = temp_styles_path("zero");
        let _ = std::fs::remove_file(path.with_extension("json.bak"));
        std::fs::write(&path, r#"{"hazard_global":{"fill_alpha":31}}"#).unwrap();
        let loaded = load_from_path(&path);
        assert!(!loaded.newer_schema);
        assert_eq!(loaded.settings.hazard_global.fill_alpha, Some(31));
        assert!(!path.with_extension("json.bak").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn signature_tracks_document_changes() {
        let default_registry = StyleRegistry::default();
        let mut settings = StyleSettings::default();
        settings.hazard_global.fill_alpha = Some(60);
        let edited = StyleRegistry::from_settings(&settings);
        assert_ne!(default_registry.signature(), edited.signature());
    }
}
