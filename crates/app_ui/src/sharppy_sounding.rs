//! SHARPpy-look sounding panel: the [`sharppyrs`] SPC sounding window
//! (skew-T, hodograph + locator, insets, index board — the exact
//! SHARPpy-Reimagined render) as the default sounding view, wrapping the
//! classic [`rw_ui::SoundingPanel`] behind a per-panel toggle.
//!
//! The wrapper mirrors the exact `SoundingPanel` surface the app already
//! uses (`set_loading` / `set_error` / `set_data` / `set_native_column` /
//! `clear` / `has_content` / view-state JSON / `ui`), feeding both views
//! from the same column so switching is lossless. Analysis (parcels,
//! effective inflow layer, every index) runs once per install, not per
//! frame.

use eframe::egui;
use rustwx_sounding::SoundingColumn;
use rw_ui::SoundingData;

use crate::formula_sounding::FormulaSoundingDiagnostic;
use crate::sounding_correction::{
    CorrectionLevel, CorrectionRecipe, CorrectionResult, QcSeverity, apply_correction_recipe,
};
use crate::sounding_correction_io::{CorrectionSourceContext, ImportedRawSounding};
use crate::sounding_correction_ui::{BatchDiagnosticValues, SoundingCorrectionEditor};
use crate::sounding_table_config::{
    SoundingTableConfig, SoundingTableEditor, config_from_view_state, write_config_to_view_state,
};

#[path = "sounding_table_builtin.rs"]
mod sounding_table_builtin;

const MS_TO_KT: f64 = 1.943_844_49;
const SHARPPY_CANVAS_MIN_WIDTH: f32 = 1_630.0;
const SHARPPY_CANVAS_MIN_HEIGHT: f32 = 900.0;
const SHARPPY_TEXT_SCALE_MIN: f32 = 0.5;
const SHARPPY_TEXT_SCALE_MAX: f32 = 2.0;
const SHARPPY_TEXT_SCALE_DEFAULT: f32 = 1.0;
const LEGACY_DEFAULT_LAYOUT_WITH_STP: &str =
    "speed,advection|hodograph|slinky,thetae,srwinds,locationmap|indexboard,streamwiseness,stp|250";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SoundingFontChoice {
    #[default]
    SpaceGrotesk,
    CleanSans,
    TechnicalMono,
}

impl SoundingFontChoice {
    const ALL: [Self; 3] = [Self::SpaceGrotesk, Self::CleanSans, Self::TechnicalMono];

    fn key(self) -> &'static str {
        match self {
            Self::SpaceGrotesk => "space-grotesk",
            Self::CleanSans => "clean-sans",
            Self::TechnicalMono => "technical-mono",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "space-grotesk" | "space_grotesk" | "spacegrotesk" => Some(Self::SpaceGrotesk),
            "clean-sans" | "clean_sans" | "proportional" => Some(Self::CleanSans),
            "technical-mono" | "technical_mono" | "monospace" => Some(Self::TechnicalMono),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SpaceGrotesk => "Space Grotesk",
            Self::CleanSans => "Clean Sans",
            Self::TechnicalMono => "Technical Mono",
        }
    }

    fn preset(self) -> sharppyrs::SoundingFontPreset {
        match self {
            Self::SpaceGrotesk => sharppyrs::SoundingFontPreset::SpaceGrotesk,
            Self::CleanSans => sharppyrs::SoundingFontPreset::CleanProportional,
            Self::TechnicalMono => sharppyrs::SoundingFontPreset::TechnicalMonospace,
        }
    }
}

fn restored_layout_tokens(tokens: &str) -> Option<String> {
    if tokens.trim() == LEGACY_DEFAULT_LAYOUT_WITH_STP {
        // v0.34 persisted the then-default STP bar. Treat that exact layout as
        // a default, not as a customization, so existing users receive the
        // split movable index panels introduced with Rusty Weather v0.4 as
        // well. Check the raw legacy token before upstream canonicalization,
        // which now expands three bottom cells into the six-cell grid.
        return Some(sharppyrs::SoundingLayout::default().to_tokens());
    }
    Some(sharppyrs::SoundingLayout::from_tokens(tokens)?.to_tokens())
}

fn fit_sounding_canvas_size(viewport: egui::Vec2) -> egui::Vec2 {
    // The SHARPpy board is one coordinated graphic, so fit it uniformly
    // instead of squeezing individual columns. The result never exceeds its
    // current host: a floating window can therefore be resized on either axis
    // without reviving the old desktop-sized, clipped canvas.
    let width_scale = viewport.x.max(0.0) / SHARPPY_CANVAS_MIN_WIDTH;
    let height_scale = viewport.y.max(0.0) / SHARPPY_CANVAS_MIN_HEIGHT;
    let scale = width_scale.min(height_scale);
    egui::vec2(
        (SHARPPY_CANVAS_MIN_WIDTH * scale).min(viewport.x),
        (SHARPPY_CANVAS_MIN_HEIGHT * scale).min(viewport.y),
    )
}

fn sounding_canvas_size(viewport: egui::Vec2, stretch: bool) -> egui::Vec2 {
    let viewport = egui::vec2(viewport.x.max(0.0), viewport.y.max(0.0));
    if stretch {
        viewport
    } else {
        fit_sounding_canvas_size(viewport)
    }
}

struct SharppyAnalysis {
    prof: sharppyrs::Profile,
    derived: sharppyrs::DerivedParams,
    title: String,
    obs_adjusted_model: bool,
}

#[derive(Clone)]
struct SoundingSource {
    data: SoundingData,
    column: SoundingColumn,
    footprint: Option<sharppyrs::LocationFootprint>,
    manual_editable: bool,
}

