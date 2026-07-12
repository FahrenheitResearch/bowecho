//! Algorithm Truth Lab adapters and first-class UI for synthetic radar volumes.
//!
//! The observation operator can optionally retain Ideal (`I*`) and Measured
//! (`M*`) moment grids beside the canonical Presented products. This module
//! turns those exact, co-gridded stages into scorecards without regridding or
//! nearest-gate pairing. The pure report remains usable by tests and future
//! batch exporters; [`AlgorithmTruthLabState`] is the thin, cached egui shell.

use std::collections::BTreeMap;
use std::sync::Arc;

use eframe::egui;
use radar_core::{ElevationCut, MomentGrid, MomentType, RadarVolume};
use thiserror::Error;

use crate::wrf_radar_estimator::{
    SummaryStats, VelocityTruthSample, VelocityTruthScorecard, fold_velocity_f64,
    score_velocity_truth,
};

#[derive(Clone, Debug, Error, PartialEq)]
pub enum TruthLabError {
    #[error("synthetic radar cut {index} is outside {cut_count} cuts")]
    CutOutOfRange { index: usize, cut_count: usize },
    #[error("synthetic radar does not carry {name}; enable Ideal + Measured diagnostic moments")]
    MissingStageMoment { name: &'static str },
    #[error("{left} and {right} do not share exact gate/radial geometry")]
    GeometryMismatch {
        left: &'static str,
        right: &'static str,
    },
    #[error("dealiased velocity does not share exact geometry with IVEL")]
    DealiasedGeometryMismatch,
    #[error("synthetic radar stage diagnostics contain no finite paired samples")]
    NoPairedSamples,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StageComparisonScorecard {
    pub paired_samples: usize,
    pub ideal_minus_measured: SummaryStats,
    pub ideal_minus_presented: SummaryStats,
    pub measured_minus_presented: SummaryStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlgorithmTruthLabReport {
    pub site_id: String,
    pub volume_time_utc: String,
    pub cut_index: usize,
    pub elevation_deg: f32,
    pub velocity: VelocityTruthScorecard,
    pub stage_moments: BTreeMap<&'static str, StageComparisonScorecard>,
}

#[derive(Clone)]
struct StageDefinition {
    label: &'static str,
    short_name: &'static str,
    unit: &'static str,
    ideal: &'static str,
    measured: &'static str,
    presented: MomentType,
}

const STAGES: &[StageDefinition] = &[
    StageDefinition {
        label: "Reflectivity",
        short_name: "REF",
        unit: "dBZ",
        ideal: "IREF",
        measured: "MREF",
        presented: MomentType::Reflectivity,
    },
    StageDefinition {
        label: "Velocity",
        short_name: "VEL",
        unit: "m/s",
        ideal: "IVEL",
        measured: "MVEL",
        presented: MomentType::Velocity,
    },
    StageDefinition {
        label: "Spectrum width",
        short_name: "SW",
        unit: "m/s",
        ideal: "ISW",
        measured: "MSW",
        presented: MomentType::SpectrumWidth,
    },
    StageDefinition {
        label: "Differential reflectivity",
        short_name: "ZDR",
        unit: "dB",
        ideal: "IZDR",
        measured: "MZDR",
        presented: MomentType::DifferentialReflectivity,
    },
    StageDefinition {
        label: "Correlation coefficient",
        short_name: "RHO",
        unit: "",
        ideal: "IRHO",
        measured: "MRHO",
        presented: MomentType::CorrelationCoefficient,
    },
    StageDefinition {
        label: "Specific differential phase",
        short_name: "KDP",
        unit: "deg/km",
        ideal: "IKDP",
        measured: "MKDP",
        presented: MomentType::SpecificDifferentialPhase,
    },
];

/// Build exact-gate instrument-stage and velocity-dealias scorecards.
///
/// `dealiased_velocity`, when supplied, must be the production dealiaser's
/// output for this same cut. The expected folded value is computed from IVEL
/// and each source radial's stamped Nyquist, keeping estimator noise/bias in
/// the separate Ideal/Measured comparison instead of misclassifying it as a
/// folding-contract error.
pub fn build_algorithm_truth_lab_report(
    volume: &RadarVolume,
    cut_index: usize,
    dealiased_velocity: Option<&MomentGrid>,
    recovery_tolerance_mps: f64,
) -> Result<AlgorithmTruthLabReport, TruthLabError> {
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or(TruthLabError::CutOutOfRange {
            index: cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let ideal_velocity = required_stage_grid(cut, "IVEL")?;
    let dealiased_velocity = dealiased_velocity.or_else(|| stage_grid(cut, "DVEL"));
    if let Some(dealiased) = dealiased_velocity
        && !same_geometry(ideal_velocity, dealiased)
    {
        return Err(TruthLabError::DealiasedGeometryMismatch);
    }

    let mut velocity_samples = Vec::new();
    for row in 0..ideal_velocity.radial_count() {
        let radial_index = ideal_velocity.radial_indices[row];
        let nyquist = cut
            .radials
            .get(radial_index)
            .and_then(|radial| radial.nyquist_velocity_mps)
            .map(f64::from)
            .filter(|value| value.is_finite() && *value > 0.0);
        for gate in 0..ideal_velocity.gate_range.gate_count {
            let Some(truth) = ideal_velocity
                .scaled_value(row, gate)
                .map(f64::from)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            let folded = nyquist.map(|value| fold_velocity_f64(truth, value));
            let dealiased = dealiased_velocity
                .and_then(|grid| grid.scaled_value(row, gate))
                .map(f64::from)
                .filter(|value| value.is_finite());
            velocity_samples.push(VelocityTruthSample {
                true_velocity_mps: truth,
                folded_velocity_mps: folded,
                dealiased_velocity_mps: dealiased,
                nyquist_velocity_mps: nyquist,
            });
        }
    }
    if velocity_samples.is_empty() {
        return Err(TruthLabError::NoPairedSamples);
    }

    let mut stage_moments = BTreeMap::new();
    for definition in STAGES {
        let Some(ideal) = stage_grid(cut, definition.ideal) else {
            continue;
        };
        let Some(measured) = stage_grid(cut, definition.measured) else {
            continue;
        };
        let Some(presented) = cut.moments.get(&definition.presented) else {
            continue;
        };
        if !same_geometry(ideal, measured) {
            return Err(TruthLabError::GeometryMismatch {
                left: definition.ideal,
                right: definition.measured,
            });
        }
        if !same_geometry(ideal, presented) {
            return Err(TruthLabError::GeometryMismatch {
                left: definition.ideal,
                right: definition.label,
            });
        }
        stage_moments.insert(
            definition.label,
            score_stage_grids(ideal, measured, presented),
        );
    }

    Ok(AlgorithmTruthLabReport {
        site_id: volume.site.id.clone(),
        volume_time_utc: volume.volume_time.to_rfc3339(),
        cut_index,
        elevation_deg: cut.elevation_deg,
        velocity: score_velocity_truth(&velocity_samples, recovery_tolerance_mps),
        stage_moments,
    })
}

#[derive(Clone)]
struct SuppliedDealiasedVelocity {
    grid: Arc<MomentGrid>,
    label: String,
}

/// Cached state for the first-class Algorithm Truth Lab window.
///
/// Integration is intentionally small: call [`Self::open_with_volume`] with
/// the displayed synthetic volume, optionally call
/// [`Self::supply_dealiased_velocity`] with a production DVEL grid, and call
/// [`Self::show_window`] once per frame.
pub(crate) struct AlgorithmTruthLabState {
    pub(crate) open: bool,
    volume: Option<Arc<RadarVolume>>,
    selected_cut: usize,
    selected_moment: &'static str,
    recovery_tolerance_mps: f64,
    report_dirty: bool,
    report: Option<Result<AlgorithmTruthLabReport, TruthLabError>>,
    supplied_dealiased: BTreeMap<usize, SuppliedDealiasedVelocity>,
}

impl Default for AlgorithmTruthLabState {
    fn default() -> Self {
        Self {
            open: false,
            volume: None,
            selected_cut: 0,
            selected_moment: "Reflectivity",
            recovery_tolerance_mps: 1.0,
            report_dirty: false,
            report: None,
            supplied_dealiased: BTreeMap::new(),
        }
    }
}

impl AlgorithmTruthLabState {
    /// Open the lab on `volume`. Reopening the same `Arc` preserves the user's
    /// cut and supplied DVEL products; a different source starts a clean lab.
    pub(crate) fn open_with_volume(&mut self, volume: Arc<RadarVolume>) {
        let source_changed = self
            .volume
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &volume));
        if source_changed {
            self.volume = Some(volume);
            self.selected_cut = 0;
            self.selected_moment = "Reflectivity";
            self.supplied_dealiased.clear();
            self.refresh_report();
        } else if self.report.is_none() {
            self.refresh_report();
        }
        self.open = true;
    }

    /// Replace the source behind an already-open lab. `None` leaves the window
    /// open with an honest empty state instead of retaining stale scorecards.
    pub(crate) fn set_volume(&mut self, volume: Option<Arc<RadarVolume>>) {
        let unchanged = match (&self.volume, &volume) {
            (Some(current), Some(next)) => Arc::ptr_eq(current, next),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        self.volume = volume;
        self.selected_cut = 0;
        self.selected_moment = "Reflectivity";
        self.supplied_dealiased.clear();
        self.refresh_report();
    }

    /// Attach an exact-geometry output from the production dealiaser.
    ///
    /// The grid is keyed to a cut of the current source and is rejected before
    /// it can contaminate a scorecard if its radial/gate geometry differs from
    /// IVEL. Supplying it is optional: folding exposure is still scored without
    /// DVEL, while recovery/branch metrics are explicitly marked unavailable.
    pub(crate) fn supply_dealiased_velocity(
        &mut self,
        cut_index: usize,
        grid: Arc<MomentGrid>,
        label: impl Into<String>,
    ) -> Result<(), TruthLabError> {
        let volume = self.volume.as_ref().ok_or(TruthLabError::CutOutOfRange {
            index: cut_index,
            cut_count: 0,
        })?;
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(TruthLabError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let ideal = required_stage_grid(cut, "IVEL")?;
        if !same_geometry(ideal, &grid) {
            return Err(TruthLabError::DealiasedGeometryMismatch);
        }
        self.supplied_dealiased.insert(
            cut_index,
            SuppliedDealiasedVelocity {
                grid,
                label: label.into(),
            },
        );
        if self.selected_cut == cut_index {
            self.refresh_report();
        }
        Ok(())
    }

    pub(crate) fn show_window(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Algorithm Truth Lab")
            .id(egui::Id::new("bowecho_algorithm_truth_lab"))
            .open(&mut open)
            .default_width(860.0)
            .default_height(720.0)
            .min_width(650.0)
            .min_height(500.0)
            .resizable(true)
            .show(ctx, |ui| self.ui(ui));
        self.open = open;
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("algorithm_truth_lab_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.content_ui(ui));
    }

    fn content_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Algorithm Truth Lab");
            badge(
                ui,
                "SYNTHETIC GATE TRUTH",
                egui::Color32::from_rgb(118, 92, 214),
            );
        });
        ui.label(
            egui::RichText::new(
                "Exact, co-gridded audit of the radar observation operator: Ideal to Measured to Presented.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        scope_callout(ui);
        ui.add_space(8.0);

        let Some(volume) = self.volume.clone() else {
            empty_source_ui(ui);
            return;
        };

        self.source_ui(ui, &volume);
        ui.add_space(8.0);

        if self.report_dirty {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(214, 163, 45, 20))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new("Tolerance changed")
                                .strong()
                                .color(egui::Color32::from_rgb(225, 181, 74)),
                        );
                        ui.label("Recalculate to apply it to recovery and branch scoring.");
                    });
                });
            ui.add_space(6.0);
        }

