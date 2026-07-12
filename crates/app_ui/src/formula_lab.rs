//! Native egui Formula Lab for raw WRF files and rw-store fields.
//!
//! This module owns presentation and background orchestration only. Scientific
//! resolution, time honesty, output-shape validation, and f64-to-f32 narrowing
//! live in `rw-formula`. The host supplies the current evaluation source and
//! installs a completed `FieldData` into its viewer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::SystemTime;

use eframe::egui;
use rw_formula::{
    BoundaryPolicy, BridgeError, CompiledFormula, ErrorKind, EvaluationOptions, ExactStoreTime,
    FormulaError, FormulaProvenance, HeightDatum, MissingPolicy, NonFinitePolicy, ParameterSpec,
    ParameterValues, Recipe, RecipeReference, RecipeRequirements, Requirement, ResourceLimits,
    Span, StoreRunResolver, evaluate_resolver_2d, evaluate_wrf_path_2d_with_limits,
};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use rw_store::atomic::atomic_write_bytes;
use rw_store::grid::GridFile;
use rw_store::run::RwsRunManifest;
use rw_ui::{FieldData, FieldKey, HourKey, VarInfo, VarKind};

const LARGE_RAW_WRF_BYTES: u64 = 1 << 30;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const MAX_RECIPE_BYTES: u64 = 4 * 1024 * 1024;
const PACKED_WRF_FORMULA_FIELDS: &[&str] = &[
    "cape2d",
    "uvmet",
    "uvmet10",
    "bunkers_rm",
    "bunkers_lm",
    "mean_wind_0_6km",
    "mean_wind",
    "effective_inflow",
    "cloudfrac",
];

/// Current rw-store source offered by the host.
#[derive(Debug, Clone)]
pub struct StoreFormulaSource {
    pub store_root: PathBuf,
    pub hour: HourKey,
    /// Must be empty unless the host verified every stored timestep's valid
    /// time. Exact-time runs supply the complete map, never a partial axis.
    pub exact_times: BTreeMap<u16, ExactStoreTime>,
    /// Host validation that the complete selected time axis has at least two
    /// distinct, strictly increasing times. Pointwise formulas remain
    /// available when false; only plans requiring adjacent times are blocked.
    pub temporal_axis_verified: bool,
    /// Complete variable inventory for the selected timestep, supplied by the
    /// same rw-ui `HourVars` response that populates the Models viewer. Keeping
    /// names, units, kinds, and pressure levels lets Formula Lab preflight the
    /// selected dataset before a background evaluation is launched.
    pub variables: Vec<VarInfo>,
}

/// Staged raw WRF source offered by the host. Full map/height calculus is
/// available. `display_hour` supplies model/run identity; Formula Lab replaces
/// its numeric hour with the selected WRF time index for the ephemeral field.
#[derive(Debug, Clone)]
pub struct RawWrfFormulaSource {
    pub path: PathBuf,
    pub initial_time_index: usize,
    pub display_hour: HourKey,
}

/// Both sources can be present. Formula Lab renders an explicit Store/Raw WRF
/// selector instead of silently preferring one.
#[derive(Clone, Copy, Default)]
pub struct FormulaLabSources<'a> {
    pub store: Option<&'a StoreFormulaSource>,
    pub raw_wrf: Option<&'a RawWrfFormulaSource>,
    /// Host-side writer/import activity that makes a stable source snapshot
    /// impossible. Formula Lab keeps compiling but cannot launch evaluation.
    pub evaluation_blocked: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormulaSourceKind {
    Store,
    RawWrf,
}

#[derive(Debug, Default)]
struct SourceReadiness {
    ready: bool,
    blockers: Vec<String>,
    notes: Vec<String>,
    override_suggestions: Vec<UnitOverrideSuggestion>,
}

#[derive(Debug, Clone)]
struct UnitOverrideSuggestion {
    field: String,
    stored_units: String,
    formula_units: &'static str,
}

#[derive(Debug)]
struct FormulaStarter {
    title: &'static str,
    description: String,
    source: Option<String>,
    output_name: &'static str,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct FormulaLabPersistedState {
    schema: u8,
    source: String,
    output_name: String,
    recipe_name: String,
    recipe_version: String,
    recipe_description: String,
    expected_output_units: String,
    authors: Vec<String>,
    references: Vec<RecipeReference>,
    tags: Vec<String>,
    requirements: RecipeRequirements,
    resource_limits: Option<ResourceLimits>,
    parameter_specs: Vec<ParameterSpec>,
    parameter_values: ParameterValues,
    evaluation_options: EvaluationOptions,
    unit_overrides_text: String,
    source_kind: String,
    large_research_profile: bool,
    field_filter: String,
}

#[derive(Debug, Clone)]
enum EvaluationSource {
    Store(StoreFormulaSource),
    RawWrf {
        path: PathBuf,
        time_index: usize,
        display_hour: HourKey,
        revision: RawFileRevision,
    },
}

impl EvaluationSource {
    fn label(&self) -> String {
        match self {
            Self::Store(source) => {
                let time_note = if source.temporal_axis_verified {
                    "dt ready: verified adjacent time axis"
                } else if source.exact_times.is_empty() {
                    "dt disabled: valid times not verified"
                } else {
                    "dt disabled: fewer than two increasing times"
                };
                format!("Store {} ({time_note})", source.hour)
            }
            Self::RawWrf {
                path, time_index, ..
            } => format!("Raw WRF {} · time index {time_index}", path.display()),
        }
    }

    fn display_hour(&self) -> &HourKey {
        match self {
            Self::Store(source) => &source.hour,
            Self::RawWrf { display_hour, .. } => display_hour,
        }
    }

    fn result_source(&self) -> FormulaResultSource {
        match self {
            Self::Store(source) => FormulaResultSource::Store {
                store_root: source.store_root.clone(),
                hour: source.hour.clone(),
            },
            Self::RawWrf {
                path,
                time_index,
                revision,
                ..
            } => FormulaResultSource::RawWrf {
                path: path.clone(),
                time_index: *time_index,
                revision: revision.clone(),
            },
        }
    }
}

/// A completed UI result. The host should pass `field` to
/// `FieldViewerPanel::install_generated_field` with the current style settings
/// and may retain provenance for export/research records.
#[derive(Debug, Clone)]
pub struct FormulaLabResult {
    pub field: FieldData,
    pub description: String,
    pub provenance: FormulaProvenance,
    pub warnings: Vec<String>,
    pub source: FormulaResultSource,
}

/// Source identity captured when an asynchronous evaluation starts. The host
/// uses it to discard a result if the user switches store/hour/raw file before
/// the worker completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormulaResultSource {
    Store {
        store_root: PathBuf,
        hour: HourKey,
    },
    RawWrf {
        path: PathBuf,
        time_index: usize,
        revision: RawFileRevision,
    },
}

impl FormulaResultSource {
    /// Final host-side acceptance guard. The panel checks the revision while
    /// polling too, but the caller may perform additional work before it
    /// installs the generated field.
    pub(crate) fn revision_is_current(&self) -> bool {
        match self {
            Self::Store { .. } => true,
            Self::RawWrf { path, revision, .. } => inspect_raw_file_revision(path)
                .as_ref()
                .is_ok_and(|current| current == revision),
        }
    }
}

struct EvaluationTask {
    rx: Receiver<Result<FormulaLabResult, String>>,
    generation: u64,
    source: FormulaResultSource,
    raw_revision: Option<RawFileRevision>,
}

/// Cheap file identity used to invalidate consent and in-flight results when a
/// producer replaces or continues writing a raw WRF file at the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFileRevision {
    canonical_path: PathBuf,
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
}

/// Run-wide persisted identity captured around one store-backed evaluation.
/// Formula recipes may read adjacent times, so guarding only the displayed
/// hour can still accept a mixed-revision temporal stencil.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoreRunRevision {
    manifest: RawFileRevision,
    grid: RawFileRevision,
    hours: Vec<(u16, RawFileRevision)>,
}

/// Reusable Formula Lab editor state. Window and docking ownership stay with
/// the BowEcho host so this body can render identically in a dock tile or a
/// floating workspace window.
pub struct FormulaLabPanel {
    source: String,
    output_name: String,
    recipe_name: String,
    recipe_version: String,
    recipe_description: String,
    expected_output_units: String,
    authors: Vec<String>,
    references: Vec<RecipeReference>,
    tags: Vec<String>,
    requirements: RecipeRequirements,
    resource_limits: Option<ResourceLimits>,
    parameter_specs: Vec<ParameterSpec>,
    parameter_values: ParameterValues,
    evaluation_options: EvaluationOptions,
    unit_overrides_text: String,
    compiled: Option<CompiledFormula>,
    compile_error: Option<FormulaError>,
    task: Option<EvaluationTask>,
    status: Option<String>,
    last_provenance: Option<FormulaProvenance>,
    last_warnings: Vec<String>,
    raw_path: Option<PathBuf>,
    raw_revision: Option<RawFileRevision>,
    raw_source_error: Option<String>,
    raw_time_index: usize,
    source_kind: FormulaSourceKind,
    large_raw_confirmed: bool,
    large_research_profile: bool,
    field_filter: String,
    editor_cursor: usize,
    editor_selection: Option<(usize, usize)>,
    pending_editor_cursor: Option<usize>,
    /// Every input that can affect an evaluation advances this counter. A
    /// worker captures it at launch and can never publish an obsolete result.
    editor_generation: u64,
}

impl Default for FormulaLabPanel {
    fn default() -> Self {
        let mut panel = Self {
            source: "sqrt(u_10m^2 + v_10m^2)".to_string(),
            output_name: "formula_result".to_string(),
            recipe_name: "formula_result".to_string(),
            recipe_version: "1.0.0".to_string(),
            recipe_description: "Custom Formula Lab diagnostic".to_string(),
            expected_output_units: String::new(),
            authors: Vec::new(),
            references: Vec::new(),
            tags: Vec::new(),
            requirements: RecipeRequirements::default(),
            resource_limits: None,
            parameter_specs: Vec::new(),
            parameter_values: BTreeMap::new(),
            evaluation_options: EvaluationOptions::default(),
            unit_overrides_text: String::new(),
            compiled: None,
            compile_error: None,
            task: None,
            status: None,
            last_provenance: None,
            last_warnings: Vec::new(),
            raw_path: None,
            raw_revision: None,
            raw_source_error: None,
            raw_time_index: 0,
            source_kind: FormulaSourceKind::Store,
            large_raw_confirmed: false,
            large_research_profile: false,
            field_filter: String::new(),
            editor_cursor: "sqrt(u_10m^2 + v_10m^2)".chars().count(),
            editor_selection: Some((
                "sqrt(u_10m^2 + v_10m^2)".chars().count(),
                "sqrt(u_10m^2 + v_10m^2)".chars().count(),
            )),
            pending_editor_cursor: None,
            editor_generation: 0,
        };
        panel.refresh_compile();
        panel
    }
}

