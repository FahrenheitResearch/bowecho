//! Local WRF / NetCDF import — ported verbatim from rusty-weather's
//! `rusty-weather-ui` shell (rev edb9d277) so BowEcho can ingest a WRF run
//! folder into the model store directly, without the standalone shell.
//!
//! `netcrust` provides the 2D metadata (variable list, dims, units, global
//! attrs) for every file; for raw wrfout the 2D data PLANES and the isobaric
//! sounding volumes are decoded through `wrf-core`'s single-timestep reader
//! (netcrust's `hdf5-reader` path burns ~10 s + ~8M minor page faults per
//! 800×800 plane on compressed 250 m wrfouts — allocation churn, see
//! docs/wrf-import-large-grids.md — while wrf-core reads the same slice in
//! tens of ms). Plain NetCDF and post-processed climate files stay entirely
//! on netcrust. Each file is written as one forecast-hour slot via
//! `rw_store::write_hour_from_fields_with_derived`; imported runs then sound
//! through the existing ModelDataDock skew-T path.
#![allow(dead_code)]
// Ported verbatim: `push_direct` threads the netcrust handle + grid + selector
// as separate args, and `try_postprocessed_wrf` returns the nested field/volume
// tuple the store writer consumes. Both are the upstream API shape.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use netcrust::{File as NcFile, Variable as NcVariable};
use rustwx_core::{
    CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
};
use rw_store::{DerivedFieldInput, WrittenHour, write_hour_from_fields_with_derived};
use wrf_core::WrfFile;

use crate::wrf_volumes::{IsoVolume, SurfaceFallback, build_iso_volumes, interpolate_iso_volumes};

const LOCAL_IMPORT_MAX_SCAN_DEPTH: usize = 8;
const LOCAL_IMPORT_MAX_DISCOVERED_FILES: usize = 10_000;

#[derive(Debug)]
pub struct LocalImportTask {
    pub label: String,
    pub rx: Receiver<LocalImportMessage>,
}

/// Worker → UI messages, same shape as `wrf_process::WrfProcessMessage`: the
/// dock shows the latest `Progress` line while the import runs (on a 250 m
/// grid the light path is legitimately minutes per file — an anonymous
/// spinner reads as a hang), then a single terminal `Done`.
#[derive(Debug)]
pub enum LocalImportMessage {
    Progress(String),
    Done(Result<LocalImportSummary, String>),
}

#[derive(Debug, Clone)]
pub struct LocalImportSummary {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub files_seen: usize,
    pub hours_written: usize,
    pub variables: Vec<String>,
    /// Per-file degradations that did not fail the import (e.g. isobaric
    /// sounding volumes unavailable) — surfaced in the completion status line.
    pub notes: Vec<String>,
}

struct ImportedWrfFields {
    canonical: Vec<(String, SelectedField2D)>,
    raw_2d: Vec<RawField2D>,
    grid: LatLonGrid,
    projection: Option<GridProjection>,
}

/// One raw 2-D plane under the light-import `wrf_*` store naming.
/// `pub(crate)`: the wrf2d route hands these to BOTH import workers through
/// [`PostprocessedWrfHour`], and `wrf_process` maps them into its derived-
/// field refs itself.
pub(crate) struct RawField2D {
    pub(crate) name: String,
    pub(crate) units: String,
    pub(crate) values: Vec<f32>,
}

pub fn spawn_import_paths(paths: Vec<PathBuf>, store_root: PathBuf) -> LocalImportTask {
    let label = if paths.len() == 1 {
        format!("Import {}", display_name(&paths[0]))
    } else {
        format!("Import {} local files", paths.len())
    };
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("rw-ui-local-import".to_string())
        .spawn(move || {
            crate::wrf_process::lower_import_thread_priority();
            let mut progress = |message: String| {
                let _ = tx.send(LocalImportMessage::Progress(message));
            };
            let result =
                import_paths(&paths, &store_root, &mut progress).map_err(|err| err.to_string());
            let _ = tx.send(LocalImportMessage::Done(result));
        })
        .expect("spawn local import worker");
    LocalImportTask { label, rx }
}

pub fn supported_files_in_folder(folder: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_model_file(&path) {
                paths.push(path);
                if paths.len() >= LOCAL_IMPORT_MAX_DISCOVERED_FILES {
                    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                    return paths;
                }
            } else if depth < LOCAL_IMPORT_MAX_SCAN_DEPTH && path.is_dir() {
                stack.push((path, depth + 1));
            }
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    paths
}

pub fn is_supported_model_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.starts_with("wrfout")
        || matches!(
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase())
                .as_deref(),
            Some("nc" | "nc4" | "cdf")
        )
        // GRIB Edition 1 (.grb/.grib — ERA-20C / GDEX reanalysis); routed to
        // `grib_import` below. GRIB2 extensions stay unsupported here.
        || crate::grib_import::is_grib1_file(path)
}

fn import_paths(
    paths: &[PathBuf],
    store_root: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<LocalImportSummary, ImportError> {
    if paths.is_empty() {
        return Err(ImportError::NoFiles);
    }
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| is_supported_model_file(path))
        .cloned()
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err(ImportError::NoSupportedFiles);
    }
    if files.len() > u16::MAX as usize {
        return Err(ImportError::TooManyFiles(files.len()));
    }

    // GRIB1 files carry many timesteps each and decode through grib-core,
    // not netcrust — the whole selection routes to `grib_import`. Mixed
    // selections are refused rather than guessed at: GRIB hour slots are
    // valid-time-derived while the WRF path below is file-index-derived,
    // and interleaving the two would scramble the run's hour axis.
    if files
        .iter()
        .any(|path| crate::grib_import::is_grib1_file(path))
    {
        if !files
            .iter()
            .all(|path| crate::grib_import::is_grib1_file(path))
        {
            return Err(ImportError::MixedGribSelection);
        }
        return crate::grib_import::import_grib1_files(&files, store_root, progress)
            .map_err(ImportError::Grib);
    }

    let model = "wrf".to_string();
    let run = import_run_name(&files);
    let total = files.len();
    let mut all_vars = Vec::new();
    let mut written = Vec::<WrittenHour>::new();
    let mut notes = Vec::<String>::new();
    for (index, path) in files.iter().enumerate() {
        let hour = u16::try_from(index).expect("bounded above");
        // Every stage line carries the file position, so a folder import reads
        // "file 3/10 (wrfout_…): interpolating …" rather than a bare spinner.
        let tag = format!("file {}/{total} ({})", index + 1, display_name(path));
        // One netcrust handle per file: `netcrust::open` eagerly indexes the
        // NetCDF-4 metadata twice over (NcFile + Hdf5File — ~57 s of
        // hdf5-reader churn on a 2 GB Enderlin wrfout, measured), so the
        // post-processed gate and the 2D reader below must share it. A file
        // netcrust can't open would have failed `read_wrf_2d_fields` with
        // this same error before; it just surfaces one stage earlier now.
        let nc = netcrust::open(path)?;
        // Post-processed climate wrfout (CONUS-I/II, GDEX: derived TK/Z/P, no
        // raw T/PB) can't go through the raw-wrfout reader — build it directly.
        // (Bound before the `if let` so the prefixing closure's borrow of
        // `progress` ends before the block uses `progress` again.)
        let postprocessed = try_postprocessed_wrf_shared(&nc, path, &mut |message| {
            progress(format!("{tag}: {message}"))
        })?;
        if let Some((canonical, severe, volumes, raw_2d)) = postprocessed {
            let refs = canonical
                .iter()
                .map(|(name, field)| (name.as_str(), field))
                .collect::<Vec<_>>();
            // Severe/thermo fields ride the derived-field slot under the same
            // store slugs the heavy getvar path writes, so labels and Solar
            // styles apply identically.
            let mut derived_refs = severe
                .iter()
                .map(|field| DerivedFieldInput {
                    name: field.name,
                    units: field.units,
                    values: field.values.as_slice(),
                })
                .collect::<Vec<_>>();
            // Raw `wrf_*` planes from the 2-D wrf2d route share that slot —
            // the same convention the raw-wrfout light import uses (empty on
            // the 3-D route, so its hours are written exactly as before).
            derived_refs.extend(raw_2d.iter().map(|field| DerivedFieldInput {
                name: field.name.as_str(),
                units: field.units.as_str(),
                values: field.values.as_slice(),
            }));
            let volume_inputs = volumes.iter().map(IsoVolume::as_input).collect::<Vec<_>>();
            progress(format!("{tag}: writing forecast hour f{hour:03} to store"));
            let result = write_hour_from_fields_with_derived(
                store_root,
                &model,
                &run,
                hour,
                &refs,
                &derived_refs,
                &volume_inputs,
                writer_build(),
                now_unix(),
            )?;
            all_vars.extend(result.vars.iter().cloned());
            written.push(result);
            continue;
        }
        progress(format!("{tag}: reading 2D surface fields"));
        // One wrf-core handle per raw wrfout, shared by the fast 2D plane
        // reads AND the isobaric volume build. `None` (plain NetCDF, or a
        // panic on a pathological header) keeps every 2D read on netcrust
        // and skips the volumes — exactly the pre-fast-path behavior.
        let wrf_file = crate::wrf_process::isolate_panics("open WRF file", || {
            WrfFile::open(path).map_err(|err| err.to_string())
        })
        .ok();
        let mut fields = read_wrf_2d_fields(&nc, path, wrf_file.as_ref(), &mut |message| {
            progress(format!("{tag}: {message}"))
        })?;
        // Release the netcrust metadata/mmap before the volume build + store
        // write, matching the lifetime the per-stage opens used to have.
        drop(nc);
        if fields.canonical.is_empty() {
            return Err(ImportError::NoFields(path.clone()));
        }
        // Isobaric sounding volumes + lowest-model-level surface fallback, so an
        // imported WRF run makes soundings. Built through wrf-core; a plain
        // NetCDF wrf-core can't open yields neither. Fill any surface field the
        // 2D read missed (e.g. PSFC in a split wrf3d file) from the fallback.
        let (iso_volumes, surface_fallback, volume_note) =
            read_iso_volumes(wrf_file.as_ref(), &mut |message| {
                progress(format!("{tag}: {message}"))
            });
        if let Some(note) = volume_note {
            progress(format!("{tag}: {note}"));
            notes.push(format!("{}: {note}", display_name(path)));
        }
        if let Some(surface) = surface_fallback {
            fill_missing_surface(&mut fields, surface);
        }
        let refs = fields
            .canonical
            .iter()
            .map(|(name, field)| (name.as_str(), field))
            .collect::<Vec<_>>();
        let raw_refs = fields
            .raw_2d
            .iter()
            .map(|field| DerivedFieldInput {
                name: field.name.as_str(),
                units: field.units.as_str(),
                values: field.values.as_slice(),
            })
            .collect::<Vec<_>>();
        // Volume planes come from wrf-core, the 2D grid from netcrust; if they
        // ever disagree on grid size, drop volumes rather than fail the hour.
        let grid_cells = fields.grid.shape.len();
        let volumes_match = iso_volumes.iter().all(|volume| {
            volume
                .levels
                .iter()
                .all(|(_, plane)| plane.len() == grid_cells)
        });
        let volume_inputs = if volumes_match {
            iso_volumes
                .iter()
                .map(IsoVolume::as_input)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        progress(format!("{tag}: writing forecast hour f{hour:03} to store"));
        let result = write_hour_from_fields_with_derived(
            store_root,
            &model,
            &run,
            hour,
            &refs,
            &raw_refs,
            &volume_inputs,
            writer_build(),
            now_unix(),
        )?;
        all_vars.extend(result.vars.iter().cloned());
        written.push(result);
    }
    all_vars.sort();
    all_vars.dedup();
    Ok(LocalImportSummary {
        store_root: store_root.to_path_buf(),
        model,
        run,
        files_seen: files.len(),
        hours_written: written.len(),
        variables: all_vars,
        notes,
    })
}

