//! Native egui Formula Lab for raw WRF files and rw-store fields.
//!
//! This module owns presentation and background orchestration only. Scientific
//! resolution, time honesty, output-shape validation, and f64-to-f32 narrowing
//! live in `rw-formula`. The host supplies the current evaluation source and
//! installs a completed `FieldData` into its viewer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(any(windows, target_os = "macos"))]
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::SystemTime;

use eframe::egui;
use rw_formula::{
    BoundaryPolicy, BridgeError, CompiledFormula, ErrorKind, EvaluationOptions, ExactStoreTime,
    FormulaError, FormulaProvenance, MissingPolicy, NonFinitePolicy, ParameterSpec,
    ParameterValues, Recipe, RecipeReference, RecipeRequirements, Requirement, ResourceLimits,
    Span, StoreRunResolver, evaluate_resolver_2d, evaluate_wrf_path_2d_with_limits,
};
#[cfg(any(windows, target_os = "macos"))]
use rw_store::atomic::atomic_write_bytes;
use rw_store::grid::GridFile;
use rw_store::run::RwsRunManifest;
use rw_ui::{FieldData, FieldKey, HourKey};

const LARGE_RAW_WRF_BYTES: u64 = 1 << 30;
#[cfg(any(windows, target_os = "macos"))]
const MAX_RECIPE_BYTES: u64 = 4 * 1024 * 1024;

