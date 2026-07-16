//! Nonmodal editor for analyst-authored sounding corrections.
//!
//! This editor deliberately lives in its own [`egui::Window`]. It must never
//! participate in the sounding pane's vertical layout: a long correction
//! recipe remains scrollable while the SHARPpy canvas keeps every pixel of
//! its allocation.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;

use eframe::egui;
use rustwx_sounding::SoundingColumn;

use crate::sounding_correction::{
    BlendControlPoint, BlendExtent, BlendShape, BlendSpec, CorrectionLevel, CorrectionRecipe,
    CorrectionResult, MoistureEdit, MoistureMode, MoistureTarget, QcIssue, QcIssueKind, QcSeverity,
    ThermalEdit, ThermalMode, ThermalTarget, WindEdit, WindMode, WindTarget,
    apply_correction_recipe, preview_convective_adjustment,
};
use crate::sounding_correction_io::{
    BatchAxis, BatchMember, BatchValue, CorrectionBatchAxisKind, CorrectionSourceContext,
    CorrectionSourceProvenance, ImportedRawSounding, MinMedianMax, SoundingCorrectionBundle,
    apply_correction_batch_axis, cartesian_batch_members, corrected_profile_csv,
    correction_batch_axis_value, finite_min_median_max, parse_sharppy_raw_text, sharppy_raw_text,
    source_bound_bundle_from_json,
};

const EDITOR_ID: &str = "bowecho_sounding_correction_editor";
const EDITOR_DEFAULT_WIDTH: f32 = 860.0;
const EDITOR_DEFAULT_HEIGHT: f32 = 620.0;
const EDITOR_MIN_WIDTH: f32 = 620.0;
const EDITOR_MIN_HEIGHT: f32 = 360.0;
const CURVE_HEIGHT: f32 = 150.0;
const CURVE_MIN_WIDTH: f32 = 280.0;
const CURVE_MAX_WIDTH: f32 = 520.0;
const CURVE_HANDLE_RADIUS: f32 = 5.0;
const CURVE_X_EPSILON: f32 = 0.01;
const MAX_CUSTOM_POINTS: usize = 32;
const MAX_BATCH_AXES: usize = 4;
const MAX_BATCH_MEMBERS: usize = 256;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BatchDiagnosticSpec {
    pub(crate) label: &'static str,
    pub(crate) unit: &'static str,
}

pub(crate) const BATCH_DIAGNOSTICS: [BatchDiagnosticSpec; 13] = [
    BatchDiagnosticSpec {
        label: "SFC CAPE",
        unit: "J/kg",
    },
    BatchDiagnosticSpec {
        label: "SFC CINH",
        unit: "J/kg",
    },
    BatchDiagnosticSpec {
        label: "SFC LCL",
        unit: "m AGL",
    },
    BatchDiagnosticSpec {
        label: "ML CAPE",
        unit: "J/kg",
    },
    BatchDiagnosticSpec {
        label: "MU CAPE",
        unit: "J/kg",
    },
    BatchDiagnosticSpec {
        label: "PWAT",
        unit: "in",
    },
    BatchDiagnosticSpec {
        label: "DCAPE",
        unit: "J/kg",
    },
    BatchDiagnosticSpec {
        label: "0-3 km lapse",
        unit: "°C/km",
    },
    BatchDiagnosticSpec {
        label: "0-1 km SRH",
        unit: "m²/s²",
    },
    BatchDiagnosticSpec {
        label: "0-3 km SRH",
        unit: "m²/s²",
    },
    BatchDiagnosticSpec {
        label: "0-6 km shear",
        unit: "kt",
    },
    BatchDiagnosticSpec {
        label: "STP (cin)",
        unit: "",
    },
    BatchDiagnosticSpec {
        label: "Supercell composite",
        unit: "",
    },
];

#[derive(Clone, Debug)]
pub(crate) struct BatchDiagnosticValues {
    pub(crate) values: Vec<f64>,
}

#[derive(Clone, Debug)]
struct BatchAxisEditor {
    kind: CorrectionBatchAxisKind,
    start: f64,
    end: f64,
    count: usize,
}

#[derive(Clone, Debug)]
struct EvaluatedBatchMember {
    member: BatchMember,
    diagnostics: Option<BatchDiagnosticValues>,
    failure: Option<String>,
}

#[derive(Clone, Debug)]
struct BatchRunResults {
    base_recipe: CorrectionRecipe,
    axes: Vec<BatchAxis>,
    members: Vec<EvaluatedBatchMember>,
    summaries: Vec<Option<MinMedianMax>>,
    failed_members: usize,
}

#[derive(Debug, Default)]
struct BatchExperimentState {
    selected_level: usize,
    axes: Vec<BatchAxisEditor>,
    results: Option<BatchRunResults>,
    status: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum VariableEditor {
    Thermal,
    Moisture,
    Wind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlendShapeChoice {
    Cosine,
    Linear,
    LayerConstantUpperCosine,
    Custom,
}

impl BlendShapeChoice {
    const ALL: [Self; 4] = [
        Self::Cosine,
        Self::Linear,
        Self::LayerConstantUpperCosine,
        Self::Custom,
    ];

    fn from_shape(shape: &BlendShape) -> Self {
        match shape {
            BlendShape::Cosine => Self::Cosine,
            BlendShape::Linear => Self::Linear,
            BlendShape::LayerConstantUpperCosine { .. } => Self::LayerConstantUpperCosine,
            BlendShape::Custom { .. } => Self::Custom,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Cosine => "Cosine taper",
            Self::Linear => "Linear taper",
            Self::LayerConstantUpperCosine => "Layer core + top cosine",
            Self::Custom => "Custom W(z)",
        }
    }
}

/// UI-only state. The correction recipe itself remains owned by the current
/// sounding and is intentionally not persisted across source changes.
#[derive(Debug, Default)]
pub(crate) struct SoundingCorrectionEditor {
    open: bool,
    expand_first_level_on_open: bool,
    expanded_levels: BTreeSet<usize>,
    expanded_variables: BTreeSet<(usize, VariableEditor)>,
    qc_details_open: bool,
    selected_curve_point: Option<(usize, VariableEditor, usize)>,
    convective_preview_recipe: Option<CorrectionRecipe>,
    convective_preview: Option<CorrectionResult>,
    file_status: Option<(bool, String)>,
    batch: BatchExperimentState,
}

/// Compact host contract. Direct recipe edits are already reflected in the
/// supplied [`CorrectionRecipe`]; the flags let the sounding host invalidate
/// diagnostics, provenance, or any external undo affordance exactly once.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CorrectionEditorOutcome {
    pub(crate) recipe_changed: bool,
    pub(crate) reset: bool,
    pub(crate) preview_requested: bool,
    pub(crate) adjustment_applied: bool,
    pub(crate) adjustment_undone: bool,
    pub(crate) imported_raw: Option<ImportedRawSounding>,
}

impl SoundingCorrectionEditor {
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.expand_first_level_on_open = true;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.expand_first_level_on_open = false;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Drop selection/collapse state when a new sounding takes ownership.
    /// This does not retain or manufacture any correction values.
    pub(crate) fn reset_source_state(&mut self) {
        self.expanded_levels.clear();
        self.expanded_variables.clear();
        self.qc_details_open = false;
        self.selected_curve_point = None;
        self.convective_preview_recipe = None;
        self.convective_preview = None;
        self.batch = BatchExperimentState::default();
    }