        match self.report.clone() {
            Some(Ok(report)) => self.report_ui(ui, &volume, &report),
            Some(Err(error)) => error_ui(ui, &error),
            None => empty_source_ui(ui),
        }
    }

    fn source_ui(&mut self, ui: &mut egui::Ui, volume: &RadarVolume) {
        let synthetic = volume
            .metadata
            .forward_operator
            .as_deref()
            .is_some_and(|value| value.contains("BowEcho"));
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(&volume.site.id).strong().size(16.0));
                ui.label(volume.volume_time.format("%Y-%m-%d %H:%M:%SZ").to_string());
                ui.separator();
                ui.label(format!("{} cuts", volume.cuts.len()));
                if synthetic {
                    badge(
                        ui,
                        "BOWECHO SYNTHETIC",
                        egui::Color32::from_rgb(52, 165, 123),
                    );
                } else {
                    badge(
                        ui,
                        "SOURCE NOT MARKED SYNTHETIC",
                        egui::Color32::from_rgb(194, 139, 38),
                    );
                }
            });

            let old_cut = self.selected_cut;
            ui.horizontal_wrapped(|ui| {
                ui.label("Cut");
                let selected_text = volume
                    .cuts
                    .get(self.selected_cut)
                    .map(|cut| cut_label(self.selected_cut, cut))
                    .unwrap_or_else(|| "No cuts".to_owned());
                egui::ComboBox::from_id_salt("truth_lab_cut")
                    .selected_text(selected_text)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (index, cut) in volume.cuts.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.selected_cut,
                                index,
                                cut_label(index, cut),
                            );
                        }
                    });
                ui.separator();
                ui.label("Recovery tolerance");
                if ui
                    .add(
                        egui::DragValue::new(&mut self.recovery_tolerance_mps)
                            .range(0.0..=20.0)
                            .speed(0.1)
                            .suffix(" m/s"),
                    )
                    .changed()
                {
                    self.report_dirty = true;
                }
                if ui
                    .add_enabled(self.report_dirty, egui::Button::new("Recalculate"))
                    .clicked()
                {
                    self.refresh_report();
                }
            });
            if old_cut != self.selected_cut {
                self.refresh_report();
            }

            if let Some(operator) = &volume.metadata.forward_operator {
                ui.label(egui::RichText::new(operator).small().weak());
            }
        });
    }

    fn report_ui(
        &mut self,
        ui: &mut egui::Ui,
        volume: &RadarVolume,
        report: &AlgorithmTruthLabReport,
    ) {
        pipeline_ui(ui);
        ui.add_space(8.0);

        let available = report.stage_moments.len();
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Instrument-stage scorecards")
                    .strong()
                    .size(15.0),
            );
            badge(
                ui,
                &format!("{available}/{} MOMENT TRIPLES", STAGES.len()),
                if available == STAGES.len() {
                    egui::Color32::from_rgb(52, 165, 123)
                } else {
                    egui::Color32::from_rgb(194, 139, 38)
                },
            );
        });
        ui.label(
            egui::RichText::new(
                "Bias is signed first-minus-second. MAE, RMSE, p95, and max are absolute-error summaries over finite exact-gate triples.",
            )
            .small()
            .weak(),
        );
        ui.add_space(4.0);

        stage_summary_table(ui, report);
        ui.add_space(8.0);

        let available_labels = STAGES
            .iter()
            .filter(|stage| report.stage_moments.contains_key(stage.label))
            .map(|stage| stage.label)
            .collect::<Vec<_>>();
        if !available_labels.contains(&self.selected_moment)
            && let Some(first) = available_labels.first()
        {
            self.selected_moment = first;
        }
        if !available_labels.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Inspect moment");
                egui::ComboBox::from_id_salt("truth_lab_moment")
                    .selected_text(self.selected_moment)
                    .show_ui(ui, |ui| {
                        for label in &available_labels {
                            ui.selectable_value(&mut self.selected_moment, *label, *label);
                        }
                    });
            });
            if let Some(score) = report.stage_moments.get(self.selected_moment)
                && let Some(definition) = STAGES
                    .iter()
                    .find(|stage| stage.label == self.selected_moment)
            {
                stage_detail_cards(ui, definition, score);
            }
        }

        ui.add_space(10.0);
        let dealiased_label = self.dealiased_label(volume, report.cut_index);
        velocity_truth_ui(ui, &report.velocity, dealiased_label.as_deref());
    }

    fn dealiased_label(&self, volume: &RadarVolume, cut_index: usize) -> Option<String> {
        if let Some(supplied) = self.supplied_dealiased.get(&cut_index) {
            return Some(supplied.label.clone());
        }
        volume
            .cuts
            .get(cut_index)
            .and_then(|cut| stage_grid(cut, "DVEL"))
            .map(|_| "Stored DVEL diagnostic".to_owned())
    }

    fn refresh_report(&mut self) {
        self.report_dirty = false;
        let Some(volume) = self.volume.as_ref() else {
            self.report = None;
            return;
        };
        if volume.cuts.is_empty() {
            self.report = Some(Err(TruthLabError::CutOutOfRange {
                index: self.selected_cut,
                cut_count: 0,
            }));
            return;
        }
        self.selected_cut = self.selected_cut.min(volume.cuts.len() - 1);
        let dealiased = self
            .supplied_dealiased
            .get(&self.selected_cut)
            .map(|value| value.grid.as_ref());
        self.report = Some(build_algorithm_truth_lab_report(
            volume,
            self.selected_cut,
            dealiased,
            self.recovery_tolerance_mps,
        ));
    }
}