/// Host-owned actions that sit beside the SHARPpy/Classic selector.  The
/// sounding widget owns the row and its visual grammar, while BowEcho keeps
/// ownership of map tools and model readiness.
#[derive(Clone, Debug)]
pub(crate) struct BoxSoundingHeaderControl {
    pub(crate) ready: bool,
    pub(crate) armed: bool,
    pub(crate) unavailable_reason: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SoundingHeaderControls {
    pub(crate) box_sounding: Option<BoxSoundingHeaderControl>,
    /// Whether surface-observation adjustment is currently effective. The
    /// app owns the setting because enabling it also enables the map's
    /// Surface obs layer; the sounding widget only renders the control.
    pub(crate) obs_adjusted: Option<bool>,
    /// Last completed store-backed Formula Lab field sampled for the model
    /// sounding. The panel independently verifies exact HourKey ownership and
    /// refuses it for RAOB/native sources before offering or rendering it.
    pub(crate) formula_diagnostic: Option<FormulaSoundingDiagnostic>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SoundingHeaderActions {
    pub(crate) toggle_box_sounding: bool,
    pub(crate) toggle_obs_adjusted: bool,
    /// Visible physical sounding board to copy/save on the next composited
    /// viewport screenshot. Kept as a host action because eframe owns capture.
    pub(crate) capture_sounding: Option<egui::Rect>,
}

pub struct SharppySoundingPanel {
    inner: rw_ui::SoundingPanel,
    analysis: Option<Box<SharppyAnalysis>>,
    classic: bool,
    /// SHARPpy boards fill both available host axes by default. Users can
    /// switch to aspect-fit from the inline controls in either a dock or a
    /// floating window; this choice is persisted alongside the panel layout.
    docked_stretch: bool,
    /// Sounding-only typography, independent from BowEcho's global egui zoom.
    font_choice: SoundingFontChoice,
    text_scale: f32,
    /// Last-seen SPC-window layout tokens ([`sharppyrs::SoundingLayout::to_tokens`]),
    /// mirrored out of egui memory during `ui()` so `view_state_json` (no ctx)
    /// can persist them.
    layout_tokens: Option<String>,
    /// Tokens applied from a saved view state, waiting for the next `ui()`
    /// (which has the ctx) to store them into egui memory.
    pending_layout_tokens: Option<String>,
    /// Untouched input retained for the lifetime of the displayed sounding.
    /// Manual correction always rebuilds from this copy, never from a prior
    /// edited result, so Reset is exact and the model store is never mutated.
    source: Option<SoundingSource>,
    correction_editor: SoundingCorrectionEditor,
    correction_recipe: CorrectionRecipe,
    correction_result: Option<CorrectionResult>,
    table_config: SoundingTableConfig,
    table_editor: SoundingTableEditor,
}

impl SharppySoundingPanel {
    pub fn new() -> Self {
        Self {
            inner: rw_ui::SoundingPanel::new(),
            analysis: None,
            classic: false,
            docked_stretch: true,
            font_choice: SoundingFontChoice::default(),
            text_scale: SHARPPY_TEXT_SCALE_DEFAULT,
            layout_tokens: None,
            pending_layout_tokens: None,
            source: None,
            correction_editor: SoundingCorrectionEditor::default(),
            correction_recipe: CorrectionRecipe::default(),
            correction_result: None,
            table_config: SoundingTableConfig::default(),
            table_editor: SoundingTableEditor::default(),
        }
    }

    /// Stable egui-memory key for the SPC-window panel layout, pinned so the
    /// layout survives the widget moving between panes and so it can be
    /// read/written outside the widget for persistence.
    fn layout_memory_id() -> egui::Id {
        egui::Id::new("bowecho_sharppy_layout")
    }

    pub fn set_loading(&mut self) {
        self.source = None;
        self.correction_recipe = CorrectionRecipe::default();
        self.correction_result = None;
        self.correction_editor.reset_source_state();
        self.inner.set_loading();
    }

    pub fn set_error(&mut self, message: String) {
        self.analysis = None;
        self.source = None;
        self.correction_recipe = CorrectionRecipe::default();
        self.correction_result = None;
        self.correction_editor.reset_source_state();
        self.inner.set_error(message);
    }

    pub fn clear(&mut self) {
        self.analysis = None;
        self.source = None;
        self.correction_recipe = CorrectionRecipe::default();
        self.correction_result = None;
        self.correction_editor.reset_source_state();
        self.correction_editor.close();
        self.inner.clear();
    }

    pub fn has_content(&self) -> bool {
        self.inner.has_content() || self.analysis.is_some()
    }

    /// True only for a model column whose surface was replaced by an
    /// observation. RAOBs also use this native panel, so the app uses this
    /// distinction when a header toggle should refresh the current model
    /// sounding without ever displacing an observed sounding.
    pub(crate) fn is_obs_adjusted_model(&self) -> bool {
        self.analysis
            .as_ref()
            .is_some_and(|analysis| analysis.obs_adjusted_model)
    }

    /// The classic panel's view-state object with SHARPpy host keys:
    /// `"sharppy_layout"` carries the SPC-window layout tokens and
    /// `"sharppy_docked_stretch"` carries the sizing preference (the legacy
    /// key is retained now that the same control also applies while floating);
    /// `"sharppy_font_preset"` / `"sharppy_text_scale"` carry independent
    /// sounding typography
    /// ([`sharppyrs::SoundingLayout::to_tokens`]). Keeping the inner object's
    /// shape (rather than nesting it) preserves every key existing consumers
    /// patch directly — e.g. `model_data::patch_sounding_scene_zoom` writing
    /// `["zooms"]["scene"]` — and keeps old saves loadable as-is.
    pub fn view_state_json(&self) -> serde_json::Value {
        let mut value = self.inner.view_state_json();
        if let (Some(obj), Some(tokens)) = (value.as_object_mut(), &self.layout_tokens) {
            obj.insert(
                "sharppy_layout".to_owned(),
                serde_json::Value::String(tokens.clone()),
            );
        }
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "sharppy_docked_stretch".to_owned(),
                serde_json::Value::Bool(self.docked_stretch),
            );
            obj.insert(
                "sharppy_font_preset".to_owned(),
                serde_json::Value::String(self.font_choice.key().to_owned()),
            );
            obj.insert(
                "sharppy_text_scale".to_owned(),
                serde_json::json!(self.text_scale),
            );
        }
        let _ = write_config_to_view_state(&mut value, &self.table_config);
        value
    }