    /// Show the correction editor as an independent egui window. Because this
    /// function only receives an [`egui::Context`] (not the sounding pane's
    /// `Ui`), opening it cannot reduce the plot's `available_size()`.
    ///
    /// `current_result` may be the host's already-built result. Passing `None`
    /// asks the editor to run the lightweight correction/QC engine for its
    /// badges; either way, changes are returned for the host to rebuild the
    /// expensive sounding diagnostics once.
    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        source: &SoundingColumn,
        source_context: &CorrectionSourceContext,
        recipe: &mut CorrectionRecipe,
        current_result: Option<&CorrectionResult>,
        batch_evaluator: &mut dyn FnMut(&CorrectionRecipe) -> Result<BatchDiagnosticValues, String>,
    ) -> CorrectionEditorOutcome {
        if !self.open {
            return CorrectionEditorOutcome::default();
        }
        if self.expand_first_level_on_open {
            if recipe.levels.len() == 1 {
                self.expanded_levels.insert(0);
            }
            self.expand_first_level_on_open = false;
        }

        let fallback_result = current_result
            .is_none()
            .then(|| apply_correction_recipe(source, recipe));
        let result = current_result
            .or(fallback_result.as_ref())
            .expect("fallback result exists when the host did not supply one");
        let mut outcome = CorrectionEditorOutcome::default();
        let mut preview_requested = false;
        let mut apply_requested = false;
        let mut undo_requested = false;
        let mut open = self.open;

        egui::Window::new("Manual sounding correction")
            .id(egui::Id::new(EDITOR_ID))
            .open(&mut open)
            .default_size(egui::vec2(
                EDITOR_DEFAULT_WIDTH,
                EDITOR_DEFAULT_HEIGHT,
            ))
            .min_size(egui::vec2(EDITOR_MIN_WIDTH, EDITOR_MIN_HEIGHT))
            .resizable(true)
            .show(ctx, |ui| {
                self.file_menu(
                    ui,
                    source,
                    source_context,
                    recipe,
                    result,
                    &mut outcome,
                );
                self.toolbar(
                    ui,
                    source,
                    recipe,
                    result,
                    &mut outcome,
                    &mut preview_requested,
                    &mut apply_requested,
                    &mut undo_requested,
                );

                if self.qc_details_open {
                    qc_details(ui, &result.issues);
                }
                if let Some(preview) = self.convective_preview.as_ref() {
                    convective_preview_details(ui, preview);
                }

                self.batch_ui(ui, recipe, batch_evaluator);

                ui.separator();
                if recipe.levels.is_empty() {
                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.strong("No correction levels");
                            ui.weak(
                                "Add a surface or elevated native level. The source sounding is unchanged until a target is enabled.",
                            );
                        });
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("sounding-correction-levels")
                        .auto_shrink([false, false])
                        .max_height(ui.available_height().max(120.0))
                        .show(ui, |ui| {
                            self.levels_ui(ui, source, recipe, &result.issues, &mut outcome);
                        });
                }
            });

        self.open = open;
        if outcome.recipe_changed {
            self.convective_preview_recipe = None;
            self.convective_preview = None;
        }
        if preview_requested {
            self.convective_preview_recipe = Some(recipe.clone());
            self.convective_preview = Some(preview_convective_adjustment(source, recipe));
            outcome.preview_requested = true;
        }
        if apply_requested {
            recipe.convective_adjustment.enabled = true;
            self.convective_preview_recipe = None;
            self.convective_preview = None;
            outcome.recipe_changed = true;
            outcome.adjustment_applied = true;
        }
        if undo_requested {
            recipe.convective_adjustment.enabled = false;
            self.convective_preview_recipe = None;
            self.convective_preview = None;
            outcome.recipe_changed = true;
            outcome.adjustment_undone = true;
        }
        if self
            .batch
            .results
            .as_ref()
            .is_some_and(|results| results.base_recipe != *recipe)
        {
            self.batch.results = None;
            self.batch.status = Some("Recipe changed; run the experiment again.".to_owned());
        }
        if outcome.recipe_changed || outcome.preview_requested {
            ctx.request_repaint();
        }
        outcome
    }

    fn file_menu(
        &mut self,
        ui: &mut egui::Ui,
        source: &SoundingColumn,
        source_context: &CorrectionSourceContext,
        recipe: &mut CorrectionRecipe,
        result: &CorrectionResult,
        outcome: &mut CorrectionEditorOutcome,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Save correction project…").clicked() {
                    ui.close();
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Save source-bound correction project")
                        .add_filter("BowEcho correction project", &["json"])
                        .set_file_name("sounding-correction.json")
                        .save_file()
                    else {
                        return;
                    };
                    let saved = (|| {
                        let provenance = CorrectionSourceProvenance::from_column(
                            source,
                            source_context.clone(),
                        )
                        .map_err(|error| error.to_string())?;
                        let mut bundle = SoundingCorrectionBundle::new(
                            provenance,
                            recipe.clone(),
                            Some(chrono::Utc::now().to_rfc3339()),
                        );
                        bundle
                            .record_application(result)
                            .map_err(|error| error.to_string())?;
                        let bytes = bundle
                            .to_json_pretty()
                            .map_err(|error| error.to_string())?;
                        fs::write(&path, bytes).map_err(|error| error.to_string())?;
                        Ok::<_, String>(())
                    })();
                    self.file_status = Some(match saved {
                        Ok(()) => (true, format!("Saved project: {}", path.display())),
                        Err(error) => (false, format!("Could not save project: {error}")),
                    });
                }
                if ui.button("Load correction project…").clicked() {
                    ui.close();
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Load source-bound correction project")
                        .add_filter("BowEcho correction project", &["json"])
                        .pick_file()
                    else {
                        return;
                    };
                    let loaded = fs::read(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| {
                            source_bound_bundle_from_json(&bytes, source)
                                .map_err(|error| error.to_string())
                        });
                    match loaded {
                        Ok(bundle) => {
                            *recipe = bundle.recipe;
                            self.convective_preview_recipe = None;
                            self.convective_preview = None;
                            self.expanded_levels.clear();
                            self.expanded_variables.clear();
                            self.batch.results = None;
                            outcome.recipe_changed = true;
                            self.file_status = Some((
                                true,
                                format!(
                                    "Loaded {} for this exact source profile.",
                                    path.display()
                                ),
                            ));
                        }
                        Err(error) => {
                            self.file_status = Some((
                                false,
                                format!(
                                    "Project was not applied; {}: {error}",
                                    path.display()
                                ),
                            ));
                        }
                    }
                }
                ui.separator();
                if ui.button("Export corrected CSV…").clicked() {
                    ui.close();
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Export corrected sounding CSV")
                        .add_filter("CSV", &["csv"])
                        .set_file_name("corrected-sounding.csv")
                        .save_file()
                    else {
                        return;
                    };
                    let exported = corrected_profile_csv(&result.column)
                        .map_err(|error| error.to_string())
                        .and_then(|text| fs::write(&path, text).map_err(|error| error.to_string()));
                    self.file_status = Some(match exported {
                        Ok(()) => (true, format!("Exported corrected CSV: {}", path.display())),
                        Err(error) => (false, format!("Could not export CSV: {error}")),
                    });
                }
                if ui.button("Export corrected SHARPpy RAW…").clicked() {
                    ui.close();
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Export corrected SPC/SHARPpy RAW sounding")
                        .add_filter("SHARPpy RAW", &["txt", "raw"])
                        .set_file_name("corrected-sounding.txt")
                        .save_file()
                    else {
                        return;
                    };
                    let exported = sharppy_raw_text(&result.column, None)
                        .map_err(|error| error.to_string())
                        .and_then(|text| fs::write(&path, text).map_err(|error| error.to_string()));
                    self.file_status = Some(match exported {
                        Ok(()) => (
                            true,
                            format!("Exported SHARPpy RAW: {}", path.display()),
                        ),
                        Err(error) => (false, format!("Could not export RAW: {error}")),
                    });
                }
                ui.separator();
                if ui.button("Import SHARPpy RAW as new sounding…").clicked() {
                    ui.close();
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Import SPC/SHARPpy RAW sounding")
                        .add_filter("SHARPpy RAW", &["txt", "raw"])
                        .pick_file()
                    else {
                        return;
                    };
                    let imported = fs::read_to_string(&path)
                        .map_err(|error| error.to_string())
                        .and_then(|text| {
                            parse_sharppy_raw_text(&text).map_err(|error| error.to_string())
                        });
                    match imported {
                        Ok(imported) => {
                            let skipped = imported.skipped_missing_rows;
                            outcome.imported_raw = Some(imported);
                            self.file_status = Some((
                                true,
                                if skipped == 0 {
                                    format!("Imported RAW sounding: {}", path.display())
                                } else {
                                    format!(
                                        "Imported RAW sounding: {}; skipped {skipped} missing row(s).",
                                        path.display()
                                    )
                                },
                            ));
                        }
                        Err(error) => {
                            self.file_status =
                                Some((false, format!("Could not import RAW: {error}")));
                        }
                    }
                }
            });
            ui.weak("projects are fingerprint-bound; exports use the evaluated corrected column");
        });
        if let Some((success, message)) = &self.file_status {
            ui.colored_label(
                if *success {
                    egui::Color32::from_rgb(120, 210, 150)
                } else {
                    egui::Color32::LIGHT_RED
                },
                message,
            );
        }
        ui.separator();
    }

    fn batch_ui(
        &mut self,
        ui: &mut egui::Ui,
        recipe: &CorrectionRecipe,
        evaluator: &mut dyn FnMut(&CorrectionRecipe) -> Result<BatchDiagnosticValues, String>,
    ) {
        egui::CollapsingHeader::new("Batch experiment")
            .id_salt((EDITOR_ID, "batch-experiment"))
            .default_open(false)
            .show(ui, |ui| {
                if recipe.levels.is_empty() {
                    ui.weak("Add a correction row before defining batch axes.");
                    return;
                }

                self.batch.selected_level = self
                    .batch
                    .selected_level
                    .min(recipe.levels.len().saturating_sub(1));
                let previous_level = self.batch.selected_level;
                ui.horizontal_wrapped(|ui| {
                    ui.label("Correction row");
                    egui::ComboBox::from_id_salt((EDITOR_ID, "batch-level"))
                        .selected_text(format!("#{:02}", self.batch.selected_level + 1))
                        .show_ui(ui, |ui| {
                            for index in 0..recipe.levels.len() {
                                ui.selectable_value(
                                    &mut self.batch.selected_level,
                                    index,
                                    format!("#{:02} · {}", index + 1, level_summary(&recipe.levels[index])),
                                );
                            }
                        });
                    ui.weak("1–4 numeric axes; Cartesian order is deterministic");
                });
                if self.batch.selected_level != previous_level {
                    self.batch.axes.clear();
                    self.batch.results = None;
                    self.batch.status = None;
                }

                let level_index = self.batch.selected_level;
                let available = available_batch_axes(recipe, level_index);
                if self.batch.axes.is_empty()
                    || self
                        .batch
                        .axes
                        .iter()
                        .any(|axis| !available.contains(&axis.kind))
                {
                    self.batch.axes = available
                        .first()
                        .and_then(|kind| default_batch_axis(recipe, level_index, *kind).ok())
                        .into_iter()
                        .collect();
                    self.batch.results = None;
                }

                let mut remove_axis = None;
                let mut configuration_changed = false;
                for axis_index in 0..self.batch.axes.len() {
                    let old_kind = self.batch.axes[axis_index].kind;
                    let mut selected_kind = old_kind;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("Axis {}", axis_index + 1));
                        egui::ComboBox::from_id_salt((EDITOR_ID, "batch-axis", axis_index))
                            .selected_text(batch_axis_label(recipe, level_index, old_kind))
                            .show_ui(ui, |ui| {
                                for kind in &available {
                                    let used_elsewhere = self
                                        .batch
                                        .axes
                                        .iter()
                                        .enumerate()
                                        .any(|(index, axis)| index != axis_index && axis.kind == *kind);
                                    ui.add_enabled_ui(!used_elsewhere, |ui| {
                                        ui.selectable_value(
                                            &mut selected_kind,
                                            *kind,
                                            batch_axis_label(recipe, level_index, *kind),
                                        );
                                    });
                                }
                            });
                        if selected_kind != old_kind
                            && let Ok(replacement) =
                                default_batch_axis(recipe, level_index, selected_kind)
                        {
                            self.batch.axes[axis_index] = replacement;
                            configuration_changed = true;
                        }
                        let axis = &mut self.batch.axes[axis_index];
                        ui.label("from");
                        configuration_changed |= ui
                            .add(egui::DragValue::new(&mut axis.start).speed(batch_axis_speed(axis.kind)))
                            .changed();
                        ui.label("to");
                        configuration_changed |= ui
                            .add(egui::DragValue::new(&mut axis.end).speed(batch_axis_speed(axis.kind)))
                            .changed();
                        ui.label("samples");
                        configuration_changed |= ui
                            .add(egui::DragValue::new(&mut axis.count).range(2..=16))
                            .changed();
                        ui.weak(batch_axis_unit(recipe, level_index, axis.kind));
                        if self.batch.axes.len() > 1 && ui.small_button("Remove").clicked() {
                            remove_axis = Some(axis_index);
                        }
                    });
                }
                if let Some(index) = remove_axis {
                    self.batch.axes.remove(index);
                    configuration_changed = true;
                }
                if ui
                    .add_enabled(
                        self.batch.axes.len() < MAX_BATCH_AXES
                            && available.len() > self.batch.axes.len(),
                        egui::Button::new("+ Axis"),
                    )
                    .clicked()
                    && let Some(kind) = available
                        .iter()
                        .find(|kind| !self.batch.axes.iter().any(|axis| axis.kind == **kind))
                    && let Ok(axis) = default_batch_axis(recipe, level_index, *kind)
                {
                    self.batch.axes.push(axis);
                    configuration_changed = true;
                }
                if configuration_changed {
                    self.batch.results = None;
                    self.batch.status = None;
                }

                let member_count = self
                    .batch
                    .axes
                    .iter()
                    .try_fold(1usize, |total, axis| total.checked_mul(axis.count));
                ui.horizontal_wrapped(|ui| {
                    let within_limit = member_count.is_some_and(|count| count <= MAX_BATCH_MEMBERS);
                    let count_text = member_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "overflow".to_owned());
                    ui.label(format!("Members: {count_text} / {MAX_BATCH_MEMBERS}"));
                    if !within_limit {
                        ui.colored_label(
                            egui::Color32::LIGHT_RED,
                            "Reduce samples; the 256-member hard cap is enforced before evaluation.",
                        );
                    }
                    if ui
                        .add_enabled(within_limit, egui::Button::new("Run experiment"))
                        .clicked()
                    {
                        match run_batch_experiment(
                            recipe,
                            level_index,
                            &self.batch.axes,
                            evaluator,
                        ) {
                            Ok(results) => {
                                self.batch.status = Some(format!(
                                    "Evaluated {} member(s); {} member(s) failed correction/QC/analysis.",
                                    results.members.len(), results.failed_members
                                ));
                                self.batch.results = Some(results);
                            }
                            Err(error) => {
                                self.batch.results = None;
                                self.batch.status = Some(error);
                            }
                        }
                    }
                });
                if let Some(status) = &self.batch.status {
                    ui.weak(status);
                }

                let Some(results) = self.batch.results.clone() else {
                    return;
                };
                ui.separator();
                egui::ScrollArea::horizontal()
                    .id_salt((EDITOR_ID, "batch-summary"))
                    .show(ui, |ui| {
                        egui::Grid::new((EDITOR_ID, "batch-summary-grid"))
                            .striped(true)
                            .min_col_width(92.0)
                            .show(ui, |ui| {
                                ui.strong("Diagnostic");
                                ui.strong("Min");
                                ui.strong("Median");
                                ui.strong("Max");
                                ui.strong("Finite / failed");
                                ui.end_row();
                                for (spec, summary) in
                                    BATCH_DIAGNOSTICS.iter().zip(&results.summaries)
                                {
                                    ui.label(if spec.unit.is_empty() {
                                        spec.label.to_owned()
                                    } else {
                                        format!("{} ({})", spec.label, spec.unit)
                                    });
                                    if let Some(summary) = summary {
                                        ui.label(format_batch_number(summary.minimum));
                                        ui.label(format_batch_number(summary.median));
                                        ui.label(format_batch_number(summary.maximum));
                                        ui.label(format!(
                                            "{} / {}",
                                            summary.finite_count, summary.ignored_non_finite
                                        ));
                                    } else {
                                        ui.label("—");
                                        ui.label("—");
                                        ui.label("—");
                                        ui.label(format!("0 / {}", results.members.len()));
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                if ui.button("Export member CSV…").clicked() {
                    let Some(path) = rfd::FileDialog::new()
                        .set_title("Export sounding batch members")
                        .add_filter("CSV", &["csv"])
                        .set_file_name("sounding-correction-batch.csv")
                        .save_file()
                    else {
                        return;
                    };
                    let exported = batch_members_csv(&results)
                        .and_then(|csv| fs::write(&path, csv).map_err(|error| error.to_string()));
                    self.batch.status = Some(match exported {
                        Ok(()) => format!("Exported batch members: {}", path.display()),
                        Err(error) => format!("Could not export batch members: {error}"),
                    });
                }
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        source: &SoundingColumn,
        recipe: &mut CorrectionRecipe,
        result: &CorrectionResult,
        outcome: &mut CorrectionEditorOutcome,
        preview_requested: &mut bool,
        apply_requested: &mut bool,
        undo_requested: &mut bool,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("DISPLAY-ONLY ANALYST CORRECTION")
                    .strong()
                    .color(egui::Color32::from_rgb(255, 190, 70)),
            );
            ui.weak("source files are never modified");
            ui.separator();

            if ui.button("+ Level").clicked() {
                let max_agl = source_max_agl_m(source);
                let target = recipe
                    .levels
                    .last()
                    .map(|level| level.target_agl_m + 500.0)
                    .unwrap_or(0.0)
                    .clamp(0.0, max_agl);
                recipe.levels.push(CorrectionLevel::at_height(target));
                let index = recipe.levels.len() - 1;
                self.expanded_levels.insert(index);
                outcome.recipe_changed = true;
            }
            if ui.small_button("+ Surface").clicked() {
                recipe.levels.push(CorrectionLevel::at_height(0.0));
                let index = recipe.levels.len() - 1;
                self.expanded_levels.insert(index);
                outcome.recipe_changed = true;
            }
            if ui
                .add_enabled(
                    !recipe.levels.is_empty() || recipe.convective_adjustment.enabled,
                    egui::Button::new("Reset original"),
                )
                .on_hover_text("Remove all edits and rebuild from the untouched source column")
                .clicked()
            {
                *recipe = CorrectionRecipe::default();
                self.reset_source_state();
                outcome.recipe_changed = true;
                outcome.reset = true;
            }

            ui.separator();
            if qc_summary_button(ui, &result.issues, self.qc_details_open).clicked() {
                self.qc_details_open = !self.qc_details_open;
            }
        });

        ui.horizontal_wrapped(|ui| {
            ui.strong("Dry convective adjustment");
            ui.weak("optional, previewed, reversible");
            ui.label("surface exemption");
            if ui
                .add(
                    egui::DragValue::new(
                        &mut recipe
                            .convective_adjustment
                            .protected_surface_depth_m,
                    )
                    .range(0.0..=2_000.0)
                    .speed(10.0)
                    .suffix(" m AGL"),
                )
                .on_hover_text(
                    "The dry stability repair begins above this analyst-defined surface layer",
                )
                .changed()
            {
                outcome.recipe_changed = true;
            }
            if ui
                .button("Preview")
                .on_hover_text(
                    "Preview dry static-stability repair and enthalpy conservation without changing the recipe",
                )
                .clicked()
            {
                *preview_requested = true;
            }
            let preview_can_apply = self.convective_preview.as_ref().is_some_and(|preview| {
                preview.convective_adjustment.applied && !preview.has_errors()
            }) && self.convective_preview_recipe.as_ref() == Some(recipe)
                && !recipe.convective_adjustment.enabled;
            if ui
                .add_enabled(preview_can_apply, egui::Button::new("Apply preview"))
                .on_hover_text(
                    "Commit the previewed dry adjustment; rebuild from the source to undo it",
                )
                .clicked()
            {
                *apply_requested = true;
            }
            if ui
                .add_enabled(
                    recipe.convective_adjustment.enabled,
                    egui::Button::new("Undo adjustment"),
                )
                .on_hover_text("Disable the adjustment and rebuild from the untouched source")
                .clicked()
            {
                *undo_requested = true;
            }
        });
    }

    fn levels_ui(
        &mut self,
        ui: &mut egui::Ui,
        source: &SoundingColumn,
        recipe: &mut CorrectionRecipe,
        issues: &[QcIssue],
        outcome: &mut CorrectionEditorOutcome,
    ) {
        let mut remove = None;
        let mut move_level = None;
        let count = recipe.levels.len();
        ui.weak(
            "Rows are resolved by native height. The arrows only organize this editor; row order does not change the corrected profile.",
        );
        ui.add_space(4.0);
        for index in 0..count {
            let mut expanded = self.expanded_levels.contains(&index);
            let summary = level_summary(&recipe.levels[index]);
            let row_issues = issues_for_recipe_level(issues, index);
            egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(if expanded { "\u{25be}" } else { "\u{25b8}" }).small())
                            .on_hover_text(if expanded { "Collapse level" } else { "Expand level" })
                            .clicked()
                        {
                            expanded = !expanded;
                        }
                        ui.label(egui::RichText::new(format!("#{:02}", index + 1)).monospace().strong());
                        ui.label(summary);
                        qc_badge(ui, &row_issues);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                remove = Some(index);
                            }
                            if ui
                                .add_enabled(index + 1 < count, egui::Button::new("\u{2193}").small())
                                .on_hover_text(
                                    "Move this row down for editor organization; calculations are order-independent",
                                )
                                .clicked()
                            {
                                move_level = Some((index, index + 1));
                            }
                            if ui
                                .add_enabled(index > 0, egui::Button::new("\u{2191}").small())
                                .on_hover_text(
                                    "Move this row up for editor organization; calculations are order-independent",
                                )
                                .clicked()
                            {
                                move_level = Some((index, index - 1));
                            }
                        });
                    });
                    if expanded {
                        self.level_body(ui, source, &mut recipe.levels[index], index, &row_issues, outcome);
                    }
                });
            if expanded {
                self.expanded_levels.insert(index);
            } else {
                self.expanded_levels.remove(&index);
            }
            ui.add_space(5.0);
        }
        if let Some((from, to)) = move_level {
            recipe.levels.swap(from, to);
            self.expanded_levels.clear();
            self.expanded_variables.clear();
            self.selected_curve_point = None;
            outcome.recipe_changed = true;
        }
        if let Some(index) = remove {
            recipe.levels.remove(index);
            self.expanded_levels.clear();
            self.expanded_variables.clear();
            self.selected_curve_point = None;
            outcome.recipe_changed = true;
        }
    }
}