fn cut_label(index: usize, cut: &ElevationCut) -> String {
    let stage_count = STAGES
        .iter()
        .filter(|stage| {
            stage_grid(cut, stage.ideal).is_some()
                && stage_grid(cut, stage.measured).is_some()
                && cut.moments.contains_key(&stage.presented)
        })
        .count();
    format!(
        "#{:02}  {:.2} deg  |  {} rays  |  {stage_count}/{} stages",
        index + 1,
        cut.elevation_deg,
        cut.radials.len(),
        STAGES.len()
    )
}

fn scope_callout(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(76, 121, 190, 20))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(94, 141, 214, 95),
        ))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Scope").strong());
            ui.label(
                egui::RichText::new(
                    "This window scores synthetic gate moments and, when supplied, BowEcho's production velocity-dealias output. It does not invent VWP, GBVTD, wind-retrieval, or storm-analysis truth; those require separate model-truth adapters.",
                )
                .small(),
            );
        });
}

fn empty_source_ui(ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("No synthetic radar volume is attached").strong());
        ui.label(
            egui::RichText::new(
                "Open Algorithm Truth Lab from a simulated-radar frame. Observed radar volumes do not contain Ideal and Measured gate truth.",
            )
            .small()
            .weak(),
        );
    });
}

fn error_ui(ui: &mut egui::Ui, error: &TruthLabError) {
    let diagnostics_missing = matches!(error, TruthLabError::MissingStageMoment { .. });
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(194, 139, 38, 24))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(219, 164, 52, 120),
        ))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(if diagnostics_missing {
                    "Stage diagnostics are not embedded in this volume"
                } else {
                    "This cut cannot be scored"
                })
                .strong()
                .color(egui::Color32::from_rgb(230, 184, 76)),
            );
            ui.label(error.to_string());
            if diagnostics_missing {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("In WRF > Simulated radar > Instrument & propagation:")
                        .strong(),
                );
                ui.label("1. Turn on Physically coupled single-PRF moment estimator.");
                ui.label("2. Turn on Emit Ideal + Measured diagnostic moments.");
                ui.label("3. Refresh current frame(s), then reopen Algorithm Truth Lab.");
            }
        });
}

