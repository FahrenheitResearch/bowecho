//! First-class selected-gate inspector for BowEcho synthetic radar volumes.
//!
//! The fast path reads only facts embedded in [`RadarVolume`]: geometry,
//! acquisition coordinates, quality fields, retained instrument stages, and
//! provenance. Hydrometeor decomposition and Doppler spectra are never
//! reverse-engineered from displayed moments. They appear only after the
//! caller supplies a production [`GateExplanation`] through the generation-
//! checked asynchronous API on [`SimradarGateInspectorState`].

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use eframe::egui;
use radar_core::{ElevationCut, GateRange, MomentGrid, MomentType, RadarVolume};
use thiserror::Error;

use crate::wrf_radar_estimator::{
    DopplerSpectrum, GateExplanation, GateIdentity, RadarMomentValues, SpectrumMoments,
    WhyThisGate, WhyThisGateUnavailable,
};

const QUALITY_FIELDS: [(&str, &str); 3] = [
    ("MCOV", "Model coverage"),
    ("TUNB", "Terrain unblocked"),
    ("MSIG", "Meteorological signal"),
];

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GateSelection {
    pub frame_index: usize,
    pub cut_index: usize,
    /// Row within `anchor_moment`, not a raw index into `cut.radials`.
    pub radial_row: usize,
    pub gate_index: usize,
    /// `None` chooses the first available canonical/stage grid in a stable
    /// priority order. Main should supply the displayed moment when known.
    pub anchor_moment: Option<MomentType>,
}

impl GateSelection {
    pub fn new(cut_index: usize, radial_row: usize, gate_index: usize) -> Self {
        Self {
            frame_index: 0,
            cut_index,
            radial_row,
            gate_index,
            anchor_moment: None,
        }
    }