fn available_batch_axes(
    recipe: &CorrectionRecipe,
    level_index: usize,
) -> Vec<CorrectionBatchAxisKind> {
    CorrectionBatchAxisKind::ALL
        .into_iter()
        .filter(|kind| correction_batch_axis_value(recipe, level_index, *kind).is_ok())
        .collect()
}

fn batch_axis_label(
    recipe: &CorrectionRecipe,
    level_index: usize,
    kind: CorrectionBatchAxisKind,
) -> String {
    let coordinate = recipe.levels.get(level_index).and_then(|level| match kind {
        CorrectionBatchAxisKind::ThermalTarget => {
            level.thermal.as_ref().map(|edit| match edit.target {
                ThermalTarget::TemperatureC(_) => "T target (°C)",
                ThermalTarget::PotentialTemperatureK(_) => "θ target (K)",
            })
        }
        CorrectionBatchAxisKind::MoistureTarget => {
            level.moisture.as_ref().map(|edit| match edit.target {
                MoistureTarget::DewpointC(_) => "Td target (°C)",
                MoistureTarget::MixingRatioGKg(_) => "Mixing ratio target (g/kg)",
                MoistureTarget::SpecificHumidityGKg(_) => "Specific humidity target (g/kg)",
            })
        }
        _ => None,
    });
    coordinate.unwrap_or_else(|| kind.label()).to_owned()
}