fn pipeline_ui(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        pipeline_stage(
            ui,
            "IDEAL",
            "I*",
            "Forward-scattered gate truth",
            egui::Color32::from_rgb(87, 156, 214),
        );
        ui.label(egui::RichText::new("->").strong().weak());
        pipeline_stage(
            ui,
            "MEASURED",
            "M*",
            "Pulse estimator + noise",
            egui::Color32::from_rgb(173, 119, 214),
        );
        ui.label(egui::RichText::new("->").strong().weak());
        pipeline_stage(
            ui,
            "PRESENTED",
            "REF / VEL / ...",
            "Texture, clutter, ambiguity, display",
            egui::Color32::from_rgb(67, 176, 132),
        );
    });
}

fn pipeline_stage(
    ui: &mut egui::Ui,
    title: &str,
    product: &str,
    detail: &str,
    color: egui::Color32,
) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            22,
        ))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.65)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_min_width(180.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong().color(color));
                ui.label(egui::RichText::new(product).monospace().strong());
            });
            ui.label(egui::RichText::new(detail).small().weak());
        });
}

fn stage_summary_table(ui: &mut egui::Ui, report: &AlgorithmTruthLabReport) {
    egui::ScrollArea::horizontal()
        .id_salt("truth_lab_stage_summary_scroll")
        .show(ui, |ui| {
            egui::Grid::new("truth_lab_stage_summary")
                .num_columns(5)
                .striped(true)
                .min_col_width(110.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Moment").strong());
                    ui.label(egui::RichText::new("Paired gates").strong());
                    ui.label(egui::RichText::new("I -> M MAE").strong());
                    ui.label(egui::RichText::new("M -> P MAE").strong());
                    ui.label(egui::RichText::new("I -> P RMSE").strong());
                    ui.end_row();
                    for stage in STAGES {
                        let Some(score) = report.stage_moments.get(stage.label) else {
                            continue;
                        };
                        ui.label(format!("{}  {}", stage.short_name, stage.label));
                        ui.label(score.paired_samples.to_string());
                        ui.label(format_stat(
                            score.ideal_minus_measured.mean_absolute,
                            stage.unit,
                        ));
                        ui.label(format_stat(
                            score.measured_minus_presented.mean_absolute,
                            stage.unit,
                        ));
                        ui.label(format_stat(score.ideal_minus_presented.rmse, stage.unit));
                        ui.end_row();
                    }
                });
        });
}