    pub fn with_anchor(mut self, anchor_moment: MomentType) -> Self {
        self.anchor_moment = Some(anchor_moment);
        self
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum GateSnapshotError {
    #[error("cut {index} is outside this volume's {cut_count} cuts")]
    CutOutOfRange { index: usize, cut_count: usize },
    #[error("cut {cut_index} has no moment grid to define gate geometry")]
    NoAnchorGrid { cut_index: usize },
    #[error("cut {cut_index} does not carry requested anchor moment {moment}")]
    AnchorMomentMissing {
        cut_index: usize,
        moment: MomentType,
    },
    #[error("radial row {row} is outside anchor grid {moment}'s {row_count} rows")]
    RadialRowOutOfRange {
        row: usize,
        row_count: usize,
        moment: MomentType,
    },
    #[error("anchor grid row {row} references missing cut radial {radial_index}")]
    RadialMetadataMissing { row: usize, radial_index: usize },
    #[error("gate {gate} is outside anchor grid {moment}'s {gate_count} gates")]
    GateOutOfRange {
        gate: usize,
        gate_count: usize,
        moment: MomentType,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateFieldUnavailable {
    MomentNotPresent,
    GeometryMismatch,
    RadialNotPresent,
    MissingOrCensored,
}

impl GateFieldUnavailable {
    fn label(self) -> &'static str {
        match self {
            Self::MomentNotPresent => "moment not retained in this volume",
            Self::GeometryMismatch => {
                "moment uses different gate geometry; no nearest-gate value substituted"
            }
            Self::RadialNotPresent => "selected radial is not present in this moment grid",
            Self::MissingOrCensored => "gate is missing, censored, range-folded, or non-finite",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateFieldValue {
    pub value: Option<f32>,
    pub unavailable: Option<GateFieldUnavailable>,
}

impl GateFieldValue {
    fn available(value: f32) -> Self {
        Self {
            value: Some(value),
            unavailable: None,
        }
    }

    fn unavailable(reason: GateFieldUnavailable) -> Self {
        Self {
            value: None,
            unavailable: Some(reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateStageSnapshot {
    pub label: &'static str,
    pub short_name: &'static str,
    pub unit: &'static str,
    pub ideal: GateFieldValue,
    pub measured: GateFieldValue,
    pub presented: GateFieldValue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateQualitySnapshot {
    pub id: &'static str,
    pub label: &'static str,
    pub fraction: GateFieldValue,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateProvenanceSnapshot {
    pub source_path: Option<String>,
    pub forward_operator: Option<String>,
    pub forward_operator_config: Option<String>,
    pub source_model: Option<String>,
    pub microphysics_scheme: Option<String>,
    pub scattering_model: Option<String>,
    pub calibration: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateSnapshot {
    pub identity: GateIdentity,
    pub anchor_moment: MomentType,
    pub anchor_gate_range: GateRange,
    pub acquisition_time_utc: DateTime<Utc>,
    pub ray_offset_ms: i64,
    pub nyquist_velocity_mps: Option<f32>,
    pub quality: Vec<GateQualitySnapshot>,
    pub stages: Vec<GateStageSnapshot>,
    /// Range-dependent sensitivity is not encoded in a `RadarVolume` moment.
    /// It remains `None` until a production `GateExplanation` is supplied.
    pub sensitivity_dbz: Option<f64>,
    pub provenance: GateProvenanceSnapshot,
}

/// Extract only exact, embedded selected-gate facts.
///
/// Every secondary grid must match the anchor's full `GateRange` and contain
/// the same referenced radial. A mismatched grid is reported unavailable; the
/// inspector never performs nearest-row or nearest-range substitution.
pub fn extract_gate_snapshot(
    volume: &RadarVolume,
    selection: &GateSelection,
) -> Result<GateSnapshot, GateSnapshotError> {
    let cut = volume
        .cuts
        .get(selection.cut_index)
        .ok_or(GateSnapshotError::CutOutOfRange {
            index: selection.cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let (anchor_moment, anchor) = resolve_anchor_grid(cut, selection)?;
    let Some(&radial_index) = anchor.radial_indices.get(selection.radial_row) else {
        return Err(GateSnapshotError::RadialRowOutOfRange {
            row: selection.radial_row,
            row_count: anchor.radial_count(),
            moment: anchor_moment.clone(),
        });
    };
    let radial = cut
        .radials
        .get(radial_index)
        .ok_or(GateSnapshotError::RadialMetadataMissing {
            row: selection.radial_row,
            radial_index,
        })?;
    if selection.gate_index >= anchor.gate_range.gate_count {
        return Err(GateSnapshotError::GateOutOfRange {
            gate: selection.gate_index,
            gate_count: anchor.gate_range.gate_count,
            moment: anchor_moment.clone(),
        });
    }
    let slant_range_m = f64::from(anchor.gate_range.first_gate_m)
        + selection.gate_index as f64 * f64::from(anchor.gate_range.gate_spacing_m);
    let ray_offset_ms = i64::from(radial.time_offset_ms);
    let acquisition_time_utc = volume.volume_time + Duration::milliseconds(ray_offset_ms);
    let identity = GateIdentity {
        frame_index: selection.frame_index,
        cut_index: selection.cut_index,
        radial_index,
        gate_index: selection.gate_index,
        azimuth_deg: f64::from(radial.azimuth_deg),
        elevation_deg: f64::from(radial.elevation_deg),
        slant_range_m,
    };

    let quality = QUALITY_FIELDS
        .iter()
        .map(|&(id, label)| GateQualitySnapshot {
            id,
            label,
            fraction: exact_grid_value(
                cut,
                &MomentType::Unknown(id.to_owned()),
                anchor,
                radial_index,
                selection.gate_index,
            ),
        })
        .collect();
    let stages = STAGES
        .iter()
        .map(|stage| GateStageSnapshot {
            label: stage.label,
            short_name: stage.short_name,
            unit: stage.unit,
            ideal: exact_grid_value(
                cut,
                &MomentType::Unknown(stage.ideal.to_owned()),
                anchor,
                radial_index,
                selection.gate_index,
            ),
            measured: exact_grid_value(
                cut,
                &MomentType::Unknown(stage.measured.to_owned()),
                anchor,
                radial_index,
                selection.gate_index,
            ),
            presented: exact_grid_value(
                cut,
                &stage.presented,
                anchor,
                radial_index,
                selection.gate_index,
            ),
        })
        .collect();

    Ok(GateSnapshot {
        identity,
        anchor_moment,
        anchor_gate_range: anchor.gate_range.clone(),
        acquisition_time_utc,
        ray_offset_ms,
        nyquist_velocity_mps: radial.nyquist_velocity_mps,
        quality,
        stages,
        sensitivity_dbz: None,
        provenance: GateProvenanceSnapshot {
            source_path: volume.metadata.source_path.clone(),
            forward_operator: volume.metadata.forward_operator.clone(),
            forward_operator_config: volume.metadata.forward_operator_config.clone(),
            source_model: volume.metadata.source_model.clone(),
            microphysics_scheme: volume.metadata.microphysics_scheme.clone(),
            scattering_model: volume.metadata.scattering_model.clone(),
            calibration: volume.metadata.calibration.clone(),
        },
    })
}

fn resolve_anchor_grid<'a>(
    cut: &'a ElevationCut,
    selection: &GateSelection,
) -> Result<(MomentType, &'a MomentGrid), GateSnapshotError> {
    if let Some(moment) = &selection.anchor_moment {
        return cut
            .moments
            .get(moment)
            .map(|grid| (moment.clone(), grid))
            .ok_or_else(|| GateSnapshotError::AnchorMomentMissing {
                cut_index: selection.cut_index,
                moment: moment.clone(),
            });
    }
    for moment in [
        MomentType::Reflectivity,
        MomentType::Unknown("IREF".to_owned()),
        MomentType::Velocity,
        MomentType::Unknown("IVEL".to_owned()),
    ] {
        if let Some(grid) = cut.moments.get(&moment) {
            return Ok((moment, grid));
        }
    }
    cut.moments
        .iter()
        .next()
        .map(|(moment, grid)| (moment.clone(), grid))
        .ok_or(GateSnapshotError::NoAnchorGrid {
            cut_index: selection.cut_index,
        })
}

fn exact_grid_value(
    cut: &ElevationCut,
    moment: &MomentType,
    anchor: &MomentGrid,
    radial_index: usize,
    gate_index: usize,
) -> GateFieldValue {
    let Some(grid) = cut.moments.get(moment) else {
        return GateFieldValue::unavailable(GateFieldUnavailable::MomentNotPresent);
    };
    if grid.gate_range != anchor.gate_range {
        return GateFieldValue::unavailable(GateFieldUnavailable::GeometryMismatch);
    }
    let Some(row) = grid
        .radial_indices
        .iter()
        .position(|candidate| *candidate == radial_index)
    else {
        return GateFieldValue::unavailable(GateFieldUnavailable::RadialNotPresent);
    };
    grid.scaled_value(row, gate_index)
        .filter(|value| value.is_finite())
        .map(GateFieldValue::available)
        .unwrap_or_else(|| GateFieldValue::unavailable(GateFieldUnavailable::MissingOrCensored))
}

#[derive(Clone)]
pub(crate) struct GateInspectorRequest {
    pub(crate) generation: u64,
    pub(crate) volume: Arc<RadarVolume>,
    pub(crate) identity: GateIdentity,
}

#[derive(Clone, Debug)]
struct SpectrumVisibility {
    true_signal: bool,
    aliased_signal: bool,
    white_noise: bool,
    measured: bool,
    noise_subtracted: bool,
    species_true: bool,
    species_aliased: bool,
    selected_species: BTreeSet<String>,
}

impl Default for SpectrumVisibility {
    fn default() -> Self {
        Self {
            true_signal: true,
            aliased_signal: true,
            white_noise: false,
            measured: true,
            noise_subtracted: false,
            species_true: false,
            species_aliased: false,
            selected_species: BTreeSet::new(),
        }
    }
}

pub(crate) struct SimradarGateInspectorState {
    pub(crate) open: bool,
    volume: Option<Arc<RadarVolume>>,
    selection: GateSelection,
    snapshot: Option<Result<GateSnapshot, GateSnapshotError>>,
    generation: u64,
    queued_request: Option<GateInspectorRequest>,
    explanation: Option<WhyThisGate>,
    spectrum_visibility: SpectrumVisibility,
}

impl Default for SimradarGateInspectorState {
    fn default() -> Self {
        Self {
            open: false,
            volume: None,
            selection: GateSelection::new(0, 0, 0),
            snapshot: None,
            generation: 0,
            queued_request: None,
            explanation: None,
            spectrum_visibility: SpectrumVisibility::default(),
        }
    }
}

impl SimradarGateInspectorState {
    pub(crate) fn open_with_gate(&mut self, volume: Arc<RadarVolume>, selection: GateSelection) {
        let source_changed = self
            .volume
            .as_ref()
            .is_none_or(|current| !Arc::ptr_eq(current, &volume));
        if source_changed || self.selection != selection {
            self.volume = Some(volume);
            self.selection = selection;
            self.refresh_snapshot();
        }
        self.open = true;
    }

    /// Returns a single queued on-demand request. Main may run it on a worker
    /// and later call [`Self::supply_explanation`] with the same generation.
    pub(crate) fn take_explanation_request(&mut self) -> Option<GateInspectorRequest> {
        self.queued_request.take()
    }

    /// Supply an asynchronous result if it still belongs to the current gate.
    /// Stale generations or mismatched available identities are rejected.
    pub(crate) fn supply_explanation(&mut self, generation: u64, result: WhyThisGate) -> bool {
        if generation != self.generation {
            return false;
        }
        let result_identity = match &result {
            WhyThisGate::Available(explanation) => Some(explanation.identity),
            WhyThisGate::Loading(identity) => Some(*identity),
            WhyThisGate::Unavailable(_) => None,
        };
        if result_identity.is_some_and(|identity| {
            self.current_identity()
                .is_none_or(|expected| !same_identity(expected, identity))
        }) {
            return false;
        }
        if let WhyThisGate::Available(explanation) = &result {
            self.spectrum_visibility.selected_species = explanation
                .spectrum
                .as_ref()
                .map(|spectrum| {
                    spectrum
                        .species
                        .iter()
                        .map(|species| species.name.clone())
                        .collect()
                })
                .unwrap_or_default();
        }
        self.explanation = Some(result);
        true
    }

    pub(crate) fn show_window(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Why This Gate?")
            .id(egui::Id::new("bowecho_simradar_gate_inspector"))
            .open(&mut open)
            .default_width(900.0)
            .default_height(760.0)
            .min_width(680.0)
            .min_height(520.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("simradar_gate_inspector_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.ui(ui));
            });
        self.open = open;
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Why This Gate?");
            badge(
                ui,
                "EXACT SYNTHETIC GATE",
                egui::Color32::from_rgb(118, 92, 214),
            );
        });
        ui.label(
            egui::RichText::new(
                "Inspect retained gate stages immediately; request hydrometeor and Doppler-spectrum truth only when needed.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
        honesty_callout(ui);
        ui.add_space(8.0);

        let Some(volume) = self.volume.clone() else {
            empty_ui(ui);
            return;
        };
        self.selection_ui(ui, &volume);
        ui.add_space(8.0);

        match self.snapshot.clone() {
            Some(Ok(snapshot)) => {
                snapshot_header_ui(ui, &volume, &snapshot);
                ui.add_space(8.0);
                quality_ui(ui, &snapshot);
                ui.add_space(8.0);
                stages_ui(ui, &snapshot);
                ui.add_space(8.0);
                sensitivity_ui(ui, self.available_explanation());
                ui.add_space(8.0);
                provenance_ui(ui, &snapshot.provenance);
                ui.add_space(10.0);
                self.explanation_ui(ui, &snapshot);
            }
            Some(Err(error)) => extraction_error_ui(ui, &error),
            None => empty_ui(ui),
        }
    }

    fn selection_ui(&mut self, ui: &mut egui::Ui, volume: &RadarVolume) {
        let old = self.selection.clone();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(&volume.site.id).strong().size(16.0));
                ui.label(volume.volume_time.format("%Y-%m-%d %H:%M:%SZ").to_string());
                ui.separator();
                ui.label("Cut");
                let cut_text = volume
                    .cuts
                    .get(self.selection.cut_index)
                    .map(|cut| {
                        format!(
                            "#{:02}  {:.2} deg",
                            self.selection.cut_index + 1,
                            cut.elevation_deg
                        )
                    })
                    .unwrap_or_else(|| "No cut".to_owned());
                egui::ComboBox::from_id_salt("gate_inspector_cut")
                    .selected_text(cut_text)
                    .show_ui(ui, |ui| {
                        for (index, cut) in volume.cuts.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.selection.cut_index,
                                index,
                                format!("#{:02}  {:.2} deg", index + 1, cut.elevation_deg),
                            );
                        }
                    });
            });

            if self.selection.cut_index != old.cut_index {
                self.selection.radial_row = 0;
                self.selection.gate_index = 0;
                self.selection.anchor_moment = None;
            }

            if let Some(cut) = volume.cuts.get(self.selection.cut_index) {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Geometry anchor");
                    let selected_anchor = self
                        .selection
                        .anchor_moment
                        .as_ref()
                        .map_or("Auto", MomentType::short_name);
                    egui::ComboBox::from_id_salt("gate_inspector_anchor")
                        .selected_text(selected_anchor)
                        .width(110.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.selection.anchor_moment, None, "Auto");
                            for moment in cut.moments.keys() {
                                ui.selectable_value(
                                    &mut self.selection.anchor_moment,
                                    Some(moment.clone()),
                                    moment.short_name(),
                                );
                            }
                        });

                    let anchor = resolve_anchor_grid(cut, &self.selection)
                        .ok()
                        .map(|(_, grid)| grid);
                    let row_max = anchor
                        .map(|grid| grid.radial_count().saturating_sub(1))
                        .unwrap_or(0);
                    let gate_max = anchor
                        .map(|grid| grid.gate_range.gate_count.saturating_sub(1))
                        .unwrap_or(0);
                    self.selection.radial_row = self.selection.radial_row.min(row_max);
                    self.selection.gate_index = self.selection.gate_index.min(gate_max);
                    ui.separator();
                    ui.label("Radial row");
                    ui.add(
                        egui::DragValue::new(&mut self.selection.radial_row)
                            .range(0..=row_max)
                            .speed(1.0),
                    );
                    ui.label(format!("/ {row_max}"));
                    ui.separator();
                    ui.label("Gate");
                    ui.add(
                        egui::DragValue::new(&mut self.selection.gate_index)
                            .range(0..=gate_max)
                            .speed(1.0),
                    );
                    ui.label(format!("/ {gate_max}"));
                });
            }
        });
        if self.selection != old {
            self.refresh_snapshot();
        }
    }

    fn explanation_ui(&mut self, ui: &mut egui::Ui, snapshot: &GateSnapshot) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("On-demand physical explanation")
                    .strong()
                    .size(15.0),
            );
            badge(
                ui,
                "NOT INFERRED FROM DISPLAY",
                egui::Color32::from_rgb(52, 165, 123),
            );
        });
        ui.label(
            egui::RichText::new(
                "Hydrometeor populations, estimator internals, and a selected-gate Doppler spectrum require the retained WRF source snapshot. They are not stored in ordinary moment grids.",
            )
            .small()
            .weak(),
        );
        ui.add_space(5.0);

