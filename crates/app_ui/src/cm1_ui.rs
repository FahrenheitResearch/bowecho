//! First-class CM1 native-output import UI and background store writer.
//!
//! CM1 files are deliberately kept out of the generic WRF/NetCDF route:
//! native CM1 coordinates are local Cartesian and do not contain a map
//! projection. The user must explicitly choose a domain-centre anchor and a
//! moving-domain placement policy before a scalar plane can enter the model
//! store.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use app_ui::cm1::{
    self, Cm1Availability, Cm1DomainMotion, Cm1FileLayout, Cm1Inventory, Cm1Placement,
    Cm1PlacementMode, Cm1VariableRole,
};
use chrono::{DateTime, Utc};
use eframe::egui;
use rustwx_core::{GridProjection, GridShape, LatLonGrid};
use rw_store::{
    DerivedFieldInput, RwsExactTime, atomic::atomic_write_bytes,
    write_hour_from_grid_with_derived_exact,
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct Cm1ImportTask {
    pub rx: Receiver<Cm1ImportMessage>,
}

// This short-lived channel message is consumed once per poll; the summary
// payload is never stored in a collection.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Cm1ImportMessage {
    Progress(String),
    Done(Result<Cm1ImportSummary, String>),
}

#[derive(Debug, Clone)]
pub struct Cm1ImportSummary {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub hours_written: usize,
    pub hour: rw_ui::HourKey,
    native_variable: String,
    native_long_name: Option<String>,
    native_units: Option<String>,
    native_level_index: Option<usize>,
    native_nominal_level_m: Option<f64>,
    plane_statistics: Cm1PlaneStatistics,
}

#[derive(Debug, Clone)]
struct Cm1PlaneStatistics {
    total_values: usize,
    finite_values: usize,
    minimum: Option<f64>,
    maximum: Option<f64>,
}

#[derive(Debug, Clone)]
struct Cm1CompletionPlane {
    variable: String,
    long_name: Option<String>,
    units: Option<String>,
    level_index: Option<usize>,
    nominal_level_m: Option<f64>,
    statistics: Cm1PlaneStatistics,
}

impl Cm1CompletionPlane {
    fn from_plane(plane: &cm1::Cm1NativePlane) -> Self {
        Self {
            variable: plane.variable.clone(),
            long_name: plane.long_name.clone(),
            units: plane.units.clone(),
            level_index: plane.level_index,
            nominal_level_m: plane.nominal_level_m,
            statistics: Cm1PlaneStatistics::from_values(&plane.values),
        }
    }
}

impl Cm1ImportSummary {
    /// Human-facing completion text. Store slugs and provenance paths are
    /// intentionally omitted: after an import, the useful facts are what was
    /// opened in Models and whether the plane actually contains variation.
    pub fn completion_message(&self) -> String {
        let field = match self.native_long_name.as_deref() {
            Some(long_name) if !long_name.eq_ignore_ascii_case(&self.native_variable) => {
                format!("{long_name} ({})", self.native_variable)
            }
            _ => self.native_variable.clone(),
        };
        let level = self.native_level_index.map_or_else(String::new, |level| {
            self.native_nominal_level_m.map_or_else(
                || format!(", native level k={level}"),
                |height| format!(", native level k={level} ({height:.1} m nominal)"),
            )
        });
        let frames = if self.hours_written == 1 {
            "1 frame".to_owned()
        } else {
            format!("{} frames", self.hours_written)
        };
        format!(
            "Opened CM1 {field}{level} in Models ({frames}) — {}",
            self.plane_statistics.describe(self.native_units.as_deref())
        )
    }
}

impl Cm1PlaneStatistics {
    fn from_values(values: &[f64]) -> Self {
        let mut minimum: Option<f64> = None;
        let mut maximum: Option<f64> = None;
        let mut finite_values = 0usize;
        for &value in values.iter().filter(|value| value.is_finite()) {
            finite_values += 1;
            minimum = Some(minimum.map_or(value, |current| current.min(value)));
            maximum = Some(maximum.map_or(value, |current| current.max(value)));
        }
        Self {
            total_values: values.len(),
            finite_values,
            minimum,
            maximum,
        }
    }

    fn describe(&self, units: Option<&str>) -> String {
        let suffix = units
            .filter(|units| !units.trim().is_empty())
            .map(|units| format!(" {units}"))
            .unwrap_or_default();
        match (self.minimum, self.maximum) {
            (None, None) => format!(
                "no finite values in {} cells, so the plot is blank",
                self.total_values
            ),
            (Some(minimum), Some(maximum)) if minimum == maximum => format!(
                "constant {}{suffix}, so the plot is correctly a single color",
                format_plot_value(minimum)
            ),
            (Some(minimum), Some(maximum)) => {
                let missing = self.total_values.saturating_sub(self.finite_values);
                let missing_note = if missing > 0 {
                    format!("; {missing} missing cell(s)")
                } else {
                    String::new()
                };
                format!(
                    "range {} to {}{suffix}{missing_note}",
                    format_plot_value(minimum),
                    format_plot_value(maximum)
                )
            }
            _ => "value range unavailable".to_owned(),
        }
    }
}

/// Immutable snapshot of every scientific and placement choice made in the
/// panel. The worker never reads mutable UI state.
#[derive(Debug, Clone)]
pub struct Cm1ImportRequest {
    pub source_path: PathBuf,
    pub inventory: Cm1Inventory,
    pub variable: String,
    /// Ordered, immutable native output-record selections. Each becomes one
    /// ordinal rw-store slot with its own exact physical time.
    pub time_indices: Vec<usize>,
    /// Record to select after the run lands (must be in `time_indices`).
    pub display_time_index: usize,
    pub level_index: Option<usize>,
    pub placement: Cm1Placement,
}

#[derive(Debug)]
struct Cm1Inspection {
    inventory: Cm1Inventory,
    diagnostic_files: Vec<PathBuf>,
    diagnostic_note: Option<String>,
}

#[derive(Debug)]
enum InspectMode {
    Inventory,
    AttachDiagnostics,
}

#[derive(Debug)]
struct InspectTask {
    rx: Receiver<Result<Cm1Inspection, String>>,
}

#[derive(Debug)]
struct ProfileTask {
    rx: Receiver<Result<cm1::Cm1NativeColumnProfile, String>>,
}

#[derive(Debug)]
struct ThermodynamicTask {
    rx: Receiver<Result<cm1::Cm1ThermodynamicColumn, String>>,
}

#[derive(Debug, Default)]
pub struct Cm1ImportPanel {
    pub open: bool,
    source_path: Option<PathBuf>,
    inspect_task: Option<InspectTask>,
    inventory: Option<Cm1Inventory>,
    diagnostic_files: Vec<PathBuf>,
    diagnostic_note: Option<String>,
    selected_variable: Option<String>,
    time_index: usize,
    import_all_times: bool,
    radar_all_times: bool,
    level_index: usize,
    anchor_latitude: String,
    anchor_longitude: String,
    placement_mode: Option<Cm1PlacementMode>,
    assume_flat_radar_terrain: bool,
    pending_radar_request: Option<crate::wrf_radar::Cm1RadarRequest>,
    profile_x_index: usize,
    profile_y_index: usize,
    profile_task: Option<ProfileTask>,
    profile: Option<cm1::Cm1NativeColumnProfile>,
    profile_message: Option<String>,
    accept_default_thermodynamic_constants: bool,
    thermodynamic_task: Option<ThermodynamicTask>,
    thermodynamic_profile: Option<cm1::Cm1ThermodynamicColumn>,
    thermodynamic_message: Option<String>,
    message: Option<String>,
}

impl Cm1ImportPanel {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn take_radar_request(&mut self) -> Option<crate::wrf_radar::Cm1RadarRequest> {
        self.pending_radar_request.take()
    }

    pub fn show_window(
        &mut self,
        ctx: &egui::Context,
        import_busy: bool,
        shared_import_message: Option<&str>,
    ) -> Option<Cm1ImportRequest> {
        self.poll_inspection(ctx);
        self.poll_profile(ctx);
        self.poll_thermodynamic_profile(ctx);
        if !self.open {
            return None;
        }
        let mut open = true;
        let mut request = None;
        egui::Window::new("CM1 native output")
            .open(&mut open)
            .default_size([820.0, 720.0])
            .min_size([560.0, 440.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.heading("CM1 import");
                    ui.weak("Native NCAR CM1 horizontal fields with explicit world placement");
                });
                ui.label(
                    egui::RichText::new(
                        "CM1 supplies a local Cartesian grid, not map-projected latitude/longitude. BowEcho will not infer a location from ctrlat/ctrlon; staggered u/v/w are explicitly averaged onto the scalar grid.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);

                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            self.inspect_task.is_none() && !import_busy,
                            egui::Button::new("Open cm1out file…"),
                        )
                        .clicked()
                        && let Some(path) = pick_cm1_file()
                    {
                        self.begin_inspection(path, InspectMode::Inventory, ctx);
                    }
                    if let Some(path) = &self.source_path {
                        ui.monospace(path.display().to_string());
                    } else {
                        ui.weak("No file selected");
                    }
                });

                if self.inspect_task.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading CM1 schema and native variable inventory…");
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                if let Some(message) = &self.message {
                    ui.label(egui::RichText::new(message).small());
                }

                let Some(inventory) = self.inventory.clone() else {
                    return;
                };
                ui.separator();
                self.inventory_summary(ui, &inventory, ctx);
                ui.add_space(8.0);
                self.selection_ui(ui, &inventory, ctx);
                ui.add_space(8.0);
                self.placement_ui(ui, &inventory);
                ui.add_space(8.0);

                ui.separator();
                ui.strong("Plot in Models");
                ui.label(
                    egui::RichText::new(
                        "Store exactly the selected field, output time, and native level, then open the resulting field in Models.",
                    )
                    .small()
                    .weak(),
                );