    /// Accepts both shapes: a plain classic-panel state (old saves) and one
    /// carrying the added `"sharppy_layout"` key (the classic panel ignores
    /// unknown keys). Layout tokens take effect on the next `ui()` frame,
    /// which has the ctx to write egui memory.
    pub fn apply_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.table_config = config_from_view_state(value).unwrap_or_default();
        if let Some(tokens) = value.get("sharppy_layout").and_then(|v| v.as_str())
            && let Some(tokens) = restored_layout_tokens(tokens)
        {
            self.layout_tokens = Some(tokens.clone());
            self.pending_layout_tokens = Some(tokens);
        }
        if let Some(stretch) = value
            .get("sharppy_docked_stretch")
            .and_then(serde_json::Value::as_bool)
        {
            self.docked_stretch = stretch;
        }
        if let Some(choice) = value
            .get("sharppy_font_preset")
            .and_then(serde_json::Value::as_str)
            .and_then(SoundingFontChoice::from_key)
        {
            self.font_choice = choice;
        }
        if let Some(scale) = value
            .get("sharppy_text_scale")
            .and_then(serde_json::Value::as_f64)
            .map(|scale| scale as f32)
            .filter(|scale| scale.is_finite())
        {
            self.text_scale = scale.clamp(SHARPPY_TEXT_SCALE_MIN, SHARPPY_TEXT_SCALE_MAX);
        }
        self.inner.apply_view_state_json(value)
    }

    fn compatible_formula<'a>(
        &self,
        controls: &'a SoundingHeaderControls,
    ) -> Option<&'a FormulaSoundingDiagnostic> {
        let source = self.source.as_ref()?;
        let formula = controls.formula_diagnostic.as_ref()?;
        (source.manual_editable && source.data.hour == formula.source_hour).then_some(formula)
    }

    fn sharppy_style(&self) -> sharppyrs::SkewTStyle {
        sharppyrs::SkewTStyle::space_grotesk()
            .with_font_preset(self.font_choice.preset())
            .with_text_scale(self.text_scale)
    }

    #[allow(dead_code)] // parity with rw_ui::SoundingPanel's surface
    pub fn last_timings(&self) -> Option<(f32, f32)> {
        self.inner.last_timings()
    }

    pub fn set_data(&mut self, data: SoundingData) {
        self.set_data_with_footprint(data, None);
    }

    /// Install a real area-mean sounding with the sampled grid-cell extent
    /// that contributed to it. This is deliberately separate from point
    /// sounding installation so a later point click always clears the box.
    pub(crate) fn set_box_data(
        &mut self,
        data: SoundingData,
        footprint: Option<sharppyrs::LocationFootprint>,
    ) {
        self.set_data_with_footprint(data, footprint);
    }

    fn set_data_with_footprint(
        &mut self,
        data: SoundingData,
        footprint: Option<sharppyrs::LocationFootprint>,
    ) {
        match rw_ui::skewt::build_sounding_column(&data) {
            Ok(column) => self.install_source(data, column, footprint),
            Err(_) => {
                self.source = None;
                self.correction_recipe = CorrectionRecipe::default();
                self.correction_result = None;
                self.correction_editor.reset_source_state();
                self.analysis = None;
                self.inner.set_data(data);
            }
        }
    }

    pub fn set_native_column(&mut self, data: SoundingData, column: SoundingColumn) {
        self.install_source(data, column, None);
    }

    fn install_source(
        &mut self,
        data: SoundingData,
        column: SoundingColumn,
        footprint: Option<sharppyrs::LocationFootprint>,
    ) {
        // Observed RAOBs share this panel with model soundings, but manual
        // model-bias correction must never masquerade as an observation edit.
        let manual_editable = !data.hour.model.to_ascii_uppercase().contains("RAOB");
        self.source = Some(SoundingSource {
            data,
            column,
            footprint,
            manual_editable,
        });
        self.correction_recipe = CorrectionRecipe::default();
        self.correction_result = None;
        self.correction_editor.reset_source_state();
        if !manual_editable {
            self.correction_editor.close();
        }
        self.rebuild_from_source();
    }

    fn active_manual_correction_count(&self) -> usize {
        self.correction_recipe.active_level_count()
    }

    fn install_imported_raw(&mut self, imported: ImportedRawSounding) {
        let title = if imported.title.trim().is_empty() {
            "Imported profile".to_owned()
        } else {
            imported.title.trim().to_owned()
        };
        let data = SoundingData {
            hour: rw_ui::HourKey {
                model: "SHARPpy RAW".to_owned(),
                run: title,
                hour: 0,
                exact_time: None,
            },
            fx: 0.0,
            fy: 0.0,
            lat: imported
                .column
                .metadata
                .latitude_deg
                .map(|value| value as f32),
            lon: imported
                .column
                .metadata
                .longitude_deg
                .map(|value| value as f32),
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        };
        self.install_source(data, imported.column, None);
    }

    fn rebuild_from_source(&mut self) {
        let Some(source) = self.source.clone() else {
            return;
        };
        let active = source.manual_editable
            && (self.active_manual_correction_count() > 0
                || self.correction_recipe.convective_adjustment.enabled);
        let result = if source.manual_editable {
            Some(apply_correction_recipe(
                &source.column,
                &self.correction_recipe,
            ))
        } else {
            None
        };
        let candidate_column = result
            .as_ref()
            .filter(|_| active)
            .map_or_else(|| source.column.clone(), |result| result.column.clone());
        let candidate_analysis = build_analysis(&source.data, &candidate_column, source.footprint);
        // sharppyrs deliberately rejects structurally invalid or supersaturated
        // profiles. Keep the last honest source plot visible while the editor
        // reports the exact QC issue; never silently clip Td or show a blank
        // sounding as the user experiments.
        let preview_blocked = active && candidate_analysis.is_none();
        let (column, analysis) = if preview_blocked {
            let column = source.column.clone();
            let analysis = build_analysis(&source.data, &column, source.footprint);
            (column, analysis)
        } else {
            (candidate_column, candidate_analysis)
        };
        self.analysis = analysis;
        if active && let Some(analysis) = self.analysis.as_mut() {
            analysis.title.push_str(if preview_blocked {
                "  [CORRECTION BLOCKED - SEE QC]"
            } else if self.correction_recipe.convective_adjustment.enabled {
                "  [MANUAL + DRY ADJUSTMENT]"
            } else {
                "  [MANUAL CORRECTION]"
            });
        }
        // The classic plot receives the same corrected copy as SHARPpy. This
        // remains panel-local: `source.column` and the Rusty Weather store are
        // untouched, and Reset restores their exact values.
        self.inner.set_native_column(source.data, column);
        self.correction_result = result;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.ui_with_host(ui, false, &SoundingHeaderControls::default());
    }

    /// Render responsively inside a dock pane. Floating and docked hosts both
    /// size the complete board from their current content rectangle.
    pub fn ui_docked(&mut self, ui: &mut egui::Ui) {
        self.ui_with_host(ui, true, &SoundingHeaderControls::default());
    }

    pub(crate) fn ui_with_header(
        &mut self,
        ui: &mut egui::Ui,
        controls: &SoundingHeaderControls,
    ) -> SoundingHeaderActions {
        self.ui_with_host(ui, false, controls)
    }

    pub(crate) fn ui_docked_with_header(
        &mut self,
        ui: &mut egui::Ui,
        controls: &SoundingHeaderControls,
    ) -> SoundingHeaderActions {
        self.ui_with_host(ui, true, controls)
    }

    fn ui_with_host(
        &mut self,
        ui: &mut egui::Ui,
        docked: bool,
        controls: &SoundingHeaderControls,
    ) -> SoundingHeaderActions {
        let mut actions = SoundingHeaderActions::default();
        let mut capture_sounding_requested = false;
        let mut rendered_sounding_rect = None;
        let layout_id = Self::layout_memory_id();
        let stretch_id = layout_id.with("docked_stretch");
        let mut docked_stretch: bool = ui
            .ctx()
            .data_mut(|data| data.get_temp(stretch_id))
            .unwrap_or(self.docked_stretch);
        // A layout restored from saved view state lands in egui memory here,
        // on the first frame with a ctx. Model and native/RAOB panels share
        // this id; a second host opening later must not replay its stale
        // startup copy over geometry the first host has already edited.
        if let Some(tokens) = self.pending_layout_tokens.take()
            && let Some(layout) = sharppyrs::SoundingLayout::from_tokens(&tokens)
            && sharppyrs::stored_layout(ui.ctx(), layout_id).is_none()
        {
            sharppyrs::store_layout(ui.ctx(), layout_id, &layout);
        }
        if self.analysis.is_some() {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.classic, false, "SHARPpy");
                ui.selectable_value(&mut self.classic, true, "Classic");
                if !self.classic {
                    ui.menu_button("Text", |ui| {
                        ui.set_min_width(230.0);
                        ui.label(egui::RichText::new("FONT").small().strong());
                        for choice in SoundingFontChoice::ALL {
                            ui.selectable_value(&mut self.font_choice, choice, choice.label());
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("Size");
                            let mut percent = self.text_scale * 100.0;
                            if ui
                                .add(
                                    egui::Slider::new(&mut percent, 50.0..=200.0)
                                        .suffix("%")
                                        .integer(),
                                )
                                .changed()
                            {
                                self.text_scale = (percent / 100.0)
                                    .clamp(SHARPPY_TEXT_SCALE_MIN, SHARPPY_TEXT_SCALE_MAX);
                            }
                            if ui.small_button("Reset").on_hover_text("Reset size to 100%").clicked()
                            {
                                self.text_scale = SHARPPY_TEXT_SCALE_DEFAULT;
                            }
                        });
                        ui.weak("Changes sounding text only; panel geometry stays independent.");
                    })
                    .response
                    .on_hover_text("Sounding font family and independent text size");
                    ui.separator();
                    self.table_editor.header_button(ui, &self.table_config);
                    ui.separator();
                    capture_sounding_requested = ui
                        .small_button("Save image")
                        .on_hover_text(
                            "Copy the visible sounding board and save it as a native-resolution PNG",
                        )
                        .clicked();
                }
                let manual_editable = self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.manual_editable);
                if manual_editable {
                    ui.separator();
                    let active = self.active_manual_correction_count();
                    let adjusted = self.correction_recipe.convective_adjustment.enabled;
                    let label = if active > 0 {
                        format!("Corrected ({active})")
                    } else if adjusted {
                        "Corrected".to_owned()
                    } else {
                        "Correct".to_owned()
                    };
                    if ui
                        .add(egui::Button::selectable(
                            self.correction_editor.is_open(),
                            label,
                        ))
                        .on_hover_text(
                            "Open the resizable sounding-correction lab. Thermal, moisture, and U/V wind corrections have independent vertical domains and blend shapes; source files are never changed.",
                        )
                        .clicked()
                    {
                        if self.correction_editor.is_open() {
                            self.correction_editor.close();
                        } else {
                            self.correction_editor.open();
                            if self.correction_recipe.levels.is_empty() {
                                self.correction_recipe
                                    .levels
                                    .push(CorrectionLevel::at_height(0.0));
                            }
                        }
                    }
                    if active > 0 || adjusted {
                        ui.label(
                            egui::RichText::new("MANUAL")
                                .small()
                                .strong()
                                .color(egui::Color32::from_rgb(255, 190, 70)),
                        );
                    }
                }
                if let Some(box_sounding) = &controls.box_sounding {
                    ui.separator();
                    let response = ui
                        .add_enabled(
                            box_sounding.ready,
                            egui::Button::selectable(box_sounding.armed, "Box sounding"),
                        )
                        .on_hover_text(if box_sounding.ready {
                            "Arm the radar map, then drag a rectangle. BowEcho averages the model's primitive sounding columns inside it before deriving diagnostics."
                                .to_owned()
                        } else {
                            box_sounding.unavailable_reason.clone()
                        });
                    actions.toggle_box_sounding = response.clicked();
                }
                if let Some(obs_adjusted) = controls.obs_adjusted {
                    ui.separator();
                    actions.toggle_obs_adjusted = ui
                        .add(egui::Button::selectable(obs_adjusted, "Obs adjust"))
                        .on_hover_text(if obs_adjusted {
                            "Observation adjustment is on for model soundings. Turn it off without hiding the Surface obs map layer."
                        } else {
                            "Replace a model sounding's surface T/Td/wind with the nearest eligible observation and recompute diagnostics. This also enables Surface obs."
                        })
                        .clicked();
                }
                if !self.classic {
                    ui.separator();
                    ui.weak(if docked { "Pane" } else { "Window" });
                    ui.selectable_value(&mut docked_stretch, true, "Stretch")
                        .on_hover_text(
                            "Fill the current host in both directions. Resize the pane or window to resize the complete sounding.",
                        );
                    ui.selectable_value(&mut docked_stretch, false, "Fit")
                        .on_hover_text(
                            "Preserve the desktop board aspect ratio and leave unused space when the host has a different shape.",
                        );
                }
            });
        }
        if !self.classic {
            let formula = self.compatible_formula(controls).cloned();
            let catalog = sounding_table_builtin::catalog(formula.as_ref());
            let defaults = sounding_table_builtin::default_config();
            self.table_editor
                .show(ui.ctx(), &mut self.table_config, &defaults, &catalog);
        }
        if let Some(source) = self
            .source
            .as_ref()
            .filter(|source| source.manual_editable)
            .cloned()
        {
            let source_context = CorrectionSourceContext {
                source_kind: Some(if source.data.vars.is_empty() {
                    "native".to_owned()
                } else {
                    "model".to_owned()
                }),
                source_identity: Some(format!(
                    "{} {} F{:03}",
                    source.data.hour.model, source.data.hour.run, source.data.hour.hour
                )),
                model: Some(source.data.hour.model.clone()),
                run: Some(source.data.hour.run.clone()),
                lead: Some(format!("F{:03}", source.data.hour.hour)),
            };
            let batch_source = source.column.clone();
            let batch_data = source.data.clone();
            let batch_footprint = source.footprint;
            let mut batch_evaluator = |member_recipe: &CorrectionRecipe| {
                let corrected = apply_correction_recipe(&batch_source, member_recipe);
                let analysis = build_analysis(&batch_data, &corrected.column, batch_footprint)
                    .ok_or_else(|| "SHARPpy analysis rejected the corrected profile".to_owned())?;
                if corrected.has_errors() {
                    let errors = corrected
                        .issues
                        .iter()
                        .filter(|issue| issue.severity == QcSeverity::Error)
                        .map(|issue| issue.message.as_str())
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(if errors.is_empty() {
                        "correction engine reported an error".to_owned()
                    } else {
                        errors
                    });
                }
                Ok(batch_diagnostic_values(&analysis))
            };
            let outcome = self.correction_editor.show(
                ui.ctx(),
                &source.column,
                &source_context,
                &mut self.correction_recipe,
                self.correction_result.as_ref(),
                &mut batch_evaluator,
            );
            if let Some(imported) = outcome.imported_raw {
                self.install_imported_raw(imported);
            } else if outcome.recipe_changed {
                self.rebuild_from_source();
            }
        }
        if !self.classic
            && let Some(analysis) = self.analysis.as_ref()
        {
            let formula = self.compatible_formula(controls);
            let diagnostic_table_overrides =
                sounding_table_builtin::build_board(&self.table_config, analysis, formula);
            let render_style = self.sharppy_style();
            let available = ui.available_size();
            let size = sounding_canvas_size(available, docked_stretch);
            let view = || {
                let view = sharppyrs::SoundingView::new(&analysis.prof, &analysis.derived)
                    .title(analysis.title.clone())
                    .brand("BowEcho")
                    .style(render_style.clone())
                    .layout_memory_id(layout_id)
                    .size(size);
                let Some(overrides) = diagnostic_table_overrides.as_ref() else {
                    return view;
                };
                let view = if overrides.generic.panels.is_empty() {
                    view
                } else {
                    view.diagnostic_tables(&overrides.generic)
                };
                if overrides.native_patches.patches.is_empty() {
                    view
                } else {
                    view.native_diagnostic_patches(&overrides.native_patches)
                }
            };
            let response = egui::Frame::new()
                .fill(egui::Color32::BLACK)
                .show(ui, |ui| {
                    ui.add(view());
                });
            rendered_sounding_rect = Some(response.response.rect);
        } else {
            self.inner.ui(ui);
        }
        // Mirror the (possibly gear-edited) layout back out so the ctx-less
        // `view_state_json` can persist it.
        if let Some(layout) = sharppyrs::stored_layout(ui.ctx(), layout_id) {
            self.layout_tokens = Some(layout.to_tokens());
        }
        self.docked_stretch = docked_stretch;
        ui.ctx()
            .data_mut(|data| data.insert_temp(stretch_id, docked_stretch));
        if capture_sounding_requested {
            actions.capture_sounding = rendered_sounding_rect
                .map(|rect| rect.intersect(ui.clip_rect()))
                .filter(|rect| rect.is_positive());
        }
        actions
    }
}