fn read_wrf_2d_fields(
    nc: &NcFile,
    path: &Path,
    wrf: Option<&WrfFile>,
    progress: &mut dyn FnMut(String),
) -> Result<ImportedWrfFields, ImportError> {
    let src = PlaneSource::new(nc, wrf);
    let lat = read_first_2d_any(&src, &["XLAT", "XLAT_M", "lat", "latitude"])?;
    let lon = read_first_2d_any(&src, &["XLONG", "XLONG_M", "lon", "longitude"])?;
    if lat.nx != lon.nx || lat.ny != lon.ny || lat.values.len() != lon.values.len() {
        return Err(ImportError::GridMismatch(path.to_path_buf()));
    }
    let shape = GridShape::new(lat.nx, lat.ny)?;
    let grid = LatLonGrid::new(shape, lat.values, lon.values)?;
    let projection = wrf_projection(nc);
    let mut canonical = Vec::new();
    push_canonical_surface_fields(&mut canonical, &src, &grid, &projection)?;

    let raw_2d = read_raw_wrf_mass_grid_fields(&src, lat.nx, lat.ny, progress)?;

    // Surface how the planes were actually decoded: the stage timestamps in
    // the instrumented harness (and the dock) should show whether the fast
    // path engaged and which planes, if any, wrf-core could not read.
    if wrf.is_some() {
        let fallbacks = src.netcrust_fallbacks.borrow();
        if fallbacks.is_empty() {
            progress(format!(
                "read {} 2D planes via wrf-core reader",
                src.wrf_reads.get()
            ));
        } else {
            progress(format!(
                "read {} 2D planes via wrf-core reader; {} fell back to netcrust: {}",
                src.wrf_reads.get(),
                fallbacks.len(),
                fallbacks.join(", ")
            ));
        }
    }

    Ok(ImportedWrfFields {
        canonical,
        raw_2d,
        grid,
        projection,
    })
}

/// The canonical surface-field suite shared by the raw-wrfout 2-D read and
/// the post-processed 2-D (`wrf2d`) route: direct T2/U10/V10/PSFC/HGT/SLP/
/// REFD_MAX/WSPD10MAX planes plus the derived 10 m wind speed, 2 m dewpoint /
/// relative humidity, and total-precipitation fields — each pushed only when
/// its source planes exist in the file. Body moved unchanged from
/// `read_wrf_2d_fields` (only the borrow spellings changed for the
/// by-reference parameters).
fn push_canonical_surface_fields(
    canonical: &mut Vec<(String, SelectedField2D)>,
    src: &PlaneSource,
    grid: &LatLonGrid,
    projection: &Option<GridProjection>,
) -> Result<(), ImportError> {
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "T2",
        "temperature_2m",
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
        Some("K"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "U10",
        "u_10m",
        FieldSelector::height_agl(CanonicalField::UWind, 10),
        Some("m/s"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "V10",
        "v_10m",
        FieldSelector::height_agl(CanonicalField::VWind, 10),
        Some("m/s"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "PSFC",
        "surface_pressure",
        FieldSelector::surface(CanonicalField::Pressure),
        Some("Pa"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "HGT",
        "orography",
        FieldSelector::surface(CanonicalField::GeopotentialHeight),
        Some("m"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "SLP",
        "mslp",
        FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
        Some("Pa"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "REFD_MAX",
        "composite_reflectivity",
        FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        Some("dBZ"),
    )?;
    push_direct(
        canonical,
        src,
        grid,
        projection.clone(),
        "WSPD10MAX",
        "wind_speed_10m_max",
        FieldSelector::height_agl(CanonicalField::WindGust, 10),
        Some("m/s"),
    )?;

    if let (Some(u10), Some(v10)) = (read_first_2d(src, "U10")?, read_first_2d(src, "V10")?) {
        let values = combine_same_grid(&u10, &v10, |u, v| (u.mul_add(u, v * v)).sqrt())?;
        push_computed(
            canonical,
            grid,
            projection.clone(),
            "wind_speed_10m",
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            "m/s",
            values,
        )?;
    }

    if let (Some(t2), Some(q2), Some(psfc)) = (
        read_first_2d(src, "T2")?,
        read_first_2d(src, "Q2")?,
        read_first_2d(src, "PSFC")?,
    ) {
        let dewpoint = derive_dewpoint_k(&t2, &q2, &psfc)?;
        push_computed(
            canonical,
            grid,
            projection.clone(),
            "dewpoint_2m",
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            dewpoint,
        )?;
        let rh = derive_relative_humidity_percent(&t2, &q2, &psfc)?;
        push_computed(
            canonical,
            grid,
            projection.clone(),
            "relative_humidity_2m",
            FieldSelector::height_agl(CanonicalField::RelativeHumidity, 2),
            "%",
            rh,
        )?;
    }

    if let (Some(rainc), Some(rainnc)) =
        (read_first_2d(src, "RAINC")?, read_first_2d(src, "RAINNC")?)
    {
        let rainsh = read_first_2d(src, "RAINSH")?;
        let values = combine_precip(&rainc, &rainnc, rainsh.as_ref())?;
        push_computed(
            canonical,
            grid,
            projection.clone(),
            "apcp",
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "kg/m^2",
            values,
        )?;
    }

    Ok(())
}

fn push_direct(
    out: &mut Vec<(String, SelectedField2D)>,
    src: &PlaneSource,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    wrf_name: &str,
    store_name: &str,
    selector: FieldSelector,
    units_override: Option<&str>,
) -> Result<(), ImportError> {
    let Some(plane) = read_first_2d(src, wrf_name)? else {
        return Ok(());
    };
    let units = units_override
        .map(str::to_string)
        .or_else(|| variable_units(src.nc, wrf_name))
        .unwrap_or_else(|| selector.native_units().to_string());
    push_computed(
        out,
        grid,
        projection,
        store_name,
        selector,
        &units,
        plane.values,
    )
}

fn push_computed(
    out: &mut Vec<(String, SelectedField2D)>,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    store_name: &str,
    selector: FieldSelector,
    units: &str,
    values: Vec<f32>,
) -> Result<(), ImportError> {
    let mut field = SelectedField2D::new(selector, units, grid.clone(), values)?;
    if let Some(projection) = projection {
        field = field.with_projection(projection);
    }
    out.push((store_name.to_string(), field));
    Ok(())
}

fn read_raw_wrf_mass_grid_fields(
    src: &PlaneSource,
    nx: usize,
    ny: usize,
    progress: &mut dyn FnMut(String),
) -> Result<Vec<RawField2D>, ImportError> {
    let mut seen = HashSet::<String>::new();
    let mut raw = Vec::new();
    for var in src.nc.variables()? {
        let wrf_name = var.name();
        if !is_raw_wrf_mass_grid_variable(&var, nx, ny) || !raw_wrf_variable_allowed(wrf_name) {
            continue;
        }
        // One line per raw plane: on a compressed 250 m wrfout each first-
        // record read decompresses real data, and there are dozens of them.
        progress(format!("reading raw 2D field {wrf_name}"));
        let Some(plane) = read_first_2d(src, wrf_name)? else {
            continue;
        };
        if plane.nx != nx || plane.ny != ny {
            continue;
        }
        let name = format!("wrf_{}", sanitize_store_var_name(wrf_name));
        if name == "wrf_" || !seen.insert(name.clone()) {
            continue;
        }
        raw.push(RawField2D {
            name,
            units: variable_units(src.nc, wrf_name).unwrap_or_else(|| "1".to_string()),
            values: plane.values,
        });
    }
    Ok(raw)
}

fn is_raw_wrf_mass_grid_variable(var: &NcVariable, nx: usize, ny: usize) -> bool {
    let dims = var.dimensions();
    let shape = var.shape();
    dims.len() == 3
        && shape.len() == 3
        && dims[0].name() == "Time"
        && dims[1].name() == "south_north"
        && dims[2].name() == "west_east"
        && shape[1] == ny
        && shape[2] == nx
}

fn raw_wrf_variable_allowed(name: &str) -> bool {
    !matches!(
        name.to_ascii_uppercase().as_str(),
        "XLAT"
            | "XLONG"
            | "XLAT_M"
            | "XLONG_M"
            | "CLAT"
            | "NEST_POS"
            | "AREA2D"
            | "DX2D"
            | "MAPFAC_M"
            | "MAPFAC_MX"
            | "MAPFAC_MY"
            | "F"
            | "E"
            | "SINALPHA"
            | "COSALPHA"
    )
}

fn sanitize_store_var_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_was_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_underscore = false;
        } else if !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone)]
struct Plane2D {
    nx: usize,
    ny: usize,
    values: Vec<f32>,
}

/// One file's 2-D plane source. `netcrust` always provides the metadata
/// (variable existence, dims, shapes, units); when `wrf` is set (raw wrfout)
/// the plane DATA is decoded through wrf-core's single-timestep reader
/// instead of netcrust's `hdf5-reader` path, which was measured at ~10.3 s
/// and ~8M minor page faults per 800×800 plane on compressed 250 m wrfouts
/// (allocation churn — docs/wrf-import-large-grids.md) versus tens of ms
/// for wrf-core reading the same slice.
struct PlaneSource<'a> {
    nc: &'a NcFile,
    wrf: Option<&'a WrfFile>,
    /// Planes decoded via wrf-core (the fast path actually engaged).
    wrf_reads: Cell<usize>,
    /// WRF-layout planes wrf-core failed to read, served by netcrust instead.
    netcrust_fallbacks: RefCell<Vec<String>>,
}

impl<'a> PlaneSource<'a> {
    fn new(nc: &'a NcFile, wrf: Option<&'a WrfFile>) -> Self {
        Self {
            nc,
            wrf,
            wrf_reads: Cell::new(0),
            netcrust_fallbacks: RefCell::new(Vec::new()),
        }
    }

    fn netcrust_only(nc: &'a NcFile) -> Self {
        Self::new(nc, None)
    }
}

fn read_first_2d_any(src: &PlaneSource, names: &[&str]) -> Result<Plane2D, ImportError> {
    for name in names {
        if let Some(plane) = read_first_2d(src, name)? {
            return Ok(plane);
        }
    }
    Err(ImportError::MissingAny(
        names.iter().map(|value| value.to_string()).collect(),
    ))
}

fn read_first_2d(src: &PlaneSource, name: &str) -> Result<Option<Plane2D>, ImportError> {
    let Some(var) = src.nc.variable(name) else {
        return Ok(None);
    };
    // Fast path: for the `[Time, …, ny, nx]` record layout — exactly the case
    // netcrust's first-record read would hyperslab — decode the timestep-0
    // slice through wrf-core instead. Identical value positions: both paths
    // yield the record's `…, ny, nx` values, and `plane_from_last_record`
    // applies the same tail-plane + f32 narrowing to either. Anything else
    // (no Time dim, rank < 3, unexpected length, wrf-core read error) keeps
    // the legacy netcrust read byte-for-byte.
    if let Some(wrf) = src.wrf {
        let dims = var.dimensions();
        let shape = var.shape();
        if dims.len() >= 3 && shape.len() == dims.len() && dims[0].name() == "Time" {
            let expected = shape[1..]
                .iter()
                .try_fold(1usize, |acc, &dim| acc.checked_mul(dim));
            let outcome = match expected {
                None => Err("dimension product overflows usize".to_string()),
                Some(expected) => match wrf.read_var(name, 0) {
                    Ok(values) if values.len() == expected => Ok(values),
                    Ok(values) => Err(format!("expected {expected} values, got {}", values.len())),
                    Err(err) => Err(err.to_string()),
                },
            };
            match outcome {
                Ok(values) => {
                    src.wrf_reads.set(src.wrf_reads.get() + 1);
                    return plane_from_last_record(name, &shape[1..], &values);
                }
                // Carry the WHY: the fallback summary line is how a plane
                // that is genuinely only reachable via netcrust gets reported.
                Err(reason) => src
                    .netcrust_fallbacks
                    .borrow_mut()
                    .push(format!("{name} ({reason})")),
            }
        }
    }
    read_first_2d_netcrust(src.nc, name)
}

/// Legacy netcrust plane read — the pre-fast-path implementation, kept intact
/// as the fallback for non-wrfout files (and the reference side of the
/// value-identity fixture test).
fn read_first_2d_netcrust(nc: &NcFile, name: &str) -> Result<Option<Plane2D>, ImportError> {
    if nc.variable(name).is_none() {
        return Ok(None);
    }
    let array = nc.read_array_f64_first_record_or_all(name)?;
    plane_from_last_record(name, array.shape(), array.values())
}

/// Build a [`Plane2D`] from the LAST `ny * nx` values of a decoded record,
/// with the non-finite → NaN f32 narrowing both read paths share. `shape` is
/// the decoded record's shape (`…, ny, nx`); a leading level dimension means
/// the deepest plane wins — the tail-of-record convention the netcrust read
/// has always used.
fn plane_from_last_record(
    name: &str,
    shape: &[usize],
    values: &[f64],
) -> Result<Option<Plane2D>, ImportError> {
    if shape.len() < 2 {
        return Ok(None);
    }
    let ny = shape[shape.len() - 2];
    let nx = shape[shape.len() - 1];
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| ImportError::BadShape(name.to_string(), shape.to_vec()))?;
    if values.len() < cells {
        return Err(ImportError::BadShape(name.to_string(), shape.to_vec()));
    }
    let offset = values.len() - cells;
    Ok(Some(Plane2D {
        nx,
        ny,
        values: values[offset..]
            .iter()
            .map(|value| {
                if value.is_finite() {
                    *value as f32
                } else {
                    f32::NAN
                }
            })
            .collect(),
    }))
}