fn batch_axis_unit(
    recipe: &CorrectionRecipe,
    level_index: usize,
    kind: CorrectionBatchAxisKind,
) -> &'static str {
    recipe
        .levels
        .get(level_index)
        .map_or(kind.unit(), |level| match kind {
            CorrectionBatchAxisKind::ThermalTarget => {
                level.thermal.as_ref().map_or("", |edit| match edit.target {
                    ThermalTarget::TemperatureC(_) => "°C",
                    ThermalTarget::PotentialTemperatureK(_) => "K",
                })
            }
            CorrectionBatchAxisKind::MoistureTarget => {
                level
                    .moisture
                    .as_ref()
                    .map_or("", |edit| match edit.target {
                        MoistureTarget::DewpointC(_) => "°C",
                        MoistureTarget::MixingRatioGKg(_)
                        | MoistureTarget::SpecificHumidityGKg(_) => "g/kg",
                    })
            }
            _ => kind.unit(),
        })
}

fn default_batch_axis(
    recipe: &CorrectionRecipe,
    level_index: usize,
    kind: CorrectionBatchAxisKind,
) -> Result<BatchAxisEditor, String> {
    let center = correction_batch_axis_value(recipe, level_index, kind)
        .map_err(|error| error.to_string())?;
    let delta = match kind {
        CorrectionBatchAxisKind::LevelHeight => 250.0,
        CorrectionBatchAxisKind::ThermalTarget => 2.0,
        CorrectionBatchAxisKind::ThermalDepth
        | CorrectionBatchAxisKind::MoistureDepth
        | CorrectionBatchAxisKind::WindDepth => 250.0,
        CorrectionBatchAxisKind::MoistureTarget => 1.0,
        CorrectionBatchAxisKind::WindDirection => 15.0,
        CorrectionBatchAxisKind::WindSpeed => 5.0,
        CorrectionBatchAxisKind::WindU | CorrectionBatchAxisKind::WindV => 2.5,
    };
    let nonnegative = matches!(
        kind,
        CorrectionBatchAxisKind::LevelHeight
            | CorrectionBatchAxisKind::ThermalDepth
            | CorrectionBatchAxisKind::MoistureDepth
            | CorrectionBatchAxisKind::WindSpeed
            | CorrectionBatchAxisKind::WindDepth
    );
    Ok(BatchAxisEditor {
        kind,
        start: if nonnegative {
            (center - delta).max(0.0)
        } else {
            center - delta
        },
        end: center + delta,
        count: 3,
    })
}

fn batch_axis_speed(kind: CorrectionBatchAxisKind) -> f64 {
    match kind {
        CorrectionBatchAxisKind::LevelHeight
        | CorrectionBatchAxisKind::ThermalDepth
        | CorrectionBatchAxisKind::MoistureDepth
        | CorrectionBatchAxisKind::WindDepth => 25.0,
        CorrectionBatchAxisKind::WindDirection => 5.0,
        _ => 0.5,
    }
}

fn batch_plan_axes(
    recipe: &CorrectionRecipe,
    level_index: usize,
    editors: &[BatchAxisEditor],
) -> Result<Vec<BatchAxis>, String> {
    if !(1..=MAX_BATCH_AXES).contains(&editors.len()) {
        return Err(format!("Select between 1 and {MAX_BATCH_AXES} batch axes."));
    }
    let mut axes = Vec::with_capacity(editors.len());
    for editor in editors {
        correction_batch_axis_value(recipe, level_index, editor.kind)
            .map_err(|error| error.to_string())?;
        if !editor.start.is_finite() || !editor.end.is_finite() {
            return Err(format!("{} bounds must be finite.", editor.kind.label()));
        }
        if !(2..=16).contains(&editor.count) {
            return Err(format!(
                "{} must contain between 2 and 16 samples.",
                editor.kind.label()
            ));
        }
        let denominator = (editor.count - 1) as f64;
        let values = (0..editor.count)
            .map(|index| {
                let fraction = index as f64 / denominator;
                BatchValue::Number(editor.start + (editor.end - editor.start) * fraction)
            })
            .collect();
        axes.push(BatchAxis {
            key: editor.kind.key().to_owned(),
            label: batch_axis_label(recipe, level_index, editor.kind),
            unit: Some(batch_axis_unit(recipe, level_index, editor.kind).to_owned()),
            values,
        });
    }
    Ok(axes)
}

fn run_batch_experiment(
    recipe: &CorrectionRecipe,
    level_index: usize,
    editors: &[BatchAxisEditor],
    evaluator: &mut dyn FnMut(&CorrectionRecipe) -> Result<BatchDiagnosticValues, String>,
) -> Result<BatchRunResults, String> {
    let axes = batch_plan_axes(recipe, level_index, editors)?;
    let planned =
        cartesian_batch_members(&axes, MAX_BATCH_MEMBERS).map_err(|error| error.to_string())?;
    let mut members = Vec::with_capacity(planned.len());
    for member in planned {
        let mut member_recipe = recipe.clone();
        let application = (|| {
            for selection in &member.selections {
                let kind = CorrectionBatchAxisKind::from_key(&selection.axis_key)
                    .ok_or_else(|| format!("unknown batch axis `{}`", selection.axis_key))?;
                let BatchValue::Number(value) = selection.value else {
                    return Err(format!("{} received a non-numeric selection", kind.label()));
                };
                apply_correction_batch_axis(&mut member_recipe, level_index, kind, value)
                    .map_err(|error| error.to_string())?;
            }
            let diagnostics = evaluator(&member_recipe)?;
            if diagnostics.values.len() != BATCH_DIAGNOSTICS.len() {
                return Err(format!(
                    "analysis returned {} diagnostics; expected {}",
                    diagnostics.values.len(),
                    BATCH_DIAGNOSTICS.len()
                ));
            }
            Ok(diagnostics)
        })();
        match application {
            Ok(diagnostics) => members.push(EvaluatedBatchMember {
                member,
                diagnostics: Some(diagnostics),
                failure: None,
            }),
            Err(error) => members.push(EvaluatedBatchMember {
                member,
                diagnostics: None,
                failure: Some(error),
            }),
        }
    }
    let failed_members = members
        .iter()
        .filter(|member| member.failure.is_some())
        .count();
    let summaries = (0..BATCH_DIAGNOSTICS.len())
        .map(|diagnostic_index| {
            let values = members
                .iter()
                .map(|member| {
                    member
                        .diagnostics
                        .as_ref()
                        .and_then(|values| values.values.get(diagnostic_index))
                        .copied()
                        .unwrap_or(f64::NAN)
                })
                .collect::<Vec<_>>();
            finite_min_median_max(&values)
        })
        .collect();
    Ok(BatchRunResults {
        base_recipe: recipe.clone(),
        axes,
        members,
        summaries,
        failed_members,
    })
}