/// Build the sharppyrs analysis from the exact column the classic panel
/// renders (store-native units: u/v m/s -> wdir/wspd kt).
fn build_analysis(
    data: &SoundingData,
    column: &SoundingColumn,
    explicit_footprint: Option<sharppyrs::LocationFootprint>,
) -> Option<Box<SharppyAnalysis>> {
    let n = column.len();
    if n < 3 {
        return None;
    }
    let mut wdir = vec![f64::NAN; n];
    let mut wspd = vec![f64::NAN; n];
    for i in 0..n {
        let (u, v) = (column.u_ms[i], column.v_ms[i]);
        if u.is_finite() && v.is_finite() {
            let speed = (u * u + v * v).sqrt();
            let mut dir = (-u).atan2(-v).to_degrees();
            if dir < 0.0 {
                dir += 360.0;
            }
            wdir[i] = dir;
            wspd[i] = speed * MS_TO_KT;
        }
    }
    let meta = &column.metadata;
    let latitude = meta
        .latitude_deg
        .or_else(|| data.lat.map(f64::from))
        .unwrap_or(35.0);
    let station = sharppyrs::sharprs::profile::StationInfo {
        station_id: meta.station_id.clone(),
        latitude,
        longitude: meta
            .longitude_deg
            .or_else(|| data.lon.map(f64::from))
            .unwrap_or(f64::NAN),
        elevation: meta.elevation_m.unwrap_or(f64::NAN),
        datetime: meta.valid_time.clone(),
    };
    let sp = sharppyrs::sharprs::Profile::new(
        &column.pressure_hpa,
        &column.height_m_msl,
        &column.temperature_c,
        &column.dewpoint_c,
        &wdir,
        &wspd,
        &column.omega_pa_s,
        station,
    )
    .ok()?;
    let mut prof = sharppyrs::Profile::from_sharprs(sp);
    prof.set_location_footprint(
        explicit_footprint.or_else(|| location_footprint_from_metadata(meta)),
    );
    let derived = sharppyrs::DerivedParams::compute(&prof);

    let title = sounding_title(data, meta);
    let obs_adjusted_model = data.hour.run.contains("obs-adj");

    Some(Box::new(SharppyAnalysis {
        prof,
        derived,
        title,
        obs_adjusted_model,
    }))
}