                let validation = self.build_request(&inventory);
                if let Err(reason) = &validation {
                    ui.label(egui::RichText::new(reason).small().color(ui.visuals().warn_fg_color));
                }
                if let Some(status) = shared_import_message {
                    ui.label(egui::RichText::new(status).small().weak());
                }
                let import_label = if self.import_all_times {
                    "Build exact-time loop and open in Models"
                } else {
                    "Plot selected field in Models"
                };
                if ui
                    .add_enabled(
                        !import_busy && validation.is_ok(),
                        egui::Button::new(import_label),
                    )
                    .on_hover_text(
                        "Reads the immutable native field/time/level selection and writes it under model=cm1 with exact physical times and a provenance sidecar.",
                    )
                    .clicked()
                {
                    request = validation.ok();
                    self.message = Some("CM1 import queued…".to_owned());
                }

                ui.add_space(8.0);
                self.radar_ui(ui, &inventory, import_busy);
            });
        self.open = open;
        request
    }

    fn begin_inspection(&mut self, path: PathBuf, mode: InspectMode, ctx: &egui::Context) {
        self.source_path = Some(path.clone());
        if matches!(mode, InspectMode::Inventory) {
            self.inventory = None;
            self.selected_variable = None;
            self.time_index = 0;
            self.import_all_times = false;
            self.radar_all_times = false;
            self.level_index = 0;
            self.anchor_latitude.clear();
            self.anchor_longitude.clear();
            self.placement_mode = None;
            self.assume_flat_radar_terrain = false;
            self.pending_radar_request = None;
            self.diagnostic_files.clear();
            self.profile_x_index = 0;
            self.profile_y_index = 0;
            self.profile_task = None;
            self.profile = None;
            self.profile_message = None;
            self.accept_default_thermodynamic_constants = false;
            self.thermodynamic_task = None;
            self.thermodynamic_profile = None;
            self.thermodynamic_message = None;
        }
        self.message = Some(match mode {
            InspectMode::Inventory => "Inspecting native CM1 metadata…".to_owned(),
            InspectMode::AttachDiagnostics => {
                "Matching official cm1out_diag files by exact elapsed time…".to_owned()
            }
        });
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("bowecho-cm1-inspect".to_owned())
            .spawn(move || {
                let result = inspect_cm1(&path, matches!(mode, InspectMode::AttachDiagnostics));
                let _ = tx.send(result.map_err(|error| error.to_string()));
            })
            .expect("spawn CM1 inspection worker");
        self.inspect_task = Some(InspectTask { rx });
        ctx.request_repaint();
    }

    fn poll_inspection(&mut self, ctx: &egui::Context) {
        let Some(task) = self.inspect_task.as_ref() else {
            return;
        };
        match task.rx.try_recv() {
            Ok(Ok(result)) => {
                // A freshly opened evolving simulation should show its most
                // mature available state, not the usually quiet initialization
                // record. Attaching motion diagnostics to an already inspected
                // file must preserve the user's explicit time selection.
                let initialize_selection = self.inventory.is_none();
                self.diagnostic_files = result.diagnostic_files;
                self.diagnostic_note = result.diagnostic_note;
                if self.selected_variable.as_deref().is_none_or(|selected| {
                    result
                        .inventory
                        .variable(selected)
                        .is_none_or(|variable| !variable.role.is_horizontal_plane_compatible())
                }) {
                    self.selected_variable = preferred_horizontal_variable(&result.inventory)
                        .map(|variable| variable.name.clone());
                }
                if initialize_selection {
                    self.time_index = default_cm1_time_index(&result.inventory);
                }
                self.message = Some(format!(
                    "CM1 inventory ready: {} compatible horizontal field(s), {} record(s); selected record index {}",
                    result.inventory.horizontal_plane_variables().count(),
                    result.inventory.time.record_count,
                    self.time_index,
                ));
                self.inventory = Some(result.inventory);
                self.inspect_task = None;
                ctx.request_repaint();
            }
            Ok(Err(error)) => {
                self.message = Some(format!("CM1 inspection failed: {error}"));
                self.inspect_task = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.message = Some("CM1 inspection worker stopped unexpectedly".to_owned());
                self.inspect_task = None;
                ctx.request_repaint();
            }
        }
    }

    fn begin_profile(
        &mut self,
        inventory: Cm1Inventory,
        variable: String,
        time_index: usize,
        x_index: usize,
        y_index: usize,
        ctx: &egui::Context,
    ) {
        self.profile = None;
        self.profile_message = Some(format!(
            "Reading native {variable} column at x={x_index}, y={y_index}..."
        ));
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("bowecho-cm1-profile".to_owned())
            .spawn(move || {
                crate::wrf_process::lower_import_thread_priority();
                let result = (|| {
                    let nc = netcrust::open(&inventory.source_path)
                        .map_err(|error| error.to_string())?;
                    cm1::read_native_column_profile(
                        &nc, &inventory, &variable, time_index, x_index, y_index,
                    )
                    .map_err(|error| error.to_string())
                })();
                let _ = tx.send(result);
            })
            .expect("spawn CM1 profile worker");
        self.profile_task = Some(ProfileTask { rx });
        ctx.request_repaint();
    }

    fn poll_profile(&mut self, ctx: &egui::Context) {
        let Some(task) = self.profile_task.as_ref() else {
            return;
        };
        match task.rx.try_recv() {
            Ok(Ok(profile)) => {
                self.profile_message = Some(format!(
                    "Loaded {} native model level(s) from {}.",
                    profile.values.len(),
                    profile.variable
                ));
                self.profile = Some(profile);
                self.profile_task = None;
                ctx.request_repaint();
            }
            Ok(Err(error)) => {
                self.profile_message = Some(format!("CM1 column read failed: {error}"));
                self.profile = None;
                self.profile_task = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.profile_message = Some("CM1 column worker stopped unexpectedly".to_owned());
                self.profile = None;
                self.profile_task = None;
                ctx.request_repaint();
            }
        }
    }

    fn begin_thermodynamic_profile(
        &mut self,
        inventory: Cm1Inventory,
        time_index: usize,
        x_index: usize,
        y_index: usize,
        ctx: &egui::Context,
    ) {
        self.thermodynamic_profile = None;
        self.thermodynamic_message = Some(format!(
            "Deriving native thermodynamic column at x={x_index}, y={y_index}..."
        ));
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("bowecho-cm1-thermodynamic-profile".to_owned())
            .spawn(move || {
                crate::wrf_process::lower_import_thread_priority();
                let result = (|| {
                    let nc = netcrust::open(&inventory.source_path)
                        .map_err(|error| error.to_string())?;
                    cm1::read_thermodynamic_column(
                        &nc,
                        &inventory,
                        time_index,
                        x_index,
                        y_index,
                        cm1::Cm1ThermodynamicConstants::official_defaults(),
                    )
                    .map_err(|error| error.to_string())
                })();
                let _ = tx.send(result);
            })
            .expect("spawn CM1 thermodynamic-profile worker");
        self.thermodynamic_task = Some(ThermodynamicTask { rx });
        ctx.request_repaint();
    }

    fn poll_thermodynamic_profile(&mut self, ctx: &egui::Context) {
        let Some(task) = self.thermodynamic_task.as_ref() else {
            return;
        };
        match task.rx.try_recv() {
            Ok(Ok(profile)) => {
                self.thermodynamic_message = Some(format!(
                    "Derived {} native thermodynamic level(s); {} level(s) contain unavailable values.",
                    profile.pressure_hpa.len(),
                    profile.invalid_levels.len()
                ));
                self.thermodynamic_profile = Some(profile);
                self.thermodynamic_task = None;
                ctx.request_repaint();
            }
            Ok(Err(error)) => {
                self.thermodynamic_message =
                    Some(format!("CM1 thermodynamic profile failed: {error}"));
                self.thermodynamic_profile = None;
                self.thermodynamic_task = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.thermodynamic_message =
                    Some("CM1 thermodynamic-profile worker stopped unexpectedly".to_owned());
                self.thermodynamic_profile = None;
                self.thermodynamic_task = None;
                ctx.request_repaint();
            }
        }
    }

    fn inventory_summary(
        &mut self,
        ui: &mut egui::Ui,
        inventory: &Cm1Inventory,
        ctx: &egui::Context,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("Detected native CM1");
            ui.label(format!(
                "{:?} · {:?} · version {}",
                inventory.detection.confidence,
                inventory.topology.family,
                inventory.version.as_deref().unwrap_or("not declared")
            ));
        });
        match &inventory.file_layout {
            Cm1FileLayout::CompleteDomain { nx, ny } => {
                ui.weak(format!("Complete domain: {nx} × {ny} scalar cells"));
            }
            Cm1FileLayout::MpiTile {
                local_nx,
                local_ny,
                global_nx,
                global_ny,
                process_index,
                ..
            } => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "MPI tile {:?}: local {local_nx} × {local_ny}, global {global_nx} × {global_ny}. Tile assembly is required before plotting.",
                        process_index
                    ),
                );
            }
            Cm1FileLayout::Unresolved { reason } => {
                ui.colored_label(ui.visuals().warn_fg_color, reason);
            }
        }

        let motion_label = match &inventory.motion.domain_motion {
            Cm1DomainMotion::Static => "Domain motion: static".to_owned(),
            Cm1DomainMotion::ExplicitDisplacement { east_source, .. } => {
                format!("Domain motion: exact positions attached ({east_source})")
            }
            Cm1DomainMotion::Unresolved { reason } => {
                format!("Domain motion: moving, Fixed world unavailable — {reason}")
            }
        };
        ui.label(egui::RichText::new(motion_label).small());
        if !self.diagnostic_files.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.weak(format!(
                    "Found {} official cm1out_diag file(s) beside the output.",
                    self.diagnostic_files.len()
                ));
                if ui
                    .add_enabled(
                        self.inspect_task.is_none(),
                        egui::Button::new("Attach exact diagnostic positions"),
                    )
                    .clicked()
                    && let Some(path) = self.source_path.clone()
                {
                    self.begin_inspection(path, InspectMode::AttachDiagnostics, ctx);
                }
            });
        }
        if let Some(note) = &self.diagnostic_note {
            ui.label(egui::RichText::new(note).small().weak());
        }

        let plane_count = inventory.horizontal_plane_variables().count();
        let radar_missing = cm1_radar_missing_fields(inventory);
        let sounding_ready = cm1::thermodynamic_readiness(inventory).can_derive_native_profile();
        ui.horizontal_wrapped(|ui| {
            ui.strong("File capabilities:");
            ui.label(format!("{plane_count} plottable native field(s)"));
            if radar_missing.is_empty() {
                ui.label("· native REF/VEL ready");
            } else {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("· radar unavailable ({})", radar_missing.join(", ")),
                );
            }
            if sounding_ready {
                ui.label("· sounding data ready");
            } else {
                ui.weak("· sounding unavailable");
            }
        });
    }

    fn selection_ui(&mut self, ui: &mut egui::Ui, inventory: &Cm1Inventory, ctx: &egui::Context) {
        ui.strong("Native horizontal plane");
        let plottable = inventory.horizontal_plane_variables().collect::<Vec<_>>();
        let selected_label = self
            .selected_variable
            .as_deref()
            .and_then(|name| inventory.variable(name))
            .map(variable_display_label)
            .unwrap_or_else(|| "Choose a native scalar".to_owned());
        egui::ComboBox::from_id_salt("cm1_native_scalar")
            .selected_text(selected_label)
            .width(360.0)
            .show_ui(ui, |ui| {
                for variable in &plottable {
                    let label = variable_display_label(variable);
                    if ui
                        .selectable_value(
                            &mut self.selected_variable,
                            Some(variable.name.clone()),
                            label,
                        )
                        .clicked()
                    {
                        self.level_index = 0;
                        self.profile = None;
                    }
                }
            });
        let selected = self
            .selected_variable
            .as_deref()
            .and_then(|name| inventory.variable(name));
        if let Some(variable) = selected {
            ui.label(
                egui::RichText::new(format!(
                    "Selected plot field: {}. Change this before using Plot in Models below.",
                    variable_display_label(variable)
                ))
                .small(),
            );
            ui.label(
                egui::RichText::new(format!(
                    "{} · units {} · dims {:?} · shape {:?}",
                    match variable.role {
                        Cm1VariableRole::NativeScalar2D => "2-D scalar",
                        Cm1VariableRole::NativeScalar3D => "3-D scalar",
                        Cm1VariableRole::NativeXStaggered3D => "3-D x-staggered; averaged to xh",
                        Cm1VariableRole::NativeYStaggered3D => "3-D y-staggered; averaged to yh",
                        Cm1VariableRole::NativeZStaggered3D => "3-D z-staggered; averaged to zh",
                        _ => "unsupported",
                    },
                    variable.units.as_deref().unwrap_or("not declared"),
                    variable.dimensions,
                    variable.shape
                ))
                .small()
                .weak(),
            );
        }

        ui.horizontal(|ui| {
            ui.label("Selected record index");
            let time_label = cm1_time_label(inventory, self.time_index);
            egui::ComboBox::from_id_salt("cm1_output_time")
                .selected_text(time_label)
                .show_ui(ui, |ui| {
                    for index in 0..inventory.time.record_count {
                        if ui
                            .selectable_value(
                                &mut self.time_index,
                                index,
                                cm1_time_label(inventory, index),
                            )
                            .clicked()
                        {
                            self.profile = None;
                            self.thermodynamic_profile = None;
                        }
                    }
                });
        });
        if inventory.time.record_count > 1 {
            ui.label(
                egui::RichText::new(format!(
                    "New files open on the final output ({}), which is normally the most evolved state. Output 0 is the initialization record and may be meteorologically quiet.",
                    default_cm1_time_index(inventory)
                ))
                .small()
                .weak(),
            );
            if self.time_index == 0 {
                ui.label(
                    egui::RichText::new(
                        "Output 0 is selected. A uniform plot or radar can be the model's real initialization state; choose a later output to inspect the evolved storm.",
                    )
                    .small()
                    .color(ui.visuals().warn_fg_color),
                );
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("Model-field import scope");
                ui.selectable_value(&mut self.import_all_times, false, "Selected record");
                ui.selectable_value(
                    &mut self.import_all_times,
                    true,
                    format!("All {} records (loop)", inventory.time.record_count),
                )
                .on_hover_text(
                    "Write the ordered CM1 time axis into one exact-time model run. Every frame must use a bit-identical placed grid.",
                );
            });
        }

        if let Some(variable) = selected
            && matches!(
                variable.role,
                Cm1VariableRole::NativeScalar3D
                    | Cm1VariableRole::NativeXStaggered3D
                    | Cm1VariableRole::NativeYStaggered3D
                    | Cm1VariableRole::NativeZStaggered3D
            )
        {
            let levels = inventory.axes.zh.raw_values.len();
            if self.level_index >= levels {
                self.level_index = 0;
            }
            ui.horizontal(|ui| {
                ui.label("Native level");
                egui::ComboBox::from_id_salt("cm1_native_level")
                    .selected_text(cm1_level_label(inventory, self.level_index))
                    .show_ui(ui, |ui| {
                        for index in 0..levels {
                            ui.selectable_value(
                                &mut self.level_index,
                                index,
                                cm1_level_label(inventory, index),
                            );
                        }
                    });
            });
            let height_note = match &inventory.physical_height_variable {
                Cm1Availability::Available(name) => format!(
                    "The selector uses nominal zh. Terrain-following physical height is available separately as {name}."
                ),
                Cm1Availability::Unavailable { reason } => format!(
                    "Nominal zh only; terrain-following physical-height field unavailable: {reason}"
                ),
            };
            ui.label(egui::RichText::new(height_note).small().weak());

            self.column_profile_ui(ui, inventory, variable, ctx);
        }

        self.thermodynamic_profile_ui(ui, inventory, ctx);

        let unavailable = inventory
            .variables
            .iter()
            .filter(|variable| !variable.role.is_horizontal_plane_compatible())
            .collect::<Vec<_>>();
        egui::CollapsingHeader::new(format!(
            "Unavailable / non-scalar fields ({})",
            unavailable.len()
        ))
        .default_open(false)
        .show(ui, |ui| {
            for variable in unavailable {
                let reason = match &variable.role {
                    Cm1VariableRole::Coordinate => "coordinate axis".to_owned(),
                    Cm1VariableRole::Time => "time coordinate".to_owned(),
                    Cm1VariableRole::Metadata => "metadata".to_owned(),
                    Cm1VariableRole::Unsupported { reason } => reason.clone(),
                    role => format!("role {role:?}"),
                };
                ui.label(
                    egui::RichText::new(format!("{} — {reason}", variable.name))
                        .small()
                        .weak(),
                );
            }
        });
    }

    fn column_profile_ui(
        &mut self,
        ui: &mut egui::Ui,
        inventory: &Cm1Inventory,
        variable: &cm1::Cm1Variable,
        ctx: &egui::Context,
    ) {
        egui::CollapsingHeader::new("Native 3-D column / profile")
            .default_open(false)
            .show(ui, |ui| {
                let nx = inventory.axes.xh.raw_values.len();
                let ny = inventory.axes.yh.raw_values.len();
                self.profile_x_index = self.profile_x_index.min(nx.saturating_sub(1));
                self.profile_y_index = self.profile_y_index.min(ny.saturating_sub(1));
                ui.label(
                    egui::RichText::new(
                        "Browse the exact native model column. This preserves CM1 levels and does not invent pressure coordinates or an MSL vertical datum.",
                    )
                    .small()
                    .weak(),
                );
                ui.horizontal_wrapped(|ui| {
                    ui.label("Scalar cell");
                    let x_changed = ui
                        .add(
                        egui::DragValue::new(&mut self.profile_x_index)
                            .range(0..=nx.saturating_sub(1))
                            .prefix("x "),
                        )
                        .changed();
                    let y_changed = ui
                        .add(
                        egui::DragValue::new(&mut self.profile_y_index)
                            .range(0..=ny.saturating_sub(1))
                            .prefix("y "),
                        )
                        .changed();
                    if x_changed || y_changed {
                        self.profile = None;
                        self.thermodynamic_profile = None;
                    }
                    if ui
                        .add_enabled(
                            self.profile_task.is_none() && nx > 0 && ny > 0,
                            egui::Button::new("Read native column"),
                        )
                        .clicked()
                    {
                        self.begin_profile(
                            inventory.clone(),
                            variable.name.clone(),
                            self.time_index,
                            self.profile_x_index,
                            self.profile_y_index,
                            ctx,
                        );
                    }
                    if self.profile_task.is_some() {
                        ui.spinner();
                    }
                });
                if let Some(message) = &self.profile_message {
                    ui.label(egui::RichText::new(message).small());
                }
                let Some(profile) = &self.profile else {
                    return;
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{} at local x={:.3} km, y={:.3} km - units {} - {:?}",
                        profile.variable,
                        profile.local_x_m / 1_000.0,
                        profile.local_y_m / 1_000.0,
                        profile.units.as_deref().unwrap_or("not declared"),
                        profile.transform
                    ))
                    .small(),
                );
                match &profile.model_level_height_m {
                    Cm1Availability::Available(height) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Physical model-level height: {} ({})",
                                height.variable, height.interpretation
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    Cm1Availability::Unavailable { reason } => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Physical model-level height unavailable: {reason}"
                            ))
                            .small()
                            .weak(),
                        );
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt("cm1_native_column_rows")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        egui::Grid::new("cm1_native_column_grid")
                            .striped(true)
                            .min_col_width(100.0)
                            .show(ui, |ui| {
                                ui.strong("k");
                                ui.strong("nominal zh");
                                ui.strong("model height");
                                ui.strong("value");
                                ui.end_row();
                                for (level, value) in profile.values.iter().enumerate() {
                                    ui.monospace(level.to_string());
                                    ui.monospace(format_profile_height(
                                        profile
                                            .nominal_level_m
                                            .available()
                                            .map(Vec::as_slice),
                                        level,
                                    ));
                                    ui.monospace(format_profile_height(
                                        profile
                                            .model_level_height_m
                                            .available()
                                            .map(|height| height.values_m.as_slice()),
                                        level,
                                    ));
                                    ui.monospace(format_profile_value(*value));
                                    ui.end_row();
                                }
                            });
                    });
                ui.label(
                    egui::RichText::new(&profile.provenance)
                        .small()
                        .weak(),
                );
            });
    }

    fn thermodynamic_profile_ui(
        &mut self,
        ui: &mut egui::Ui,
        inventory: &Cm1Inventory,
        ctx: &egui::Context,
    ) {
        egui::CollapsingHeader::new("Meteorological profile readiness")
            .default_open(false)
            .show(ui, |ui| {
                let readiness = cm1::thermodynamic_readiness(inventory);
                ui.label(
                    egui::RichText::new(
                        "A real CM1 thermodynamic column requires exact total th/prs/qv, physical zhval, horizontal winds, and a defensible moving-frame correction.",
                    )
                    .small()
                    .weak(),
                );
                thermodynamic_field_status(
                    ui,
                    "Potential temperature",
                    &readiness.potential_temperature,
                );
                thermodynamic_field_status(ui, "Pressure", &readiness.pressure);
                thermodynamic_field_status(
                    ui,
                    "Water-vapor mixing ratio",
                    &readiness.water_vapor_mixing_ratio,
                );
                thermodynamic_field_status(ui, "Horizontal u", &readiness.grid_relative_u);
                thermodynamic_field_status(ui, "Horizontal v", &readiness.grid_relative_v);
                thermodynamic_field_status(
                    ui,
                    "Physical model height",
                    &readiness.model_level_height,
                );
                match &readiness.wind_frame_correction {
                    Cm1Availability::Available(cm1::Cm1WindFrameCorrection::StationaryDomain) => {
                        ui.label(
                            egui::RichText::new("Wind frame: ready - stationary domain")
                                .small(),
                        );
                    }
                    Cm1Availability::Available(
                        cm1::Cm1WindFrameCorrection::AddDomainVelocity { provenance, .. },
                    ) => {
                        ui.label(
                            egui::RichText::new(format!("Wind frame: ready - {provenance}"))
                                .small(),
                        );
                    }
                    Cm1Availability::Unavailable { reason } => {
                        ui.label(
                            egui::RichText::new(format!("Wind frame: unavailable - {reason}"))
                                .small()
                                .color(ui.visuals().warn_fg_color),
                        );
                    }
                }
                if let Some(reason) = readiness.sounding_viewer.unavailable_reason() {
                    ui.label(
                        egui::RichText::new(format!("Sounding viewer: not enabled - {reason}"))
                            .small()
                            .color(ui.visuals().warn_fg_color),
                    );
                }
                if !readiness.can_derive_native_profile() {
                    return;
                }
                ui.separator();
                ui.checkbox(
                    &mut self.accept_default_thermodynamic_constants,
                    "Use official CM1 default Rd/Cp/Rv constants",
                )
                .on_hover_text(
                    "Native cm1out does not record testcase. Testcase 4 and 5 override Cp, so this choice cannot claim those special-testcase constants are exact.",
                );
                ui.label(
                    egui::RichText::new(
                        "Default conversion: Rd=287.04, Cp=1005.7, Rv=461.5 J kg^-1 K^-1. Special testcase 4/5 constants cannot be identified from cm1out alone.",
                    )
                    .small()
                    .weak(),
                );
                let nx = inventory.axes.xh.raw_values.len();
                let ny = inventory.axes.yh.raw_values.len();
                self.profile_x_index = self.profile_x_index.min(nx.saturating_sub(1));
                self.profile_y_index = self.profile_y_index.min(ny.saturating_sub(1));
                ui.horizontal_wrapped(|ui| {
                    ui.label("Scalar cell");
                    let x_changed = ui
                        .add(
                            egui::DragValue::new(&mut self.profile_x_index)
                                .range(0..=nx.saturating_sub(1))
                                .prefix("x "),
                        )
                        .changed();
                    let y_changed = ui
                        .add(
                            egui::DragValue::new(&mut self.profile_y_index)
                                .range(0..=ny.saturating_sub(1))
                                .prefix("y "),
                        )
                        .changed();
                    if x_changed || y_changed {
                        self.profile = None;
                        self.thermodynamic_profile = None;
                    }
                    if ui
                        .add_enabled(
                            self.accept_default_thermodynamic_constants
                                && self.thermodynamic_task.is_none()
                                && nx > 0
                                && ny > 0,
                            egui::Button::new("Derive native thermodynamic profile"),
                        )
                        .clicked()
                    {
                        self.begin_thermodynamic_profile(
                            inventory.clone(),
                            self.time_index,
                            self.profile_x_index,
                            self.profile_y_index,
                            ctx,
                        );
                    }
                    if self.thermodynamic_task.is_some() {
                        ui.spinner();
                    }
                });
                if let Some(message) = &self.thermodynamic_message {
                    ui.label(egui::RichText::new(message).small());
                }
                let Some(profile) = &self.thermodynamic_profile else {
                    return;
                };
                egui::ScrollArea::vertical()
                    .id_salt("cm1_thermodynamic_column_rows")
                    .max_height(280.0)
                    .show(ui, |ui| {
                        egui::Grid::new("cm1_thermodynamic_column_grid")
                            .striped(true)
                            .show(ui, |ui| {
                                for heading in [
                                    "k",
                                    "model z",
                                    "p",
                                    "T",
                                    "Td",
                                    "u/v grid",
                                    "u/v east-north",
                                ] {
                                    ui.strong(heading);
                                }
                                ui.end_row();
                                for level in 0..profile.pressure_hpa.len() {
                                    ui.monospace(level.to_string());
                                    ui.monospace(format_profile_height(
                                        Some(profile.model_level_height_m.as_slice()),
                                        level,
                                    ));
                                    ui.monospace(format!(
                                        "{} hPa",
                                        format_profile_value(profile.pressure_hpa[level])
                                    ));
                                    ui.monospace(format!(
                                        "{} C",
                                        format_profile_value(profile.temperature_c[level])
                                    ));
                                    ui.monospace(format!(
                                        "{} C",
                                        format_profile_value(profile.dewpoint_c[level])
                                    ));
                                    ui.monospace(format!(
                                        "{}/{}",
                                        format_profile_value(profile.u_grid_relative_mps[level]),
                                        format_profile_value(profile.v_grid_relative_mps[level])
                                    ));
                                    ui.monospace(format!(
                                        "{}/{} m/s",
                                        format_profile_value(profile.u_east_mps[level]),
                                        format_profile_value(profile.v_north_mps[level])
                                    ));
                                    ui.end_row();
                                    if let Some((_, reason)) = profile
                                        .invalid_levels
                                        .iter()
                                        .find(|(invalid_level, _)| *invalid_level == level)
                                    {
                                        ui.weak("");
                                        ui.label(
                                            egui::RichText::new(reason)
                                                .small()
                                                .color(ui.visuals().warn_fg_color),
                                        );
                                        ui.end_row();
                                    }
                                }
                            });
                    });
                ui.label(
                    egui::RichText::new(&profile.provenance)
                        .small()
                        .weak(),
                );
            });
    }

    fn placement_ui(&mut self, ui: &mut egui::Ui, inventory: &Cm1Inventory) {
        ui.strong("World placement");
        ui.horizontal_wrapped(|ui| {
            ui.label("Domain-center latitude");
            ui.add(
                egui::TextEdit::singleline(&mut self.anchor_latitude)
                    .desired_width(90.0)
                    .hint_text("e.g. 35.0"),
            );
            ui.label("longitude");
            ui.add(
                egui::TextEdit::singleline(&mut self.anchor_longitude)
                    .desired_width(90.0)
                    .hint_text("e.g. -97.0"),
            );
        });
        ui.label(
            egui::RichText::new(
                "This is an explicit BowEcho domain-center anchor. It is not read from CM1 ctrlat/ctrlon.",
            )
            .small()
            .weak(),
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("Placement mode");
            ui.selectable_value(
                &mut self.placement_mode,
                Some(Cm1PlacementMode::FollowDomain),
                "Follow domain",
            )
            .on_hover_text(
                "Keep the computational grid pinned to the chosen anchor at every output time.",
            );
            let fixed_available = !matches!(
                inventory.motion.domain_motion,
                Cm1DomainMotion::Unresolved { .. }
            );
            ui.add_enabled_ui(fixed_available, |ui| {
                ui.selectable_value(
                    &mut self.placement_mode,
                    Some(Cm1PlacementMode::FixedWorld),
                    "Fixed world",
                )
                .on_hover_text(
                    "Preserve the exact native-domain displacement attached for this output time.",
                );
            });
        });
        if let Cm1DomainMotion::Unresolved { reason } = &inventory.motion.domain_motion {
            ui.label(
                egui::RichText::new(format!("Fixed world unavailable: {reason}"))
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
        if inventory.geographic_hints.control_latitude_deg.is_some()
            || inventory.geographic_hints.control_longitude_deg.is_some()
        {
            ui.label(
                egui::RichText::new(format!(
                    "File hints: ctrlat={:?}, ctrlon={:?}. CM1 documents these as whole-domain physics inputs, so BowEcho does not use them as geolocation.",
                    inventory.geographic_hints.control_latitude_deg,
                    inventory.geographic_hints.control_longitude_deg
                ))
                .small()
                .weak(),
            );
        }
    }

    fn radar_ui(&mut self, ui: &mut egui::Ui, inventory: &Cm1Inventory, import_busy: bool) {
        ui.separator();
        ui.strong("Native simulated radar");
        let missing = cm1_radar_missing_fields(inventory);
        if !missing.is_empty() {
            ui.label(
                egui::RichText::new("Radar is unavailable for this particular CM1 file.")
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
            ui.label(
                egui::RichText::new(format!(
                    "Required native radar input unavailable: {}. This does not prevent plotting the available horizontal fields above.",
                    missing.join(", ")
                ))
                .small()
                .weak(),
            );
            return;
        }
        ui.label(
            egui::RichText::new(
                "Build REF and radial velocity from the selected CM1 record, or process every record as one exact-time radar loop. The selected record is processed first so its first completed tilt appears immediately; an all-record loop is installed in native time order when complete.",
            )
            .small(),
        );

        if inventory.time.record_count > 1 {
            ui.horizontal_wrapped(|ui| {
                ui.label("Radar time scope");
                ui.selectable_value(
                    &mut self.radar_all_times,
                    false,
                    format!("Selected record index {}", self.time_index),
                );
                ui.selectable_value(
                    &mut self.radar_all_times,
                    true,
                    format!(
                        "All {} records (ordered loop)",
                        inventory.time.record_count
                    ),
                )
                .on_hover_text(
                    "Process one CM1 record at a time, retain each completed radar volume, and install one loop ordered by exact CM1 valid time.",
                );
            });
        } else {
            self.radar_all_times = false;
        }

        let native_terrain = inventory.variable("zs").is_some();
        if native_terrain {
            self.assume_flat_radar_terrain = false;
            ui.label(
                egui::RichText::new("Terrain: native CM1 zs in the same model-z datum as zhval.")
                    .small()
                    .weak(),
            );
        } else {
            ui.checkbox(
                &mut self.assume_flat_radar_terrain,
                "This is a flat idealized domain; explicitly use model-z = 0 terrain",
            )
            .on_hover_text(
                "Required only when the file has no native zs field. BowEcho never assumes flat terrain silently.",
            );
        }
        ui.label(
            egui::RichText::new(
                "Fixed for CM1: scalar native reflectivity, CPU sampling, frozen single-time atmosphere, standard 4/3-Earth beam geometry. Dual-pol, T-matrix, WRF refractivity, Stoelinga recomputation, and adjacent-WRF interpolation are unavailable.",
            )
            .small()
            .weak(),
        );
        ui.label(
            egui::RichText::new(
                "The CM1 virtual radar is placed at the center of this explicitly placed domain. Scan, range, gate, blockage, noise, and presentation controls come from the WRF simulated-radar control panel; saved WRF site coordinates and incompatible WRF-only science controls do not carry into CM1.",
            )
            .small()
            .weak(),
        );

        let validation = self.build_radar_request(inventory);
        if let Err(reason) = &validation {
            ui.label(
                egui::RichText::new(reason)
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
        if ui
            .add_enabled(
                !import_busy && validation.is_ok(),
                egui::Button::new(if self.radar_all_times {
                    format!(
                        "Build all {} records as a REF/VEL loop",
                        inventory.time.record_count
                    )
                } else {
                    format!(
                        "Build record index {} native REF/VEL in Radar",
                        self.time_index
                    )
                }),
            )
            .clicked()
        {
            self.pending_radar_request = validation.ok();
            self.message = Some("CM1 native radar queued...".to_owned());
        }
    }

    fn build_placement(&self, inventory: &Cm1Inventory) -> Result<Cm1Placement, String> {
        let latitude = parse_coordinate(&self.anchor_latitude, "latitude", -90.0, 90.0)?;
        let longitude = parse_coordinate(&self.anchor_longitude, "longitude", -180.0, 180.0)?;
        let mode = self
            .placement_mode
            .ok_or_else(|| "Choose Follow domain or Fixed world.".to_owned())?;
        let placement = Cm1Placement {
            mode,
            anchor_latitude_deg: latitude,
            anchor_longitude_deg: longitude,
        };
        cm1::georeference_scalar_grid(inventory, &placement, self.time_index)
            .map_err(|error| error.to_string())?;
        Ok(placement)
    }

    fn build_radar_request(
        &self,
        inventory: &Cm1Inventory,
    ) -> Result<crate::wrf_radar::Cm1RadarRequest, String> {
        if inventory.file_layout.requires_tile_assembly() {
            return Err(
                "This is one CM1 MPI tile; assemble the complete domain before radar sampling."
                    .to_owned(),
            );
        }
        if let Cm1FileLayout::Unresolved { reason } = &inventory.file_layout {
            return Err(format!("CM1 file layout is unresolved: {reason}"));
        }
        let source_path = self
            .source_path
            .clone()
            .ok_or_else(|| "Choose a CM1 output file.".to_owned())?;
        let missing = cm1_radar_missing_fields(inventory);
        if !missing.is_empty() {
            return Err(format!(
                "Native radar is unavailable for this file: {}.",
                missing.join(", ")
            ));
        }
        let time_indices = if self.radar_all_times {
            (0..inventory.time.record_count).collect::<Vec<_>>()
        } else {
            vec![self.time_index]
        };
        for &time_index in &time_indices {
            exact_time(inventory, time_index)?;
        }
        let placement = self.build_placement(inventory)?;
        for &time_index in &time_indices {
            cm1::georeference_scalar_grid(inventory, &placement, time_index)
                .map_err(|error| error.to_string())?;
        }
        let terrain_policy = if inventory.variable("zs").is_some() {
            cm1::Cm1TerrainPolicy::RequireNative
        } else if self.assume_flat_radar_terrain {
            cm1::Cm1TerrainPolicy::AssumeFlatModelZero
        } else {
            return Err(
                "No native zs terrain field: explicitly confirm flat model-z = 0 terrain to continue."
                    .to_owned(),
            );
        };
        Ok(crate::wrf_radar::Cm1RadarRequest {
            source_path,
            inventory: inventory.clone(),
            placement,
            time_indices,
            display_time_index: self.time_index,
            terrain_policy,
        })
    }

    fn build_request(&self, inventory: &Cm1Inventory) -> Result<Cm1ImportRequest, String> {
        if inventory.file_layout.requires_tile_assembly() {
            return Err(
                "This is one CM1 MPI tile; assemble the complete domain before plotting."
                    .to_owned(),
            );
        }
        if let Cm1FileLayout::Unresolved { reason } = &inventory.file_layout {
            return Err(format!("CM1 file layout is unresolved: {reason}"));
        }
        let source_path = self
            .source_path
            .clone()
            .ok_or_else(|| "Choose a CM1 output file.".to_owned())?;
        let variable = self
            .selected_variable
            .clone()
            .ok_or_else(|| "Choose a native scalar field.".to_owned())?;
        let metadata = inventory
            .variable(&variable)
            .ok_or_else(|| "The selected field is no longer in the inventory.".to_owned())?;
        let level_index = match metadata.role {
            Cm1VariableRole::NativeScalar2D => None,
            Cm1VariableRole::NativeScalar3D
            | Cm1VariableRole::NativeXStaggered3D
            | Cm1VariableRole::NativeYStaggered3D
            | Cm1VariableRole::NativeZStaggered3D => Some(self.level_index),
            _ => return Err("Choose a native scalar field.".to_owned()),
        };
        let placement = self.build_placement(inventory)?;
        let mode = placement.mode;
        let time_indices = if self.import_all_times {
            (0..inventory.time.record_count).collect::<Vec<_>>()
        } else {
            vec![self.time_index]
        };
        if time_indices.len() > usize::from(u16::MAX) + 1 {
            return Err(format!(
                "CM1 run has {} records; one BowEcho run supports at most {} ordered slots.",
                time_indices.len(),
                usize::from(u16::MAX) + 1
            ));
        }
        for &time_index in &time_indices {
            exact_time(inventory, time_index)?;
        }
        if self.import_all_times
            && mode == Cm1PlacementMode::FixedWorld
            && let Cm1DomainMotion::ExplicitDisplacement {
                east_m, north_m, ..
            } = &inventory.motion.domain_motion
            && (east_m
                .windows(2)
                .any(|pair| pair[0].to_bits() != pair[1].to_bits())
                || north_m
                    .windows(2)
                    .any(|pair| pair[0].to_bits() != pair[1].to_bits()))
        {
            return Err(
                "Moving Fixed-world frames do not share one grid. Choose Follow domain for a loop, or import one selected record."
                    .to_owned(),
            );
        }
        Ok(Cm1ImportRequest {
            source_path,
            inventory: inventory.clone(),
            variable,
            time_indices,
            display_time_index: self.time_index,
            level_index,
            placement,
        })
    }
}

fn default_cm1_time_index(inventory: &Cm1Inventory) -> usize {
    // The official CM1 time axis is ordered by elapsed simulation time. Avoid
    // guessing "storminess" from one field (or eagerly reading a large 3-D
    // volume during metadata inspection); the final recorded state is the
    // deterministic, scientifically transparent default.
    inventory.time.record_count.saturating_sub(1)
}

fn preferred_horizontal_variable(inventory: &Cm1Inventory) -> Option<&cm1::Cm1Variable> {
    // NetCDF variable iteration order is not a scientific preference and can
    // differ between classic/HDF5 writers. Pick a useful, already-centred
    // diagnostic deterministically before falling back to file order.
    const PREFERRED: &[&str] = &[
        "cref", "dbz", "winterp", "zvort", "thpert", "th", "prspert", "uinterp", "vinterp", "w",
    ];
    PREFERRED
        .iter()
        .find_map(|name| {
            inventory
                .variable(name)
                .filter(|variable| variable.role.is_horizontal_plane_compatible())
        })
        .or_else(|| inventory.horizontal_plane_variables().next())
}

fn variable_display_label(variable: &cm1::Cm1Variable) -> String {
    match variable.long_name.as_deref() {
        Some(long_name) if !long_name.eq_ignore_ascii_case(&variable.name) => {
            format!("{long_name} ({})", variable.name)
        }
        _ => variable.name.clone(),
    }
}

fn cm1_radar_missing_fields(inventory: &Cm1Inventory) -> Vec<String> {
    let has_role = |name: &str, role: &Cm1VariableRole| {
        inventory
            .variable(name)
            .is_some_and(|variable| &variable.role == role)
    };
    let mut missing = Vec::new();
    if !has_role("dbz", &Cm1VariableRole::NativeScalar3D) {
        missing.push("3-D dbz".to_owned());
    }
    if inventory.physical_height_variable.available().is_none() {
        missing.push("3-D zhval heights".to_owned());
    }
    if !has_role("uinterp", &Cm1VariableRole::NativeScalar3D)
        && !has_role("u", &Cm1VariableRole::NativeXStaggered3D)
    {
        missing.push("3-D u wind".to_owned());
    }
    if !has_role("vinterp", &Cm1VariableRole::NativeScalar3D)
        && !has_role("v", &Cm1VariableRole::NativeYStaggered3D)
    {
        missing.push("3-D v wind".to_owned());
    }
    if !has_role("winterp", &Cm1VariableRole::NativeScalar3D)
        && !has_role("w", &Cm1VariableRole::NativeZStaggered3D)
    {
        missing.push("3-D w wind".to_owned());
    }
    let readiness = cm1::thermodynamic_readiness(inventory);
    if let Some(reason) = readiness.wind_frame_correction.unavailable_reason() {
        missing.push(format!("wind-frame correction: {reason}"));
    }
    missing
}

pub fn spawn_import(request: Cm1ImportRequest, store_root: PathBuf) -> Cm1ImportTask {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("bowecho-cm1-import".to_owned())
        .spawn(move || {
            crate::wrf_process::lower_import_thread_priority();
            let mut progress = |message: String| {
                let _ = tx.send(Cm1ImportMessage::Progress(message));
            };
            let result = import_selected_plane(&request, &store_root, &mut progress);
            let _ = tx.send(Cm1ImportMessage::Done(result));
        })
        .expect("spawn CM1 import worker");
    Cm1ImportTask { rx }
}

fn inspect_cm1(path: &Path, attach_diagnostics: bool) -> Result<Cm1Inspection, cm1::Cm1Error> {
    let mut inventory = cm1::inspect_path(path)?;
    let diagnostic_files = path
        .parent()
        .map(cm1::diagnostic_files_in_folder)
        .unwrap_or_default();
    let diagnostic_note = if attach_diagnostics {
        let attachment = cm1::attach_motion_diagnostics(&mut inventory, &diagnostic_files)?;
        Some(format!(
            "Attached {} exact diagnostic position sample(s); no velocity integration or time interpolation was used.",
            attachment.matched_times_seconds.len()
        ))
    } else {
        None
    };
    Ok(Cm1Inspection {
        inventory,
        diagnostic_files,
        diagnostic_note,
    })
}

fn import_selected_plane(
    request: &Cm1ImportRequest,
    store_root: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<Cm1ImportSummary, String> {
    validate_request_times(request)?;
    let total = request.time_indices.len();
    progress(format!(
        "CM1: preflighting {total} ordered frame(s) for one bit-identical placed grid"
    ));
    let mut exact_times: Vec<(usize, RwsExactTime)> = Vec::with_capacity(total);
    let mut reference_georef = None;
    let mut reference_offset: Option<(f64, f64)> = None;
    for &time_index in &request.time_indices {
        let exact = exact_time(&request.inventory, time_index)?;
        if let Some((previous_index, previous)) = exact_times.last()
            && previous.lead_seconds >= exact.lead_seconds
        {
            return Err(format!(
                "CM1 exact times are not strictly increasing: output {previous_index} is {} s and output {time_index} is {} s.",
                previous.lead_seconds, exact.lead_seconds
            ));
        }
        let offset = request
            .inventory
            .placement_offset_m(request.placement.mode, time_index)
            .map_err(|error| error.to_string())?;
        if let (Some(reference), Some(reference_offset)) = (&reference_georef, reference_offset) {
            // Identical placement offsets feed identical x/y axes and anchor
            // into the deterministic georeference, so the f32 grid is
            // necessarily bit-identical without repeating millions of trig
            // operations for every Follow-domain frame. Changed offsets are
            // evaluated and compared at the actual stored f32 precision.
            if offset.0.to_bits() != reference_offset.0.to_bits()
                || offset.1.to_bits() != reference_offset.1.to_bits()
            {
                let georef = cm1::georeference_scalar_grid(
                    &request.inventory,
                    &request.placement,
                    time_index,
                )
                .map_err(|error| error.to_string())?;
                if !georeferenced_grids_bit_identical(reference, &georef) {
                    return Err(format!(
                        "CM1 output {time_index} does not share a bit-identical placed grid with output {}. Moving Fixed-world frames must be imported separately; choose Follow domain for one loop.",
                        request.time_indices[0]
                    ));
                }
            }
        } else {
            reference_georef = Some(
                cm1::georeference_scalar_grid(&request.inventory, &request.placement, time_index)
                    .map_err(|error| error.to_string())?,
            );
            reference_offset = Some(offset);
        }
        exact_times.push((time_index, exact));
    }
    let georef = reference_georef.ok_or_else(|| "No CM1 output times were selected.".to_owned())?;
    let shape = GridShape::new(georef.nx, georef.ny).map_err(|error| error.to_string())?;
    let grid = LatLonGrid::new(shape, georef.lat_deg.clone(), georef.lon_deg.clone())
        .map_err(|error| error.to_string())?;
    let nc = netcrust::open(&request.source_path).map_err(|error| error.to_string())?;
    let field_name = stored_field_name(&request.variable, request.level_index);
    let run = run_name(request);
    let projection = GridProjection::Geographic;
    let mut representative_plane = None;
    let mut display_plane = None;
    let mut hours_written = 0usize;
    for (slot, &(time_index, exact)) in exact_times.iter().enumerate() {
        progress(format!(
            "CM1 frame {}/{}: reading {} at output {}{}",
            slot + 1,
            total,
            request.variable,
            time_index,
            request
                .level_index
                .map(|level| format!(", native level {level}"))
                .unwrap_or_default()
        ));
        let result = (|| {
            let plane = cm1::read_horizontal_mass_grid_plane(
                &nc,
                &request.inventory,
                &request.variable,
                time_index,
                request.level_index,
            )
            .map_err(|error| error.to_string())?;
            let values = values_f32(&request.variable, &plane.values)?;
            let derived = [DerivedFieldInput {
                name: &field_name,
                units: plane.units.as_deref().unwrap_or("unknown"),
                values: &values,
            }];
            progress(format!(
                "CM1 frame {}/{}: writing exact-time slot {slot} to model=cm1 run={run}",
                slot + 1,
                total
            ));
            write_hour_from_grid_with_derived_exact(
                store_root,
                "cm1",
                &run,
                u16::try_from(slot).expect("request time count is bounded"),
                exact,
                &grid,
                Some(&projection),
                &[],
                &derived,
                &[],
                concat!("bowecho-cm1-import-", env!("CARGO_PKG_VERSION")),
                now_unix(),
            )
            .map_err(|error| error.to_string())?;
            Ok::<_, String>(plane)
        })();
        match result {
            Ok(plane) => {
                hours_written += 1;
                if time_index == request.display_time_index {
                    display_plane = Some(Cm1CompletionPlane::from_plane(&plane));
                }
                representative_plane.get_or_insert(plane);
            }
            Err(error) => {
                return Err(format!(
                    "CM1 import stopped after writing {hours_written}/{total} frame(s): {error}"
                ));
            }
        }
    }
    let representative_plane = representative_plane
        .ok_or_else(|| "CM1 import produced no representative field plane.".to_owned())?;
    let display_plane = display_plane
        .ok_or_else(|| "CM1 import did not read the requested display plane.".to_owned())?;

    let provenance_path = store_root
        .join("cm1")
        .join(&run)
        .join("cm1-provenance.json");
    if let Err(error) = write_provenance(
        &provenance_path,
        request,
        &representative_plane,
        &georef.provenance,
        &field_name,
        &exact_times,
    ) {
        return Err(format!(
            "CM1 wrote {hours_written}/{total} frame(s), but the provenance sidecar failed: {error}"
        ));
    }
    let display_slot = request
        .time_indices
        .iter()
        .position(|&index| index == request.display_time_index)
        .ok_or_else(|| "CM1 display time is not part of the imported selection.".to_owned())?;
    let display_exact = exact_times[display_slot].1;
    let hour = rw_ui::HourKey {
        model: "cm1".to_owned(),
        run: run.clone(),
        hour: u16::try_from(display_slot).expect("request time count is bounded"),
        exact_time: Some(display_exact),
    };
    Ok(Cm1ImportSummary {
        store_root: store_root.to_path_buf(),
        model: "cm1".to_owned(),
        run,
        hours_written,
        hour,
        native_variable: display_plane.variable,
        native_long_name: display_plane.long_name,
        native_units: display_plane.units,
        native_level_index: display_plane.level_index,
        native_nominal_level_m: display_plane.nominal_level_m,
        plane_statistics: display_plane.statistics,
    })
}

fn write_provenance(
    path: &Path,
    request: &Cm1ImportRequest,
    plane: &cm1::Cm1NativePlane,
    georeference_provenance: &str,
    stored_variable: &str,
    exact_times: &[(usize, RwsExactTime)],
) -> Result<(), String> {
    let motion = match &request.inventory.motion.domain_motion {
        Cm1DomainMotion::Static => serde_json::json!({ "kind": "static" }),
        Cm1DomainMotion::ExplicitDisplacement {
            east_source,
            north_source,
            ..
        } => serde_json::json!({
            "kind": "explicit_displacement",
            "east_source": east_source,
            "north_source": north_source,
        }),
        Cm1DomainMotion::Unresolved { reason } => {
            serde_json::json!({ "kind": "unresolved", "reason": reason })
        }
    };
    let metadata = request.inventory.variable(&request.variable);
    let document = serde_json::json!({
        "format": "bowecho-cm1-provenance-v1",
        "schema_source": cm1::CM1_SCHEMA_SOURCE,
        "source_path": request.source_path,
        "detection": {
            "confidence": format!("{:?}", request.inventory.detection.confidence),
            "evidence": request.inventory.detection.evidence,
            "missing_evidence": request.inventory.detection.missing_evidence,
            "cm1_version": request.inventory.version,
            "schema_family": format!("{:?}", request.inventory.topology.family),
        },
        "native_variable": {
            "name": request.variable,
            "long_name": plane.long_name,
            "units": plane.units,
            "dimensions": metadata.map(|value| &value.dimensions),
            "shape": metadata.map(|value| &value.shape),
            "stored_name": stored_variable,
            "grid_transform": format!("{:?}", plane.transform),
        },
        "selection": {
            "time_indices": request.time_indices,
            "display_time_index": request.display_time_index,
            "level_index": request.level_index,
            "nominal_level_m": plane.nominal_level_m,
            "frames": exact_times.iter().enumerate().map(|(slot, (time_index, exact))| serde_json::json!({
                "storage_slot": slot,
                "time_index": time_index,
                "lead_seconds": exact.lead_seconds,
                "valid_unix": exact.valid_unix,
            })).collect::<Vec<_>>(),
        },
        "placement": {
            "mode": format!("{:?}", request.placement.mode),
            "anchor_interpretation": "user-supplied CM1 domain center",
            "anchor_latitude_deg": request.placement.anchor_latitude_deg,
            "anchor_longitude_deg": request.placement.anchor_longitude_deg,
            "source_has_map_projection": false,
            "georeference_provenance": georeference_provenance,
            "motion": motion,
        },
    });
    let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
    atomic_write_bytes(path, &bytes)
        .map_err(|error| format!("atomically write {}: {error}", path.display()))
}

fn validate_request_times(request: &Cm1ImportRequest) -> Result<(), String> {
    match &request.inventory.file_layout {
        Cm1FileLayout::CompleteDomain { .. } => {}
        Cm1FileLayout::MpiTile { .. } => {
            return Err(
                "A single CM1 MPI tile cannot be imported as a complete domain.".to_owned(),
            );
        }
        Cm1FileLayout::Unresolved { reason } => {
            return Err(format!("CM1 file layout is unresolved: {reason}"));
        }
    }
    if request.time_indices.is_empty() {
        return Err("No CM1 output times were selected.".to_owned());
    }
    if request.time_indices.len() > usize::from(u16::MAX) + 1 {
        return Err(format!(
            "CM1 selection has {} records; one run supports at most {} ordered slots.",
            request.time_indices.len(),
            usize::from(u16::MAX) + 1
        ));
    }
    if request
        .time_indices
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
    {
        return Err("CM1 output selections must be unique and strictly ordered.".to_owned());
    }
    if request
        .time_indices
        .iter()
        .any(|&index| index >= request.inventory.time.record_count)
    {
        return Err(format!(
            "CM1 output selection exceeds the {}-record native time axis.",
            request.inventory.time.record_count
        ));
    }
    if !request.time_indices.contains(&request.display_time_index) {
        return Err("CM1 display time is not part of the immutable import selection.".to_owned());
    }
    let variable = request
        .inventory
        .variable(&request.variable)
        .ok_or_else(|| {
            format!(
                "CM1 variable {} is no longer inventoried.",
                request.variable
            )
        })?;
    match variable.role {
        Cm1VariableRole::NativeScalar2D if request.level_index.is_none() => {}
        Cm1VariableRole::NativeScalar3D
        | Cm1VariableRole::NativeXStaggered3D
        | Cm1VariableRole::NativeYStaggered3D
        | Cm1VariableRole::NativeZStaggered3D
            if request
                .level_index
                .is_some_and(|level| level < request.inventory.axes.zh.raw_values.len()) => {}
        _ => {
            return Err(format!(
                "CM1 variable {} and native level {:?} are not a valid scalar-grid plane selection.",
                request.variable, request.level_index
            ));
        }
    }
    Ok(())
}

fn georeferenced_grids_bit_identical(
    left: &cm1::Cm1GeoreferencedGrid,
    right: &cm1::Cm1GeoreferencedGrid,
) -> bool {
    left.nx == right.nx
        && left.ny == right.ny
        && left.lat_deg.len() == right.lat_deg.len()
        && left.lon_deg.len() == right.lon_deg.len()
        && left
            .lat_deg
            .iter()
            .zip(&right.lat_deg)
            .all(|(left, right)| left.to_bits() == right.to_bits())
        && left
            .lon_deg
            .iter()
            .zip(&right.lon_deg)
            .all(|(left, right)| left.to_bits() == right.to_bits())
}

fn values_f32(variable: &str, values: &[f64]) -> Result<Vec<f32>, String> {
    values
        .iter()
        .enumerate()
        .map(|(index, &value)| {
            if value.is_nan() {
                Ok(f32::NAN)
            } else if value.is_finite()
                && value >= f64::from(f32::MIN)
                && value <= f64::from(f32::MAX)
            {
                Ok(value as f32)
            } else {
                Err(format!(
                    "CM1 {variable} value at cell {index} cannot be represented as a finite f32: {value}"
                ))
            }
        })
        .collect()
}

fn exact_time(inventory: &Cm1Inventory, time_index: usize) -> Result<RwsExactTime, String> {
    let offsets = inventory.time.offsets_seconds.available().ok_or_else(|| {
        format!(
            "Exact CM1 time unavailable: {}",
            inventory
                .time
                .offsets_seconds
                .unavailable_reason()
                .unwrap_or("time offsets are not convertible to seconds")
        )
    })?;
    let &offset = offsets
        .get(time_index)
        .ok_or_else(|| format!("Output time index {time_index} is unavailable."))?;
    if !offset.is_finite() || offset < 0.0 {
        return Err(format!(
            "CM1 elapsed time {offset} s is not a finite nonnegative exact time."
        ));
    }
    let rounded = offset.round();
    if (offset - rounded).abs() > 1.0e-6 || rounded > u64::MAX as f64 {
        return Err(format!(
            "CM1 elapsed time {offset} s is not representable by the store's exact whole-second axis."
        ));
    }
    let lead_seconds = rounded as u64;
    let start_text = inventory
        .time
        .simulation_start_utc
        .available()
        .ok_or_else(|| {
            format!(
                "Exact UTC unavailable: {}",
                inventory
                    .time
                    .simulation_start_utc
                    .unavailable_reason()
                    .unwrap_or("CM1 start date/time globals are incomplete")
            )
        })?;
    let start = DateTime::parse_from_rfc3339(start_text)
        .map_err(|error| format!("CM1 simulation start `{start_text}` is invalid: {error}"))?
        .with_timezone(&Utc)
        .timestamp();
    let valid_unix = start
        .checked_add(
            i64::try_from(lead_seconds)
                .map_err(|_| "CM1 elapsed time exceeds the signed UTC range".to_owned())?,
        )
        .ok_or_else(|| "CM1 valid UTC overflows the signed timestamp range".to_owned())?;
    Ok(RwsExactTime::new(lead_seconds, valid_unix))
}

fn format_profile_height(levels_m: Option<&[f64]>, index: usize) -> String {
    levels_m
        .and_then(|levels| levels.get(index))
        .filter(|value| value.is_finite())
        .map(|value| format!("{value:.1} m"))
        .unwrap_or_else(|| "unavailable".to_owned())
}

fn thermodynamic_field_status(
    ui: &mut egui::Ui,
    label: &str,
    field: &Cm1Availability<cm1::Cm1ThermodynamicField>,
) {
    match field {
        Cm1Availability::Available(field) => {
            ui.label(
                egui::RichText::new(format!(
                    "{label}: ready - {} [{}] - {}",
                    field.variable, field.units, field.interpretation
                ))
                .small(),
            );
        }
        Cm1Availability::Unavailable { reason } => {
            ui.label(
                egui::RichText::new(format!("{label}: unavailable - {reason}"))
                    .small()
                    .color(ui.visuals().warn_fg_color),
            );
        }
    }
}

fn format_profile_value(value: f64) -> String {
    if !value.is_finite() {
        "missing".to_owned()
    } else if value != 0.0 && !(1.0e-3..1.0e5).contains(&value.abs()) {
        format!("{value:.6e}")
    } else {
        format!("{value:.6}")
    }
}

fn format_plot_value(value: f64) -> String {
    let value = if value == 0.0 { 0.0 } else { value };
    if value != 0.0 && !(1.0e-3..1.0e5).contains(&value.abs()) {
        format!("{value:.4e}")
    } else {
        format!("{value:.4}")
    }
}

fn cm1_time_label(inventory: &Cm1Inventory, index: usize) -> String {
    let elapsed = inventory
        .time
        .offsets_seconds
        .available()
        .and_then(|values| values.get(index))
        .map(|value| format!("{value} s"))
        .unwrap_or_else(|| "elapsed time unavailable".to_owned());
    match exact_time(inventory, index) {
        Ok(exact) => {
            let valid = DateTime::<Utc>::from_timestamp(exact.valid_unix, 0)
                .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| "UTC out of range".to_owned());
            format!("record {index} · {elapsed} · {valid}")
        }
        Err(_) => format!("record {index} · {elapsed}"),
    }
}

fn cm1_level_label(inventory: &Cm1Inventory, index: usize) -> String {
    inventory
        .axes
        .zh
        .values_m
        .available()
        .and_then(|values| values.get(index))
        .map(|height| format!("k={index} · nominal zh={height:.1} m"))
        .unwrap_or_else(|| format!("k={index} · nominal zh unavailable"))
}

fn parse_coordinate(text: &str, name: &str, min: f64, max: f64) -> Result<f64, String> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("Enter a numeric domain-center {name}."))?;
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(format!("Domain-center {name} must be in [{min}, {max}]."));
    }
    Ok(value)
}

