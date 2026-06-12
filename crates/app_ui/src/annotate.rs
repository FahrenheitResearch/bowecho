//! Geo-anchored map annotations: GR2Analyst-style pointers (crosshair, box,
//! arrow, freehand) plus the full meteorological-graphics vocabulary —
//! fronts (cold/warm/stationary/occluded/dryline/outflow/trough/squall),
//! flow arrows, watch boxes with hatch fills, warning polygons, and
//! pressure/hazard icons.
//!
//! The meteorological glyph vocabulary — front pip geometry and spacing,
//! hatch fills, warning-polygon styling, and the icon designs — was
//! contributed by GBW Overlay; its author (grayskieswx on YouTube) shared
//! the tool's source for this reimplementation. The geometry is ported to
//! Rust in [`geometry`] and [`draw`]; no JS is copied.
//!
//! Anchors are stored as lon/lat so shapes stay glued to the weather while
//! the user pans/zooms and while the history loop animates. Annotations are
//! painted into the map canvas (after hazard overlays, before hover UI), so
//! screenshots and loop recordings include them automatically. Sets persist
//! as geo-anchored JSON via [`persist`], so a saved analysis reloads
//! correctly over any site, zoom, or basemap.

pub(crate) mod draw;
pub(crate) mod geometry;
pub(crate) mod persist;

use std::collections::BTreeMap;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::ViewerApp;

// Default annotation style. Grouped here so a future style registry
// (docs/customization-spec.md style work) can lift them wholesale.
pub(crate) const ANNOTATION_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 40, 40);
pub(crate) const ANNOTATION_STROKE_WIDTH: f32 = 2.5;
/// Subtle dark halo painted underneath so strokes stay visible on top of
/// any echo color.
pub(crate) const ANNOTATION_HALO_COLOR: egui::Color32 =
    egui::Color32::from_rgba_premultiplied(8, 10, 14, 150);