impl FormulaLabPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn busy(&self) -> bool {
        self.task.is_some()
    }

    pub fn set_source(&mut self, source: impl Into<String>) {
        let source = source.into();
        if self.source != source {
            self.source = source;
            self.editor_cursor = self.source.chars().count();
            self.editor_selection = Some((self.editor_cursor, self.editor_cursor));
            self.pending_editor_cursor = Some(self.editor_cursor);
            self.refresh_compile();
        }
    }

    pub fn raw_time_index(&self) -> usize {
        self.raw_time_index
    }

    pub fn source_kind(&self) -> FormulaSourceKind {
        self.source_kind
    }

    pub fn set_source_kind(&mut self, source_kind: FormulaSourceKind) {
        if self.source_kind != source_kind {
            self.source_kind = source_kind;
            self.mark_editor_changed();
        }
    }

    pub fn note_result_discarded(&mut self, reason: &str) {
        self.status = Some(format!("Formula result discarded: {reason}"));
        // A host-side identity check is the final guard. Never leave metadata
        // from a result the viewer refused to install presented as successful.
        self.last_provenance = None;
        self.last_warnings.clear();
    }

    /// Versioned editor state only. Runtime workers, results, raw-file
    /// revisions, status text, and large-file consent are deliberately absent.
    pub fn state_json(&self) -> serde_json::Value {
        serde_json::to_value(FormulaLabPersistedState {
            schema: 1,
            source: self.source.clone(),
            output_name: self.output_name.clone(),
            recipe_name: self.recipe_name.clone(),
            recipe_version: self.recipe_version.clone(),
            recipe_description: self.recipe_description.clone(),
            expected_output_units: self.expected_output_units.clone(),
            authors: self.authors.clone(),
            references: self.references.clone(),
            tags: self.tags.clone(),
            requirements: self.requirements.clone(),
            resource_limits: self.resource_limits.clone(),
            parameter_specs: self.parameter_specs.clone(),
            parameter_values: self.parameter_values.clone(),
            evaluation_options: self.evaluation_options.clone(),
            unit_overrides_text: self.unit_overrides_text.clone(),
            source_kind: match self.source_kind {
                FormulaSourceKind::Store => "store",
                FormulaSourceKind::RawWrf => "raw_wrf",
            }
            .to_owned(),
            large_research_profile: self.large_research_profile,
            field_filter: self.field_filter.clone(),
        })
        .unwrap_or(serde_json::Value::Null)
    }

    pub fn apply_state_json(&mut self, value: &serde_json::Value) -> bool {
        let Ok(state) = serde_json::from_value::<FormulaLabPersistedState>(value.clone()) else {
            return false;
        };
        if state.schema != 1 {
            return false;
        }
        let source_kind = match state.source_kind.as_str() {
            "store" => FormulaSourceKind::Store,
            "raw_wrf" => FormulaSourceKind::RawWrf,
            _ => return false,
        };
        self.source = state.source;
        self.output_name = state.output_name;
        self.recipe_name = state.recipe_name;
        self.recipe_version = state.recipe_version;
        self.recipe_description = state.recipe_description;
        self.expected_output_units = state.expected_output_units;
        self.authors = state.authors;
        self.references = state.references;
        self.tags = state.tags;
        self.requirements = state.requirements;
        self.resource_limits = state.resource_limits.map(clamp_desktop_limits);
        self.parameter_specs = state.parameter_specs;
        self.parameter_values = state.parameter_values;
        self.evaluation_options = state.evaluation_options;
        self.unit_overrides_text = state.unit_overrides_text;
        self.source_kind = source_kind;
        self.large_research_profile = state.large_research_profile;
        self.field_filter = state.field_filter;
        self.editor_cursor = self.source.chars().count();
        self.editor_selection = Some((self.editor_cursor, self.editor_cursor));
        self.pending_editor_cursor = Some(self.editor_cursor);
        self.sync_parameter_values();
        self.refresh_compile();
        true
    }

    /// Synchronize source identity and poll an in-flight worker. The host calls
    /// this from its app-wide pump even when Formula Lab is not visible.
    pub fn poll(&mut self, sources: FormulaLabSources<'_>) -> Option<FormulaLabResult> {
        self.sync_sources(sources);
        self.poll_task(sources)
    }

    fn sync_sources(&mut self, sources: FormulaLabSources<'_>) {
        self.sync_raw_source(sources.raw_wrf);
    }

    /// Draw the reusable editor body in a host-owned dock tile or window.
    pub fn ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        self.sync_sources(sources);
        egui::ScrollArea::vertical()
            .id_salt("formula_lab_body_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.window_ui(ui, sources));
    }

    fn window_ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open recipe…").clicked() {
                self.load_recipe_dialog();
            }
            if ui.button("Save recipe…").clicked() {
                self.save_recipe_dialog();
            }
            ui.separator();
            ui.label(
                egui::RichText::new(match self.source_kind {
                    FormulaSourceKind::Store => "Stored model",
                    FormulaSourceKind::RawWrf => "Raw WRF",
                })
                .strong(),
            );
            match self.effective_source(sources) {
                Some(source) => {
                    ui.label(egui::RichText::new(source.label()).small());
                }
                None => {
                    ui.label(
                        egui::RichText::new("Select a store timestep or stage a raw WRF file")
                            .small()
                            .weak(),
                    );
                }
            }
        });

        self.capabilities_ui(ui, sources);

        if self.source_kind == FormulaSourceKind::RawWrf && sources.raw_wrf.is_some() {
            ui.horizontal(|ui| {
                ui.label("Raw WRF time index");
                if ui
                    .add(
                        egui::DragValue::new(&mut self.raw_time_index)
                            .range(0..=usize::MAX)
                            .speed(1.0),
                    )
                    .changed()
                {
                    self.mark_editor_changed();
                }
                ui.label(
                    egui::RichText::new(
                        "The worker validates this against the file's Times dimension.",
                    )
                    .small()
                    .weak(),
                );
            });
            if let Some(error) = &self.raw_source_error {
                ui.label(
                    egui::RichText::new(format!(
                        "Raw WRF source is not readable and evaluation is disabled: {error}"
                    ))
                    .small()
                    .color(egui::Color32::LIGHT_RED),
                );
            }
            if let Some(source) = sources.raw_wrf
                && self.raw_source_error.is_none()
                && let Some(revision) = &self.raw_revision
                && revision.len >= LARGE_RAW_WRF_BYTES
            {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Large raw-WRF formula evaluation").strong(),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "{} is {:.1} GB. A 3-D formula can retain several large f64 fields in addition to wrf-core's diagnostic cache.",
                            source.path.display(),
                            revision.len as f64 / 1.0e9
                        ))
                        .small(),
                    );
                    ui.checkbox(
                        &mut self.large_raw_confirmed,
                        "I understand the memory cost; allow evaluation",
                    );
                });
            }
        }

        self.starters_ui(ui, sources);

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("Output field");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.output_name)
                        .desired_width(170.0)
                        .hint_text("formula_result"),
                )
                .changed()
            {
                self.mark_editor_changed();
            }
            ui.label("Recipe");
            let name_changed = ui
                .add(
                    egui::TextEdit::singleline(&mut self.recipe_name)
                        .desired_width(170.0)
                        .hint_text("diagnostic_name"),
                )
                .changed();
            ui.label("Version");
            let version_changed = ui
                .add(egui::TextEdit::singleline(&mut self.recipe_version).desired_width(90.0))
                .changed();
            if name_changed || version_changed {
                self.refresh_compile();
            }
        });

        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(ui.available_width().clamp(190.0, 270.0));
                self.field_browser_ui(ui, sources);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Equation").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.set_source(String::new());
                            self.editor_cursor = 0;
                        }
                    });
                });
                let mut output = egui::TextEdit::multiline(&mut self.source)
                    .id_salt("formula_lab_equation")
                    .code_editor()
                    .desired_rows(11)
                    .desired_width(f32::INFINITY)
                    .hint_text("wind = grid_vector(u_10m, v_10m)\nmagnitude(wind)")
                    .show(ui);
                let response_changed = output.response.changed();
                if let Some(cursor) = self.pending_editor_cursor.take() {
                    let cursor = cursor.min(self.source.chars().count());
                    let range = egui::text::CCursorRange::one(egui::text::CCursor::new(cursor));
                    output.state.cursor.set_char_range(Some(range));
                    output.state.store(ui.ctx(), output.response.id);
                    output.response.request_focus();
                    self.editor_cursor = cursor;
                    self.editor_selection = Some((cursor, cursor));
                } else if let Some(range) = output.cursor_range {
                    let selected = range.as_sorted_char_range();
                    self.editor_cursor = range.primary.index;
                    self.editor_selection = Some((selected.start, selected.end));
                }
                if response_changed {
                    self.refresh_compile();
                }
                self.compile_status_ui(ui, sources);
            });
        });
        ui.separator();

        egui::CollapsingHeader::new("Parameters")
            .default_open(!self.parameter_specs.is_empty())
            .show(ui, |ui| self.parameters_ui(ui));
        egui::CollapsingHeader::new("Evaluation options")
            .default_open(false)
            .show(ui, |ui| self.options_ui(ui));
        egui::CollapsingHeader::new("Recipe metadata")
            .default_open(false)
            .show(ui, |ui| self.metadata_ui(ui));

        ui.separator();
        let output_name = normalized_output_name(&self.output_name);
        let evaluation_source = self.effective_source(sources);
        let can_run = self.compiled.is_some()
            && evaluation_source.is_some()
            && self.task.is_none()
            && output_name.is_ok()
            && self.source_readiness(sources).ready
            && !self.large_raw_needs_confirmation(sources)
            && sources.evaluation_blocked.is_none();
        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled(can_run, egui::Button::new("Evaluate and display"))
                .clicked();
            if clicked
                && let (Some(source), Ok(output_name)) =
                    (self.effective_source(sources), output_name)
            {
                self.start_evaluation(ui.ctx(), source, output_name);
            }
            if self.task.is_some() {
                ui.spinner();
                ui.label("evaluating in background");
            }
        });
        if let Err(error) = normalized_output_name(&self.output_name) {
            ui.label(
                egui::RichText::new(error)
                    .small()
                    .color(egui::Color32::LIGHT_RED),
            );
        }
        if let Some(reason) = sources.evaluation_blocked {
            ui.label(
                egui::RichText::new(format!("Evaluation is paused: {reason}"))
                    .small()
                    .color(egui::Color32::YELLOW),
            );
        }
        if !self.temporal_source_allowed(sources) {
            ui.label(
                egui::RichText::new(
                    "Temporal evaluation is disabled: this store source does not have a complete, host-verified exact-time axis",
                )
                .small()
                .color(egui::Color32::YELLOW),
            );
        }
        if let Some(status) = &self.status {
            ui.label(egui::RichText::new(status).small());
        }

        if !self.last_warnings.is_empty() {
            ui.separator();
            ui.label(egui::RichText::new("Last result warnings").strong());
            for warning in &self.last_warnings {
                ui.label(egui::RichText::new(format!("• {warning}")).small());
            }
        }
        if let Some(provenance) = &self.last_provenance {
            egui::CollapsingHeader::new("Last result provenance")
                .default_open(false)
                .show(ui, |ui| provenance_ui(ui, provenance));
        }
    }

    fn capabilities_ui(&self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        let text = match self.source_kind {
            FormulaSourceKind::Store => {
                let temporal = if sources
                    .store
                    .is_some_and(|source| source.temporal_axis_verified)
                {
                    "time derivatives ready"
                } else {
                    "time derivatives need at least two verified times"
                };
                format!(
                    "Stored model data: field algebra + pressure volumes + explicit-height vertical operators; {temporal}. Horizontal derivatives need Raw WRF grid metrics."
                )
            }
            FormulaSourceKind::RawWrf => "Raw WRF: full grid metrics, map factors, physical height, projected vectors, horizontal/vertical calculus, and multi-time dt when present.".to_owned(),
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Source capabilities").strong());
                ui.label(egui::RichText::new(text).small().weak());
            });
        });
    }

    fn starters_ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        let starters = self.starters(sources);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Quick starts").small().strong());
            for starter in starters {
                let enabled = starter.source.is_some();
                let response = ui
                    .add_enabled(enabled, egui::Button::new(starter.title))
                    .on_hover_text(&starter.description);
                if response.clicked()
                    && let Some(source) = starter.source
                {
                    self.source = source;
                    self.output_name = starter.output_name.to_owned();
                    self.recipe_name = starter.output_name.to_owned();
                    self.recipe_description = starter.description;
                    self.editor_cursor = self.source.chars().count();
                    self.editor_selection = Some((self.editor_cursor, self.editor_cursor));
                    self.pending_editor_cursor = Some(self.editor_cursor);
                    self.refresh_compile();
                }
            }
        });
    }

    fn starters(&self, sources: FormulaLabSources<'_>) -> Vec<FormulaStarter> {
        if self.source_kind == FormulaSourceKind::RawWrf {
            let available = sources.raw_wrf.is_some() && self.raw_source_error.is_none();
            let formula = |text: &str| available.then(|| text.to_owned());
            return vec![
                FormulaStarter {
                    title: "10 m wind",
                    description: "WRF U10/V10 wind-speed magnitude.".to_owned(),
                    source: formula("sqrt(U10^2 + V10^2)"),
                    output_name: "wind_speed_10m",
                },
                FormulaStarter {
                    title: "2 m temperature",
                    description: "Direct WRF T2 field, useful as a safe recipe starting point."
                        .to_owned(),
                    source: formula("T2"),
                    output_name: "temperature_2m",
                },
                FormulaStarter {
                    title: "10 m divergence",
                    description: "Grid-aware divergence; Raw WRF supplies map factors and spacing."
                        .to_owned(),
                    source: formula("div(grid_vector(U10, V10))"),
                    output_name: "divergence_10m",
                },
                FormulaStarter {
                    title: "10 m vorticity",
                    description: "Grid-aware vertical vorticity from the WRF 10 m wind.".to_owned(),
                    source: formula("curl(grid_vector(U10, V10))"),
                    output_name: "vorticity_10m",
                },
                FormulaStarter {
                    title: "T2 tendency",
                    description:
                        "Temporal derivative; requires adjacent Times in this raw WRF file."
                            .to_owned(),
                    source: formula("dt(T2)"),
                    output_name: "temperature_2m_tendency",
                },
            ];
        }

        let variables = sources
            .store
            .map(|source| source.variables.as_slice())
            .unwrap_or(&[]);
        let find = |aliases: &[&str]| find_inventory_name(variables, aliases);
        let u = find(&["u_10m", "u10"]);
        let v = find(&["v_10m", "v10"]);
        let temperature = find(&["temperature_2m", "t_2m", "t2"]);
        // Relative humidity is scientifically distinct and must never be used
        // as an implicit dewpoint substitute.
        let dewpoint = find(&["dewpoint_2m", "td_2m", "td2"]);
        let precip = find(&["apcp_run_total", "apcp", "precipitation_accumulation"]);
        let reflectivity = find(&["composite_reflectivity", "refc"]);
        let temporal = sources
            .store
            .is_some_and(|source| source.temporal_axis_verified);
        vec![
            FormulaStarter {
                title: "10 m wind",
                description: missing_pair_description(
                    "Portable speed magnitude using the exact U/V tokens in this run.",
                    u.as_deref(),
                    v.as_deref(),
                    "u_10m/U10",
                    "v_10m/V10",
                ),
                source: u
                    .as_ref()
                    .zip(v.as_ref())
                    .map(|(u, v)| format!("sqrt({u}^2 + {v}^2)")),
                output_name: "wind_speed_10m",
            },
            FormulaStarter {
                title: "Dewpoint spread",
                description: missing_pair_description(
                    "2 m temperature minus actual 2 m dewpoint.",
                    temperature.as_deref(),
                    dewpoint.as_deref(),
                    "temperature_2m",
                    "dewpoint_2m (RH is not substituted)",
                ),
                source: temperature
                    .as_ref()
                    .zip(dewpoint.as_ref())
                    .map(|(temperature, dewpoint)| format!("{temperature} - {dewpoint}")),
                output_name: "dewpoint_depression_2m",
            },
            FormulaStarter {
                title: "2 m temperature",
                description: starter_field_description(
                    "Direct 2 m temperature field.",
                    temperature.as_deref(),
                    "temperature_2m/T2",
                ),
                source: temperature.clone(),
                output_name: "temperature_2m",
            },
            FormulaStarter {
                title: "Run precip field",
                description: starter_field_description(
                    "Direct run accumulation field. Confirm the selected token's accumulation window in the model metadata; APCP and apcp_run_total are not treated as scientifically interchangeable.",
                    precip.as_deref(),
                    "apcp_run_total/APCP",
                ),
                source: precip,
                output_name: "accumulated_precipitation",
            },
            FormulaStarter {
                title: "Composite reflectivity",
                description: starter_field_description(
                    "Direct composite reflectivity field.",
                    reflectivity.as_deref(),
                    "composite_reflectivity/REFC",
                ),
                source: reflectivity,
                output_name: "composite_reflectivity",
            },
            FormulaStarter {
                title: "Temperature tendency",
                description: if !temporal {
                    "Needs 2 m temperature and at least two distinct verified run times.".to_owned()
                } else {
                    starter_field_description(
                        "Temporal derivative across the selected run's verified time axis.",
                        temperature.as_deref(),
                        "temperature_2m/T2",
                    )
                },
                source: temporal
                    .then_some(temperature)
                    .flatten()
                    .map(|temperature| format!("dt({temperature})")),
                output_name: "temperature_2m_tendency",
            },
        ]
    }

    fn field_browser_ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        ui.label(egui::RichText::new("Fields").strong());
        ui.label(
            egui::RichText::new("Click a field to insert its exact token at the editor cursor.")
                .small()
                .weak(),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.field_filter)
                .hint_text("Search name or units")
                .desired_width(f32::INFINITY),
        );
        let query = self.field_filter.trim().to_ascii_lowercase();
        let fields: Vec<VarInfo> = match self.source_kind {
            FormulaSourceKind::Store => sources
                .store
                .map(|source| source.variables.clone())
                .unwrap_or_default(),
            FormulaSourceKind::RawWrf => raw_wrf_common_fields(),
        };
        let filtered = fields.into_iter().filter(|field| {
            query.is_empty()
                || field.name.to_ascii_lowercase().contains(&query)
                || field.units.to_ascii_lowercase().contains(&query)
        });
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("formula_lab_fields")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for field in filtered.take(300) {
                    let kind = match field.kind {
                        VarKind::Surface2D => "2-D",
                        VarKind::Pressure3D if self.source_kind == FormulaSourceKind::RawWrf => {
                            "3-D WRF/native"
                        }
                        VarKind::Pressure3D => "3-D pressure",
                    };
                    let label = if field.units.trim().is_empty() {
                        field.name.clone()
                    } else {
                        format!("{}  [{}]", field.name, field.units)
                    };
                    let raw_details = (self.source_kind == FormulaSourceKind::RawWrf)
                        .then(|| wrf_core::variables::get_var_def(&field.name))
                        .flatten()
                        .map(|definition| {
                            let aliases = if definition.aliases.is_empty() {
                                String::new()
                            } else {
                                format!(" · aliases: {}", definition.aliases.join(", "))
                            };
                            format!(" · {}{aliases}", definition.description)
                        })
                        .unwrap_or_default();
                    if ui
                        .button(label)
                        .on_hover_text(format!(
                            "{kind}{}{raw_details}",
                            if field.levels_hpa.is_empty() {
                                String::new()
                            } else {
                                format!(" · {} levels", field.levels_hpa.len())
                            }
                        ))
                        .clicked()
                    {
                        clicked = Some(field.name);
                    }
                }
            });
        if let Some(field) = clicked {
            self.insert_field_token(&field);
        }
        if self.source_kind == FormulaSourceKind::Store
            && sources
                .store
                .is_some_and(|source| source.variables.is_empty())
        {
            ui.label(
                egui::RichText::new("Loading the selected timestep's field inventory…")
                    .small()
                    .weak(),
            );
        }
        if self.source_kind == FormulaSourceKind::RawWrf {
            ui.label(
                egui::RichText::new(
                    "Common WRF tokens are listed; the file resolver validates availability when you evaluate.",
                )
                .small()
                .weak(),
            );
        }
    }

    fn insert_field_token(&mut self, field: &str) {
        let char_count = self.source.chars().count();
        let (selection_start, selection_end) = self
            .editor_selection
            .unwrap_or((self.editor_cursor, self.editor_cursor));
        let start = selection_start.min(selection_end).min(char_count);
        let end = selection_start.max(selection_end).min(char_count);
        let start_byte = char_to_byte_index(&self.source, start);
        let end_byte = char_to_byte_index(&self.source, end);
        self.source.replace_range(start_byte..end_byte, field);
        self.editor_cursor = start + field.chars().count();
        self.editor_selection = Some((self.editor_cursor, self.editor_cursor));
        self.pending_editor_cursor = Some(self.editor_cursor);
        self.refresh_compile();
    }

    fn source_readiness(&self, sources: FormulaLabSources<'_>) -> SourceReadiness {
        let mut readiness = SourceReadiness::default();
        let Some(compiled) = &self.compiled else {
            readiness
                .blockers
                .push("Fix the equation syntax first.".to_owned());
            return readiness;
        };
        if compiled.plan().dependencies.is_empty() {
            readiness.blockers.push(
                "The result is scalar; include at least one grid field so Formula Lab can produce a displayable [Y, X] field."
                    .to_owned(),
            );
        }
        let has_vertical_output_reducer = compiled.plan().functions.iter().any(|function| {
            matches!(
                function.as_str(),
                "mean_z" | "integrate_z" | "interpolate_z"
            )
        });
        match self.source_kind {
            FormulaSourceKind::RawWrf => {
                if sources.raw_wrf.is_none() {
                    readiness
                        .blockers
                        .push("Choose a raw WRF file for this source.".to_owned());
                } else if let Some(error) = &self.raw_source_error {
                    readiness
                        .blockers
                        .push(format!("Raw WRF source is unreadable: {error}"));
                } else {
                    readiness.notes.push(
                        "Raw WRF field names and time count are validated by the file resolver at evaluation."
                            .to_owned(),
                    );
                }
                let mut known_three_dimensional = Vec::new();
                let raw_inventory = raw_wrf_common_fields();
                for dependency in &compiled.plan().dependencies {
                    if let Some(definition) = wrf_core::variables::get_var_def(dependency) {
                        if PACKED_WRF_FORMULA_FIELDS.contains(&definition.name) {
                            readiness.blockers.push(format!(
                                "Raw WRF field '{}' is a packed component product and cannot be used directly; choose component-specific fields.",
                                dependency
                            ));
                        } else if definition.dim == wrf_core::variables::VarDim::ThreeD {
                            known_three_dimensional.push(dependency.clone());
                        }
                    } else if raw_inventory.iter().any(|field| {
                        field.name.eq_ignore_ascii_case(dependency)
                            && field.kind == VarKind::Pressure3D
                    }) {
                        known_three_dimensional.push(dependency.clone());
                    }
                }
                if !known_three_dimensional.is_empty() && !has_vertical_output_reducer {
                    readiness.blockers.push(format!(
                        "Known 3-D raw WRF input(s) {} need mean_z, integrate_z, or interpolate_z before the result can display as 2-D.",
                        known_three_dimensional.join(", ")
                    ));
                }
                if let Some(requirements) = &compiled.plan().recipe_requirements
                    && (requirements.maximum_cadence_seconds.is_some()
                        || requirements.maximum_horizontal_spacing_m.is_some()
                        || requirements.minimum_vertical_levels.is_some())
                {
                    readiness.notes.push(
                        "Recipe cadence, grid-spacing, and vertical-level requirements are validated from the raw WRF file at evaluation."
                            .to_owned(),
                    );
                }
            }
            FormulaSourceKind::Store => {
                let Some(source) = sources.store else {
                    readiness
                        .blockers
                        .push("Select a model, run, and time from the store.".to_owned());
                    return readiness;
                };
                if source.variables.is_empty() {
                    readiness
                        .blockers
                        .push("Waiting for this timestep's field inventory.".to_owned());
                }
                let overrides = parse_unit_overrides(&self.unit_overrides_text).unwrap_or_default();
                let mut dependencies = compiled.plan().dependencies.clone();
                if let Some(requirements) = &compiled.plan().recipe_requirements {
                    dependencies.extend(requirements.fields.iter().cloned());
                }
                dependencies.sort_by_key(|name| name.to_ascii_lowercase());
                dependencies.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
                let mut dependency_fields = Vec::new();
                for dependency in dependencies {
                    let Some(field) = source
                        .variables
                        .iter()
                        .find(|field| field.name.eq_ignore_ascii_case(&dependency))
                    else {
                        readiness.blockers.push(format!(
                            "Missing field '{dependency}' in {}/{} {}.",
                            source.hour.model,
                            source.hour.run,
                            source.hour.time_label()
                        ));
                        continue;
                    };
                    dependency_fields.push(field);
                    let overridden = overrides
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case(&field.name));
                    if overridden
                        && !field.units.trim().is_empty()
                        && wrf_formula::parse_unit(&field.units).is_ok()
                    {
                        readiness.blockers.push(format!(
                            "Field '{}' now has recognized stored units '{}'; remove its stale unit override before evaluating.",
                            field.name, field.units
                        ));
                    } else if !overridden && field.units.trim().is_empty() {
                        readiness.blockers.push(format!(
                            "Field '{}' has no stored unit metadata. Add an explicit, scientifically verified unit override.",
                            field.name
                        ));
                    } else if !overridden && wrf_formula::parse_unit(&field.units).is_err() {
                        if let Some(formula_units) = safe_unit_override(&field.units) {
                            readiness.blockers.push(format!(
                                "Field '{}' uses unsupported stored units '{}'. Review and apply the conservative override offered below.",
                                field.name, field.units
                            ));
                            readiness.override_suggestions.push(UnitOverrideSuggestion {
                                field: field.name.clone(),
                                stored_units: field.units.clone(),
                                formula_units,
                            });
                        } else {
                            readiness.blockers.push(format!(
                                "Field '{}' uses unsupported stored units '{}'. This needs a scale-aware scientific conversion; no automatic relabeling override is offered.",
                                field.name, field.units
                            ));
                        }
                    }
                }
                let pressure_fields = dependency_fields
                    .iter()
                    .copied()
                    .filter(|field| field.kind == VarKind::Pressure3D)
                    .collect::<Vec<_>>();
                if pressure_fields.len() > 1 {
                    let reference = &pressure_fields[0].levels_hpa;
                    if pressure_fields
                        .iter()
                        .skip(1)
                        .any(|field| &field.levels_hpa != reference)
                    {
                        readiness.blockers.push(format!(
                            "Pressure-volume inputs use different level axes ({}); store formulas require identical levels.",
                            pressure_fields
                                .iter()
                                .map(|field| format!("{}:{}", field.name, field.levels_hpa.len()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
                if !pressure_fields.is_empty() && !has_vertical_output_reducer {
                    readiness.blockers.push(format!(
                        "3-D pressure input(s) {} need mean_z, integrate_z, or interpolate_z before the result can display as 2-D.",
                        pressure_fields
                            .iter()
                            .map(|field| field.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                for requirement in &compiled.plan().requirements {
                    match requirement {
                        Requirement::MassMapFactor => readiness.blockers.push(
                            "Horizontal derivatives need grid spacing/map factors that rw-store does not persist; choose Raw WRF."
                                .to_owned(),
                        ),
                        Requirement::PhysicalHeight {
                            datum: HeightDatum::ResolverDefault,
                        } => readiness.blockers.push(
                            "ddz(field) needs the resolver's default physical-height coordinate; use ddz(field, height_field) or choose Raw WRF."
                                .to_owned(),
                        ),
                        Requirement::AdjacentTimes if !source.temporal_axis_verified => readiness
                            .blockers
                            .push("dt() needs at least two distinct, increasing verified run times."
                                .to_owned()),
                        _ => {}
                    }
                }
                if let Some(requirements) = &compiled.plan().recipe_requirements {
                    if let Some(maximum_cadence) = requirements.maximum_cadence_seconds {
                        match selected_axis_neighbor_interval_seconds(
                            &source.exact_times,
                            source.hour.hour,
                        ) {
                            Some(interval) if source.temporal_axis_verified => {
                                if interval > maximum_cadence {
                                    readiness.blockers.push(format!(
                                        "Recipe requires cadence ≤ {maximum_cadence:.0} s, but the selected output's largest neighbor interval is {interval:.0} s."
                                    ));
                                }
                            }
                            _ => readiness.blockers.push(format!(
                                "Recipe requires cadence ≤ {maximum_cadence:.0} s, but this store source lacks two verified increasing times."
                            )),
                        }
                    }
                    if let Some(maximum_spacing) = requirements.maximum_horizontal_spacing_m {
                        readiness.blockers.push(format!(
                            "Recipe requires horizontal spacing ≤ {maximum_spacing:.0} m, but rw-store does not persist resolver dx/dy; choose Raw WRF."
                        ));
                    }
                    if let Some(minimum_levels) = requirements.minimum_vertical_levels {
                        readiness.blockers.push(format!(
                            "Recipe requires at least {minimum_levels} vertical levels, but the store resolver cannot expose a run-wide nz; choose Raw WRF."
                        ));
                    }
                }
                readiness.notes.push(format!(
                    "{} fields inventoried for {} / {} / {}.",
                    source.variables.len(),
                    source.hour.model,
                    source.hour.run,
                    source.hour.time_label()
                ));
            }
        }
        readiness.ready = readiness.blockers.is_empty();
        readiness
    }

    fn append_unit_override(&mut self, suggestion: &UnitOverrideSuggestion) {
        if self
            .unit_overrides_text
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
            .any(|name| name.eq_ignore_ascii_case(&suggestion.field))
        {
            return;
        }
        if !self.unit_overrides_text.trim().is_empty() {
            self.unit_overrides_text.push('\n');
        }
        self.unit_overrides_text.push_str(&format!(
            "{} = {}",
            suggestion.field, suggestion.formula_units
        ));
        self.refresh_compile();
    }

    fn compile_status_ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        if let Some(error) = &self.compile_error {
            ui.label(
                egui::RichText::new(format!("{:?}: {}", error.kind, error.message))
                    .color(egui::Color32::LIGHT_RED),
            );
            if let Some(span) = error.span {
                let excerpt = span_excerpt(&self.source, span);
                ui.label(
                    egui::RichText::new(format!(
                        "source bytes {}..{}{}",
                        span.start,
                        span.end,
                        excerpt
                            .as_deref()
                            .map(|text| format!(" · {text:?}"))
                            .unwrap_or_default()
                    ))
                    .small()
                    .monospace(),
                );
            }
            for note in &error.notes {
                ui.label(egui::RichText::new(format!("• {note}")).small());
            }
            return;
        }

        let Some(compiled) = &self.compiled else {
            ui.label(egui::RichText::new("Formula has not compiled").weak());
            return;
        };
        let readiness = self.source_readiness(sources);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("✓ Syntax valid").color(egui::Color32::LIGHT_GREEN));
            ui.separator();
            if readiness.ready {
                ui.label(
                    egui::RichText::new("✓ Ready for selected source")
                        .strong()
                        .color(egui::Color32::LIGHT_GREEN),
                );
            } else {
                ui.label(
                    egui::RichText::new("Source not ready")
                        .strong()
                        .color(egui::Color32::YELLOW),
                );
            }
        });
        for blocker in &readiness.blockers {
            ui.label(
                egui::RichText::new(format!("• {blocker}"))
                    .small()
                    .color(egui::Color32::YELLOW),
            );
        }
        for note in &readiness.notes {
            ui.label(egui::RichText::new(note).small().weak());
        }
        let plan = compiled.plan();
        ui.label(
            egui::RichText::new(format!(
                "Dependencies: {}",
                if plan.dependencies.is_empty() {
                    "none".to_string()
                } else {
                    plan.dependencies.join(", ")
                }
            ))
            .small(),
        );
        if !plan.functions.is_empty() {
            ui.label(
                egui::RichText::new(format!("Functions: {}", plan.functions.join(", "))).small(),
            );
        }
        if !plan.requirements.is_empty() {
            ui.label(egui::RichText::new("Requirements:").small().strong());
            for requirement in &plan.requirements {
                ui.label(
                    egui::RichText::new(format!("• {}", requirement_text(requirement))).small(),
                );
            }
        }
        if let Some(requirements) = &plan.recipe_requirements {
            if !requirements.fields.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "Recipe-required fields: {}",
                        requirements.fields.join(", ")
                    ))
                    .small(),
                );
            }
            for note in &requirements.notes {
                ui.label(egui::RichText::new(format!("• {note}")).small());
            }
        }
        ui.label(
            egui::RichText::new(format!(
                "Bounded syntax: {} AST nodes, depth {}",
                plan.ast_nodes, plan.ast_depth
            ))
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new(
                "Concrete output units and shape are verified against the selected dataset at evaluation time.",
            )
            .small()
            .weak(),
        );
        let mut apply_override = None;
        for suggestion in &readiness.override_suggestions {
            if ui
                .small_button(format!(
                    "Use {} = {} (stored {})",
                    suggestion.field, suggestion.formula_units, suggestion.stored_units
                ))
                .on_hover_text(
                    "Add this explicit, conservative unit interpretation to Evaluation options.",
                )
                .clicked()
            {
                apply_override = Some(suggestion.clone());
            }
        }
        if let Some(suggestion) = apply_override {
            self.append_unit_override(&suggestion);
        }
    }

    fn parameters_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut remove = None;
        for (index, spec) in self.parameter_specs.iter_mut().enumerate() {
            ui.group(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{}.", index + 1));
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut spec.name)
                                .desired_width(120.0)
                                .hint_text("parameter"),
                        )
                        .changed();
                    ui.label("units");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut spec.units)
                                .desired_width(100.0)
                                .hint_text("1"),
                        )
                        .changed();
                    ui.label("default");
                    changed |= ui
                        .add(egui::DragValue::new(&mut spec.default).speed(0.1))
                        .changed();
                    if ui.small_button("Remove").clicked() {
                        remove = Some(index);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    changed |= optional_bound_ui(ui, "min", &mut spec.minimum, spec.default);
                    changed |= optional_bound_ui(ui, "max", &mut spec.maximum, spec.default);
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut spec.description)
                                .desired_width(300.0)
                                .hint_text("description"),
                        )
                        .changed();
                });
            });
        }
        if let Some(index) = remove {
            self.parameter_specs.remove(index);
            changed = true;
        }
        if ui.button("Add parameter").clicked() {
            let suffix = self.parameter_specs.len() + 1;
            self.parameter_specs.push(ParameterSpec {
                name: format!("parameter_{suffix}"),
                units: "1".to_string(),
                default: 1.0,
                minimum: None,
                maximum: None,
                description: String::new(),
            });
            changed = true;
        }
        if changed {
            self.sync_parameter_values();
            self.refresh_compile();
        }

        self.sync_parameter_values();
        if !self.parameter_specs.is_empty() {
            ui.separator();
            ui.label("Evaluation values");
        }
        for spec in &self.parameter_specs {
            if let Some(value) = self.parameter_values.get_mut(&spec.name) {
                ui.horizontal(|ui| {
                    ui.label(&spec.name);
                    let mut drag = egui::DragValue::new(value).speed(0.1);
                    let minimum = spec.minimum.unwrap_or(f64::NEG_INFINITY);
                    let maximum = spec.maximum.unwrap_or(f64::INFINITY);
                    if minimum <= maximum {
                        drag = drag.range(minimum..=maximum);
                    }
                    if ui.add(drag).changed() {
                        changed = true;
                    }
                    ui.label(egui::RichText::new(&spec.units).small().weak());
                });
            }
        }
        if changed {
            self.mark_editor_changed();
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        changed |= ui
            .checkbox(
                &mut self.large_research_profile,
                "Large research memory profile (up to 128M elements / 4 GiB cumulative)",
            )
            .on_hover_text(
                "Off: 64M elements, 512 MiB per allocation, 2 GiB cumulative, 1B operations. This still admits an 800x800x79 f64 volume. Enable only for equations that fail the standard meter.",
            )
            .changed();
        ui.horizontal_wrapped(|ui| {
            ui.label("Boundary");
            egui::ComboBox::from_id_salt("formula_boundary_policy")
                .selected_text(boundary_text(self.evaluation_options.boundary_policy))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.boundary_policy,
                            BoundaryPolicy::OneSidedSecondOrder,
                            "one-sided second order",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.boundary_policy,
                            BoundaryPolicy::Missing,
                            "missing",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.boundary_policy,
                            BoundaryPolicy::Error,
                            "error",
                        )
                        .changed();
                });
            ui.label("Missing");
            egui::ComboBox::from_id_salt("formula_missing_policy")
                .selected_text(missing_text(self.evaluation_options.missing_policy))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.missing_policy,
                            MissingPolicy::Propagate,
                            "propagate",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.missing_policy,
                            MissingPolicy::Error,
                            "error",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.missing_policy,
                            MissingPolicy::IgnoreInReductions,
                            "ignore in reductions",
                        )
                        .changed();
                });
            ui.label("Non-finite");
            egui::ComboBox::from_id_salt("formula_nonfinite_policy")
                .selected_text(nonfinite_text(self.evaluation_options.non_finite_policy))
                .show_ui(ui, |ui| {
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.non_finite_policy,
                            NonFinitePolicy::Propagate,
                            "propagate",
                        )
                        .changed();
                    changed |= ui
                        .selectable_value(
                            &mut self.evaluation_options.non_finite_policy,
                            NonFinitePolicy::Error,
                            "error",
                        )
                        .changed();
                });
        });
        ui.label("Raw-field unit overrides (one NAME = unit per line)");
        changed |= ui
            .add(
                egui::TextEdit::multiline(&mut self.unit_overrides_text)
                    .code_editor()
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            )
            .changed();
        if changed {
            self.refresh_compile();
        }
    }

    fn metadata_ui(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.label("Description");
        changed |= ui
            .add(
                egui::TextEdit::multiline(&mut self.recipe_description)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            )
            .changed();
        ui.horizontal(|ui| {
            ui.label("Expected output units");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.expected_output_units)
                        .desired_width(160.0)
                        .hint_text("optional"),
                )
                .changed();
        });
        if !self.requirements.fields.is_empty() {
            ui.label(format!(
                "Required fields: {}",
                self.requirements.fields.join(", ")
            ));
        }
        if let Some(seconds) = self.requirements.maximum_cadence_seconds {
            ui.label(format!("Maximum cadence: {seconds} s"));
        }
        if let Some(spacing) = self.requirements.maximum_horizontal_spacing_m {
            ui.label(format!("Maximum horizontal spacing: {spacing} m"));
        }
        if let Some(levels) = self.requirements.minimum_vertical_levels {
            ui.label(format!("Minimum vertical levels: {levels}"));
        }
        for note in &self.requirements.notes {
            ui.label(egui::RichText::new(format!("• {note}")).small());
        }
        if changed {
            self.refresh_compile();
        }
    }

    fn refresh_compile(&mut self) {
        self.mark_editor_changed();
        self.sync_parameter_values();
        let result = self.build_recipe().and_then(|recipe| recipe.compile());
        match result {
            Ok(compiled) => {
                self.compiled = Some(compiled);
                self.compile_error = None;
            }
            Err(error) => {
                self.compiled = None;
                self.compile_error = Some(error);
            }
        }
    }

    fn mark_editor_changed(&mut self) {
        self.editor_generation = self.editor_generation.wrapping_add(1);
        if self.task.is_some() {
            self.status =
                Some("Formula inputs changed; the running result will be discarded".to_string());
        }
    }

    fn sync_raw_source(&mut self, source: Option<&RawWrfFormulaSource>) {
        let Some(source) = source else {
            if self.raw_path.take().is_some()
                || self.raw_revision.take().is_some()
                || self.raw_source_error.take().is_some()
            {
                self.large_raw_confirmed = false;
                self.mark_editor_changed();
            }
            return;
        };

        let path_changed = self.raw_path.as_ref() != Some(&source.path);
        let inspected = inspect_raw_file_revision(&source.path);
        let (revision, error) = match inspected {
            Ok(revision) => (Some(revision), None),
            Err(error) => (None, Some(error)),
        };
        let revision_changed = self.raw_revision != revision || self.raw_source_error != error;
        if path_changed || revision_changed {
            self.raw_path = Some(source.path.clone());
            self.raw_revision = revision;
            self.raw_source_error = error;
            self.raw_time_index = source.initial_time_index;
            // Consent applies to one concrete file revision, never merely to a
            // pathname that another process may replace or continue writing.
            self.large_raw_confirmed = false;
            self.mark_editor_changed();
        }
    }

    fn large_raw_needs_confirmation(&self, sources: FormulaLabSources<'_>) -> bool {
        if self.source_kind != FormulaSourceKind::RawWrf || self.large_raw_confirmed {
            return false;
        }
        if sources.raw_wrf.is_none() || self.raw_source_error.is_some() {
            return true;
        }
        self.raw_revision
            .as_ref()
            .is_some_and(|revision| revision.len >= LARGE_RAW_WRF_BYTES)
    }

    fn effective_source(&self, sources: FormulaLabSources<'_>) -> Option<EvaluationSource> {
        match self.source_kind {
            FormulaSourceKind::Store => sources.store.cloned().map(EvaluationSource::Store),
            FormulaSourceKind::RawWrf => self.raw_evaluation_source(sources.raw_wrf),
        }
    }

    fn temporal_source_allowed(&self, sources: FormulaLabSources<'_>) -> bool {
        let needs_adjacent_times = self.compiled.as_ref().is_some_and(|compiled| {
            compiled
                .plan()
                .requirements
                .iter()
                .any(|requirement| matches!(requirement, Requirement::AdjacentTimes))
        });
        if !needs_adjacent_times || self.source_kind != FormulaSourceKind::Store {
            return true;
        }
        sources
            .store
            .is_some_and(|source| source.temporal_axis_verified)
    }

    fn raw_evaluation_source(
        &self,
        source: Option<&RawWrfFormulaSource>,
    ) -> Option<EvaluationSource> {
        let source = source?;
        if self.raw_path.as_ref() != Some(&source.path)
            || self.raw_revision.is_none()
            || self.raw_source_error.is_some()
        {
            return None;
        }
        let mut display_hour = source.display_hour.clone();
        display_hour.hour = u16::try_from(self.raw_time_index).unwrap_or(u16::MAX);
        Some(EvaluationSource::RawWrf {
            path: source.path.clone(),
            time_index: self.raw_time_index,
            display_hour,
            revision: self.raw_revision.clone()?,
        })
    }

    fn build_recipe(&self) -> Result<Recipe, FormulaError> {
        let mut evaluation_options = self.evaluation_options.clone();
        evaluation_options.variable_unit_overrides =
            parse_unit_overrides(&self.unit_overrides_text)?;
        Ok(Recipe {
            schema: "wrf-formula/v1".to_string(),
            name: self.recipe_name.clone(),
            version: self.recipe_version.clone(),
            description: self.recipe_description.clone(),
            authors: self.authors.clone(),
            references: self.references.clone(),
            tags: self.tags.clone(),
            source: self.source.clone(),
            parameters: self.parameter_specs.clone(),
            expected_output_units: (!self.expected_output_units.trim().is_empty())
                .then(|| self.expected_output_units.trim().to_string()),
            requirements: self.requirements.clone(),
            evaluation_options,
            resource_limits: Some(self.effective_resource_limits()),
        })
    }

    fn effective_resource_limits(&self) -> ResourceLimits {
        let ceiling = if self.large_research_profile {
            ResourceLimits::default()
        } else {
            desktop_standard_limits()
        };
        let requested = self
            .resource_limits
            .clone()
            .unwrap_or_else(|| ceiling.clone());
        clamp_limits_to(requested, &ceiling)
    }

    fn sync_parameter_values(&mut self) {
        let names = self
            .parameter_specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<BTreeSet<_>>();
        self.parameter_values.retain(|name, _| names.contains(name));
        for spec in &self.parameter_specs {
            self.parameter_values
                .entry(spec.name.clone())
                .or_insert(spec.default);
        }
    }

    fn start_evaluation(
        &mut self,
        ctx: &egui::Context,
        source: EvaluationSource,
        output_name: String,
    ) {
        let Some(compiled) = self.compiled.clone() else {
            self.status = Some("Formula must compile before evaluation".to_string());
            return;
        };
        if matches!(&source, EvaluationSource::Store(source) if !source.temporal_axis_verified)
            && compiled
                .plan()
                .requirements
                .iter()
                .any(|requirement| matches!(requirement, Requirement::AdjacentTimes))
        {
            self.status = Some(
                "Temporal formula requires a complete, host-verified exact-time axis".to_string(),
            );
            return;
        }
        let parameters = self.parameter_values.clone();
        let mut options = self.evaluation_options.clone();
        match parse_unit_overrides(&self.unit_overrides_text) {
            Ok(overrides) => options.variable_unit_overrides = overrides,
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        }
        let display_hour = source.display_hour().clone();
        let source_identity = source.result_source();
        let resource_limits = self.effective_resource_limits();
        let store_revision_source = match &source {
            EvaluationSource::Store(source) => Some(source.clone()),
            EvaluationSource::RawWrf { .. } => None,
        };
        let raw_revision_source = match &source {
            EvaluationSource::RawWrf { path, revision, .. } => {
                let current = match inspect_raw_file_revision(path) {
                    Ok(current) => current,
                    Err(error) => {
                        self.status = Some(format!(
                            "Could not capture a stable Formula Lab raw source: {error}"
                        ));
                        return;
                    }
                };
                if &current != revision {
                    self.status = Some(
                        "Raw WRF file changed immediately before Formula Lab evaluation; retry after the writer finishes"
                            .to_string(),
                    );
                    return;
                }
                Some((path.clone(), current))
            }
            EvaluationSource::Store(_) => None,
        };
        let raw_revision = raw_revision_source
            .as_ref()
            .map(|(_, revision)| revision.clone());
        let worker_raw_revision = raw_revision.clone();
        let generation = self.editor_generation;
        let (tx, rx) = channel();
        let repaint = ctx.clone();
        self.status = Some(format!("Evaluating {}", source.label()));
        let spawn = std::thread::Builder::new()
            .name("rw-formula-lab".to_string())
            .spawn(move || {
                rw_ingest::throttle::set_current_thread_background_priority();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // A minute-cadence run may contain thousands of files. Keep
                    // both run-wide revision walks on the background worker so
                    // clicking Evaluate and accepting its result never blocks
                    // the egui thread on O(timesteps) filesystem calls.
                    let store_revision_before = store_revision_source
                        .as_ref()
                        .map(inspect_store_run_revision)
                        .transpose()
                        .map_err(BridgeError::Store)?;
                    let evaluated = evaluate_source(
                        source,
                        display_hour,
                        output_name,
                        &compiled,
                        &parameters,
                        &options,
                        &resource_limits,
                    )?;
                    if let (Some(source), Some(before)) =
                        (store_revision_source.as_ref(), store_revision_before.as_ref())
                    {
                        let after = inspect_store_run_revision(source).map_err(BridgeError::Store)?;
                        if &after != before {
                            return Err(BridgeError::Store(
                                "rw-store run changed while Formula Lab evaluated it; result discarded"
                                    .to_string(),
                            ));
                        }
                    }
                    if let (Some((path, _)), Some(before)) =
                        (raw_revision_source.as_ref(), worker_raw_revision.as_ref())
                    {
                        let after = inspect_raw_file_revision(path).map_err(BridgeError::Wrf)?;
                        if &after != before {
                            return Err(BridgeError::Wrf(
                                "raw WRF file changed while Formula Lab evaluated it; result discarded"
                                    .to_string(),
                            ));
                        }
                    }
                    Ok(evaluated)
                }))
                .map_err(panic_message)
                .and_then(|result| result.map_err(|error| error.to_string()));
                let _ = tx.send(result);
                repaint.request_repaint();
        });
        match spawn {
            Ok(_) => {
                self.task = Some(EvaluationTask {
                    rx,
                    generation,
                    source: source_identity,
                    raw_revision,
                })
            }
            Err(error) => {
                self.status = Some(format!("Could not start Formula Lab worker: {error}"));
            }
        }
    }

    fn poll_task(&mut self, sources: FormulaLabSources<'_>) -> Option<FormulaLabResult> {
        let task = self.task.take()?;
        match task.rx.try_recv() {
            Ok(Ok(result)) => {
                if self.task_is_stale(&task, sources) {
                    self.status = Some(
                        "Formula result discarded because its equation, options, parameters, output, or data source changed while it ran"
                            .to_string(),
                    );
                    return None;
                }
                if result.source != task.source {
                    self.status = Some(
                        "Formula result discarded because the worker returned an unexpected source identity"
                            .to_string(),
                    );
                    return None;
                }
                self.status = Some(format!(
                    "Generated {} ({}×{}, {}) · {}",
                    result.field.key.var,
                    result.field.nx,
                    result.field.ny,
                    result.field.units,
                    result.description
                ));
                self.last_provenance = Some(result.provenance.clone());
                self.last_warnings = result.warnings.clone();
                Some(result)
            }
            Ok(Err(error)) => {
                if self.task_is_stale(&task, sources) {
                    self.status = Some(
                        "Obsolete Formula Lab evaluation stopped after its inputs changed"
                            .to_string(),
                    );
                } else {
                    self.status = Some(format!("Formula evaluation failed: {error}"));
                }
                None
            }
            Err(TryRecvError::Empty) => {
                self.task = Some(task);
                None
            }
            Err(TryRecvError::Disconnected) => {
                if self.task_is_stale(&task, sources) {
                    self.status = Some(
                        "Obsolete Formula Lab worker stopped after its inputs changed".to_string(),
                    );
                } else {
                    self.status = Some("Formula Lab worker stopped unexpectedly".to_string());
                }
                None
            }
        }
    }

    fn task_is_stale(&self, task: &EvaluationTask, sources: FormulaLabSources<'_>) -> bool {
        task.generation != self.editor_generation
            || self
                .effective_source(sources)
                .map(|source| source.result_source())
                .as_ref()
                != Some(&task.source)
            || match (&task.raw_revision, &task.source) {
                // Store stability is verified by two complete revision walks
                // inside the worker. Source selection identity was compared
                // immediately above, so no run-wide scan belongs on this UI
                // polling path.
                (None, FormulaResultSource::Store { .. }) => false,
                (Some(expected), FormulaResultSource::RawWrf { path, .. }) => {
                    inspect_raw_file_revision(path)
                        .map(|current| &current != expected)
                        .unwrap_or(true)
                }
                // Missing or cross-wired revisions violate the launch
                // invariant and must never land.
                _ => true,
            }
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn load_recipe_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WRF Formula Recipe", &["json"])
            .pick_file()
        else {
            return;
        };
        let result = load_recipe_bounded(&path);
        match result {
            Ok(recipe) => {
                let limits_clamped = recipe
                    .resource_limits
                    .as_ref()
                    .is_some_and(|limits| clamp_desktop_limits(limits.clone()) != *limits);
                self.apply_recipe(recipe);
                self.status = Some(if limits_clamped {
                    format!(
                        "Loaded recipe {}; resource limits were clamped to desktop safety ceilings",
                        path.display()
                    )
                } else {
                    format!("Loaded recipe {}", path.display())
                });
            }
            Err(error) => {
                self.status = Some(format!("Could not load recipe: {error}"));
            }
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn load_recipe_dialog(&mut self) {
        self.status = Some("Recipe file dialogs are unavailable on this platform".to_string());
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn save_recipe_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(format!("{}.wrf-formula.json", self.recipe_name))
            .add_filter("WRF Formula Recipe", &["json"])
            .save_file()
        else {
            return;
        };
        let result = self.build_recipe().and_then(|recipe| {
            let _ = recipe.compile()?;
            let mut bytes = serde_json::to_vec_pretty(&recipe).map_err(|error| {
                FormulaError::new(ErrorKind::Internal, format!("serialize recipe: {error}"))
            })?;
            bytes.push(b'\n');
            atomic_write_bytes(&path, &bytes).map_err(|error| {
                FormulaError::new(
                    ErrorKind::Internal,
                    format!("atomically write recipe: {error}"),
                )
            })?;
            Ok(())
        });
        match result {
            Ok(()) => self.status = Some(format!("Saved recipe {}", path.display())),
            Err(error) => self.status = Some(format!("Could not save recipe: {error}")),
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn save_recipe_dialog(&mut self) {
        self.status = Some("Recipe file dialogs are unavailable on this platform".to_string());
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    fn apply_recipe(&mut self, recipe: Recipe) {
        self.source = recipe.source;
        self.recipe_name = recipe.name;
        self.recipe_version = recipe.version;
        self.recipe_description = recipe.description;
        self.authors = recipe.authors;
        self.references = recipe.references;
        self.tags = recipe.tags;
        self.parameter_specs = recipe.parameters;
        self.expected_output_units = recipe.expected_output_units.unwrap_or_default();
        self.requirements = recipe.requirements;
        self.unit_overrides_text =
            format_unit_overrides(&recipe.evaluation_options.variable_unit_overrides);
        self.evaluation_options = recipe.evaluation_options;
        self.resource_limits = recipe.resource_limits.map(clamp_desktop_limits);
        self.parameter_values.clear();
        self.editor_cursor = self.source.chars().count();
        self.editor_selection = Some((self.editor_cursor, self.editor_cursor));
        self.pending_editor_cursor = Some(self.editor_cursor);
        self.sync_parameter_values();
        self.refresh_compile();
    }
}

fn evaluate_source(
    source: EvaluationSource,
    display_hour: HourKey,
    output_name: String,
    compiled: &CompiledFormula,
    parameters: &ParameterValues,
    options: &EvaluationOptions,
    resource_limits: &ResourceLimits,
) -> Result<FormulaLabResult, BridgeError> {
    let result_source = match &source {
        EvaluationSource::Store(source) => FormulaResultSource::Store {
            store_root: source.store_root.clone(),
            hour: source.hour.clone(),
        },
        EvaluationSource::RawWrf {
            path,
            time_index,
            revision,
            ..
        } => FormulaResultSource::RawWrf {
            path: path.clone(),
            time_index: *time_index,
            revision: revision.clone(),
        },
    };
    let (evaluated, grid): (_, Arc<GridFile>) = match source {
        EvaluationSource::Store(source) => {
            let resolver = StoreRunResolver::open_with_exact_times_and_limits(
                source.store_root,
                source.hour.model,
                source.hour.run,
                source.hour.hour,
                source.exact_times,
                resource_limits.clone(),
            )?;
            let grid = resolver.grid();
            let output = evaluate_resolver_2d(compiled, &resolver, parameters, options)?;
            (output, grid)
        }
        EvaluationSource::RawWrf {
            path, time_index, ..
        } => evaluate_wrf_path_2d_with_limits(
            compiled,
            path,
            time_index,
            parameters,
            options,
            resource_limits,
        )?,
    };
    let range = rw_ui::colormap::finite_min_max(&evaluated.values);
    let lat_descending = grid.lat_descending().unwrap_or(false);
    let mut warnings = evaluated.provenance.warnings.clone();
    warnings.extend(evaluated.warnings.iter().cloned());
    if range.is_none() {
        warnings.push("formula result contains no finite display values".to_string());
    }
    warnings.sort();
    warnings.dedup();
    let field = FieldData {
        key: FieldKey {
            hour: display_hour,
            var: output_name,
        },
        units: evaluated.units.clone(),
        nx: evaluated.nx,
        ny: evaluated.ny,
        values: evaluated.values,
        range,
        grid: Some(grid),
        lat_descending,
        style: None,
    };
    Ok(FormulaLabResult {
        field,
        description: evaluated.description,
        provenance: evaluated.provenance,
        warnings,
        source: result_source,
    })
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn load_recipe_bounded(path: &Path) -> Result<Recipe, FormulaError> {
    let file = fs::File::open(path)
        .map_err(|error| FormulaError::new(ErrorKind::Parse, error.to_string()))?;
    let len = file
        .metadata()
        .map_err(|error| FormulaError::new(ErrorKind::Parse, error.to_string()))?
        .len();
    if len > MAX_RECIPE_BYTES {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!("recipe is {len} bytes; desktop limit is {MAX_RECIPE_BYTES} bytes"),
        ));
    }
    // Keep the read bounded even if another process grows the file after the
    // metadata check.
    Recipe::from_json_reader(BufReader::new(file.take(MAX_RECIPE_BYTES + 1)))
}

fn inspect_store_run_revision(source: &StoreFormulaSource) -> Result<StoreRunRevision, String> {
    let root = fs::canonicalize(&source.store_root).map_err(|error| {
        format!(
            "resolve Formula Lab store root {}: {error}",
            source.store_root.display()
        )
    })?;
    let requested_run = root.join(&source.hour.model).join(&source.hour.run);
    let run_dir = fs::canonicalize(&requested_run)
        .map_err(|error| format!("resolve store run {}: {error}", requested_run.display()))?;
    if !run_dir.starts_with(&root) {
        return Err("resolved Formula Lab run escapes its store root".to_string());
    }

    let manifest_path = run_dir.join("run.json");
    let manifest_before = inspect_raw_file_revision(&manifest_path)?;
    let manifest =
        RwsRunManifest::load_for_run(&manifest_path, &source.hour.model, &source.hour.run)
            .map_err(|error| format!("load Formula Lab run manifest: {error}"))?;
    let Some(selected_entry) = manifest.hours.get(&source.hour.hour) else {
        return Err(format!(
            "Formula Lab run no longer contains {}",
            source.hour.time_label()
        ));
    };
    if selected_entry.exact_time() != source.hour.exact_time {
        return Err(format!(
            "Formula Lab selected timestep {} no longer matches the run manifest",
            source.hour.time_label()
        ));
    }
    if manifest.is_exact_time_axis() {
        if source.exact_times.len() != manifest.hours.len() {
            return Err(
                "Formula Lab exact-time axis is incomplete for the selected run".to_string(),
            );
        }
        for (&slot, entry) in &manifest.hours {
            let exact = entry.exact_time().ok_or_else(|| {
                format!("Formula Lab exact-time run is missing metadata for storage slot {slot}")
            })?;
            let supplied = source.exact_times.get(&slot).ok_or_else(|| {
                format!("Formula Lab exact-time axis is missing storage slot {slot}")
            })?;
            if supplied.seconds != exact.lead_seconds as f64 {
                return Err(format!(
                    "Formula Lab exact time for storage slot {slot} no longer matches the run manifest"
                ));
            }
        }
    } else if !source.exact_times.is_empty() {
        if source.exact_times.len() != manifest.hours.len() {
            return Err(
                "Formula Lab legacy forecast-hour axis is incomplete for the selected run"
                    .to_string(),
            );
        }
        for &forecast_hour in manifest.hours.keys() {
            let supplied = source.exact_times.get(&forecast_hour).ok_or_else(|| {
                format!("Formula Lab legacy forecast-hour axis is missing f{forecast_hour:03}")
            })?;
            let expected_seconds = f64::from(forecast_hour) * 3_600.0;
            if supplied.seconds != expected_seconds {
                return Err(format!(
                    "Formula Lab legacy f{forecast_hour:03} time must be {expected_seconds} seconds"
                ));
            }
        }
    }
    if source.temporal_axis_verified && !time_axis_supports_adjacent(&source.exact_times) {
        return Err(
            "Formula Lab source marked temporal-ready without two distinct, increasing times"
                .to_string(),
        );
    }

    let grid = inspect_raw_file_revision(&run_dir.join("grid.rwg"))?;
    if !grid.canonical_path.starts_with(&run_dir) {
        return Err("resolved Formula Lab grid escapes its run directory".to_string());
    }
    let mut hours = Vec::with_capacity(manifest.hours.len());
    for (&hour, entry) in &manifest.hours {
        let revision = inspect_raw_file_revision(&run_dir.join(&entry.file))?;
        if !revision.canonical_path.starts_with(&run_dir) {
            return Err(format!(
                "resolved Formula Lab storage slot {hour} escapes its run directory"
            ));
        }
        hours.push((hour, revision));
    }
    let manifest_after = inspect_raw_file_revision(&manifest_path)?;
    if manifest_before != manifest_after {
        return Err("Formula Lab run manifest changed while its revision was captured".to_string());
    }
    Ok(StoreRunRevision {
        manifest: manifest_after,
        grid,
        hours,
    })
}

fn inspect_raw_file_revision(path: &Path) -> Result<RawFileRevision, String> {
    let canonical_path =
        fs::canonicalize(path).map_err(|error| format!("resolve {}: {error}", path.display()))?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|error| format!("inspect {}: {error}", canonical_path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file",
            canonical_path.display()
        ));
    }
    let modified = metadata.modified().map_err(|error| {
        format!(
            "read modification time for {}: {error}",
            canonical_path.display()
        )
    })?;
    Ok(RawFileRevision {
        canonical_path,
        len: metadata.len(),
        modified,
        created: metadata.created().ok(),
    })
}

fn parse_unit_overrides(text: &str) -> Result<BTreeMap<String, String>, FormulaError> {
    let mut output = BTreeMap::new();
    let mut canonical = BTreeSet::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, units)) = line.split_once('=') else {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!("unit override line {} must be NAME = unit", index + 1),
            ));
        };
        let name = name.trim();
        let units = units.trim();
        if name.is_empty() || units.is_empty() {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!("unit override line {} has an empty name or unit", index + 1),
            ));
        }
        if !canonical.insert(name.to_ascii_lowercase()) {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!("duplicate case-insensitive unit override '{name}'"),
            ));
        }
        output.insert(name.to_string(), units.to_string());
    }
    Ok(output)
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn format_unit_overrides(overrides: &BTreeMap<String, String>) -> String {
    overrides
        .iter()
        .map(|(name, units)| format!("{name} = {units}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalized_output_name(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Output field name cannot be empty".to_string());
    }
    if trimmed.len() > 128 {
        return Err("Output field name is longer than 128 bytes".to_string());
    }
    let mut output = String::new();
    let mut underscore = false;
    for character in trimmed.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character.to_ascii_lowercase());
            underscore = false;
        } else if !underscore {
            output.push('_');
            underscore = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output.is_empty() {
        return Err("Output field name has no usable ASCII characters".to_string());
    }
    if output.as_bytes()[0].is_ascii_digit() {
        output.insert_str(0, "formula_");
    }
    if output.len() > 128 {
        return Err("Sanitized output field name is longer than 128 bytes".to_string());
    }
    Ok(output)
}