        // Move the potentially large spectrum out temporarily so ordinary UI
        // frames never clone it merely to satisfy mutable visibility controls.
        let explanation_state = self.explanation.take();
        match explanation_state.as_ref() {
            None => {
                if ui
                    .button("Build exact explanation + Doppler spectrum")
                    .clicked()
                {
                    self.queue_explanation(snapshot.identity);
                }
                ui.label(
                    egui::RichText::new(
                        "Runs only for this gate. Main may complete it asynchronously while the embedded snapshot remains visible.",
                    )
                    .small()
                    .weak(),
                );
                unavailable_hydrometeor_ui(ui);
            }
            Some(WhyThisGate::Loading(identity)) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Reconstructing cut {} radial {} gate {}...",
                        identity.cut_index + 1,
                        identity.radial_index + 1,
                        identity.gate_index + 1
                    ));
                });
                unavailable_hydrometeor_ui(ui);
            }
            Some(WhyThisGate::Unavailable(reason)) => {
                unavailable_result_ui(ui, reason);
                if ui.button("Retry exact explanation").clicked() {
                    self.queue_explanation(snapshot.identity);
                }
                unavailable_hydrometeor_ui(ui);
            }
            Some(WhyThisGate::Available(explanation)) => {
                explanation_overview_ui(ui, explanation);
                ui.add_space(8.0);
                hydrometeor_ui(ui, explanation);
                ui.add_space(8.0);
                if let Some(spectrum) = &explanation.spectrum {
                    self.spectrum_ui(ui, spectrum);
                } else {
                    missing_spectrum_ui(ui);
                }
                ui.add_space(8.0);
                explanation_provenance_ui(ui, &explanation.provenance);
            }
        }
        if self.explanation.is_none() {
            self.explanation = explanation_state;
        }
    }

    fn spectrum_ui(&mut self, ui: &mut egui::Ui, spectrum: &DopplerSpectrum) {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Doppler spectrum").strong().size(15.0));
            badge(
                ui,
                &format!(
                    "{} TRUE / {} OUTPUT BINS",
                    spectrum.true_velocity_centers_mps.len(),
                    spectrum.output_velocity_centers_mps.len()
                ),
                egui::Color32::from_rgb(118, 92, 214),
            );
        });
        ui.label(
            egui::RichText::new(
                "Power is plotted in dB relative to the strongest visible bin. The two velocity grids share one physical x axis; no bin interpolation is used for moment summaries.",
            )
            .small()
            .weak(),
        );
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.spectrum_visibility.true_signal, "True signal");
            ui.checkbox(
                &mut self.spectrum_visibility.aliased_signal,
                "Aliased signal",
            );
            ui.checkbox(&mut self.spectrum_visibility.white_noise, "White noise");
            ui.checkbox(&mut self.spectrum_visibility.measured, "Measured");
            ui.checkbox(
                &mut self.spectrum_visibility.noise_subtracted,
                "Noise-subtracted",
            );
            ui.separator();
            ui.checkbox(&mut self.spectrum_visibility.species_true, "Species true");
            ui.checkbox(
                &mut self.spectrum_visibility.species_aliased,
                "Species aliased",
            );
        });
        if !spectrum.species.is_empty()
            && (self.spectrum_visibility.species_true || self.spectrum_visibility.species_aliased)
        {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Hydrometeors").small().strong());
                for (index, species) in spectrum.species.iter().enumerate() {
                    let mut selected = self
                        .spectrum_visibility
                        .selected_species
                        .contains(&species.name);
                    let color = species_color(index);
                    if ui
                        .checkbox(
                            &mut selected,
                            egui::RichText::new(&species.name).color(color),
                        )
                        .changed()
                    {
                        if selected {
                            self.spectrum_visibility
                                .selected_species
                                .insert(species.name.clone());
                        } else {
                            self.spectrum_visibility
                                .selected_species
                                .remove(&species.name);
                        }
                    }
                }
            });
        }
        spectrum_plot(ui, spectrum, &self.spectrum_visibility);
        spectrum_moments_ui(ui, spectrum);
    }

    fn available_explanation(&self) -> Option<&GateExplanation> {
        match self.explanation.as_ref() {
            Some(WhyThisGate::Available(explanation)) => Some(explanation),
            _ => None,
        }
    }

    fn queue_explanation(&mut self, identity: GateIdentity) {
        let Some(volume) = self.volume.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        self.queued_request = Some(GateInspectorRequest {
            generation: self.generation,
            volume,
            identity,
        });
        self.explanation = Some(WhyThisGate::Loading(identity));
    }

    fn current_identity(&self) -> Option<GateIdentity> {
        self.snapshot
            .as_ref()
            .and_then(|result| result.as_ref().ok())
            .map(|snapshot| snapshot.identity)
    }

    fn refresh_snapshot(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.queued_request = None;
        self.explanation = None;
        self.spectrum_visibility = SpectrumVisibility::default();
        self.snapshot = self
            .volume
            .as_ref()
            .map(|volume| extract_gate_snapshot(volume, &self.selection));
    }
}