/// Halo strokes are this much wider than the stroke they back.
pub(crate) const ANNOTATION_HALO_EXTRA: f32 = 3.0;
pub(crate) const CROSSHAIR_RING_RADIUS_PX: f32 = 10.0;
pub(crate) const CROSSHAIR_TICK_INNER_PX: f32 = 4.0;
pub(crate) const CROSSHAIR_TICK_OUTER_PX: f32 = 17.0;
const FREEHAND_MIN_POINT_SPACING_PX: f32 = 4.0;
/// Dragging a path tool sketches vertices at this screen spacing
/// (freehand-smooth front drawing).
const PATH_DRAG_VERTEX_SPACING_PX: f32 = 12.0;
/// Consecutive path vertices closer than this collapse when finishing
/// (a double-click's second press lands on the first click's vertex).
const PATH_FINISH_DEDUPE_PX: f32 = 4.0;
/// In-progress shapes draw at GBW's preview alpha.
const DRAFT_ALPHA: f32 = 0.6;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct GeoPoint {
    pub(crate) lon: f32,
    pub(crate) lat: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrontKind {
    Cold,
    Warm,
    Stationary,
    Occluded,
    Dryline,
    Outflow,
    Trough,
    Squall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WatchKind {
    Tor,
    Svr,
    Wind,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WarnKind {
    Tor,
    Svr,
    Ffw,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IconKind {
    Low,
    High,
    Meso,
    Tornado,
    Hail,
}

/// Per-shape style, captured from the toolbar sliders when the shape is
/// started. `color: None` means the tool's default color.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ShapeStyle {
    pub(crate) thickness: f32,
    pub(crate) opacity: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) color: Option<[u8; 3]>,
}

impl Default for ShapeStyle {
    fn default() -> Self {
        Self {
            thickness: ANNOTATION_STROKE_WIDTH,
            opacity: 1.0,
            color: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Annotation {
    Crosshair {
        at: GeoPoint,
        #[serde(default)]
        style: ShapeStyle,
    },
    Box {
        a: GeoPoint,
        b: GeoPoint,
        #[serde(default)]
        style: ShapeStyle,
    },
    Arrow {
        tail: GeoPoint,
        head: GeoPoint,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// Range circle dragged center→edge — the classic radar distance
    /// tool. Geometry stays geographic (both anchors), so the circle and
    /// its mi/km radius label survive pan/zoom and re-projection.
    RangeCircle {
        center: GeoPoint,
        edge: GeoPoint,
        #[serde(default)]
        style: ShapeStyle,
    },
    Freehand {
        points: Vec<GeoPoint>,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// Free text stamped at a geo point (the toolbar's Text field at stamp
    /// time). The Width slider doubles as the type size.
    Text {
        at: GeoPoint,
        text: String,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// A front drawn along a vertex path, smoothed at render time; glyphs
    /// march along the smoothed curve at even arc-length spacing. `flip`
    /// mirrors the glyph side; `pips` adds the optional outflow-boundary
    /// scallops.
    Front {
        front: FrontKind,
        points: Vec<GeoPoint>,
        #[serde(default)]
        flip: bool,
        #[serde(default)]
        pips: bool,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// Curved multi-vertex arrow (GBW "Flow Arrow"): smoothed path with a
    /// single arrowhead at the terminal end.
    FlowArrow {
        points: Vec<GeoPoint>,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// Watch box: two-corner geo rectangle with kind preset + optional
    /// diagonal hatch fill.
    WatchBox {
        watch: WatchKind,
        a: GeoPoint,
        b: GeoPoint,
        #[serde(default)]
        hatch: bool,
        /// Custom chip text; None = the kind's preset wording (none for
        /// Free boxes).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        style: ShapeStyle,
    },
    /// Warning polygon: click-vertex outline with translucent fill.
    WarnPolygon {
        warn: WarnKind,
        points: Vec<GeoPoint>,
        /// Custom banner text; None = the kind's preset wording (none for
        /// Free polygons).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default)]
        style: ShapeStyle,
    },
    Icon {
        icon: IconKind,
        at: GeoPoint,
        #[serde(default)]
        style: ShapeStyle,
    },
}

impl Annotation {
    /// Vertices of a path-built shape (fronts, flow arrows, warning
    /// polygons), if this is one.
    fn path_points_mut(&mut self) -> Option<&mut Vec<GeoPoint>> {
        match self {
            Annotation::Front { points, .. }
            | Annotation::FlowArrow { points, .. }
            | Annotation::WarnPolygon { points, .. } => Some(points),
            _ => None,
        }
    }

    fn path_points(&self) -> Option<&Vec<GeoPoint>> {
        match self {
            Annotation::Front { points, .. }
            | Annotation::FlowArrow { points, .. }
            | Annotation::WarnPolygon { points, .. } => Some(points),
            _ => None,
        }
    }
}

// ── TOOLS AS DATA ────────────────────────────────────────────────────────
// The toolbar renders from this table so the UI overhaul can re-home it
// without touching tool logic.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Crosshair,
    Box,
    Arrow,
    RangeCircle,
    Freehand,
    Text,
    Front(FrontKind),
    FlowArrow,
    Watch(WatchKind),
    Warn(WarnKind),
    Icon(IconKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolGroup {
    Fronts,
    Lines,
    Boxes,
    Icons,
}

impl ToolGroup {
    pub(crate) const ALL: [ToolGroup; 4] = [
        ToolGroup::Fronts,
        ToolGroup::Lines,
        ToolGroup::Boxes,
        ToolGroup::Icons,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            ToolGroup::Fronts => "Fronts",
            ToolGroup::Lines => "Lines",
            ToolGroup::Boxes => "Boxes",
            ToolGroup::Icons => "Icons",
        }
    }
}

/// How the pointer builds a shape for a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interaction {
    /// Single click stamps the shape.
    Stamp,
    /// Drag from one anchor to the other.
    Drag,
    /// Drag a continuous sketch, committed on release.
    Sketch,
    /// Click vertices (or drag to sketch); Enter / double-click finishes.
    Path,
}

fn interaction(tool: ToolKind) -> Interaction {
    match tool {
        ToolKind::Crosshair | ToolKind::Text | ToolKind::Icon(_) => Interaction::Stamp,
        ToolKind::Box | ToolKind::Arrow | ToolKind::RangeCircle | ToolKind::Watch(_) => {
            Interaction::Drag
        }
        ToolKind::Freehand => Interaction::Sketch,
        ToolKind::Front(_) | ToolKind::FlowArrow | ToolKind::Warn(_) => Interaction::Path,
    }
}

pub(crate) struct ToolDescriptor {
    /// Stable id (also keys per-tool color overrides).
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) group: ToolGroup,
    pub(crate) tool: ToolKind,
}

pub(crate) const TOOLS: &[ToolDescriptor] = &[
    // Fronts (GBW vocabulary).
    ToolDescriptor {
        id: "front_cold",
        label: "Cold Front",
        hint: "Click vertices (or drag); Enter/double-click finishes. F flips the triangle side.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Cold),
    },
    ToolDescriptor {
        id: "front_warm",
        label: "Warm Front",
        hint: "Click vertices (or drag); Enter/double-click finishes. F flips the semicircle side.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Warm),
    },
    ToolDescriptor {
        id: "front_stationary",
        label: "Stationary Front",
        hint: "Alternating red semicircles / blue triangles on opposite sides. F swaps sides.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Stationary),
    },
    ToolDescriptor {
        id: "front_occluded",
        label: "Occluded Front",
        hint: "Purple, alternating triangle + semicircle on the same side. F flips the side.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Occluded),
    },
    ToolDescriptor {
        id: "front_dryline",
        label: "Dryline",
        hint: "Unfilled scallops along the moisture gradient. F flips the scallop side.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Dryline),
    },
    ToolDescriptor {
        id: "front_outflow",
        label: "Outflow Boundary",
        hint: "Dashed cyan; toggle scallop pips in this menu. F flips the pip side.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Outflow),
    },
    ToolDescriptor {
        id: "front_trough",
        label: "Trough",
        hint: "Dash-dot axis line.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Trough),
    },
    ToolDescriptor {
        id: "front_squall",
        label: "Squall Line",
        hint: "Solid line with paired tick marks.",
        group: ToolGroup::Fronts,
        tool: ToolKind::Front(FrontKind::Squall),
    },
    // Lines / pointers.
    ToolDescriptor {
        id: "mark",
        label: "Mark",
        hint: "Click to drop a crosshair",
        group: ToolGroup::Lines,
        tool: ToolKind::Crosshair,
    },
    ToolDescriptor {
        id: "arrow",
        label: "Arrow",
        hint: "Drag from tail to head",
        group: ToolGroup::Lines,
        tool: ToolKind::Arrow,
    },
    ToolDescriptor {
        id: "flow_arrow",
        label: "Flow Arrow",
        hint: "Curved arrow: click vertices (or drag); Enter/double-click finishes at the head.",
        group: ToolGroup::Lines,
        tool: ToolKind::FlowArrow,
    },
    ToolDescriptor {
        id: "range_circle",
        label: "Range Circle",
        hint: "Drag outward from the center; the label reads the radius in mi / km.",
        group: ToolGroup::Lines,
        tool: ToolKind::RangeCircle,
    },
    ToolDescriptor {
        id: "text",
        label: "Text",
        hint: "Click to stamp the toolbar's Text field at that spot (Width sets the type size)",
        group: ToolGroup::Lines,
        tool: ToolKind::Text,
    },
    ToolDescriptor {
        id: "freehand",
        label: "Draw",
        hint: "Drag to sketch freehand",
        group: ToolGroup::Lines,
        tool: ToolKind::Freehand,
    },
    // Boxes / polygons.
    ToolDescriptor {
        id: "box",
        label: "Box",
        hint: "Drag two corners to box an area",
        group: ToolGroup::Boxes,
        tool: ToolKind::Box,
    },
    ToolDescriptor {
        id: "watch_tor",
        label: "TOR Watch",
        hint: "Drag a watch box (red, optional hatch fill)",
        group: ToolGroup::Boxes,
        tool: ToolKind::Watch(WatchKind::Tor),
    },
    ToolDescriptor {
        id: "watch_svr",
        label: "SVR Watch",
        hint: "Drag a watch box (yellow, optional hatch fill)",
        group: ToolGroup::Boxes,
        tool: ToolKind::Watch(WatchKind::Svr),
    },
    ToolDescriptor {
        id: "watch_wind",
        label: "Wind Watch",
        hint: "Drag a watch box (blue, optional hatch fill)",
        group: ToolGroup::Boxes,
        tool: ToolKind::Watch(WatchKind::Wind),
    },
    ToolDescriptor {
        id: "watch_free",
        label: "Free Watch",
        hint: "Drag an unlabeled watch box (recolor it with the color swatch)",
        group: ToolGroup::Boxes,
        tool: ToolKind::Watch(WatchKind::Free),
    },
    ToolDescriptor {
        id: "warn_tor",
        label: "TOR Warning",
        hint: "Click polygon vertices; Enter/double-click closes the warning",
        group: ToolGroup::Boxes,
        tool: ToolKind::Warn(WarnKind::Tor),
    },
    ToolDescriptor {
        id: "warn_svr",
        label: "SVR Warning",
        hint: "Click polygon vertices; Enter/double-click closes the warning",
        group: ToolGroup::Boxes,
        tool: ToolKind::Warn(WarnKind::Svr),
    },
    ToolDescriptor {
        id: "warn_ffw",
        label: "FFW Warning",
        hint: "Click polygon vertices; Enter/double-click closes the warning",
        group: ToolGroup::Boxes,
        tool: ToolKind::Warn(WarnKind::Ffw),
    },
    ToolDescriptor {
        id: "warn_free",
        label: "Free Polygon",
        hint: "Click polygon vertices; Enter/double-click closes it",
        group: ToolGroup::Boxes,
        tool: ToolKind::Warn(WarnKind::Free),
    },
    // Icons.
    ToolDescriptor {
        id: "icon_low",
        label: "Low (L)",
        hint: "Stamp a circled red L",
        group: ToolGroup::Icons,
        tool: ToolKind::Icon(IconKind::Low),
    },
    ToolDescriptor {
        id: "icon_high",
        label: "High (H)",
        hint: "Stamp a circled blue H",
        group: ToolGroup::Icons,
        tool: ToolKind::Icon(IconKind::High),
    },
    ToolDescriptor {
        id: "icon_meso",
        label: "Meso",
        hint: "Stamp a mesocyclone circulation glyph",
        group: ToolGroup::Icons,
        tool: ToolKind::Icon(IconKind::Meso),
    },
    ToolDescriptor {
        id: "icon_tornado",
        label: "Tornado",
        hint: "Stamp a tornado funnel",
        group: ToolGroup::Icons,
        tool: ToolKind::Icon(IconKind::Tornado),
    },
    ToolDescriptor {
        id: "icon_hail",
        label: "Large Hail",
        hint: "Stamp a large-hail marker",
        group: ToolGroup::Icons,
        tool: ToolKind::Icon(IconKind::Hail),
    },
];

pub(crate) fn descriptor(tool: ToolKind) -> &'static ToolDescriptor {
    TOOLS
        .iter()
        .find(|d| d.tool == tool)
        .expect("every ToolKind has a descriptor")
}

/// Default stroke colors. Front colors follow the GBW renderer (which in
/// turn follows NWS surface-analysis conventions); the original pointer
/// tools keep BowEcho red.
pub(crate) fn tool_default_color(tool: ToolKind) -> egui::Color32 {
    use egui::Color32;
    match tool {
        ToolKind::Crosshair | ToolKind::Box | ToolKind::Arrow | ToolKind::Freehand => {
            ANNOTATION_COLOR
        }
        ToolKind::RangeCircle | ToolKind::Text => Color32::from_rgb(240, 240, 245),
        ToolKind::FlowArrow => Color32::from_rgb(255, 255, 0),
        ToolKind::Front(front) => match front {
            FrontKind::Cold | FrontKind::Stationary => Color32::from_rgb(0x33, 0x33, 0xcc),
            FrontKind::Warm => Color32::from_rgb(0xdd, 0x00, 0x00),
            FrontKind::Occluded => Color32::from_rgb(0x88, 0x00, 0xcc),
            FrontKind::Dryline => Color32::from_rgb(0xcc, 0x66, 0x00),
            FrontKind::Outflow => Color32::from_rgb(0x00, 0xee, 0xff),
            FrontKind::Trough => Color32::from_rgb(0xaa, 0x77, 0x00),
            FrontKind::Squall => Color32::from_rgb(0xee, 0x44, 0x00),
        },
        ToolKind::Watch(watch) => match watch {
            WatchKind::Tor => Color32::from_rgb(255, 0, 0),
            WatchKind::Svr => Color32::from_rgb(255, 170, 0),
            WatchKind::Wind => Color32::from_rgb(0, 136, 255),
            WatchKind::Free => Color32::from_rgb(255, 255, 255),
        },
        ToolKind::Warn(warn) => match warn {
            WarnKind::Tor => Color32::from_rgb(255, 0, 0),
            WarnKind::Svr => Color32::from_rgb(255, 170, 0),
            WarnKind::Ffw => Color32::from_rgb(0, 255, 136),
            WarnKind::Free => Color32::from_rgb(255, 255, 0),
        },
        ToolKind::Icon(icon) => match icon {
            IconKind::Low => Color32::from_rgb(0xee, 0x22, 0x22),
            IconKind::High => Color32::from_rgb(0x22, 0x55, 0xee),
            IconKind::Meso => Color32::from_rgb(0xff, 0x88, 0x00),
            IconKind::Tornado => Color32::from_rgb(0xbb, 0x00, 0xee),
            IconKind::Hail => Color32::from_rgb(0x00, 0xcc, 0xcc),
        },
    }
}

pub(crate) struct AnnotationState {
    pub(crate) shapes: Vec<Annotation>,
    /// `Some` while annotate mode is active.
    pub(crate) active_tool: Option<ToolKind>,
    /// Tool restored when annotate mode is toggled back on.
    pub(crate) last_tool: ToolKind,
    /// In-progress shape, drawn live and committed on finish.
    pub(crate) draft: Option<Annotation>,
    /// Pointer geo position for the path-tool rubber band.
    pub(crate) hover_geo: Option<GeoPoint>,
    /// Style applied to NEW shapes (stored per shape on commit).
    pub(crate) thickness: f32,
    pub(crate) opacity: f32,
    /// Mirrors front glyph sides for new fronts (F while drawing flips the
    /// draft live).
    pub(crate) flip: bool,
    /// Optional outflow-boundary scallop pips for new outflow fronts.
    pub(crate) outflow_pips: bool,
    /// Diagonal hatch fill for new watch boxes.
    pub(crate) watch_hatch: bool,
    /// Toolbar Text field: stamped by the Text tool and written into new
    /// watch boxes / warning polygons (empty = the kind's preset wording).
    pub(crate) label_text: String,
    /// Per-tool color overrides (descriptor id → RGB), applied to NEW
    /// shapes of that tool.
    pub(crate) color_overrides: BTreeMap<&'static str, [u8; 3]>,
    /// Save-set name field in the toolbar.
    pub(crate) save_name: String,
    /// Last save/load result shown in the toolbar.
    pub(crate) io_status: Option<String>,
}

impl Default for AnnotationState {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            active_tool: None,
            last_tool: ToolKind::Crosshair,
            draft: None,
            hover_geo: None,
            thickness: ANNOTATION_STROKE_WIDTH,
            opacity: 1.0,
            flip: false,
            outflow_pips: false,
            watch_hatch: true,
            label_text: String::new(),
            color_overrides: BTreeMap::new(),
            save_name: String::new(),
            io_status: None,
        }
    }
}

impl AnnotationState {
    pub(crate) fn current_style(&self, tool: ToolKind) -> ShapeStyle {
        ShapeStyle {
            thickness: self.thickness,
            opacity: self.opacity,
            color: self.color_overrides.get(descriptor(tool).id).copied(),
        }
    }

    /// Toolbar Text field, trimmed; None when empty (= preset wording).
    pub(crate) fn normalized_label(&self) -> Option<String> {
        let text = self.label_text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// Rewrite the text of the most recent text-bearing shape (Text stamp,
    /// watch box, or warning polygon) from the toolbar field — the v1 text
    /// editor while shapes have no selection mechanic. Returns whether a
    /// shape was found.
    pub(crate) fn apply_label_to_last(&mut self) -> bool {
        let label = self.normalized_label();
        for shape in self.shapes.iter_mut().rev() {
            match shape {
                Annotation::Text { text, .. } => {
                    *text = label.unwrap_or_else(|| "Text".to_owned());
                    return true;
                }
                Annotation::WatchBox { label: slot, .. }
                | Annotation::WarnPolygon { label: slot, .. } => {
                    *slot = label;
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Undo one step: drop the last path vertex while drafting, else the
    /// whole draft, else the last committed shape.
    pub(crate) fn undo(&mut self) {
        if let Some(draft) = &mut self.draft {
            if let Some(points) = draft.path_points_mut()
                && points.len() > 1
            {
                points.pop();
                return;
            }
            self.draft = None;
            return;
        }
        self.shapes.pop();
    }
}

impl ViewerApp {
    pub(crate) fn annotate_top_bar_ui(&mut self, ui: &mut egui::Ui) {
        let active = self.annotations.active_tool.is_some();
        if ui
            .add_sized(
                egui::vec2(72.0, crate::PANEL_BUTTON_HEIGHT),
                egui::Button::selectable(active, "Annotate"),
            )
            .on_hover_text(
                "Draw geo-anchored weather graphics on the map: fronts, \
                 arrows, watch/warning shapes, icons, freehand. They follow \
                 pan/zoom, animate with the loop, and appear in \
                 screenshots/recordings. Esc cancels/exits draw mode.",
            )
            .clicked()
        {
            self.annotations.active_tool = if active {
                None
            } else {
                Some(self.annotations.last_tool)
            };
            self.annotations.draft = None;
        }
    }

    /// The compact annotation tool row shown under the top bar while
    /// annotate mode is active. Built from [`TOOLS`] so the planned UI
    /// overhaul can re-home it wholesale.
    pub(crate) fn annotate_tool_row_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().slider_width = 64.0;
            let active = self.annotations.active_tool;

            for group in ToolGroup::ALL {
                let current = active.map(descriptor).filter(|d| d.group == group);
                let title = match current {
                    Some(d) => format!("{}: {} ▾", group.label(), d.label),
                    None => format!("{} ▾", group.label()),
                };
                ui.menu_button(title, |ui| {
                    ui.set_min_width(170.0);
                    for d in TOOLS.iter().filter(|d| d.group == group) {
                        let selected = active == Some(d.tool);
                        ui.horizontal(|ui| {
                            let color = self
                                .annotations
                                .color_overrides
                                .get(d.id)
                                .map(|[r, g, b]| egui::Color32::from_rgb(*r, *g, *b))
                                .unwrap_or_else(|| tool_default_color(d.tool));
                            let (dot, _) = ui
                                .allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, color);
                            if ui
                                .selectable_label(selected, d.label)
                                .on_hover_text(d.hint)
                                .clicked()
                            {
                                self.annotations.active_tool = Some(d.tool);
                                self.annotations.last_tool = d.tool;
                                self.annotations.draft = None;
                                ui.close();
                            }
                        });
                    }
                    match group {
                        ToolGroup::Fronts => {
                            ui.separator();
                            ui.checkbox(&mut self.annotations.outflow_pips, "Outflow pips")
                                .on_hover_text("Add semicircle scallops to new outflow boundaries");
                        }
                        ToolGroup::Boxes => {
                            ui.separator();
                            ui.checkbox(&mut self.annotations.watch_hatch, "Hatch fill")
                                .on_hover_text("Diagonal hatch fill for new watch boxes");
                        }
                        _ => {}
                    }
                });
            }

            ui.separator();

            if let Some(tool) = active {
                if matches!(tool, ToolKind::Front(_)) {
                    let flipped = self.annotations.flip;
                    if ui
                        .add_sized(
                            egui::vec2(44.0, crate::PANEL_BUTTON_HEIGHT),
                            egui::Button::selectable(flipped, "Flip"),
                        )
                        .on_hover_text("Mirror the glyph side (F while drawing)")
                        .clicked()
                    {
                        self.annotations.flip = !flipped;
                        if let Some(Annotation::Front { flip, .. }) = &mut self.annotations.draft {
                            *flip = self.annotations.flip;
                        }
                    }
                }

                ui.label("Width");
                ui.add(
                    egui::Slider::new(&mut self.annotations.thickness, 1.0..=10.0)
                        .fixed_decimals(1),
                )
                .on_hover_text("Line thickness for new shapes");
                ui.label("Opacity");
                ui.add(
                    egui::Slider::new(&mut self.annotations.opacity, 0.1..=1.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                )
                .on_hover_text("Opacity for new shapes");

                // Per-tool color override (icons keep their semantic colors).
                if !matches!(tool, ToolKind::Icon(_)) {
                    let id = descriptor(tool).id;
                    let default = tool_default_color(tool);
                    let mut color = self
                        .annotations
                        .color_overrides
                        .get(id)
                        .map(|[r, g, b]| egui::Color32::from_rgb(*r, *g, *b))
                        .unwrap_or(default);
                    if ui
                        .color_edit_button_srgba(&mut color)
                        .on_hover_text("Color override for new shapes of this tool")
                        .changed()
                    {
                        if color == default {
                            self.annotations.color_overrides.remove(id);
                        } else {
                            self.annotations
                                .color_overrides
                                .insert(id, [color.r(), color.g(), color.b()]);
                        }
                    }
                    if self.annotations.color_overrides.contains_key(id)
                        && ui
                            .small_button("↺")
                            .on_hover_text("Reset to the tool's default color")
                            .clicked()
                    {
                        self.annotations.color_overrides.remove(id);
                    }
                }

                // Text editor: stamped by the Text tool; watch boxes and
                // warning polygons take it as custom chip/banner wording.
                if matches!(
                    tool,
                    ToolKind::Text | ToolKind::Watch(_) | ToolKind::Warn(_)
                ) {
                    ui.label("Text");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.annotations.label_text)
                            .desired_width(130.0)
                            .hint_text(match tool {
                                ToolKind::Text => "click the map to stamp",
                                _ => "preset wording",
                            }),
                    )
                    .on_hover_text(
                        "Applied to NEW shapes of this tool. Empty keeps the preset \
                         wording (TOR WATCH, TORNADO WARNING, …; Free shapes stay \
                         unlabeled).",
                    );
                    let has_target = self.annotations.shapes.iter().any(|shape| {
                        matches!(
                            shape,
                            Annotation::Text { .. }
                                | Annotation::WatchBox { .. }
                                | Annotation::WarnPolygon { .. }
                        )
                    });
                    if ui
                        .add_enabled(has_target, egui::Button::new("→ last"))
                        .on_hover_text(
                            "Rewrite the text of the most recent text / watch / \
                             warning shape (empty restores the preset wording)",
                        )
                        .clicked()
                    {
                        self.annotations.apply_label_to_last();
                    }
                }
            }

            ui.separator();

            let has_anything =
                !self.annotations.shapes.is_empty() || self.annotations.draft.is_some();
            if ui
                .add_enabled(has_anything, egui::Button::new("Undo"))
                .on_hover_text("Remove the last vertex/shape (Ctrl+Z)")
                .clicked()
            {
                self.annotations.undo();
            }
            if ui
                .add_enabled(has_anything, egui::Button::new("Clear"))
                .on_hover_text("Remove all annotations")
                .clicked()
            {
                self.annotations.shapes.clear();
                self.annotations.draft = None;
            }

            ui.menu_button("Save ▾", |ui| {
                ui.set_min_width(220.0);
                ui.label("Save annotation set (geo-anchored JSON):");
                ui.text_edit_singleline(&mut self.annotations.save_name);
                let can_save = !self.annotations.shapes.is_empty();
                if ui
                    .add_enabled(can_save, egui::Button::new("Save set"))
                    .on_hover_text("Writes to the BowEcho config dir, annotations folder")
                    .clicked()
                {
                    let result = persist::save_named(
                        &settings::annotations_dir(),
                        &self.annotations.save_name,
                        &self.annotations.shapes,
                    );
                    self.annotations.io_status = Some(match result {
                        Ok(path) => format!("Saved {}", path.display()),
                        Err(error) => format!("Save failed: {error}"),
                    });
                    ui.close();
                }
                if !can_save {
                    ui.weak("Nothing to save yet");
                }
            });
            ui.menu_button("Load ▾", |ui| {
                ui.set_min_width(220.0);
                let dir = settings::annotations_dir();
                let names = persist::list_saved(&dir);
                if names.is_empty() {
                    ui.weak("No saved annotation sets");
                }
                let mut picked: Option<String> = None;
                for name in &names {
                    if ui.button(name).clicked() {
                        picked = Some(name.clone());
                        ui.close();
                    }
                }
                if let Some(name) = picked {
                    match persist::load_named(&dir, &name) {
                        Ok(shapes) => {
                            let count = shapes.len();
                            self.annotations.shapes = shapes;
                            self.annotations.draft = None;
                            self.annotations.save_name = name.clone();
                            self.annotations.io_status =
                                Some(format!("Loaded {name} ({count} shapes)"));
                        }
                        Err(error) => {
                            self.annotations.io_status = Some(format!("Load failed: {error}"));
                        }
                    }
                }
            });

            if let Some(status) = &self.annotations.io_status {
                ui.weak(status);
            }
        });
    }

    /// Routes map pointer input to annotation drawing while annotate mode is
    /// active. Returns true when the pointer is owned by annotations so the
    /// map canvas skips pan and click-to-select handling.
    pub(crate) fn handle_annotation_input(
        &mut self,
        rect: egui::Rect,
        response: &egui::Response,
        ui: &egui::Ui,
    ) -> bool {
        let typing = ui.ctx().text_edit_focused();
        if self.annotations.active_tool.is_some()
            && !typing
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            // Esc is staged: first cancels the in-progress shape, then
            // exits annotate mode.
            if self.annotations.draft.is_some() {
                self.annotations.draft = None;
            } else {
                self.annotations.active_tool = None;
            }
        }
        let Some(tool) = self.annotations.active_tool else {
            self.annotations.hover_geo = None;
            return false;
        };
        if response.hovered() {
            ui.ctx()
                .output_mut(|output| output.cursor_icon = egui::CursorIcon::Crosshair);
        }

        self.annotations.hover_geo = response.hover_pos().map(|position| {
            let (lon, lat) = self.screen_to_lon_lat(rect, position);
            GeoPoint { lon, lat }
        });

        let drafting_path =
            self.annotations.draft.is_some() && interaction(tool) == Interaction::Path;
        if !typing {
            if matches!(tool, ToolKind::Front(_))
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F))
            {
                self.annotations.flip = !self.annotations.flip;
                if let Some(Annotation::Front { flip, .. }) = &mut self.annotations.draft {
                    *flip = self.annotations.flip;
                }
            }
            if drafting_path
                && ui.input_mut(|input| {
                    input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)
                })
            {
                self.annotations.undo();
            }
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z)) {
                self.annotations.undo();
            }
        }
        let finish_requested = drafting_path
            && !typing
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));

        let pointer_geo = response.interact_pointer_pos().map(|position| {
            let (lon, lat) = self.screen_to_lon_lat(rect, position);
            GeoPoint { lon, lat }
        });

        match interaction(tool) {
            Interaction::Stamp => {
                if response.clicked()
                    && let Some(geo) = pointer_geo
                {
                    let style = self.annotations.current_style(tool);
                    self.annotations.shapes.push(match tool {
                        ToolKind::Icon(icon) => Annotation::Icon {
                            icon,
                            at: geo,
                            style,
                        },
                        ToolKind::Text => Annotation::Text {
                            at: geo,
                            // Empty field still stamps something visible.
                            text: self
                                .annotations
                                .normalized_label()
                                .unwrap_or_else(|| "Text".to_owned()),
                            style,
                        },
                        _ => Annotation::Crosshair { at: geo, style },
                    });
                }
            }
            Interaction::Drag | Interaction::Sketch => {
                if response.drag_started()
                    && let Some(geo) = pointer_geo
                {
                    self.annotations.draft = Some(self.start_annotation_draft(tool, geo));
                } else if response.dragged()
                    && let Some(geo) = pointer_geo
                {
                    let spacing_ok = self.sketch_spacing_ok(rect, geo);
                    if let Some(draft) = &mut self.annotations.draft {
                        update_draft(draft, geo, spacing_ok);
                    }
                }
                if response.drag_stopped()
                    && let Some(draft) = self.annotations.draft.take()
                    && draft_is_valid(&draft)
                {
                    self.annotations.shapes.push(draft);
                }
            }
            Interaction::Path => {
                if response.double_clicked() {
                    self.finish_path_draft(rect);
                } else if response.clicked() || response.drag_started() {
                    if let Some(geo) = pointer_geo {
                        self.push_path_vertex(tool, geo, rect, 0.0);
                    }
                } else if response.dragged()
                    && let Some(geo) = pointer_geo
                {
                    self.push_path_vertex(tool, geo, rect, PATH_DRAG_VERTEX_SPACING_PX);
                }
                if finish_requested {
                    self.finish_path_draft(rect);
                }
            }
        }

        true
    }

    fn start_annotation_draft(&self, tool: ToolKind, geo: GeoPoint) -> Annotation {
        start_draft(
            tool,
            geo,
            self.annotations.current_style(tool),
            DraftFlags {
                flip: self.annotations.flip,
                pips: self.annotations.outflow_pips,
                hatch: self.annotations.watch_hatch,
                label: self.annotations.normalized_label(),
            },
        )
    }

    fn sketch_spacing_ok(&self, rect: egui::Rect, geo: GeoPoint) -> bool {
        match &self.annotations.draft {
            Some(Annotation::Freehand { points, .. }) => points.last().is_none_or(|last| {
                let last_screen = self.lon_lat_to_screen(rect, last.lon, last.lat);
                let current_screen = self.lon_lat_to_screen(rect, geo.lon, geo.lat);
                last_screen.distance(current_screen) >= FREEHAND_MIN_POINT_SPACING_PX
            }),
            _ => true,
        }
    }

    /// Adds a vertex to the active path draft (starting it if needed).
    /// `min_spacing_px > 0` gates drag-sketched vertices to even screen
    /// spacing.
    fn push_path_vertex(
        &mut self,
        tool: ToolKind,
        geo: GeoPoint,
        rect: egui::Rect,
        min_spacing_px: f32,
    ) {
        if self.annotations.draft.is_none() {
            self.annotations.draft = Some(self.start_annotation_draft(tool, geo));
            return;
        }
        let spacing_ok = if min_spacing_px > 0.0 {
            self.annotations
                .draft
                .as_ref()
                .and_then(|draft| draft.path_points())
                .and_then(|points| points.last())
                .is_none_or(|last| {
                    let last_screen = self.lon_lat_to_screen(rect, last.lon, last.lat);
                    let current_screen = self.lon_lat_to_screen(rect, geo.lon, geo.lat);
                    last_screen.distance(current_screen) >= min_spacing_px
                })
        } else {
            true
        };
        if spacing_ok
            && let Some(points) = self
                .annotations
                .draft
                .as_mut()
                .and_then(|draft| draft.path_points_mut())
        {
            points.push(geo);
        }
    }

    /// Finishes the in-progress path shape (Enter / double-click): dedupes
    /// trailing vertices that landed on top of each other and commits when
    /// the shape is valid.
    fn finish_path_draft(&mut self, rect: egui::Rect) {
        let Some(mut draft) = self.annotations.draft.take() else {
            return;
        };
        if let Some(points) = draft.path_points_mut() {
            let screen: Vec<egui::Pos2> = points
                .iter()
                .map(|p| self.lon_lat_to_screen(rect, p.lon, p.lat))
                .collect();
            let mut keep = vec![true; points.len()];
            for i in 1..screen.len() {
                if screen[i].distance(screen[i - 1]) < PATH_FINISH_DEDUPE_PX {
                    keep[i] = false;
                }
            }
            let mut keep_it = keep.iter();
            points.retain(|_| *keep_it.next().unwrap_or(&true));
        }
        if draft_is_valid(&draft) {
            self.annotations.shapes.push(draft);
        }
    }

    pub(crate) fn draw_map_annotations(&self, painter: &egui::Painter, rect: egui::Rect) {
        if self.annotations.shapes.is_empty() && self.annotations.draft.is_none() {
            return;
        }
        let project = |point: GeoPoint| self.lon_lat_to_screen(rect, point.lon, point.lat);
        for annotation in &self.annotations.shapes {
            draw::annotation(painter, annotation, &project, 1.0);
        }
        if let Some(draft) = &self.annotations.draft {
            let preview = preview_shape(draft, self.annotations.hover_geo);
            draw::annotation(painter, &preview, &project, DRAFT_ALPHA);
            if let Some(points) = draft.path_points() {
                let screen: Vec<egui::Pos2> = points.iter().map(|p| project(*p)).collect();
                draw::draft_nodes(painter, &screen);
            }
        }
    }
}