fn format_batch_number(value: f64) -> String {
    if !value.is_finite() {
        "—".to_owned()
    } else if value.abs() >= 1_000.0 {
        format!("{value:.0}")
    } else if value.abs() >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn batch_members_csv(results: &BatchRunResults) -> Result<String, String> {
    let mut output = String::new();
    output.push_str("member_id");
    for axis in &results.axes {
        write!(output, ",{}", csv_field(&axis.label)).map_err(|error| error.to_string())?;
    }
    output.push_str(",status");
    for spec in BATCH_DIAGNOSTICS {
        let header = if spec.unit.is_empty() {
            spec.label.to_owned()
        } else {
            format!("{} ({})", spec.label, spec.unit)
        };
        write!(output, ",{}", csv_field(&header)).map_err(|error| error.to_string())?;
    }
    output.push('\n');

    for evaluated in &results.members {
        output.push_str(&evaluated.member.display_id());
        for selection in &evaluated.member.selections {
            let value = match &selection.value {
                BatchValue::Number(value) if value.is_finite() => value.to_string(),
                BatchValue::Number(_) => String::new(),
                BatchValue::Text(value) => value.clone(),
                BatchValue::Bool(value) => value.to_string(),
            };
            write!(output, ",{}", csv_field(&value)).map_err(|error| error.to_string())?;
        }
        let status = evaluated.failure.as_deref().unwrap_or("ok");
        write!(output, ",{}", csv_field(status)).map_err(|error| error.to_string())?;
        for index in 0..BATCH_DIAGNOSTICS.len() {
            let value = evaluated
                .diagnostics
                .as_ref()
                .and_then(|diagnostics| diagnostics.values.get(index))
                .filter(|value| value.is_finite())
                .map(|value| value.to_string())
                .unwrap_or_default();
            write!(output, ",{value}").map_err(|error| error.to_string())?;
        }
        output.push('\n');
    }
    Ok(output)
}

fn source_max_agl_m(source: &SoundingColumn) -> f64 {
    let Some(surface) = source.height_m_msl.first().copied() else {
        return 0.0;
    };
    source
        .height_m_msl
        .iter()
        .copied()
        .filter(|height| height.is_finite())
        .map(|height| height - surface)
        .fold(0.0, f64::max)
        .max(0.0)
}

fn nearest_native_level(source: &SoundingColumn, target_agl_m: f64) -> Option<usize> {
    let surface = *source.height_m_msl.first()?;
    let target_msl = surface + target_agl_m.max(0.0);
    source
        .height_m_msl
        .iter()
        .enumerate()
        .filter(|(_, height)| height.is_finite())
        .min_by(|(_, left), (_, right)| {
            (*left - target_msl)
                .abs()
                .total_cmp(&(*right - target_msl).abs())
        })
        .map(|(index, _)| index)
}

fn default_thermal_edit(source: &SoundingColumn, anchor: usize) -> ThermalEdit {
    let temperature = ThermalTarget::TemperatureC(source.temperature_c[anchor]);
    let target = temperature
        .converted(
            ThermalMode::PotentialTemperature,
            source.pressure_hpa[anchor],
        )
        .unwrap_or(temperature);
    ThermalEdit::new(target)
}

fn default_wind_edit(source: &SoundingColumn, anchor: usize) -> WindEdit {
    let components = WindTarget::UV {
        u_ms: source.u_ms[anchor],
        v_ms: source.v_ms[anchor],
    };
    WindEdit::new(
        components
            .converted(WindMode::DirectionSpeed)
            .unwrap_or(components),
    )
}

fn level_summary(level: &CorrectionLevel) -> String {
    let mut parts = Vec::with_capacity(3);
    if let Some(edit) = &level.thermal {
        parts.push(thermal_summary(edit));
    }
    if let Some(edit) = &level.moisture {
        parts.push(moisture_summary(edit));
    }
    if let Some(edit) = &level.wind {
        parts.push(wind_summary(edit));
    }
    let edits = if parts.is_empty() {
        "inactive".to_owned()
    } else {
        parts.join("  |  ")
    };
    format!("{:.0} m AGL  ·  {edits}", level.target_agl_m)
}

fn thermal_summary(edit: &ThermalEdit) -> String {
    match edit.target {
        ThermalTarget::TemperatureC(value) => {
            format!("T {value:.1} °C / {:.0} m", edit.blend.depth_m)
        }
        ThermalTarget::PotentialTemperatureK(value) => {
            format!("θ {value:.1} K / {:.0} m", edit.blend.depth_m)
        }
    }
}

fn moisture_summary(edit: &MoistureEdit) -> String {
    match edit.target {
        MoistureTarget::DewpointC(value) => {
            format!("Td {value:.1} °C / {:.0} m", edit.blend.depth_m)
        }
        MoistureTarget::MixingRatioGKg(value) => {
            format!("r {value:.2} g/kg / {:.0} m", edit.blend.depth_m)
        }
        MoistureTarget::SpecificHumidityGKg(value) => {
            format!("q {value:.2} g/kg / {:.0} m", edit.blend.depth_m)
        }
    }
}

fn wind_summary(edit: &WindEdit) -> String {
    match edit.target {
        WindTarget::DirectionSpeed {
            direction_deg,
            speed_kt,
        } => format!(
            "wind {:03.0}/{:.0} kt / {:.0} m",
            direction_deg.rem_euclid(360.0),
            speed_kt,
            edit.blend.depth_m
        ),
        WindTarget::UV { u_ms, v_ms } => format!(
            "wind U {u_ms:.1} V {v_ms:.1} m/s / {:.0} m",
            edit.blend.depth_m
        ),
    }
}

fn thermal_target_value_ui(ui: &mut egui::Ui, target: &mut ThermalTarget) -> bool {
    match target {
        ThermalTarget::TemperatureC(value) => ui
            .add(
                egui::DragValue::new(value)
                    .range(-150.0..=80.0)
                    .speed(0.1)
                    .suffix(" °C"),
            )
            .changed(),
        ThermalTarget::PotentialTemperatureK(value) => ui
            .add(
                egui::DragValue::new(value)
                    .range(100.0..=600.0)
                    .speed(0.1)
                    .suffix(" K"),
            )
            .changed(),
    }
}

fn moisture_target_value_ui(ui: &mut egui::Ui, target: &mut MoistureTarget) -> bool {
    match target {
        MoistureTarget::DewpointC(value) => ui
            .add(
                egui::DragValue::new(value)
                    .range(-150.0..=80.0)
                    .speed(0.1)
                    .suffix(" °C"),
            )
            .changed(),
        MoistureTarget::MixingRatioGKg(value) | MoistureTarget::SpecificHumidityGKg(value) => ui
            .add(
                egui::DragValue::new(value)
                    .range(0.0..=1_000.0)
                    .speed(0.05)
                    .suffix(" g/kg"),
            )
            .changed(),
    }
}

fn wind_target_value_ui(ui: &mut egui::Ui, target: &mut WindTarget) -> bool {
    let mut changed = false;
    match target {
        WindTarget::DirectionSpeed {
            direction_deg,
            speed_kt,
        } => {
            ui.label("direction");
            changed |= ui
                .add(
                    egui::DragValue::new(direction_deg)
                        .range(0.0..=360.0)
                        .speed(1.0)
                        .suffix("°"),
                )
                .changed();
            ui.label("speed");
            changed |= ui
                .add(
                    egui::DragValue::new(speed_kt)
                        .range(0.0..=300.0)
                        .speed(0.5)
                        .suffix(" kt"),
                )
                .changed();
        }
        WindTarget::UV { u_ms, v_ms } => {
            ui.label("U");
            changed |= ui
                .add(
                    egui::DragValue::new(u_ms)
                        .range(-200.0..=200.0)
                        .speed(0.25)
                        .suffix(" m/s"),
                )
                .changed();
            ui.label("V");
            changed |= ui
                .add(
                    egui::DragValue::new(v_ms)
                        .range(-200.0..=200.0)
                        .speed(0.25)
                        .suffix(" m/s"),
                )
                .changed();
        }
    }
    changed
}

fn blend_extent_label(extent: BlendExtent) -> &'static str {
    match extent {
        BlendExtent::SymmetricLocal => "Local (± depth)",
        BlendExtent::UpwardFromAnchor => "Anchor upward",
        BlendExtent::SurfaceLayer => "Surface core + upper blend",
    }
}

fn shape_from_choice(choice: BlendShapeChoice, previous: &BlendShape) -> BlendShape {
    match choice {
        BlendShapeChoice::Cosine => BlendShape::Cosine,
        BlendShapeChoice::Linear => BlendShape::Linear,
        BlendShapeChoice::LayerConstantUpperCosine => BlendShape::LayerConstantUpperCosine {
            taper_fraction: 0.25,
        },
        BlendShapeChoice::Custom => BlendShape::Custom {
            points: [0.0, 0.25, 0.5, 0.75, 1.0]
                .into_iter()
                .map(|x| BlendControlPoint::new(x, normalized_shape_weight(previous, x)))
                .collect(),
        },
    }
}