fn same_identity(left: GateIdentity, right: GateIdentity) -> bool {
    left.frame_index == right.frame_index
        && left.cut_index == right.cut_index
        && left.radial_index == right.radial_index
        && left.gate_index == right.gate_index
        && (left.azimuth_deg - right.azimuth_deg).abs() <= 1.0e-6
        && (left.elevation_deg - right.elevation_deg).abs() <= 1.0e-6
        && (left.slant_range_m - right.slant_range_m).abs() <= 1.0e-6
}

fn honesty_callout(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(76, 121, 190, 20))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(94, 141, 214, 95),
        ))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Two evidence levels").strong());
            ui.label(
                egui::RichText::new(
                    "Embedded values come directly from this RadarVolume. Physical decomposition and spectra are shown only from a generation-matched production GateExplanation. Missing details remain visibly unavailable.",
                )
                .small(),
            );
        });
}

fn snapshot_header_ui(ui: &mut egui::Ui, volume: &RadarVolume, snapshot: &GateSnapshot) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Acquisition geometry")
                    .strong()
                    .size(15.0),
            );
            badge(
                ui,
                snapshot.anchor_moment.short_name(),
                egui::Color32::from_rgb(87, 156, 214),
            );
            if volume
                .metadata
                .forward_operator
                .as_deref()
                .is_some_and(|value| value.contains("BowEcho"))
            {
                badge(ui, "SYNTHETIC", egui::Color32::from_rgb(52, 165, 123));
            }
        });
        egui::Grid::new("gate_inspector_geometry")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                metric(ui, "Cut", format!("{}", snapshot.identity.cut_index + 1));
                metric(
                    ui,
                    "Radial index",
                    format!("{}", snapshot.identity.radial_index + 1),
                );
                metric(ui, "Gate", format!("{}", snapshot.identity.gate_index + 1));
                metric(
                    ui,
                    "Range",
                    format!("{:.3} km", snapshot.identity.slant_range_m / 1_000.0),
                );
                metric(
                    ui,
                    "Azimuth",
                    format!("{:.3} deg", snapshot.identity.azimuth_deg),
                );
                metric(
                    ui,
                    "Elevation",
                    format!("{:.3} deg", snapshot.identity.elevation_deg),
                );
                metric(
                    ui,
                    "Nyquist",
                    snapshot
                        .nyquist_velocity_mps
                        .map(|value| format!("{value:.3} m/s"))
                        .unwrap_or_else(|| "not stamped".to_owned()),
                );
                metric(
                    ui,
                    "Gate spacing",
                    format!("{} m", snapshot.anchor_gate_range.gate_spacing_m),
                );
            });
        ui.label(
            egui::RichText::new(format!(
                "Ray acquisition {}  |  offset {:+} ms from volume start",
                snapshot
                    .acquisition_time_utc
                    .format("%Y-%m-%d %H:%M:%S%.3fZ"),
                snapshot.ray_offset_ms
            ))
            .small()
            .monospace(),
        );
    });
}

fn quality_ui(ui: &mut egui::Ui, snapshot: &GateSnapshot) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("Pulse-volume support")
                .strong()
                .size(15.0),
        );
        badge(
            ui,
            "MCOV / TUNB / MSIG",
            egui::Color32::from_rgb(52, 165, 123),
        );
    });
    ui.columns(3, |columns| {
        for (column, quality) in columns.iter_mut().zip(&snapshot.quality) {
            quality_card(column, quality);
        }
    });
}

fn quality_card(ui: &mut egui::Ui, quality: &GateQualitySnapshot) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(quality.id).strong().monospace());
            ui.label(egui::RichText::new(quality.label).small().weak());
        });
        match quality.fraction.value {
            Some(value) => {
                ui.add(
                    egui::ProgressBar::new(value.clamp(0.0, 1.0))
                        .text(format!("{:.1}%", value * 100.0)),
                );
            }
            None => {
                let reason = quality
                    .fraction
                    .unavailable
                    .map_or("unavailable", GateFieldUnavailable::label);
                ui.label(egui::RichText::new("Not retained").strong().weak())
                    .on_hover_text(reason);
            }
        }
    });
}

fn stages_ui(ui: &mut egui::Ui, snapshot: &GateSnapshot) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("Ideal -> Measured -> Presented")
                .strong()
                .size(15.0),
        );
        badge(
            ui,
            "EXACT GRID VALUES",
            egui::Color32::from_rgb(118, 92, 214),
        );
    });
    ui.label(
        egui::RichText::new(
            "Unavailable geometry is left blank. The inspector never samples a neighboring gate to complete a row.",
        )
        .small()
        .weak(),
    );
    egui::Grid::new("gate_inspector_stages")
        .num_columns(5)
        .striped(true)
        .min_col_width(120.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Moment").strong());
            ui.label(egui::RichText::new("Ideal (I*)").strong());
            ui.label(egui::RichText::new("Measured (M*)").strong());
            ui.label(egui::RichText::new("Presented").strong());
            ui.label(egui::RichText::new("I - P").strong());
            ui.end_row();
            for stage in &snapshot.stages {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(stage.short_name).strong().monospace());
                    ui.label(egui::RichText::new(stage.label).small().weak());
                });
                field_label(ui, stage.ideal, stage.unit);
                field_label(ui, stage.measured, stage.unit);
                field_label(ui, stage.presented, stage.unit);
                let difference = stage
                    .ideal
                    .value
                    .zip(stage.presented.value)
                    .map(|(ideal, presented)| ideal - presented);
                ui.label(format_optional_f32(difference, stage.unit));
                ui.end_row();
            }
        });
}