/// Flags captured into new drafts from the toolbar state.
#[derive(Clone, Debug, Default)]
pub(crate) struct DraftFlags {
    pub(crate) flip: bool,
    pub(crate) pips: bool,
    pub(crate) hatch: bool,
    /// Custom text for watch boxes / warning polygons (None = preset).
    pub(crate) label: Option<String>,
}

pub(crate) fn start_draft(
    tool: ToolKind,
    geo: GeoPoint,
    style: ShapeStyle,
    flags: DraftFlags,
) -> Annotation {
    match tool {
        ToolKind::Crosshair => Annotation::Crosshair { at: geo, style },
        ToolKind::Box => Annotation::Box {
            a: geo,
            b: geo,
            style,
        },
        ToolKind::Arrow => Annotation::Arrow {
            tail: geo,
            head: geo,
            style,
        },
        ToolKind::RangeCircle => Annotation::RangeCircle {
            center: geo,
            edge: geo,
            style,
        },
        ToolKind::Freehand => Annotation::Freehand {
            points: vec![geo],
            style,
        },
        ToolKind::Text => Annotation::Text {
            at: geo,
            text: flags.label.unwrap_or_else(|| "Text".to_owned()),
            style,
        },
        ToolKind::Front(front) => Annotation::Front {
            front,
            points: vec![geo],
            flip: flags.flip,
            pips: front == FrontKind::Outflow && flags.pips,
            style,
        },
        ToolKind::FlowArrow => Annotation::FlowArrow {
            points: vec![geo],
            style,
        },
        ToolKind::Watch(watch) => Annotation::WatchBox {
            watch,
            a: geo,
            b: geo,
            hatch: flags.hatch,
            label: flags.label,
            style,
        },
        ToolKind::Warn(warn) => Annotation::WarnPolygon {
            warn,
            points: vec![geo],
            label: flags.label,
            style,
        },
        ToolKind::Icon(icon) => Annotation::Icon {
            icon,
            at: geo,
            style,
        },
    }
}