fn stored_field_name(variable: &str, level_index: Option<usize>) -> String {
    match level_index {
        Some(level) => format!("cm1_{}_k{level:03}", sanitize_slug(variable)),
        None => format!("cm1_{}", sanitize_slug(variable)),
    }
}

fn run_name(request: &Cm1ImportRequest) -> String {
    let stem = request
        .source_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("cm1out");
    let config = format!(
        "{}|{}|{:?}|{}|{:?}|{:016x}|{:016x}",
        request.source_path.display(),
        request.variable,
        request.time_indices,
        request
            .level_index
            .map(|level| level.to_string())
            .unwrap_or_else(|| "surface".to_owned()),
        request.placement.mode,
        request.placement.anchor_latitude_deg.to_bits(),
        request.placement.anchor_longitude_deg.to_bits(),
    );
    let digest = Sha256::digest(config.as_bytes());
    let hash = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let level = request
        .level_index
        .map(|value| format!("k{value:03}"))
        .unwrap_or_else(|| "surface".to_owned());
    let mode = match request.placement.mode {
        Cm1PlacementMode::FixedWorld => "fixed",
        Cm1PlacementMode::FollowDomain => "follow",
    };
    let time_scope = if request.time_indices.len() == 1 {
        format!("t{:03}", request.time_indices[0])
    } else {
        format!("loop{:03}", request.time_indices.len())
    };
    let human = format!(
        "cm1_{}_{}_{}_{}_{}_{}",
        hash,
        sanitize_slug(stem),
        sanitize_slug(&request.variable),
        time_scope,
        level,
        mode
    );
    human.chars().take(120).collect()
}