fn stage_detail_cards(
    ui: &mut egui::Ui,
    stage: &StageDefinition,
    score: &StageComparisonScorecard,
) {
    ui.columns(3, |columns| {
        comparison_card(
            &mut columns[0],
            "Ideal -> Measured",
            "Estimator contribution",
            &score.ideal_minus_measured,
            stage.unit,
            egui::Color32::from_rgb(87, 156, 214),
            "truth_lab_im",
        );
        comparison_card(
            &mut columns[1],
            "Measured -> Presented",
            "Presentation contribution",
            &score.measured_minus_presented,
            stage.unit,
            egui::Color32::from_rgb(173, 119, 214),
            "truth_lab_mp",
        );
        comparison_card(
            &mut columns[2],
            "Ideal -> Presented",
            "End-to-end difference",
            &score.ideal_minus_presented,
            stage.unit,
            egui::Color32::from_rgb(67, 176, 132),
            "truth_lab_ip",
        );
    });
}

fn comparison_card(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    stats: &SummaryStats,
    unit: &str,
    color: egui::Color32,
    id: &'static str,
) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            16,
        ))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(9, 8))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).strong().color(color));
            ui.label(egui::RichText::new(subtitle).small().weak());
            ui.add_space(3.0);
            egui::Grid::new(id).num_columns(2).show(ui, |ui| {
                metric_row(ui, "Bias", stats.mean, unit);
                metric_row(ui, "MAE", stats.mean_absolute, unit);
                metric_row(ui, "RMSE", stats.rmse, unit);
                metric_row(ui, "p95 abs", stats.p95_absolute, unit);
                metric_row(ui, "Max abs", stats.maximum_absolute, unit);
                ui.label(egui::RichText::new("N").small().weak());
                ui.label(stats.count.to_string());
                ui.end_row();
            });
        });
}