fn desktop_standard_limits() -> ResourceLimits {
    ResourceLimits {
        max_output_elements: 64 * 1024 * 1024,
        max_working_bytes: 512 * 1024 * 1024,
        max_total_allocated_bytes: 2 * 1024 * 1024 * 1024,
        max_operations: 1_000_000_000,
        ..ResourceLimits::default()
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn clamp_desktop_limits(requested: ResourceLimits) -> ResourceLimits {
    clamp_limits_to(requested, &ResourceLimits::default())
}

fn clamp_limits_to(mut requested: ResourceLimits, ceiling: &ResourceLimits) -> ResourceLimits {
    requested.max_source_bytes = requested.max_source_bytes.min(ceiling.max_source_bytes);
    requested.max_tokens = requested.max_tokens.min(ceiling.max_tokens);
    requested.max_ast_nodes = requested.max_ast_nodes.min(ceiling.max_ast_nodes);
    requested.max_ast_depth = requested.max_ast_depth.min(ceiling.max_ast_depth);
    requested.max_identifier_bytes = requested
        .max_identifier_bytes
        .min(ceiling.max_identifier_bytes);
    requested.max_function_arity = requested.max_function_arity.min(ceiling.max_function_arity);
    requested.max_assignments = requested.max_assignments.min(ceiling.max_assignments);
    requested.max_dependencies = requested.max_dependencies.min(ceiling.max_dependencies);
    requested.max_output_elements = requested
        .max_output_elements
        .min(ceiling.max_output_elements);
    requested.max_working_bytes = requested.max_working_bytes.min(ceiling.max_working_bytes);
    requested.max_total_allocated_bytes = requested
        .max_total_allocated_bytes
        .min(ceiling.max_total_allocated_bytes);
    requested.max_operations = requested.max_operations.min(ceiling.max_operations);
    requested
}

fn optional_bound_ui(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<f64>,
    fallback: f64,
) -> bool {
    let mut enabled = value.is_some();
    let mut changed = ui.checkbox(&mut enabled, label).changed();
    if enabled && value.is_none() {
        *value = Some(fallback);
        changed = true;
    } else if !enabled && value.is_some() {
        *value = None;
        changed = true;
    }
    if let Some(value) = value {
        changed |= ui.add(egui::DragValue::new(value).speed(0.1)).changed();
    }
    changed
}

fn span_excerpt(source: &str, span: Span) -> Option<String> {
    let start = span.start.min(source.len());
    let end = span.end.min(source.len()).max(start);
    source.get(start..end).map(ToString::to_string)
}

fn requirement_text(requirement: &Requirement) -> String {
    match requirement {
        Requirement::Field { name } => format!("field {name}"),
        Requirement::MassMapFactor => "WRF mass-grid map factor".to_string(),
        Requirement::PhysicalHeight { datum } => format!("physical height ({datum:?})"),
        Requirement::AdjacentTimes => "verified adjacent valid times".to_string(),
        Requirement::GridProjectedVector => "grid-projected vector components".to_string(),
    }
}

fn provenance_ui(ui: &mut egui::Ui, provenance: &FormulaProvenance) {
    ui.label(format!("Engine: {}", provenance.engine_version));
    ui.label(format!("Fingerprint: {}", provenance.source_fingerprint));
    if let Some(valid_time) = &provenance.valid_time {
        ui.label(format!("Valid time: {valid_time}"));
    }
    if let Some(identity) = &provenance.input_identity {
        ui.label(format!("Input: {identity}"));
    }
    if let (Some(name), Some(version)) = (&provenance.recipe_name, &provenance.recipe_version) {
        ui.label(format!("Recipe: {name} {version}"));
    }
    if !provenance.inputs.is_empty() {
        ui.label("Resolved inputs:");
        for input in &provenance.inputs {
            ui.label(
                egui::RichText::new(format!(
                    "• {} → {} · {:?} · {}",
                    input.requested_name, input.resolved_name, input.shape, input.effective_units
                ))
                .small(),
            );
        }
    }
}

fn boundary_text(policy: BoundaryPolicy) -> &'static str {
    match policy {
        BoundaryPolicy::OneSidedSecondOrder => "one-sided second order",
        BoundaryPolicy::Missing => "missing",
        BoundaryPolicy::Error => "error",
    }
}

fn missing_text(policy: MissingPolicy) -> &'static str {
    match policy {
        MissingPolicy::Propagate => "propagate",
        MissingPolicy::Error => "error",
        MissingPolicy::IgnoreInReductions => "ignore in reductions",
    }
}