pub(crate) fn update_draft(draft: &mut Annotation, geo: GeoPoint, spacing_ok: bool) {
    match draft {
        Annotation::Crosshair { at, .. }
        | Annotation::Text { at, .. }
        | Annotation::Icon { at, .. } => *at = geo,
        Annotation::Box { b, .. } | Annotation::WatchBox { b, .. } => *b = geo,
        Annotation::Arrow { head, .. } => *head = geo,
        Annotation::RangeCircle { edge, .. } => *edge = geo,
        Annotation::Freehand { points, .. }
        | Annotation::Front { points, .. }
        | Annotation::FlowArrow { points, .. }
        | Annotation::WarnPolygon { points, .. } => {
            if spacing_ok {
                points.push(geo);
            }
        }
    }
}

pub(crate) fn draft_is_valid(draft: &Annotation) -> bool {
    match draft {
        Annotation::Crosshair { .. } | Annotation::Text { .. } | Annotation::Icon { .. } => true,
        Annotation::Box { a, b, .. } | Annotation::WatchBox { a, b, .. } => a != b,
        Annotation::Arrow { tail, head, .. } => tail != head,
        Annotation::RangeCircle { center, edge, .. } => center != edge,
        Annotation::Freehand { points, .. }
        | Annotation::Front { points, .. }
        | Annotation::FlowArrow { points, .. } => points.len() >= 2,
        Annotation::WarnPolygon { points, .. } => points.len() >= 3,
    }
}

