//! Full WRF processing — ported verbatim from rusty-weather's
//! `rusty-weather-ui` shell (rev edb9d277). Computes the model's 2D
//! diagnostics (CAPE/severe/etc.) and the isobaric sounding volumes through
//! `wrf-core`'s `getvar`, then writes each WRF time as one forecast-hour slot.
//! Heavier than `local_import`, but produces the full model field set.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{SystemTime, UNIX_EPOCH};

use rustwx_core::{
    CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
};
use rw_store::{DerivedFieldInput, WrittenHour, write_hour_from_fields_with_derived};
use serde::{Deserialize, Serialize};

use crate::wrf_volumes::{IsoVolume, SurfaceFallback, build_iso_volumes};
use wrf_core::variables::{VARS, VarDim};
use wrf_core::{ComputeOpts, VarOutput, WrfFile, getvar};

#[derive(Debug)]
pub struct WrfProcessTask {
    pub label: String,
    pub rx: Receiver<WrfProcessMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrfProcessOptions {
    #[serde(default = "default_true")]
    pub core_fields: bool,
    #[serde(default = "default_true")]
    pub diagnostics: bool,
    #[serde(default)]
    pub heavy_ecape: bool,
    #[serde(default = "default_true")]
    pub raw_extras: bool,
    #[serde(default)]
    pub only: Vec<String>,
    #[serde(default)]
    pub skip: Vec<String>,
}

impl Default for WrfProcessOptions {
    fn default() -> Self {
        Self {
            core_fields: true,
            diagnostics: true,
            heavy_ecape: false,
            raw_extras: true,
            only: Vec::new(),
            skip: Vec::new(),
        }
    }
}

impl WrfProcessOptions {
    pub fn normalized(mut self) -> Self {
        self.only = normalize_filter_tokens(self.only);
        self.skip = normalize_filter_tokens(self.skip);
        self
    }

    /// The store field names the current selection WOULD write, for the
    /// import UI's "what will be processed" preview. Mirrors the decisions in
    /// [`read_wrf_products`] using the same [`Self::should_process`] predicate
    /// and the same field catalogs, so the preview tracks the real output.
    /// This is a static plan (it never opens a file); a field a given `wrfout`
    /// happens not to carry is simply skipped at process time with a note.
    pub fn planned_store_fields(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (wrf_name, store_name) in CORE_FIELD_CATALOG {
            if self.should_process(wrf_name, Some(store_name), WrfProductGroup::Core) {
                names.push((*store_name).to_string());
            }
        }
        // Isobaric sounding volumes ride along with the core group (they are
        // gated on `core_fields` in `read_wrf_products`).
        if self.core_fields {
            for iso in ISO_VOLUME_NAMES {
                names.push((*iso).to_string());
            }
        }
        for def in VARS {
            if def.dim != VarDim::TwoD || matches!(def.name, "lat" | "lon" | "cape2d") {
                continue;
            }
            let store_name = derived_name(def.name, None);
            let group = if is_heavy_wrf_diagnostic(&store_name) || is_heavy_wrf_diagnostic(def.name)
            {
                WrfProductGroup::Heavy
            } else {
                WrfProductGroup::Diagnostic
            };
            if self.should_process(def.name, Some(&store_name), group) {
                names.push(store_name);
            }
        }
        for raw in RAW_EXTRA_CATALOG {
            let store_name = derived_name(raw, None);
            if self.should_process(raw, Some(&store_name), WrfProductGroup::Raw) {
                names.push(store_name);
            }
        }
        names.sort();
        names.dedup();
        names
    }