fn nonfinite_text(policy: NonFinitePolicy) -> &'static str {
    match policy {
        NonFinitePolicy::Propagate => "propagate",
        NonFinitePolicy::Error => "error",
    }
}

fn find_inventory_name(variables: &[VarInfo], aliases: &[&str]) -> Option<String> {
    aliases.iter().find_map(|alias| {
        variables
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case(alias))
            .map(|field| field.name.clone())
    })
}

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len())
}

fn starter_field_description(base: &str, field: Option<&str>, wanted: &str) -> String {
    match field {
        Some(field) => format!("{base} Uses exact token '{field}'."),
        None => format!("Unavailable: this timestep has no {wanted} field."),
    }
}

fn missing_pair_description(
    base: &str,
    first: Option<&str>,
    second: Option<&str>,
    first_wanted: &str,
    second_wanted: &str,
) -> String {
    if first.is_some() && second.is_some() {
        base.to_owned()
    } else {
        let mut missing = Vec::new();
        if first.is_none() {
            missing.push(first_wanted);
        }
        if second.is_none() {
            missing.push(second_wanted);
        }
        format!("Unavailable: missing {}.", missing.join(" and "))
    }
}

fn raw_wrf_common_fields() -> Vec<VarInfo> {
    let mut fields = [
        ("U10", "m s-1", VarKind::Surface2D),
        ("V10", "m s-1", VarKind::Surface2D),
        ("T2", "K", VarKind::Surface2D),
        ("Q2", "kg kg-1", VarKind::Surface2D),
        ("PSFC", "Pa", VarKind::Surface2D),
        ("RAINC", "mm", VarKind::Surface2D),
        ("RAINNC", "mm", VarKind::Surface2D),
        ("REFL_10CM", "dBZ", VarKind::Pressure3D),
        ("ua", "m s-1", VarKind::Pressure3D),
        ("va", "m s-1", VarKind::Pressure3D),
        ("wa", "m s-1", VarKind::Pressure3D),
        ("tk", "K", VarKind::Pressure3D),
        ("pressure", "hPa", VarKind::Pressure3D),
        ("pres", "Pa", VarKind::Pressure3D),
        ("z", "m", VarKind::Pressure3D),
    ]
    .into_iter()
    .map(|(name, units, kind)| VarInfo {
        name: name.to_owned(),
        units: units.to_owned(),
        kind,
        levels_hpa: Vec::new(),
    })
    .collect::<Vec<_>>();
    fields.extend(
        wrf_core::variables::VARS
            .iter()
            .filter(|definition| !PACKED_WRF_FORMULA_FIELDS.contains(&definition.name))
            .map(|definition| VarInfo {
                name: definition.name.to_owned(),
                units: definition.default_units.to_owned(),
                kind: match definition.dim {
                    wrf_core::variables::VarDim::TwoD => VarKind::Surface2D,
                    wrf_core::variables::VarDim::ThreeD => VarKind::Pressure3D,
                },
                levels_hpa: Vec::new(),
            }),
    );
    fields.sort_by_key(|field| field.name.to_ascii_lowercase());
    fields.dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
    fields
}