fn batch_diagnostic_values(analysis: &SharppyAnalysis) -> BatchDiagnosticValues {
    let derived = &analysis.derived;
    BatchDiagnosticValues {
        values: vec![
            analysis.prof.sfcpcl.bplus,
            analysis.prof.sfcpcl.bminus,
            analysis.prof.sfcpcl.lclhght,
            analysis.prof.mlpcl.bplus,
            analysis.prof.mupcl.bplus,
            derived.pwat,
            derived.dcape,
            derived.lapserate_3km,
            derived.srh1km,
            derived.srh3km,
            derived.sfc_6km_shear.0.hypot(derived.sfc_6km_shear.1),
            derived.stp_cin,
            derived.right_scp,
        ],
    }
}

fn location_footprint_from_metadata(
    meta: &rustwx_sounding::SoundingMetadata,
) -> Option<sharppyrs::LocationFootprint> {
    let latitude = meta.latitude_deg?;
    let longitude = meta.longitude_deg?;
    let lat_radius = meta
        .box_radius_lat_deg
        .filter(|radius| radius.is_finite() && *radius >= 0.0)?;
    let lon_radius = meta
        .box_radius_lon_deg
        .filter(|radius| radius.is_finite() && *radius >= 0.0)?;
    sharppyrs::LocationFootprint::new(
        latitude - lat_radius,
        longitude - lon_radius,
        latitude + lat_radius,
        longitude + lon_radius,
    )
}