fn variable_units(nc: &NcFile, name: &str) -> Option<String> {
    nc.variable(name)?
        .attribute("units")
        .and_then(|attr| attr.as_string())
        .map(str::to_string)
}

fn combine_same_grid(
    a: &Plane2D,
    b: &Plane2D,
    f: impl Fn(f32, f32) -> f32,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(a, b)?;
    Ok(a.values
        .iter()
        .zip(&b.values)
        .map(|(&a, &b)| {
            if a.is_finite() && b.is_finite() {
                f(a, b)
            } else {
                f32::NAN
            }
        })
        .collect())
}

fn combine_precip(
    rainc: &Plane2D,
    rainnc: &Plane2D,
    rainsh: Option<&Plane2D>,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(rainc, rainnc)?;
    if let Some(rainsh) = rainsh {
        ensure_same_grid(rainc, rainsh)?;
    }
    Ok((0..rainc.values.len())
        .map(|idx| {
            let mut value = 0.0;
            let mut valid = true;
            for plane in [Some(rainc), Some(rainnc), rainsh].into_iter().flatten() {
                let v = plane.values[idx];
                if v.is_finite() {
                    value += v;
                } else {
                    valid = false;
                }
            }
            if valid { value } else { f32::NAN }
        })
        .collect())
}

fn derive_dewpoint_k(t2: &Plane2D, q2: &Plane2D, psfc: &Plane2D) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(t2, q2)?;
    ensure_same_grid(t2, psfc)?;
    Ok((0..t2.values.len())
        .map(|idx| dewpoint_from_q_psfc(q2.values[idx], psfc.values[idx]))
        .collect())
}

fn derive_relative_humidity_percent(
    t2: &Plane2D,
    q2: &Plane2D,
    psfc: &Plane2D,
) -> Result<Vec<f32>, ImportError> {
    ensure_same_grid(t2, q2)?;
    ensure_same_grid(t2, psfc)?;
    Ok((0..t2.values.len())
        .map(|idx| {
            relative_humidity_from_t_q_psfc(t2.values[idx], q2.values[idx], psfc.values[idx])
        })
        .collect())
}

fn dewpoint_from_q_psfc(q: f32, p_pa: f32) -> f32 {
    if !q.is_finite() || !p_pa.is_finite() || q <= 0.0 || p_pa <= 0.0 {
        return f32::NAN;
    }
    let q = q as f64;
    let p = p_pa as f64;
    let e = (q * p / (0.622 + 0.378 * q)).max(1.0);
    let ln = (e / 611.2).ln();
    let td_c = 243.5 * ln / (17.67 - ln);
    (td_c + 273.15) as f32
}

fn relative_humidity_from_t_q_psfc(t_k: f32, q: f32, p_pa: f32) -> f32 {
    if !t_k.is_finite() || !q.is_finite() || !p_pa.is_finite() || t_k <= 0.0 {
        return f32::NAN;
    }
    let e = q as f64 * p_pa as f64 / (0.622 + 0.378 * q as f64);
    let t_c = t_k as f64 - 273.15;
    let es = 611.2 * (17.67 * t_c / (t_c + 243.5)).exp();
    (100.0 * e / es).clamp(0.0, 100.0) as f32
}

fn ensure_same_grid(a: &Plane2D, b: &Plane2D) -> Result<(), ImportError> {
    if a.nx == b.nx && a.ny == b.ny && a.values.len() == b.values.len() {
        Ok(())
    } else {
        Err(ImportError::PlaneMismatch)
    }
}

fn wrf_projection(nc: &NcFile) -> Option<GridProjection> {
    let map_proj = global_attr_f64(nc, "MAP_PROJ")? as i32;
    match map_proj {
        1 => Some(GridProjection::LambertConformal {
            standard_parallel_1_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(30.0),
            standard_parallel_2_deg: global_attr_f64(nc, "TRUELAT2")
                .or_else(|| global_attr_f64(nc, "TRUELAT1"))
                .unwrap_or(60.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
        }),
        2 => Some(GridProjection::PolarStereographic {
            true_latitude_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(60.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
            south_pole_on_projection_plane: global_attr_f64(nc, "CEN_LAT").unwrap_or(45.0) < 0.0,
        }),
        3 => Some(GridProjection::Mercator {
            latitude_of_true_scale_deg: global_attr_f64(nc, "TRUELAT1").unwrap_or(0.0),
            central_meridian_deg: global_attr_f64(nc, "STAND_LON")
                .or_else(|| global_attr_f64(nc, "CEN_LON"))
                .unwrap_or(0.0),
        }),
        6 => Some(GridProjection::Geographic),
        other => Some(GridProjection::Other {
            template: other.max(0) as u16,
        }),
    }
}

fn global_attr_f64(nc: &NcFile, name: &str) -> Option<f64> {
    nc.attribute(name).and_then(|attr| attr.as_f64())
}

fn import_run_name(paths: &[PathBuf]) -> String {
    let first = paths.first();
    let stamp = first
        .and_then(|path| timestamp_from_path(path))
        .unwrap_or_else(|| {
            first
                .and_then(|path| path.file_stem())
                .and_then(|value| value.to_str())
                .unwrap_or("local")
                .to_string()
        });
    sanitize_run_name(&format!("local_wrf_{stamp}"))
}

fn timestamp_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let bytes = name.as_bytes();
    for start in 0..bytes.len().saturating_sub(18) {
        let slice = name.get(start..start + 19)?;
        if is_wrf_timestamp(slice) {
            return Some(normalize_wrf_timestamp(slice));
        }
    }
    None
}

fn is_wrf_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'_'
        && matches!(b[13], b':' | b'_')
        && matches!(b[16], b':' | b'_')
        && b.iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit())
}