fn sensitivity_ui(ui: &mut egui::Ui, explanation: Option<&GateExplanation>) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Sensitivity & estimator").strong());
            if explanation.is_some() {
                badge(
                    ui,
                    "EXACT EXPLANATION",
                    egui::Color32::from_rgb(52, 165, 123),
                );
            } else {
                badge(
                    ui,
                    "ON DEMAND",
                    egui::Color32::from_rgb(194, 139, 38),
                );
            }
        });
        if let Some(explanation) = explanation {
            egui::Grid::new("gate_inspector_sensitivity")
                .num_columns(4)
                .show(ui, |ui| {
                    metric(
                        ui,
                        "Sensitivity",
                        format_optional_f64(explanation.measured.sensitivity_dbz, "dBZ"),
                    );
                    metric(
                        ui,
                        "SNR",
                        format_optional_f64(explanation.measured.snr_db, "dB"),
                    );
                    metric(
                        ui,
                        "Independent samples",
                        format!("{:.2}", explanation.measured.sampling.independent_samples),
                    );
                    metric(
                        ui,
                        "Censored",
                        if explanation.measured.censored {
                            "yes".to_owned()
                        } else {
                            "no".to_owned()
                        },
                    );
                });
        } else {
            ui.label(
                egui::RichText::new(
                    "Range-dependent sensitivity, SNR, estimator uncertainty, and noise draws are not encoded in RadarVolume. Request the exact explanation to inspect them.",
                )
                .small()
                .weak(),
            );
        }
    });
}

fn provenance_ui(ui: &mut egui::Ui, provenance: &GateProvenanceSnapshot) {
    egui::CollapsingHeader::new("Volume provenance")
        .default_open(false)
        .show(ui, |ui| {
            provenance_row(
                ui,
                "Forward operator",
                provenance.forward_operator.as_deref(),
            );
            provenance_row(ui, "Source model", provenance.source_model.as_deref());
            provenance_row(
                ui,
                "Microphysics",
                provenance.microphysics_scheme.as_deref(),
            );
            provenance_row(ui, "Scattering", provenance.scattering_model.as_deref());
            provenance_row(ui, "Calibration", provenance.calibration.as_deref());
            provenance_row(ui, "Source path", provenance.source_path.as_deref());
            provenance_row(
                ui,
                "Operator config",
                provenance.forward_operator_config.as_deref(),
            );
        });
}

fn explanation_overview_ui(ui: &mut egui::Ui, explanation: &GateExplanation) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Production reconstruction").strong());
            badge(
                ui,
                &explanation.instrument.name,
                egui::Color32::from_rgb(52, 165, 123),
            );
        });
        ui.label(format!(
            "{:.3} GHz  |  wavelength {:.3} cm  |  pulse {:.3} us",
            explanation.instrument.frequency_hz / 1.0e9,
            explanation.instrument.wavelength_m() * 100.0,
            explanation.instrument.pulse_width_s * 1.0e6
        ));
        if let Some(timing) = explanation.timing {
            ui.label(format!(
                "PRF {:.3} Hz  |  Nyquist {:.3} m/s  |  unambiguous range {:.3} km",
                timing.prf_hz,
                timing.nyquist_velocity_mps,
                timing.unambiguous_range_m / 1_000.0
            ));
        }
        egui::Grid::new("gate_explanation_moments")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Moment").strong());
                ui.label(egui::RichText::new("Ideal").strong());
                ui.label(egui::RichText::new("Measured").strong());
                ui.label(egui::RichText::new("Presented").strong());
                ui.end_row();
                explanation_moment_rows(
                    ui,
                    explanation.ideal.values,
                    explanation.measured.values,
                    explanation.presented.values,
                );
            });
        ui.add_space(4.0);
        egui::Grid::new("gate_explanation_velocity")
            .num_columns(4)
            .show(ui, |ui| {
                metric(
                    ui,
                    "Air radial velocity",
                    format!("{:.3} m/s", explanation.velocity.air_velocity_mps),
                );
                metric(
                    ui,
                    "Fall correction",
                    format!(
                        "{:+.3} m/s",
                        explanation.velocity.terminal_fall_correction_mps
                    ),
                );
                metric(
                    ui,
                    "Scatterer velocity",
                    format!("{:.3} m/s", explanation.velocity.scatterer_velocity_mps),
                );
                metric(
                    ui,
                    "Temporal alpha",
                    format!("{:.4}", explanation.time.temporal_alpha),
                );
            });
        egui::Grid::new("gate_explanation_coverage")
            .num_columns(4)
            .show(ui, |ui| {
                metric(
                    ui,
                    "Model coverage",
                    format!(
                        "{:.1}%",
                        explanation.coverage.model_coverage_fraction * 100.0
                    ),
                );
                metric(
                    ui,
                    "Terrain unblocked",
                    format!(
                        "{:.1}%",
                        explanation.coverage.terrain_unblocked_fraction * 100.0
                    ),
                );
                metric(
                    ui,
                    "Meteorological signal",
                    format!(
                        "{:.1}%",
                        explanation.coverage.meteorological_signal_fraction * 100.0
                    ),
                );
                metric(
                    ui,
                    "Unblocked power",
                    format!(
                        "{:.1}%",
                        explanation.coverage.unblocked_power_fraction * 100.0
                    ),
                );
            });
        egui::CollapsingHeader::new("Propagation and temporal reconstruction")
            .default_open(false)
            .show(ui, |ui| {
                egui::Grid::new("gate_explanation_propagation")
                    .num_columns(4)
                    .show(ui, |ui| {
                        metric(
                            ui,
                            "Intrinsic REF",
                            format_optional_f64(
                                explanation.propagation.intrinsic_reflectivity_dbz,
                                "dBZ",
                            ),
                        );
                        metric(
                            ui,
                            "Observed REF",
                            format_optional_f64(
                                explanation.propagation.observed_reflectivity_dbz,
                                "dBZ",
                            ),
                        );
                        metric(
                            ui,
                            "PIA",
                            format_optional_f64(explanation.propagation.pia_db, "dB"),
                        );
                        metric(
                            ui,
                            "PhiDP",
                            format_optional_f64(explanation.propagation.phi_dp_deg, "deg"),
                        );
                        metric(
                            ui,
                            "Intrinsic ZDR",
                            format_optional_f64(explanation.propagation.intrinsic_zdr_db, "dB"),
                        );
                        metric(
                            ui,
                            "Observed ZDR",
                            format_optional_f64(explanation.propagation.observed_zdr_db, "dB"),
                        );
                        metric(
                            ui,
                            "PIDA",
                            format_optional_f64(explanation.propagation.pida_db, "dB"),
                        );
                        metric(
                            ui,
                            "Scene interpolation",
                            if explanation.time.held_anchor {
                                "held anchor".to_owned()
                            } else {
                                format!("alpha {:.4}", explanation.time.temporal_alpha)
                            },
                        );
                    });
            });
    });
}