fn normalized_shape_weight(shape: &BlendShape, x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    match shape {
        BlendShape::Cosine => 0.5 * (1.0 + (std::f64::consts::PI * x).cos()),
        BlendShape::Linear => 1.0 - x,
        BlendShape::LayerConstantUpperCosine { taper_fraction } => {
            let taper = taper_fraction.clamp(0.01, 1.0);
            let taper_start = 1.0 - taper;
            if x <= taper_start {
                1.0
            } else {
                let fraction = (x - taper_start) / taper;
                0.5 * (1.0 + (std::f64::consts::PI * fraction).cos())
            }
        }
        BlendShape::Custom { points } => {
            let mut points = points.clone();
            points.sort_by(|left, right| left.x.total_cmp(&right.x));
            if let Some(first) = points.first()
                && x <= first.x
            {
                return first.y.clamp(0.0, 1.0);
            }
            for pair in points.windows(2) {
                if x <= pair[1].x {
                    let width = pair[1].x - pair[0].x;
                    if width <= f64::EPSILON {
                        return pair[1].y.clamp(0.0, 1.0);
                    }
                    let alpha = (x - pair[0].x) / width;
                    return (pair[0].y + alpha * (pair[1].y - pair[0].y)).clamp(0.0, 1.0);
                }
            }
            points.last().map_or(0.0, |point| point.y.clamp(0.0, 1.0))
        }
    }
}

fn severity_counts(issues: &[QcIssue]) -> (usize, usize, usize) {
    let mut advisories = 0;
    let mut warnings = 0;
    let mut errors = 0;
    for issue in issues {
        match issue.severity {
            QcSeverity::Advisory => advisories += 1,
            QcSeverity::Warning => warnings += 1,
            QcSeverity::Error => errors += 1,
        }
    }
    (advisories, warnings, errors)
}

fn severity_color(severity: QcSeverity) -> egui::Color32 {
    match severity {
        QcSeverity::Advisory => egui::Color32::from_rgb(95, 200, 235),
        QcSeverity::Warning => egui::Color32::from_rgb(255, 190, 70),
        QcSeverity::Error => egui::Color32::from_rgb(255, 95, 95),
    }
}

fn qc_summary_button(ui: &mut egui::Ui, issues: &[QcIssue], selected: bool) -> egui::Response {
    let (advisories, warnings, errors) = severity_counts(issues);
    let (label, color) = if errors > 0 {
        (
            format!("QC: {errors} error · {warnings} warning · {advisories} note"),
            severity_color(QcSeverity::Error),
        )
    } else if warnings > 0 {
        (
            format!("QC: {warnings} warning · {advisories} note"),
            severity_color(QcSeverity::Warning),
        )
    } else if advisories > 0 {
        (
            format!("QC: {advisories} note"),
            severity_color(QcSeverity::Advisory),
        )
    } else {
        (
            "QC: clear".to_owned(),
            egui::Color32::from_rgb(95, 210, 125),
        )
    };
    ui.add(egui::Button::selectable(
        selected,
        egui::RichText::new(label).strong().color(color),
    ))
    .on_hover_text("Show post-edit physical-consistency checks")
}

fn issues_for_recipe_level(issues: &[QcIssue], level_index: usize) -> Vec<&QcIssue> {
    issues
        .iter()
        .filter(|issue| issue.correction_index == Some(level_index))
        .collect()
}

fn qc_badge(ui: &mut egui::Ui, issues: &[&QcIssue]) {
    let worst = issues
        .iter()
        .map(|issue| issue.severity)
        .max_by_key(|severity| match severity {
            QcSeverity::Advisory => 0,
            QcSeverity::Warning => 1,
            QcSeverity::Error => 2,
        });
    if let Some(severity) = worst {
        let label = match severity {
            QcSeverity::Advisory => "QC note",
            QcSeverity::Warning => "QC warning",
            QcSeverity::Error => "QC error",
        };
        ui.label(
            egui::RichText::new(label)
                .small()
                .strong()
                .color(severity_color(severity)),
        )
        .on_hover_text(
            issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
}

fn qc_details(ui: &mut egui::Ui, issues: &[QcIssue]) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Post-edit QC");
                ui.weak(
                    "warnings remain visible; supersaturation is never silently projected or clipped",
                );
            });
            if issues.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(95, 210, 125),
                    "No QC issues in the current corrected profile.",
                );
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("sounding-correction-qc-details")
                .max_height(150.0)
                .show(ui, |ui| {
                    for issue in issues {
                        issue_line(ui, issue);
                    }
                });
        });
}

fn issue_line(ui: &mut egui::Ui, issue: &QcIssue) {
    ui.horizontal_wrapped(|ui| {
        let severity = match issue.severity {
            QcSeverity::Advisory => "NOTE",
            QcSeverity::Warning => "WARN",
            QcSeverity::Error => "ERROR",
        };
        ui.label(
            egui::RichText::new(severity)
                .small()
                .strong()
                .color(severity_color(issue.severity)),
        );
        ui.label(format!(
            "{}: {}",
            issue_kind_label(issue.kind),
            issue.message
        ));
        if issue.kind == QcIssueKind::Supersaturation {
            ui.weak("Revise T/θ or Td/r/q; BowEcho does not hide this with a clamp.");
        }
    });
}

fn issue_kind_label(kind: QcIssueKind) -> &'static str {
    match kind {
        QcIssueKind::Structural => "profile structure",
        QcIssueKind::InvalidTarget => "target",
        QcIssueKind::InvalidMoisture => "moisture",
        QcIssueKind::Supersaturation => "supersaturation",
        QcIssueKind::DryStaticInstability => "dry stability",
        QcIssueKind::WindShearKink => "wind seam",
        QcIssueKind::ConvectiveAdjustmentAborted => "dry adjustment",
    }
}

fn convective_preview_details(ui: &mut egui::Ui, preview: &CorrectionResult) {
    let report = &preview.convective_adjustment;
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Dry-adjustment preview");
                if report.applied && !preview.has_errors() {
                    ui.colored_label(
                        egui::Color32::from_rgb(95, 210, 125),
                        format!(
                            "{} levels · {} mixed blocks · enthalpy residual {:.2e}",
                            report.adjusted_levels,
                            report.mixed_blocks,
                            report.relative_enthalpy_residual
                        ),
                    );
                } else {
                    ui.colored_label(
                        severity_color(QcSeverity::Error),
                        report
                            .aborted_reason
                            .as_deref()
                            .unwrap_or("Preview did not produce a committable adjustment"),
                    );
                }
            });
            if !preview.issues.is_empty() {
                for issue in &preview.issues {
                    issue_line(ui, issue);
                }
            }
        });
}