    fn should_process(
        &self,
        wrf_name: &str,
        store_name: Option<&str>,
        group: WrfProductGroup,
    ) -> bool {
        match group {
            WrfProductGroup::Core if !self.core_fields => return false,
            WrfProductGroup::Diagnostic if !self.diagnostics => return false,
            WrfProductGroup::Heavy if !self.heavy_ecape => return false,
            WrfProductGroup::Raw if !self.raw_extras => return false,
            _ => {}
        }

        let keys = product_filter_keys(wrf_name, store_name);
        if !self.only.is_empty()
            && !self
                .only
                .iter()
                .any(|token| keys.iter().any(|key| filter_token_matches(key, token)))
        {
            return false;
        }
        !self
            .skip
            .iter()
            .any(|token| keys.iter().any(|key| filter_token_matches(key, token)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrfProductGroup {
    Core,
    Diagnostic,
    Heavy,
    Raw,
}

/// Core 2D surface fields the heavy path writes, as `(wrf source name, store
/// field name)`. Single source of truth for the `Core` group, shared by the
/// processor's `push_core!` sites and the UI's planned-field preview so the two
/// never drift. `PSFC`/`apcp` are pushed by dedicated blocks but still belong
/// to the `Core` group (they check `should_process(.., Core)`).
const CORE_FIELD_CATALOG: &[(&str, &str)] = &[
    ("terrain", "orography"),
    ("t2", "temperature_2m"),
    ("dp2m", "dewpoint_2m"),
    ("rh2m", "relative_humidity_2m"),
    ("U10", "u_10m"),
    ("V10", "v_10m"),
    ("wspd10", "wind_speed_10m"),
    ("slp", "mslp"),
    ("PSFC", "surface_pressure"),
    ("pw", "pwat"),
    ("maxdbz", "composite_reflectivity"),
    ("UP_HELI_MAX", "updraft_helicity_2to5km"),
    ("apcp", "apcp"),
];

/// Isobaric sounding volumes written alongside the `Core` group (skew-T
/// columns). 3D `pressure3d` store variables, not 2D fields.
const ISO_VOLUME_NAMES: &[&str] = &[
    "temperature_iso",
    "dewpoint_iso",
    "u_iso",
    "v_iso",
    "height_iso",
];

/// Raw WRF model outputs pulled verbatim (no `getvar` diagnostic) for the
/// `Raw` extras group. Single source of truth shared by the processor loop and
/// the planned-field preview.
const RAW_EXTRA_CATALOG: &[&str] = &[
    "PBLH",
    "HFX",
    "LH",
    "SWDOWN",
    "GLW",
    "OLR",
    "TSK",
    "SST",
    "SNOWNC",
    "GRAUPELNC",
    "WSPD10MAX",
    "UP_HELI_MAX",
];

#[derive(Debug)]
pub enum WrfProcessMessage {
    Progress(String),
    Done(Result<WrfProcessSummary, String>),
}

#[derive(Debug, Clone)]
pub struct WrfProcessSummary {
    pub store_root: PathBuf,
    pub model: String,
    pub run: String,
    pub files_seen: usize,
    pub hours_written: usize,
    pub variables: Vec<String>,
    pub notes: Vec<String>,
}

struct WrfHourFields {
    canonical: Vec<(String, SelectedField2D)>,
    derived: Vec<OwnedDerivedField>,
    volumes: Vec<IsoVolume>,
    notes: Vec<String>,
}

struct OwnedDerivedField {
    name: String,
    units: String,
    values: Vec<f32>,
}

pub fn spawn_process_paths(
    paths: Vec<PathBuf>,
    store_root: PathBuf,
    options: WrfProcessOptions,
) -> WrfProcessTask {
    let options = options.normalized();
    let label = if paths.len() == 1 {
        format!("Process WRF {}", display_name(&paths[0]))
    } else {
        format!("Process {} WRF files", paths.len())
    };
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("rw-ui-wrf-process".to_string())
        .spawn({
            let label = label.clone();
            move || {
                lower_import_thread_priority();
                let result = process_paths(&paths, &store_root, &options, &tx).map_err(|err| {
                    if err.trim().is_empty() {
                        format!("{label} failed")
                    } else {
                        err
                    }
                });
                let _ = tx.send(WrfProcessMessage::Done(result));
            }
        })
        .expect("spawn WRF processing worker");
    WrfProcessTask { label, rx }
}

/// Large-grid imports grind for minutes with heavy allocation churn; run the
/// worker below normal priority so the desktop (and the live radar UI) stay
/// responsive. Windows-only: other platforms schedule this fine as-is.
pub(crate) fn lower_import_thread_priority() {
    #[cfg(windows)]
    // SAFETY: GetCurrentThread returns a pseudo-handle that needs no
    // cleanup; SetThreadPriority on it affects only this thread.
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

pub fn is_supported_wrf_file(path: &Path) -> bool {
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
}

pub fn wrf_files_in_folder(folder: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 8;
    const MAX_FILES: usize = 10_000;

    let mut paths = Vec::new();
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_wrf_file(&path) {
                paths.push(path);
                if paths.len() >= MAX_FILES {
                    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                    return paths;
                }
            } else if depth < MAX_DEPTH && path.is_dir() {
                stack.push((path, depth + 1));
            }
        }
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    paths
}

fn process_paths(
    paths: &[PathBuf],
    store_root: &Path,
    options: &WrfProcessOptions,
    tx: &Sender<WrfProcessMessage>,
) -> Result<WrfProcessSummary, String> {
    if paths.is_empty() {
        return Err("No WRF files selected".to_string());
    }

    let mut files = paths
        .iter()
        .filter(|path| is_supported_wrf_file(path))
        .cloned()
        .collect::<Vec<_>>();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err("No supported WRF files selected".to_string());
    }
    let domains = selected_wrf_domains(&files);
    if domains.len() > 1 {
        return Err(format!(
            "Selection mixes WRF domains {}; process one domain at a time so each grid is stored in its own run",
            domains.join(", ")
        ));
    }

    let model = "wrf".to_string();
    let run = process_run_name(&files);
    let mut written = Vec::<WrittenHour>::new();
    let mut all_vars = Vec::<String>::new();
    let mut all_notes = Vec::<String>::new();

    for path in &files {
        if written.len() > u16::MAX as usize {
            return Err(format!("Too many WRF times to store: {}", written.len()));
        }
        let _ = tx.send(WrfProcessMessage::Progress(format!(
            "Opening WRF {}",
            display_name(path)
        )));
        // Probe the raw wrf-core reader FIRST: raw wrfouts are the common
        // wrench-flow case and the probe fails fast on post-processed files
        // (they carry no raw T). The previous order ran netcrust::open's
        // eager NetCDF-4 metadata indexing (~57 s on a 2 GB compressed
        // wrfout, docs/wrf-import-large-grids.md) on EVERY raw file just to
        // conclude "not post-processed". The two detectors cannot overlap:
        // raw files have T (wrf-core opens), post-processed have TK/Z/P and
        // no PB (netcrust path claims them).
        let raw_open = WrfFile::open(path);
        if raw_open.is_err() {
            // Post-processed climate wrfout (CONUS-I/II, GDEX: derived
            // TK/Z/P, no raw T/PB) — wrf-core can't open these, so route
            // them through the netcrust-based reader before reporting the
            // raw open error below.
            match crate::local_import::try_postprocessed_wrf(path, &mut |message: String| {
                let _ = tx.send(WrfProcessMessage::Progress(message));
            }) {
                Ok(Some((canonical, severe, volumes, raw_2d))) => {
                    let hour = u16::try_from(written.len()).expect("bounded above");
                    let _ = tx.send(WrfProcessMessage::Progress(format!(
                        "Reading post-processed WRF {} -> f{hour:03}",
                        display_name(path)
                    )));
                    let refs = canonical
                        .iter()
                        .map(|(name, field)| (name.as_str(), field))
                        .collect::<Vec<_>>();
                    // Severe/thermo suite under the same store slugs this
                    // path's getvar loop writes for raw wrfouts — the wrench
                    // flow now yields severe fields for post-processed files
                    // too.
                    let mut derived_refs = severe
                        .iter()
                        .map(|field| DerivedFieldInput {
                            name: field.name,
                            units: field.units,
                            values: field.values.as_slice(),
                        })
                        .collect::<Vec<_>>();
                    // Raw `wrf_*` planes from the 2-D wrf2d route (empty on
                    // the 3-D route) — the wrench flow imports pure surface
                    // archives the same way the light import does.
                    derived_refs.extend(raw_2d.iter().map(|field| DerivedFieldInput {
                        name: field.name.as_str(),
                        units: field.units.as_str(),
                        values: field.values.as_slice(),
                    }));
                    let volume_inputs = volumes.iter().map(IsoVolume::as_input).collect::<Vec<_>>();
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
                    )
                    .map_err(|err| format!("Write WRF f{hour:03} failed: {err}"))?;
                    all_vars.extend(result.vars.iter().cloned());
                    written.push(result);
                    continue;
                }
                Ok(None) => {}
                Err(err) => {
                    return Err(format!("Process WRF {} failed: {err}", path.display()));
                }
            }
        }

        let file = raw_open.map_err(|err| format!("Open WRF {} failed: {err}", path.display()))?;

        for timeidx in 0..file.nt {
            if written.len() > u16::MAX as usize {
                return Err(format!("Too many WRF times to store: {}", written.len()));
            }
            let hour = u16::try_from(written.len()).expect("bounded above");
            let _ = tx.send(WrfProcessMessage::Progress(format!(
                "Computing WRF {} time {} -> f{hour:03}",
                display_name(path),
                timeidx
            )));
            let mut progress = |message: String| {
                let _ = tx.send(WrfProcessMessage::Progress(message));
            };
            let fields = read_wrf_products(&file, path, timeidx, options, &mut progress)?;
            if fields.canonical.is_empty() {
                return Err(format!(
                    "WRF {} time {} produced no canonical 2D grid fields",
                    path.display(),
                    timeidx
                ));
            }

            let refs = fields
                .canonical
                .iter()
                .map(|(name, field)| (name.as_str(), field))
                .collect::<Vec<_>>();
            let derived_refs = fields
                .derived
                .iter()
                .map(|field| DerivedFieldInput {
                    name: field.name.as_str(),
                    units: field.units.as_str(),
                    values: field.values.as_slice(),
                })
                .collect::<Vec<_>>();
            let volume_inputs = fields
                .volumes
                .iter()
                .map(IsoVolume::as_input)
                .collect::<Vec<_>>();
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
            )
            .map_err(|err| format!("Write WRF f{hour:03} failed: {err}"))?;
            all_vars.extend(result.vars.iter().cloned());
            all_notes.extend(fields.notes);
            written.push(result);
        }
    }

    all_vars.sort();
    all_vars.dedup();
    all_notes.sort();
    all_notes.dedup();
    Ok(WrfProcessSummary {
        store_root: store_root.to_path_buf(),
        model,
        run,
        files_seen: files.len(),
        hours_written: written.len(),
        variables: all_vars,
        notes: all_notes,
    })
}

fn read_wrf_products(
    file: &WrfFile,
    path: &Path,
    timeidx: usize,
    options: &WrfProcessOptions,
    progress: &mut impl FnMut(String),
) -> Result<WrfHourFields, String> {
    let lat = file
        .xlat(timeidx)
        .map_err(|err| format!("Read XLAT from {} failed: {err}", path.display()))?;
    let lon = file
        .xlong(timeidx)
        .map_err(|err| format!("Read XLONG from {} failed: {err}", path.display()))?;
    let shape = GridShape::new(file.nx, file.ny).map_err(|err| err.to_string())?;
    if lat.len() != shape.len() || lon.len() != shape.len() {
        return Err(format!(
            "WRF {} grid mismatch: expected {} cells, got lat {} lon {}",
            path.display(),
            shape.len(),
            lat.len(),
            lon.len()
        ));
    }
    let grid = LatLonGrid::new(
        shape,
        lat.iter().map(|value| *value as f32).collect(),
        lon.iter().map(|value| *value as f32).collect(),
    )
    .map_err(|err| err.to_string())?;
    let projection = wrf_projection(file);

    let mut fields = WrfHourFields {
        canonical: Vec::new(),
        derived: Vec::new(),
        volumes: Vec::new(),
        notes: Vec::new(),
    };

    macro_rules! push_core {
        ($wrf:expr, $store:expr, $selector:expr, $units:expr) => {
            if options.should_process($wrf, Some($store), WrfProductGroup::Core) {
                push_canonical(
                    &mut fields,
                    file,
                    timeidx,
                    &grid,
                    projection.clone(),
                    $wrf,
                    $store,
                    $selector,
                    $units,
                );
            }
        };
    }

    push_core!(
        "terrain",
        "orography",
        FieldSelector::surface(CanonicalField::GeopotentialHeight),
        None
    );
    push_core!(
        "t2",
        "temperature_2m",
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
        Some("K")
    );
    push_core!(
        "dp2m",
        "dewpoint_2m",
        FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
        Some("K")
    );
    push_core!(
        "rh2m",
        "relative_humidity_2m",
        FieldSelector::height_agl(CanonicalField::RelativeHumidity, 2),
        Some("%")
    );
    push_core!(
        "U10",
        "u_10m",
        FieldSelector::height_agl(CanonicalField::UWind, 10),
        Some("m/s")
    );
    push_core!(
        "V10",
        "v_10m",
        FieldSelector::height_agl(CanonicalField::VWind, 10),
        Some("m/s")
    );
    push_core!(
        "wspd10",
        "wind_speed_10m",
        FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
        Some("m/s")
    );
    push_core!(
        "slp",
        "mslp",
        FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
        Some("Pa")
    );
    // Surface pressure (Pa) — required by the skew-T column builder. WRF PSFC
    // is a raw field (no `getvar` diagnostic), so push it explicitly with a
    // forced "Pa" unit rather than through `push_core!`.
    if options.should_process("PSFC", Some("surface_pressure"), WrfProductGroup::Core) {
        match compute_var(file, "PSFC", timeidx, Some("Pa")) {
            Ok(output) => match single_plane(output, shape.len()) {
                Ok((values, _units)) => push_canonical_values(
                    &mut fields,
                    &grid,
                    projection.clone(),
                    "surface_pressure",
                    FieldSelector::surface(CanonicalField::Pressure),
                    "Pa",
                    values,
                ),
                Err(err) => fields.notes.push(format!("PSFC skipped: {err}")),
            },
            Err(err) => fields.notes.push(format!("PSFC unavailable: {err}")),
        }
    }
    push_core!(
        "pw",
        "pwat",
        FieldSelector::entire_atmosphere(CanonicalField::PrecipitableWater),
        None
    );
    push_core!(
        "maxdbz",
        "composite_reflectivity",
        FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        Some("dBZ")
    );
    push_core!(
        "UP_HELI_MAX",
        "updraft_helicity_2to5km",
        FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
        Some("m2/s2")
    );

    if options.should_process("apcp", Some("apcp"), WrfProductGroup::Core)
        && let Some(values) = total_precip(file, timeidx, shape.len())
    {
        push_canonical_values(
            &mut fields,
            &grid,
            projection.clone(),
            "apcp",
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "kg/m^2",
            values,
        );
    }

    let total_twod = VARS
        .iter()
        .filter(|def| def.dim == VarDim::TwoD && !matches!(def.name, "lat" | "lon" | "cape2d"))
        .count();
    let mut diagnostic_index = 0usize;
    for def in VARS {
        if def.dim != VarDim::TwoD || matches!(def.name, "lat" | "lon" | "cape2d") {
            continue;
        }
        let store_name = derived_name(def.name, None);
        let group = if is_heavy_wrf_diagnostic(&store_name) || is_heavy_wrf_diagnostic(def.name) {
            WrfProductGroup::Heavy
        } else {
            WrfProductGroup::Diagnostic
        };
        if !options.should_process(def.name, Some(&store_name), group) {
            continue;
        }
        diagnostic_index += 1;
        progress(format!(
            "Computing WRF diagnostic {diagnostic_index}/{total_twod}: {}",
            def.name
        ));
        match compute_var(file, def.name, timeidx, None) {
            Ok(output) => push_derived_output(&mut fields, def.name, output, shape.len()),
            Err(err) => fields
                .notes
                .push(format!("{} unavailable: {err}", def.name)),
        }
    }

    for raw in RAW_EXTRA_CATALOG {
        let store_name = derived_name(raw, None);
        if !options.should_process(raw, Some(&store_name), WrfProductGroup::Raw) {
            continue;
        }
        if let Ok(output) = compute_var(file, raw, timeidx, None) {
            push_derived_output(&mut fields, raw, output, shape.len());
        }
    }

    // Isobaric sounding volumes (temperature_iso/dewpoint_iso/u_iso/v_iso/
    // height_iso) so imported WRF runs produce skew-T soundings like the
    // downloaded models. Failure here never fails the hour — the 2D fields
    // still write; only the sounding is unavailable.
    //
    // NOTE: wrf-core's per-timestep intermediate cache must stay WARM into
    // this block. The volume build re-getvars pressure/temp/td/height/uvmet;
    // with the cache populated by the diagnostics above those are cheap
    // copies (measured peak 8.85 GB on the 800x800x79 Enderlin grid).
    // Clearing the cache first was measured to more than DOUBLE the peak
    // (18.3 GB): every read then recomputes its whole dependency chain
    // (staggered reads, destaggering, theta->T, …) with multi-hundred-MB
    // transients stacking on the re-growing cache. `build_iso_volumes` itself
    // clears the cache right after its LAST getvar (the hour's last), so the
    // interpolation loop and the store write below run without the ~5 GB of
    // dead intermediates.
    if options.core_fields {
        // Isolated for the same reason as `compute_var`: the volume builder's
        // `getvar` reads must degrade to a note, not kill the hour.
        let volumes_result = isolate_panics("isobaric volumes", || {
            build_iso_volumes(file, timeidx, shape.len(), progress)
        });
        match volumes_result {
            Ok((volumes, surface)) => {
                fields.volumes = volumes;
                // Split wrf3d files (CONUS404 / GDEX CONUS-II) omit PSFC (and
                // sometimes T2/Td2/winds); synthesize any missing surface field
                // from the lowest model level so the sounding still builds.
                fill_missing_surface(&mut fields, &grid, projection.clone(), surface);
            }
            Err(err) => fields
                .notes
                .push(format!("WRF isobaric sounding volumes unavailable: {err}")),
        }
    }

    // Release wrf-core's per-timestep intermediate cache before the caller
    // writes the hour to the store. `getvar` memoizes every 3-D f64
    // intermediate (pressure, theta, temperature, geopotential, heights,
    // QVAPOR, destaggered winds, …) inside `WrfFile` and only evicts when the
    // *timestep changes* (`prepare_cache_for_time`); its `clear_cache` is
    // never invoked upstream despite its doc comment. On a 250 m Enderlin
    // grid (800x800x79 ≈ 50.5 M cells, ~400 MB per cached field) that cache
    // holds ~5 GB. Dropping it here — after the hour's last `getvar`, before
    // `write_hour_from_fields_with_derived` — releases that memory for the
    // write phase and beyond at zero recompute cost (measured: working set
    // fell 8.8 GB -> 1.3 GB at this point instead of riding the write).
    // Usually a no-op now that `build_iso_volumes` clears after its last
    // getvar, but still load-bearing when `core_fields` is off (no volume
    // build) or the volume build failed partway. catch_unwind: if a caught
    // diagnostic panic above poisoned the cache mutex, clearing would
    // re-panic; a stuck cache must not fail the hour.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| file.clear_cache()));

    Ok(fields)
}

/// Add any skew-T surface field the file did not already provide, using the
/// lowest-model-level [`SurfaceFallback`]. Real 2D fields (T2, U10, V10, …)
/// already pushed win; only the genuinely-missing ones (chiefly
/// `surface_pressure`) are synthesized.
fn fill_missing_surface(
    fields: &mut WrfHourFields,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    surface: SurfaceFallback,
) {
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
        if !fields
            .canonical
            .iter()
            .any(|(existing, _)| existing == name)
        {
            push_canonical_values(
                fields,
                grid,
                projection.clone(),
                name,
                selector,
                units,
                values,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_canonical(
    fields: &mut WrfHourFields,
    file: &WrfFile,
    timeidx: usize,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    wrf_name: &str,
    store_name: &str,
    selector: FieldSelector,
    units: Option<&str>,
) {
    match compute_var(file, wrf_name, timeidx, units) {
        Ok(output) => match single_plane(output, grid.shape.len()) {
            Ok((values, actual_units)) => push_canonical_values(
                fields,
                grid,
                projection,
                store_name,
                selector,
                &actual_units,
                values,
            ),
            Err(err) => fields.notes.push(format!("{wrf_name} skipped: {err}")),
        },
        Err(err) => fields.notes.push(format!("{wrf_name} unavailable: {err}")),
    }
}

fn push_canonical_values(
    fields: &mut WrfHourFields,
    grid: &LatLonGrid,
    projection: Option<GridProjection>,
    store_name: &str,
    selector: FieldSelector,
    units: &str,
    values: Vec<f32>,
) {
    match SelectedField2D::new(selector, units, grid.clone(), values) {
        Ok(field) => {
            let field = if let Some(projection) = projection {
                field.with_projection(projection)
            } else {
                field
            };
            fields.canonical.push((store_name.to_string(), field));
        }
        Err(err) => fields
            .notes
            .push(format!("{store_name} skipped: invalid field: {err}")),
    }
}

fn push_derived_output(
    fields: &mut WrfHourFields,
    wrf_name: &str,
    output: VarOutput,
    cells: usize,
) {
    let units = output.units.clone();
    match output.shape.as_slice() {
        [ny, nx] if ny * nx == cells => {
            fields.derived.push(OwnedDerivedField {
                name: derived_name(wrf_name, None),
                units,
                values: clean_values(&output.data),
            });
        }
        [count, ny, nx] if ny * nx == cells => {
            for index in 0..*count {
                let start = index * cells;
                let end = start + cells;
                if end > output.data.len() {
                    fields.notes.push(format!(
                        "{wrf_name} skipped split {index}: shape {:?} exceeded data length {}",
                        output.shape,
                        output.data.len()
                    ));
                    return;
                }
                fields.derived.push(OwnedDerivedField {
                    name: derived_name(wrf_name, Some(index)),
                    units: units.clone(),
                    values: clean_values(&output.data[start..end]),
                });
            }
        }
        other => fields.notes.push(format!(
            "{wrf_name} skipped: unsupported shape {:?} for 2D store",
            other
        )),
    }
}

fn single_plane(output: VarOutput, cells: usize) -> Result<(Vec<f32>, String), String> {
    match output.shape.as_slice() {
        [ny, nx] if ny * nx == cells => Ok((clean_values(&output.data), output.units)),
        [1, ny, nx] if ny * nx == cells => Ok((clean_values(&output.data), output.units)),
        other => Err(format!("expected [ny,nx], got {other:?}")),
    }
}

fn compute_var(
    file: &WrfFile,
    name: &str,
    timeidx: usize,
    units: Option<&str>,
) -> Result<VarOutput, String> {
    let opts = ComputeOpts {
        units: units.map(str::to_string),
        ..ComputeOpts::default()
    };
    // Isolate each diagnostic: a panic inside wrf-core (or a crate it calls
    // into, e.g. ecape-rs) on a pathological grid/profile must cost that ONE
    // field — recorded as a note — not the whole multi-minute import. Without
    // this, the unwind kills the rw-ui-wrf-process worker and the entire
    // import dies with "WRF worker stopped unexpectedly".
    isolate_panics(name, || {
        getvar(file, name, Some(timeidx), &opts).map_err(|err| err.to_string())
    })
}

/// Run `f`, converting a panic into an `Err` naming `what`, so one failing
/// field computation degrades to a per-field note instead of unwinding the
/// import worker thread (shared with `local_import`'s volume build, which
/// needs the same guarantee). Inputs are shared references plus `WrfFile`'s
/// internal mutex (which poisons — and is then handled — rather than being
/// observed broken) and progress closures that only append/send messages,
/// so `AssertUnwindSafe` is sound here.
pub(crate) fn isolate_panics<T>(
    what: &str,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|msg| (*msg).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(format!("panicked computing {what}: {message}"))
    })
}

fn total_precip(file: &WrfFile, timeidx: usize, cells: usize) -> Option<Vec<f32>> {
    let mut total = vec![0.0f32; cells];
    let mut found = false;
    for name in ["RAINC", "RAINNC", "RAINSH"] {
        let Ok(output) = compute_var(file, name, timeidx, None) else {
            continue;
        };
        let Ok((values, _)) = single_plane(output, cells) else {
            continue;
        };
        for (accum, value) in total.iter_mut().zip(values) {
            if value.is_finite() {
                *accum += value;
            } else {
                *accum = f32::NAN;
            }
        }
        found = true;
    }
    found.then_some(total)
}

fn clean_values(values: &[f64]) -> Vec<f32> {
    values
        .iter()
        .map(|value| {
            if !value.is_finite() || value.abs() >= 1.0e30 || *value <= -9998.0 {
                f32::NAN
            } else {
                *value as f32
            }
        })
        .collect()
}

fn derived_name(wrf_name: &str, split_index: Option<usize>) -> String {
    let base = match (wrf_name.to_ascii_lowercase().as_str(), split_index) {
        ("uvmet10", Some(0)) => "uvmet10_u".to_string(),
        ("uvmet10", Some(1)) => "uvmet10_v".to_string(),
        ("cloudfrac", Some(0)) => "cloudfrac_low".to_string(),
        ("cloudfrac", Some(1)) => "cloudfrac_mid".to_string(),
        ("cloudfrac", Some(2)) => "cloudfrac_high".to_string(),
        ("bunkers_rm", Some(0)) => "bunkers_rm_u".to_string(),
        ("bunkers_rm", Some(1)) => "bunkers_rm_v".to_string(),
        ("bunkers_lm", Some(0)) => "bunkers_lm_u".to_string(),
        ("bunkers_lm", Some(1)) => "bunkers_lm_v".to_string(),
        ("effective_inflow", Some(0)) => "effective_inflow_base".to_string(),
        ("effective_inflow", Some(1)) => "effective_inflow_top".to_string(),
        (_, Some(index)) => format!("{wrf_name}_{}", index + 1),
        (_, None) => wrf_name.to_string(),
    };
    let base = slug(&base);
    wrf_product_slug(&base)
        .map(str::to_string)
        .unwrap_or_else(|| format!("wrf_{base}"))
}

/// `pub(crate)`: `postproc_severe` asserts every slug it emits is one of
/// these, so the post-processed suite can never drift from the heavy path's
/// store names (and the labels/styles keyed on them).
pub(crate) fn wrf_product_slug(base: &str) -> Option<&'static str> {
    match base {
        "sbcape" => Some("sbcape"),
        "sbcin" => Some("sbcin"),
        "mlcape" => Some("mlcape"),
        "mlcin" => Some("mlcin"),
        "mucape" => Some("mucape"),
        "mucin" => Some("mucin"),
        "dcape" => Some("dcape"),
        "sbecape" => Some("sbecape"),
        "mlecape" => Some("mlecape"),
        "muecape" => Some("muecape"),
        "sbncape" => Some("sbncape"),
        "sbecin" => Some("sbecin"),
        "mlecin" => Some("mlecin"),
        "ecape_scp" => Some("ecape_scp"),
        "ecape_ehi" => Some("ecape_ehi"),
        "ecape_ehi_0_1km" => Some("ecape_ehi_0_1km"),
        "ecape_ehi_0_3km" => Some("ecape_ehi_0_3km"),
        "ecape_stp" => Some("ecape_stp"),
        "lcl" => Some("lcl"),
        "lfc" => Some("lfc"),
        "el" => Some("el"),
        "ecape_lfc" => Some("ecape_lfc"),
        "ecape_el" => Some("ecape_el"),
        "srh1" => Some("srh_0_1km"),
        "srh3" => Some("srh_0_3km"),
        "srh_0_1km" => Some("srh_0_1km"),
        "srh_0_3km" => Some("srh_0_3km"),
        "shear_0_1km" => Some("bulk_shear_0_1km"),
        "shear_0_6km" => Some("bulk_shear_0_6km"),
        "bulk_shear_0_1km" => Some("bulk_shear_0_1km"),
        "bulk_shear_0_6km" => Some("bulk_shear_0_6km"),
        "stp" => Some("stp"),
        "stp_fixed" => Some("stp_fixed"),
        "stp_effective" => Some("stp_effective"),
        "scp" => Some("scp"),
        "ehi" => Some("ehi"),
        "tehi" => Some("tehi"),
        "tts" => Some("tts"),
        "vtp_mod" => Some("vtp_mod"),
        "uhel" => Some("uhel"),
        _ => None,
    }
}

fn wrf_projection(file: &WrfFile) -> Option<GridProjection> {
    match wrf_core::WrfProjection::from_file(file).ok()? {
        wrf_core::WrfProjection::Lambert {
            truelat1,
            truelat2,
            stand_lon,
            ..
        } => Some(GridProjection::LambertConformal {
            standard_parallel_1_deg: truelat1,
            standard_parallel_2_deg: truelat2,
            central_meridian_deg: stand_lon,
        }),
        wrf_core::WrfProjection::PolarStereographic {
            truelat1,
            stand_lon,
            ..
        } => Some(GridProjection::PolarStereographic {
            true_latitude_deg: truelat1,
            central_meridian_deg: stand_lon,
            south_pole_on_projection_plane: false,
        }),
        wrf_core::WrfProjection::Mercator {
            truelat1, cen_lon, ..
        } => Some(GridProjection::Mercator {
            latitude_of_true_scale_deg: truelat1,
            central_meridian_deg: cen_lon,
        }),
        wrf_core::WrfProjection::LatLon { .. } => Some(GridProjection::Geographic),
    }
}

fn process_run_name(files: &[PathBuf]) -> String {
    let first_path = files.first();
    let first = first_path
        .and_then(|path| path.file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("wrfout");
    let domain_suffix = parse_wrf_domain(first)
        .map(|domain| format!("_{domain}"))
        .unwrap_or_default();
    if let Some(stamp) = parse_wrf_timestamp(first) {
        format!("local_{stamp}{domain_suffix}")
    } else {
        format!("local_{}{domain_suffix}", now_unix())
    }
}

pub(crate) fn selected_wrf_domains(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .filter_map(|path| path.file_name())
        .filter_map(|name| name.to_str())
        .filter_map(parse_wrf_domain)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Extract and normalize the WRF domain token from an ordinary unrestricted
/// `wrfout_dNN_*` filename. The file may be extensionless and the domain may
/// contain more than two digits; the returned store token is always at least
/// two digits (`d2` becomes `d02`). A renamed/nonstandard file safely returns
/// `None`, preserving the pre-domain run-name fallback.
pub(crate) fn parse_wrf_domain(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let marker = "wrfout_d";
    for marker_start in lower.match_indices(marker).map(|(start, _)| start) {
        let digits_start = marker_start + marker.len();
        let digits = lower[digits_start..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            continue;
        }
        let domain = digits.parse::<u16>().ok()?;
        if domain > 0 {
            return Some(format!("d{domain:02}"));
        }
    }
    None
}

/// Extract a sortable `YYYYMMDDHHMMSS` stamp from a wrfout-style filename
/// (`wrfout_d03_2025-06-21_01_30_00` / `..._01:30:00`). Shared with
/// `wrf_radar`'s multi-file loop ordering, which must sort frames by model
/// time rather than raw filename.
pub(crate) fn parse_wrf_timestamp(name: &str) -> Option<String> {
    for token in name.split(['.', '/', '\\']) {
        let bytes = token.as_bytes();
        if bytes.len() < 19 {
            continue;
        }
        for start in 0..=bytes.len().saturating_sub(19) {
            let slice = &token[start..start + 19];
            let chars = slice.as_bytes();
            let timestampish = chars[4] == b'-'
                && chars[7] == b'-'
                && chars[10] == b'_'
                && matches!(chars[13], b':' | b'_')
                && matches!(chars[16], b':' | b'_')
                && chars.iter().enumerate().all(|(index, byte)| {
                    matches!(index, 4 | 7 | 10 | 13 | 16) || byte.is_ascii_digit()
                });
            if timestampish {
                return Some(
                    slice
                        .replace('-', "")
                        .replace([':', '_'], "")
                        .chars()
                        .take(14)
                        .collect(),
                );
            }
        }
    }
    None
}

fn slug(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_underscore = false;
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            output.push(lower);
            last_was_underscore = false;
        } else if !last_was_underscore {
            output.push('_');
            last_was_underscore = true;
        }
    }
    output.trim_matches('_').to_string()
}

fn default_true() -> bool {
    true
}

fn normalize_filter_tokens(tokens: Vec<String>) -> Vec<String> {
    let mut normalized = tokens
        .into_iter()
        .flat_map(|token| {
            token
                .split([',', ';', '\n', '\r', '\t', ' '])
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .map(|token| slug(&token))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn product_filter_keys(wrf_name: &str, store_name: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    push_filter_key(&mut keys, wrf_name);
    if let Some(store_name) = store_name {
        push_filter_key(&mut keys, store_name);
    }
    if let Some(mapped) = wrf_product_slug(&slug(wrf_name)) {
        push_filter_key(&mut keys, mapped);
    }
    if let Some(store_name) = store_name.and_then(|name| name.strip_prefix("wrf_")) {
        push_filter_key(&mut keys, store_name);
    }
    keys
}

fn push_filter_key(keys: &mut Vec<String>, value: &str) {
    let key = slug(value);
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

fn filter_token_matches(key: &str, token: &str) -> bool {
    key == token || key.contains(token)
}

fn is_heavy_wrf_diagnostic(name: &str) -> bool {
    let key = slug(name);
    // "ncape" is a full ecape-rs solve (normalized CAPE) but does not
    // literally contain "ecape", so without the explicit match it leaks
    // into the default pass at ~10 s/file on an 800x800x79 grid; in the
    // Heavy group it rides the ecape stack cache for milliseconds.
    key.contains("ecape")
        || matches!(
            key.as_str(),
            "ncape"
                | "sbecape"
                | "mlecape"
                | "muecape"
                | "sbncape"
                | "sbecin"
                | "mlecin"
                | "muecin"
                | "ecape_scp"
                | "ecape_ehi"
                | "ecape_ehi_0_1km"
                | "ecape_ehi_0_3km"
                | "ecape_stp"
                | "ecape_lfc"
                | "ecape_el"
        )
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn writer_build() -> &'static str {
    concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrf_timestamp_with_colons_or_underscores() {
        assert_eq!(
            parse_wrf_timestamp("wrfout_d02_1974-04-03_09:00:00"),
            Some("19740403090000".to_string())
        );
        assert_eq!(
            parse_wrf_timestamp("wrfout_d02_1974-04-03_09_00_00"),
            Some("19740403090000".to_string())
        );
    }

    #[test]
    fn processed_run_names_separate_domains_but_group_one_domain_time_series() {
        let d02_first = PathBuf::from("wrfout_d02_1974-04-03_13_15_00");
        let d02_later = PathBuf::from("wrfout_d02_1974-04-03_13_20_00");
        let d03_first = PathBuf::from("wrfout_d03_1974-04-03_13_15_00");

        let d02_run = process_run_name(&[d02_first.clone(), d02_later]);
        let d03_run = process_run_name(&[d03_first]);

        assert_eq!(d02_run, "local_19740403131500_d02");
        assert_eq!(d03_run, "local_19740403131500_d03");
        assert_ne!(d02_run, d03_run);
        assert_eq!(process_run_name(&[d02_first]), d02_run);
    }

    #[test]
    fn wrf_domain_parser_accepts_any_extension_and_normalizes_width() {
        assert_eq!(
            parse_wrf_domain("wrfout_d2_1974-04-03_13:15:00.nc"),
            Some("d02".to_owned())
        );
        assert_eq!(
            parse_wrf_domain("WRFOUT_D104_1974-04-03_13_15_00"),
            Some("d104".to_owned())
        );
        assert_eq!(parse_wrf_domain("renamed_output.nc"), None);
    }

    #[test]
    fn mixed_domain_processing_stops_before_writing_a_shared_grid_run() {
        let files = [
            PathBuf::from("wrfout_d02_1974-04-03_13_15_00"),
            PathBuf::from("wrfout_d03_1974-04-03_13_15_00"),
        ];
        let (tx, _rx) = channel();
        let error = process_paths(
            &files,
            Path::new("unused-store"),
            &WrfProcessOptions::default(),
            &tx,
        )
        .expect_err("mixed-domain selection must not share one run");
        assert!(error.contains("d02, d03"), "unexpected error: {error}");
    }

    #[test]
    fn derived_split_names_are_stable() {
        assert_eq!(derived_name("uvmet10", Some(0)), "wrf_uvmet10_u");
        assert_eq!(derived_name("cloudfrac", Some(2)), "wrf_cloudfrac_high");
        assert_eq!(derived_name("sbcape", None), "sbcape");
        assert_eq!(derived_name("srh1", None), "srh_0_1km");
        assert_eq!(derived_name("shear_0_6km", None), "bulk_shear_0_6km");
    }

    #[test]
    fn optional_real_fixture_processes_wrf_products() {
        let Some(path) = std::env::var_os("RW_WRF_PROCESS_FIXTURE") else {
            return;
        };
        let store =
            std::env::temp_dir().join(format!("rw-wrf-process-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let (tx, _rx) = channel();
        let summary = process_paths(
            &[PathBuf::from(path)],
            &store,
            &WrfProcessOptions {
                heavy_ecape: true,
                ..WrfProcessOptions::default()
            },
            &tx,
        )
        .expect("real WRF fixture should process");
        assert!(summary.hours_written >= 1);
        assert!(
            summary
                .variables
                .iter()
                .any(|name| name == "temperature_2m")
        );
        assert!(summary.variables.iter().any(|name| name == "sbcape"));
        assert!(summary.variables.iter().any(|name| name == "wrf_wspd10"));
        let _ = std::fs::remove_dir_all(&store);
    }

    /// End-to-end guard for the sounding fix: a real WRF file must land the
    /// `*_iso` isobaric volumes (as `pressure3d`) plus `surface_pressure`, and
    /// an interior column pull must carry real mid-tropospheric data. Gated on
    /// `RW_WRF_PROCESS_FIXTURE` (a `wrfout_*` path) like the sibling test.
    #[test]
    fn real_fixture_writes_isobaric_sounding_volumes() {
        let Some(path) = std::env::var_os("RW_WRF_PROCESS_FIXTURE") else {
            return;
        };
        let store =
            std::env::temp_dir().join(format!("rw-wrf-sounding-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let (tx, _rx) = channel();
        // Sounding-focused: skip the heavy 2D diagnostics (CAPE/severe) so this
        // stays fast even on a ~2M-cell CONUS grid; core fields + volumes +
        // surface fallback are what matter here.
        let summary = process_paths(
            &[PathBuf::from(path)],
            &store,
            &WrfProcessOptions {
                diagnostics: false,
                raw_extras: false,
                heavy_ecape: false,
                ..WrfProcessOptions::default()
            },
            &tx,
        )
        .expect("real WRF fixture should process");

        let hour_path = store
            .join(&summary.model)
            .join(&summary.run)
            .join("f000.rws");
        let reader = rw_store::reader::HourReader::open(&hour_path).expect("open hour file");

        for name in [
            "temperature_iso",
            "dewpoint_iso",
            "u_iso",
            "v_iso",
            "height_iso",
        ] {
            let var = reader
                .variable(name)
                .unwrap_or_else(|| panic!("{name} missing from store"));
            assert_eq!(var.kind, "pressure3d", "{name} should be a 3D volume");
            assert!(!var.levels_hpa.is_empty(), "{name} has no isobaric levels");
        }
        assert!(
            reader.variable("surface_pressure").is_some(),
            "surface_pressure missing — the skew-T column builder needs it"
        );

        // An interior column pull carries real, physical data: temperatures
        // in a sane Kelvin band and geopotential height increasing as pressure
        // decreases (store levels are descending, so 1000 hPa is index 0).
        // This catches the unit/ordering regressions a finite check misses.
        let levels = reader
            .variable("temperature_iso")
            .expect("temperature_iso")
            .levels_hpa
            .clone();
        let temps = reader
            .read_profile_3d("temperature_iso", 5.0, 5.0)
            .expect("read temperature_iso profile");
        let heights = reader
            .read_profile_3d("height_iso", 5.0, 5.0)
            .expect("read height_iso profile");
        assert_eq!(temps.len(), levels.len());
        assert_eq!(heights.len(), levels.len());

        let finite_temps = temps.iter().filter(|value| value.is_finite()).count();
        assert!(
            finite_temps >= 5,
            "expected several finite isobaric temperatures, got {finite_temps} of {}",
            temps.len()
        );
        for (level, temp) in levels.iter().zip(&temps) {
            if temp.is_finite() {
                assert!(
                    (180.0..=330.0).contains(temp),
                    "{level} hPa temperature {temp} K is non-physical (unit bug?)"
                );
            }
        }
        let mut last_height = f32::NEG_INFINITY;
        for height in &heights {
            if height.is_finite() {
                assert!(
                    *height > last_height,
                    "height must increase as pressure decreases, got {height} after {last_height}"
                );
                last_height = *height;
            }
        }

        let _ = std::fs::remove_dir_all(&store);
    }

    /// Instrumented harness for the large-grid full-diagnostics "crash"
    /// (FABLE_BACKLOG #9): runs the DEFAULT full-diagnostics import — the
    /// exact path the "Process WRF" dock button drives, including the same
    /// `spawn_process_paths` worker-thread configuration — on the wrfout named
    /// by `RW_WRF_CRASH_FIXTURE`, forwarding every Progress message to stderr
    /// with a timestamp so any abort pinpoints WHICH diagnostic died. Run with
    /// `--nocapture` and stderr captured to a file. Env-gated like the sibling
    /// fixtures; heavy — release builds only on large grids.
    ///
    /// Findings from the 2026-07-06 investigation (Enderlin 250 m,
    /// 800x800x79): the import COMPLETES in optimized builds (~275 s,
    /// 117 variables) — the reported `0xffffffff` abort was an external
    /// kill (only `process::exit(-1)`-style termination yields that code on
    /// this toolchain; a Rust abort/alloc-failure is 0xC0000409, a stack
    /// overflow 0xC00000FD, a panic 101), i.e. a tool-timeout kill of a
    /// 20-40x-slower debug run — not an in-process bug. See
    /// docs/wrf-import-large-grids.md.
    #[test]
    fn optional_real_fixture_default_import_instrumented() {
        let Some(fixture) = std::env::var_os("RW_WRF_CRASH_FIXTURE") else {
            return;
        };
        let store = std::env::temp_dir().join(format!("rw-wrf-crash-repro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let start = std::time::Instant::now();
        let task = spawn_process_paths(
            vec![PathBuf::from(fixture)],
            store.clone(),
            WrfProcessOptions::default(),
        );
        loop {
            match task.rx.recv() {
                Ok(WrfProcessMessage::Progress(line)) => {
                    eprintln!("[{:9.2?}] {line}", start.elapsed());
                }
                Ok(WrfProcessMessage::Done(result)) => {
                    let summary = result.expect("default full-diagnostics import should succeed");
                    eprintln!(
                        "[{:9.2?}] DONE: {} hour(s), {} variables: {}",
                        start.elapsed(),
                        summary.hours_written,
                        summary.variables.len(),
                        summary.variables.join(", ")
                    );
                    for note in &summary.notes {
                        eprintln!("[note] {note}");
                    }
                    assert!(summary.hours_written >= 1);
                    assert!(summary.variables.iter().any(|name| name == "sbcape"));
                    break;
                }
                // Disconnected without Done == the worker thread panicked
                // (a process-fatal abort never reaches this arm).
                Err(err) => panic!(
                    "[{:9.2?}] worker died without Done (panic in worker): {err}",
                    start.elapsed()
                ),
            }
        }
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn isolate_panics_converts_panic_to_error_and_passes_results_through() {
        assert_eq!(
            isolate_panics("field_ok", || Ok::<_, String>(7)),
            Ok(7),
            "successful computations must pass through untouched"
        );
        assert_eq!(
            isolate_panics("field_err", || Err::<(), _>("no such var".to_string())),
            Err("no such var".to_string()),
            "ordinary errors must pass through untouched"
        );
        let caught = isolate_panics::<()>("sbcape", || panic!("index out of bounds: 42"));
        assert_eq!(
            caught,
            Err("panicked computing sbcape: index out of bounds: 42".to_string()),
            "a panicking diagnostic must degrade to a named per-field error"
        );
        let caught_string =
            isolate_panics::<()>("srh3", || std::panic::panic_any("boom".to_string()));
        assert_eq!(
            caught_string,
            Err("panicked computing srh3: boom".to_string()),
            "String payloads must be extracted too"
        );
    }

    #[test]
    fn wrf_options_filter_heavy_and_names() {
        let default_options = WrfProcessOptions::default().normalized();
        assert!(!default_options.should_process(
            "sbecape",
            Some("sbecape"),
            WrfProductGroup::Heavy
        ));
        assert!(default_options.should_process(
            "srh1",
            Some("srh_0_1km"),
            WrfProductGroup::Diagnostic
        ));

        let filtered = WrfProcessOptions {
            only: vec!["srh".to_string()],
            skip: vec!["srh_0_3km".to_string()],
            ..WrfProcessOptions::default()
        }
        .normalized();
        assert!(filtered.should_process("srh1", Some("srh_0_1km"), WrfProductGroup::Diagnostic));
        assert!(!filtered.should_process("srh3", Some("srh_0_3km"), WrfProductGroup::Diagnostic));
        assert!(!filtered.should_process("t2", Some("temperature_2m"), WrfProductGroup::Core));
    }

    #[test]
    fn ncape_is_classified_with_the_heavy_ecape_group() {
        // ncape is a full ecape-rs solve; misclassified as Diagnostic it cost
        // ~10 s per 800x800x79 file in the default pass (perf audit
        // 2026-07-09). In the Heavy group it rides the ecape stack cache.
        assert!(is_heavy_wrf_diagnostic("ncape"));
        assert!(is_heavy_wrf_diagnostic("sbncape"));
        assert!(!is_heavy_wrf_diagnostic("sbcape"));
        assert!(!is_heavy_wrf_diagnostic("stp"));
    }

    /// Real-data proof for the UI field selector: the SAME wrfout processed
    /// with a narrowed selection (core fields only — no diagnostics, raw, or
    /// heavy eCAPE) must write ONLY the selected fields into the store hour — a
    /// strict, strictly-smaller subset of the full default set. This exercises
    /// the exact path the "WRF full diagnostics…" import drives. Gated on
    /// `RW_WRF_PROCESS_FIXTURE` (a `wrfout_*` path) like the sibling fixtures.
    #[test]
    fn real_fixture_selection_narrows_written_fields() {
        let Some(fixture) = std::env::var_os("RW_WRF_PROCESS_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(fixture);

        // Process `path` once under `options`, returning the store hour's
        // authoritative on-disk variable-name set (sorted, deduped).
        let written_fields = |options: WrfProcessOptions, tag: &str| -> Vec<String> {
            let store =
                std::env::temp_dir().join(format!("rw-wrf-select-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&store);
            let (tx, _rx) = channel();
            let summary = process_paths(std::slice::from_ref(&path), &store, &options, &tx)
                .unwrap_or_else(|err| panic!("process ({tag}) failed: {err}"));
            let hour_path = store
                .join(&summary.model)
                .join(&summary.run)
                .join("f000.rws");
            let reader = rw_store::reader::HourReader::open(&hour_path).expect("open hour file");
            let mut names: Vec<String> = reader
                .meta()
                .variables
                .iter()
                .map(|var| var.name.clone())
                .collect();
            names.sort();
            names.dedup();
            let _ = std::fs::remove_dir_all(&store);
            names
        };

        let narrowed = written_fields(
            WrfProcessOptions {
                diagnostics: false,
                raw_extras: false,
                heavy_ecape: false,
                ..WrfProcessOptions::default()
            }
            .normalized(),
            "narrow",
        );
        let default = written_fields(WrfProcessOptions::default().normalized(), "default");

        eprintln!(
            "NARROWED core-only ({} fields): {}",
            narrowed.len(),
            narrowed.join(", ")
        );
        eprintln!(
            "DEFAULT full set ({} fields): {}",
            default.len(),
            default.join(", ")
        );

        // Narrowed keeps the core surface fields + isobaric sounding volumes…
        assert!(
            narrowed.iter().any(|name| name == "temperature_2m"),
            "narrowed selection must still write the core surface fields"
        );
        assert!(
            narrowed.iter().any(|name| name == "temperature_iso"),
            "narrowed selection must still write the sounding volumes"
        );
        // …but drops the severe diagnostics and raw extras the default writes.
        assert!(
            !narrowed.iter().any(|name| name == "sbcape"),
            "narrowed (diagnostics off) must NOT write CAPE and friends"
        );
        assert!(
            default.iter().any(|name| name == "sbcape"),
            "default set must include the severe diagnostics"
        );
        // Strict subset, strictly smaller: the selection genuinely narrowed the
        // written store hour rather than falling back to the full default set.
        assert!(
            narrowed.iter().all(|name| default.contains(name)),
            "narrowed field set must be a subset of the default field set"
        );
        assert!(
            narrowed.len() < default.len(),
            "narrowed selection must write fewer fields ({}) than the default ({})",
            narrowed.len(),
            default.len()
        );
    }

    #[test]
    fn planned_store_fields_track_the_group_selection() {
        // Default: core + diagnostics + raw (no heavy eCAPE).
        let default_plan = WrfProcessOptions::default()
            .normalized()
            .planned_store_fields();
        assert!(default_plan.iter().any(|name| name == "temperature_2m"));
        assert!(default_plan.iter().any(|name| name == "temperature_iso"));
        assert!(default_plan.iter().any(|name| name == "sbcape"));
        // Heavy eCAPE (any entrainment-CAPE field) is off by default.
        assert!(!default_plan.iter().any(|name| name.contains("ecape")));

        // Core-only, heavy off: drops every diagnostic and raw field but keeps
        // the core surface fields and the isobaric sounding volumes.
        let core_only = WrfProcessOptions {
            diagnostics: false,
            raw_extras: false,
            heavy_ecape: false,
            ..WrfProcessOptions::default()
        }
        .normalized()
        .planned_store_fields();
        assert!(core_only.iter().any(|name| name == "temperature_2m"));
        assert!(core_only.iter().any(|name| name == "height_iso"));
        assert!(!core_only.iter().any(|name| name == "sbcape"));

        // Enabling heavy eCAPE adds the entrainment-CAPE family (e.g.
        // ecape_scp / ecape_ehi from wrf-core's VARS).
        let heavy = WrfProcessOptions {
            heavy_ecape: true,
            ..WrfProcessOptions::default()
        }
        .normalized()
        .planned_store_fields();
        assert!(heavy.iter().any(|name| name.contains("ecape")));
        assert!(heavy.len() > default_plan.len());

        // An only-list narrows the plan to the matching fields (plus the iso
        // volumes, which ride with the core group toggle, not the name filter).
        let only_cape = WrfProcessOptions {
            only: vec!["sbcape".to_string()],
            ..WrfProcessOptions::default()
        }
        .normalized()
        .planned_store_fields();
        assert!(only_cape.iter().any(|name| name == "sbcape"));
        assert!(!only_cape.iter().any(|name| name == "temperature_2m"));
    }
}