fn velocity_truth_ui(
    ui: &mut egui::Ui,
    velocity: &VelocityTruthScorecard,
    dealiased_label: Option<&str>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("Velocity ambiguity & dealias truth")
                .strong()
                .size(15.0),
        );
        badge(ui, "IVEL REFERENCE", egui::Color32::from_rgb(87, 156, 214));
        if let Some(label) = dealiased_label {
            badge(ui, label, egui::Color32::from_rgb(67, 176, 132));
        } else {
            badge(
                ui,
                "DVEL NOT SUPPLIED",
                egui::Color32::from_rgb(194, 139, 38),
            );
        }
    });
    ui.label(
        egui::RichText::new(
            "Folding exposure is computed analytically from IVEL and each radial's stamped Nyquist. DVEL, when present, is compared gate-for-gate with IVEL.",
        )
        .small()
        .weak(),
    );
    ui.add_space(4.0);

    egui::Frame::group(ui.style()).show(ui, |ui| {
        egui::Grid::new("truth_lab_velocity_summary")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                velocity_metric(ui, "Finite IVEL gates", velocity.input_samples.to_string());
                velocity_metric(
                    ui,
                    "Truth gates requiring a fold",
                    format_count_percent(velocity.folded_samples, velocity.input_samples),
                );
                velocity_metric(
                    ui,
                    "Expected folded-vs-IVEL RMSE",
                    format_stat(velocity.folded_error.rmse, "m/s"),
                );
                velocity_metric(
                    ui,
                    "Expected folded-vs-IVEL p95",
                    format_stat(velocity.folded_error.p95_absolute, "m/s"),
                );
            });
    });

    if dealiased_label.is_some() {
        ui.add_space(5.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            egui::Grid::new("truth_lab_dealias_summary")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    velocity_metric(
                        ui,
                        "Paired DVEL gates",
                        velocity.dealiased_samples.to_string(),
                    );
                    velocity_metric(
                        ui,
                        "Recovered folded gates",
                        format_count_percent(
                            velocity.recovered_folded_samples,
                            velocity.folded_samples,
                        ),
                    );
                    velocity_metric(
                        ui,
                        "Wrong Nyquist branches",
                        velocity.branch_errors.to_string(),
                    );
                    velocity_metric(ui, "False unfolds", velocity.false_unfolds.to_string());
                    velocity_metric(
                        ui,
                        "DVEL-vs-IVEL MAE",
                        format_stat(velocity.dealiased_error.mean_absolute, "m/s"),
                    );
                    velocity_metric(
                        ui,
                        "DVEL-vs-IVEL RMSE",
                        format_stat(velocity.dealiased_error.rmse, "m/s"),
                    );
                    velocity_metric(
                        ui,
                        "DVEL-vs-IVEL p95",
                        format_stat(velocity.dealiased_error.p95_absolute, "m/s"),
                    );
                    velocity_metric(
                        ui,
                        "DVEL-vs-IVEL max",
                        format_stat(velocity.dealiased_error.maximum_absolute, "m/s"),
                    );
                });
        });
    } else {
        egui::Frame::new()
            .fill(egui::Color32::from_rgba_unmultiplied(194, 139, 38, 18))
            .corner_radius(4)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Fold exposure is available, but recovery, branch-error, and false-unfold scores need an exact-geometry production DVEL grid for this cut.",
                    )
                    .small(),
                );
            });
    }
}