impl SoundingCorrectionEditor {
    fn level_body(
        &mut self,
        ui: &mut egui::Ui,
        source: &SoundingColumn,
        level: &mut CorrectionLevel,
        level_index: usize,
        issues: &[&QcIssue],
        outcome: &mut CorrectionEditorOutcome,
    ) {
        ui.separator();
        let max_agl = source_max_agl_m(source);
        ui.horizontal_wrapped(|ui| {
            ui.label("Anchor");
            if ui
                .add(
                    egui::DragValue::new(&mut level.target_agl_m)
                        .range(0.0..=max_agl)
                        .speed(25.0)
                        .suffix(" m AGL"),
                )
                .on_hover_text("Snaps to the nearest native model level")
                .changed()
            {
                outcome.recipe_changed = true;
            }
            if let Some(anchor) = nearest_native_level(source, level.target_agl_m) {
                let surface = source.height_m_msl.first().copied().unwrap_or(0.0);
                ui.weak(format!(
                    "native {:.0} m AGL · {:.0} hPa · {:.1} °C / {:.1} °C",
                    source.height_m_msl[anchor] - surface,
                    source.pressure_hpa[anchor],
                    source.temperature_c[anchor],
                    source.dewpoint_c[anchor]
                ));
            } else {
                ui.colored_label(egui::Color32::LIGHT_RED, "No valid native anchor");
            }
        });

        let Some(anchor) = nearest_native_level(source, level.target_agl_m) else {
            return;
        };
        let pressure_hpa = source.pressure_hpa[anchor];

        self.thermal_section(
            ui,
            level_index,
            &mut level.thermal,
            source,
            anchor,
            pressure_hpa,
            outcome,
        );
        self.moisture_section(
            ui,
            level_index,
            &mut level.moisture,
            source,
            anchor,
            pressure_hpa,
            outcome,
        );
        self.wind_section(ui, level_index, &mut level.wind, source, anchor, outcome);

        if !issues.is_empty() {
            ui.add_space(3.0);
            for issue in issues {
                issue_line(ui, issue);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn thermal_section(
        &mut self,
        ui: &mut egui::Ui,
        level_index: usize,
        edit: &mut Option<ThermalEdit>,
        source: &SoundingColumn,
        anchor: usize,
        pressure_hpa: f64,
        outcome: &mut CorrectionEditorOutcome,
    ) {
        let key = (level_index, VariableEditor::Thermal);
        let mut expanded = self.expanded_variables.contains(&key);
        let mut enabled = edit.is_some();
        ui.horizontal(|ui| {
            if ui.checkbox(&mut enabled, "Thermal").changed() {
                *edit = enabled.then(|| default_thermal_edit(source, anchor));
                outcome.recipe_changed = true;
                expanded = enabled;
            }
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(if expanded { "\u{25be}" } else { "\u{25b8}" }).small(),
                )
                .clicked()
            {
                expanded = !expanded;
            }
            if let Some(edit) = edit.as_ref() {
                ui.weak(thermal_summary(edit));
            }
        });
        if expanded {
            self.expanded_variables.insert(key);
        } else {
            self.expanded_variables.remove(&key);
        }
        if !expanded {
            return;
        }
        let Some(edit) = edit.as_mut() else {
            return;
        };

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(7))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Input coordinate");
                    for (mode, label) in [
                        (ThermalMode::Temperature, "T (°C)"),
                        (ThermalMode::PotentialTemperature, "θ (K)"),
                    ] {
                        if ui
                            .selectable_label(edit.target.mode() == mode, label)
                            .clicked()
                            && edit.target.mode() != mode
                            && let Some(converted) = edit.target.converted(mode, pressure_hpa)
                        {
                            edit.target = converted;
                            outcome.recipe_changed = true;
                        }
                    }
                    ui.label("target");
                    outcome.recipe_changed |= thermal_target_value_ui(ui, &mut edit.target);
                });
                ui.weak(match edit.target.mode() {
                    ThermalMode::Temperature => {
                        "T mode blends a temperature increment (ΔT). Switching coordinates preserves the anchor air state."
                    }
                    ThermalMode::PotentialTemperature => {
                        "θ mode blends a potential-temperature increment (Δθ). The default mixed-layer shape holds it constant through the core and tapers only at the top."
                    }
                });
                outcome.recipe_changed |= self.blend_spec_ui(
                    ui,
                    level_index,
                    VariableEditor::Thermal,
                    &mut edit.blend,
                );
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn moisture_section(
        &mut self,
        ui: &mut egui::Ui,
        level_index: usize,
        edit: &mut Option<MoistureEdit>,
        source: &SoundingColumn,
        anchor: usize,
        pressure_hpa: f64,
        outcome: &mut CorrectionEditorOutcome,
    ) {
        let key = (level_index, VariableEditor::Moisture);
        let mut expanded = self.expanded_variables.contains(&key);
        let mut enabled = edit.is_some();
        ui.horizontal(|ui| {
            if ui.checkbox(&mut enabled, "Moisture").changed() {
                *edit = enabled.then(|| {
                    MoistureEdit::new(MoistureTarget::DewpointC(source.dewpoint_c[anchor]))
                });
                outcome.recipe_changed = true;
                expanded = enabled;
            }
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(if expanded { "\u{25be}" } else { "\u{25b8}" }).small(),
                )
                .clicked()
            {
                expanded = !expanded;
            }
            if let Some(edit) = edit.as_ref() {
                ui.weak(moisture_summary(edit));
            }
        });
        if expanded {
            self.expanded_variables.insert(key);
        } else {
            self.expanded_variables.remove(&key);
        }
        if !expanded {
            return;
        }
        let Some(edit) = edit.as_mut() else {
            return;
        };

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(7))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Input coordinate");
                    for (mode, label) in [
                        (MoistureMode::Dewpoint, "Td (°C)"),
                        (MoistureMode::MixingRatio, "r (g/kg)"),
                        (MoistureMode::SpecificHumidity, "q (g/kg)"),
                    ] {
                        if ui
                            .selectable_label(edit.target.mode() == mode, label)
                            .clicked()
                            && edit.target.mode() != mode
                            && let Some(converted) = edit.target.converted(mode, pressure_hpa)
                        {
                            edit.target = converted;
                            outcome.recipe_changed = true;
                        }
                    }
                    ui.label("target");
                    outcome.recipe_changed |= moisture_target_value_ui(ui, &mut edit.target);
                });
                ui.weak(
                    "BowEcho converts every input to specific humidity q, blends q, then diagnoses Td. Supersaturation is reported, never silently clipped.",
                );
                outcome.recipe_changed |= self.blend_spec_ui(
                    ui,
                    level_index,
                    VariableEditor::Moisture,
                    &mut edit.blend,
                );
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn wind_section(
        &mut self,
        ui: &mut egui::Ui,
        level_index: usize,
        edit: &mut Option<WindEdit>,
        source: &SoundingColumn,
        anchor: usize,
        outcome: &mut CorrectionEditorOutcome,
    ) {
        let key = (level_index, VariableEditor::Wind);
        let mut expanded = self.expanded_variables.contains(&key);
        let mut enabled = edit.is_some();
        ui.horizontal(|ui| {
            if ui.checkbox(&mut enabled, "Wind").changed() {
                *edit = enabled.then(|| default_wind_edit(source, anchor));
                outcome.recipe_changed = true;
                expanded = enabled;
            }
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(if expanded { "\u{25be}" } else { "\u{25b8}" }).small(),
                )
                .clicked()
            {
                expanded = !expanded;
            }
            if let Some(edit) = edit.as_ref() {
                ui.weak(wind_summary(edit));
            }
        });
        if expanded {
            self.expanded_variables.insert(key);
        } else {
            self.expanded_variables.remove(&key);
        }
        if !expanded {
            return;
        }
        let Some(edit) = edit.as_mut() else {
            return;
        };

        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(7))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Input coordinate");
                    for (mode, label) in [
                        (WindMode::DirectionSpeed, "Dir / speed"),
                        (WindMode::Components, "U / V"),
                    ] {
                        if ui
                            .selectable_label(edit.target.mode() == mode, label)
                            .clicked()
                            && edit.target.mode() != mode
                            && let Some(converted) = edit.target.converted(mode)
                        {
                            edit.target = converted;
                            outcome.recipe_changed = true;
                        }
                    }
                    outcome.recipe_changed |= wind_target_value_ui(ui, &mut edit.target);
                });
                ui.weak(
                    "Direction/speed is an input view only. BowEcho converts to earth-relative U/V, blends both components with one shared weight, then derives direction/speed.",
                );
                outcome.recipe_changed |= self.blend_spec_ui(
                    ui,
                    level_index,
                    VariableEditor::Wind,
                    &mut edit.blend,
                );
            });
    }

    fn blend_spec_ui(
        &mut self,
        ui: &mut egui::Ui,
        level_index: usize,
        variable: VariableEditor,
        spec: &mut BlendSpec,
    ) -> bool {
        let mut changed = false;
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label("Blend domain");
            egui::ComboBox::from_id_salt((EDITOR_ID, level_index, variable, "extent"))
                .selected_text(blend_extent_label(spec.extent))
                .show_ui(ui, |ui| {
                    for extent in [
                        BlendExtent::SymmetricLocal,
                        BlendExtent::UpwardFromAnchor,
                        BlendExtent::SurfaceLayer,
                    ] {
                        changed |= ui
                            .selectable_value(&mut spec.extent, extent, blend_extent_label(extent))
                            .changed();
                    }
                });
            ui.label("depth");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut spec.depth_m)
                        .range(0.0..=20_000.0)
                        .speed(25.0)
                        .suffix(" m"),
                )
                .changed();

            let old_choice = BlendShapeChoice::from_shape(&spec.shape);
            let mut choice = old_choice;
            egui::ComboBox::from_id_salt((EDITOR_ID, level_index, variable, "shape"))
                .selected_text(choice.label())
                .show_ui(ui, |ui| {
                    for candidate in BlendShapeChoice::ALL {
                        ui.selectable_value(&mut choice, candidate, candidate.label());
                    }
                });
            if choice != old_choice {
                spec.shape = shape_from_choice(choice, &spec.shape);
                self.selected_curve_point = None;
                changed = true;
            }
        });

        match &mut spec.shape {
            BlendShape::LayerConstantUpperCosine { taper_fraction } => {
                ui.horizontal(|ui| {
                    ui.label("Top taper");
                    let mut percent = *taper_fraction * 100.0;
                    if ui
                        .add(
                            egui::Slider::new(&mut percent, 1.0..=100.0)
                                .suffix("% of depth")
                                .integer(),
                        )
                        .changed()
                    {
                        *taper_fraction = percent / 100.0;
                        changed = true;
                    }
                    ui.weak("full increment below the cap; cosine taper only near its top");
                });
            }
            BlendShape::Custom { points } => {
                let mut curve = points
                    .iter()
                    .map(|point| CurvePoint::new(point.x as f32, point.y as f32))
                    .collect::<Vec<_>>();
                let selection_key = (level_index, variable);
                let mut selected = self.selected_curve_point.and_then(
                    |(selected_level, selected_variable, selected_point)| {
                        (selected_level, selected_variable)
                            .eq(&selection_key)
                            .then_some(selected_point)
                    },
                );
                if custom_curve_editor(
                    ui,
                    (EDITOR_ID, level_index, variable, "custom-curve"),
                    &mut curve,
                    &mut selected,
                ) {
                    *points = curve
                        .iter()
                        .map(|point| {
                            BlendControlPoint::new(f64::from(point.x), f64::from(point.weight))
                        })
                        .collect();
                    changed = true;
                }
                self.selected_curve_point = selected.map(|point| (level_index, variable, point));
            }
            BlendShape::Cosine | BlendShape::Linear => {}
        }
        if changed {
            *spec = spec.normalized();
        }
        changed
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CurvePoint {
    x: f32,
    weight: f32,
}

impl CurvePoint {
    const fn new(x: f32, weight: f32) -> Self {
        Self { x, weight }
    }
}