/// Current rw-store source offered by the host.
#[derive(Debug, Clone)]
pub struct StoreFormulaSource {
    pub store_root: PathBuf,
    pub hour: HourKey,
    /// Must be empty unless the host verified every stored timestep's valid
    /// time. Exact-time runs supply the complete map, never a partial axis.
    pub exact_times: BTreeMap<u16, ExactStoreTime>,
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
                let time_note = if source.exact_times.is_empty() {
                    "dt disabled: valid times not verified"
                } else {
                    "verified exact time axis"
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

/// Reusable Formula Lab window state.
pub struct FormulaLabPanel {
    pub open: bool,
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
    /// Every input that can affect an evaluation advances this counter. A
    /// worker captures it at launch and can never publish an obsolete result.
    editor_generation: u64,
}

impl Default for FormulaLabPanel {
    fn default() -> Self {
        let mut panel = Self {
            open: false,
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

    pub fn compiled(&self) -> Option<&CompiledFormula> {
        self.compiled.as_ref()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn set_source(&mut self, source: impl Into<String>) {
        let source = source.into();
        if self.source != source {
            self.source = source;
            self.refresh_compile();
        }
    }

    pub fn raw_time_index(&self) -> usize {
        self.raw_time_index
    }

    pub fn set_raw_time_index(&mut self, time_index: usize) {
        if self.raw_time_index != time_index {
            self.raw_time_index = time_index;
            self.mark_editor_changed();
        }
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

    /// Poll any worker and draw the Formula Lab window. Polling continues when
    /// the window is closed, so a completed background result is never lost.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        sources: FormulaLabSources<'_>,
    ) -> Option<FormulaLabResult> {
        self.sync_raw_source(sources.raw_wrf);
        if self.source_kind == FormulaSourceKind::Store
            && sources.store.is_none()
            && sources.raw_wrf.is_some()
        {
            self.set_source_kind(FormulaSourceKind::RawWrf);
        } else if self.source_kind == FormulaSourceKind::RawWrf
            && sources.raw_wrf.is_none()
            && sources.store.is_some()
        {
            self.set_source_kind(FormulaSourceKind::Store);
        }
        // Source synchronization must happen before polling so a result from a
        // replaced raw file cannot briefly update provenance or reach the host.
        let completed = self.poll_task(sources);
        if !self.open {
            return completed;
        }

        let mut open = self.open;
        egui::Window::new("Formula Lab")
            .open(&mut open)
            .default_size([720.0, 720.0])
            .resizable(true)
            .show(ctx, |ui| {
                self.window_ui(ui, sources);
            });
        self.open = open;
        completed
    }

    fn window_ui(&mut self, ui: &mut egui::Ui, sources: FormulaLabSources<'_>) {
        let source_kind_before = self.source_kind;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open recipe…").clicked() {
                self.load_recipe_dialog();
            }
            if ui.button("Save recipe…").clicked() {
                self.save_recipe_dialog();
            }
            ui.separator();
            ui.label("Source:");
            ui.add_enabled_ui(sources.store.is_some(), |ui| {
                ui.selectable_value(&mut self.source_kind, FormulaSourceKind::Store, "Store");
            });
            ui.add_enabled_ui(sources.raw_wrf.is_some(), |ui| {
                ui.selectable_value(&mut self.source_kind, FormulaSourceKind::RawWrf, "Raw WRF");
            });
            ui.separator();
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
        if self.source_kind != source_kind_before {
            self.mark_editor_changed();
        }

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
            if let Some(source) = sources.raw_wrf {
                if self.raw_source_error.is_none() {
                    if let Some(revision) = &self.raw_revision {
                        if revision.len >= LARGE_RAW_WRF_BYTES {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new("Large raw-WRF formula evaluation")
                                        .strong(),
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
                }
            }
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Starters:").small().weak());
            if ui
                .add_enabled(
                    sources.store.is_some(),
                    egui::Button::new("Store 10 m wind"),
                )
                .clicked()
            {
                self.set_source_kind(FormulaSourceKind::Store);
                self.set_source("sqrt(u_10m^2 + v_10m^2)");
            }
            if ui
                .add_enabled(
                    sources.raw_wrf.is_some(),
                    egui::Button::new("Raw WRF 10 m wind"),
                )
                .clicked()
            {
                self.set_source_kind(FormulaSourceKind::RawWrf);
                self.set_source("sqrt(U10^2 + V10^2)");
            }
        });

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

        ui.label("Equation");
        let source_response = ui.add(
            egui::TextEdit::multiline(&mut self.source)
                .code_editor()
                .desired_rows(9)
                .desired_width(f32::INFINITY)
                .hint_text("wind = grid_vector(ua, va)\nmagnitude(wind)"),
        );
        if source_response.changed() {
            self.refresh_compile();
        }

        self.compile_status_ui(ui);
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
            && !self.large_raw_needs_confirmation(sources)
            && sources.evaluation_blocked.is_none();
        ui.horizontal(|ui| {
            let clicked = ui
                .add_enabled(can_run, egui::Button::new("Evaluate and display"))
                .clicked();
            if clicked {
                if let (Some(source), Ok(output_name)) =
                    (self.effective_source(sources), output_name)
                {
                    self.start_evaluation(ui.ctx(), source, output_name);
                }
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

    fn compile_status_ui(&self, ui: &mut egui::Ui) {
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
        ui.label(egui::RichText::new("✓ Formula compiled").color(egui::Color32::LIGHT_GREEN));
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
                    "Generated {} ({}×{}, {})",
                    result.field.key.var, result.field.nx, result.field.ny, result.field.units
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

    #[cfg(any(windows, target_os = "macos"))]
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

    #[cfg(not(any(windows, target_os = "macos")))]
    fn load_recipe_dialog(&mut self) {
        self.status = Some("Recipe file dialogs need Windows or macOS".to_string());
    }

    #[cfg(any(windows, target_os = "macos"))]
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

    #[cfg(not(any(windows, target_os = "macos")))]
    fn save_recipe_dialog(&mut self) {
        self.status = Some("Recipe file dialogs need Windows or macOS".to_string());
    }

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

#[cfg(any(windows, target_os = "macos"))]
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
        return Err("Formula Lab received exact times for a legacy forecast-hour run".to_string());
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
    let mut limits = ResourceLimits::default();
    limits.max_output_elements = 64 * 1024 * 1024;
    limits.max_working_bytes = 512 * 1024 * 1024;
    limits.max_total_allocated_bytes = 2 * 1024 * 1024 * 1024;
    limits.max_operations = 1_000_000_000;
    limits
}

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
    fn unit_overrides_reject_case_collisions() {
        let error = parse_unit_overrides("T2 = K\nt2 = degC").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Compile);
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