fn safe_unit_override(stored_units: &str) -> Option<&'static str> {
    match stored_units.trim().to_ascii_lowercase().as_str() {
        "gpm" => Some("m"),
        "-" | "0/1" | "fraction" | "index" | "count" => Some("1"),
        "w m{-2}" => Some("W/m2"),
        "j m-2" => Some("kg s-2"),
        _ => None,
    }
}

fn time_axis_supports_adjacent(axis: &BTreeMap<u16, ExactStoreTime>) -> bool {
    if axis.len() < 2 {
        return false;
    }
    axis.values()
        .map(|time| time.seconds)
        .try_fold(None, |previous, seconds| {
            if !seconds.is_finite() || previous.is_some_and(|prior| seconds <= prior) {
                Err(())
            } else {
                Ok(Some(seconds))
            }
        })
        .is_ok()
}

fn selected_axis_neighbor_interval_seconds(
    axis: &BTreeMap<u16, ExactStoreTime>,
    selected_slot: u16,
) -> Option<f64> {
    use std::ops::Bound::{Excluded, Unbounded};

    let current = axis.get(&selected_slot)?.seconds;
    if !current.is_finite() {
        return None;
    }
    let mut intervals = Vec::with_capacity(2);
    if let Some((_, previous)) = axis.range(..selected_slot).next_back() {
        let interval = current - previous.seconds;
        if !interval.is_finite() || interval <= 0.0 {
            return None;
        }
        intervals.push(interval);
    }
    if let Some((_, next)) = axis.range((Excluded(selected_slot), Unbounded)).next() {
        let interval = next.seconds - current;
        if !interval.is_finite() || interval <= 0.0 {
            return None;
        }
        intervals.push(interval);
    }
    intervals.into_iter().reduce(f64::max)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string());
    format!("Formula Lab isolated an internal panic: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_names_are_store_safe() {
        assert_eq!(
            normalized_output_name(" 0–3 km lapse rate ").unwrap(),
            "formula_0_3_km_lapse_rate"
        );
        assert!(normalized_output_name("***").is_err());
    }

    #[test]
    fn field_insertion_advances_and_replaces_the_saved_selection() {
        let mut panel = FormulaLabPanel::new();
        panel.source.clear();
        panel.editor_cursor = 0;
        panel.editor_selection = Some((0, 0));
        panel.insert_field_token("temperature_2m");
        panel.insert_field_token(" + dewpoint_2m");
        assert_eq!(panel.source, "temperature_2m + dewpoint_2m");
        assert_eq!(panel.editor_cursor, panel.source.chars().count());

        panel.editor_selection = Some((0, "temperature_2m".chars().count()));
        panel.insert_field_token("T2");
        assert_eq!(panel.source, "T2 + dewpoint_2m");
        assert_eq!(panel.pending_editor_cursor, Some(2));
    }

    #[test]
    fn unit_overrides_reject_case_collisions() {
        let error = parse_unit_overrides("T2 = K\nt2 = degC").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Compile);
    }

    #[test]
    fn every_offered_unit_override_is_accepted_by_the_pinned_evaluator() {
        for stored in [
            "gpm", "-", "0/1", "fraction", "index", "count", "W m{-2}", "J m-2",
        ] {
            let suggested = safe_unit_override(stored).expect("known safe suggestion");
            assert!(
                wrf_formula::parse_unit(suggested).is_ok(),
                "{stored} suggestion {suggested} must parse"
            );
        }
    }

    #[test]
    fn cadence_preflight_uses_only_neighbors_of_selected_output() {
        let axis = BTreeMap::from([
            (0, ExactStoreTime::new(0.0, None)),
            (1, ExactStoreTime::new(3_600.0, None)),
            (2, ExactStoreTime::new(7_200.0, None)),
            (3, ExactStoreTime::new(18_000.0, None)),
        ]);
        assert_eq!(
            selected_axis_neighbor_interval_seconds(&axis, 0),
            Some(3_600.0)
        );
        assert_eq!(
            selected_axis_neighbor_interval_seconds(&axis, 2),
            Some(10_800.0)
        );
    }

    #[test]
    fn loaded_recipes_cannot_raise_desktop_resource_ceilings() {
        let ceiling = ResourceLimits::default();
        let mut requested = ceiling.clone();
        requested.max_working_bytes = usize::MAX;
        requested.max_total_allocated_bytes = u64::MAX;
        requested.max_operations = u64::MAX;
        let clamped = clamp_desktop_limits(requested);
        assert_eq!(clamped.max_working_bytes, ceiling.max_working_bytes);
        assert_eq!(
            clamped.max_total_allocated_bytes,
            ceiling.max_total_allocated_bytes
        );
        assert_eq!(clamped.max_operations, ceiling.max_operations);
    }

    #[test]
    fn standard_profile_fits_known_large_wrf_volume_but_is_bounded() {
        let mut panel = FormulaLabPanel::new();
        let standard = panel.effective_resource_limits();
        assert!(
            standard.max_output_elements >= 800 * 800 * 79,
            "known 800x800x79 volume must fit"
        );
        assert_eq!(standard.max_working_bytes, 512 * 1024 * 1024);
        assert_eq!(standard.max_total_allocated_bytes, 2 * 1024 * 1024 * 1024);
        panel.large_research_profile = true;
        let large = panel.effective_resource_limits();
        assert!(large.max_output_elements > standard.max_output_elements);
        assert!(large.max_total_allocated_bytes > standard.max_total_allocated_bytes);
    }

    #[test]
    fn explicit_source_selection_never_falls_back_silently() {
        let mut panel = FormulaLabPanel::new();
        panel.set_source_kind(FormulaSourceKind::RawWrf);
        let store = StoreFormulaSource {
            store_root: PathBuf::from("store"),
            hour: HourKey {
                model: "wrf".to_string(),
                run: "run".to_string(),
                hour: 0,
                exact_time: None,
            },
            exact_times: BTreeMap::new(),
            temporal_axis_verified: false,
            variables: Vec::new(),
        };
        assert!(
            panel
                .effective_source(FormulaLabSources {
                    store: Some(&store),
                    raw_wrf: None,
                    evaluation_blocked: None,
                })
                .is_none()
        );
        panel.sync_sources(FormulaLabSources {
            store: Some(&store),
            raw_wrf: None,
            evaluation_blocked: None,
        });
        assert_eq!(panel.source_kind(), FormulaSourceKind::RawWrf);
    }

    #[test]
    fn unverified_store_time_axis_blocks_only_temporal_formulas() {
        let store = StoreFormulaSource {
            store_root: PathBuf::from("store"),
            hour: HourKey {
                model: "wrf".to_string(),
                run: "run".to_string(),
                hour: 0,
                exact_time: None,
            },
            exact_times: BTreeMap::new(),
            temporal_axis_verified: false,
            variables: Vec::new(),
        };
        let sources = FormulaLabSources {
            store: Some(&store),
            raw_wrf: None,
            evaluation_blocked: None,
        };
        let mut panel = FormulaLabPanel::new();
        panel.set_source_kind(FormulaSourceKind::Store);
        panel.set_source("temperature_2m");
        assert!(panel.compiled.is_some());
        assert!(panel.temporal_source_allowed(sources));

        panel.set_source("dt(temperature_2m)");
        assert!(panel.compiled.is_some());
        assert!(!panel.temporal_source_allowed(sources));

        let mut verified = store;
        verified.temporal_axis_verified = true;
        assert!(panel.temporal_source_allowed(FormulaLabSources {
            store: Some(&verified),
            raw_wrf: None,
            evaluation_blocked: None,
        }));
    }

    fn store_var(name: &str, units: &str, kind: VarKind, levels: &[u16]) -> VarInfo {
        VarInfo {
            name: name.to_owned(),
            units: units.to_owned(),
            kind,
            levels_hpa: levels.to_vec(),
        }
    }

    fn test_store_source(variables: Vec<VarInfo>) -> StoreFormulaSource {
        StoreFormulaSource {
            store_root: PathBuf::from("store"),
            hour: HourKey {
                model: "hrrr".to_owned(),
                run: "20260711_00z".to_owned(),
                hour: 0,
                exact_time: None,
            },
            exact_times: BTreeMap::from([
                (0, ExactStoreTime::new(0.0, Some("f000".to_owned()))),
                (1, ExactStoreTime::new(3_600.0, Some("f001".to_owned()))),
            ]),
            temporal_axis_verified: true,
            variables,
        }
    }

    #[test]
    fn store_readiness_preflights_fields_units_and_capabilities() {
        let source = test_store_source(vec![
            store_var("U10", "m/s", VarKind::Surface2D, &[]),
            store_var("V10", "m/s", VarKind::Surface2D, &[]),
            store_var("terrain_height", "gpm", VarKind::Surface2D, &[]),
            store_var("pressure_bad", "kPa", VarKind::Surface2D, &[]),
            store_var("unitless_unknown", "", VarKind::Surface2D, &[]),
            store_var(
                "temperature_iso",
                "K",
                VarKind::Pressure3D,
                &[1000, 850, 700],
            ),
            store_var("temperature_2m", "K", VarKind::Surface2D, &[]),
        ]);
        let sources = FormulaLabSources {
            store: Some(&source),
            raw_wrf: None,
            evaluation_blocked: None,
        };
        let mut panel = FormulaLabPanel::new();
        panel.set_source("sqrt(U10^2 + V10^2)");
        assert!(panel.source_readiness(sources).ready);

        panel.set_source("U10 + missing_wind");
        let missing = panel.source_readiness(sources);
        assert!(!missing.ready);
        assert!(
            missing
                .blockers
                .iter()
                .any(|message| message.contains("missing_wind"))
        );

        panel.set_source("ddx(U10)");
        let calculus = panel.source_readiness(sources);
        assert!(
            calculus
                .blockers
                .iter()
                .any(|message| message.contains("Raw WRF"))
        );

        panel.set_source("terrain_height");
        let units = panel.source_readiness(sources);
        assert_eq!(units.override_suggestions.len(), 1);
        panel.append_unit_override(&units.override_suggestions[0]);
        assert!(panel.source_readiness(sources).ready);

        panel.set_source("pressure_bad");
        let scaled_units = panel.source_readiness(sources);
        assert!(!scaled_units.ready);
        assert!(scaled_units.override_suggestions.is_empty());
        assert!(
            scaled_units
                .blockers
                .iter()
                .any(|message| message.contains("scale-aware"))
        );

        panel.set_source("unitless_unknown");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("no stored unit metadata"))
        );

        panel.set_source("2 + 2");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("result is scalar"))
        );

        panel.unit_overrides_text = "U10 = m/s".to_owned();
        panel.set_source("U10");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("remove its stale unit override"))
        );
        panel.unit_overrides_text.clear();
        panel.set_source("temperature_iso - temperature_2m");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("need mean_z"))
        );

        panel.requirements.maximum_cadence_seconds = Some(1_800.0);
        panel.requirements.maximum_horizontal_spacing_m = Some(3_000.0);
        panel.requirements.minimum_vertical_levels = Some(20);
        panel.set_source("temperature_2m");
        let recipe = panel.source_readiness(sources);
        assert!(
            recipe
                .blockers
                .iter()
                .any(|message| message.contains("largest neighbor interval is 3600"))
        );
        assert!(
            recipe
                .blockers
                .iter()
                .any(|message| message.contains("does not persist resolver dx/dy"))
        );
        assert!(
            recipe
                .blockers
                .iter()
                .any(|message| message.contains("run-wide nz"))
        );
    }

    #[test]
    fn store_starters_use_exact_tokens_and_never_substitute_humidity_for_dewpoint() {
        let source = test_store_source(vec![
            store_var("U10", "m/s", VarKind::Surface2D, &[]),
            store_var("V10", "m/s", VarKind::Surface2D, &[]),
            store_var("temperature_2m", "K", VarKind::Surface2D, &[]),
            store_var("rh_2m", "%", VarKind::Surface2D, &[]),
        ]);
        let panel = FormulaLabPanel::new();
        let starters = panel.starters(FormulaLabSources {
            store: Some(&source),
            raw_wrf: None,
            evaluation_blocked: None,
        });
        assert_eq!(
            starters
                .iter()
                .find(|starter| starter.title == "10 m wind")
                .and_then(|starter| starter.source.as_deref()),
            Some("sqrt(U10^2 + V10^2)")
        );
        assert!(
            starters
                .iter()
                .find(|starter| starter.title == "Dewpoint spread")
                .is_some_and(|starter| starter.source.is_none())
        );
    }

    #[test]
    fn raw_wrf_browser_uses_registry_but_excludes_packed_outputs() {
        let fields = raw_wrf_common_fields();
        assert!(
            fields.len() > 40,
            "wrf-core registry should provide breadth"
        );
        assert!(fields.iter().any(|field| field.name == "temp"));
        for packed in ["cape2d", "uvmet", "uvmet10", "mean_wind", "cloudfrac"] {
            assert!(
                fields.iter().all(|field| field.name != packed),
                "packed component output {packed} must not be advertised"
            );
        }
    }

    #[test]
    fn raw_wrf_readiness_blocks_known_packed_and_unreduced_3d_fields() {
        let path = std::env::temp_dir().join(format!(
            "bowecho-formula-raw-readiness-{}",
            std::process::id()
        ));
        fs::write(&path, b"stable raw source identity").expect("raw fixture");
        let raw = RawWrfFormulaSource {
            path: path.clone(),
            initial_time_index: 0,
            display_hour: HourKey {
                model: "raw-wrf".to_owned(),
                run: "fixture".to_owned(),
                hour: 0,
                exact_time: None,
            },
        };
        let sources = FormulaLabSources {
            store: None,
            raw_wrf: Some(&raw),
            evaluation_blocked: None,
        };
        let mut panel = FormulaLabPanel::new();
        panel.set_source_kind(FormulaSourceKind::RawWrf);
        panel.sync_sources(sources);

        panel.set_source("pressure");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("Known 3-D raw WRF"))
        );
        panel.set_source("cape2d");
        assert!(
            panel
                .source_readiness(sources)
                .blockers
                .iter()
                .any(|message| message.contains("packed component"))
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn formula_editor_state_round_trips_without_runtime_state() {
        let mut panel = FormulaLabPanel::new();
        panel.set_source("dt(temperature_2m)");
        panel.output_name = "temperature_tendency".to_owned();
        panel.source_kind = FormulaSourceKind::RawWrf;
        panel.status = Some("runtime-only".to_owned());
        panel.large_raw_confirmed = true;
        let state = panel.state_json();
        assert!(state.get("status").is_none());
        assert!(state.get("large_raw_confirmed").is_none());

        let mut restored = FormulaLabPanel::new();
        assert!(restored.apply_state_json(&state));
        assert_eq!(restored.source, "dt(temperature_2m)");
        assert_eq!(restored.output_name, "temperature_tendency");
        assert_eq!(restored.source_kind, FormulaSourceKind::RawWrf);
        assert!(!restored.large_raw_confirmed);
        assert!(restored.status.is_none());
    }

    #[test]
    fn legacy_store_revision_accepts_only_complete_forecast_hour_times() {
        let root = std::env::temp_dir().join(format!(
            "bowecho-formula-v1-revision-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let run_dir = root.join("hrrr").join("20260711_00z");
        fs::create_dir_all(&run_dir).expect("create v1 fixture");
        fs::write(run_dir.join("grid.rwg"), b"grid").expect("grid identity file");
        fs::write(run_dir.join("f000.rws"), b"hour zero").expect("hour zero");
        fs::write(run_dir.join("f003.rws"), b"hour three").expect("hour three");
        let manifest = serde_json::json!({
            "schema": "rw-store.run.v1",
            "model": "hrrr",
            "run": "20260711_00z",
            "grid_hash": "fixture-grid",
            "nx": 2,
            "ny": 2,
            "hours": {
                "0": {"file":"f000.rws","written_unix":1,"encode_ms":1,"variables":["temperature_2m"]},
                "3": {"file":"f003.rws","written_unix":2,"encode_ms":1,"variables":["temperature_2m"]}
            },
            "writer": {"name":"test","version":"1","build":"fixture"}
        });
        fs::write(
            run_dir.join("run.json"),
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("write manifest");
        let mut source = StoreFormulaSource {
            store_root: root.clone(),
            hour: HourKey {
                model: "hrrr".to_owned(),
                run: "20260711_00z".to_owned(),
                hour: 0,
                exact_time: None,
            },
            exact_times: BTreeMap::from([
                (0, ExactStoreTime::new(0.0, Some("f000".to_owned()))),
                (3, ExactStoreTime::new(10_800.0, Some("f003".to_owned()))),
            ]),
            temporal_axis_verified: true,
            variables: Vec::new(),
        };
        assert!(inspect_store_run_revision(&source).is_ok());

        source.exact_times.remove(&3);
        assert!(
            inspect_store_run_revision(&source)
                .unwrap_err()
                .contains("incomplete")
        );
        source.exact_times.insert(3, ExactStoreTime::new(3.0, None));
        assert!(
            inspect_store_run_revision(&source)
                .unwrap_err()
                .contains("must be 10800")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_size_raw_replacement_invalidates_task_result_and_consent() {
        let path = std::env::temp_dir().join(format!(
            "rusty_weather_formula_revision_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::write(&path, b"one").expect("write first revision");
        let source = RawWrfFormulaSource {
            path: path.clone(),
            initial_time_index: 7,
            display_hour: HourKey {
                model: "raw-wrf".to_string(),
                run: "revision-test".to_string(),
                hour: 0,
                exact_time: None,
            },
        };
        let mut panel = FormulaLabPanel::new();
        panel.sync_raw_source(Some(&source));
        let first = panel.raw_revision.clone().expect("first revision");
        panel.large_raw_confirmed = true;
        panel.raw_time_index = 3;

        // Keep both the path and length unchanged: revision protection must
        // not depend only on metadata.len().
        let mut replacement_revision = None;
        for attempt in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(25));
            let contents: &[u8] = if attempt % 2 == 0 { b"two" } else { b"six" };
            fs::write(&path, contents).expect("replace raw source with equal-length content");
            let current = inspect_raw_file_revision(&path).expect("replacement revision");
            if current != first {
                replacement_revision = Some(current);
                break;
            }
        }
        let replacement_revision = replacement_revision
            .expect("filesystem must eventually expose the same-size replacement revision");
        let stale_source = FormulaResultSource::RawWrf {
            path: path.clone(),
            time_index: 3,
            revision: first.clone(),
        };
        assert!(!stale_source.revision_is_current());

        let (_tx, rx) = channel();
        let task = EvaluationTask {
            rx,
            generation: panel.editor_generation,
            source: stale_source,
            raw_revision: Some(first.clone()),
        };
        assert!(panel.task_is_stale(
            &task,
            FormulaLabSources {
                store: None,
                raw_wrf: Some(&source),
                evaluation_blocked: None,
            }
        ));

        panel.sync_raw_source(Some(&source));
        let second = panel.raw_revision.clone().expect("second revision");
        assert_eq!(second, replacement_revision);
        assert_ne!(first, second);
        assert_eq!(first.len, second.len);
        assert!(!panel.large_raw_confirmed);
        assert_eq!(panel.raw_time_index, source.initial_time_index);
        let _ = fs::remove_file(path);
    }
}