fn explanation_moment_rows(
    ui: &mut egui::Ui,
    ideal: RadarMomentValues,
    measured: RadarMomentValues,
    presented: RadarMomentValues,
) {
    for (name, unit, i, m, p) in [
        (
            "REF",
            "dBZ",
            ideal.reflectivity_dbz,
            measured.reflectivity_dbz,
            presented.reflectivity_dbz,
        ),
        (
            "VEL",
            "m/s",
            ideal.velocity_mps,
            measured.velocity_mps,
            presented.velocity_mps,
        ),
        (
            "SW",
            "m/s",
            ideal.spectrum_width_mps,
            measured.spectrum_width_mps,
            presented.spectrum_width_mps,
        ),
        ("ZDR", "dB", ideal.zdr_db, measured.zdr_db, presented.zdr_db),
        ("RHO", "", ideal.rho_hv, measured.rho_hv, presented.rho_hv),
        (
            "KDP",
            "deg/km",
            ideal.kdp_deg_km,
            measured.kdp_deg_km,
            presented.kdp_deg_km,
        ),
    ] {
        ui.label(egui::RichText::new(name).monospace().strong());
        ui.label(format_optional_f64(i, unit));
        ui.label(format_optional_f64(m, unit));
        ui.label(format_optional_f64(p, unit));
        ui.end_row();
    }
}

fn hydrometeor_ui(ui: &mut egui::Ui, explanation: &GateExplanation) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("Hydrometeor decomposition")
                .strong()
                .size(15.0),
        );
        badge(
            ui,
            &format!("{} COMPONENTS", explanation.hydrometeors.len()),
            egui::Color32::from_rgb(173, 119, 214),
        );
    });
    if explanation.hydrometeors.is_empty() {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(egui::RichText::new("No decomposition returned").strong());
            ui.label(
                egui::RichText::new(
                    "The production explanation did not retain species contributions for this gate. No mixture is inferred from bulk radar moments.",
                )
                .small()
                .weak(),
            );
        });
        return;
    }
    let total_zh = explanation
        .hydrometeors
        .iter()
        .map(|component| component.zh_linear.max(0.0))
        .sum::<f64>();
    egui::Grid::new("gate_hydrometeor_components")
        .num_columns(7)
        .striped(true)
        .show(ui, |ui| {
            for heading in ["Species", "ZH", "ZH share", "ZV", "KDP", "Ah", "Fall speed"] {
                ui.label(egui::RichText::new(heading).strong());
            }
            ui.end_row();
            for component in &explanation.hydrometeors {
                ui.label(egui::RichText::new(&component.name).strong());
                ui.label(format!("{:.4e}", component.zh_linear));
                ui.label(if total_zh > 0.0 {
                    format!("{:.1}%", component.zh_linear.max(0.0) / total_zh * 100.0)
                } else {
                    "--".to_owned()
                });
                ui.label(format!("{:.4e}", component.zv_linear));
                ui.label(format!("{:.4} deg/km", component.kdp_deg_km));
                ui.label(format!("{:.4} dB/km", component.ah_db_km));
                ui.label(format!("{:.3} m/s", component.fall_speed_mps));
                ui.end_row();
            }
        });
}

fn unavailable_hydrometeor_ui(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(194, 139, 38, 18))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Hydrometeor decomposition unavailable").strong());
            ui.label(
                egui::RichText::new(
                    "RadarVolume does not store per-species gate contributions. REF/ZDR/RHO/KDP are not enough to uniquely recover them, so this inspector does not guess.",
                )
                .small()
                .weak(),
            );
        });
}

fn missing_spectrum_ui(ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Doppler spectrum not returned").strong());
        ui.label(
            egui::RichText::new(
                "The exact gate explanation is valid, but this reconstruction did not include a DopplerSpectrum. No Gaussian or single-moment surrogate is drawn.",
            )
            .small()
            .weak(),
        );
    });
}

fn spectrum_moments_ui(ui: &mut egui::Ui, spectrum: &DopplerSpectrum) {
    egui::Grid::new("gate_spectrum_moments")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Spectrum").strong());
            ui.label(egui::RichText::new("Power").strong());
            ui.label(egui::RichText::new("Mean velocity").strong());
            ui.label(egui::RichText::new("Width").strong());
            ui.end_row();
            spectrum_moment_row(ui, "True signal", spectrum.true_moments);
            spectrum_moment_row(ui, "Aliased signal", spectrum.aliased_signal_moments);
            spectrum_moment_row(ui, "Measured", spectrum.measured_moments);
            spectrum_moment_row(ui, "Noise-subtracted", spectrum.noise_subtracted_moments);
        });
}

fn spectrum_moment_row(ui: &mut egui::Ui, label: &str, moments: SpectrumMoments) {
    ui.label(label);
    ui.label(format!("{:.5e}", moments.total_power));
    ui.label(format!("{:.3} m/s", moments.mean_velocity_mps));
    ui.label(format!("{:.3} m/s", moments.spectrum_width_mps));
    ui.end_row();
}