/// Compose the SHARPpy board headline. Observation-adjusted model profiles
/// already carry the observation station and time in `hour.run`; repeating
/// the model valid time after that makes the useful coordinates disappear in
/// narrower docks.
fn sounding_title(data: &SoundingData, meta: &rustwx_sounding::SoundingMetadata) -> String {
    // "HRRR 2026-06-25 06z F018  Valid: ... @36.68°N 95.66°W" style title.
    let mut title = format!(
        "{} {} F{:03}",
        data.hour.model.to_uppercase(),
        data.hour.run,
        data.hour.hour
    );
    if !data.hour.run.contains("obs-adj") && !meta.valid_time.is_empty() {
        title.push_str(&format!("  Valid: {}", meta.valid_time));
    }
    if let (Some(lat), Some(lon)) = (
        meta.latitude_deg.or_else(|| data.lat.map(f64::from)),
        meta.longitude_deg.or_else(|| data.lon.map(f64::from)),
    ) {
        let ns = if lat >= 0.0 { "N" } else { "S" };
        let ew = if lon >= 0.0 { "E" } else { "W" };
        title.push_str(&format!(
            "  @{:.2}\u{b0}{ns} {:.2}\u{b0}{ew}",
            lat.abs(),
            lon.abs()
        ));
    }

    title
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store-native column converts into a full sharppyrs analysis:
    /// winds become wdir/wspd kt, parcels lift, and the headline indices
    /// come out finite for a convective column.
    #[test]
    fn column_converts_to_analysis() {
        let pres = [
            1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0,
        ];
        let hght = [
            110.0, 780.0, 1500.0, 3100.0, 5800.0, 7500.0, 9600.0, 10900.0, 12300.0, 14100.0,
        ];
        let tmpc = [
            27.0, 22.0, 17.5, 8.0, -8.5, -20.0, -36.0, -46.0, -55.0, -60.0,
        ];
        let dwpc = [
            22.0, 19.0, 15.0, 4.0, -15.0, -30.0, -48.0, -58.0, -68.0, -75.0,
        ];
        let u = [2.0, 6.0, 9.0, 13.0, 18.0, 22.0, 27.0, 30.0, 32.0, 30.0];
        let v = [8.0, 10.0, 11.0, 12.0, 14.0, 15.0, 16.0, 16.0, 15.0, 14.0];
        let column = SoundingColumn {
            pressure_hpa: pres.to_vec(),
            height_m_msl: hght.to_vec(),
            temperature_c: tmpc.to_vec(),
            dewpoint_c: dwpc.to_vec(),
            u_ms: u.to_vec(),
            v_ms: v.to_vec(),
            omega_pa_s: vec![f64::NAN; pres.len()],
            metadata: rustwx_sounding::SoundingMetadata {
                station_id: "TEST".to_owned(),
                valid_time: "2026-06-26 00z".to_owned(),
                latitude_deg: Some(36.7),
                longitude_deg: Some(-95.7),
                elevation_m: Some(110.0),
                sample_method: None,
                box_radius_lat_deg: None,
                box_radius_lon_deg: None,
            },
        };
        let data = SoundingData {
            hour: rw_ui::HourKey {
                model: "hrrr".to_owned(),
                run: "2026-06-25 06z".to_owned(),
                hour: 18,
                exact_time: None,
            },
            fx: 0.0,
            fy: 0.0,
            lat: Some(36.7),
            lon: Some(-95.7),
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        };
        let analysis = build_analysis(&data, &column, None).expect("analysis builds");
        assert!(
            analysis.prof.mupcl.bplus > 0.0,
            "convective column has CAPE"
        );
        assert!(analysis.derived.pwat.is_finite(), "PWAT computes");
        assert!(analysis.derived.srh1km.is_finite(), "SRH computes");
        let batch = batch_diagnostic_values(&analysis);
        assert_eq!(
            batch.values.len(),
            crate::sounding_correction_ui::BATCH_DIAGNOSTICS.len()
        );
        assert_eq!(batch.values[0], analysis.prof.sfcpcl.bplus);
        assert_eq!(batch.values[5], analysis.derived.pwat);
        assert_eq!(batch.values[11], analysis.derived.stp_cin);
        assert!(analysis.title.starts_with("HRRR 2026-06-25 06z F018"));
        assert!(analysis.title.contains("Valid: 2026-06-26 00z"));

        let sampled = sharppyrs::LocationFootprint::new(36.3, -96.2, 37.0, -95.1).unwrap();
        let explicit =
            build_analysis(&data, &column, Some(sampled)).expect("sampled extent builds");
        assert_eq!(explicit.prof.location_footprint(), Some(sampled));
    }

    #[test]
    fn box_metadata_becomes_a_location_map_footprint() {
        let metadata = rustwx_sounding::SoundingMetadata {
            station_id: "BOX".to_owned(),
            valid_time: String::new(),
            latitude_deg: Some(36.7),
            longitude_deg: Some(-95.7),
            elevation_m: None,
            sample_method: Some("MeanProfile".to_owned()),
            box_radius_lat_deg: Some(0.5),
            box_radius_lon_deg: Some(0.75),
        };

        let footprint = location_footprint_from_metadata(&metadata).expect("box footprint");

        assert!((footprint.south - 36.2).abs() < 1e-10);
        assert!((footprint.west + 96.45).abs() < 1e-10);
        assert!((footprint.north - 37.2).abs() < 1e-10);
        assert!((footprint.east + 94.95).abs() < 1e-10);
    }

    #[test]
    fn obs_adjusted_title_omits_redundant_valid_time_but_keeps_coordinates() {
        let data = SoundingData {
            hour: rw_ui::HourKey {
                model: "hrrr".to_owned(),
                run: "20260702_22z · OH074 obs-adj 7km @ 2026-07-14 20:20Z".to_owned(),
                hour: 6,
                exact_time: None,
            },
            fx: 0.0,
            fy: 0.0,
            lat: Some(39.97),
            lon: Some(-81.48),
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        };
        let metadata = rustwx_sounding::SoundingMetadata {
            station_id: "OH074 obs-adj 7km".to_owned(),
            valid_time: "20260702 22z F006".to_owned(),
            latitude_deg: Some(39.97),
            longitude_deg: Some(-81.48),
            elevation_m: None,
            sample_method: None,
            box_radius_lat_deg: None,
            box_radius_lon_deg: None,
        };

        let title = sounding_title(&data, &metadata);

        assert!(title.contains("OH074 obs-adj 7km @ 2026-07-14 20:20Z F006"));
        assert!(!title.contains("Valid:"), "{title}");
        assert!(title.ends_with("@39.97°N 81.48°W"), "{title}");
    }

    fn manual_test_column() -> SoundingColumn {
        SoundingColumn {
            pressure_hpa: vec![1000.0, 950.0, 900.0, 850.0, 800.0],
            height_m_msl: vec![100.0, 600.0, 1100.0, 1600.0, 2100.0],
            temperature_c: vec![20.0, 16.0, 12.0, 8.0, 4.0],
            dewpoint_c: vec![12.0, 9.0, 6.0, 3.0, 0.0],
            u_ms: vec![0.0, 1.0, 2.0, 3.0, 4.0],
            v_ms: vec![-5.0, -6.0, -7.0, -8.0, -9.0],
            omega_pa_s: vec![0.0; 5],
            metadata: rustwx_sounding::SoundingMetadata::default(),
        }
    }

    fn manual_test_data(model: &str) -> SoundingData {
        SoundingData {
            hour: rw_ui::HourKey {
                model: model.to_owned(),
                run: "test".to_owned(),
                hour: 0,
                exact_time: None,
            },
            fx: 0.0,
            fy: 0.0,
            lat: Some(35.0),
            lon: Some(-97.0),
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        }
    }

    #[test]
    fn model_correction_source_is_kept_but_raob_is_not_editable() {
        use crate::sounding_correction::{ThermalEdit, ThermalTarget};

        let mut panel = SharppySoundingPanel::new();
        let model_column = manual_test_column();
        panel.install_source(manual_test_data("wrf"), model_column.clone(), None);
        assert!(panel.source.as_ref().unwrap().manual_editable);
        assert_eq!(panel.source.as_ref().unwrap().column, model_column);

        let mut level = CorrectionLevel::at_height(0.0);
        level.thermal = Some(ThermalEdit::new(ThermalTarget::TemperatureC(25.0)));
        panel.correction_recipe.levels.push(level);
        panel.rebuild_from_source();
        assert_eq!(panel.source.as_ref().unwrap().column, model_column);

        panel.correction_editor.open();
        panel.install_source(manual_test_data("KOUN RAOB"), manual_test_column(), None);
        assert!(!panel.source.as_ref().unwrap().manual_editable);
        assert!(panel.correction_recipe.levels.is_empty());
        assert!(!panel.correction_editor.is_open());
    }

    #[test]
    fn imported_raw_replaces_the_source_with_a_native_editable_profile() {
        let mut panel = SharppySoundingPanel::new();
        panel.install_source(manual_test_data("wrf"), manual_test_column(), None);
        panel.correction_editor.open();
        let mut imported_column = manual_test_column();
        imported_column.omega_pa_s.clear();
        imported_column.metadata.station_id = "RAW TEST".to_owned();

        panel.install_imported_raw(ImportedRawSounding {
            title: "RAW TEST".to_owned(),
            column: imported_column.clone(),
            skipped_missing_rows: 0,
        });

        let source = panel.source.as_ref().expect("imported source");
        assert!(source.manual_editable);
        assert_eq!(source.data.hour.model, "SHARPpy RAW");
        assert_eq!(source.column, imported_column);
        assert!(panel.correction_editor.is_open());
        assert!(panel.correction_recipe.levels.is_empty());
    }

    #[test]
    fn formula_table_value_requires_exact_model_hour_and_never_reaches_raob() {
        let mut panel = SharppySoundingPanel::new();
        let model = manual_test_data("wrf");
        panel.install_source(model.clone(), manual_test_column(), None);
        let mut controls = SoundingHeaderControls {
            formula_diagnostic: Some(FormulaSoundingDiagnostic {
                id: "formula_lab:test".to_owned(),
                label: "Test".to_owned(),
                units: "K".to_owned(),
                source_hour: model.hour.clone(),
                value: Some(300.0),
                unavailable_reason: None,
            }),
            ..Default::default()
        };
        assert!(panel.compatible_formula(&controls).is_some());

        controls
            .formula_diagnostic
            .as_mut()
            .unwrap()
            .source_hour
            .hour += 1;
        assert!(panel.compatible_formula(&controls).is_none());

        let raob = manual_test_data("KOUN RAOB");
        controls.formula_diagnostic.as_mut().unwrap().source_hour = raob.hour.clone();
        panel.install_source(raob, manual_test_column(), None);
        assert!(panel.compatible_formula(&controls).is_none());
    }

    #[test]
    fn custom_table_board_round_trips_through_sounding_view_state() {
        let mut panel = SharppySoundingPanel::new();
        panel.table_config = sounding_table_builtin::default_config();
        let state = panel.view_state_json();
        assert!(state.get("sharppy_table_board").is_some());

        let mut restored = SharppySoundingPanel::new();
        assert!(restored.apply_view_state_json(&state));
        assert_eq!(restored.table_config, panel.table_config);

        restored.table_config.reset_to_canonical();
        assert!(
            restored
                .view_state_json()
                .get("sharppy_table_board")
                .is_none()
        );
    }

    /// Old saves (plain classic-panel state, no `sharppy_layout` key) still
    /// apply, and the emitted view state keeps the classic keys the app
    /// patches directly (`["zooms"]["scene"]`).
    #[test]
    fn view_state_stays_compatible_with_old_saves() {
        let mut panel = SharppySoundingPanel::new();
        let mut old_save = panel.inner.view_state_json();
        crate::model_data::patch_sounding_scene_zoom(&mut old_save, 1.25);
        assert!(panel.apply_view_state_json(&old_save));
        let back = panel.view_state_json();
        assert!((back["zooms"]["scene"].as_f64().unwrap() - 1.25).abs() < 1e-6);
        assert!(
            back.get("sharppy_layout").is_none(),
            "no layout seen yet, none emitted"
        );
        // The augmented shape must still be valid input for the classic panel.
        assert!(panel.inner.apply_view_state_json(&back));
    }

    #[test]
    fn typography_defaults_are_backward_compatible_and_persisted() {
        let mut panel = SharppySoundingPanel::new();
        let old_save = panel.inner.view_state_json();
        assert!(panel.apply_view_state_json(&old_save));

        let state = panel.view_state_json();
        assert_eq!(state["sharppy_font_preset"].as_str(), Some("space-grotesk"));
        assert_eq!(state["sharppy_text_scale"].as_f64(), Some(1.0));
    }

    #[test]
    fn typography_round_trips_and_clamps_persisted_scale() {
        let mut panel = SharppySoundingPanel::new();
        let mut state = panel.view_state_json();
        state["sharppy_font_preset"] = serde_json::json!("technical-mono");
        state["sharppy_text_scale"] = serde_json::json!(1.35);
        assert!(panel.apply_view_state_json(&state));

        let emitted = panel.view_state_json();
        assert_eq!(
            emitted["sharppy_font_preset"].as_str(),
            Some("technical-mono")
        );
        assert!((emitted["sharppy_text_scale"].as_f64().unwrap() - 1.35).abs() < 1e-5);

        state["sharppy_text_scale"] = serde_json::json!(9.0);
        assert!(panel.apply_view_state_json(&state));
        assert_eq!(
            panel.view_state_json()["sharppy_text_scale"].as_f64(),
            Some(2.0)
        );
    }

    #[test]
    fn typography_selection_reaches_the_render_style() {
        let mut panel = SharppySoundingPanel::new();
        panel.font_choice = SoundingFontChoice::CleanSans;
        panel.text_scale = 1.25;
        let clean = panel.sharppy_style().regular_font(10.0);
        assert_eq!(clean.family, egui::FontFamily::Proportional);
        assert!((clean.size - 12.5).abs() < 0.001);

        panel.font_choice = SoundingFontChoice::TechnicalMono;
        let mono = panel.sharppy_style().bold_font(10.0);
        assert_eq!(mono.family, egui::FontFamily::Monospace);
        assert!((mono.size - 12.5).abs() < 0.001);

        panel.font_choice = SoundingFontChoice::SpaceGrotesk;
        let space = panel.sharppy_style().regular_font(10.0);
        assert_eq!(
            space.family,
            egui::FontFamily::Name(sharppyrs::FONT_FAMILY.into())
        );
    }

    #[test]
    fn model_and_native_hosts_share_typography_through_view_state_handoff() {
        let mut model = SharppySoundingPanel::new();
        model.font_choice = SoundingFontChoice::TechnicalMono;
        model.text_scale = 1.6;

        let mut native = SharppySoundingPanel::new();
        assert!(native.apply_view_state_json(&model.view_state_json()));
        assert_eq!(native.font_choice, SoundingFontChoice::TechnicalMono);
        assert!((native.text_scale - 1.6).abs() < f32::EPSILON);
        assert_eq!(
            native.view_state_json()["sharppy_font_preset"].as_str(),
            Some("technical-mono")
        );
    }

    /// The SPC-window layout tokens ride along in the view-state JSON and
    /// survive an apply -> emit round trip, without disturbing zoom patching.
    #[test]
    fn layout_tokens_round_trip_through_view_state() {
        let tokens =
            "hidden,advection|slinky|speed,thetae,srwinds,hazardtype|indexboard,ship,stp|180";
        let canonical = sharppyrs::SoundingLayout::from_tokens(tokens)
            .expect("legacy custom layout migrates")
            .to_tokens();
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!(tokens));
        assert!(panel.apply_view_state_json(&save));
        let mut emitted = panel.view_state_json();
        assert_eq!(emitted["sharppy_layout"].as_str(), Some(canonical.as_str()));
        crate::model_data::patch_sounding_scene_zoom(&mut emitted, 1.4);
        assert_eq!(emitted["sharppy_layout"].as_str(), Some(canonical.as_str()));
        assert!((emitted["zooms"]["scene"].as_f64().unwrap() - 1.4).abs() < 1e-6);
        // Malformed tokens are dropped rather than persisted or applied.
        let mut bad = panel.inner.view_state_json();
        bad.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!("gibberish"));
        let mut fresh = SharppySoundingPanel::new();
        assert!(fresh.apply_view_state_json(&bad));
        assert!(fresh.view_state_json().get("sharppy_layout").is_none());
    }

    #[test]
    fn legacy_default_stp_layout_migrates_to_wider_index_board() {
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut().unwrap().insert(
            "sharppy_layout".to_owned(),
            serde_json::json!(LEGACY_DEFAULT_LAYOUT_WITH_STP),
        );
        assert!(panel.apply_view_state_json(&save));

        let state = panel.view_state_json();
        let migrated = state["sharppy_layout"]
            .as_str()
            .expect("migrated layout tokens");
        assert_eq!(migrated, sharppyrs::SoundingLayout::default().to_tokens());
        assert!(
            migrated
                .contains("convectiveindices,kinematics,ship,severeindices,streamwiseness,hidden")
        );
    }

    #[test]
    fn stretched_sounding_canvas_tracks_the_resizable_host_exactly() {
        assert_eq!(
            sounding_canvas_size(egui::vec2(1_200.0, 700.0), true),
            egui::vec2(1_200.0, 700.0)
        );
        assert_eq!(
            sounding_canvas_size(egui::vec2(720.0, 430.0), true),
            egui::vec2(720.0, 430.0)
        );
    }

    #[test]
    fn fitted_sounding_canvas_scales_uniformly_without_exceeding_host() {
        let narrow = sounding_canvas_size(egui::vec2(1_000.0, 850.0), false);
        assert!((narrow.x - 1_000.0).abs() < 0.01);
        assert!((narrow.y - 552.147_2).abs() < 0.01);
        assert!(
            (narrow.x / narrow.y - SHARPPY_CANVAS_MIN_WIDTH / SHARPPY_CANVAS_MIN_HEIGHT).abs()
                < 0.001
        );

        let short = sounding_canvas_size(egui::vec2(1_800.0, 700.0), false);
        assert!((short.y - 700.0).abs() < 0.01);
        assert!(short.x < 1_800.0);

        let tiny = sounding_canvas_size(egui::vec2(120.0, 60.0), false);
        assert!(tiny.x <= 120.0 && tiny.y <= 60.0);
    }

    #[test]
    fn docked_stretch_choice_round_trips_in_view_state() {
        let mut panel = SharppySoundingPanel::new();
        assert_eq!(
            panel.view_state_json()["sharppy_docked_stretch"].as_bool(),
            Some(true)
        );
        let mut state = panel.view_state_json();
        state["sharppy_docked_stretch"] = serde_json::json!(false);
        assert!(panel.apply_view_state_json(&state));
        assert_eq!(
            panel.view_state_json()["sharppy_docked_stretch"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn model_and_native_hosts_share_docked_stretch_live() {
        let mut first = SharppySoundingPanel::new();
        let mut first_state = first.view_state_json();
        first_state["sharppy_docked_stretch"] = serde_json::json!(false);
        assert!(first.apply_view_state_json(&first_state));
        let mut second = SharppySoundingPanel::new();
        assert_eq!(
            second.view_state_json()["sharppy_docked_stretch"].as_bool(),
            Some(true)
        );

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| first.ui_docked(ui));
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| second.ui_docked(ui));
        assert_eq!(
            second.view_state_json()["sharppy_docked_stretch"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn unopened_second_host_cannot_replay_stale_layout_tokens() {
        let mut second = SharppySoundingPanel::new();
        let mut stale = second.view_state_json();
        stale.as_object_mut().unwrap().insert(
            "sharppy_layout".to_owned(),
            serde_json::json!(sharppyrs::SoundingLayout::default().to_tokens()),
        );
        assert!(second.apply_view_state_json(&stale));

        let ctx = egui::Context::default();
        let mut edited = sharppyrs::SoundingLayout::default();
        edited.top_height_fraction = 0.58;
        edited.bottom_column_fractions = [0.30, 0.20, 0.15, 0.15, 0.20];
        sharppyrs::store_layout(&ctx, SharppySoundingPanel::layout_memory_id(), &edited);

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| second.ui_docked(ui));
        let stored = sharppyrs::stored_layout(&ctx, SharppySoundingPanel::layout_memory_id())
            .expect("shared layout remains installed");
        assert_eq!(stored, edited);
    }

    /// A restored layout lands in egui memory on the next `ui()` frame under
    /// the pinned id, and `ui()` mirrors the in-memory layout back into the
    /// tokens the ctx-less `view_state_json` emits.
    #[test]
    fn pending_layout_lands_in_egui_memory_on_ui() {
        let tokens =
            "speed,advection|hodograph|slinky,thetae,srwinds,hazardtype|indexboard,ship,stp|300";
        let canonical = sharppyrs::SoundingLayout::from_tokens(tokens)
            .expect("legacy custom layout migrates")
            .to_tokens();
        let mut panel = SharppySoundingPanel::new();
        let mut save = panel.inner.view_state_json();
        save.as_object_mut()
            .unwrap()
            .insert("sharppy_layout".to_owned(), serde_json::json!(tokens));
        assert!(panel.apply_view_state_json(&save));

        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| panel.ui(ui));
        let stored = sharppyrs::stored_layout(&ctx, SharppySoundingPanel::layout_memory_id())
            .expect("layout stored under the pinned id");
        assert_eq!(stored.to_tokens(), canonical);
        assert_eq!(
            panel.view_state_json()["sharppy_layout"].as_str(),
            Some(canonical.as_str())
        );
    }
}
