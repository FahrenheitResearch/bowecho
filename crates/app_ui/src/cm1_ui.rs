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

#[derive(Debug)]
pub enum Cm1ImportMessage {
    Progress(String),
    Done(Result<Cm1ImportSummary, String>),
}

#[derive(Debug, Clone)]
pub struct Cm1ImportSummary {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub variable: String,
    pub hour: rw_ui::HourKey,
    pub provenance_path: PathBuf,
}

/// Immutable snapshot of every scientific and placement choice made in the
/// panel. The worker never reads mutable UI state.
#[derive(Debug, Clone)]
pub struct Cm1ImportRequest {
    pub source_path: PathBuf,
    pub inventory: Cm1Inventory,
    pub variable: String,
    pub time_index: usize,
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
    level_index: usize,
    anchor_latitude: String,
    anchor_longitude: String,
    placement_mode: Option<Cm1PlacementMode>,
    message: Option<String>,
}

impl Cm1ImportPanel {
    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn show_window(
        &mut self,
        ctx: &egui::Context,
        import_busy: bool,
        shared_import_message: Option<&str>,
    ) -> Option<Cm1ImportRequest> {
        self.poll_inspection(ctx);
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
                self.selection_ui(ui, &inventory);
                ui.add_space(8.0);
                self.placement_ui(ui, &inventory);
                ui.add_space(8.0);

                let validation = self.build_request(&inventory);
                if let Err(reason) = &validation {
                    ui.label(egui::RichText::new(reason).small().color(ui.visuals().warn_fg_color));
                }
                if let Some(status) = shared_import_message {
                    ui.label(egui::RichText::new(status).small().weak());
                }
                if ui
                    .add_enabled(
                        !import_busy && validation.is_ok(),
                        egui::Button::new("Store selected plane and open in Models"),
                    )
                    .on_hover_text(
                        "Reads exactly the selected native scalar/time/level and writes it under model=cm1 with an exact time and a provenance sidecar.",
                    )
                    .clicked()
                {
                    request = validation.ok();
                    self.message = Some("CM1 import queued…".to_owned());
                }
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
            self.level_index = 0;
            self.anchor_latitude.clear();
            self.anchor_longitude.clear();
            self.placement_mode = None;
            self.diagnostic_files.clear();
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
                self.diagnostic_files = result.diagnostic_files;
                self.diagnostic_note = result.diagnostic_note;
                if self.selected_variable.as_deref().is_none_or(|selected| {
                    result
                        .inventory
                        .variable(selected)
                        .is_none_or(|variable| !variable.role.is_horizontal_plane_compatible())
                }) {
                    self.selected_variable = result
                        .inventory
                        .horizontal_plane_variables()
                        .next()
                        .map(|variable| variable.name.clone());
                }
                self.message = Some(format!(
                    "CM1 inventory ready: {} compatible horizontal field(s), {} output time(s)",
                    result.inventory.horizontal_plane_variables().count(),
                    result.inventory.time.record_count
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
    }

    fn selection_ui(&mut self, ui: &mut egui::Ui, inventory: &Cm1Inventory) {
        ui.strong("Native horizontal plane");
        let plottable = inventory.horizontal_plane_variables().collect::<Vec<_>>();
        let selected_label = self
            .selected_variable
            .as_deref()
            .unwrap_or("Choose a native scalar");
        egui::ComboBox::from_id_salt("cm1_native_scalar")
            .selected_text(selected_label)
            .width(360.0)
            .show_ui(ui, |ui| {
                for variable in &plottable {
                    let label = match &variable.long_name {
                        Some(long_name) => format!("{} — {}", variable.name, long_name),
                        None => variable.name.clone(),
                    };
                    if ui
                        .selectable_value(
                            &mut self.selected_variable,
                            Some(variable.name.clone()),
                            label,
                        )
                        .clicked()
                    {
                        self.level_index = 0;
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
            ui.label("Output time");
            let time_label = cm1_time_label(inventory, self.time_index);
            egui::ComboBox::from_id_salt("cm1_output_time")
                .selected_text(time_label)
                .show_ui(ui, |ui| {
                    for index in 0..inventory.time.record_count {
                        ui.selectable_value(
                            &mut self.time_index,
                            index,
                            cm1_time_label(inventory, index),
                        );
                    }
                });
        });

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
        }

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
        exact_time(inventory, self.time_index)?;
        Ok(Cm1ImportRequest {
            source_path,
            inventory: inventory.clone(),
            variable,
            time_index: self.time_index,
            level_index,
            placement,
        })
    }
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
    progress(format!(
        "CM1: reading {} at output {}{}",
        request.variable,
        request.time_index,
        request
            .level_index
            .map(|level| format!(", native level {level}"))
            .unwrap_or_default()
    ));
    let nc = netcrust::open(&request.source_path).map_err(|error| error.to_string())?;
    let plane = cm1::read_horizontal_mass_grid_plane(
        &nc,
        &request.inventory,
        &request.variable,
        request.time_index,
        request.level_index,
    )
    .map_err(|error| error.to_string())?;
    let georef =
        cm1::georeference_scalar_grid(&request.inventory, &request.placement, request.time_index)
            .map_err(|error| error.to_string())?;
    let shape = GridShape::new(georef.nx, georef.ny).map_err(|error| error.to_string())?;
    let grid = LatLonGrid::new(shape, georef.lat_deg.clone(), georef.lon_deg.clone())
        .map_err(|error| error.to_string())?;
    let values = plane
        .values
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
                    "CM1 {} value at cell {index} cannot be represented as a finite f32: {value}",
                    request.variable
                ))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let exact = exact_time(&request.inventory, request.time_index)?;
    let field_name = stored_field_name(&request.variable, request.level_index);
    let units = plane.units.as_deref().unwrap_or("unknown");
    let run = run_name(request);
    let derived = [DerivedFieldInput {
        name: &field_name,
        units,
        values: &values,
    }];
    let projection = GridProjection::Geographic;
    progress(format!("CM1: writing model=cm1 run={run}"));
    write_hour_from_grid_with_derived_exact(
        store_root,
        "cm1",
        &run,
        0,
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

    let provenance_path = store_root
        .join("cm1")
        .join(&run)
        .join("cm1-provenance.json");
    write_provenance(
        &provenance_path,
        request,
        &plane,
        &georef.provenance,
        &field_name,
        exact,
    )?;
    let hour = rw_ui::HourKey {
        model: "cm1".to_owned(),
        run: run.clone(),
        hour: 0,
        exact_time: Some(exact),
    };
    Ok(Cm1ImportSummary {
        store_root: store_root.to_path_buf(),
        model: "cm1".to_owned(),
        run,
        variable: field_name,
        hour,
        provenance_path,
    })
}

fn write_provenance(
    path: &Path,
    request: &Cm1ImportRequest,
    plane: &cm1::Cm1NativePlane,
    georeference_provenance: &str,
    stored_variable: &str,
    exact: RwsExactTime,
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
            "time_index": request.time_index,
            "level_index": request.level_index,
            "nominal_level_m": plane.nominal_level_m,
            "lead_seconds": exact.lead_seconds,
            "valid_unix": exact.valid_unix,
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
        "{}|{}|{}|{:?}|{:.9}|{:.9}",
        request.source_path.display(),
        request.variable,
        request.time_index,
        request.placement.mode,
        request.placement.anchor_latitude_deg,
        request.placement.anchor_longitude_deg,
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
    let human = format!(
        "cm1_{}_{}_t{:03}_{}_{}_{}",
        sanitize_slug(stem),
        sanitize_slug(&request.variable),
        request.time_index,
        level,
        mode,
        hash
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
            time_index: 1,
            level_index: Some(1),
            placement: Cm1Placement {
                mode: Cm1PlacementMode::FollowDomain,
                anchor_latitude_deg: 35.0,
                anchor_longitude_deg: -97.0,
            },
        };
        let store_root = std::env::temp_dir().join(format!(
            "bowecho-cm1-import-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let mut progress = Vec::new();
        let summary =
            import_selected_plane(&request, &store_root, &mut |message| progress.push(message))
                .expect("import fixture plane");
        assert_eq!(summary.model, "cm1");
        assert_eq!(summary.variable, "cm1_u_k001");
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
        let provenance =
            std::fs::read_to_string(&summary.provenance_path).expect("read CM1 provenance sidecar");
        assert!(provenance.contains("bowecho-cm1-provenance-v1"));
        assert!(provenance.contains("\"name\": \"u\""));
        assert!(provenance.contains("DestaggeredX"));
        assert!(provenance.contains("FollowDomain"));
        assert!(!progress.is_empty());
        let _ = std::fs::remove_dir_all(store_root);
    }
}