fn spectrum_plot(ui: &mut egui::Ui, spectrum: &DopplerSpectrum, visible: &SpectrumVisibility) {
    let size = egui::vec2(ui.available_width().max(420.0), 300.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    painter.rect_stroke(
        rect,
        4.0,
        visuals.widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let plot = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(48.0, 12.0),
        rect.right_bottom() - egui::vec2(12.0, 28.0),
    );
    if plot.width() <= 20.0 || plot.height() <= 20.0 {
        return;
    }
    let x_min = spectrum
        .true_velocity_centers_mps
        .first()
        .copied()
        .into_iter()
        .chain(spectrum.output_velocity_centers_mps.first().copied())
        .fold(f64::INFINITY, f64::min);
    let x_max = spectrum
        .true_velocity_centers_mps
        .last()
        .copied()
        .into_iter()
        .chain(spectrum.output_velocity_centers_mps.last().copied())
        .fold(f64::NEG_INFINITY, f64::max);
    if !x_min.is_finite() || !x_max.is_finite() || x_max <= x_min {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "Invalid spectrum velocity support",
            egui::FontId::proportional(12.0),
            visuals.error_fg_color,
        );
        return;
    }
    let visible_max = visible_spectrum_max(spectrum, visible).max(f64::MIN_POSITIVE);
    let grid = visuals
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.7);
    let weak = visuals.weak_text_color();
    for db in [-60.0, -40.0, -20.0, 0.0] {
        let y = plot.bottom() - ((db + 60.0) / 60.0) as f32 * plot.height();
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(1.0, grid),
        );
        painter.text(
            egui::pos2(plot.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            format!("{db:.0}"),
            egui::FontId::monospace(10.0),
            weak,
        );
    }
    for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let x = plot.left() + fraction * plot.width();
        let velocity = x_min + f64::from(fraction) * (x_max - x_min);
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, grid.gamma_multiply(0.65)),
        );
        painter.text(
            egui::pos2(x, plot.bottom() + 7.0),
            egui::Align2::CENTER_TOP,
            format!("{velocity:.1}"),
            egui::FontId::monospace(10.0),
            weak,
        );
    }

    let draw = |centers: &[f64], powers: &[f64], color: egui::Color32, width: f32| {
        draw_spectrum_series(
            &painter,
            plot,
            centers,
            powers,
            x_min,
            x_max,
            visible_max,
            color,
            width,
        );
    };
    if visible.true_signal {
        draw(
            &spectrum.true_velocity_centers_mps,
            &spectrum.true_signal_power,
            egui::Color32::from_rgb(84, 184, 255),
            1.8,
        );
    }
    if visible.aliased_signal {
        draw(
            &spectrum.output_velocity_centers_mps,
            &spectrum.aliased_signal_power,
            egui::Color32::from_rgb(181, 120, 255),
            1.8,
        );
    }
    if visible.white_noise {
        draw(
            &spectrum.output_velocity_centers_mps,
            &spectrum.white_noise_power,
            egui::Color32::from_rgb(142, 151, 163),
            1.0,
        );
    }
    if visible.measured {
        draw(
            &spectrum.output_velocity_centers_mps,
            &spectrum.measured_power,
            egui::Color32::from_rgb(255, 183, 77),
            1.7,
        );
    }
    if visible.noise_subtracted {
        draw(
            &spectrum.output_velocity_centers_mps,
            &spectrum.noise_subtracted_power,
            egui::Color32::from_rgb(75, 210, 151),
            1.6,
        );
    }
    for (index, species) in spectrum.species.iter().enumerate() {
        if !visible.selected_species.contains(&species.name) {
            continue;
        }
        let color = species_color(index);
        if visible.species_true {
            draw(
                &spectrum.true_velocity_centers_mps,
                &species.true_power,
                color,
                1.2,
            );
        }
        if visible.species_aliased {
            draw(
                &spectrum.output_velocity_centers_mps,
                &species.aliased_power,
                color.gamma_multiply(0.72),
                1.2,
            );
        }
    }
    painter.text(
        egui::pos2(plot.center().x, rect.bottom() - 2.0),
        egui::Align2::CENTER_BOTTOM,
        "radial velocity (m/s)",
        egui::FontId::proportional(10.0),
        weak,
    );
    painter.text(
        egui::pos2(rect.left() + 4.0, plot.center().y),
        egui::Align2::LEFT_CENTER,
        "dB rel.",
        egui::FontId::proportional(10.0),
        weak,
    );

    if response.hovered()
        && let Some(position) = ui.ctx().pointer_hover_pos()
        && plot.contains(position)
    {
        let fraction = ((position.x - plot.left()) / plot.width()).clamp(0.0, 1.0);
        let velocity = x_min + f64::from(fraction) * (x_max - x_min);
        painter.line_segment(
            [
                egui::pos2(position.x, plot.top()),
                egui::pos2(position.x, plot.bottom()),
            ],
            egui::Stroke::new(1.0, visuals.text_color().gamma_multiply(0.45)),
        );
        response.on_hover_text(format!("{velocity:.3} m/s"));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_spectrum_series(
    painter: &egui::Painter,
    rect: egui::Rect,
    centers: &[f64],
    powers: &[f64],
    x_min: f64,
    x_max: f64,
    maximum_power: f64,
    color: egui::Color32,
    width: f32,
) {
    if centers.len() != powers.len() {
        return;
    }
    let points = centers
        .iter()
        .zip(powers)
        .filter_map(|(&velocity, &power)| {
            if !velocity.is_finite() || !power.is_finite() || power <= 0.0 {
                return None;
            }
            let x_fraction = ((velocity - x_min) / (x_max - x_min)).clamp(0.0, 1.0);
            let db_relative = (10.0 * (power / maximum_power).log10()).clamp(-60.0, 0.0);
            let y_fraction = (db_relative + 60.0) / 60.0;
            Some(egui::pos2(
                rect.left() + x_fraction as f32 * rect.width(),
                rect.bottom() - y_fraction as f32 * rect.height(),
            ))
        })
        .collect::<Vec<_>>();
    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, egui::Stroke::new(width, color)));
    } else if let Some(point) = points.first() {
        painter.circle_filled(*point, width.max(1.5), color);
    }
}

fn visible_spectrum_max(spectrum: &DopplerSpectrum, visible: &SpectrumVisibility) -> f64 {
    let mut maximum = 0.0_f64;
    let mut include = |powers: &[f64]| {
        maximum = powers
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(maximum, f64::max);
    };
    if visible.true_signal {
        include(&spectrum.true_signal_power);
    }
    if visible.aliased_signal {
        include(&spectrum.aliased_signal_power);
    }
    if visible.white_noise {
        include(&spectrum.white_noise_power);
    }
    if visible.measured {
        include(&spectrum.measured_power);
    }
    if visible.noise_subtracted {
        include(&spectrum.noise_subtracted_power);
    }
    for species in &spectrum.species {
        if !visible.selected_species.contains(&species.name) {
            continue;
        }
        if visible.species_true {
            include(&species.true_power);
        }
        if visible.species_aliased {
            include(&species.aliased_power);
        }
    }
    maximum
}

fn species_color(index: usize) -> egui::Color32 {
    const COLORS: [egui::Color32; 8] = [
        egui::Color32::from_rgb(70, 191, 152),
        egui::Color32::from_rgb(242, 166, 78),
        egui::Color32::from_rgb(221, 101, 121),
        egui::Color32::from_rgb(86, 156, 214),
        egui::Color32::from_rgb(177, 119, 214),
        egui::Color32::from_rgb(225, 211, 93),
        egui::Color32::from_rgb(90, 202, 221),
        egui::Color32::from_rgb(198, 146, 91),
    ];
    COLORS[index % COLORS.len()]
}

fn unavailable_result_ui(ui: &mut egui::Ui, reason: &WhyThisGateUnavailable) {
    let (title, detail) = match reason {
        WhyThisGateUnavailable::NotSyntheticRadar => (
            "Not a reconstructable synthetic gate",
            "The displayed volume is not a BowEcho synthetic-radar product.",
        ),
        WhyThisGateUnavailable::SourceSnapshotExpired => (
            "Source snapshot expired",
            "Refresh the synthetic frame, then request the gate explanation again.",
        ),
        WhyThisGateUnavailable::SourceFileUnavailable => (
            "Source file unavailable",
            "The WRF source used to build this frame is no longer readable.",
        ),
        WhyThisGateUnavailable::StaleFrameWitness => (
            "Frame changed before reconstruction",
            "The result was withheld rather than attaching another frame's physics to this gate.",
        ),
        WhyThisGateUnavailable::UnsupportedSourceContract => (
            "Source contract cannot reconstruct this gate",
            "This synthetic volume predates or omits the retained-source contract required for exact reconstruction.",
        ),
        WhyThisGateUnavailable::WorkerFailed(error) => ("Reconstruction failed", error.as_str()),
    };
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(194, 139, 38, 20))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.label(egui::RichText::new(detail).small().weak());
        });
}

fn explanation_provenance_ui(ui: &mut egui::Ui, provenance: &[String]) {
    egui::CollapsingHeader::new(format!("Explanation provenance ({})", provenance.len()))
        .default_open(false)
        .show(ui, |ui| {
            if provenance.is_empty() {
                ui.label(egui::RichText::new("No additional worker provenance returned").weak());
            } else {
                for line in provenance {
                    ui.label(egui::RichText::new(line).small().monospace());
                }
            }
        });
}

fn extraction_error_ui(ui: &mut egui::Ui, error: &GateSnapshotError) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(194, 72, 78, 20))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new("Gate selection is outside retained geometry")
                    .strong()
                    .color(ui.visuals().error_fg_color),
            );
            ui.label(error.to_string());
        });
}