fn canonical_curve_points(points: &mut Vec<CurvePoint>) {
    points.retain(|point| point.x.is_finite() && point.weight.is_finite());
    for point in points.iter_mut() {
        point.x = point.x.clamp(0.0, 1.0);
        point.weight = point.weight.clamp(0.0, 1.0);
    }
    points.sort_by(|left, right| left.x.total_cmp(&right.x));

    if points.len() < 2 {
        *points = default_custom_curve();
        return;
    }

    // Custom W(z) always owns the complete normalized blend domain. Fixed
    // endpoints prevent an accidental nonzero correction outside that domain.
    points[0] = CurvePoint::new(0.0, 1.0);
    let last = points.len() - 1;
    points[last] = CurvePoint::new(1.0, 0.0);

    // Repair imported/legacy duplicates without changing point order. If a
    // recipe is too crowded to satisfy the normal drag epsilon, distribute it
    // uniformly; every resulting x is strictly increasing.
    let max_interior = ((1.0 / CURVE_X_EPSILON).floor() as usize).saturating_sub(1);
    if points.len().saturating_sub(2) > max_interior {
        let denominator = (points.len() - 1) as f32;
        for (index, point) in points.iter_mut().enumerate() {
            point.x = index as f32 / denominator;
        }
        return;
    }
    for index in 1..last {
        let min_x = points[index - 1].x + CURVE_X_EPSILON;
        points[index].x = points[index].x.max(min_x);
    }
    for index in (1..last).rev() {
        let max_x = points[index + 1].x - CURVE_X_EPSILON;
        points[index].x = points[index].x.min(max_x);
    }
}

fn default_custom_curve() -> Vec<CurvePoint> {
    vec![
        CurvePoint::new(0.0, 1.0),
        CurvePoint::new(0.5, 0.5),
        CurvePoint::new(1.0, 0.0),
    ]
}

fn insert_curve_point(points: &mut Vec<CurvePoint>) -> usize {
    canonical_curve_points(points);
    let (gap_index, _) = points
        .windows(2)
        .enumerate()
        .map(|(index, pair)| (index, pair[1].x - pair[0].x))
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .unwrap_or((0, 1.0));
    let left = points[gap_index];
    let right = points[gap_index + 1];
    let inserted = CurvePoint::new(0.5 * (left.x + right.x), 0.5 * (left.weight + right.weight));
    let index = gap_index + 1;
    points.insert(index, inserted);
    canonical_curve_points(points);
    index
}

fn remove_curve_point(points: &mut Vec<CurvePoint>, index: usize) -> bool {
    canonical_curve_points(points);
    if index == 0 || index + 1 >= points.len() {
        return false;
    }
    points.remove(index);
    canonical_curve_points(points);
    true
}

fn drag_curve_point(points: &mut [CurvePoint], index: usize, x: f32, weight: f32) -> bool {
    if index >= points.len() {
        return false;
    }
    let last = points.len().saturating_sub(1);
    let next = if index == 0 {
        CurvePoint::new(0.0, 1.0)
    } else if index == last {
        CurvePoint::new(1.0, 0.0)
    } else {
        let min_x = points[index - 1].x + CURVE_X_EPSILON;
        let max_x = points[index + 1].x - CURVE_X_EPSILON;
        if min_x > max_x {
            return false;
        }
        CurvePoint::new(x.clamp(min_x, max_x), weight.clamp(0.0, 1.0))
    };
    let changed = points[index] != next;
    points[index] = next;
    changed
}

fn curve_point_to_screen(rect: egui::Rect, point: CurvePoint) -> egui::Pos2 {
    egui::pos2(
        egui::lerp(rect.left()..=rect.right(), point.x),
        egui::lerp(rect.bottom()..=rect.top(), point.weight),
    )
}

fn curve_point_from_screen(rect: egui::Rect, position: egui::Pos2) -> CurvePoint {
    CurvePoint::new(
        ((position.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
        ((rect.bottom() - position.y) / rect.height()).clamp(0.0, 1.0),
    )
}

fn custom_curve_editor(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    points: &mut Vec<CurvePoint>,
    selected: &mut Option<usize>,
) -> bool {
    canonical_curve_points(points);
    let mut changed = false;

    ui.horizontal_wrapped(|ui| {
        ui.strong("Custom W(z)");
        ui.weak("x = normalized vertical coordinate; y = correction weight");
    });

    let width = ui.available_width().clamp(CURVE_MIN_WIDTH, CURVE_MAX_WIDTH);
    let (outer_rect, _) =
        ui.allocate_exact_size(egui::vec2(width, CURVE_HEIGHT + 28.0), egui::Sense::hover());
    let plot = egui::Rect::from_min_max(
        outer_rect.min + egui::vec2(34.0, 8.0),
        outer_rect.max - egui::vec2(8.0, 20.0),
    );
    let painter = ui.painter_at(outer_rect);
    painter.rect_filled(plot, 3.0, ui.visuals().extreme_bg_color);
    painter.rect_stroke(
        plot,
        3.0,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color),
        egui::StrokeKind::Inside,
    );
    for step in 0..=4 {
        let fraction = step as f32 / 4.0;
        let x = egui::lerp(plot.left()..=plot.right(), fraction);
        let y = egui::lerp(plot.bottom()..=plot.top(), fraction);
        let grid = egui::Stroke::new(0.5_f32, ui.visuals().faint_bg_color);
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            grid,
        );
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            grid,
        );
    }
    painter.text(
        egui::pos2(plot.center().x, outer_rect.bottom() - 2.0),
        egui::Align2::CENTER_BOTTOM,
        "normalized z  (0 = core/anchor, 1 = blend edge)",
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );
    painter.text(
        egui::pos2(outer_rect.left() + 2.0, plot.center().y),
        egui::Align2::LEFT_CENTER,
        "W(z)",
        egui::FontId::proportional(10.0),
        ui.visuals().weak_text_color(),
    );

    let line = points
        .iter()
        .copied()
        .map(|point| curve_point_to_screen(plot, point))
        .collect::<Vec<_>>();
    painter.add(egui::Shape::line(
        line,
        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(55, 210, 205)),
    ));

    let base_id = ui.make_persistent_id(id_salt);
    for index in 0..points.len() {
        let center = curve_point_to_screen(plot, points[index]);
        let hit = egui::Rect::from_center_size(center, egui::vec2(18.0, 18.0));
        let response = ui
            .interact(hit, base_id.with(index), egui::Sense::click_and_drag())
            .on_hover_text(if index == 0 || index + 1 == points.len() {
                "Safe endpoint: its normalized position and weight are fixed"
            } else {
                "Drag horizontally to change normalized height and vertically to change W(z)"
            });
        if response.clicked() {
            *selected = Some(index);
        }
        if response.dragged()
            && let Some(position) = response.interact_pointer_pos()
        {
            let candidate = curve_point_from_screen(plot, position);
            changed |= drag_curve_point(points, index, candidate.x, candidate.weight);
            *selected = Some(index);
        }
        let selected_here = *selected == Some(index);
        let fill = if selected_here {
            egui::Color32::WHITE
        } else if response.hovered() || response.dragged() {
            egui::Color32::from_rgb(255, 210, 95)
        } else {
            egui::Color32::from_rgb(55, 210, 205)
        };
        painter.circle_filled(
            curve_point_to_screen(plot, points[index]),
            CURVE_HANDLE_RADIUS,
            fill,
        );
    }

    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                points.len() < MAX_CUSTOM_POINTS,
                egui::Button::new("+ Point").small(),
            )
            .clicked()
        {
            *selected = Some(insert_curve_point(points));
            changed = true;
        }
        let removable = selected.is_some_and(|index| index > 0 && index + 1 < points.len());
        if ui
            .add_enabled(removable, egui::Button::new("Remove point").small())
            .clicked()
            && let Some(index) = *selected
        {
            changed |= remove_curve_point(points, index);
            *selected = None;
        }
        if ui.small_button("Reset curve").clicked() {
            *points = default_custom_curve();
            *selected = None;
            changed = true;
        }
        if let Some(index) = selected.filter(|index| *index < points.len()) {
            let point = points[index];
            ui.weak(format!("x {:.2}  W {:.2}", point.x, point.weight));
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_curve_normalization_has_safe_strict_endpoints() {
        let mut points = vec![
            CurvePoint::new(0.7, 1.4),
            CurvePoint::new(0.2, -0.2),
            CurvePoint::new(0.2, 0.4),
        ];
        canonical_curve_points(&mut points);
        assert_eq!(points.first(), Some(&CurvePoint::new(0.0, 1.0)));
        assert_eq!(points.last(), Some(&CurvePoint::new(1.0, 0.0)));
        assert!(points.windows(2).all(|pair| pair[0].x < pair[1].x));
        assert!(
            points
                .iter()
                .all(|point| (0.0..=1.0).contains(&point.weight))
        );
    }

    #[test]
    fn curve_insertion_uses_largest_gap_and_preserves_line() {
        let mut points = vec![
            CurvePoint::new(0.0, 1.0),
            CurvePoint::new(0.25, 0.8),
            CurvePoint::new(1.0, 0.0),
        ];
        let inserted = insert_curve_point(&mut points);
        assert_eq!(inserted, 2);
        assert!((points[inserted].x - 0.625).abs() < 1.0e-6);
        assert!((points[inserted].weight - 0.4).abs() < 1.0e-6);
    }

    #[test]
    fn fixed_curve_endpoints_cannot_be_dragged_or_removed() {
        let mut points = default_custom_curve();
        assert!(!drag_curve_point(&mut points, 0, 0.3, 0.2));
        assert_eq!(points[0], CurvePoint::new(0.0, 1.0));
        assert!(!remove_curve_point(&mut points, 0));
        let last = points.len() - 1;
        assert!(!remove_curve_point(&mut points, last));
    }

    #[test]
    fn interior_drag_is_bounded_by_neighbors_and_weight_domain() {
        let mut points = default_custom_curve();
        assert!(drag_curve_point(&mut points, 1, 2.0, -1.0));
        assert!((points[1].x - (1.0 - CURVE_X_EPSILON)).abs() < 1.0e-6);
        assert_eq!(points[1].weight, 0.0);
    }
}