fn metric_row(ui: &mut egui::Ui, label: &str, value: Option<f64>, unit: &str) {
    ui.label(egui::RichText::new(label).small().weak());
    ui.label(format_stat(value, unit));
    ui.end_row();
}

fn velocity_metric(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).small().weak());
        ui.label(egui::RichText::new(value).strong().monospace());
    });
}

fn format_stat(value: Option<f64>, unit: &str) -> String {
    let Some(value) = value else {
        return "--".to_owned();
    };
    if unit.is_empty() {
        format!("{value:.4}")
    } else {
        format!("{value:.3} {unit}")
    }
}

fn format_count_percent(count: usize, total: usize) -> String {
    if total == 0 {
        return format!("{count} / 0");
    }
    format!(
        "{count} / {total} ({:.1}%)",
        count as f64 / total as f64 * 100.0
    )
}

fn badge(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            28,
        ))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.65)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().strong().color(color));
        });
}

fn required_stage_grid<'a>(
    cut: &'a ElevationCut,
    name: &'static str,
) -> Result<&'a MomentGrid, TruthLabError> {
    stage_grid(cut, name).ok_or(TruthLabError::MissingStageMoment { name })
}

fn stage_grid<'a>(cut: &'a ElevationCut, name: &str) -> Option<&'a MomentGrid> {
    cut.moments.get(&MomentType::Unknown(name.to_owned()))
}

fn same_geometry(left: &MomentGrid, right: &MomentGrid) -> bool {
    left.gate_range == right.gate_range && left.radial_indices == right.radial_indices
}