fn normalize_wrf_timestamp(value: &str) -> String {
    let date = value[..10]
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    let time = value[11..]
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    format!("{date}_{time}")
}

fn sanitize_run_name(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "local_wrf".to_string()
    } else {
        out
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("local file")
        .to_string()
}

fn writer_build() -> &'static str {
    concat!("bowecho-wrf-local-import-", env!("CARGO_PKG_VERSION"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Build isobaric sounding volumes for one WRF file via wrf-core (time index 0,
/// matching the single-record 2D import). `file` is the handle `import_paths`
/// opened for the fast 2D reads; `None` (plain NetCDF wrf-core can't open)
/// yields no volumes so the 2D import still succeeds. The third element
/// carries the human-readable reason when the volumes degraded on a file
/// wrf-core DID open.
fn read_iso_volumes(
    file: Option<&WrfFile>,
    progress: &mut dyn FnMut(String),
) -> (Vec<IsoVolume>, Option<SurfaceFallback>, Option<String>) {
    let Some(file) = file else {
        // Plain NetCDF with no WRF 3D state — a 2D-only import, not a note.
        return (Vec::new(), None, None);
    };
    let cells = file.nx.saturating_mul(file.ny);
    // Same per-field panic isolation as the heavy path's `compute_var`: a
    // wrf-core panic on a pathological grid must degrade to "no soundings",
    // not unwind the rw-ui-local-import worker and lose the whole import.
    let result = crate::wrf_process::isolate_panics("isobaric volumes", || {
        build_iso_volumes(file, 0, cells, progress)
    });
    match result {
        Ok((volumes, surface)) => (volumes, Some(surface), None),
        Err(err) => (
            Vec::new(),
            None,
            Some(format!("isobaric sounding volumes unavailable — {err}")),
        ),
    }
}

/// Add any skew-T surface field the netcrust 2D read did not provide, from the
/// wrf-core lowest-model-level fallback — so a split `wrf3d` file (which omits
/// `PSFC`) still sounds. Fields already present are kept; planes that don't
/// match the hour grid are skipped.
fn fill_missing_surface(fields: &mut ImportedWrfFields, surface: SurfaceFallback) {
    let cells = fields.grid.shape.len();
    let entries: [(&str, FieldSelector, &str, Vec<f32>); 5] = [
        (
            "surface_pressure",
            FieldSelector::surface(CanonicalField::Pressure),
            "Pa",
            surface.surface_pressure_pa,
        ),
        (
            "temperature_2m",
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            surface.temperature_2m_k,
        ),
        (
            "dewpoint_2m",
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            surface.dewpoint_2m_k,
        ),
        (
            "u_10m",
            FieldSelector::height_agl(CanonicalField::UWind, 10),
            "m/s",
            surface.u_10m,
        ),
        (
            "v_10m",
            FieldSelector::height_agl(CanonicalField::VWind, 10),
            "m/s",
            surface.v_10m,
        ),
    ];
    for (name, selector, units, values) in entries {
        if values.len() != cells
            || fields
                .canonical
                .iter()
                .any(|(existing, _)| existing == name)
        {
            continue;
        }
        if let Ok(field) = SelectedField2D::new(selector, units, fields.grid.clone(), values) {
            let field = match &fields.projection {
                Some(projection) => field.with_projection(projection.clone()),
                None => field,
            };
            fields.canonical.push((name.to_string(), field));
        }
    }
}

/// Build a soundable store hour from a POST-PROCESSED climate wrfout (NCAR
/// CONUS-I/II, GDEX): these ship derived `TK` (K), `Z` (m MSL), `P` (full
/// pressure, Pa) and staggered `U`/`V` instead of the raw `T`/`PB`/`PH`/`PHB`
/// the wrf-core reader needs, and carry no surface fields. Returns the
/// synthesized surface 2D fields + the severe/thermo suite + the isobaric
/// volumes (+ raw `wrf_*` planes when the file is a pure 2-D `wrf2d` surface
/// archive), or `None` if this isn't a post-processed WRF file (so the
/// caller falls back to the raw path). `progress` streams the stage messages
/// both import paths show in the dock.
pub(crate) fn try_postprocessed_wrf(
    path: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<Option<PostprocessedWrfHour>, ImportError> {
    // If netcrust can't open it at all, it's not our post-processed case —
    // let the caller's raw-wrfout path try instead of failing here.
    let Ok(nc) = netcrust::open(path) else {
        return Ok(None);
    };
    try_postprocessed_wrf_shared(&nc, path, progress)
}

/// Vertically destagger a `(nz+1) x cells` w-level field to `nz` mass levels
/// in place (mass level k = mean of staggered levels k and k+1), then
/// truncate. In-place forward iteration is safe — the write slot k is only
/// read again by iteration k itself — and avoids allocating a second
/// multi-hundred-MB buffer on CONUS-II grids.
fn destagger_z_to_mass_levels(values: &mut Vec<f64>, nz: usize, cells: usize) {
    debug_assert!(values.len() >= (nz + 1) * cells);
    for k in 0..nz {
        for i in 0..cells {
            let lo = values[k * cells + i];
            let hi = values[(k + 1) * cells + i];
            values[k * cells + i] = 0.5 * (lo + hi);
        }
    }
    values.truncate(nz * cells);
}

/// Everything one post-processed hour yields: the synthesized surface 2D
/// fields, the severe/thermo suite (heavy-path store slugs, written through
/// the derived-field slot), the isobaric sounding volumes, and — for the
/// 2-D-only `wrf2d` route — every mass-grid data plane as a raw `wrf_*`
/// field (same derived-slot convention as the raw-wrfout light import; the
/// 3-D route always returns this empty).
pub(crate) type PostprocessedWrfHour = (
    Vec<(String, SelectedField2D)>,
    Vec<crate::postproc_severe::SevereField>,
    Vec<IsoVolume>,
    Vec<RawField2D>,
);

/// Post-processed climate-WRF routing rule: TRUE when the `TK` variable is a
/// single 2-D surface plane — i.e. after dropping one leading record
/// ("Time") dimension, exactly `[ny, nx]` remains. That is the CONUS-II
/// `wrf2d` surface-archive dialect (every data variable at the lowest model
/// level / surface, `(Time=1, ny, nx)`). `wrf3d`-style archives carry TK on
/// model levels (`[Time, nz, ny, nx]` or `[nz, ny, nx]`) and return FALSE so
/// the existing 3-D reader (including its staggered-Z destagger) is
/// untouched. The Time-squeeze mirrors what
/// `read_array_f64_first_record_or_all` does on the read side.
fn postproc_tk_is_2d(dim_names: &[&str], shape: &[usize]) -> bool {
    if dim_names.len() != shape.len() {
        return false;
    }
    let squeezed_rank = if dim_names.first().copied() == Some("Time") {
        shape.len() - 1
    } else {
        shape.len()
    };
    squeezed_rank == 2
}

/// A `wrf2d`-style data variable for the post-processed 2-D route: a single
/// plane on the `ny x nx` mass grid — shaped `[Time, ny, nx]`, `[1, ny, nx]`,
/// or `[ny, nx]` — that is not a coordinate/bookkeeping variable (the raw
/// wrfout blocklist plus the lat/lon/time axis names the grid reader
/// consumes). Staggered planes and model-level stacks never match the shape
/// rule.
fn is_postproc_2d_data_plane(
    name: &str,
    dim_names: &[&str],
    shape: &[usize],
    ny: usize,
    nx: usize,
) -> bool {
    if !raw_wrf_variable_allowed(name) || is_coordinate_axis_name(name) {
        return false;
    }
    if dim_names.len() != shape.len() {
        return false;
    }
    match shape {
        [y, x] => *y == ny && *x == nx,
        [t, y, x] => (*t == 1 || dim_names[0] == "Time") && *y == ny && *x == nx,
        _ => false,
    }
}

/// Coordinate-axis variable names the 2-D enumeration must skip (the grid
/// reader consumes these; they are not data planes). The uppercase WRF forms
/// (XLAT/XLONG/…) are already on `raw_wrf_variable_allowed`'s blocklist.
fn is_coordinate_axis_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "lat" | "lon" | "latitude" | "longitude" | "time" | "times" | "xtime"
    )
}

/// Build the store hour for a PURE 2-D post-processed surface archive
/// (CONUS-II GDEX `wrf2d`: `Time(1)` + ~190 single-plane data variables, no
/// model-level stacks — owner-reported "bad shape for variable TK" when the
/// 3-D reader claimed one). Yields the canonical surface suite (these files
/// ship T2/Q2/PSFC/U10/V10/WSPD10MAX) plus EVERY mass-grid data plane as a
/// raw `wrf_*` field through the same derived-slot store-write convention
/// the raw-wrfout light import uses, so picker labels and Solar fallback
/// styles resolve identically. No isobaric volumes and no computed severe
/// suite — nothing 3-D exists to build them from; the files carry their own
/// pre-computed severe planes (SBCAPE/MUCAPE/SRH01/…), which land as raw
/// fields. Memory stays flat: one netcrust f64 record (~16 MB on the
/// 1419 x 1429 CONUS-II grid) is narrowed to f32 and dropped per plane.
fn postprocessed_wrf2d_hour(
    nc: &NcFile,
    path: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<PostprocessedWrfHour, ImportError> {
    // Same netcrust-only source as the 3-D post-processed route: wrf-core
    // cannot open these files (no raw T/PB).
    let src = PlaneSource::netcrust_only(nc);
    let lat = read_first_2d_any(&src, &["XLAT", "XLAT_M", "lat", "latitude"])?;
    let lon = read_first_2d_any(&src, &["XLONG", "XLONG_M", "lon", "longitude"])?;
    if lat.nx != lon.nx || lat.ny != lon.ny || lat.values.len() != lon.values.len() {
        return Err(ImportError::GridMismatch(path.to_path_buf()));
    }
    let (nx, ny) = (lat.nx, lat.ny);
    let shape = GridShape::new(nx, ny)?;
    let grid = LatLonGrid::new(shape, lat.values, lon.values)?;
    let projection = wrf_projection(nc);

    let mut canonical = Vec::new();
    push_canonical_surface_fields(&mut canonical, &src, &grid, &projection)?;
    // The store writer needs at least one extracted 2-D field to carry the
    // hour grid. Real wrf2d archives ship PSFC/T2/U10/…, so this fallback is
    // for pathological surface archives only: lowest-model-level P (always
    // present — it is part of the post-processed gate) as the
    // surface-pressure proxy, the same approximation the 3-D route documents
    // for its parcel state.
    if canonical.is_empty() {
        push_direct(
            &mut canonical,
            &src,
            &grid,
            projection.clone(),
            "P",
            "surface_pressure",
            FieldSelector::surface(CanonicalField::Pressure),
            Some("Pa"),
        )?;
    }
    if canonical.is_empty() {
        return Err(ImportError::NoFields(path.to_path_buf()));
    }

    // Every mass-grid data plane, read-narrow-drop, one at a time. A count
    // line every few planes keeps the dock's progress live without spamming
    // the channel (~190 variables on a real wrf2d file).
    let variables = nc.variables()?;
    let planned = variables
        .iter()
        .filter(|var| {
            let names: Vec<&str> = var.dimensions().iter().map(|dim| dim.name()).collect();
            is_postproc_2d_data_plane(var.name(), &names, &var.shape(), ny, nx)
        })
        .map(|var| var.name().to_string())
        .collect::<Vec<_>>();
    let total = planned.len();
    progress(format!("reading {total} 2-D surface planes"));
    let mut seen = HashSet::<String>::new();
    let mut raw_2d = Vec::new();
    for (index, wrf_name) in planned.iter().enumerate() {
        if index % 10 == 0 {
            progress(format!(
                "reading 2-D surface plane {}/{total} ({wrf_name})",
                index + 1
            ));
        }
        let Some(plane) = read_first_2d(&src, wrf_name)? else {
            continue;
        };
        if plane.nx != nx || plane.ny != ny {
            continue;
        }
        let name = format!("wrf_{}", sanitize_store_var_name(wrf_name));
        if name == "wrf_" || !seen.insert(name.clone()) {
            continue;
        }
        raw_2d.push(RawField2D {
            name,
            units: variable_units(nc, wrf_name).unwrap_or_else(|| "1".to_string()),
            values: plane.values,
        });
    }
    progress(format!(
        "read {} 2-D surface planes ({} canonical fields)",
        raw_2d.len(),
        canonical.len()
    ));

    Ok((canonical, Vec::new(), Vec::new(), raw_2d))
}

/// [`try_postprocessed_wrf`] against an already-open netcrust handle, so the
/// light import's per-file loop pays `netcrust::open`'s eager NetCDF-4
/// metadata indexing once, not once per stage (~57 s per open on a 2 GB
/// compressed 250 m wrfout — docs/wrf-import-large-grids.md).
fn try_postprocessed_wrf_shared(
    nc: &NcFile,
    path: &Path,
    progress: &mut dyn FnMut(String),
) -> Result<Option<PostprocessedWrfHour>, ImportError> {
    let is_postprocessed = nc.variable("TK").is_some()
        && nc.variable("Z").is_some()
        && nc.variable("P").is_some()
        && nc.variable("PB").is_none();
    if !is_postprocessed {
        return Ok(None);
    }
    // CONUS-II `wrf2d` surface archives pass the TK/Z/P gate too, but carry
    // every variable as a SINGLE lowest-model-level / surface plane — the 3-D
    // reader below would fail on them ("bad shape for variable TK",
    // owner-reported). Route them to the 2-D-only import instead.
    let tk_is_2d = nc
        .variable("TK")
        .map(|var| {
            let names: Vec<&str> = var.dimensions().iter().map(|dim| dim.name()).collect();
            postproc_tk_is_2d(&names, &var.shape())
        })
        .unwrap_or(false);
    if tk_is_2d {
        return postprocessed_wrf2d_hour(nc, path, progress).map(Some);
    }

    // Post-processed climate files stay entirely on netcrust: wrf-core can't
    // open them (no raw T/PB), so there is no fast plane path here.
    let src = PlaneSource::netcrust_only(nc);
    let lat = read_first_2d_any(&src, &["XLAT", "XLAT_M", "lat", "latitude"])?;
    let lon = read_first_2d_any(&src, &["XLONG", "XLONG_M", "lon", "longitude"])?;
    if lat.nx != lon.nx || lat.ny != lon.ny {
        return Err(ImportError::GridMismatch(path.to_path_buf()));
    }
    let (nx, ny) = (lat.nx, lat.ny);
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| ImportError::BadShape("grid".to_string(), vec![ny, nx]))?;
    let shape = GridShape::new(nx, ny)?;
    let grid = LatLonGrid::new(shape, lat.values, lon.values)?;
    let projection = wrf_projection(nc);

    // 3D mass-point state. `read3d` verifies the horizontal shape and returns
    // the level count. `into_values` hands back the decoded buffer without a
    // copy — each of these is `nz * cells * 8` bytes (hundreds of MB on a
    // CONUS-II grid), so `values().to_vec()` would double the transient cost.
    let read3d = |name: &str| -> Result<(Vec<f64>, usize), ImportError> {
        let array = nc.read_array_f64_first_record_or_all(name)?;
        let s = array.shape().to_vec();
        if s.len() != 3 || s[1] != ny || s[2] != nx {
            return Err(ImportError::BadShape(name.to_string(), s));
        }
        let nz = s[0];
        Ok((array.into_values(), nz))
    };
    progress("reading post-processed 3D fields (TK/P/Z/QVAPOR)".to_string());
    // `tk` and `z_m` are `mut`: after the iso interpolation they are converted
    // in place (K -> C, MSL -> AGL) for the severe suite below, instead of
    // allocating two more full-3D arrays (hundreds of MB each on CONUS-II).
    let (mut tk, nz) = read3d("TK")?;
    let (p_pa, _) = read3d("P")?;
    let (mut z_m, z_nz) = read3d("Z")?;
    let (qv, _) = read3d("QVAPOR")?;
    // CONUS-II era quirk: the CTRL/history wrf3d files carry Z on the
    // STAGGERED vertical grid (w-levels, nz+1 = bottom_top_stag, like W),
    // while the future-era files carry it destaggered on mass levels (nz).
    // Destagger vertically when needed so both eras import identically.
    if z_nz == nz + 1 {
        destagger_z_to_mass_levels(&mut z_m, nz, cells);
    }
    let expected = nz.checked_mul(cells).unwrap_or(0);
    if expected == 0
        || [tk.len(), p_pa.len(), z_m.len(), qv.len()]
            .iter()
            .any(|len| *len != expected)
    {
        return Err(ImportError::PlaneMismatch);
    }

    // Destagger the C-grid winds to mass points.
    progress("destaggering U/V winds to mass points".to_string());
    let u_mass = destagger_x(nc, "U", nz, ny, nx)?;
    let v_mass = destagger_y(nc, "V", nz, ny, nx)?;

    let p_hpa: Vec<f64> = p_pa.iter().map(|pa| pa / 100.0).collect();
    let dewpoint_k: Vec<f64> = qv
        .iter()
        .zip(&p_pa)
        .map(|(&q, &pa)| dewpoint_k_from_q_p(q, pa))
        .collect();

    let (volumes, surface) = interpolate_iso_volumes(
        &p_hpa,
        &tk,
        &dewpoint_k,
        &z_m,
        &u_mass,
        &v_mass,
        nz,
        cells,
        progress,
    );

    // The 3D file carries no surface fields; synthesize all five from the
    // lowest model level so the sounding column can anchor at the surface.
    let mut canonical = Vec::new();
    let surface_entries: [(&str, FieldSelector, &str, Vec<f32>); 5] = [
        (
            "surface_pressure",
            FieldSelector::surface(CanonicalField::Pressure),
            "Pa",
            surface.surface_pressure_pa,
        ),
        (
            "temperature_2m",
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            surface.temperature_2m_k,
        ),
        (
            "dewpoint_2m",
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            surface.dewpoint_2m_k,
        ),
        (
            "u_10m",
            FieldSelector::height_agl(CanonicalField::UWind, 10),
            "m/s",
            surface.u_10m,
        ),
        (
            "v_10m",
            FieldSelector::height_agl(CanonicalField::VWind, 10),
            "m/s",
            surface.v_10m,
        ),
    ];
    for (name, selector, units, values) in surface_entries {
        push_computed(
            &mut canonical,
            &grid,
            projection.clone(),
            name,
            selector,
            units,
            values,
        )?;
    }

    // Severe/thermo suite via the wrf-core met kernels (postproc_severe.rs
    // documents the approximations). Memory discipline: reuse the 3-D buffers
    // above with two in-place unit conversions instead of new allocations,
    // and give back `dewpoint_k` (only the iso interpolation needed it)
    // before the parcel lifts start.
    drop(dewpoint_k);
    // Surface parcel state from the lowest model level — the post-processed
    // files carry no PSFC/T2/Q2 (same approximation as the synthesized 2 m /
    // 10 m fields above). t2 must be captured in Kelvin BEFORE the in-place
    // Celsius conversion; psfc/q2 borrow the lowest-level planes directly.
    let t2_k: Vec<f64> = tk[..cells].to_vec();
    for value in tk.iter_mut() {
        *value -= 273.15;
    }
    // Height MSL -> AGL with the lowest model level as the terrain proxy (no
    // HGT in these files; documented approximation). Walk levels top-down so
    // the level-0 plane — the terrain itself — is consumed last and zeroes.
    for k in (0..nz).rev() {
        let base = k * cells;
        for cell in 0..cells {
            let terrain = z_m[cell];
            z_m[base + cell] -= terrain;
        }
    }
    let severe_inputs = crate::postproc_severe::SevereInputs {
        nx,
        ny,
        nz,
        pressure_pa: &p_pa,
        pressure_hpa: &p_hpa,
        temperature_c: &tk,
        qvapor: &qv,
        height_agl_m: &z_m,
        u_ms: &u_mass,
        v_ms: &v_mass,
        psfc_pa: &p_pa[..cells],
        t2_k: &t2_k,
        q2_kgkg: &qv[..cells],
    };
    // A pathological column must degrade to "no severe fields for this hour",
    // never fail the import (the heavy getvar loop's isolate_panics rule).
    let severe = match crate::wrf_process::isolate_panics("post-processed severe suite", || {
        Ok::<_, String>(crate::postproc_severe::compute(
            &severe_inputs,
            &mut *progress,
        ))
    }) {
        Ok(fields) => fields,
        Err(err) => {
            progress(format!("severe suite skipped: {err}"));
            Vec::new()
        }
    };

    Ok(Some((canonical, severe, volumes, Vec::new())))
}

/// Destagger a `[nz, ny, nx+1]` (west_east_stag) field to `[nz, ny, nx]` mass
/// points by averaging adjacent x faces.
fn destagger_x(
    nc: &NcFile,
    name: &str,
    nz: usize,
    ny: usize,
    nx: usize,
) -> Result<Vec<f64>, ImportError> {
    let array = nc.read_array_f64_first_record_or_all(name)?;
    let s = array.shape();
    let nxs = nx + 1;
    if s.len() != 3 || s[0] != nz || s[1] != ny || s[2] != nxs {
        return Err(ImportError::BadShape(name.to_string(), s.to_vec()));
    }
    let src = array.values();
    let mut out = vec![0f64; nz * ny * nx];
    for k in 0..nz {
        for y in 0..ny {
            let base_s = (k * ny + y) * nxs;
            let base_d = (k * ny + y) * nx;
            for x in 0..nx {
                out[base_d + x] = 0.5 * (src[base_s + x] + src[base_s + x + 1]);
            }
        }
    }
    Ok(out)
}

/// Destagger a `[nz, ny+1, nx]` (south_north_stag) field to `[nz, ny, nx]` mass
/// points by averaging adjacent y faces.
fn destagger_y(
    nc: &NcFile,
    name: &str,
    nz: usize,
    ny: usize,
    nx: usize,
) -> Result<Vec<f64>, ImportError> {
    let array = nc.read_array_f64_first_record_or_all(name)?;
    let s = array.shape();
    let nys = ny + 1;
    if s.len() != 3 || s[0] != nz || s[1] != nys || s[2] != nx {
        return Err(ImportError::BadShape(name.to_string(), s.to_vec()));
    }
    let src = array.values();
    let mut out = vec![0f64; nz * ny * nx];
    for k in 0..nz {
        for y in 0..ny {
            let base_lo = (k * nys + y) * nx;
            let base_hi = (k * nys + y + 1) * nx;
            let base_d = (k * ny + y) * nx;
            for x in 0..nx {
                out[base_d + x] = 0.5 * (src[base_lo + x] + src[base_hi + x]);
            }
        }
    }
    Ok(out)
}

/// Dewpoint (K) from water-vapor mixing ratio (kg/kg) and pressure (Pa), via
/// vapor pressure and the Bolton inversion — the 3D analog of the 2 m
/// `dewpoint_from_q_psfc` used above.
fn dewpoint_k_from_q_p(q: f64, p_pa: f64) -> f64 {
    if !q.is_finite() || !p_pa.is_finite() || q <= 0.0 || p_pa <= 0.0 {
        return f64::NAN;
    }
    let e = (q * p_pa / (0.622 + q)).max(1.0);
    let ln = (e / 611.2).ln();
    let td_c = 243.5 * ln / (17.67 - ln);
    td_c + 273.15
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destagger_z_averages_adjacent_w_levels_and_truncates() {
        // 3 mass levels, 2 columns; staggered = 4 levels. Level-major layout:
        // [k0c0, k0c1, k1c0, k1c1, ...].
        let mut z = vec![
            0.0, 100.0, // stag level 0
            10.0, 110.0, // stag level 1
            30.0, 130.0, // stag level 2
            70.0, 170.0, // stag level 3
        ];
        destagger_z_to_mass_levels(&mut z, 3, 2);
        assert_eq!(z, vec![5.0, 105.0, 20.0, 120.0, 50.0, 150.0]);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let unique = now_unix();
        std::env::temp_dir().join(format!("rw-local-import-{name}-{unique}"))
    }

    /// The post-processed routing rule (owner-reported CONUS-II wrf2d
    /// misroute, "bad shape for variable TK: [1419, 1429]"): single-plane TK
    /// (surface archive) routes 2-D, model-level TK (wrf3d, either Z era)
    /// stays on the 3-D path. Dim names/shapes taken from the real GDEX
    /// files.
    #[test]
    fn postproc_routing_separates_wrf2d_from_wrf3d() {
        // Real wrf2d TK: (Time, south_north, west_east) = (1, 1419, 1429).
        assert!(postproc_tk_is_2d(
            &["Time", "south_north", "west_east"],
            &[1, 1419, 1429]
        ));
        // Real wrf3d TK: (Time, bottom_top, south_north, west_east).
        assert!(!postproc_tk_is_2d(
            &["Time", "bottom_top", "south_north", "west_east"],
            &[1, 50, 1419, 1429]
        ));
        // No record dim: bare planes route 2-D, bare stacks route 3-D.
        assert!(postproc_tk_is_2d(
            &["south_north", "west_east"],
            &[1419, 1429]
        ));
        assert!(!postproc_tk_is_2d(
            &["bottom_top", "south_north", "west_east"],
            &[50, 1419, 1429]
        ));
        // Degenerate ranks and dims/shape disagreement never claim 2-D.
        assert!(!postproc_tk_is_2d(&["Time"], &[1]));
        assert!(!postproc_tk_is_2d(
            &["Time", "south_north"],
            &[1, 1419, 1429]
        ));
    }

    /// The wrf2d plane scanner: accepts single mass-grid planes in all three
    /// stored shapes, rejects coordinates, bookkeeping axes, staggered
    /// single-level winds, and model-level stacks. Names/shapes from the
    /// real wrf2d probe (192 variables, 185 mass-grid data planes).
    #[test]
    fn wrf2d_plane_scanner_selects_mass_grid_data_vars() {
        let (ny, nx) = (1419usize, 1429usize);
        let t = &["Time", "south_north", "west_east"][..];
        // Data planes: float and int-bucket vars alike.
        assert!(is_postproc_2d_data_plane("TK", t, &[1, ny, nx], ny, nx));
        assert!(is_postproc_2d_data_plane("SBCAPE", t, &[1, ny, nx], ny, nx));
        assert!(is_postproc_2d_data_plane(
            "I_ACLWDNB",
            t,
            &[1, ny, nx],
            ny,
            nx
        ));
        // Bare (ny, nx) planes count too, as does a non-Time leading dim of
        // length 1, and a multi-record Time dim (record 0 is read).
        assert!(is_postproc_2d_data_plane(
            "TSK",
            &["south_north", "west_east"],
            &[ny, nx],
            ny,
            nx
        ));
        assert!(is_postproc_2d_data_plane(
            "TSK",
            &["level", "south_north", "west_east"],
            &[1, ny, nx],
            ny,
            nx
        ));
        assert!(is_postproc_2d_data_plane("T2", t, &[4, ny, nx], ny, nx));
        // Coordinates and bookkeeping are never data.
        assert!(!is_postproc_2d_data_plane(
            "XLAT",
            &["south_north", "west_east"],
            &[ny, nx],
            ny,
            nx
        ));
        assert!(!is_postproc_2d_data_plane(
            "lat",
            &["south_north", "west_east"],
            &[ny, nx],
            ny,
            nx
        ));
        assert!(!is_postproc_2d_data_plane("XTIME", &["Time"], &[1], ny, nx));
        assert!(!is_postproc_2d_data_plane(
            "Times",
            &["Time", "DateStrLen"],
            &[1, 19],
            ny,
            nx
        ));
        // Staggered single-level winds do not sit on the mass grid.
        assert!(!is_postproc_2d_data_plane(
            "U",
            &["Time", "south_north", "west_east_stag"],
            &[1, ny, nx + 1],
            ny,
            nx
        ));
        assert!(!is_postproc_2d_data_plane(
            "V",
            &["Time", "south_north_stag", "west_east"],
            &[1, ny + 1, nx],
            ny,
            nx
        ));
        // Model-level stacks belong to the 3-D route.
        assert!(!is_postproc_2d_data_plane(
            "TK",
            &["Time", "bottom_top", "south_north", "west_east"],
            &[1, 50, ny, nx],
            ny,
            nx
        ));
    }

    /// Real-data proof for the CONUS-II `wrf2d` 2-D route (the owner's
    /// failing file): runs the full light import on the surface archive
    /// named by `RW_WRF2D_FIXTURE` and asserts the canonical suite + raw
    /// `wrf_*` planes land with physical values and NO iso volumes. Skips
    /// (passing) when the env var is unset.
    #[test]
    fn optional_wrf2d_fixture_imports_surface_planes() {
        let Ok(fixture) = std::env::var("RW_WRF2D_FIXTURE") else {
            eprintln!("skipping; set RW_WRF2D_FIXTURE to a CONUS-II wrf2d file");
            return;
        };
        let store_root = temp_dir("wrf2d");
        let start = std::time::Instant::now();
        let summary = import_paths(&[PathBuf::from(&fixture)], &store_root, &mut |message| {
            eprintln!("[{:9.2?}] {message}", start.elapsed());
        })
        .unwrap();
        eprintln!(
            "[{:9.2?}] DONE: {} hour(s), {} variables; peak RSS {}",
            start.elapsed(),
            summary.hours_written,
            summary.variables.len(),
            peak_rss_label()
        );
        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.hours_written, 1);
        // Canonical suite from the file's own T2/Q2/PSFC/U10/V10 planes.
        for var in [
            "temperature_2m",
            "dewpoint_2m",
            "relative_humidity_2m",
            "surface_pressure",
            "u_10m",
            "v_10m",
            "wind_speed_10m",
            "wind_speed_10m_max",
        ] {
            assert!(
                summary.variables.iter().any(|name| name == var),
                "{var} missing: {:?}",
                summary.variables
            );
        }
        // Raw planes: the misrouting trio plus a severe plane and an
        // accumulated-flux plane, under the light-import wrf_* naming.
        for var in ["wrf_tk", "wrf_z", "wrf_p", "wrf_sbcape", "wrf_aclwdnb"] {
            assert!(
                summary.variables.iter().any(|name| name == var),
                "{var} missing: {:?}",
                summary.variables
            );
        }
        // A pure 2-D archive must not synthesize sounding volumes.
        assert!(
            !summary.variables.iter().any(|name| name.ends_with("_iso")),
            "unexpected iso volumes: {:?}",
            summary.variables
        );

        // Value roundtrip through the store: lowest-model-level TK must be
        // physical air temperature over the CONUS grid.
        let hour = store_root
            .join(&summary.model)
            .join(&summary.run)
            .join("f000.rws");
        let reader = rw_store::reader::HourReader::open(&hour).expect("open hour");
        let tk = reader.read_full_2d("wrf_tk").expect("read wrf_tk");
        let finite = tk.iter().filter(|value| value.is_finite()).count();
        assert!(
            finite > tk.len() / 2,
            "wrf_tk mostly NaN: {finite}/{}",
            tk.len()
        );
        for value in tk.iter().filter(|value| value.is_finite()) {
            assert!((180.0..=340.0).contains(value), "TK {value} K non-physical");
        }

        let _ = std::fs::remove_dir_all(store_root);
    }

    #[test]
    fn wrf_timestamp_accepts_colon_and_underscore_time() {
        let colon = Path::new("wrfout_d02_1974-04-03_09:00:00");
        let underscore = Path::new("wrfout_d02_1974-04-03_09_00_00");
        assert_eq!(
            timestamp_from_path(colon).as_deref(),
            Some("19740403_090000")
        );
        assert_eq!(
            timestamp_from_path(underscore).as_deref(),
            Some("19740403_090000")
        );
    }

    #[test]
    fn folder_scan_finds_extensionless_nested_wrf_files() {
        let root = temp_dir("scan");
        let nested = root.join("member").join("d02");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::File::create(nested.join("wrfout_d02_1974-04-03_09_00_00")).unwrap();
        std::fs::File::create(root.join("not_a_model.txt")).unwrap();

        let files = supported_files_in_folder(&root);
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("wrfout")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// Instrumented real-fixture guard for the light import (the "📄 WRF/
    /// NetCDF file…" dock path): runs `import_paths` on the wrfout named by
    /// `RW_LOCAL_IMPORT_FIXTURE`, forwarding every progress line to stderr
    /// with a timestamp and printing peak RSS (`VmHWM`) at the end — the
    /// before/after measurement harness for the large-grid memory fix
    /// (docs/wrf-import-large-grids.md). Release builds only on large grids.
    #[test]
    fn optional_wrf_fixture_imports_to_store() {
        let Ok(fixture) = std::env::var("RW_LOCAL_IMPORT_FIXTURE") else {
            eprintln!("skipping WRF import fixture; set RW_LOCAL_IMPORT_FIXTURE");
            return;
        };
        let store_root = temp_dir("store");
        let start = std::time::Instant::now();
        let mut lines = Vec::new();
        let summary = import_paths(&[PathBuf::from(&fixture)], &store_root, &mut |message| {
            eprintln!("[{:9.2?}] {message}", start.elapsed());
            lines.push(message);
        })
        .unwrap();
        eprintln!(
            "[{:9.2?}] DONE: {} hour(s), {} variables; peak RSS {}",
            start.elapsed(),
            summary.hours_written,
            summary.variables.len(),
            peak_rss_label()
        );
        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.hours_written, 1);
        assert!(summary.variables.iter().any(|var| var == "temperature_2m"));
        assert!(summary.variables.iter().any(|var| var == "dewpoint_2m"));
        assert!(summary.variables.iter().any(|var| var == "wind_speed_10m"));
        // No `apcp` assert: this harness runs against ANY wrfout, and some
        // (e.g. the Enderlin 250 m d03 outputs) carry no RAINC/RAINNC — the
        // variables line above shows what the file actually yielded.
        // Progress must stream per-stage detail, not one line per file: the
        // 2D read, each wrf-core sounding field, interpolation percentages,
        // and the store write all pass through the same channel the dock
        // renders.
        assert!(
            lines.iter().any(|l| l.contains("file 1/1")),
            "stage lines must carry the file position: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("reading 2D surface fields")),
            "missing 2D-read stage: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("sounding field 5/5")),
            "missing per-field getvar stages: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("isobaric levels")),
            "missing interpolation stages: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("writing forecast hour")),
            "missing store-write stage: {lines:?}"
        );

        let _ = std::fs::remove_dir_all(store_root);
    }

    /// Value-identity proof for the fast 2-D read path
    /// (docs/wrf-import-large-grids.md): on the real wrfout fixture
    /// (`RW_LOCAL_IMPORT_FIXTURE`, same resolution as
    /// `optional_wrf_fixture_imports_to_store`), every `[Time, …, ny, nx]`
    /// plane must be BIT-identical between the legacy netcrust read
    /// (`read_first_2d_netcrust`) and the wrf-core fast path, the fast path
    /// must actually engage for every such plane (no silent netcrust
    /// fallback), and the end-to-end 2-D field set the import builds
    /// (canonical + raw + grid) must match name-for-name, unit-for-unit,
    /// bit-for-bit between `read_wrf_2d_fields` with and without the wrf-core
    /// handle.
    #[test]
    fn optional_wrf_fixture_fast_and_netcrust_2d_reads_match() {
        let Ok(fixture) = std::env::var("RW_LOCAL_IMPORT_FIXTURE") else {
            eprintln!("skipping WRF read-path identity; set RW_LOCAL_IMPORT_FIXTURE");
            return;
        };
        let path = PathBuf::from(&fixture);
        let nc = netcrust::open(&path).expect("netcrust opens the fixture");
        let wrf = WrfFile::open(&path).expect("wrf-core opens the wrfout fixture");

        // Per-plane sweep: every WRF-record-layout variable in the file — a
        // superset of the planes the import reads (canonical names, derived
        // inputs, and the raw mass-grid loop all go through read_first_2d).
        let slow = PlaneSource::netcrust_only(&nc);
        let fast = PlaneSource::new(&nc, Some(&wrf));
        let mut compared = 0usize;
        for var in nc.variables().expect("list fixture variables") {
            let name = var.name();
            let dims = var.dimensions();
            if dims.len() < 3 || dims[0].name() != "Time" {
                continue;
            }
            let legacy = read_first_2d(&slow, name)
                .unwrap_or_else(|err| panic!("{name}: netcrust read failed: {err}"))
                .unwrap_or_else(|| panic!("{name}: netcrust read yielded no plane"));
            let routed = read_first_2d(&fast, name)
                .unwrap_or_else(|err| panic!("{name}: fast-path read failed: {err}"))
                .unwrap_or_else(|| panic!("{name}: fast-path read yielded no plane"));
            assert_eq!(
                (legacy.nx, legacy.ny),
                (routed.nx, routed.ny),
                "{name}: plane shape differs between read paths"
            );
            assert_bits_eq(name, &legacy.values, &routed.values);
            compared += 1;
        }
        assert!(
            compared >= 20,
            "fixture only exposed {compared} record-layout planes — wrong fixture?"
        );
        assert_eq!(
            fast.wrf_reads.get(),
            compared,
            "every record-layout plane must take the wrf-core fast path"
        );
        assert!(
            fast.netcrust_fallbacks.borrow().is_empty(),
            "unexpected netcrust fallbacks: {:?}",
            fast.netcrust_fallbacks.borrow()
        );
        eprintln!("read-path identity: {compared} planes bit-identical");

        // End-to-end: the exact field set the import writes, both routes.
        let legacy_fields =
            read_wrf_2d_fields(&nc, &path, None, &mut |_: String| {}).expect("legacy 2D read");
        let fast_fields = read_wrf_2d_fields(&nc, &path, Some(&wrf), &mut |_: String| {})
            .expect("fast-path 2D read");
        assert_bits_eq(
            "grid latitudes",
            &legacy_fields.grid.lat_deg,
            &fast_fields.grid.lat_deg,
        );
        assert_bits_eq(
            "grid longitudes",
            &legacy_fields.grid.lon_deg,
            &fast_fields.grid.lon_deg,
        );
        assert_eq!(
            legacy_fields
                .canonical
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            fast_fields
                .canonical
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            "canonical field names/ordering changed"
        );
        for ((name, legacy), (_, routed)) in
            legacy_fields.canonical.iter().zip(&fast_fields.canonical)
        {
            assert_eq!(legacy.units, routed.units, "{name}: units changed");
            assert_bits_eq(name, &legacy.values, &routed.values);
        }
        assert_eq!(
            legacy_fields
                .raw_2d
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            fast_fields
                .raw_2d
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            "raw field names/ordering changed"
        );
        for (legacy, routed) in legacy_fields.raw_2d.iter().zip(&fast_fields.raw_2d) {
            assert_eq!(legacy.units, routed.units, "{}: units changed", legacy.name);
            assert_bits_eq(&legacy.name, &legacy.values, &routed.values);
        }
        eprintln!(
            "end-to-end identity: {} canonical + {} raw fields bit-identical",
            legacy_fields.canonical.len(),
            legacy_fields.raw_2d.len()
        );
    }

    /// Bitwise f32 equality (NaN == NaN: both read paths narrow every
    /// non-finite source value to the same `f32::NAN` constant).
    fn assert_bits_eq(name: &str, legacy: &[f32], routed: &[f32]) {
        assert_eq!(legacy.len(), routed.len(), "{name}: plane length differs");
        for (index, (a, b)) in legacy.iter().zip(routed).enumerate() {
            assert!(
                a.to_bits() == b.to_bits(),
                "{name}[{index}]: {a} ({:#010x}) != {b} ({:#010x})",
                a.to_bits(),
                b.to_bits()
            );
        }
    }

    /// Peak resident set (Linux `VmHWM`), for the instrumented fixture runs on
    /// the verify node; other platforms report unavailable.
    fn peak_rss_label() -> String {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("VmHWM"))
                    .map(|line| {
                        line.split_whitespace()
                            .skip(1)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            })
            .unwrap_or_else(|| "unavailable (no /proc)".to_string())
    }

    /// The dock consumes the worker through the message channel: a failing
    /// import must still deliver a terminal `Done(Err)` (never a hang, never
    /// a bare disconnect) — the UI's completion path depends on it.
    #[test]
    fn spawn_import_delivers_done_error_for_bad_selection() {
        let task = spawn_import_paths(
            vec![PathBuf::from("definitely-missing.wrfout.nc")],
            temp_dir("spawn-err"),
        );
        loop {
            match task.rx.recv() {
                Ok(LocalImportMessage::Progress(_)) => continue,
                Ok(LocalImportMessage::Done(result)) => {
                    result.expect_err("missing file must fail the import");
                    break;
                }
                Err(err) => panic!("worker died without Done: {err}"),
            }
        }
    }

    /// End-to-end guard for the post-processed climate-wrfout path (TK/Z/P, no
    /// raw T/PB, no surface fields): the store must land the `*_iso` volumes +
    /// a synthesized `surface_pressure`, with physical temps, monotonic height,
    /// and sane winds. Gated on `RW_POSTPROCESSED_WRF_FIXTURE` (a `wrf3d`-style
    /// CONUS-I/II / GDEX file).
    #[test]
    fn optional_postprocessed_fixture_sounds() {
        let Ok(fixture) = std::env::var("RW_POSTPROCESSED_WRF_FIXTURE") else {
            eprintln!("skipping; set RW_POSTPROCESSED_WRF_FIXTURE to a TK/Z/P wrf3d file");
            return;
        };
        let store_root = temp_dir("postproc");
        let summary = import_paths(&[PathBuf::from(&fixture)], &store_root, &mut |message| {
            eprintln!("{message}");
        })
        .unwrap();
        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.hours_written, 1);

        let hour = store_root
            .join(&summary.model)
            .join(&summary.run)
            .join("f000.rws");
        let reader = rw_store::reader::HourReader::open(&hour).expect("open hour");
        for name in [
            "temperature_iso",
            "dewpoint_iso",
            "u_iso",
            "v_iso",
            "height_iso",
        ] {
            let var = reader
                .variable(name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(var.kind, "pressure3d", "{name} should be a volume");
            assert!(!var.levels_hpa.is_empty(), "{name} has no levels");
        }
        assert!(
            reader.variable("surface_pressure").is_some(),
            "surface_pressure must be synthesized from the lowest level"
        );

        let temps = reader.read_profile_3d("temperature_iso", 5.0, 5.0).unwrap();
        let heights = reader.read_profile_3d("height_iso", 5.0, 5.0).unwrap();
        let us = reader.read_profile_3d("u_iso", 5.0, 5.0).unwrap();
        let vs = reader.read_profile_3d("v_iso", 5.0, 5.0).unwrap();

        let finite_t = temps.iter().filter(|value| value.is_finite()).count();
        assert!(finite_t >= 5, "expected finite temps, got {finite_t}");
        for temp in &temps {
            if temp.is_finite() {
                assert!((180.0..=330.0).contains(temp), "T {temp} K non-physical");
            }
        }
        let mut last = f32::NEG_INFINITY;
        for height in &heights {
            if height.is_finite() {
                assert!(*height > last, "height {height} after {last}");
                last = *height;
            }
        }
        for (u, v) in us.iter().zip(&vs) {
            if u.is_finite() {
                assert!(u.abs() < 150.0, "u {u} m/s implausible");
            }
            if v.is_finite() {
                assert!(v.abs() < 150.0, "v {v} m/s implausible");
            }
        }

        let _ = std::fs::remove_dir_all(store_root);
    }

    /// Real-data proof + timing harness for the post-processed severe suite:
    /// runs the full light-import path on the GDEX wrf3d file named by
    /// `RW_POSTPROC_SEVERE_FIXTURE` and asserts every wrf-core-met severe
    /// slug lands in the store with physically sane values. Every progress
    /// line is timestamped (the `severe suite [..]: done` line carries the
    /// suite's own wall time) and peak RSS prints at the end. `#[ignore]`d:
    /// needs a multi-GB real file and minutes of parcel lifts — run once on a
    /// verify node, release build:
    /// `RW_POSTPROC_SEVERE_FIXTURE=/tmp/wrf3d_... cargo test --release
    ///  -p app_ui optional_postproc_severe -- --ignored --nocapture`
    #[test]
    #[ignore = "needs RW_POSTPROC_SEVERE_FIXTURE (real post-processed wrf3d file); run release on a node"]
    fn optional_postproc_severe_fixture_lands_sane_fields() {
        let fixture = std::env::var("RW_POSTPROC_SEVERE_FIXTURE")
            .expect("set RW_POSTPROC_SEVERE_FIXTURE to a TK/Z/P wrf3d file");
        let store_root = temp_dir("postproc-severe");
        let start = std::time::Instant::now();
        let summary = import_paths(&[PathBuf::from(&fixture)], &store_root, &mut |message| {
            eprintln!("[{:9.2?}] {message}", start.elapsed());
        })
        .unwrap();
        eprintln!(
            "[{:9.2?}] DONE: {} hour(s), {} variables; peak RSS {}",
            start.elapsed(),
            summary.hours_written,
            summary.variables.len(),
            peak_rss_label()
        );
        assert_eq!(summary.model, "wrf");
        assert_eq!(summary.hours_written, 1);

        const SEVERE_SLUGS: [&str; 16] = [
            "sbcape",
            "sbcin",
            "mlcape",
            "mlcin",
            "mucape",
            "mucin",
            "lcl",
            "lfc",
            "el",
            "srh_0_1km",
            "srh_0_3km",
            "bulk_shear_0_1km",
            "bulk_shear_0_6km",
            "stp",
            "scp",
            "ehi",
        ];
        for slug in SEVERE_SLUGS {
            assert!(
                summary.variables.iter().any(|name| name == slug),
                "{slug} missing from import summary: {:?}",
                summary.variables
            );
        }

        let hour = store_root
            .join(&summary.model)
            .join(&summary.run)
            .join("f000.rws");
        let reader = rw_store::reader::HourReader::open(&hour).expect("open hour");
        let plane = |slug: &str| -> Vec<f32> {
            reader
                .read_full_2d(slug)
                .unwrap_or_else(|err| panic!("{slug}: read_full_2d failed: {err}"))
        };
        let finite_stats = |slug: &str, values: &[f32]| -> (usize, f32, f32) {
            let mut count = 0usize;
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            for &value in values {
                if value.is_finite() {
                    count += 1;
                    min = min.min(value);
                    max = max.max(value);
                }
            }
            assert!(count > 0, "{slug}: entirely NaN");
            eprintln!("{slug}: {count} finite, min {min}, max {max}");
            (count, min, max)
        };

        // CAPE: nonnegative and physically bounded on every parcel flavor.
        for slug in ["sbcape", "mlcape", "mucape"] {
            let values = plane(slug);
            let (_, min, max) = finite_stats(slug, &values);
            assert!(min >= 0.0, "{slug}: negative CAPE {min}");
            assert!(max <= 8000.0, "{slug}: implausible CAPE {max}");
        }
        // CIN: never positive (kernel accumulates negative buoyancy only).
        for slug in ["sbcin", "mlcin", "mucin"] {
            let values = plane(slug);
            let (_, _, max) = finite_stats(slug, &values);
            assert!(max <= 0.0, "{slug}: positive CIN {max}");
        }
        // Parcel levels: nonnegative heights below the model top.
        for slug in ["lcl", "lfc", "el"] {
            let values = plane(slug);
            let (_, min, max) = finite_stats(slug, &values);
            assert!(min >= 0.0, "{slug}: negative height {min} m AGL");
            assert!(max < 25_000.0, "{slug}: height {max} m above model top");
        }
        // Kinematics: bounded magnitudes.
        for slug in ["srh_0_1km", "srh_0_3km"] {
            let values = plane(slug);
            let (_, min, max) = finite_stats(slug, &values);
            assert!(
                min > -3000.0 && max < 3000.0,
                "{slug}: implausible SRH range {min}..{max}"
            );
        }
        for slug in ["bulk_shear_0_1km", "bulk_shear_0_6km"] {
            let values = plane(slug);
            let (_, min, max) = finite_stats(slug, &values);
            assert!(min >= 0.0, "{slug}: negative shear magnitude {min}");
            assert!(max < 150.0, "{slug}: shear {max} m/s implausible");
        }
        // Composites: finite (finite_stats already proves that) and STP/SCP
        // nonnegative by construction.
        for slug in ["stp", "scp"] {
            let values = plane(slug);
            let (_, min, _) = finite_stats(slug, &values);
            assert!(min >= 0.0, "{slug}: negative composite {min}");
        }
        finite_stats("ehi", &plane("ehi"));

        let _ = std::fs::remove_dir_all(store_root);
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ImportError {
    #[error("no files selected")]
    NoFiles,
    #[error("no supported local model files found in selection")]
    NoSupportedFiles,
    #[error("folder contains too many files to map into rw-store forecast-hour slots: {0}")]
    TooManyFiles(usize),
    #[error("missing any required grid variable: {0:?}")]
    MissingAny(Vec<String>),
    #[error("bad shape for variable {0}: {1:?}")]
    BadShape(String, Vec<usize>),
    #[error("XLAT/XLONG grid dimensions do not match in {0}")]
    GridMismatch(PathBuf),
    #[error("WRF planes do not share the same grid shape")]
    PlaneMismatch,
    #[error("no importable 2D WRF fields found in {0}")]
    NoFields(PathBuf),
    #[error("GRIB1 import failed: {0}")]
    Grib(String),
    #[error("selection mixes GRIB1 and WRF/NetCDF files — import them separately")]
    MixedGribSelection,
    #[error(transparent)]
    Netcdf(#[from] netcrust::Error),
    #[error(transparent)]
    Core(#[from] rustwx_core::RustwxError),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
}