fn sanitize_slug(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            output.push(character.to_ascii_lowercase());
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "field".to_owned()
    } else {
        output
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn pick_cm1_file() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open native CM1 output")
        .add_filter("CM1 NetCDF", &["nc", "nc4", "cdf"])
        .pick_file()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn pick_cm1_file() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_names_preserve_native_variable_and_level() {
        assert_eq!(stored_field_name("dbz", None), "cm1_dbz");
        assert_eq!(
            stored_field_name("theta pert", Some(4)),
            "cm1_theta_pert_k004"
        );
    }

    #[test]
    fn default_field_is_scientific_not_backend_iteration_order() {
        let inventory = cm1::inspect_path(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc"),
        )
        .expect("fixture inventory");
        assert_eq!(
            preferred_horizontal_variable(&inventory).map(|variable| variable.name.as_str()),
            Some("cref")
        );
    }

    #[test]
    fn evolving_cm1_file_defaults_to_final_output_record() {
        let inventory = cm1::inspect_path(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc"),
        )
        .expect("fixture inventory");
        assert_eq!(inventory.time.record_count, 2);
        assert_eq!(default_cm1_time_index(&inventory), 1);
    }

    #[test]
    fn radar_scope_is_independent_and_builds_selected_or_all_record_indices() {
        let source_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc");
        let inventory = cm1::inspect_path(&source_path).expect("fixture inventory");
        assert!(matches!(
            inventory.motion.domain_motion,
            Cm1DomainMotion::Unresolved { .. }
        ));
        assert!(
            inventory
                .motion
                .east_velocity_mps
                .available()
                .into_iter()
                .chain(inventory.motion.north_velocity_mps.available())
                .flatten()
                .any(|velocity| velocity.abs() > 1.0e-9),
            "fixture must exercise a nonzero moving frame"
        );
        assert!(
            cm1::thermodynamic_readiness(&inventory)
                .wind_frame_correction
                .available()
                .is_some(),
            "complete unit-bearing umove/vmove is a valid wind-frame correction"
        );
        assert!(
            cm1_radar_missing_fields(&inventory).is_empty(),
            "Follow-domain radar must not require accumulated displacement"
        );
        let mut panel = Cm1ImportPanel {
            source_path: Some(source_path),
            time_index: 1,
            import_all_times: false,
            radar_all_times: true,
            anchor_latitude: "35.0".to_owned(),
            anchor_longitude: "-97.0".to_owned(),
            placement_mode: Some(Cm1PlacementMode::FollowDomain),
            ..Cm1ImportPanel::default()
        };

        let loop_request = panel
            .build_radar_request(&inventory)
            .expect("all-record radar request");
        assert_eq!(loop_request.time_indices, vec![0, 1]);
        assert_eq!(loop_request.display_time_index, 1);

        panel.radar_all_times = false;
        let selected_request = panel
            .build_radar_request(&inventory)
            .expect("selected-record radar request");
        assert_eq!(selected_request.time_indices, vec![1]);
        assert_eq!(selected_request.display_time_index, 1);
        assert!(
            !panel.import_all_times,
            "model import scope must stay independent"
        );

        panel.placement_mode = Some(Cm1PlacementMode::FixedWorld);
        let fixed_error = panel
            .build_radar_request(&inventory)
            .expect_err("Fixed world still requires exact accumulated displacement");
        assert!(
            fixed_error.contains("domainlocx/domainlocy"),
            "{fixed_error}"
        );
    }

    #[test]
    fn moving_radar_reports_incomplete_or_missing_wind_frame_correction() {
        let source_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc");
        let base = cm1::inspect_path(&source_path).expect("fixture inventory");
        let panel = Cm1ImportPanel {
            source_path: Some(source_path),
            time_index: 1,
            anchor_latitude: "35.0".to_owned(),
            anchor_longitude: "-97.0".to_owned(),
            placement_mode: Some(Cm1PlacementMode::FollowDomain),
            ..Cm1ImportPanel::default()
        };
        let assert_blocked = |inventory: &Cm1Inventory| {
            let issues = cm1_radar_missing_fields(inventory);
            assert_eq!(issues.len(), 1, "unexpected radar readiness: {issues:?}");
            assert!(
                issues[0].contains("complete unit-bearing umove/vmove records are unavailable"),
                "{}",
                issues[0]
            );
            let user_error = panel
                .build_radar_request(inventory)
                .expect_err("invalid moving-frame correction must block radar");
            assert!(
                user_error.contains("complete unit-bearing umove/vmove records are unavailable"),
                "{user_error}"
            );
        };

        let mut incomplete = base.clone();
        incomplete.motion.east_velocity_mps = Cm1Availability::Available(vec![12.0]);
        assert_blocked(&incomplete);

        let mut missing = base;
        missing.motion.north_velocity_mps = Cm1Availability::Unavailable {
            reason: "vmove is missing from this test file".to_owned(),
        };
        assert_blocked(&missing);
    }

    #[test]
    fn optional_real_evolving_cm1_default_reads_the_storm_scene() {
        let Some(path) = std::env::var_os("BOWECHO_CM1_EVOLVING_FIXTURE").map(PathBuf::from) else {
            return;
        };
        let nc = netcrust::open(&path).expect("open evolving CM1 fixture");
        let inventory = cm1::inspect_file(&nc, &path).expect("inspect evolving CM1 fixture");
        let selected = default_cm1_time_index(&inventory);
        assert!(
            selected > 0,
            "evolving fixture must have a post-initialization record"
        );
        let scene = cm1::read_radar_scene(
            &nc,
            &inventory,
            &Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
            selected,
            cm1::Cm1TerrainPolicy::RequireNative,
        )
        .expect("read default evolving radar scene");
        let maximum_dbz = scene
            .dbz
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);
        let maximum_updraft_mps = scene
            .w_mps
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            maximum_dbz >= 20.0,
            "default scene max REF was {maximum_dbz}"
        );
        assert!(
            maximum_updraft_mps >= 5.0,
            "default scene max updraft was {maximum_updraft_mps} m/s"
        );
    }

    #[test]
    fn constant_plane_status_explains_single_color_plot() {
        let statistics = Cm1PlaneStatistics::from_values(&[0.0, -0.0, 0.0]);
        assert_eq!(
            statistics.describe(Some("m/s")),
            "constant 0.0000 m/s, so the plot is correctly a single color"
        );
    }

    #[test]
    fn exact_fixture_time_uses_official_start_and_elapsed_seconds() {
        let inventory = cm1::inspect_path(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc"),
        )
        .expect("fixture inventory");
        let exact = exact_time(&inventory, 1).expect("exact time");
        assert_eq!(exact.lead_seconds, 60);
        let declared_start = DateTime::parse_from_rfc3339(
            inventory
                .time
                .simulation_start_utc
                .available()
                .expect("fixture start"),
        )
        .expect("parse fixture start")
        .timestamp();
        assert_eq!(exact.origin_unix(), Some(declared_start));
    }

    #[test]
    fn selected_fixture_plane_writes_cm1_store_and_provenance() {
        let source_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc");
        let inventory = cm1::inspect_path(&source_path).expect("fixture inventory");
        let request = Cm1ImportRequest {
            source_path,
            inventory,
            variable: "u".to_owned(),
            time_indices: vec![1],
            display_time_index: 1,
            level_index: Some(1),
            placement: Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
        };
        let store_root = test_store_root("selected");
        let mut progress = Vec::new();
        let summary =
            import_selected_plane(&request, &store_root, &mut |message| progress.push(message))
                .expect("import fixture plane");
        assert_eq!(summary.model, "cm1");
        let completion = summary.completion_message();
        assert!(
            completion.contains("Opened CM1 u velocity (u)"),
            "{completion}"
        );
        assert!(completion.contains("in Models"), "{completion}");
        assert!(completion.contains("range"), "{completion}");
        assert_eq!(
            summary.hour.exact_time.map(|time| time.lead_seconds),
            Some(60)
        );
        assert!(
            store_root
                .join("cm1")
                .join(&summary.run)
                .join("f000.rws")
                .is_file()
        );
        let provenance = std::fs::read_to_string(
            store_root
                .join("cm1")
                .join(&summary.run)
                .join("cm1-provenance.json"),
        )
        .expect("read CM1 provenance sidecar");
        assert!(provenance.contains("bowecho-cm1-provenance-v1"));
        assert!(provenance.contains("\"name\": \"u\""));
        assert!(provenance.contains("DestaggeredX"));
        assert!(provenance.contains("FollowDomain"));
        assert!(!progress.is_empty());
        let _ = std::fs::remove_dir_all(store_root);
    }

    #[test]
    fn follow_domain_import_writes_ordered_exact_time_loop() {
        let source_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc");
        let inventory = cm1::inspect_path(&source_path).expect("fixture inventory");
        let request = Cm1ImportRequest {
            source_path,
            inventory,
            variable: "cref".to_owned(),
            time_indices: vec![0, 1],
            display_time_index: 1,
            level_index: None,
            placement: Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
        };
        let store_root = test_store_root("loop");
        let summary = import_selected_plane(&request, &store_root, &mut |_| {})
            .expect("import exact CM1 loop");
        let run_dir = store_root.join("cm1").join(&summary.run);
        assert_eq!(summary.hours_written, 2);
        assert_eq!(summary.hour.hour, 1);
        assert_eq!(
            summary.hour.exact_time.map(|time| time.lead_seconds),
            Some(60)
        );
        assert!(run_dir.join("f000.rws").is_file());
        assert!(run_dir.join("f001.rws").is_file());
        let provenance = std::fs::read_to_string(run_dir.join("cm1-provenance.json"))
            .expect("read loop provenance sidecar");
        assert!(provenance.contains("\"time_indices\": ["));
        assert!(provenance.contains("\"storage_slot\": 1"));
        let _ = std::fs::remove_dir_all(store_root);
    }

    #[test]
    fn moving_fixed_world_loop_fails_before_writing_any_slot() {
        let source_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cm1/cm1out_schema.nc");
        let mut inventory = cm1::inspect_path(&source_path).expect("fixture inventory");
        let diagnostics = cm1::diagnostic_files_in_folder(source_path.parent().expect("parent"));
        cm1::attach_motion_diagnostics(&mut inventory, &diagnostics)
            .expect("attach exact motion diagnostics");
        let request = Cm1ImportRequest {
            source_path,
            inventory,
            variable: "cref".to_owned(),
            time_indices: vec![0, 1],
            display_time_index: 0,
            level_index: None,
            placement: Cm1Placement {
                mode: Cm1PlacementMode::FixedWorld,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
        };
        let store_root = test_store_root("moving-fixed");
        let error = import_selected_plane(&request, &store_root, &mut |_| {})
            .expect_err("moving Fixed-world grids must not share one run");
        assert!(error.contains("bit-identical placed grid"), "{error}");
        assert!(!store_root.join("cm1").exists());
    }

    fn test_store_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bowecho-cm1-import-test-{label}-{}-{}",
            std::process::id(),
            now_unix()
        ))
    }
}