/// The draft plus a rubber-band vertex at the pointer, for live preview of
/// path tools. Non-path drafts pass through unchanged (their own draft
/// updates already track the pointer).
fn preview_shape(draft: &Annotation, hover: Option<GeoPoint>) -> Annotation {
    let mut preview = draft.clone();
    if let (Some(points), Some(hover)) = (preview.path_points_mut(), hover)
        && points.last() != Some(&hover)
    {
        points.push(hover);
    }
    preview
}

/// Corners of the two-corner box in draw order: a lat/lon-aligned
/// quadrilateral. Under the map's AEQD projection the projected edges are
/// very slightly curved; drawing straight segments between the projected
/// corners is indistinguishable at storm scale and keeps the box anchored.
pub(crate) fn box_corners(a: GeoPoint, b: GeoPoint) -> [GeoPoint; 4] {
    [
        a,
        GeoPoint {
            lon: b.lon,
            lat: a.lat,
        },
        b,
        GeoPoint {
            lon: a.lon,
            lat: b.lat,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> ShapeStyle {
        ShapeStyle::default()
    }

    #[test]
    fn box_corners_form_axis_aligned_rectangle() {
        let a = GeoPoint {
            lon: -97.8,
            lat: 35.1,
        };
        let b = GeoPoint {
            lon: -97.2,
            lat: 35.6,
        };
        let corners = box_corners(a, b);
        assert_eq!(corners[0], a);
        assert_eq!(corners[2], b);
        // Adjacent corners share exactly one axis with their neighbors.
        assert_eq!(corners[1].lat, a.lat);
        assert_eq!(corners[1].lon, b.lon);
        assert_eq!(corners[3].lat, b.lat);
        assert_eq!(corners[3].lon, a.lon);
    }

    #[test]
    fn drag_draft_flow_builds_box_and_arrow() {
        let start = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let end = GeoPoint {
            lon: -97.0,
            lat: 35.0,
        };

        let mut draft = start_draft(ToolKind::Box, start, style(), DraftFlags::default());
        assert!(!draft_is_valid(&draft), "zero-size box must not commit");
        update_draft(&mut draft, end, true);
        assert_eq!(
            draft,
            Annotation::Box {
                a: start,
                b: end,
                style: style()
            }
        );
        assert!(draft_is_valid(&draft));

        let mut arrow = start_draft(ToolKind::Arrow, start, style(), DraftFlags::default());
        update_draft(&mut arrow, end, true);
        assert_eq!(
            arrow,
            Annotation::Arrow {
                tail: start,
                head: end,
                style: style()
            }
        );
        assert!(draft_is_valid(&arrow));
    }

    #[test]
    fn freehand_draft_respects_point_spacing_gate() {
        let start = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let near = GeoPoint {
            lon: -97.999,
            lat: 34.0,
        };
        let far = GeoPoint {
            lon: -97.5,
            lat: 34.2,
        };
        let mut draft = start_draft(ToolKind::Freehand, start, style(), DraftFlags::default());
        update_draft(&mut draft, near, false);
        assert!(!draft_is_valid(&draft), "single point sketch is invalid");
        update_draft(&mut draft, far, true);
        match &draft {
            Annotation::Freehand { points, .. } => assert_eq!(points.as_slice(), &[start, far]),
            other => panic!("unexpected draft: {other:?}"),
        }
        assert!(draft_is_valid(&draft));
    }

    #[test]
    fn front_draft_carries_flags_and_needs_two_points() {
        let p1 = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let p2 = GeoPoint {
            lon: -97.0,
            lat: 34.5,
        };
        let flags = DraftFlags {
            flip: true,
            pips: true,
            hatch: false,
            label: None,
        };
        let mut draft = start_draft(
            ToolKind::Front(FrontKind::Outflow),
            p1,
            style(),
            flags.clone(),
        );
        assert!(!draft_is_valid(&draft));
        update_draft(&mut draft, p2, true);
        match &draft {
            Annotation::Front {
                front, flip, pips, ..
            } => {
                assert_eq!(*front, FrontKind::Outflow);
                assert!(*flip);
                assert!(*pips, "outflow keeps the pips flag");
            }
            other => panic!("unexpected draft: {other:?}"),
        }
        assert!(draft_is_valid(&draft));
        // Pips only apply to outflow boundaries.
        let cold = start_draft(ToolKind::Front(FrontKind::Cold), p1, style(), flags);
        match cold {
            Annotation::Front { pips, .. } => assert!(!pips),
            other => panic!("unexpected draft: {other:?}"),
        }
    }

    #[test]
    fn warn_polygon_needs_three_vertices() {
        let p = |lon: f32, lat: f32| GeoPoint { lon, lat };
        let mut draft = start_draft(
            ToolKind::Warn(WarnKind::Tor),
            p(-98.0, 34.0),
            style(),
            DraftFlags::default(),
        );
        update_draft(&mut draft, p(-97.5, 34.4), true);
        assert!(!draft_is_valid(&draft), "two vertices is not a polygon");
        update_draft(&mut draft, p(-97.2, 33.9), true);
        assert!(draft_is_valid(&draft));
    }

    #[test]
    fn undo_steps_back_through_vertices_then_shapes() {
        let p = |lon: f32, lat: f32| GeoPoint { lon, lat };
        let mut state = AnnotationState::default();
        state.shapes.push(Annotation::Crosshair {
            at: p(-98.0, 34.0),
            style: style(),
        });
        let mut draft = start_draft(
            ToolKind::Front(FrontKind::Cold),
            p(-98.0, 34.0),
            style(),
            DraftFlags::default(),
        );
        update_draft(&mut draft, p(-97.0, 34.5), true);
        state.draft = Some(draft);

        state.undo(); // drops the second vertex
        assert_eq!(
            state
                .draft
                .as_ref()
                .and_then(|d| d.path_points())
                .map(Vec::len),
            Some(1)
        );
        state.undo(); // drops the draft entirely
        assert!(state.draft.is_none());
        assert_eq!(state.shapes.len(), 1);
        state.undo(); // pops the committed shape
        assert!(state.shapes.is_empty());
    }

    #[test]
    fn preview_appends_hover_vertex_only_for_paths() {
        let p1 = GeoPoint {
            lon: -98.0,
            lat: 34.0,
        };
        let hover = GeoPoint {
            lon: -97.0,
            lat: 34.5,
        };
        let front = start_draft(
            ToolKind::Front(FrontKind::Warm),
            p1,
            style(),
            DraftFlags::default(),
        );
        let preview = preview_shape(&front, Some(hover));
        assert_eq!(preview.path_points().map(Vec::len), Some(2));
        // Drag shapes track the pointer through their own draft updates.
        let arrow = start_draft(ToolKind::Arrow, p1, style(), DraftFlags::default());
        assert_eq!(preview_shape(&arrow, Some(hover)), arrow);
    }

    #[test]
    fn every_tool_has_a_descriptor_and_distinct_id() {
        let mut ids: Vec<&str> = TOOLS.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), TOOLS.len(), "descriptor ids must be unique");
        for d in TOOLS {
            assert_eq!(descriptor(d.tool).id, d.id);
            // Each tool resolves a deterministic default color.
            let _ = tool_default_color(d.tool);
        }
    }
}