fn empty_ui(ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("No synthetic gate is attached").strong());
        ui.label(
            egui::RichText::new(
                "Open Why This Gate? from a synthetic-radar readout so main can supply the volume and exact cut/radial-row/gate selection.",
            )
            .small()
            .weak(),
        );
    });
}

fn field_label(ui: &mut egui::Ui, field: GateFieldValue, unit: &str) {
    let response = ui.label(format_optional_f32(field.value, unit));
    if let Some(reason) = field.unavailable {
        response.on_hover_text(reason.label());
    }
}

fn metric(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).small().weak());
        ui.label(egui::RichText::new(value).strong().monospace());
    });
}

fn provenance_row(ui: &mut egui::Ui, label: &str, value: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(format!("{label}:")).small().strong());
        ui.label(
            egui::RichText::new(value.unwrap_or("not embedded"))
                .small()
                .monospace(),
        );
    });
}

fn format_optional_f32(value: Option<f32>, unit: &str) -> String {
    value.map_or_else(
        || "--".to_owned(),
        |value| {
            if unit.is_empty() {
                format!("{value:.4}")
            } else {
                format!("{value:.3} {unit}")
            }
        },
    )
}

fn format_optional_f64(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(
        || "--".to_owned(),
        |value| {
            if unit.is_empty() {
                format!("{value:.4}")
            } else {
                format!("{value:.3} {unit}")
            }
        },
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use radar_core::{MomentStorage, RadarSite, Radial};

    use super::*;

    fn grid(
        moment: MomentType,
        range: GateRange,
        radial_indices: Vec<usize>,
        values: Vec<f32>,
    ) -> MomentGrid {
        MomentGrid {
            moment,
            gate_range: range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices,
            storage: MomentStorage::F32(values),
        }
    }

    fn volume() -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new("KWHY"),
            Utc.with_ymd_and_hms(2026, 7, 12, 12, 0, 0).unwrap(),
        );
        let range = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 250,
            gate_count: 2,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 10.0,
            elevation_deg: 0.5,
            time_offset_ms: 500,
            gate_range: range.clone(),
            nyquist_velocity_mps: Some(15.0),
            radial_status: None,
        });
        cut.radials.push(Radial {
            azimuth_deg: 20.0,
            elevation_deg: 0.6,
            time_offset_ms: 1_500,
            gate_range: range.clone(),
            nyquist_velocity_mps: Some(20.0),
            radial_status: None,
        });
        for (moment, values) in [
            (MomentType::Reflectivity, vec![40.0, 41.0]),
            (MomentType::Unknown("IREF".to_owned()), vec![42.0, 43.0]),
            (MomentType::Unknown("MREF".to_owned()), vec![41.0, 42.0]),
            (MomentType::Unknown("MCOV".to_owned()), vec![0.8, 0.9]),
            (MomentType::Unknown("TUNB".to_owned()), vec![0.7, 0.6]),
            (MomentType::Unknown("MSIG".to_owned()), vec![0.5, 0.4]),
        ] {
            cut.moments
                .insert(moment.clone(), grid(moment, range.clone(), vec![1], values));
        }
        volume.cuts.push(cut);
        volume
    }

    #[test]
    fn extraction_uses_anchor_row_mapping_and_exact_gate() {
        let volume = volume();
        let selection = GateSelection::new(0, 0, 1).with_anchor(MomentType::Reflectivity);
        let snapshot = extract_gate_snapshot(&volume, &selection).unwrap();

        assert_eq!(snapshot.identity.radial_index, 1);
        assert_eq!(snapshot.identity.gate_index, 1);
        assert_eq!(snapshot.identity.slant_range_m, 500.0);
        assert_eq!(snapshot.identity.azimuth_deg, 20.0);
        assert_eq!(snapshot.nyquist_velocity_mps, Some(20.0));
        assert_eq!(snapshot.ray_offset_ms, 1_500);
        assert_eq!(snapshot.quality[0].fraction.value, Some(0.9));
        assert_eq!(snapshot.stages[0].ideal.value, Some(43.0));
        assert_eq!(snapshot.stages[0].measured.value, Some(42.0));
        assert_eq!(snapshot.stages[0].presented.value, Some(41.0));
    }

    #[test]
    fn mismatched_gate_geometry_is_disclosed_not_resampled() {
        let mut volume = volume();
        let mismatched = GateRange {
            first_gate_m: 250,
            gate_spacing_m: 500,
            gate_count: 2,
        };
        let moment = MomentType::Unknown("MREF".to_owned());
        volume.cuts[0].moments.insert(
            moment.clone(),
            grid(moment, mismatched, vec![1], vec![99.0, 100.0]),
        );
        let snapshot = extract_gate_snapshot(
            &volume,
            &GateSelection::new(0, 0, 1).with_anchor(MomentType::Reflectivity),
        )
        .unwrap();

        assert_eq!(snapshot.stages[0].measured.value, None);
        assert_eq!(
            snapshot.stages[0].measured.unavailable,
            Some(GateFieldUnavailable::GeometryMismatch)
        );
    }

    #[test]
    fn invalid_radial_row_and_gate_fail_at_selection_boundary() {
        let volume = volume();
        assert!(matches!(
            extract_gate_snapshot(
                &volume,
                &GateSelection::new(0, 1, 0).with_anchor(MomentType::Reflectivity)
            ),
            Err(GateSnapshotError::RadialRowOutOfRange { .. })
        ));
        assert!(matches!(
            extract_gate_snapshot(
                &volume,
                &GateSelection::new(0, 0, 2).with_anchor(MomentType::Reflectivity)
            ),
            Err(GateSnapshotError::GateOutOfRange { .. })
        ));
    }

    #[test]
    fn radial_membership_mismatch_is_not_row_position_substituted() {
        let mut volume = volume();
        let range = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .unwrap()
            .gate_range
            .clone();
        let moment = MomentType::Unknown("IREF".to_owned());
        volume.cuts[0].moments.insert(
            moment.clone(),
            grid(moment, range, vec![0], vec![80.0, 81.0]),
        );
        let snapshot = extract_gate_snapshot(
            &volume,
            &GateSelection::new(0, 0, 0).with_anchor(MomentType::Reflectivity),
        )
        .unwrap();
        assert_eq!(snapshot.stages[0].ideal.value, None);
        assert_eq!(
            snapshot.stages[0].ideal.unavailable,
            Some(GateFieldUnavailable::RadialNotPresent)
        );
    }

    #[test]
    fn asynchronous_results_are_generation_and_identity_checked() {
        let volume = Arc::new(volume());
        let selection = GateSelection::new(0, 0, 1).with_anchor(MomentType::Reflectivity);
        let mut state = SimradarGateInspectorState::default();
        state.open_with_gate(volume, selection);
        let identity = state.current_identity().unwrap();
        state.queue_explanation(identity);
        let request = state.take_explanation_request().unwrap();

        assert!(!state.supply_explanation(
            request.generation.wrapping_add(1),
            WhyThisGate::Unavailable(WhyThisGateUnavailable::WorkerFailed("stale".to_owned()))
        ));
        assert!(!state.supply_explanation(
            request.generation,
            WhyThisGate::Loading(GateIdentity {
                gate_index: identity.gate_index + 1,
                ..identity
            })
        ));
        assert!(state.supply_explanation(
            request.generation,
            WhyThisGate::Unavailable(WhyThisGateUnavailable::SourceSnapshotExpired)
        ));
    }
}