fn score_stage_grids(
    ideal: &MomentGrid,
    measured: &MomentGrid,
    presented: &MomentGrid,
) -> StageComparisonScorecard {
    let mut ideal_minus_measured = Vec::new();
    let mut ideal_minus_presented = Vec::new();
    let mut measured_minus_presented = Vec::new();
    for row in 0..ideal.radial_count() {
        for gate in 0..ideal.gate_range.gate_count {
            let Some(ideal_value) = ideal
                .scaled_value(row, gate)
                .map(f64::from)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            let Some(measured_value) = measured
                .scaled_value(row, gate)
                .map(f64::from)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            let Some(presented_value) = presented
                .scaled_value(row, gate)
                .map(f64::from)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            ideal_minus_measured.push(ideal_value - measured_value);
            ideal_minus_presented.push(ideal_value - presented_value);
            measured_minus_presented.push(measured_value - presented_value);
        }
    }
    StageComparisonScorecard {
        paired_samples: ideal_minus_measured.len(),
        ideal_minus_measured: SummaryStats::from_errors(&ideal_minus_measured),
        ideal_minus_presented: SummaryStats::from_errors(&ideal_minus_presented),
        measured_minus_presented: SummaryStats::from_errors(&measured_minus_presented),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use radar_core::{GateRange, MomentStorage, RadarSite, Radial};

    use super::*;

    fn grid(moment: MomentType, values: Vec<f32>) -> MomentGrid {
        MomentGrid {
            moment,
            gate_range: GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: 2,
            },
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: vec![0],
            storage: MomentStorage::F32(values),
        }
    }

    fn volume() -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("KLAB"),
            Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap(),
        );
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: GateRange {
                first_gate_m: 250,
                gate_spacing_m: 250,
                gate_count: 2,
            },
            nyquist_velocity_mps: Some(10.0),
            radial_status: None,
        });
        for (name, values) in [
            ("IVEL", vec![15.0, 2.0]),
            ("MVEL", vec![-5.0, 2.5]),
            ("IREF", vec![40.0, 20.0]),
            ("MREF", vec![39.0, 19.0]),
        ] {
            let moment = MomentType::Unknown(name.to_owned());
            cut.moments.insert(moment.clone(), grid(moment, values));
        }
        cut.moments.insert(
            MomentType::Velocity,
            grid(MomentType::Velocity, vec![-5.5, 2.75]),
        );
        cut.moments.insert(
            MomentType::Reflectivity,
            grid(MomentType::Reflectivity, vec![38.0, 18.0]),
        );
        volume.cuts.push(cut);
        volume
    }

    #[test]
    fn exact_stage_adapter_scores_measurement_presentation_and_dealiasing() {
        let volume = volume();
        let dealiased = grid(MomentType::Velocity, vec![15.0, 2.0]);
        let report = build_algorithm_truth_lab_report(&volume, 0, Some(&dealiased), 0.1).unwrap();
        assert_eq!(report.velocity.folded_samples, 1);
        assert_eq!(report.velocity.recovered_folded_samples, 1);
        assert_eq!(report.velocity.branch_errors, 0);
        assert_eq!(report.stage_moments["Reflectivity"].paired_samples, 2);
        assert_eq!(
            report.stage_moments["Reflectivity"]
                .ideal_minus_measured
                .mean,
            Some(1.0)
        );
        assert_eq!(
            report.stage_moments["Reflectivity"]
                .measured_minus_presented
                .mean,
            Some(1.0)
        );
    }

    #[test]
    fn missing_stage_diagnostics_fail_with_actionable_message() {
        let mut volume = volume();
        volume.cuts[0]
            .moments
            .remove(&MomentType::Unknown("IVEL".to_owned()));
        assert!(matches!(
            build_algorithm_truth_lab_report(&volume, 0, None, 0.5),
            Err(TruthLabError::MissingStageMoment { name: "IVEL" })
        ));
    }

    #[test]
    fn dealiased_geometry_must_be_exact() {
        let volume = volume();
        let mut dealiased = grid(MomentType::Velocity, vec![15.0, 2.0]);
        dealiased.gate_range.gate_spacing_m = 500;
        assert!(matches!(
            build_algorithm_truth_lab_report(&volume, 0, Some(&dealiased), 0.5),
            Err(TruthLabError::DealiasedGeometryMismatch)
        ));
    }

    #[test]
    fn stored_dvel_is_used_when_caller_has_no_external_grid() {
        let mut volume = volume();
        let moment = MomentType::Unknown("DVEL".to_owned());
        volume.cuts[0]
            .moments
            .insert(moment.clone(), grid(moment, vec![15.0, 2.0]));

        let report = build_algorithm_truth_lab_report(&volume, 0, None, 0.1).unwrap();
        assert_eq!(report.velocity.dealiased_samples, 2);
        assert_eq!(report.velocity.recovered_folded_samples, 1);
        assert_eq!(report.velocity.branch_errors, 0);
    }
}
